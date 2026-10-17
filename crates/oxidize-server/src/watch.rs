use std::{
    collections::BTreeMap,
    fs,
    path::Path,
    time::UNIX_EPOCH,
};

use axum::extract::ws::{Message, WebSocket};
use futures_util::StreamExt;
use notify::{RecursiveMode, Watcher};
use serde::Serialize;
use tokio::{
    sync::mpsc,
    time::{Duration, interval},
};

use crate::files::{WorkspaceInfo, should_skip};

const MAX_WATCH_ENTRIES: usize = 20_000;
const MAX_CHANGES: usize = 500;
const DEBOUNCE: Duration = Duration::from_millis(75);
const POLL_INTERVAL: Duration = Duration::from_millis(1200);

#[derive(Debug, Clone, PartialEq, Eq)]
struct Fingerprint {
    directory: bool,
    len: u64,
    modified: u128,
    mode: u32,
}

#[derive(Debug, Serialize)]
struct ChangeBatch {
    r#type: &'static str,
    changes: Vec<FileChange>,
}

#[derive(Debug, Serialize)]
struct FileChange {
    kind: &'static str,
    path: String,
}

pub async fn handle_socket(mut socket: WebSocket, workspace: WorkspaceInfo) {
    let (event_tx, mut event_rx) =
        mpsc::unbounded_channel::<notify::Result<notify::Event>>();
    let root = Path::new(&workspace.root).to_path_buf();
    let mut watcher: Option<notify::RecommendedWatcher> = None;
    match notify::recommended_watcher(move |result: notify::Result<notify::Event>| {
        if let Ok(event) = &result
            && !event.paths.is_empty()
            && event
                .paths
                .iter()
                .all(|path| is_under_skipped_directory(&root, path))
        {
            return;
        }
        let _ = event_tx.send(result);
    }) {
        Ok(mut candidate) => {
            let mode = if workspace.initial_file.is_some() {
                RecursiveMode::NonRecursive
            } else {
                RecursiveMode::Recursive
            };
            match candidate.watch(Path::new(&workspace.root), mode) {
                Ok(()) => watcher = Some(candidate),
                Err(error) => {
                    tracing::warn!("failed to watch workspace, falling back to polling: {error}");
                }
            }
        }
        Err(error) => {
            tracing::warn!("failed to start file watcher, falling back to polling: {error}");
        }
    }

    let mut previous = match snapshot_async(&workspace).await {
        Ok(snapshot) => snapshot,
        Err(error) => {
            tracing::warn!("failed to snapshot workspace: {error}");
            BTreeMap::new()
        }
    };
    let polling_only = watcher.is_none();
    let mut poll = interval(POLL_INTERVAL);
    poll.tick().await;

    loop {
        tokio::select! {
            Some(_) = event_rx.recv() => {
                // Coalesce a burst of events before walking the tree.
                loop {
                    tokio::time::sleep(DEBOUNCE).await;
                    let mut more = false;
                    while event_rx.try_recv().is_ok() {
                        more = true;
                    }
                    if !more {
                        break;
                    }
                }
                let Ok(current) = snapshot_async(&workspace).await else { continue };
                let changes = compare(&previous, &current);
                previous = current;
                if !changes.is_empty() && !send_changes(&mut socket, changes).await {
                    break;
                }
            }
            _ = poll.tick(), if polling_only => {
                let Ok(current) = snapshot_async(&workspace).await else { continue };
                let changes = compare(&previous, &current);
                previous = current;
                if !changes.is_empty() && !send_changes(&mut socket, changes).await {
                    break;
                }
            }
            message = socket.next() => {
                match message {
                    Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                    _ => {}
                }
            }
        }
    }
}

async fn snapshot_async(
    workspace: &WorkspaceInfo,
) -> std::io::Result<BTreeMap<String, Fingerprint>> {
    let workspace = workspace.clone();
    tokio::task::spawn_blocking(move || snapshot(&workspace))
        .await
        .map_err(std::io::Error::other)?
}

async fn send_changes(socket: &mut WebSocket, changes: Vec<FileChange>) -> bool {
    let payload = match serde_json::to_string(&ChangeBatch {
        r#type: "changes",
        changes,
    }) {
        Ok(payload) => payload,
        Err(_) => return true,
    };
    socket.send(Message::Text(payload.into())).await.is_ok()
}

fn snapshot(workspace: &WorkspaceInfo) -> std::io::Result<BTreeMap<String, Fingerprint>> {
    let mut entries = BTreeMap::new();
    if let Some(initial_file) = &workspace.initial_file {
        add_path(
            Path::new(&workspace.root),
            Path::new(initial_file),
            &mut entries,
        )?;
    } else {
        collect(Path::new(&workspace.root), Path::new(""), &mut entries)?;
    }
    Ok(entries)
}

fn collect(
    root: &Path,
    relative_dir: &Path,
    entries: &mut BTreeMap<String, Fingerprint>,
) -> std::io::Result<()> {
    if entries.len() >= MAX_WATCH_ENTRIES {
        return Ok(());
    }
    let mut children = fs::read_dir(root.join(relative_dir))?.collect::<Result<Vec<_>, _>>()?;
    children.sort_by_key(|child| child.file_name());
    for child in children {
        let name = child.file_name();
        let Some(name) = name.to_str() else { continue };
        if should_skip(name) {
            continue;
        }
        let relative = relative_dir.join(name);
        let file_type = child.file_type()?;
        add_path(root, &relative, entries)?;
        if file_type.is_dir() && !file_type.is_symlink() {
            collect(root, &relative, entries)?;
        }
        if entries.len() >= MAX_WATCH_ENTRIES {
            break;
        }
    }
    Ok(())
}

fn add_path(
    root: &Path,
    relative: &Path,
    entries: &mut BTreeMap<String, Fingerprint>,
) -> std::io::Result<()> {
    let metadata = fs::symlink_metadata(root.join(relative))?;
    let modified = metadata
        .modified()
        .ok()
        .and_then(|value| value.duration_since(UNIX_EPOCH).ok())
        .map(|value| value.as_nanos())
        .unwrap_or_default();
    entries.insert(
        slash_path(relative),
        Fingerprint {
            directory: metadata.is_dir(),
            len: metadata.len(),
            modified,
            mode: permissions_mode(&metadata),
        },
    );
    Ok(())
}

#[cfg(unix)]
fn permissions_mode(metadata: &fs::Metadata) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    metadata.permissions().mode()
}

#[cfg(not(unix))]
fn permissions_mode(metadata: &fs::Metadata) -> u32 {
    u32::from(metadata.permissions().readonly())
}

fn compare(
    previous: &BTreeMap<String, Fingerprint>,
    current: &BTreeMap<String, Fingerprint>,
) -> Vec<FileChange> {
    let mut changes = Vec::new();
    for (path, fingerprint) in current {
        match previous.get(path) {
            None => changes.push(FileChange {
                kind: "created",
                path: path.clone(),
            }),
            Some(old) if old != fingerprint => changes.push(FileChange {
                kind: "modified",
                path: path.clone(),
            }),
            _ => {}
        }
        if changes.len() > MAX_CHANGES {
            return resync();
        }
    }
    for path in previous.keys() {
        if !current.contains_key(path) {
            changes.push(FileChange {
                kind: "removed",
                path: path.clone(),
            });
        }
        if changes.len() > MAX_CHANGES {
            return resync();
        }
    }
    changes
}

fn resync() -> Vec<FileChange> {
    vec![FileChange {
        kind: "resync",
        path: String::new(),
    }]
}

fn slash_path(path: &Path) -> String {
    path.components()
        .filter_map(|component| match component {
            std::path::Component::Normal(value) => Some(value.to_string_lossy()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/")
}

fn is_under_skipped_directory(root: &Path, path: &Path) -> bool {
    path.strip_prefix(root)
        .ok()
        .map(|relative| {
            relative.components().any(|component| match component {
                std::path::Component::Normal(name) => {
                    name.to_str().map(should_skip).unwrap_or(false)
                }
                _ => false,
            })
        })
        .unwrap_or(false)
}
