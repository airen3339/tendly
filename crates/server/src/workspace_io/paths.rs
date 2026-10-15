//! Resolving workspace-relative paths without escaping the root, following
//! symlinks out of it, or touching files Tendly keeps for itself.

use super::*;

/// Resolve a relative workspace path while preventing traversal and symlink
/// escapes. Existing paths are canonicalised; for a new path the canonical
/// parent is checked and the original (non-existent) path is returned.
pub fn safe_path(root: &Path, relative: &str) -> Result<PathBuf, IoError> {
    let canonical_root = dunce::canonicalize(root).map_err(IoError::from)?;
    if !canonical_root.is_dir() {
        return Err(IoError::new(400, "workspace root is not a directory"));
    }
    let rel = safe_relative(relative)?;
    let candidate = canonical_root.join(&rel);
    match fs::symlink_metadata(&candidate) {
        Ok(_) => {
            let canonical = dunce::canonicalize(&candidate).map_err(IoError::from)?;
            if !canonical.starts_with(&canonical_root) {
                return Err(IoError::new(403, "path escapes workspace root"));
            }
            Ok(canonical)
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            // For a new file (or a new nested directory), walk up to the
            // nearest existing ancestor and canonicalise that component. This
            // still catches a symlinked ancestor while allowing callers to
            // create `src/new/file.txt` in one operation.
            let mut parent = candidate
                .parent()
                .ok_or_else(|| IoError::new(400, "invalid path"))?;
            while !parent.exists() {
                parent = parent
                    .parent()
                    .ok_or_else(|| IoError::new(400, "invalid path"))?;
            }
            let canonical_parent = dunce::canonicalize(parent).map_err(IoError::from)?;
            if !canonical_parent.starts_with(&canonical_root)
                || is_protected_path(
                    canonical_parent
                        .strip_prefix(&canonical_root)
                        .unwrap_or(&canonical_parent),
                )
            {
                return Err(IoError::new(403, "path escapes workspace root"));
            }
            Ok(candidate)
        }
        Err(error) => Err(IoError::from(error)),
    }
}

pub(crate) fn safe_relative(relative: &str) -> Result<PathBuf, IoError> {
    if relative.as_bytes().contains(&0) {
        return Err(IoError::new(400, "path contains NUL"));
    }
    let path = Path::new(relative);
    if path.is_absolute() {
        return Err(IoError::new(400, "absolute paths are not allowed"));
    }
    let mut result = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::Normal(part) => result.push(part),
            Component::ParentDir => return Err(IoError::new(400, "path traversal is not allowed")),
            Component::RootDir | Component::Prefix(_) => {
                return Err(IoError::new(400, "absolute paths are not allowed"));
            }
        }
    }
    Ok(result)
}

pub(super) fn relative_string(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

pub(crate) fn is_protected_path(relative: &Path) -> bool {
    // Tendly must never mutate repository metadata, credentials, or
    // provider configuration.  These names are intentionally conservative.
    const PROTECTED: &[&str] = &[
        ".git",
        ".tendly",
        ".ssh",
        ".aws",
        ".azure",
        ".config",
        "credentials",
        "credential",
        "secrets",
    ];
    relative.components().any(|component| {
        let Component::Normal(part) = component else {
            return false;
        };
        let value = part.to_string_lossy();
        PROTECTED
            .iter()
            .chain(crate::adapters::all().flat_map(|client| client.protected_names()))
            .any(|name| value == *name)
    })
}

/// What the tree calls an entry, from the metadata of the entry itself. A link
/// is a link here rather than what it resolves to, which is what a rename acts
/// on and what the tree already shows.
pub(super) fn entry_kind(metadata: &fs::Metadata) -> String {
    if metadata.file_type().is_symlink() {
        "symlink"
    } else if metadata.is_dir() {
        "directory"
    } else if metadata.is_file() {
        "file"
    } else {
        "other"
    }
    .into()
}

pub(super) fn has_symlink_component(root: &Path, relative: &Path) -> Result<bool, IoError> {
    let mut current = root.to_path_buf();
    for component in relative.components() {
        let Component::Normal(part) = component else {
            continue;
        };
        current.push(part);
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => return Ok(true),
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => break,
            Err(error) => return Err(IoError::from(error)),
        }
    }
    Ok(false)
}

pub(super) fn hash_version(bytes: &[u8]) -> String {
    // Stable, dependency-free content hash. Prefix permits a future migration
    // to SHA-256 without invalidating the optimistic-concurrency contract.
    let mut hash = 0xcbf29ce484222325u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("v1:{hash:016x}")
}
