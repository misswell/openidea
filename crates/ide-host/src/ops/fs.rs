use super::Channel;
use crate::Host;
use globset::GlobBuilder;
use ide_core::{CancellationToken, IdeError, IdeResult, RequestId};
use ide_vfs::FileEntry;
use serde::Serialize;
use std::path::{Path, PathBuf};

/// Largest chunk one RPC may append. The desktop streams a file in these
/// bounded pieces, so importing a large file does not turn into one unbounded
/// JSON-RPC frame or one large allocation on either side.
pub const REMOTE_IMPORT_CHUNK_LIMIT: usize = 1 << 20;

/// FileStat is the JSON shape Mayoi's `workspace.stat` API exposes.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginWorkspaceStat {
    pub is_dir: bool,
    pub size: u64,
    pub mtime: i64,
}

/// Reads a text file for a sandboxed plugin. The requested path must resolve
/// to a real file below root, including every existing symbolic link.
pub async fn plugin_workspace_read_text(
    host: &Host,
    root: PathBuf,
    path: String,
    request: Option<RequestId>,
) -> IdeResult<String> {
    let cancel = begin_optional_request(host, request);
    let result = host
        .blocking(move || {
            let real = ide_plugin::resolve_in_root(&root, &path).map_err(IdeError::invalid)?;
            if cancel.is_cancelled() {
                return Err(IdeError::cancelled());
            }
            let text = ide_plugin::read_text(&real).map_err(IdeError::invalid)?;
            if cancel.is_cancelled() {
                return Err(IdeError::cancelled());
            }
            Ok(text)
        })
        .await;
    end_optional_request(host, request);
    result
}

/// Stats one path for a sandboxed plugin, returning null when the target is
/// absent. Symlinks are followed only when their real target remains in root.
pub async fn plugin_workspace_stat(
    host: &Host,
    root: PathBuf,
    path: String,
    request: Option<RequestId>,
) -> IdeResult<Option<PluginWorkspaceStat>> {
    let cancel = begin_optional_request(host, request);
    let result = host
        .blocking(move || {
            if cancel.is_cancelled() {
                return Err(IdeError::cancelled());
            }
            let real = match ide_plugin::resolve_in_root(&root, &path) {
                Ok(real) => real,
                Err(message) if message.ends_with(": no such file") => return Ok(None),
                Err(message) => return Err(IdeError::invalid(message)),
            };
            let meta = std::fs::metadata(real)?;
            if cancel.is_cancelled() {
                return Err(IdeError::cancelled());
            }
            let modified = meta
                .modified()?
                .duration_since(std::time::UNIX_EPOCH)
                .map_err(|error| IdeError::internal(error.to_string()))?;
            Ok(Some(PluginWorkspaceStat {
                is_dir: meta.is_dir(),
                size: if meta.is_file() { meta.len() } else { 0 },
                mtime: i64::try_from(modified.as_millis()).unwrap_or(i64::MAX),
            }))
        })
        .await;
    end_optional_request(host, request);
    result
}

/// Replaces a plugin-owned workspace file atomically. The write limit and root
/// resolution match the plugin sandbox in `ide-plugin`; writing Git's own
/// administrative files is refused.
pub async fn plugin_workspace_write_text(
    host: &Host,
    root: PathBuf,
    path: String,
    text: String,
    request: Option<RequestId>,
) -> IdeResult<()> {
    let cancel = begin_optional_request(host, request);
    let result = host
        .blocking(move || {
            if text.len() > ide_plugin::MAX_READ_BYTES as usize {
                return Err(IdeError::invalid(format!(
                    "workspace.writeFile is limited to {} MiB",
                    ide_plugin::MAX_READ_BYTES >> 20
                )));
            }
            if cancel.is_cancelled() {
                return Err(IdeError::cancelled());
            }
            let real = ide_plugin::resolve_writable_in_root(&root, &path).map_err(IdeError::invalid)?;
            if let Some(parent) = real.parent() {
                std::fs::create_dir_all(parent)?;
            }
            if cancel.is_cancelled() {
                return Err(IdeError::cancelled());
            }
            ide_vfs::write_atomic_strict(&real, text.as_bytes())?;
            if cancel.is_cancelled() {
                return Err(IdeError::cancelled());
            }
            Ok(())
        })
        .await;
    end_optional_request(host, request);
    result
}

/// Finds workspace files by the Mayoi glob syntax. The project index supplies
/// the VCS-aware fast path; a cancelable ignore-aware walk covers the period
/// before the index is ready.
pub async fn plugin_workspace_find_files(
    host: &Host,
    window: &str,
    root: PathBuf,
    glob: String,
    limit: u32,
    request: Option<RequestId>,
) -> IdeResult<Vec<String>> {
    let cancel = begin_optional_request(host, request);
    let project = host.project(window).ok();
    let result = host
        .blocking(move || {
            find_workspace_files_sync(
                &root,
                project.as_ref().map(|project| project.index.clone()),
                &glob,
                limit,
                &cancel,
            )
        })
        .await;
    end_optional_request(host, request);
    result
}

fn begin_optional_request(host: &Host, request: Option<RequestId>) -> CancellationToken {
    match request {
        Some(request) => host.begin_request(request),
        None => CancellationToken::new(),
    }
}

fn end_optional_request(host: &Host, request: Option<RequestId>) {
    if let Some(request) = request {
        host.end_request(request);
    }
}

fn find_workspace_files_sync(
    root: &Path,
    indexed: Option<std::sync::Arc<ide_index::ProjectIndex>>,
    glob: &str,
    limit: u32,
    cancel: &CancellationToken,
) -> IdeResult<Vec<String>> {
    let real_root = dunce::canonicalize(root)?;
    let limit = usize::try_from(limit.clamp(1, 4096)).unwrap_or(4096);
    let matcher = workspace_glob(glob)?;
    let to_relative = |path: &Path| path.strip_prefix(&real_root).unwrap_or(path).to_string_lossy().replace('\\', "/");

    if let Some(index) = indexed
        && let Some(paths) = index.files_matching(&real_root, |relative| matcher.is_match(relative), limit)
    {
        let mut out = Vec::with_capacity(paths.len());
        for path in paths {
            if cancel.is_cancelled() {
                return Err(IdeError::cancelled());
            }
            out.push(to_relative(&path));
        }
        return Ok(out);
    }

    let mut out = Vec::new();
    let walker = ignore::WalkBuilder::new(&real_root).hidden(true).follow_links(false).build();
    for entry in walker {
        if cancel.is_cancelled() {
            return Err(IdeError::cancelled());
        }
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => continue,
        };
        if entry.path() == real_root || !entry.file_type().is_some_and(|kind| kind.is_file()) {
            continue;
        }
        let relative = entry.path().strip_prefix(&real_root).unwrap_or(entry.path());
        if matcher.is_match(relative) {
            out.push(relative.to_string_lossy().replace('\\', "/"));
            if out.len() >= limit {
                break;
            }
        }
    }
    out.sort();
    Ok(out)
}

fn workspace_glob(glob: &str) -> IdeResult<globset::GlobMatcher> {
    let glob = glob.trim().trim_start_matches("./");
    // A pattern without a slash names a file anywhere, matching Mayoi's
    // workspace glob semantics. Empty lists everything, and a slash anchors a
    // pattern at the project root.
    let pattern = if glob.is_empty() {
        "**".to_owned()
    } else if glob.contains('/') {
        glob.to_owned()
    } else {
        format!("**/{glob}")
    };
    Ok(GlobBuilder::new(&pattern)
        .literal_separator(true)
        .backslash_escape(false)
        .build()
        .map_err(|error| IdeError::invalid(format!("invalid workspace glob: {error}")))?
        .compile_matcher())
}

pub async fn entry_kind(host: &Host, path: PathBuf) -> IdeResult<ide_vfs::EntryKind> {
    host.blocking(move || {
        let meta = std::fs::metadata(path)?;
        Ok(if meta.is_dir() { ide_vfs::EntryKind::Dir } else { ide_vfs::EntryKind::File })
    })
    .await
}

pub async fn compare_directories(
    host: &Host,
    window: &str,
    left: PathBuf,
    right: PathBuf,
    request: RequestId,
    on_progress: Channel<ide_vfs::DirectoryCompareProgress>,
) -> IdeResult<Vec<ide_vfs::DirectoryCompareEntry>> {
    let project = host.project(window)?;
    let cancel = host.begin_request(request);
    let result = host
        .blocking(move || {
            let last = std::cell::Cell::new(std::time::Instant::now() - std::time::Duration::from_secs(1));
            ide_vfs::compare_directories(&left, &right, Some(&project.root), &project.exclude_globs(), &cancel, |p| {
                // Bound IPC traffic even when hashing multi-gigabyte files.
                if last.get().elapsed() >= std::time::Duration::from_millis(80) || p.total == Some(p.compared) {
                    let _ = on_progress.send(p);
                    last.set(std::time::Instant::now());
                }
            })
        })
        .await;
    host.end_request(request);
    result
}

pub async fn copy_merge(
    host: &Host,
    window: &str,
    source_root: PathBuf,
    destination_root: PathBuf,
    path: PathBuf,
    overwrite: bool,
) -> IdeResult<()> {
    if path.as_os_str().is_empty() || path.components().any(|c| !matches!(c, std::path::Component::Normal(_))) {
        return Err(ide_core::IdeError::invalid("expected a relative comparison entry path"));
    }
    let project = host.project(window)?;
    host.blocking(move || {
        let from = source_root.join(&path);
        let to = destination_root.join(&path);
        if overwrite {
            project.history.record_deletion(&to, DELETE_SNAPSHOT_LIMIT);
        }
        project.with_disk_sync(|| {
            ide_vfs::copy_merge(
                &from,
                &to,
                overwrite,
                Some(&project.root),
                &project.exclude_globs(),
                (&source_root, &destination_root),
            )
        })
    })
    .await
}

/// Files of a deleted folder snapshotted in Local History.
const DELETE_SNAPSHOT_LIMIT: usize = 2000;

/// Runs on the blocking pool: on a network share (`\\wsl.localhost`) every stat is slow.
pub async fn read_dir(host: &Host, window: &str, path: PathBuf) -> IdeResult<Vec<FileEntry>> {
    let project = host.project(window);
    host.blocking(move || {
        let excludes = project
            .map(|p| {
                let globs = p.exclude_globs();
                // `read_dir` matches relative to the enclosing repository: re-anchor
                // project-root patterns (`/build`) when the project is a repo subfolder.
                let repo = p.root.ancestors().find(|d| d.join(".git").exists());
                match repo.and_then(|r| p.root.strip_prefix(r).ok()).filter(|rel| !rel.as_os_str().is_empty()) {
                    Some(rel) => {
                        let rel = rel.to_string_lossy().replace('\\', "/");
                        globs.into_iter().map(|g| if g.starts_with('/') { format!("/{rel}{g}") } else { g }).collect()
                    }
                    None => globs,
                }
            })
            .unwrap_or_default();
        ide_vfs::read_dir(&path, &excludes)
    })
    .await
}

/// Creates an empty file; inside a project, honours the EditorConfig
/// `charset` (writes the BOM for `utf-8-bom` / UTF-16).
pub fn create_file(host: &Host, window: &str, path: PathBuf) -> IdeResult<()> {
    match host.project(window) {
        Ok(p) => {
            let defaults = p.documents.file_defaults();
            if defaults.editorconfig {
                ide_workspace::editorconfig::create_file(p.documents.editorconfig(), &path)?;
            } else {
                ide_vfs::create_file(&path)?;
            }
            // No EditorConfig charset: File Encodings › "Create UTF-8 files with BOM".
            let empty = std::fs::metadata(&path).map(|m| m.len() == 0).unwrap_or(false);
            let bom = defaults.new_file_bytes();
            if empty && !bom.is_empty() {
                std::fs::write(&path, bom)?;
            }
            // A checkout backend opens the file for add; Git's hook is a no-op.
            if let Err(e) = p.with_backend_of(&path, |r| r.files_added(std::slice::from_ref(&path))) {
                tracing::debug!("vcs files_added for {}: {e}", path.display());
            }
            Ok(())
        }
        Err(_) => ide_vfs::create_file(&path),
    }
}

pub async fn create_file_bytes(host: &Host, window: &str, path: PathBuf, data: Vec<u8>) -> IdeResult<()> {
    let project = host.project(window).ok();
    host.blocking(move || {
        ide_vfs::create_file_bytes(&path, &data)?;
        if let Some(p) = project
            && let Err(e) = p.with_backend_of(&path, |r| r.files_added(std::slice::from_ref(&path)))
        {
            tracing::debug!("vcs files_added for {}: {e}", path.display());
        }
        Ok(())
    })
    .await
}

/// Creates one directory without accepting an existing target or creating
/// missing parents. Remote import uses this at every folder boundary so a
/// concurrent writer cannot turn an import into an accidental merge.
pub async fn create_dir_exclusive(host: &Host, path: PathBuf) -> IdeResult<()> {
    host.blocking(move || create_dir_exclusive_sync(&path)).await
}

fn create_dir_exclusive_sync(path: &Path) -> IdeResult<()> {
    if std::fs::symlink_metadata(path).is_ok() {
        return Err(already_exists(path));
    }
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .ok_or_else(|| IdeError::invalid(format!("{} has no parent folder", path.display())))?;
    let meta =
        std::fs::metadata(parent).map_err(|_| IdeError::not_found(format!("{} does not exist", parent.display())))?;
    if !meta.is_dir() {
        return Err(IdeError::invalid(format!("{} is not a folder", parent.display())));
    }
    std::fs::create_dir(path)?;
    Ok(())
}

/// Appends one bounded chunk to a file created by the same import. The caller
/// uploads under a private temporary name and renames only after every chunk
/// and the source permissions have landed.
pub async fn append_file_bytes(host: &Host, path: PathBuf, data: Vec<u8>, request: RequestId) -> IdeResult<()> {
    let cancel = host.begin_request(request);
    let result = host.blocking(move || append_file_bytes_sync(&path, &data, &cancel)).await;
    host.end_request(request);
    result
}

fn append_file_bytes_sync(path: &Path, data: &[u8], cancel: &CancellationToken) -> IdeResult<()> {
    if data.len() > REMOTE_IMPORT_CHUNK_LIMIT {
        return Err(IdeError::invalid(format!(
            "import chunk is {} bytes; the limit is {REMOTE_IMPORT_CHUNK_LIMIT}",
            data.len()
        )));
    }
    if cancel.is_cancelled() {
        return Err(IdeError::cancelled());
    }
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new().append(true).open(path)?;
    file.write_all(data)?;
    file.sync_data()?;
    if cancel.is_cancelled() {
        return Err(IdeError::cancelled());
    }
    Ok(())
}

/// Creates one symbolic link without replacing an existing target.
pub async fn create_symlink(host: &Host, path: PathBuf, target: String) -> IdeResult<()> {
    host.blocking(move || create_symlink_sync(&path, &target)).await
}

fn create_symlink_sync(path: &Path, target: &str) -> IdeResult<()> {
    if std::fs::symlink_metadata(path).is_ok() {
        return Err(already_exists(path));
    }
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .ok_or_else(|| IdeError::invalid(format!("{} has no parent folder", path.display())))?;
    let meta =
        std::fs::metadata(parent).map_err(|_| IdeError::not_found(format!("{} does not exist", parent.display())))?;
    if !meta.is_dir() {
        return Err(IdeError::invalid(format!("{} is not a folder", parent.display())));
    }
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(target, path)?;
        Ok(())
    }
    #[cfg(not(unix))]
    {
        let _ = (path, target);
        Err(IdeError::unavailable("symbolic-link import is not supported by this server platform"))
    }
}

/// Applies the source file's Unix mode after all chunks arrive. Modes are
/// limited to file-type-independent permission bits (`07777`).
pub async fn set_permissions(host: &Host, path: PathBuf, mode: u32) -> IdeResult<()> {
    host.blocking(move || set_permissions_sync(&path, mode)).await
}

fn set_permissions_sync(path: &Path, mode: u32) -> IdeResult<()> {
    if mode & !0o7777 != 0 {
        return Err(IdeError::invalid(format!("invalid file mode {mode:#o}")));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))?;
        Ok(())
    }
    #[cfg(not(unix))]
    {
        let _ = (path, mode);
        Err(IdeError::unavailable("preserving Unix file modes is not supported by this server platform"))
    }
}

/// Permanently removes one non-folder created for an import temporary. Unlike
/// `delete`, this never moves the file to the user's trash.
pub async fn remove_file(host: &Host, path: PathBuf) -> IdeResult<()> {
    host.blocking(move || remove_file_sync(&path)).await
}

fn remove_file_sync(path: &Path) -> IdeResult<()> {
    let meta = std::fs::symlink_metadata(path)?;
    if meta.is_dir() {
        return Err(IdeError::invalid(format!("{} is a folder", path.display())));
    }
    std::fs::remove_file(path)?;
    Ok(())
}

/// Permanently removes a file or folder tree created for an import temporary.
/// Unlike `delete`, this never moves any part of the tree to the user's trash.
pub async fn remove_tree(host: &Host, path: PathBuf) -> IdeResult<()> {
    host.blocking(move || remove_tree_sync(&path)).await
}

fn remove_tree_sync(path: &Path) -> IdeResult<()> {
    let meta = std::fs::symlink_metadata(path)?;
    if meta.is_dir() {
        std::fs::remove_dir_all(path)?;
    } else {
        std::fs::remove_file(path)?;
    }
    Ok(())
}

fn already_exists(path: &Path) -> IdeError {
    IdeError::new(ide_core::error::ErrorKind::AlreadyExists, format!("{} already exists", path.display()))
}

/// Toggle Read-only Attribute: sets the writable bit (and the open document's
/// state). Returns whether the file is read-only now.
pub fn set_writable(host: &Host, window: &str, path: PathBuf, writable: bool) -> IdeResult<bool> {
    match host.project(window) {
        Ok(p) => p.documents.set_writable(&path, writable),
        Err(_) => ide_vfs::set_writable(&path, writable).map(|()| !writable),
    }
}

/// Moves to the trash; Local History keeps the contents so deleted files can be restored.
pub async fn delete(host: &Host, window: &str, path: PathBuf) -> IdeResult<()> {
    let project = host.project(window).ok();
    let history = project.as_ref().map(|p| p.history.clone());
    host.blocking(move || {
        let paths = ide_workspace::file_ops::deletion_paths(&path)?;
        if let Some(h) = history {
            for p in &paths {
                h.record_deletion(p, DELETE_SNAPSHOT_LIMIT);
            }
        }
        ide_workspace::file_ops::delete(&path)?;
        // A checkout backend opens the paths for delete; Git's hook is a no-op.
        if let Some(p) = project {
            let _ = p.with_backend_of(&path, |r| r.files_deleted(&paths));
        }
        Ok(())
    })
    .await
}

pub async fn copy(host: &Host, window: &str, from: PathBuf, to: PathBuf) -> IdeResult<()> {
    let project = host.project(window).ok();
    host.blocking(move || {
        ide_workspace::file_ops::copy(&from, &to)?;
        // A copy is an add at the destination; Git's hook is a no-op.
        if let Some(p) = project
            && let Err(e) = p.with_backend_of(&to, |r| r.files_added(std::slice::from_ref(&to)))
        {
            tracing::debug!("vcs files_added for {}: {e}", to.display());
        }
        Ok(())
    })
    .await
}

pub async fn rename(host: &Host, window: &str, from: PathBuf, to: PathBuf) -> IdeResult<()> {
    let project = host.project(window).ok();
    host.blocking(move || {
        ide_workspace::file_ops::rename(&from, &to)?;
        // A checkout backend records the rename so history follows the file;
        // Git's hook is a no-op.
        if let Some(p) = project
            && let Err(e) = p.with_backend_of(&to, |r| r.file_moved(&from, &to))
        {
            tracing::debug!("vcs file_moved {} → {}: {e}", from.display(), to.display());
        }
        Ok(())
    })
    .await
}

/// Reads a text file without opening it as a document; prefers the unsaved buffer.
pub async fn read_text(host: &Host, window: &str, path: PathBuf) -> IdeResult<String> {
    let project = host.project(window).ok();
    host.blocking(move || match project {
        Some(p) => p.text_of(&path),
        None => Ok(ide_vfs::load_text(&path)?.text),
    })
    .await
}

/// The file `window` may hand to the OS's default application: `path` itself, resolved, when it is a
/// file inside the window's project (its folder or a content root). The window must be trusted and
/// open locally; a remote window's files live on its own machine and are not launched here.
pub fn system_open_target(host: &Host, window: &str, path: PathBuf) -> IdeResult<PathBuf> {
    host.require_trust(window)?;
    let project = host.project(window)?;
    let real =
        dunce::canonicalize(&path).map_err(|_| IdeError::not_found(format!("{}: no such file", path.display())))?;
    if !real.is_file() {
        return Err(IdeError::invalid(format!("{}: not a file", path.display())));
    }
    let roots: Vec<PathBuf> = std::iter::once(project.root.clone())
        .chain(project.content_roots())
        .filter_map(|root| dunce::canonicalize(root).ok())
        .collect();
    if !inside_any_root(&real, &roots) {
        return Err(IdeError::invalid(format!("{}: outside the project", path.display())));
    }
    Ok(real)
}

/// Whether `path` lies inside one of `roots`, comparing whole path components (`/a/bc` is not inside `/a/b`).
pub(crate) fn inside_any_root(path: &Path, roots: &[PathBuf]) -> bool {
    roots.iter().any(|root| path.starts_with(root))
}

#[cfg(test)]
mod tests {
    use super::{
        REMOTE_IMPORT_CHUNK_LIMIT, append_file_bytes_sync, create_dir_exclusive_sync, create_symlink_sync,
        find_workspace_files_sync, inside_any_root, remove_file_sync, remove_tree_sync, set_permissions_sync,
        workspace_glob,
    };
    use ide_core::CancellationToken;
    use std::fs;
    use std::path::{Path, PathBuf};

    #[test]
    fn system_open_is_limited_to_paths_inside_a_project_root() {
        let roots = vec![PathBuf::from("/work/app"), PathBuf::from("/work/shared")];
        assert!(inside_any_root(Path::new("/work/app/docs/spec.pdf"), &roots));
        assert!(inside_any_root(Path::new("/work/shared/a.docx"), &roots));
        assert!(inside_any_root(Path::new("/work/app"), &roots));
        assert!(!inside_any_root(Path::new("/work/app2/spec.pdf"), &roots));
        assert!(!inside_any_root(Path::new("/home/me/.ssh/id_rsa"), &roots));
        assert!(!inside_any_root(Path::new("/work/spec.pdf"), &[]));
    }

    #[test]
    fn exclusive_directory_and_file_chunks_form_an_atomic_import() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("bundle");
        create_dir_exclusive_sync(&dir).unwrap();
        assert!(create_dir_exclusive_sync(&dir).is_err(), "an existing folder must not be merged");
        assert!(
            create_dir_exclusive_sync(&tmp.path().join("missing/child")).is_err(),
            "missing parents must not be created implicitly"
        );

        let file = dir.join("payload.bin");
        fs::OpenOptions::new().write(true).create_new(true).open(&file).unwrap();
        let cancel = CancellationToken::new();
        append_file_bytes_sync(&file, b"abc", &cancel).unwrap();
        append_file_bytes_sync(&file, b"def", &cancel).unwrap();
        assert_eq!(fs::read(&file).unwrap(), b"abcdef");

        let too_large = vec![0; REMOTE_IMPORT_CHUNK_LIMIT + 1];
        assert!(append_file_bytes_sync(&file, &too_large, &cancel).is_err());
        cancel.cancel();
        assert!(append_file_bytes_sync(&file, b"late", &cancel).is_err());
    }

    #[test]
    fn symlink_permissions_and_permanent_cleanup_do_not_replace_entries() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("file.txt");
        fs::write(&file, b"payload").unwrap();
        set_permissions_sync(&file, 0o640).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(fs::metadata(&file).unwrap().permissions().mode() & 0o777, 0o640);
        }

        let link = tmp.path().join("link.txt");
        create_symlink_sync(&link, "file.txt").unwrap();
        assert!(fs::symlink_metadata(&link).unwrap().file_type().is_symlink());
        assert!(create_symlink_sync(&link, "other.txt").is_err());

        remove_file_sync(&link).unwrap();
        assert!(fs::symlink_metadata(&link).is_err());
        assert!(remove_file_sync(tmp.path()).is_err(), "permanent cleanup must not remove folders");
        remove_tree_sync(tmp.path()).unwrap();
        assert!(!tmp.path().exists());
    }

    #[test]
    fn workspace_globs_and_fallback_walk_match_mayoi_semantics() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("Root.java"), b"root").unwrap();
        fs::create_dir_all(tmp.path().join("src/main")).unwrap();
        fs::write(tmp.path().join("src/main/App.java"), b"app").unwrap();
        fs::write(tmp.path().join("src/main/notes.txt"), b"notes").unwrap();

        assert!(workspace_glob("*.java").unwrap().is_match("src/main/App.java"));
        assert!(workspace_glob("src/**/*.java").unwrap().is_match("src/main/App.java"));
        assert!(!workspace_glob("src/*.java").unwrap().is_match("src/main/App.java"));

        let paths = find_workspace_files_sync(tmp.path(), None, "*.java", 10, &CancellationToken::new()).unwrap();
        assert_eq!(paths, vec!["Root.java", "src/main/App.java"]);
    }

    #[cfg(unix)]
    #[test]
    fn plugin_workspace_paths_cannot_escape_through_symlinks() {
        use std::os::unix::fs::symlink;

        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("project");
        fs::create_dir(&root).unwrap();
        let outside = tmp.path().join("secret.txt");
        fs::write(&outside, b"secret").unwrap();
        symlink(&outside, root.join("secret-link")).unwrap();
        symlink(tmp.path(), root.join("outside-dir")).unwrap();

        assert!(ide_plugin::resolve_in_root(&root, "secret-link").is_err());
        assert!(ide_plugin::resolve_writable_in_root(&root, "secret-link").is_err());
        assert!(ide_plugin::resolve_writable_in_root(&root, "outside-dir/new.txt").is_err());
        assert!(ide_plugin::resolve_writable_in_root(&root, ".git/config").is_err());
    }
}
