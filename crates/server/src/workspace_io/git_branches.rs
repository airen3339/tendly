//! Branches, worktrees and commits.

use super::*;

#[derive(Debug, Clone, Serialize)]
pub struct GitWorktree {
    pub path: String,
    pub branch: Option<String>,
    /// The repository's own checkout, as opposed to one added beside it.
    pub main: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct GitBranches {
    /// None when HEAD is detached.
    pub current: Option<String>,
    pub branches: Vec<String>,
    pub worktrees: Vec<GitWorktree>,
    /// The local branches again, with where each stands against its upstream.
    pub local: Vec<GitRef>,
    /// Remote-tracking branches, as `origin/main`.
    pub remote: Vec<GitRef>,
}

#[derive(Debug, Clone, Serialize)]
pub struct GitRef {
    pub name: String,
    pub hash: String,
    pub subject: String,
    /// Committer time of the tip, in seconds.
    pub date: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub upstream: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ahead: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub behind: Option<u32>,
    /// The upstream it tracked has been deleted on the remote.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub gone: bool,
}

/// Branch tips, local and remote-tracking, read in one `for-each-ref`.
pub(super) async fn git_refs(root: &Path) -> Result<(Vec<GitRef>, Vec<GitRef>), IoError> {
    let listing = git_text(
        root,
        &[
            "for-each-ref",
            "--sort=-committerdate",
            "--format=%(refname)%1f%(symref)%1f%(objectname)%1f%(committerdate:unix)%1f%(upstream:short)%1f%(upstream:track,nobracket)%1f%(subject)",
            "refs/heads",
            "refs/remotes",
        ],
    )
    .await?
    .unwrap_or_default();
    let (mut local, mut remote) = (Vec::new(), Vec::new());
    for line in listing.lines() {
        let fields: Vec<&str> = line.split('\x1f').collect();
        let [refname, symref, hash, date, upstream, track, subject] = fields[..] else {
            continue;
        };
        if !symref.is_empty() {
            continue;
        }
        let count = |label: &str| {
            track
                .split(", ")
                .find_map(|part| part.strip_prefix(label))
                .and_then(|n| n.trim().parse().ok())
        };
        let mut entry = GitRef {
            name: String::new(),
            hash: hash.to_owned(),
            subject: subject.to_owned(),
            date: date.parse().unwrap_or_default(),
            upstream: (!upstream.is_empty()).then(|| upstream.to_owned()),
            ahead: count("ahead "),
            behind: count("behind "),
            gone: track == "gone",
        };
        if let Some(name) = refname.strip_prefix("refs/heads/") {
            entry.name = name.to_owned();
            local.push(entry);
        } else if let Some(name) = refname.strip_prefix("refs/remotes/") {
            entry.name = name.to_owned();
            remote.push(entry);
        }
    }
    Ok((local, remote))
}

pub(crate) async fn git_text(root: &Path, args: &[&str]) -> Result<Option<String>, IoError> {
    let (stdout, _stderr, success) = run_git(
        root,
        args.iter().map(|arg| (*arg).to_owned()).collect(),
        GIT_TIMEOUT,
    )
    .await?;
    Ok(success.then(|| String::from_utf8_lossy(&stdout).trim().to_owned()))
}

/// Local branches and the worktrees of this repository.
pub async fn git_branches(root: &Path) -> Result<GitBranches, IoError> {
    let root = dunce::canonicalize(root).map_err(IoError::from)?;
    let branches = git_text(&root, &["branch", "--format=%(refname:short)"])
        .await?
        .ok_or_else(|| IoError::new(400, "Not a Git repository"))?;
    let current = git_text(&root, &["symbolic-ref", "--short", "-q", "HEAD"])
        .await?
        .filter(|name| !name.is_empty());
    let listing = git_text(&root, &["worktree", "list", "--porcelain"])
        .await?
        .unwrap_or_default();
    let (local, remote) = git_refs(&root).await?;
    Ok(GitBranches {
        local,
        remote,
        current,
        branches: branches
            .lines()
            .map(str::trim)
            .filter(|name| !name.is_empty() && !name.starts_with('('))
            .map(str::to_owned)
            .collect(),
        worktrees: parse_worktrees(&listing),
    })
}

pub(crate) fn parse_worktrees(listing: &str) -> Vec<GitWorktree> {
    listing
        .split("\n\n")
        .filter_map(|block| {
            let mut path = None;
            let mut branch = None;
            let mut bare = false;
            for line in block.lines() {
                if let Some(value) = line.strip_prefix("worktree ") {
                    // Git for Windows prints `C:/…`; rebuilding from components
                    // gives the native form every other path here is in.
                    let native: PathBuf = Path::new(value).components().collect();
                    path = Some(native.to_string_lossy().into_owned());
                } else if let Some(value) = line.strip_prefix("branch ") {
                    branch = Some(value.trim_start_matches("refs/heads/").to_owned());
                } else if line == "bare" {
                    bare = true;
                }
            }
            (!bare).then_some(())?;
            path.map(|path| GitWorktree {
                path,
                branch,
                main: false,
            })
        })
        .enumerate()
        .map(|(index, mut worktree)| {
            worktree.main = index == 0;
            worktree
        })
        .collect()
}

/// A name Git itself accepts for a branch, checked by Git rather than guessed.
pub(crate) async fn valid_branch(root: &Path, branch: &str) -> Result<(), IoError> {
    if branch.is_empty() || branch.len() > 200 || branch.starts_with('-') {
        return Err(IoError::new(400, "Invalid branch name"));
    }
    match git_text(root, &["check-ref-format", "--branch", branch]).await? {
        Some(_) => Ok(()),
        None => Err(IoError::new(400, "Invalid branch name")),
    }
}

/// A revision -- a commit hash, branch, remote branch or tag -- that names a
/// commit here, resolved to that commit's full hash. Git checks it; nothing
/// shaped like an option ever reaches a command line as one.
pub(crate) async fn valid_rev(root: &Path, rev: &str) -> Result<String, IoError> {
    if rev.is_empty()
        || rev.len() > 256
        || rev.starts_with('-')
        || rev.chars().any(|c| c.is_whitespace() || c.is_control())
    {
        return Err(IoError::new(400, "Invalid revision"));
    }
    git_text(
        root,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            "--end-of-options",
            &format!("{rev}^{{commit}}"),
        ],
    )
    .await?
    .filter(|hash| !hash.is_empty())
    .ok_or_else(|| IoError::new(404, format!("{rev} is not a commit in this repository")))
}

/// Switch this checkout to another local branch, or create one here --
/// from HEAD, or from `start` (a commit or a remote branch, which the new
/// branch then tracks). `detach` checks out the commit `branch` names without
/// any branch. Git refuses a switch that would overwrite uncommitted work, and
/// that refusal is passed on rather than forced through.
pub async fn git_switch(
    root: &Path,
    branch: &str,
    create: bool,
    start: Option<&str>,
    detach: bool,
) -> Result<(), IoError> {
    let root = dunce::canonicalize(root).map_err(IoError::from)?;
    let mut args: Vec<String> = vec!["switch".into()];
    if detach {
        args.extend(["--detach".into(), valid_rev(&root, branch).await?]);
    } else {
        valid_branch(&root, branch).await?;
        if create {
            args.push("-c".into());
        }
        args.push(branch.to_owned());
        if let Some(start) = start.filter(|_| create) {
            valid_rev(&root, start).await?;
            args.push(start.to_owned());
        }
    }
    let (_stdout, stderr, success) = run_git(&root, args, GIT_TIMEOUT).await?;
    if success {
        Ok(())
    } else {
        Err(IoError::new(409, git_error(&stderr)))
    }
}

/// Rename a local branch. A worktree that has it checked out follows the new
/// name -- Git updates it -- so no checkout has to move.
pub async fn git_branch_rename(root: &Path, from: &str, to: &str) -> Result<(), IoError> {
    let root = dunce::canonicalize(root).map_err(IoError::from)?;
    valid_branch(&root, from).await?;
    valid_branch(&root, to).await?;
    let args = vec!["branch".into(), "-m".into(), from.to_owned(), to.to_owned()];
    let (_stdout, stderr, success) = run_git(&root, args, GIT_TIMEOUT).await?;
    if success {
        Ok(())
    } else {
        Err(IoError::new(409, git_error(&stderr)))
    }
}

/// Delete a local branch. Git refuses one that is checked out anywhere, and
/// -- unless `force` -- one whose commits are not merged; both are passed on.
pub async fn git_branch_delete(root: &Path, branch: &str, force: bool) -> Result<(), IoError> {
    let root = dunce::canonicalize(root).map_err(IoError::from)?;
    valid_branch(&root, branch).await?;
    let args = vec![
        "branch".into(),
        if force { "-D" } else { "-d" }.into(),
        branch.to_owned(),
    ];
    let (_stdout, stderr, success) = run_git(&root, args, GIT_TIMEOUT).await?;
    if success {
        Ok(())
    } else {
        Err(IoError::new(409, git_error(&stderr)))
    }
}

/// Remove a worktree of this repository -- its directory, not its branch.
/// Git refuses one with uncommitted changes unless `force`; the repository's
/// own checkout can never be removed this way.
pub async fn git_worktree_remove(root: &Path, path: &str, force: bool) -> Result<(), IoError> {
    let root = dunce::canonicalize(root).map_err(IoError::from)?;
    let listing = git_text(&root, &["worktree", "list", "--porcelain"])
        .await?
        .ok_or_else(|| IoError::new(400, "Not a Git repository"))?;
    let tree = parse_worktrees(&listing)
        .into_iter()
        .find(|tree| tree.path == path)
        .ok_or_else(|| IoError::new(400, "Not a worktree of this repository"))?;
    if tree.main {
        return Err(IoError::new(
            400,
            "The repository's own checkout cannot be removed",
        ));
    }
    let mut args: Vec<String> = vec!["worktree".into(), "remove".into()];
    if force {
        args.push("--force".into());
    }
    args.push(tree.path);
    let (_stdout, stderr, success) = run_git(&root, args, GIT_TIMEOUT).await?;
    if success {
        Ok(())
    } else {
        Err(IoError::new(409, git_error(&stderr)))
    }
}

/// Where a new worktree for `branch` goes: beside the repository's own
/// checkout, in `<repo>.worktrees/<branch>`, so every worktree of one
/// repository sits together and none of them inside another.
pub fn worktree_path(main_checkout: &Path, branch: &str) -> Result<PathBuf, IoError> {
    let name = main_checkout
        .file_name()
        .ok_or_else(|| IoError::new(400, "The repository has no directory name"))?;
    let parent = main_checkout
        .parent()
        .ok_or_else(|| IoError::new(400, "The repository has no parent directory"))?;
    let slug: String = branch
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '-'
            }
        })
        .collect();
    let slug = slug.trim_matches(|c| c == '-' || c == '.');
    if slug.is_empty() {
        return Err(IoError::new(400, "Invalid branch name"));
    }
    Ok(parent
        .join(format!("{}.worktrees", name.to_string_lossy()))
        .join(slug))
}

/// Add a worktree for `branch` (created from HEAD when `create`), returning
/// its directory. An existing directory is never reused or overwritten.
pub async fn git_worktree_add(root: &Path, branch: &str, create: bool) -> Result<PathBuf, IoError> {
    let root = dunce::canonicalize(root).map_err(IoError::from)?;
    valid_branch(&root, branch).await?;
    let listing = git_text(&root, &["worktree", "list", "--porcelain"])
        .await?
        .ok_or_else(|| IoError::new(400, "Not a Git repository"))?;
    let main = parse_worktrees(&listing)
        .into_iter()
        .find(|worktree| worktree.main)
        .ok_or_else(|| IoError::new(400, "Not a Git repository"))?;
    let target = worktree_path(Path::new(&main.path), branch)?;
    if target.exists() {
        return Err(IoError::new(
            409,
            format!("{} already exists", target.display()),
        ));
    }
    if let Some(parent) = target.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let mut args: Vec<String> = vec!["worktree".into(), "add".into()];
    if create {
        args.extend([
            "-b".into(),
            branch.to_owned(),
            target.to_string_lossy().into_owned(),
        ]);
    } else {
        args.extend([target.to_string_lossy().into_owned(), branch.to_owned()]);
    }
    let (_stdout, stderr, success) = run_git(&root, args, COMMIT_TIMEOUT).await?;
    if !success {
        return Err(IoError::new(409, git_error(&stderr)));
    }
    Ok(target)
}

pub async fn git_commit(root: &Path, message: &str, amend: bool) -> Result<String, IoError> {
    if message.trim().is_empty() {
        return Err(IoError::new(400, "commit message is required"));
    }
    let root = dunce::canonicalize(root).map_err(IoError::from)?;
    let mut args: Vec<String> = vec!["commit".into()];
    if amend {
        args.push("--amend".into());
    }
    args.extend(["-m".into(), message.to_owned()]);
    let (_stdout, stderr, success) = run_git(&root, args, COMMIT_TIMEOUT).await?;
    if !success {
        return Err(IoError::new(400, git_error(&stderr)));
    }
    let (stdout, stderr, success) =
        run_git(&root, vec!["rev-parse".into(), "HEAD".into()], GIT_TIMEOUT).await?;
    if !success {
        return Err(IoError::new(400, git_error(&stderr)));
    }
    Ok(String::from_utf8_lossy(&stdout).trim().to_owned())
}
