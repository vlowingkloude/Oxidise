use std::{
    fs,
    path::{Component, Path, PathBuf},
};

use serde::{Serialize, Serializer};
use thiserror::Error;

const MAX_TEXT_FILE_BYTES: u64 = 2 * 1024 * 1024;
const MAX_ENTRIES: usize = 5_000;
const MAX_SEARCH_MATCHES: usize = 500;

/// Directory names that are never shown in the tree, searched, or watched.
/// This is the C++ skip list plus `target` for Rust build output.
pub const SKIPPED_NAMES: &[&str] = &[
    ".cache",
    ".git",
    ".idea",
    ".vscode",
    "build",
    "dist",
    "node_modules",
    "out",
    "target",
];

pub fn should_skip(name: &str) -> bool {
    SKIPPED_NAMES.contains(&name) || name.starts_with("cmake-build-")
}

#[derive(Debug, Clone, Serialize)]
pub struct WorkspaceInfo {
    pub name: String,
    pub root: String,
    pub kind: WorkspaceKind,
    #[serde(serialize_with = "serialize_initial_file")]
    pub initial_file: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceKind {
    Folder,
    File,
}

#[derive(Debug, Clone, Serialize)]
pub struct TreeEntry {
    pub path: String,
    pub name: String,
    pub kind: EntryKind,
    pub depth: usize,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EntryKind {
    File,
    Directory,
}

#[derive(Debug, Clone, Serialize)]
pub struct TextFile {
    pub path: String,
    pub content: String,
    pub size: u64,
}

#[derive(Debug, Clone, Copy)]
pub enum NewEntryKind {
    File,
    Directory,
}

#[derive(Debug, Clone, Serialize)]
pub struct SearchMatch {
    pub path: String,
    pub line: usize,
    pub column: usize,
    pub preview: String,
}

#[derive(Debug, Error)]
pub enum FileError {
    #[error("path does not exist: {0}")]
    NotFound(String),
    #[error("path must be a file or directory: {0}")]
    UnsupportedPath(String),
    #[error("invalid path: {0}")]
    InvalidPath(String),
    #[error("path is outside the open workspace: {0}")]
    OutsideWorkspace(String),
    #[error("file is too large to edit ({0} bytes)")]
    TooLarge(u64),
    #[error("binary files cannot be opened in the text editor")]
    Binary,
    #[error("I/O operation failed: {0}")]
    Io(#[from] std::io::Error),
}

pub fn open_workspace(path: &Path) -> Result<WorkspaceInfo, FileError> {
    let canonical = path
        .canonicalize()
        .map_err(|_| FileError::NotFound(path.display().to_string()))?;
    if canonical.is_dir() {
        return Ok(WorkspaceInfo {
            name: display_name(&canonical),
            root: canonical.to_string_lossy().into_owned(),
            kind: WorkspaceKind::Folder,
            initial_file: None,
        });
    }
    if canonical.is_file() {
        let parent = canonical
            .parent()
            .ok_or_else(|| FileError::InvalidPath(canonical.display().to_string()))?;
        let file_name = canonical
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| FileError::InvalidPath(canonical.display().to_string()))?
            .to_owned();
        return Ok(WorkspaceInfo {
            name: file_name.clone(),
            root: parent.to_string_lossy().into_owned(),
            kind: WorkspaceKind::File,
            initial_file: Some(file_name),
        });
    }
    Err(FileError::UnsupportedPath(canonical.display().to_string()))
}

/// List one directory level. The web client expands directories lazily, so
/// `relative_dir` is `""` for the workspace root.
pub fn list_tree(workspace: &WorkspaceInfo, relative_dir: &str) -> Result<Vec<TreeEntry>, FileError> {
    if let Some(initial_file) = &workspace.initial_file {
        if !relative_dir.is_empty() {
            return Err(FileError::InvalidPath(
                "single-file workspaces do not contain directories".to_owned(),
            ));
        }
        return Ok(vec![TreeEntry {
            path: initial_file.clone(),
            name: initial_file.clone(),
            kind: EntryKind::File,
            depth: 0,
        }]);
    }

    if !relative_dir.is_empty() {
        validate_relative_path(relative_dir)?;
        let directory = resolve_existing_path(workspace, relative_dir)?;
        if !fs::metadata(&directory)?.is_dir() {
            return Err(FileError::InvalidPath(
                "tree path is not a directory".to_owned(),
            ));
        }
    }

    list_directory_entries(Path::new(&workspace.root), Path::new(relative_dir))
}

/// Flat, recursive file list used by the quick-open palette.
pub fn list_workspace_files(workspace: &WorkspaceInfo) -> Result<Vec<TreeEntry>, FileError> {
    if workspace.initial_file.is_some() {
        return list_tree(workspace, "");
    }
    let mut entries = Vec::new();
    collect_workspace_files(Path::new(&workspace.root), Path::new(""), &mut entries)?;
    entries.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(entries)
}

pub fn read_text_file(workspace: &WorkspaceInfo, relative: &str) -> Result<TextFile, FileError> {
    ensure_file_in_scope(workspace, relative)?;
    let path = resolve_existing_path(workspace, relative)?;
    let metadata = fs::metadata(&path)?;
    if !metadata.is_file() {
        return Err(FileError::InvalidPath(relative.to_owned()));
    }
    if metadata.len() > MAX_TEXT_FILE_BYTES {
        return Err(FileError::TooLarge(metadata.len()));
    }
    let bytes = fs::read(&path)?;
    if bytes.contains(&0) {
        return Err(FileError::Binary);
    }
    Ok(TextFile {
        path: relative.to_owned(),
        content: String::from_utf8_lossy(&bytes).into_owned(),
        size: metadata.len(),
    })
}

pub fn write_text_file(
    workspace: &WorkspaceInfo,
    relative: &str,
    content: &str,
) -> Result<TextFile, FileError> {
    ensure_file_in_scope(workspace, relative)?;
    let path = resolve_existing_path(workspace, relative)?;
    if !path.is_file() {
        return Err(FileError::InvalidPath(relative.to_owned()));
    }
    fs::write(&path, content)?;
    Ok(TextFile {
        path: relative.to_owned(),
        content: content.to_owned(),
        size: content.len() as u64,
    })
}

pub fn raw_file_path(workspace: &WorkspaceInfo, relative: &str) -> Result<PathBuf, FileError> {
    ensure_file_in_scope(workspace, relative)?;
    let path = resolve_existing_path(workspace, relative)?;
    if path.is_file() {
        Ok(path)
    } else {
        Err(FileError::InvalidPath(relative.to_owned()))
    }
}

pub fn create_entry(
    workspace: &WorkspaceInfo,
    relative: &str,
    kind: NewEntryKind,
) -> Result<(), FileError> {
    ensure_folder_workspace(workspace)?;
    let path = resolve_new_path(workspace, relative)?;
    if fs::symlink_metadata(&path).is_ok() {
        return Err(FileError::InvalidPath(format!(
            "path already exists: {relative}"
        )));
    }
    match kind {
        NewEntryKind::File => {
            fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(path)?;
        }
        NewEntryKind::Directory => fs::create_dir(path)?,
    }
    Ok(())
}

pub fn rename_entry(
    workspace: &WorkspaceInfo,
    relative: &str,
    destination: &str,
) -> Result<(), FileError> {
    ensure_folder_workspace(workspace)?;
    let source = resolve_entry_path(workspace, relative)?;
    let destination = resolve_new_path(workspace, destination)?;
    if fs::symlink_metadata(&destination).is_ok() {
        return Err(FileError::InvalidPath(format!(
            "path already exists: {}",
            destination.display()
        )));
    }
    fs::rename(source, destination)?;
    Ok(())
}

pub fn delete_entry(workspace: &WorkspaceInfo, relative: &str) -> Result<(), FileError> {
    ensure_folder_workspace(workspace)?;
    let path = resolve_entry_path(workspace, relative)?;
    let metadata = fs::symlink_metadata(&path)?;
    if metadata.file_type().is_symlink() || metadata.is_file() {
        fs::remove_file(path)?;
    } else if metadata.is_dir() {
        fs::remove_dir_all(path)?;
    } else {
        return Err(FileError::InvalidPath(relative.to_owned()));
    }
    Ok(())
}

pub fn search_workspace(
    workspace: &WorkspaceInfo,
    query: &str,
    case_sensitive: bool,
) -> Result<Vec<SearchMatch>, FileError> {
    if query.is_empty() || query.len() > 256 {
        return Err(FileError::InvalidPath(
            "search query must contain 1–256 bytes".to_owned(),
        ));
    }
    let mut matches = Vec::new();
    if let Some(initial_file) = &workspace.initial_file {
        search_file(workspace, initial_file, query, case_sensitive, &mut matches)?;
    } else {
        search_directory(
            workspace,
            Path::new(&workspace.root),
            Path::new(""),
            query,
            case_sensitive,
            &mut matches,
        )?;
    }
    Ok(matches)
}

fn list_directory_entries(root: &Path, relative_dir: &Path) -> Result<Vec<TreeEntry>, FileError> {
    let mut children = fs::read_dir(root.join(relative_dir))?.collect::<Result<Vec<_>, _>>()?;
    children.sort_by(|left, right| {
        let left_dir = left.file_type().map(|kind| kind.is_dir()).unwrap_or(false);
        let right_dir = right.file_type().map(|kind| kind.is_dir()).unwrap_or(false);
        right_dir
            .cmp(&left_dir)
            .then_with(|| left.file_name().cmp(&right.file_name()))
    });

    let mut entries = Vec::with_capacity(children.len().min(MAX_ENTRIES));
    for child in children {
        if entries.len() >= MAX_ENTRIES {
            break;
        }
        let name = child.file_name();
        let Some(name) = name.to_str() else { continue };
        if should_skip(name) {
            continue;
        }
        let relative = relative_dir.join(name);
        let file_type = child.file_type()?;
        let path = slash_path(&relative);
        entries.push(TreeEntry {
            depth: path.matches('/').count(),
            path,
            name: name.to_owned(),
            kind: if file_type.is_dir() {
                EntryKind::Directory
            } else {
                EntryKind::File
            },
        });
    }
    Ok(entries)
}

fn collect_workspace_files(
    root: &Path,
    relative_dir: &Path,
    entries: &mut Vec<TreeEntry>,
) -> Result<(), FileError> {
    if entries.len() >= MAX_ENTRIES {
        return Ok(());
    }
    for child in fs::read_dir(root.join(relative_dir))? {
        if entries.len() >= MAX_ENTRIES {
            break;
        }
        let child = child?;
        let name = child.file_name();
        let Some(name) = name.to_str() else { continue };
        if should_skip(name) {
            continue;
        }
        let relative = relative_dir.join(name);
        let file_type = child.file_type()?;
        if file_type.is_dir() && !file_type.is_symlink() {
            collect_workspace_files(root, &relative, entries)?;
        } else if file_type.is_file() {
            let path = slash_path(&relative);
            entries.push(TreeEntry {
                depth: path.matches('/').count(),
                path,
                name: name.to_owned(),
                kind: EntryKind::File,
            });
        }
    }
    Ok(())
}

fn search_directory(
    workspace: &WorkspaceInfo,
    root: &Path,
    relative_dir: &Path,
    query: &str,
    case_sensitive: bool,
    matches: &mut Vec<SearchMatch>,
) -> Result<(), FileError> {
    if matches.len() >= MAX_SEARCH_MATCHES {
        return Ok(());
    }
    let mut children = fs::read_dir(root.join(relative_dir))?.collect::<Result<Vec<_>, _>>()?;
    children.sort_by_key(|entry| entry.file_name());
    for child in children {
        if matches.len() >= MAX_SEARCH_MATCHES {
            break;
        }
        let name = child.file_name();
        let Some(name) = name.to_str() else { continue };
        if should_skip(name) {
            continue;
        }
        let file_type = child.file_type()?;
        let relative = relative_dir.join(name);
        if file_type.is_dir() && !file_type.is_symlink() {
            search_directory(workspace, root, &relative, query, case_sensitive, matches)?;
        } else if file_type.is_file() {
            search_file(
                workspace,
                &slash_path(&relative),
                query,
                case_sensitive,
                matches,
            )?;
        }
    }
    Ok(())
}

fn search_file(
    workspace: &WorkspaceInfo,
    relative: &str,
    query: &str,
    case_sensitive: bool,
    matches: &mut Vec<SearchMatch>,
) -> Result<(), FileError> {
    let path = resolve_existing_path(workspace, relative)?;
    let metadata = fs::metadata(&path)?;
    if metadata.len() > MAX_TEXT_FILE_BYTES {
        return Ok(());
    }
    let bytes = fs::read(path)?;
    if bytes.contains(&0) {
        return Ok(());
    }
    let content = String::from_utf8_lossy(&bytes);
    let needle = if case_sensitive {
        query.to_owned()
    } else {
        query.to_ascii_lowercase()
    };
    for (line_index, line) in content.lines().enumerate() {
        let searchable = if case_sensitive {
            line.to_owned()
        } else {
            line.to_ascii_lowercase()
        };
        let mut offset = 0;
        while let Some(found) = searchable[offset..].find(&needle) {
            let column = offset + found;
            matches.push(SearchMatch {
                path: relative.to_owned(),
                line: line_index + 1,
                column: line[..column].chars().count() + 1,
                preview: line.trim().chars().take(300).collect(),
            });
            if matches.len() >= MAX_SEARCH_MATCHES {
                return Ok(());
            }
            offset = column + needle.len().max(1);
        }
    }
    Ok(())
}

fn resolve_existing_path(workspace: &WorkspaceInfo, relative: &str) -> Result<PathBuf, FileError> {
    validate_relative_path(relative)?;
    let root = Path::new(&workspace.root)
        .canonicalize()
        .map_err(|_| FileError::NotFound(workspace.root.clone()))?;
    let joined = root.join(relative);
    let resolved = joined
        .canonicalize()
        .map_err(|_| FileError::NotFound(relative.to_owned()))?;
    if !resolved.starts_with(&root) {
        return Err(FileError::OutsideWorkspace(relative.to_owned()));
    }
    Ok(resolved)
}

fn resolve_new_path(workspace: &WorkspaceInfo, relative: &str) -> Result<PathBuf, FileError> {
    validate_relative_path(relative)?;
    let root = Path::new(&workspace.root)
        .canonicalize()
        .map_err(|_| FileError::NotFound(workspace.root.clone()))?;
    let joined = root.join(relative);
    let parent = joined
        .parent()
        .ok_or_else(|| FileError::InvalidPath(relative.to_owned()))?
        .canonicalize()
        .map_err(|_| FileError::NotFound(relative.to_owned()))?;
    if !parent.starts_with(&root) {
        return Err(FileError::OutsideWorkspace(relative.to_owned()));
    }
    Ok(joined)
}

fn resolve_entry_path(workspace: &WorkspaceInfo, relative: &str) -> Result<PathBuf, FileError> {
    validate_relative_path(relative)?;
    let root = Path::new(&workspace.root)
        .canonicalize()
        .map_err(|_| FileError::NotFound(workspace.root.clone()))?;
    let joined = root.join(relative);
    fs::symlink_metadata(&joined).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            FileError::NotFound(relative.to_owned())
        } else {
            FileError::Io(error)
        }
    })?;
    let parent = joined
        .parent()
        .ok_or_else(|| FileError::InvalidPath(relative.to_owned()))?
        .canonicalize()
        .map_err(|_| FileError::NotFound(relative.to_owned()))?;
    if !parent.starts_with(&root) {
        return Err(FileError::OutsideWorkspace(relative.to_owned()));
    }
    Ok(joined)
}

fn ensure_folder_workspace(workspace: &WorkspaceInfo) -> Result<(), FileError> {
    if workspace.initial_file.is_some() {
        return Err(FileError::OutsideWorkspace(
            "file operations are unavailable in a single-file workspace".to_owned(),
        ));
    }
    Ok(())
}

fn ensure_file_in_scope(workspace: &WorkspaceInfo, relative: &str) -> Result<(), FileError> {
    if let Some(initial_file) = &workspace.initial_file
        && relative != initial_file
    {
        return Err(FileError::OutsideWorkspace(relative.to_owned()));
    }
    Ok(())
}

fn validate_relative_path(path: &str) -> Result<(), FileError> {
    if path.trim().is_empty() {
        return Err(FileError::InvalidPath(path.to_owned()));
    }
    let relative = Path::new(path);
    if relative.is_absolute() {
        return Err(FileError::InvalidPath(path.to_owned()));
    }
    if path
        .split(|character| character == '/' || character == '\\')
        .any(|part| part == "." || part == "..")
    {
        return Err(FileError::InvalidPath(path.to_owned()));
    }
    if relative.components().any(|component| {
        matches!(
            component,
            Component::ParentDir
                | Component::RootDir
                | Component::Prefix(_)
                | Component::CurDir
        )
    }) {
        return Err(FileError::InvalidPath(path.to_owned()));
    }
    Ok(())
}

fn slash_path(path: &Path) -> String {
    path.components()
        .filter_map(|component| match component {
            Component::Normal(value) => Some(value.to_string_lossy()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/")
}

fn display_name(path: &Path) -> String {
    path.file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("Workspace")
        .to_owned()
}

fn serialize_initial_file<S>(value: &Option<String>, serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    serializer.serialize_str(value.as_deref().unwrap_or(""))
}

#[cfg(test)]
mod tests {
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> Self {
            let suffix = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock should be after the Unix epoch")
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "oxidize-files-test-{}-{suffix}",
                std::process::id()
            ));
            fs::create_dir(&path).expect("test directory should be created");
            Self(path)
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn creates_renames_searches_and_deletes_entries() {
        let directory = TestDirectory::new();
        let workspace = open_workspace(&directory.0).expect("workspace should open");

        create_entry(&workspace, "src", NewEntryKind::Directory)
            .expect("directory should be created");
        create_entry(&workspace, "src/main.rs", NewEntryKind::File)
            .expect("file should be created");
        write_text_file(
            &workspace,
            "src/main.rs",
            "fn main() {\n    println!(\"hello\");\n}\n",
        )
        .expect("file should be written");

        let matches = search_workspace(&workspace, "HELLO", false).expect("search should succeed");
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].path, "src/main.rs");
        assert_eq!(matches[0].line, 2);

        rename_entry(&workspace, "src/main.rs", "src/app.rs").expect("file should be renamed");
        assert_eq!(
            read_text_file(&workspace, "src/app.rs")
                .unwrap()
                .content
                .lines()
                .count(),
            3
        );
        delete_entry(&workspace, "src/app.rs").expect("file should be deleted");
        assert!(matches!(
            read_text_file(&workspace, "src/app.rs"),
            Err(FileError::NotFound(_))
        ));
    }

    #[test]
    fn lists_directories_lazily_and_skips_generated_directories() {
        let directory = TestDirectory::new();
        fs::create_dir_all(directory.0.join("src/nested")).unwrap();
        fs::write(directory.0.join("src/main.rs"), "").unwrap();
        fs::write(directory.0.join("src/nested/deep.rs"), "").unwrap();
        fs::write(directory.0.join("notes.txt"), "").unwrap();
        fs::create_dir_all(directory.0.join("build")).unwrap();
        fs::write(directory.0.join("build/generated.rs"), "").unwrap();
        fs::create_dir_all(directory.0.join("cmake-build-debug")).unwrap();
        fs::create_dir_all(directory.0.join("out")).unwrap();
        fs::create_dir_all(directory.0.join(".cache")).unwrap();
        let workspace = open_workspace(&directory.0).unwrap();

        let root = list_tree(&workspace, "").unwrap();
        assert!(root
            .iter()
            .any(|entry| entry.path == "src" && matches!(entry.kind, EntryKind::Directory)));
        assert!(root
            .iter()
            .any(|entry| entry.path == "notes.txt" && matches!(entry.kind, EntryKind::File)));
        assert!(!root.iter().any(|entry| entry.path == "src/main.rs"));
        assert!(!root.iter().any(|entry| entry.path.starts_with("build")));
        assert!(!root.iter().any(|entry| entry.path.starts_with("out")));
        assert!(!root.iter().any(|entry| entry.path.starts_with(".cache")));
        assert!(!root
            .iter()
            .any(|entry| entry.path.starts_with("cmake-build-")));

        let source = list_tree(&workspace, "src").unwrap();
        assert_eq!(
            source.iter().map(|entry| entry.path.as_str()).collect::<Vec<_>>(),
            vec!["src/nested", "src/main.rs"]
        );
        assert!(list_tree(&workspace, "notes.txt").is_err());
        assert!(list_tree(&workspace, "../outside").is_err());

        let files = list_workspace_files(&workspace).unwrap();
        assert_eq!(
            files.iter().map(|entry| entry.path.as_str()).collect::<Vec<_>>(),
            vec!["notes.txt", "src/main.rs", "src/nested/deep.rs"]
        );

        assert!(should_skip("build"));
        assert!(should_skip("out"));
        assert!(should_skip(".cache"));
        assert!(should_skip("target"));
        assert!(should_skip("cmake-build-release"));
        assert!(!should_skip("src"));
    }

    #[test]
    fn rejects_paths_outside_the_workspace() {
        let directory = TestDirectory::new();
        fs::write(directory.0.join("safe.txt"), "safe").unwrap();
        let workspace = open_workspace(&directory.0).unwrap();

        assert!(matches!(
            read_text_file(&workspace, "../safe.txt"),
            Err(FileError::InvalidPath(_))
        ));
        assert!(matches!(
            create_entry(&workspace, "../escape.txt", NewEntryKind::File),
            Err(FileError::InvalidPath(_))
        ));
        assert!(matches!(
            read_text_file(&workspace, "./safe.txt"),
            Err(FileError::InvalidPath(_))
        ));
    }

    #[cfg(unix)]
    #[test]
    fn deleting_a_symlink_does_not_delete_its_target() {
        use std::os::unix::fs::symlink;

        let directory = TestDirectory::new();
        fs::create_dir(directory.0.join("target")).unwrap();
        fs::write(directory.0.join("target/keep.txt"), "keep").unwrap();
        symlink(directory.0.join("target"), directory.0.join("link")).unwrap();
        let workspace = open_workspace(&directory.0).unwrap();

        delete_entry(&workspace, "link").expect("symlink should be deleted");
        assert!(!directory.0.join("link").exists());
        assert!(directory.0.join("target/keep.txt").is_file());
    }
}
