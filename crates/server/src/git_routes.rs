//! Git status, diffs, staging, branches and commits for a workspace, over
//! [`crate::workspace_io`].

use super::*;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct GitPaths {
    paths: Vec<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct BranchInput {
    branch: String,
    #[serde(default)]
    create: bool,
    /// End the sessions running in the workspace directory, then switch.
    #[serde(default)]
    stop_sessions: bool,
    /// With `create`: the commit or remote branch the new branch starts at.
    #[serde(default)]
    start_point: Option<String>,
    /// Check out the commit `branch` names, on no branch.
    #[serde(default)]
    detach: bool,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct CommitInput {
    message: String,
    /// Replace the last commit with this one instead of adding another.
    #[serde(default)]
    amend: bool,
}

pub(super) async fn git_status(
    State(state): State<AppState>,
    Path(id): Path<WorkspaceId>,
) -> Result<Json<workspace_io::GitStatus>> {
    Ok(Json(
        workspace_io::git_status(&root(&state, id).await?).await?,
    ))
}
pub(super) async fn git_diff(
    State(state): State<AppState>,
    Path(id): Path<WorkspaceId>,
    Query(q): Query<PathQuery>,
) -> Result<Json<workspace_io::GitDiff>> {
    Ok(Json(
        workspace_io::git_diff(&root(&state, id).await?, q.path.as_deref(), q.staged).await?,
    ))
}
pub(super) async fn git_stage(
    State(state): State<AppState>,
    Path(id): Path<WorkspaceId>,
    Json(input): Json<GitPaths>,
) -> Result<StatusCode> {
    let _guard = state.operations.lock().await;
    workspace_io::git_stage(&root(&state, id).await?, &input.paths).await?;
    Ok(StatusCode::NO_CONTENT)
}
pub(super) async fn git_unstage(
    State(state): State<AppState>,
    Path(id): Path<WorkspaceId>,
    Json(input): Json<GitPaths>,
) -> Result<StatusCode> {
    let _guard = state.operations.lock().await;
    workspace_io::git_unstage(&root(&state, id).await?, &input.paths).await?;
    Ok(StatusCode::NO_CONTENT)
}
pub(super) async fn git_discard(
    State(state): State<AppState>,
    Path(id): Path<WorkspaceId>,
    Json(input): Json<GitPaths>,
) -> Result<StatusCode> {
    let _guard = state.operations.lock().await;
    workspace_io::git_discard(&root(&state, id).await?, &input.paths).await?;
    Ok(StatusCode::NO_CONTENT)
}
pub(super) async fn git_branches(
    State(state): State<AppState>,
    Path(id): Path<WorkspaceId>,
) -> Result<Json<workspace_io::GitBranches>> {
    let listed = workspace_io::git_branches(&root(&state, id).await?).await?;
    checkouts::refresh_labels(&state, id, &listed).await;
    Ok(Json(listed))
}
/// Switching branches rewrites the files under every session working in this
/// checkout, so it waits until none of them is running: an agent halfway
/// through an edit would otherwise find its files replaced underneath it.
pub(super) async fn git_switch(
    State(state): State<AppState>,
    Path(id): Path<WorkspaceId>,
    Json(input): Json<BranchInput>,
) -> Result<StatusCode> {
    let _guard = state.operations.lock().await;
    let busy = db(&state, move |s| s.list_sessions(Some(id)))
        .await?
        .into_iter()
        // Only sessions in the workspace directory itself have their files
        // rewritten; one in its own worktree is untouched by this switch.
        .filter(|session| {
            session.checkout_path.is_none()
                && matches!(
                    session.status,
                    SessionStatus::Running | SessionStatus::Starting | SessionStatus::Waiting
                )
        })
        .map(|session| session.id)
        .collect::<Vec<_>>();
    if !busy.is_empty() && !input.stop_sessions {
        return Err(ApiError::conflict(format!(
            "{} session(s) are running in the workspace directory. End them first, or move a session to another branch from its own branch chip instead.",
            busy.len()
        )));
    }
    for session in busy {
        stop_session_locked(&state, session).await?;
    }
    workspace_io::git_switch(
        &root(&state, id).await?,
        input.branch.trim(),
        input.create,
        input.start_point.as_deref().map(str::trim),
        input.detach,
    )
    .await?;
    Ok(StatusCode::NO_CONTENT)
}
pub(super) async fn git_commit(
    State(state): State<AppState>,
    Path(id): Path<WorkspaceId>,
    Json(input): Json<CommitInput>,
) -> Result<Json<Value>> {
    let _guard = state.operations.lock().await;
    Ok(Json(
        json!({"commit":workspace_io::git_commit(&root(&state,id).await?,&input.message,input.amend).await?}),
    ))
}

/// The routes this module serves.
pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/workspaces/{id}/git/status", get(git_status))
        .route("/api/workspaces/{id}/git/diff", get(git_diff))
        .route("/api/workspaces/{id}/git/stage", post(git_stage))
        .route("/api/workspaces/{id}/git/unstage", post(git_unstage))
        .route("/api/workspaces/{id}/git/discard", post(git_discard))
        .route("/api/workspaces/{id}/git/branches", get(git_branches))
        .route("/api/workspaces/{id}/git/switch", post(git_switch))
        .route("/api/workspaces/{id}/git/commit", post(git_commit))
}
