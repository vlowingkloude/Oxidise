use std::{path::PathBuf, sync::Arc};

use axum::{
    Json, Router,
    body::Body,
    extract::{
        DefaultBodyLimit, Query, State,
        ws::{WebSocket, WebSocketUpgrade},
    },
    http::{HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{
    files::{self, NewEntryKind, SearchMatch, TextFile, TreeEntry, WorkspaceInfo},
    lsp::{LanguageServerKind, LanguageServerQuery},
    terminal,
};

#[derive(Clone)]
struct AppState {
    initial_workspace: Arc<Option<WorkspaceInfo>>,
}

pub fn router(initial: Option<WorkspaceInfo>) -> Router {
    let state = AppState {
        initial_workspace: Arc::new(initial),
    };
    Router::new()
        .route("/health", get(health))
        .route("/workspace", get(initial_workspace))
        .route("/workspace/open", post(open_workspace))
        .route("/tree", get(tree))
        .route("/files", get(workspace_files))
        .route("/file/read", post(read_file))
        .route("/file/write", post(write_file))
        .route("/file/create", post(create_entry))
        .route("/file/rename", post(rename_entry))
        .route("/file/delete", post(delete_entry))
        .route("/file/raw", get(raw_file))
        .route("/search", post(search))
        .route("/watch", get(watch_socket))
        .route("/terminal", get(terminal_socket))
        .route("/lsp", get(lsp_socket))
        .fallback(api_not_found)
        .method_not_allowed_fallback(method_not_allowed)
        .layer(DefaultBodyLimit::max(8 << 20))
        .with_state(state)
}

async fn api_not_found() -> impl IntoResponse {
    (
        StatusCode::NOT_FOUND,
        Json(ErrorBody {
            error: "API route not found".to_owned(),
        }),
    )
}

async fn method_not_allowed() -> impl IntoResponse {
    (
        StatusCode::METHOD_NOT_ALLOWED,
        Json(ErrorBody {
            error: "method not allowed".to_owned(),
        }),
    )
}

async fn health() -> Json<serde_json::Value> {
    Json(serde_json::json!({ "ok": true }))
}

async fn initial_workspace(State(state): State<AppState>) -> Json<Option<WorkspaceInfo>> {
    Json((*state.initial_workspace).clone())
}

#[derive(Debug, Deserialize)]
struct OpenWorkspaceRequest {
    path: String,
}

async fn open_workspace(
    Json(request): Json<OpenWorkspaceRequest>,
) -> Result<Json<WorkspaceInfo>, ApiError> {
    let path = request.path.trim();
    if path.is_empty() {
        return Err(ApiError::InvalidRequest("a path is required".to_owned()));
    }
    Ok(Json(files::open_workspace(&PathBuf::from(path))?))
}

#[derive(Debug, Deserialize)]
struct WorkspaceQuery {
    root: String,
    initial_file: Option<String>,
}

#[derive(Debug, Deserialize)]
struct TreeQuery {
    root: String,
    initial_file: Option<String>,
    path: Option<String>,
}

#[derive(Debug, Deserialize)]
struct FileRequest {
    root: String,
    initial_file: Option<String>,
    path: String,
}

#[derive(Debug, Deserialize)]
struct WriteFileRequest {
    root: String,
    initial_file: Option<String>,
    path: String,
    content: String,
}

#[derive(Debug, Deserialize)]
struct CreateEntryRequest {
    root: String,
    initial_file: Option<String>,
    path: String,
    kind: String,
}

#[derive(Debug, Deserialize)]
struct RenameEntryRequest {
    root: String,
    initial_file: Option<String>,
    path: String,
    destination: String,
}

#[derive(Debug, Deserialize)]
struct DeleteEntryRequest {
    root: String,
    initial_file: Option<String>,
    path: String,
}

#[derive(Debug, Deserialize)]
struct SearchRequest {
    root: String,
    initial_file: Option<String>,
    query: String,
    #[serde(default)]
    case_sensitive: bool,
}

async fn tree(Query(query): Query<TreeQuery>) -> Result<Json<Vec<TreeEntry>>, ApiError> {
    let workspace = workspace_from_parts(&query.root, query.initial_file.as_deref())?;
    Ok(Json(files::list_tree(
        &workspace,
        query.path.as_deref().unwrap_or(""),
    )?))
}

async fn workspace_files(
    Query(query): Query<WorkspaceQuery>,
) -> Result<Json<Vec<TreeEntry>>, ApiError> {
    let workspace = workspace_from_parts(&query.root, query.initial_file.as_deref())?;
    Ok(Json(files::list_workspace_files(&workspace)?))
}

async fn read_file(Json(request): Json<FileRequest>) -> Result<Json<TextFile>, ApiError> {
    let workspace = workspace_from_parts(&request.root, request.initial_file.as_deref())?;
    Ok(Json(files::read_text_file(&workspace, &request.path)?))
}

async fn write_file(Json(request): Json<WriteFileRequest>) -> Result<Json<TextFile>, ApiError> {
    let workspace = workspace_from_parts(&request.root, request.initial_file.as_deref())?;
    Ok(Json(files::write_text_file(
        &workspace,
        &request.path,
        &request.content,
    )?))
}

async fn create_entry(Json(request): Json<CreateEntryRequest>) -> Result<StatusCode, ApiError> {
    let workspace = workspace_from_parts(&request.root, request.initial_file.as_deref())?;
    let kind = match request.kind.as_str() {
        "file" => NewEntryKind::File,
        "directory" => NewEntryKind::Directory,
        _ => {
            return Err(ApiError::InvalidRequest(
                "kind must be file or directory".to_owned(),
            ));
        }
    };
    files::create_entry(&workspace, &request.path, kind)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn rename_entry(Json(request): Json<RenameEntryRequest>) -> Result<StatusCode, ApiError> {
    let workspace = workspace_from_parts(&request.root, request.initial_file.as_deref())?;
    files::rename_entry(&workspace, &request.path, &request.destination)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn delete_entry(Json(request): Json<DeleteEntryRequest>) -> Result<StatusCode, ApiError> {
    let workspace = workspace_from_parts(&request.root, request.initial_file.as_deref())?;
    files::delete_entry(&workspace, &request.path)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn search(Json(request): Json<SearchRequest>) -> Result<Json<Vec<SearchMatch>>, ApiError> {
    let workspace = workspace_from_parts(&request.root, request.initial_file.as_deref())?;
    Ok(Json(files::search_workspace(
        &workspace,
        request.query.trim(),
        request.case_sensitive,
    )?))
}

async fn raw_file(Query(request): Query<FileRequest>) -> Result<Response, ApiError> {
    let workspace = workspace_from_parts(&request.root, request.initial_file.as_deref())?;
    let path = files::raw_file_path(&workspace, &request.path)?;
    let mime = mime_guess::from_path(&path).first_or_octet_stream();
    if mime.type_() != mime_guess::mime::IMAGE {
        return Err(ApiError::InvalidRequest(
            "raw preview is limited to image files".to_owned(),
        ));
    }
    let bytes = tokio::fs::read(path).await?;
    let mut response = Response::new(Body::from(bytes));
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_str(mime.as_ref())
            .map_err(|err| ApiError::InvalidRequest(err.to_string()))?,
    );
    Ok(response)
}

async fn terminal_socket(
    Query(query): Query<WorkspaceQuery>,
    ws: WebSocketUpgrade,
) -> Result<Response, ApiError> {
    let workspace = workspace_from_parts(&query.root, query.initial_file.as_deref())?;
    let cwd = PathBuf::from(workspace.root);
    Ok(ws
        .on_upgrade(move |socket: WebSocket| terminal::handle_socket(socket, cwd))
        .into_response())
}

async fn watch_socket(
    Query(query): Query<WorkspaceQuery>,
    ws: WebSocketUpgrade,
) -> Result<Response, ApiError> {
    let workspace = workspace_from_parts(&query.root, query.initial_file.as_deref())?;
    Ok(ws
        .on_upgrade(move |socket: WebSocket| crate::watch::handle_socket(socket, workspace))
        .into_response())
}

async fn lsp_socket(
    Query(query): Query<LanguageServerQuery>,
    ws: WebSocketUpgrade,
) -> Result<Response, ApiError> {
    let workspace = workspace_from_parts(&query.root, query.initial_file.as_deref())?;
    let kind = LanguageServerKind::from_language(&query.language).ok_or_else(|| {
        ApiError::InvalidRequest(format!("unsupported language server: {}", query.language))
    })?;
    let cwd = PathBuf::from(workspace.root);
    Ok(ws
        .on_upgrade(move |socket: WebSocket| crate::lsp::handle_socket(socket, cwd, kind))
        .into_response())
}

fn workspace_from_parts(root: &str, initial_file: Option<&str>) -> Result<WorkspaceInfo, ApiError> {
    let path = initial_file
        .map(|file| PathBuf::from(root).join(file))
        .unwrap_or_else(|| PathBuf::from(root));
    Ok(files::open_workspace(&path)?)
}

#[derive(Debug, Error)]
enum ApiError {
    #[error("{0}")]
    Files(#[from] files::FileError),
    #[error("{0}")]
    InvalidRequest(String),
    #[error("I/O operation failed: {0}")]
    Io(#[from] std::io::Error),
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status = match &self {
            Self::Files(files::FileError::NotFound(_)) => StatusCode::NOT_FOUND,
            Self::Files(files::FileError::TooLarge(_)) => StatusCode::PAYLOAD_TOO_LARGE,
            Self::Files(files::FileError::Io(_)) | Self::Io(_) => StatusCode::INTERNAL_SERVER_ERROR,
            Self::Files(_) | Self::InvalidRequest(_) => StatusCode::BAD_REQUEST,
        };
        (
            status,
            Json(ErrorBody {
                error: self.to_string(),
            }),
        )
            .into_response()
    }
}

#[derive(Serialize)]
struct ErrorBody {
    error: String,
}
