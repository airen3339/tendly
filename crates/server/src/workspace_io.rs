//! Safe workspace file and Git operations.
//!
//! The module deliberately has no HTTP/router dependencies.  Callers provide a
//! workspace root and receive serialisable domain values or an [`IoError`]
//! carrying a useful HTTP status code.

use std::{
    fs, io,
    path::{Component, Path, PathBuf},
    time::Duration,
};

use serde::Serialize;
use tokio::{io::AsyncReadExt, process::Command, time::timeout};
use uuid::Uuid;

mod attachments;
mod entries;
mod files;
mod git;
mod git_branches;
mod paths;
#[cfg(test)]
mod tests;

pub use attachments::*;
pub use entries::*;
pub use files::*;
pub use git::*;
pub use git_branches::*;
pub use paths::*;

const MAX_TEXT_BYTES: u64 = 2 * 1024 * 1024;
const MAX_GIT_OUTPUT: usize = 8 * 1024 * 1024;
pub(crate) const GIT_TIMEOUT: Duration = Duration::from_secs(30);
pub(crate) const COMMIT_TIMEOUT: Duration = Duration::from_secs(120);

/// Structured error returned by workspace operations.
#[derive(Debug, Clone, Serialize)]
pub struct IoError {
    pub status: u16,
    pub message: String,
}

impl IoError {
    pub fn new(status: u16, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }

    fn io(error: io::Error) -> Self {
        let status = match error.kind() {
            io::ErrorKind::NotFound => 404,
            io::ErrorKind::PermissionDenied => 403,
            io::ErrorKind::AlreadyExists => 409,
            _ => 500,
        };
        Self::new(status, error.to_string())
    }
}

impl std::fmt::Display for IoError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for IoError {}

impl From<io::Error> for IoError {
    fn from(value: io::Error) -> Self {
        Self::io(value)
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct FileEntry {
    pub name: String,
    pub path: String,
    pub kind: String,
    pub size: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct TextFile {
    pub path: String,
    pub content: String,
    pub version: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct GitFile {
    pub index: String,
    pub worktree: String,
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub original_path: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct GitStatus {
    pub branch: Option<String>,
    pub files: Vec<GitFile>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ahead: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub behind: Option<u32>,
    /// The remote-tracking branch this one follows, as `origin/main`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub upstream: Option<String>,
    /// A merge, rebase, cherry-pick or revert that stopped part-way and waits
    /// to be continued or aborted.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub operation: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct GitDiff {
    pub diff: String,
    pub path: Option<String>,
    pub staged: bool,
    pub binary: bool,
    pub truncated: bool,
}
