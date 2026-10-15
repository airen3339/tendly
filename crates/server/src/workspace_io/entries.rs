//! Creating, renaming and deleting files and directories from the file tree.

use super::*;

/// Make an empty folder, and any folders above it the path names. A name that
/// is already taken -- by a file or a folder -- is refused, like a new file.
pub async fn create_directory(root: &Path, relative: &str) -> Result<FileEntry, IoError> {
    let root = root.to_path_buf();
    let relative = relative.to_owned();
    tokio::task::spawn_blocking(move || {
        let canonical_root = dunce::canonicalize(&root).map_err(IoError::from)?;
        let rel = safe_relative(&relative)?;
        if rel.as_os_str().is_empty() {
            return Err(IoError::new(400, "folder name is required"));
        }
        if is_protected_path(&rel) {
            return Err(IoError::new(403, "this path is protected"));
        }
        if has_symlink_component(&canonical_root, &rel)? {
            return Err(IoError::new(403, "symlink writes are not allowed"));
        }
        let target = canonical_root.join(&rel);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).map_err(IoError::from)?;
        }
        // `create_dir`, not `create_dir_all`: it is what refuses a name taken
        // between looking and making.
        fs::create_dir(&target).map_err(|error| match error.kind() {
            io::ErrorKind::AlreadyExists => {
                IoError::new(409, "something with this name already exists")
            }
            _ => IoError::from(error),
        })?;
        Ok(FileEntry {
            name: rel
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default(),
            path: rel.to_string_lossy().replace('\\', "/"),
            kind: "directory".into(),
            size: 0,
        })
    })
    .await
    .map_err(|error| IoError::new(500, format!("folder create task failed: {error}")))?
}

/// Delete one file or directory inside a workspace.
///
/// `safe_path` already refuses anything that resolves outside the root or into
/// a protected name. Two things it cannot decide for a delete are handled here:
/// the root itself is never removable, and a symlink is unlinked rather than
/// followed — otherwise deleting a link inside the workspace would delete
/// whatever it points at.
/// Create an empty file the workspace does not have yet.
///
/// Writing would serve for a file that is new, but not for one that is not: an
/// overwrite is what a blank new file looks like to `write_file`, and losing
/// what was there is the one outcome this must never have. Refusing an existing
/// name says so instead.
///
/// Missing parent directories are created, so naming `docs/notes.md` in a
/// workspace without a `docs` makes both, the way typing the path suggests.
pub async fn create_file(root: &Path, relative: &str) -> Result<FileEntry, IoError> {
    let root = root.to_path_buf();
    let relative = relative.to_owned();
    tokio::task::spawn_blocking(move || {
        let canonical_root = dunce::canonicalize(&root).map_err(IoError::from)?;
        let rel = safe_relative(&relative)?;
        if rel.as_os_str().is_empty() {
            return Err(IoError::new(400, "file name is required"));
        }
        if is_protected_path(&rel) {
            return Err(IoError::new(403, "this path is protected"));
        }
        if has_symlink_component(&canonical_root, &rel)? {
            return Err(IoError::new(403, "symlink writes are not allowed"));
        }
        let target = canonical_root.join(&rel);
        if fs::symlink_metadata(&target).is_ok() {
            return Err(IoError::new(409, "something with this name already exists"));
        }
        let parent = target
            .parent()
            .ok_or_else(|| IoError::new(400, "invalid file path"))?;
        fs::create_dir_all(parent).map_err(IoError::from)?;
        // `create_new` is the check: between looking and creating, someone else
        // may have made this name, and this is what refuses rather than truncates.
        fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&target)
            .map_err(|error| match error.kind() {
                io::ErrorKind::AlreadyExists => {
                    IoError::new(409, "something with this name already exists")
                }
                _ => IoError::from(error),
            })?;
        Ok(FileEntry {
            name: rel
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default(),
            path: rel.to_string_lossy().replace('\\', "/"),
            kind: "file".into(),
            size: 0,
        })
    })
    .await
    .map_err(|error| IoError::new(500, format!("file create task failed: {error}")))?
}

/// Give an existing file or directory another name, or another place in this
/// workspace.
///
/// Both ends are checked the same way, because a rename is two paths and either
/// one leaving the workspace, naming protected state, or passing through a link
/// would be a way out of it. An occupied destination is refused rather than
/// replaced: renaming onto a name is not a request to destroy what holds it.
pub async fn rename_entry(root: &Path, from: &str, to: &str) -> Result<FileEntry, IoError> {
    let root = root.to_path_buf();
    let from = from.to_owned();
    let to = to.to_owned();
    tokio::task::spawn_blocking(move || {
        let canonical_root = dunce::canonicalize(&root).map_err(IoError::from)?;
        let source = safe_relative(&from)?;
        let destination = safe_relative(&to)?;
        if source.as_os_str().is_empty() || destination.as_os_str().is_empty() {
            return Err(IoError::new(400, "both names are required"));
        }
        if is_protected_path(&source) || is_protected_path(&destination) {
            return Err(IoError::new(403, "this path is protected"));
        }
        if source == destination {
            return Err(IoError::new(400, "the name is unchanged"));
        }
        // The source's own final component may be a link -- renaming one is
        // renaming the link, not what it points at -- but a link anywhere above
        // either path would take the rename outside the workspace.
        if has_symlink_component(&canonical_root, source.parent().unwrap_or(Path::new("")))?
            || has_symlink_component(&canonical_root, &destination)?
        {
            return Err(IoError::new(403, "symlink writes are not allowed"));
        }
        let source_path = canonical_root.join(&source);
        let destination_path = canonical_root.join(&destination);
        let metadata = fs::symlink_metadata(&source_path).map_err(|error| match error.kind() {
            io::ErrorKind::NotFound => IoError::new(404, "this file no longer exists"),
            _ => IoError::from(error),
        })?;
        if fs::symlink_metadata(&destination_path).is_ok() {
            return Err(IoError::new(409, "something with this name already exists"));
        }
        let parent = destination_path
            .parent()
            .ok_or_else(|| IoError::new(400, "invalid file path"))?;
        if !parent.is_dir() {
            return Err(IoError::new(404, "the destination folder does not exist"));
        }
        fs::rename(&source_path, &destination_path).map_err(IoError::from)?;
        Ok(FileEntry {
            name: destination
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default(),
            path: destination.to_string_lossy().replace('\\', "/"),
            kind: entry_kind(&metadata),
            size: if metadata.is_file() {
                metadata.len()
            } else {
                0
            },
        })
    })
    .await
    .map_err(|error| IoError::new(500, format!("file rename task failed: {error}")))?
}

pub async fn delete_entry(root: &Path, relative: &str) -> Result<(), IoError> {
    let canonical = safe_path(root, relative)?;
    let canonical_root = dunce::canonicalize(root).map_err(IoError::from)?;
    if canonical == canonical_root {
        return Err(IoError::new(400, "the workspace root cannot be deleted"));
    }
    if is_protected_path(&safe_relative(relative)?) {
        return Err(IoError::new(403, "this path is protected"));
    }
    let candidate = canonical_root.join(safe_relative(relative)?);
    let link = fs::symlink_metadata(&candidate)
        .map(|meta| meta.file_type().is_symlink())
        .unwrap_or(false);
    let target = if link { candidate } else { canonical };
    let metadata = fs::symlink_metadata(&target).map_err(IoError::from)?;
    if metadata.file_type().is_dir() {
        tokio::fs::remove_dir_all(&target)
            .await
            .map_err(IoError::from)
    } else {
        tokio::fs::remove_file(&target).await.map_err(IoError::from)
    }
}
