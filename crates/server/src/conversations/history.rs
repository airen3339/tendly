//! The stored conversation: appending events, the pages and snapshots a view
//! reads, and searching it.

use super::*;

pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/sessions/{id}/conversation", get(get_conversation))
        .route(
            "/api/sessions/{id}/conversation/search",
            get(search_conversation),
        )
}

/// How many events a page holds: the latest page a view opens with, and the
/// most an older page may ask for. History is kept in full and read in pages.
const PAGE: usize = 1000;
/// A view at most this many events behind is sent what it missed when it
/// resumes; one further behind is sent the latest page instead.
pub(super) const RESUME_LIMIT: usize = 5000;

/// The session's mode, and whether its process is running.
pub(super) async fn state_of(state: &AppState, id: SessionId) -> Result<(InteractionMode, bool)> {
    let session = session_record(state, id).await?;
    let running = if session.interaction_mode == InteractionMode::Structured {
        state.chats.get(id).is_some_and(|r| r.running())
    } else {
        state
            .runtime
            .get(&id.to_string())
            .is_some_and(|r| r.running())
    };
    Ok((session.interaction_mode, running))
}
/// The latest page of the conversation, with the state a view needs to read it.
pub(super) async fn snapshot(state: &AppState, id: SessionId) -> Result<Value> {
    snapshot_of(state, id, PAGE).await
}
async fn snapshot_of(state: &AppState, id: SessionId, limit: usize) -> Result<Value> {
    let (mode, running) = state_of(state, id).await?;
    let page = db(state, move |s| s.conversation_page(id, None, limit)).await?;
    Ok(
        json!({"type":"snapshot","mode":mode,"running":running,"events":page.events,"truncated":page.truncated,"older":page.older}),
    )
}
/// The most places one search reports: enough to step through, few enough to read.
const SEARCH_LIMIT: usize = 200;
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SearchQuery {
    q: String,
    limit: Option<usize>,
}
/// Where words appear in the whole conversation, newest first: the event,
/// the message or call it belongs to, and the words around it. The view
/// reads older pages as far as it needs to show one.
async fn search_conversation(
    State(state): State<AppState>,
    Path(id): Path<SessionId>,
    Query(query): Query<SearchQuery>,
) -> Result<Json<Value>> {
    session_record(&state, id).await?;
    if query.q.chars().count() > 200 {
        return Err(ApiError::bad("A search is at most 200 characters"));
    }
    let limit = query.limit.unwrap_or(SEARCH_LIMIT).clamp(1, SEARCH_LIMIT);
    let text = query.q.clone();
    // One more than asked says whether there are more than were sent.
    let mut hits = db(&state, move |s| s.search_conversation(id, &text, limit + 1)).await?;
    let more = hits.len() > limit;
    hits.truncate(limit);
    let hits: Vec<Value> = hits
        .into_iter()
        .map(|hit| json!({"seq":hit.seq,"kind":hit.kind,"id":hit.id,"snippet":hit.snippet}))
        .collect();
    Ok(Json(json!({"hits":hits,"more":more})))
}
/// Which page of a conversation: the latest, or the one before `before`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PageQuery {
    before: Option<u64>,
    limit: Option<usize>,
}
async fn get_conversation(
    State(state): State<AppState>,
    Path(id): Path<SessionId>,
    Query(query): Query<PageQuery>,
) -> Result<Json<Value>> {
    let limit = query.limit.unwrap_or(PAGE).clamp(1, PAGE);
    let Some(before) = query.before else {
        return snapshot_of(&state, id, limit).await.map(Json);
    };
    session_record(&state, id).await?;
    let page = db(&state, move |s| {
        s.conversation_page(id, Some(before), limit)
    })
    .await?;
    Ok(Json(
        json!({"type":"page","events":page.events,"older":page.older}),
    ))
}

pub async fn persist(state: &AppState, id: SessionId, event: Value) -> Result<()> {
    let event = db(state, move |s| s.append_conversation_event(id, event)).await?;
    state.chats.publish(id, event);
    Ok(())
}

/// Tell connected views the conversation changed outside a turn.
pub async fn publish_snapshot(state: &AppState, id: SessionId) {
    if let Ok(value) = snapshot(state, id).await {
        state.chats.publish(id, value);
    }
}
