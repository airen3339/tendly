//! Each account's private directory, its metadata files, and the reset keys
//! kept beside them. Directories and files are created owner-only.

use super::*;

pub(super) fn protected_directory(path: &Path, create: bool) -> Result<PathBuf> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
        Ok(_) => {
            return Err(ApiError::bad(
                "Account storage must be a real directory, not a symlink",
            ));
        }
        Err(error) if create && error.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir(path).map_err(ApiError::internal)?;
        }
        Err(_) => return Err(ApiError::missing("Account storage")),
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).map_err(ApiError::internal)?;
    }
    dunce::canonicalize(path).map_err(ApiError::internal)
}
pub(super) fn root_directory(state: &AppState, create: bool) -> Result<PathBuf> {
    let state_dir = protected_directory(&state.state_dir, false)?;
    protected_directory(&state_dir.join("accounts"), create)
}
pub(super) fn directory(state: &AppState, id: Uuid) -> Result<PathBuf> {
    let root = root_directory(state, false)?;
    let directory = protected_directory(&root.join(id.to_string()), false)?;
    if !directory.starts_with(root) {
        return Err(ApiError::bad("Account path escaped its storage root"));
    }
    for file in [
        "account.json",
        "auth.json",
        "config.toml",
        ".credentials.json",
        "quota-reset-attempts.json",
        "usage.json",
    ] {
        if let Ok(metadata) = fs::symlink_metadata(directory.join(file))
            && (!metadata.is_file() || metadata.file_type().is_symlink())
        {
            return Err(ApiError::bad(
                "Account files must be regular files, not symlinks",
            ));
        }
    }
    protect_files(&directory)?;
    Ok(directory)
}
pub(super) fn protect_files(directory: &Path) -> Result<()> {
    for name in [
        "account.json",
        "auth.json",
        "config.toml",
        ".credentials.json",
        "quota-reset-attempts.json",
        "usage.json",
    ] {
        let path = directory.join(name);
        if let Ok(metadata) = fs::symlink_metadata(&path) {
            if metadata.file_type().is_symlink() || !metadata.is_file() {
                return Err(ApiError::bad("Account storage contains an unsafe file"));
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(path, fs::Permissions::from_mode(0o600))
                    .map_err(ApiError::internal)?;
            }
        }
    }
    Ok(())
}
pub(super) fn write_new(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut options = fs::OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path).map_err(ApiError::internal)?;
    file.write_all(bytes).map_err(ApiError::internal)?;
    file.sync_all().map_err(ApiError::internal)
}

/// Replace existing account metadata atomically. Staging plus rename means a
/// crash mid-write leaves the previous record intact rather than a truncated one.
pub(super) fn write_metadata_replace(path: &Path, bytes: &[u8]) -> Result<()> {
    if fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
        return Err(ApiError::bad("Account metadata must be a regular file"));
    }
    let staging = path.with_file_name(format!(".account-{}.tmp", Uuid::new_v4()));
    write_new(&staging, bytes)?;
    if let Err(error) = fs::rename(&staging, path) {
        let _ = fs::remove_file(&staging);
        return Err(ApiError::internal(error));
    }
    Ok(())
}

pub(super) fn write_metadata_new(path: &Path, bytes: &[u8]) -> Result<()> {
    let staging = path.with_file_name(format!(".account-{}.tmp", Uuid::new_v4()));
    write_new(&staging, bytes)?;
    if fs::symlink_metadata(path).is_ok() {
        let _ = fs::remove_file(&staging);
        return Err(ApiError::conflict("Account metadata already exists"));
    }
    if let Err(error) = fs::rename(&staging, path) {
        let _ = fs::remove_file(&staging);
        return Err(ApiError::internal(error));
    }
    Ok(())
}
pub(super) fn record(state: &AppState, id: Uuid) -> Result<AccountRecord> {
    let directory = directory(state, id)?;
    let file = directory.join("account.json");
    if fs::metadata(&file).map_err(ApiError::internal)?.len() > 16 * 1024 {
        return Err(ApiError::bad("Account metadata exceeds limit"));
    }
    let record: AccountRecord =
        serde_json::from_slice(&fs::read(file).map_err(ApiError::internal)?)
            .map_err(|_| ApiError::bad("Invalid account metadata"))?;
    if record.id != id || record.provider == ProviderKind::Terminal {
        return Err(ApiError::bad("Account metadata identity mismatch"));
    }
    Ok(record)
}

pub(super) fn reset_keys(state: &AppState, id: Uuid) -> Result<Vec<String>> {
    let path = directory(state, id)?.join("quota-reset-attempts.json");
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(ApiError::internal(error)),
    };
    if !metadata.is_file() || metadata.file_type().is_symlink() || metadata.len() > 16 * 1024 {
        return Err(ApiError::bad("Invalid quota-reset retry metadata"));
    }
    let keys: Vec<String> = serde_json::from_slice(&fs::read(path).map_err(ApiError::internal)?)
        .map_err(|_| ApiError::bad("Invalid quota-reset retry metadata"))?;
    if keys.len() > 64
        || keys
            .iter()
            .any(|key| key.is_empty() || key.len() > 200 || key.chars().any(char::is_control))
    {
        return Err(ApiError::bad("Invalid quota-reset retry metadata"));
    }
    Ok(keys)
}
pub(super) fn persist_reset_key(state: &AppState, id: Uuid, key: &str) -> Result<()> {
    let mut keys = reset_keys(state, id)?;
    if keys.iter().any(|existing| existing == key) {
        return Ok(());
    }
    if keys.len() == 64 {
        keys.remove(0);
    }
    keys.push(key.into());
    let path = directory(state, id)?.join("quota-reset-attempts.json");
    let staging = path.with_file_name(format!(".reset-{}.tmp", Uuid::new_v4()));
    write_new(
        &staging,
        &serde_json::to_vec(&keys).map_err(ApiError::internal)?,
    )?;
    if let Ok(metadata) = fs::symlink_metadata(&path)
        && (metadata.file_type().is_symlink() || !metadata.is_file())
    {
        let _ = fs::remove_file(staging);
        return Err(ApiError::bad("Invalid quota-reset retry metadata"));
    }
    if let Err(error) = fs::rename(&staging, &path) {
        let _ = fs::remove_file(staging);
        return Err(ApiError::internal(error));
    }
    #[cfg(unix)]
    {
        fs::File::open(path.parent().expect("account directory"))
            .and_then(|directory| directory.sync_all())
            .map_err(ApiError::internal)?;
    }
    Ok(())
}
