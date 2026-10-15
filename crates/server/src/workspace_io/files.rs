//! Listing directories and reading and writing text files, in a workspace
//! and in the host directories a session was granted.

use super::*;

pub async fn list_files(root: &Path, relative: &str) -> Result<Vec<FileEntry>, IoError> {
    let root = root.to_path_buf();
    let relative = relative.to_owned();
    tokio::task::spawn_blocking(move || {
        let canonical_root = dunce::canonicalize(&root).map_err(IoError::from)?;
        let directory = safe_path(&canonical_root, &relative)?;
        let metadata = fs::metadata(&directory).map_err(IoError::from)?;
        if !metadata.is_dir() {
            return Err(IoError::new(400, "path is not a directory"));
        }
        let mut entries = Vec::new();
        if is_protected_path(
            directory
                .strip_prefix(&canonical_root)
                .unwrap_or(&directory),
        ) {
            return Err(IoError::new(403, "protected directory"));
        }
        for (index, item) in fs::read_dir(&directory).map_err(IoError::from)?.enumerate() {
            if index >= 10000 {
                return Err(IoError::new(413, "Directory exceeds 10,000 entries"));
            }

            let item = item.map_err(IoError::from)?;
            let path = item.path();
            let lexical_metadata = match fs::symlink_metadata(&path) {
                Ok(meta) => meta,
                Err(_) => continue,
            };
            let Ok(safe) = safe_path(&canonical_root, &relative_string(&canonical_root, &path))
            else {
                continue; // Hide symlinks that leave the workspace.
            };
            let is_symlink = lexical_metadata.file_type().is_symlink();
            let metadata = match fs::metadata(&safe) {
                Ok(meta) => meta,
                Err(_) => continue,
            };
            // Preserve the user's lexical symlink path in listings while still
            // validating its target via `safe_path` above.
            let rel = relative_string(&canonical_root, &path);
            if is_protected_path(Path::new(&rel))
                || is_protected_path(safe.strip_prefix(&canonical_root).unwrap_or(&safe))
            {
                continue;
            }
            entries.push(FileEntry {
                name: item.file_name().to_string_lossy().into_owned(),
                path: rel,
                kind: if is_symlink {
                    "symlink"
                } else if metadata.is_dir() {
                    "directory"
                } else if metadata.is_file() {
                    "file"
                } else {
                    "other"
                }
                .into(),
                size: metadata.len(),
            });
        }
        entries.sort_by(|a, b| {
            (a.kind != "directory")
                .cmp(&(b.kind != "directory"))
                .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
        });
        Ok(entries)
    })
    .await
    .map_err(|error| IoError::new(500, format!("file listing task failed: {error}")))?
}

pub async fn read_file(root: &Path, path: &str) -> Result<TextFile, IoError> {
    let root = root.to_path_buf();
    let path = path.to_owned();
    tokio::task::spawn_blocking(move || {
        let canonical_root = dunce::canonicalize(&root).map_err(IoError::from)?;
        let rel = safe_relative(&path)?;
        let target = asset_path(&canonical_root, &path)?;
        if is_protected_path(&rel) {
            return Err(IoError::new(403, "protected path"));
        }
        let metadata = fs::metadata(&target).map_err(IoError::from)?;
        if !metadata.is_file() {
            return Err(IoError::new(400, "path is not a file"));
        }
        if metadata.len() > MAX_TEXT_BYTES {
            return Err(IoError::new(413, "text file exceeds 2 MiB limit"));
        }
        let bytes = fs::read(&target).map_err(IoError::from)?;
        if bytes.len() as u64 > MAX_TEXT_BYTES {
            return Err(IoError::new(413, "text file exceeds 2 MiB limit"));
        }
        let content = String::from_utf8(bytes.clone())
            .map_err(|_| IoError::new(415, "file is not valid UTF-8"))?;
        Ok(TextFile {
            path: rel.to_string_lossy().replace('\\', "/"),
            content,
            version: hash_version(&bytes),
        })
    })
    .await
    .map_err(|error| IoError::new(500, format!("file read task failed: {error}")))?
}

/// Read-only text of a file outside every workspace, for a path an agent
/// mentioned. Held to the same boundary as the folder picker -- inside a
/// browsing root -- and the same refusals as workspace reads: protected
/// names, non-text, and anything over the size limit.
/// A file outside every workspace that may be read: under one of `roots`,
/// not a credential or configuration path, and a regular file.
pub fn resolve_host_file(
    roots: &[PathBuf],
    path: &str,
) -> Result<(PathBuf, fs::Metadata), IoError> {
    let requested = Path::new(path);
    if !requested.is_absolute() {
        return Err(IoError::new(400, "an absolute path is required"));
    }
    let target = dunce::canonicalize(requested).map_err(IoError::from)?;
    let root = roots
        .iter()
        .find(|root| target.starts_with(root))
        .ok_or_else(|| IoError::new(403, "path is outside the folders Tendly may browse"))?;
    let rel = target.strip_prefix(root).unwrap_or(&target);
    if is_protected_path(rel) {
        return Err(IoError::new(403, "protected path"));
    }
    let metadata = fs::metadata(&target).map_err(IoError::from)?;
    if !metadata.is_file() {
        return Err(IoError::new(400, "path is not a file"));
    }
    Ok((target, metadata))
}

pub async fn read_host_file(roots: &[PathBuf], path: &str) -> Result<TextFile, IoError> {
    let roots = roots.to_vec();
    let path = path.to_owned();
    tokio::task::spawn_blocking(move || {
        let (target, metadata) = resolve_host_file(&roots, &path)?;
        if metadata.len() > MAX_TEXT_BYTES {
            return Err(IoError::new(413, "text file exceeds 2 MiB limit"));
        }
        let bytes = fs::read(&target).map_err(IoError::from)?;
        if bytes.len() as u64 > MAX_TEXT_BYTES {
            return Err(IoError::new(413, "text file exceeds 2 MiB limit"));
        }
        let content = String::from_utf8(bytes.clone())
            .map_err(|_| IoError::new(415, "file is not valid UTF-8"))?;
        Ok(TextFile {
            path: target.to_string_lossy().into_owned(),
            content,
            version: hash_version(&bytes),
        })
    })
    .await
    .map_err(|error| IoError::new(500, format!("file read task failed: {error}")))?
}

pub async fn write_file(
    root: &Path,
    path: &str,
    content: &str,
    expected_version: Option<&str>,
) -> Result<TextFile, IoError> {
    if content.len() as u64 > MAX_TEXT_BYTES {
        return Err(IoError::new(413, "text file exceeds 2 MiB limit"));
    }
    let root = root.to_path_buf();
    let path = path.to_owned();
    let content = content.to_owned();
    let expected_version = expected_version.map(str::to_owned);
    tokio::task::spawn_blocking(move || {
        let canonical_root = dunce::canonicalize(&root).map_err(IoError::from)?;
        let rel = safe_relative(&path)?;
        if rel.as_os_str().is_empty() {
            return Err(IoError::new(400, "file path is required"));
        }
        if is_protected_path(&rel) {
            return Err(IoError::new(403, "protected path cannot be written"));
        }
        if has_symlink_component(&canonical_root, &rel)? {
            return Err(IoError::new(403, "symlink writes are not allowed"));
        }
        // Inspect the lexical target before canonicalisation so a symlink to a
        // location inside the workspace cannot be silently overwritten.
        let lexical_target = canonical_root.join(&rel);
        if let Ok(meta) = fs::symlink_metadata(&lexical_target)
            && meta.file_type().is_symlink()
        {
            return Err(IoError::new(403, "symlink writes are not allowed"));
        }
        let target = safe_path(&canonical_root, &path)?;
        if let Ok(meta) = fs::symlink_metadata(&target) {
            if meta.file_type().is_symlink() {
                return Err(IoError::new(403, "symlink writes are not allowed"));
            }
            if !meta.is_file() {
                return Err(IoError::new(400, "path is not a regular file"));
            }
            if meta.len() > MAX_TEXT_BYTES {
                return Err(IoError::new(413, "Text exceeds 2 MiB limit"));
            }
            if expected_version.is_none() {
                return Err(IoError::new(
                    409,
                    "Read the current version before overwriting an existing file",
                ));
            }
            let existing = fs::read(&target).map_err(IoError::from)?;
            let actual = hash_version(&existing);
            if let Some(expected) = expected_version.as_deref()
                && expected != actual
            {
                return Err(IoError::new(409, "file changed since it was read"));
            }
        } else if expected_version.is_some() {
            return Err(IoError::new(409, "file no longer exists"));
        }
        let parent = target
            .parent()
            .ok_or_else(|| IoError::new(400, "invalid file path"))?;
        fs::create_dir_all(parent).map_err(IoError::from)?;
        let temp = parent.join(format!(".tendly-{}.tmp", Uuid::new_v4()));
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        let mut file = options.open(&temp).map_err(IoError::from)?;
        use io::Write;
        if let Err(error) = file
            .write_all(content.as_bytes())
            .and_then(|_| file.sync_all())
        {
            let _ = fs::remove_file(&temp);
            return Err(IoError::from(error));
        }
        if let Ok(meta) = fs::metadata(&target) {
            let _ = fs::set_permissions(&temp, meta.permissions());
        }
        if let Err(error) = fs::rename(&temp, &target) {
            let _ = fs::remove_file(&temp);
            return Err(IoError::from(error));
        }
        let bytes = content.into_bytes();
        Ok(TextFile {
            path: rel.to_string_lossy().replace('\\', "/"),
            content: String::from_utf8(bytes.clone()).expect("input is UTF-8"),
            version: hash_version(&bytes),
        })
    })
    .await
    .map_err(|error| IoError::new(500, format!("file write task failed: {error}")))?
}
