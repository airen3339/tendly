//! Files dropped or pasted into a session, saved under the workspace's
//! `.tendly-files` directory so the agent can read them by path.

use super::*;

/// Directory attachments land in, relative to the workspace root. Deliberately
/// not `.tendly`: that name is reserved for Tendly's own server state, and
/// a workspace may be the Tendly checkout itself.
pub const ATTACHMENT_DIR: &str = ".tendly-files";
pub const MAX_ATTACHMENT_BYTES: u64 = 50 * 1024 * 1024;
/// Files pasted or dropped into a terminal, under the attachment directory.
/// They are handed over as a path and serve that one prompt, so they are
/// cleared after `PASTE_DAYS`; chat attachments stay, since a conversation
/// shows them again.
pub const PASTE_DIR: &str = "pastes";
pub(super) const PASTE_DAYS: i64 = 7;

#[derive(Debug, Clone, Serialize)]
pub struct Attachment {
    /// Workspace-relative path, which is what a session is given.
    pub path: String,
    pub name: String,
    pub bytes: u64,
    /// The full host path, for a terminal, whose program may be anywhere.
    pub absolute: String,
}

/// Reduce a client-supplied filename to a single safe path component.
///
/// Only the base name is kept, so a path from a phone's file picker cannot
/// steer the write. The original stem and extension are preserved where they
/// are usable, because the agent reading this file benefits from a recognisable
/// name and a correct extension.
pub(super) fn attachment_name(raw: &str) -> (String, String, String) {
    let base = raw.rsplit(['/', '\\']).next().unwrap_or("");
    let cleaned: String = base
        .chars()
        .map(|c| {
            if c.is_control() || matches!(c, '/' | '\\' | ':' | '\0') {
                '-'
            } else {
                c
            }
        })
        .collect();
    let cleaned = cleaned.trim().trim_matches('.').trim().to_owned();
    let (stem, extension) = match cleaned.rsplit_once('.') {
        Some((stem, extension))
            if !stem.is_empty()
                && (1..=16).contains(&extension.len())
                && extension.chars().all(|c| c.is_ascii_alphanumeric()) =>
        {
            (
                stem.to_owned(),
                format!(".{}", extension.to_ascii_lowercase()),
            )
        }
        _ => (cleaned.clone(), String::new()),
    };
    let stem: String = stem.chars().take(80).collect();
    let stem = stem.trim().to_owned();
    let display = if stem.is_empty() {
        format!("attachment{extension}")
    } else {
        format!("{stem}{extension}")
    };
    // The stored path is handed to an agent as text, so whitespace collapses to
    // hyphens; a path it has to quote correctly is a path it can get wrong. The
    // display name keeps the user's original spelling.
    let mut slug = String::new();
    let mut pending_separator = false;
    for character in stem.chars() {
        if character.is_whitespace() || character == '-' {
            pending_separator = !slug.is_empty();
        } else {
            if pending_separator {
                slug.push('-');
                pending_separator = false;
            }
            slug.push(character);
        }
    }
    (display, extension, slug)
}

/// Store an uploaded attachment inside the workspace and return its relative
/// path. Nothing is overwritten: each upload gets its own unique file name, so
/// two photos with the same camera name both survive.
pub async fn write_attachment(
    root: &Path,
    name: &str,
    bytes: &[u8],
) -> Result<Attachment, IoError> {
    store_attachment(root, "", name, bytes).await
}

/// Store a file pasted into a terminal, and clear pastes older than a week.
pub async fn write_paste(root: &Path, name: &str, bytes: &[u8]) -> Result<Attachment, IoError> {
    let stored = store_attachment(root, PASTE_DIR, name, bytes).await?;
    let pastes = root.join(ATTACHMENT_DIR).join(PASTE_DIR);
    let _ =
        tokio::task::spawn_blocking(move || prune_pastes(&pastes, chrono::Utc::now().date_naive()))
            .await;
    Ok(stored)
}

/// Remove the dated paste folders more than `PASTE_DAYS` old. Only folders
/// named as dates are touched.
pub(super) fn prune_pastes(pastes: &Path, today: chrono::NaiveDate) {
    let Ok(entries) = fs::read_dir(pastes) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(date) = name
            .to_str()
            .and_then(|n| chrono::NaiveDate::parse_from_str(n, "%Y-%m-%d").ok())
        else {
            continue;
        };
        let is_dir = entry.file_type().is_ok_and(|kind| kind.is_dir());
        if is_dir && (today - date).num_days() > PASTE_DAYS {
            let _ = fs::remove_dir_all(entry.path());
        }
    }
}

pub(super) async fn store_attachment(
    root: &Path,
    subdir: &str,
    name: &str,
    bytes: &[u8],
) -> Result<Attachment, IoError> {
    if bytes.is_empty() {
        return Err(IoError::new(400, "attachment is empty"));
    }
    if bytes.len() as u64 > MAX_ATTACHMENT_BYTES {
        return Err(IoError::new(413, "attachment exceeds 50 MiB limit"));
    }
    let (display, extension, stem) = attachment_name(name);
    let unique = Uuid::new_v4().simple().to_string();
    let folder = if subdir.is_empty() {
        ATTACHMENT_DIR.to_owned()
    } else {
        format!("{ATTACHMENT_DIR}/{subdir}")
    };
    let relative = format!(
        "{folder}/{}/{}-{}{extension}",
        chrono::Utc::now().format("%Y-%m-%d"),
        if stem.is_empty() { "attachment" } else { &stem },
        &unique[..8]
    );
    let root = root.to_path_buf();
    let payload = bytes.to_vec();
    let size = payload.len() as u64;
    tokio::task::spawn_blocking(move || {
        let canonical_root = dunce::canonicalize(&root).map_err(IoError::from)?;
        let rel = safe_relative(&relative)?;
        if is_protected_path(&rel) {
            return Err(IoError::new(403, "protected path cannot be written"));
        }
        if has_symlink_component(&canonical_root, &rel)? {
            return Err(IoError::new(403, "symlink writes are not allowed"));
        }
        let target = safe_path(&canonical_root, &relative)?;
        if fs::symlink_metadata(&target).is_ok() {
            return Err(IoError::new(409, "attachment already exists"));
        }
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).map_err(IoError::from)?;
        }
        // A self-contained ignore, so uploads do not turn into pending Git
        // changes in the user's repository. It only covers this directory and
        // never edits a .gitignore the user owns.
        let ignore = canonical_root.join(ATTACHMENT_DIR).join(".gitignore");
        if fs::symlink_metadata(&ignore).is_err() {
            let _ = fs::write(&ignore, "*\n");
        }
        fs::write(&target, &payload).map_err(IoError::from)?;
        Ok(Attachment {
            path: relative_string(&canonical_root, &target),
            name: display,
            bytes: size,
            absolute: target.to_string_lossy().into_owned(),
        })
    })
    .await
    .map_err(|_| IoError::new(500, "attachment write failed"))?
}
