//! The rest of Git, beyond the working tree: history, sync with remotes,
//! stashes, tags, merging and the operations that can stop part-way.
//!
//! Every command runs through `workspace_io::run_git` -- no shell, no pager,
//! no editor, bounded output -- and every name or revision a request carries
//! is checked by Git before it is used, never trusted to look harmless.
//! Operations that rewrite files hold the workspace lock like the others;
//! fetch and push only move refs, so they leave it free while the network is
//! slow.
use crate::{
    ApiError, AppState, Result, root,
    workspace_io::{
        self, COMMIT_TIMEOUT, GIT_TIMEOUT, GitDiff, IoError, git_error, git_operation, git_text,
        is_protected_path, run_git, safe_relative, valid_branch, valid_rev,
    },
};
use agentdock_domain::WorkspaceId;
use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::StatusCode,
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use std::{path::PathBuf, time::Duration};

/// Fetch, pull and push wait on a remote that may be slow or far away.
const NETWORK_TIMEOUT: Duration = Duration::from_secs(300);
const MAX_LOG: usize = 500;
const MAX_FILES: usize = 3000;
const MAX_PATCH: usize = 4 * 1024 * 1024;

async fn repo(state: &AppState, id: WorkspaceId) -> Result<PathBuf> {
    Ok(dunce::canonicalize(root(state, id).await?).map_err(IoError::from)?)
}

fn args(list: &[&str]) -> Vec<String> {
    list.iter().map(|arg| (*arg).to_owned()).collect()
}

/// What Git said, stdout and stderr together: a merge reports its conflicts on
/// stdout and its refusal on stderr, and both are worth showing.
fn said(stdout: &[u8], stderr: &[u8]) -> String {
    [stdout, stderr]
        .iter()
        .map(|bytes| String::from_utf8_lossy(bytes).trim().to_owned())
        .filter(|text| !text.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

/// Run a command that must succeed, returning its stdout.
async fn git(root: &std::path::Path, list: Vec<String>, wait: Duration) -> Result<String> {
    let (stdout, stderr, success) = run_git(root, list, wait).await?;
    if success {
        Ok(String::from_utf8_lossy(&stdout).into_owned())
    } else {
        let message = said(&stdout, &stderr);
        Err(IoError::new(
            409,
            if message.is_empty() {
                git_error(&stderr)
            } else {
                message
            },
        )
        .into())
    }
}

/// A workspace-relative path a request names, refused if it leaves the
/// workspace or touches what Tendly never edits.
fn checked_path(path: &str) -> Result<String> {
    let relative = safe_relative(path)?;
    if path.is_empty() || path == "." || is_protected_path(&relative) {
        return Err(ApiError::bad("Invalid path"));
    }
    Ok(path.to_owned())
}

/// How an operation that can stop part-way ended: done, or stopped on
/// conflicts that are now in the working tree waiting to be resolved.
#[derive(Serialize)]
struct Outcome {
    conflicts: bool,
    message: String,
}

async fn unmerged(root: &std::path::Path) -> bool {
    run_git(root, args(&["ls-files", "--unmerged", "-z"]), GIT_TIMEOUT)
        .await
        .is_ok_and(|(stdout, _, success)| success && !stdout.is_empty())
}

/// Conflicts are an outcome, not a failure: the merge happened as far as Git
/// could take it, and the person finishes it from the changes list.
async fn finish(
    root: &std::path::Path,
    list: Vec<String>,
    wait: Duration,
) -> Result<Json<Outcome>> {
    let (stdout, stderr, success) = run_git(root, list, wait).await?;
    let message = said(&stdout, &stderr);
    if success {
        return Ok(Json(Outcome {
            conflicts: false,
            message,
        }));
    }
    if unmerged(root).await || git_operation(root).await.is_some() {
        return Ok(Json(Outcome {
            conflicts: true,
            message,
        }));
    }
    Err(IoError::new(
        409,
        if message.is_empty() {
            git_error(&stderr)
        } else {
            message
        },
    )
    .into())
}

// ---------------------------------------------------------------- history

#[derive(Debug, Serialize)]
struct Commit {
    hash: String,
    parents: Vec<String>,
    author: String,
    email: String,
    /// Author time, in seconds.
    date: i64,
    /// Branches, tags and HEAD pointing here, as Git decorates them.
    refs: Vec<String>,
    subject: String,
}

#[derive(Serialize)]
struct Log {
    commits: Vec<Commit>,
    /// More commits follow the last one listed.
    more: bool,
}

#[derive(Deserialize)]
struct LogQuery {
    /// Start here instead of HEAD.
    rev: Option<String>,
    /// Only commits that changed this path.
    path: Option<String>,
    #[serde(default)]
    all: bool,
    #[serde(default)]
    skip: usize,
    limit: Option<usize>,
}

fn decorations(raw: &str) -> Vec<String> {
    raw.split(", ")
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_owned)
        .collect()
}

fn parse_log(raw: &str) -> Vec<Commit> {
    raw.split('\0')
        .filter_map(|record| {
            let record = record.trim_start_matches('\n');
            let mut fields = record.splitn(7, '\x1f');
            let hash = fields.next().filter(|hash| !hash.is_empty())?.to_owned();
            Some(Commit {
                hash,
                parents: fields
                    .next()?
                    .split(' ')
                    .filter(|p| !p.is_empty())
                    .map(str::to_owned)
                    .collect(),
                author: fields.next()?.to_owned(),
                email: fields.next()?.to_owned(),
                date: fields.next()?.parse().unwrap_or_default(),
                refs: decorations(fields.next()?),
                subject: fields.next()?.trim_end().to_owned(),
            })
        })
        .collect()
}

async fn log(
    State(state): State<AppState>,
    Path(id): Path<WorkspaceId>,
    Query(q): Query<LogQuery>,
) -> Result<Json<Log>> {
    let root = repo(&state, id).await?;
    let limit = q.limit.unwrap_or(100).clamp(1, MAX_LOG);
    let mut list = args(&[
        "log",
        "-z",
        "--date-order",
        "--decorate=short",
        "--format=%H%x1f%P%x1f%an%x1f%ae%x1f%at%x1f%D%x1f%s",
    ]);
    list.push(format!("--skip={}", q.skip));
    list.push(format!("--max-count={}", limit + 1));
    if q.all {
        list.extend(args(&["--exclude=refs/stash", "--all"]));
    } else if let Some(rev) = q.rev.as_deref().map(str::trim).filter(|r| !r.is_empty()) {
        list.push(valid_rev(&root, rev).await?);
    } else if valid_rev(&root, "HEAD").await.is_err() {
        // A repository with no commits yet has no history to show.
        return Ok(Json(Log {
            commits: Vec::new(),
            more: false,
        }));
    }
    if let Some(path) = q.path.as_deref().filter(|p| !p.is_empty()) {
        list.extend(["--".into(), checked_path(path)?]);
    }
    let mut commits = parse_log(&git(&root, list, GIT_TIMEOUT).await?);
    let more = commits.len() > limit;
    commits.truncate(limit);
    Ok(Json(Log { commits, more }))
}

#[derive(Debug, Serialize)]
struct ChangedFile {
    /// A, M, D, R, C or T, as `git diff --name-status` reports it.
    status: String,
    path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    original_path: Option<String>,
    /// Lines added and removed; absent for a binary file.
    #[serde(skip_serializing_if = "Option::is_none")]
    added: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    removed: Option<u32>,
    /// From a stash: a file that was untracked when it was stashed.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    untracked: bool,
}

#[derive(Serialize)]
struct CommitDetail {
    hash: String,
    parents: Vec<String>,
    author: String,
    email: String,
    date: i64,
    committer: String,
    committer_email: String,
    commit_date: i64,
    refs: Vec<String>,
    message: String,
    files: Vec<ChangedFile>,
    /// More files changed than are listed.
    truncated: bool,
}

#[derive(Deserialize)]
struct CommitQuery {
    rev: String,
    /// Read the commit as a stash: its untracked files are listed too.
    #[serde(default)]
    stash: bool,
    path: Option<String>,
    original_path: Option<String>,
    /// From a stash: the file was untracked.
    #[serde(default)]
    untracked: bool,
}

/// The two sides of a commit's diff: its first parent, or nothing for a root
/// commit. A merge is shown against the branch it was merged into.
fn diff_args(hash: &str, parent: Option<&str>, extra: &[&str]) -> Vec<String> {
    let mut list = match parent {
        Some(parent) => vec!["diff".to_owned(), parent.to_owned(), hash.to_owned()],
        None => args(&["diff-tree", "--root", "-r", "--no-commit-id", hash]),
    };
    list.extend(args(&["-M", "--no-ext-diff", "--no-color"]));
    list.extend(args(extra));
    list
}

fn parse_name_status(raw: &str, untracked: bool) -> Vec<ChangedFile> {
    let mut tokens = raw.split('\0').filter(|token| !token.is_empty());
    let mut files = Vec::new();
    while let Some(code) = tokens.next() {
        let status = code.chars().next().unwrap_or('M').to_string();
        let first = tokens.next().unwrap_or_default().to_owned();
        let (path, original_path) = if matches!(status.as_str(), "R" | "C") {
            (tokens.next().unwrap_or_default().to_owned(), Some(first))
        } else {
            (first, None)
        };
        files.push(ChangedFile {
            status: if untracked { "A".into() } else { status },
            path,
            original_path,
            added: None,
            removed: None,
            untracked,
        });
    }
    files
}

/// `--numstat -z`: `added\tremoved\tpath\0`, or for a rename
/// `added\tremoved\t\0from\0to\0`. A binary file counts `-`.
fn apply_numstat(raw: &str, files: &mut [ChangedFile]) {
    let mut tokens = raw.split('\0');
    while let Some(head) = tokens.next() {
        let mut parts = head.splitn(3, '\t');
        let (Some(added), Some(removed), Some(path)) = (parts.next(), parts.next(), parts.next())
        else {
            continue;
        };
        let path = if path.is_empty() {
            tokens.next();
            tokens.next().unwrap_or_default()
        } else {
            path
        };
        if let Some(file) = files.iter_mut().find(|file| file.path == path) {
            file.added = added.parse().ok();
            file.removed = removed.parse().ok();
        }
    }
}

async fn changed_files(
    root: &std::path::Path,
    hash: &str,
    parent: Option<&str>,
    untracked: bool,
) -> Result<Vec<ChangedFile>> {
    let names = git(
        root,
        diff_args(hash, parent, &["--name-status", "-z"]),
        GIT_TIMEOUT,
    )
    .await?;
    let mut files = parse_name_status(&names, untracked);
    if files.len() <= MAX_FILES {
        let counts = git(
            root,
            diff_args(hash, parent, &["--numstat", "-z"]),
            GIT_TIMEOUT,
        )
        .await?;
        apply_numstat(&counts, &mut files);
    }
    Ok(files)
}

async fn commit_detail(
    State(state): State<AppState>,
    Path(id): Path<WorkspaceId>,
    Query(q): Query<CommitQuery>,
) -> Result<Json<CommitDetail>> {
    let root = repo(&state, id).await?;
    let hash = valid_rev(&root, q.rev.trim()).await?;
    let raw = git(
        &root,
        vec![
            "show".into(),
            "-s".into(),
            "--decorate=short".into(),
            "--format=%H%x1f%P%x1f%an%x1f%ae%x1f%at%x1f%cn%x1f%ce%x1f%ct%x1f%D%x1f%B".into(),
            hash.clone(),
        ],
        GIT_TIMEOUT,
    )
    .await?;
    let fields: Vec<&str> = raw.splitn(10, '\x1f').collect();
    let [
        _,
        parents,
        author,
        email,
        date,
        committer,
        committer_email,
        commit_date,
        refs,
        message,
    ] = fields[..]
    else {
        return Err(ApiError::internal("unreadable commit"));
    };
    let parents: Vec<String> = parents
        .split(' ')
        .filter(|p| !p.is_empty())
        .map(str::to_owned)
        .collect();
    let mut files = changed_files(&root, &hash, parents.first().map(String::as_str), false).await?;
    // A stash keeps the untracked files it took in a third, parentless commit.
    if q.stash
        && let Some(third) = parents.get(2)
    {
        files.extend(changed_files(&root, third, None, true).await?);
    }
    let truncated = files.len() > MAX_FILES;
    files.truncate(MAX_FILES);
    Ok(Json(CommitDetail {
        hash,
        parents,
        author: author.to_owned(),
        email: email.to_owned(),
        date: date.parse().unwrap_or_default(),
        committer: committer.to_owned(),
        committer_email: committer_email.to_owned(),
        commit_date: commit_date.parse().unwrap_or_default(),
        refs: decorations(refs),
        message: message.trim_end().to_owned(),
        files,
        truncated,
    }))
}

/// One file's change in a commit (or a stash), for the diff view.
async fn commit_diff(
    State(state): State<AppState>,
    Path(id): Path<WorkspaceId>,
    Query(q): Query<CommitQuery>,
) -> Result<Json<GitDiff>> {
    let root = repo(&state, id).await?;
    let hash = valid_rev(&root, q.rev.trim()).await?;
    let path = checked_path(q.path.as_deref().unwrap_or_default())?;
    let parents: Vec<String> = git_text(&root, &["rev-list", "--parents", "-n", "1", &hash])
        .await?
        .unwrap_or_default()
        .split(' ')
        .skip(1)
        .map(str::to_owned)
        .collect();
    let (target, parent) = if q.untracked {
        let third = parents
            .get(2)
            .ok_or_else(|| ApiError::bad("This stash has no untracked files"))?;
        (third.clone(), None)
    } else {
        (hash, parents.first().cloned())
    };
    let mut list = diff_args(&target, parent.as_deref(), &["-p", "--no-textconv", "--"]);
    list.push(path.clone());
    if let Some(original) = q.original_path.as_deref().filter(|p| !p.is_empty()) {
        list.push(checked_path(original)?);
    }
    let diff = git(&root, list, GIT_TIMEOUT).await?;
    Ok(Json(GitDiff {
        binary: diff.contains("Binary files") || diff.contains("GIT binary patch"),
        diff,
        path: Some(path),
        staged: false,
        truncated: false,
    }))
}

// ------------------------------------------------------------------ remotes

#[derive(Serialize)]
struct Remote {
    name: String,
    url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    push_url: Option<String>,
}

async fn list_remotes(root: &std::path::Path) -> Result<Vec<Remote>> {
    let mut remotes: Vec<Remote> = Vec::new();
    for line in git(root, args(&["remote", "-v"]), GIT_TIMEOUT)
        .await?
        .lines()
    {
        let mut parts = line.split_whitespace();
        let (Some(name), Some(url), Some(kind)) = (parts.next(), parts.next(), parts.next()) else {
            continue;
        };
        match remotes.iter_mut().find(|remote| remote.name == name) {
            Some(remote) if kind == "(push)" && remote.url != url => {
                remote.push_url = Some(url.to_owned())
            }
            Some(_) => {}
            None => remotes.push(Remote {
                name: name.to_owned(),
                url: url.to_owned(),
                push_url: None,
            }),
        }
    }
    Ok(remotes)
}

async fn remotes(
    State(state): State<AppState>,
    Path(id): Path<WorkspaceId>,
) -> Result<Json<Vec<Remote>>> {
    Ok(Json(list_remotes(&repo(&state, id).await?).await?))
}

fn valid_remote_name(name: &str) -> Result<()> {
    let ok = !name.is_empty()
        && name.len() <= 100
        && !name.starts_with(['-', '.'])
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
    if ok {
        Ok(())
    } else {
        Err(ApiError::bad("Invalid remote name"))
    }
}

/// Transports that run a command of the URL's choosing are never added.
fn valid_url(url: &str) -> Result<()> {
    let lower = url.to_ascii_lowercase();
    if url.is_empty()
        || url.len() > 2048
        || url.starts_with('-')
        || url.chars().any(char::is_control)
        || lower.starts_with("ext::")
        || lower.starts_with("fd::")
    {
        return Err(ApiError::bad("Invalid remote URL"));
    }
    Ok(())
}

/// A remote this repository already has, by name.
async fn known_remote(root: &std::path::Path, name: &str) -> Result<String> {
    valid_remote_name(name)?;
    if list_remotes(root)
        .await?
        .iter()
        .any(|remote| remote.name == name)
    {
        Ok(name.to_owned())
    } else {
        Err(ApiError::bad(format!("No remote named {name}")))
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RemoteInput {
    name: String,
    #[serde(default)]
    url: Option<String>,
    /// Renaming: the new name.
    #[serde(default)]
    to: Option<String>,
}

async fn add_remote(
    State(state): State<AppState>,
    Path(id): Path<WorkspaceId>,
    Json(input): Json<RemoteInput>,
) -> Result<Json<Vec<Remote>>> {
    let _guard = state.operations.lock().await;
    let root = repo(&state, id).await?;
    let (name, url) = (
        input.name.trim(),
        input.url.as_deref().unwrap_or_default().trim(),
    );
    valid_remote_name(name)?;
    valid_url(url)?;
    git(&root, args(&["remote", "add", name, url]), GIT_TIMEOUT).await?;
    Ok(Json(list_remotes(&root).await?))
}

async fn edit_remote(
    State(state): State<AppState>,
    Path(id): Path<WorkspaceId>,
    Json(input): Json<RemoteInput>,
) -> Result<Json<Vec<Remote>>> {
    let _guard = state.operations.lock().await;
    let root = repo(&state, id).await?;
    let mut name = known_remote(&root, input.name.trim()).await?;
    if let Some(to) = input.to.as_deref().map(str::trim).filter(|to| *to != name) {
        valid_remote_name(to)?;
        git(&root, args(&["remote", "rename", &name, to]), GIT_TIMEOUT).await?;
        name = to.to_owned();
    }
    if let Some(url) = input.url.as_deref().map(str::trim) {
        valid_url(url)?;
        git(&root, args(&["remote", "set-url", &name, url]), GIT_TIMEOUT).await?;
    }
    Ok(Json(list_remotes(&root).await?))
}

async fn remove_remote(
    State(state): State<AppState>,
    Path(id): Path<WorkspaceId>,
    Json(input): Json<RemoteInput>,
) -> Result<Json<Vec<Remote>>> {
    let _guard = state.operations.lock().await;
    let root = repo(&state, id).await?;
    let name = known_remote(&root, input.name.trim()).await?;
    git(&root, args(&["remote", "remove", &name]), GIT_TIMEOUT).await?;
    Ok(Json(list_remotes(&root).await?))
}

// --------------------------------------------------------------------- sync

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FetchInput {
    /// One remote; every remote when absent.
    #[serde(default)]
    remote: Option<String>,
}

async fn fetch(
    State(state): State<AppState>,
    Path(id): Path<WorkspaceId>,
    Json(input): Json<FetchInput>,
) -> Result<StatusCode> {
    let root = repo(&state, id).await?;
    let mut list = args(&["fetch", "--prune", "--tags"]);
    match input
        .remote
        .as_deref()
        .map(str::trim)
        .filter(|r| !r.is_empty())
    {
        Some(remote) => list.push(known_remote(&root, remote).await?),
        None => list.push("--all".into()),
    }
    git(&root, list, NETWORK_TIMEOUT).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PullInput {
    /// `merge`, `rebase` or `ff-only`; the repository's own setting when
    /// absent, and a merge when it has none.
    #[serde(default)]
    mode: Option<String>,
}

async fn pull(
    State(state): State<AppState>,
    Path(id): Path<WorkspaceId>,
    Json(input): Json<PullInput>,
) -> Result<Json<Outcome>> {
    let _guard = state.operations.lock().await;
    let root = repo(&state, id).await?;
    let mut list = args(&["pull", "--no-edit"]);
    match input.mode.as_deref() {
        Some("merge") => list.push("--no-rebase".into()),
        Some("rebase") => list.push("--rebase".into()),
        Some("ff-only") => list.push("--ff-only".into()),
        Some(_) => return Err(ApiError::bad("Unknown pull mode")),
        None => {
            let configured = git_text(&root, &["config", "--get", "pull.rebase"])
                .await?
                .is_some()
                || git_text(&root, &["config", "--get", "pull.ff"])
                    .await?
                    .is_some();
            if !configured {
                list.push("--no-rebase".into());
            }
        }
    }
    finish(&root, list, NETWORK_TIMEOUT).await
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PushInput {
    #[serde(default)]
    remote: Option<String>,
    /// A branch other than the current one; with `delete`, the remote branch
    /// to delete.
    #[serde(default)]
    branch: Option<String>,
    /// Push the current branch and make it track what it was pushed to.
    #[serde(default)]
    set_upstream: bool,
    /// Overwrite the remote branch, but only if it is where it was last seen.
    #[serde(default)]
    force: bool,
    #[serde(default)]
    tag: Option<String>,
    #[serde(default)]
    delete: bool,
}

async fn push(
    State(state): State<AppState>,
    Path(id): Path<WorkspaceId>,
    Json(input): Json<PushInput>,
) -> Result<Json<Outcome>> {
    let root = repo(&state, id).await?;
    let remote = match input
        .remote
        .as_deref()
        .map(str::trim)
        .filter(|r| !r.is_empty())
    {
        Some(remote) => Some(known_remote(&root, remote).await?),
        None => None,
    };
    // Naming no remote leaves the choice to Git and the branch's upstream;
    // anything that names a ref needs one, and takes the only or `origin`.
    let default_remote = || async {
        let remotes = list_remotes(&root).await?;
        remotes
            .iter()
            .find(|r| r.name == "origin")
            .or(remotes.first())
            .map(|r| r.name.clone())
            .ok_or_else(|| ApiError::bad("This repository has no remote. Add one first."))
    };
    let mut list = args(&["push", "--porcelain"]);
    if let Some(tag) = input.tag.as_deref().map(str::trim) {
        valid_tag(&root, tag).await?;
        list.push(match remote {
            Some(remote) => remote,
            None => default_remote().await?,
        });
        list.push(format!(
            "{}refs/tags/{tag}",
            if input.delete { ":" } else { "" }
        ));
    } else if input.delete {
        let branch = input.branch.as_deref().map(str::trim).unwrap_or_default();
        valid_branch(&root, branch).await?;
        list.push(match remote {
            Some(remote) => remote,
            None => default_remote().await?,
        });
        list.push(format!(":refs/heads/{branch}"));
    } else {
        if input.force {
            list.push("--force-with-lease".into());
        }
        let branch = match input
            .branch
            .as_deref()
            .map(str::trim)
            .filter(|b| !b.is_empty())
        {
            Some(branch) => Some(branch.to_owned()),
            None if input.set_upstream => Some(
                git_text(&root, &["symbolic-ref", "--short", "-q", "HEAD"])
                    .await?
                    .filter(|b| !b.is_empty())
                    .ok_or_else(|| ApiError::bad("HEAD is detached: there is no branch to push"))?,
            ),
            None => None,
        };
        if input.set_upstream {
            list.push("--set-upstream".into());
        }
        match (remote, branch) {
            (remote, Some(branch)) => {
                valid_branch(&root, &branch).await?;
                list.push(match remote {
                    Some(remote) => remote,
                    None => default_remote().await?,
                });
                list.push(format!("refs/heads/{branch}:refs/heads/{branch}"));
            }
            (Some(remote), None) => list.push(remote),
            (None, None) => {}
        }
    }
    let (stdout, stderr, success) = run_git(&root, list, NETWORK_TIMEOUT).await?;
    if success {
        Ok(Json(Outcome {
            conflicts: false,
            message: said(&[], &stderr),
        }))
    } else {
        let message = said(&stdout, &stderr);
        Err(IoError::new(
            409,
            if message.is_empty() {
                git_error(&stderr)
            } else {
                message
            },
        )
        .into())
    }
}

// ----------------------------------------------- merge, rebase and friends

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MergeInput {
    rev: String,
    /// Always record a merge commit, even when a fast-forward would do.
    #[serde(default)]
    no_ff: bool,
    /// Stage the combined changes for one commit instead of merging history.
    #[serde(default)]
    squash: bool,
}

async fn merge(
    State(state): State<AppState>,
    Path(id): Path<WorkspaceId>,
    Json(input): Json<MergeInput>,
) -> Result<Json<Outcome>> {
    let _guard = state.operations.lock().await;
    let root = repo(&state, id).await?;
    let rev = input.rev.trim();
    valid_rev(&root, rev).await?;
    let mut list = args(&["merge", "--no-edit"]);
    if input.squash {
        list.push("--squash".into());
    } else if input.no_ff {
        list.push("--no-ff".into());
    }
    // The name rather than its hash, so the merge commit says what was merged.
    list.push(rev.to_owned());
    finish(&root, list, COMMIT_TIMEOUT).await
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RevInput {
    rev: String,
    /// Reset: `soft`, `mixed` or `hard`.
    #[serde(default)]
    mode: Option<String>,
}

async fn rebase(
    State(state): State<AppState>,
    Path(id): Path<WorkspaceId>,
    Json(input): Json<RevInput>,
) -> Result<Json<Outcome>> {
    let _guard = state.operations.lock().await;
    let root = repo(&state, id).await?;
    let rev = input.rev.trim();
    valid_rev(&root, rev).await?;
    finish(&root, args(&["rebase", rev]), COMMIT_TIMEOUT).await
}

async fn parent_count(root: &std::path::Path, hash: &str) -> Result<usize> {
    Ok(git_text(root, &["rev-list", "--parents", "-n", "1", hash])
        .await?
        .unwrap_or_default()
        .split(' ')
        .count()
        .saturating_sub(1))
}

async fn cherry_pick(
    State(state): State<AppState>,
    Path(id): Path<WorkspaceId>,
    Json(input): Json<RevInput>,
) -> Result<Json<Outcome>> {
    let _guard = state.operations.lock().await;
    let root = repo(&state, id).await?;
    let hash = valid_rev(&root, input.rev.trim()).await?;
    let mut list = args(&["cherry-pick"]);
    // A merge is picked as its change against the branch it was merged into.
    if parent_count(&root, &hash).await? > 1 {
        list.extend(args(&["-m", "1"]));
    }
    list.push(hash);
    finish(&root, list, COMMIT_TIMEOUT).await
}

async fn revert(
    State(state): State<AppState>,
    Path(id): Path<WorkspaceId>,
    Json(input): Json<RevInput>,
) -> Result<Json<Outcome>> {
    let _guard = state.operations.lock().await;
    let root = repo(&state, id).await?;
    let hash = valid_rev(&root, input.rev.trim()).await?;
    let mut list = args(&["revert", "--no-edit"]);
    if parent_count(&root, &hash).await? > 1 {
        list.extend(args(&["-m", "1"]));
    }
    list.push(hash);
    finish(&root, list, COMMIT_TIMEOUT).await
}

/// Move the current branch to another commit. `soft` keeps everything
/// staged, `mixed` keeps it as unstaged changes, `hard` throws it away.
async fn reset(
    State(state): State<AppState>,
    Path(id): Path<WorkspaceId>,
    Json(input): Json<RevInput>,
) -> Result<StatusCode> {
    let _guard = state.operations.lock().await;
    let root = repo(&state, id).await?;
    let hash = valid_rev(&root, input.rev.trim()).await?;
    let mode = match input.mode.as_deref().unwrap_or("mixed") {
        "soft" => "--soft",
        "mixed" => "--mixed",
        "hard" => "--hard",
        _ => return Err(ApiError::bad("Unknown reset mode")),
    };
    git(&root, args(&["reset", mode, &hash]), COMMIT_TIMEOUT).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct OperationInput {
    /// `continue`, `abort` or `skip`.
    action: String,
}

/// Continue, abort or skip whichever operation stopped part-way.
async fn operation(
    State(state): State<AppState>,
    Path(id): Path<WorkspaceId>,
    Json(input): Json<OperationInput>,
) -> Result<Json<Outcome>> {
    let _guard = state.operations.lock().await;
    let root = repo(&state, id).await?;
    let current = git_operation(&root)
        .await
        .ok_or_else(|| ApiError::conflict("Nothing is in progress"))?;
    let action = match input.action.as_str() {
        "continue" => "--continue",
        "abort" => "--abort",
        "skip" if current != "merge" => "--skip",
        _ => return Err(ApiError::bad("Unknown action")),
    };
    if action == "--continue" && unmerged(&root).await {
        return Err(ApiError::conflict(
            "Resolve every conflict and stage the files before continuing.",
        ));
    }
    if action == "--abort" {
        return Ok(Json(Outcome {
            conflicts: false,
            message: git(&root, args(&[&current, action]), COMMIT_TIMEOUT).await?,
        }));
    }
    finish(&root, args(&[&current, action]), COMMIT_TIMEOUT).await
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ResolveInput {
    path: String,
    /// `ours` (the branch being merged into) or `theirs` (what is coming in).
    side: String,
}

/// Settle a conflicted file by taking one side whole, and mark it resolved.
/// When that side deleted the file, resolving it deletes it.
async fn resolve(
    State(state): State<AppState>,
    Path(id): Path<WorkspaceId>,
    Json(input): Json<ResolveInput>,
) -> Result<StatusCode> {
    let _guard = state.operations.lock().await;
    let root = repo(&state, id).await?;
    let path = checked_path(&input.path)?;
    let side = match input.side.as_str() {
        "ours" => "--ours",
        "theirs" => "--theirs",
        _ => return Err(ApiError::bad("Unknown side")),
    };
    let (_, stderr, success) = run_git(
        &root,
        vec!["checkout".into(), side.into(), "--".into(), path.clone()],
        GIT_TIMEOUT,
    )
    .await?;
    let gone = String::from_utf8_lossy(&stderr);
    if success {
        git(&root, vec!["add".into(), "--".into(), path], GIT_TIMEOUT).await?;
    } else if gone.contains("does not have our version")
        || gone.contains("does not have their version")
    {
        git(
            &root,
            vec!["rm".into(), "-q".into(), "--".into(), path],
            GIT_TIMEOUT,
        )
        .await?;
    } else {
        return Err(IoError::new(409, git_error(&stderr)).into());
    }
    Ok(StatusCode::NO_CONTENT)
}

// ------------------------------------------------------------------ stashes

#[derive(Serialize)]
struct Stash {
    index: u32,
    hash: String,
    message: String,
    date: i64,
    /// It holds untracked files too.
    untracked: bool,
}

async fn list_stashes(root: &std::path::Path) -> Result<Vec<Stash>> {
    let (stdout, _, success) = run_git(
        root,
        args(&[
            "stash",
            "list",
            "-z",
            "--format=%gd%x1f%H%x1f%ct%x1f%P%x1f%gs",
        ]),
        GIT_TIMEOUT,
    )
    .await?;
    if !success {
        return Ok(Vec::new());
    }
    Ok(String::from_utf8_lossy(&stdout)
        .split('\0')
        .filter_map(|record| {
            let mut fields = record.trim_start_matches('\n').splitn(5, '\x1f');
            let index = fields
                .next()?
                .strip_prefix("stash@{")?
                .strip_suffix('}')?
                .parse()
                .ok()?;
            Some(Stash {
                index,
                hash: fields.next()?.to_owned(),
                date: fields.next()?.parse().unwrap_or_default(),
                untracked: fields.next()?.split(' ').count() > 2,
                message: fields.next()?.to_owned(),
            })
        })
        .collect())
}

async fn stashes(
    State(state): State<AppState>,
    Path(id): Path<WorkspaceId>,
) -> Result<Json<Vec<Stash>>> {
    Ok(Json(list_stashes(&repo(&state, id).await?).await?))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StashInput {
    #[serde(default)]
    message: Option<String>,
    #[serde(default)]
    include_untracked: bool,
    /// Leave what is staged in place as well as stashing it.
    #[serde(default)]
    keep_index: bool,
    /// Only these files; every change when empty.
    #[serde(default)]
    paths: Vec<String>,
}

async fn stash_push(
    State(state): State<AppState>,
    Path(id): Path<WorkspaceId>,
    Json(input): Json<StashInput>,
) -> Result<Json<Vec<Stash>>> {
    let _guard = state.operations.lock().await;
    let root = repo(&state, id).await?;
    let mut list = args(&["stash", "push"]);
    if input.include_untracked {
        list.push("--include-untracked".into());
    }
    if input.keep_index {
        list.push("--keep-index".into());
    }
    if let Some(message) = input
        .message
        .as_deref()
        .map(str::trim)
        .filter(|m| !m.is_empty())
    {
        list.extend(["-m".into(), message.to_owned()]);
    }
    if input.paths.len() > 1000 {
        return Err(ApiError::bad("Too many paths"));
    }
    if !input.paths.is_empty() {
        list.push("--".into());
        for path in &input.paths {
            list.push(format!(":(literal){}", checked_path(path)?));
        }
    }
    let before = list_stashes(&root).await?.first().map(|s| s.hash.clone());
    git(&root, list, COMMIT_TIMEOUT).await?;
    let after = list_stashes(&root).await?;
    if after.first().map(|s| &s.hash) == before.as_ref() {
        return Err(ApiError::conflict("There were no local changes to stash."));
    }
    Ok(Json(after))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StashRef {
    index: u32,
    /// The stash the person saw: refused if the list moved underneath them.
    hash: String,
    /// Apply and drop it, rather than only apply.
    #[serde(default)]
    pop: bool,
}

async fn stash_entry(root: &std::path::Path, input: &StashRef) -> Result<String> {
    let name = format!("stash@{{{}}}", input.index);
    let hash = git_text(root, &["rev-parse", "--verify", "--quiet", &name]).await?;
    if hash.as_deref() != Some(input.hash.as_str()) {
        return Err(ApiError::conflict(
            "The stash list changed. Refresh and try again.",
        ));
    }
    Ok(name)
}

async fn stash_apply(
    State(state): State<AppState>,
    Path(id): Path<WorkspaceId>,
    Json(input): Json<StashRef>,
) -> Result<Json<Outcome>> {
    let _guard = state.operations.lock().await;
    let root = repo(&state, id).await?;
    let name = stash_entry(&root, &input).await?;
    let verb = if input.pop { "pop" } else { "apply" };
    match finish(
        &root,
        args(&["stash", verb, "--index", &name]),
        COMMIT_TIMEOUT,
    )
    .await
    {
        // `--index` refuses, changing nothing, when the staged part no longer
        // applies; the changes themselves may still, unstaged.
        Err(_) => finish(&root, args(&["stash", verb, &name]), COMMIT_TIMEOUT).await,
        done => done,
    }
}

async fn stash_drop(
    State(state): State<AppState>,
    Path(id): Path<WorkspaceId>,
    Json(input): Json<StashRef>,
) -> Result<Json<Vec<Stash>>> {
    let _guard = state.operations.lock().await;
    let root = repo(&state, id).await?;
    let name = stash_entry(&root, &input).await?;
    git(&root, args(&["stash", "drop", &name]), GIT_TIMEOUT).await?;
    Ok(Json(list_stashes(&root).await?))
}

// --------------------------------------------------------------------- tags

#[derive(Serialize)]
struct Tag {
    name: String,
    /// The commit it marks.
    hash: String,
    annotated: bool,
    subject: String,
    date: i64,
}

async fn valid_tag(root: &std::path::Path, name: &str) -> Result<()> {
    if name.is_empty() || name.len() > 200 || name.starts_with('-') {
        return Err(ApiError::bad("Invalid tag name"));
    }
    git_text(root, &["check-ref-format", &format!("refs/tags/{name}")])
        .await?
        .map(|_| ())
        .ok_or_else(|| ApiError::bad("Invalid tag name"))
}

async fn list_tags(root: &std::path::Path) -> Result<Vec<Tag>> {
    let listing = git(
        root,
        args(&[
            "for-each-ref",
            "--sort=-creatordate",
            "--format=%(refname:short)%1f%(objecttype)%1f%(objectname)%1f%(*objectname)%1f%(creatordate:unix)%1f%(contents:subject)",
            "refs/tags",
        ]),
        GIT_TIMEOUT,
    )
    .await?;
    Ok(listing
        .lines()
        .filter_map(|line| {
            let fields: Vec<&str> = line.splitn(6, '\x1f').collect();
            let [name, kind, object, peeled, date, subject] = fields[..] else {
                return None;
            };
            let annotated = kind == "tag";
            Some(Tag {
                name: name.to_owned(),
                hash: if annotated { peeled } else { object }.to_owned(),
                annotated,
                subject: if annotated { subject } else { "" }.to_owned(),
                date: date.parse().unwrap_or_default(),
            })
        })
        .collect())
}

async fn tags(
    State(state): State<AppState>,
    Path(id): Path<WorkspaceId>,
) -> Result<Json<Vec<Tag>>> {
    Ok(Json(list_tags(&repo(&state, id).await?).await?))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TagInput {
    name: String,
    /// The commit to tag; HEAD when absent.
    #[serde(default)]
    rev: Option<String>,
    /// Makes an annotated tag.
    #[serde(default)]
    message: Option<String>,
}

async fn create_tag(
    State(state): State<AppState>,
    Path(id): Path<WorkspaceId>,
    Json(input): Json<TagInput>,
) -> Result<Json<Vec<Tag>>> {
    let _guard = state.operations.lock().await;
    let root = repo(&state, id).await?;
    let name = input.name.trim();
    valid_tag(&root, name).await?;
    let hash = valid_rev(&root, input.rev.as_deref().map(str::trim).unwrap_or("HEAD")).await?;
    let mut list = args(&["tag"]);
    if let Some(message) = input
        .message
        .as_deref()
        .map(str::trim)
        .filter(|m| !m.is_empty())
    {
        list.extend(["-a".into(), "-m".into(), message.to_owned()]);
    }
    list.extend([name.to_owned(), hash]);
    git(&root, list, COMMIT_TIMEOUT).await?;
    Ok(Json(list_tags(&root).await?))
}

async fn delete_tag(
    State(state): State<AppState>,
    Path(id): Path<WorkspaceId>,
    Json(input): Json<TagInput>,
) -> Result<Json<Vec<Tag>>> {
    let _guard = state.operations.lock().await;
    let root = repo(&state, id).await?;
    let name = input.name.trim();
    valid_tag(&root, name).await?;
    git(&root, args(&["tag", "-d", name]), GIT_TIMEOUT).await?;
    Ok(Json(list_tags(&root).await?))
}

// ------------------------------------------------------- hunks and the rest

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ApplyInput {
    /// The one file the patch changes; anything else in it is ignored.
    path: String,
    patch: String,
    /// Apply to the index (stage) rather than the working tree.
    #[serde(default)]
    cached: bool,
    /// Take the change back out: unstage, or discard.
    #[serde(default)]
    reverse: bool,
}

/// `git apply --include` reads a glob; the path is meant literally.
fn literal_glob(path: &str) -> String {
    path.chars()
        .flat_map(|c| match c {
            '*' | '?' | '[' | ']' | '\\' => vec!['\\', c],
            _ => vec![c],
        })
        .collect()
}

/// Stage, unstage or discard part of a file: the browser sends one hunk of
/// the diff it showed, and Git applies it to the index or the working tree.
async fn apply(
    State(state): State<AppState>,
    Path(id): Path<WorkspaceId>,
    Json(input): Json<ApplyInput>,
) -> Result<StatusCode> {
    let _guard = state.operations.lock().await;
    let root = repo(&state, id).await?;
    let path = checked_path(&input.path)?;
    if input.patch.len() > MAX_PATCH || input.patch.matches("diff --git ").count() != 1 {
        return Err(ApiError::bad("A patch for exactly one file is required"));
    }
    let file = std::env::temp_dir().join(format!("agentdock-hunk-{}.patch", uuid::Uuid::new_v4()));
    tokio::fs::write(&file, input.patch.as_bytes())
        .await
        .map_err(IoError::from)?;
    let mut list = args(&["apply", "--recount", "--whitespace=nowarn"]);
    if input.cached {
        list.push("--cached".into());
    }
    if input.reverse {
        list.push("--reverse".into());
    }
    list.push(format!("--include={}", literal_glob(&path)));
    list.push(file.to_string_lossy().into_owned());
    let result = git(&root, list, GIT_TIMEOUT).await;
    let _ = tokio::fs::remove_file(&file).await;
    result.map(|_| StatusCode::NO_CONTENT)
}

/// Make the workspace directory a repository of its own.
async fn init(State(state): State<AppState>, Path(id): Path<WorkspaceId>) -> Result<StatusCode> {
    let _guard = state.operations.lock().await;
    let root = repo(&state, id).await?;
    if git_text(&root, &["rev-parse", "--is-inside-work-tree"])
        .await?
        .is_some()
    {
        return Err(ApiError::conflict(
            "This directory is already in a Git repository.",
        ));
    }
    git(&root, args(&["init"]), GIT_TIMEOUT).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct IgnoreInput {
    path: String,
}

/// Add a path to the workspace's `.gitignore`, anchored to the root so it
/// names that one file or directory and nothing else of the same name.
async fn ignore(
    State(state): State<AppState>,
    Path(id): Path<WorkspaceId>,
    Json(input): Json<IgnoreInput>,
) -> Result<StatusCode> {
    let _guard = state.operations.lock().await;
    let root = repo(&state, id).await?;
    let path = checked_path(input.path.trim_end_matches('/'))?;
    let escaped: String = path
        .chars()
        .flat_map(|c| match c {
            '*' | '?' | '[' | '\\' | '!' | '#' => vec!['\\', c],
            _ => vec![c],
        })
        .collect();
    let line = format!("/{}", escaped.trim_end());
    let target = workspace_io::safe_path(&root, ".gitignore")?;
    let existing = tokio::fs::read_to_string(&target).await.unwrap_or_default();
    if existing.lines().any(|entry| entry.trim() == line) {
        return Ok(StatusCode::NO_CONTENT);
    }
    let mut next = existing;
    if !next.is_empty() && !next.ends_with('\n') {
        next.push('\n');
    }
    next.push_str(&line);
    next.push('\n');
    tokio::fs::write(&target, next)
        .await
        .map_err(IoError::from)?;
    Ok(StatusCode::NO_CONTENT)
}

pub fn routes() -> Router<AppState> {
    let base = "/api/workspaces/{id}/git";
    let at = |path: &str| format!("{base}/{path}");
    Router::new()
        .route(&at("log"), get(log))
        .route(&at("commit/detail"), get(commit_detail))
        .route(&at("commit/diff"), get(commit_diff))
        .route(&at("remotes"), get(remotes).post(add_remote))
        .route(&at("remotes/edit"), post(edit_remote))
        .route(&at("remotes/remove"), post(remove_remote))
        .route(&at("fetch"), post(fetch))
        .route(&at("pull"), post(pull))
        .route(&at("push"), post(push))
        .route(&at("merge"), post(merge))
        .route(&at("rebase"), post(rebase))
        .route(&at("cherry-pick"), post(cherry_pick))
        .route(&at("revert"), post(revert))
        .route(&at("reset"), post(reset))
        .route(&at("operation"), post(operation))
        .route(&at("resolve"), post(resolve))
        .route(&at("stashes"), get(stashes).post(stash_push))
        .route(&at("stashes/apply"), post(stash_apply))
        .route(&at("stashes/drop"), post(stash_drop))
        .route(&at("tags"), get(tags).post(create_tag))
        .route(&at("tags/delete"), post(delete_tag))
        .route(&at("apply"), post(apply))
        .route(&at("init"), post(init))
        .route(&at("ignore"), post(ignore))
}
