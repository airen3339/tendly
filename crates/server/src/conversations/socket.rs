//! The WebSocket a view follows a conversation on.

use super::*;

pub(super) fn routes() -> Router<AppState> {
    Router::new().route("/api/sessions/{id}/chat/ws", get(socket))
}

/// Where a view that already holds part of the conversation resumes.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Resume {
    /// The last event the view holds; the greeting carries only what follows.
    after: Option<u64>,
}
async fn socket(
    State(state): State<AppState>,
    Path(id): Path<SessionId>,
    Query(resume): Query<Resume>,
    upgrade: WebSocketUpgrade,
) -> Result<Response> {
    session_record(&state, id).await?;
    Ok(upgrade
        .max_message_size(1024)
        .on_upgrade(move |socket| stream(state, id, socket, resume.after)))
}
/// The greeting a socket opens with: the whole conversation, or -- for a view
/// that says which event it holds last -- only the events after it, marked
/// with `after` so the view adds them instead of replacing what it has.
/// Sequences never reset, so a view's last event is always a valid place to
/// resume from; one claiming an event past the end gets everything.
/// The greeting a socket opens with: the latest page, or -- for a view that
/// says which event it holds last -- only the events after it, marked with
/// `after` so the view adds them instead of replacing what it has. A view too
/// far behind, or holding an event past the end, gets the latest page.
async fn greeting(state: &AppState, id: SessionId, after: Option<u64>) -> Result<(Value, u64)> {
    let last = |events: &Value, floor: u64| {
        events
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|e| e["seq"].as_u64())
            .fold(floor, u64::max)
    };
    if let Some(after) = after
        && let Some(events) = db(state, move |s| {
            s.conversation_after(id, after, RESUME_LIMIT)
        })
        .await?
    {
        let (mode, running) = state_of(state, id).await?;
        let events = json!(events);
        let last_seq = last(&events, after);
        return Ok((
            json!({"type":"snapshot","mode":mode,"running":running,"events":events,"after":after}),
            last_seq,
        ));
    }
    let snapshot = snapshot(state, id).await?;
    let last_seq = last(&snapshot["events"], 0);
    Ok((snapshot, last_seq))
}
async fn stream(state: AppState, id: SessionId, socket: WebSocket, after: Option<u64>) {
    let mut events = state.chats.channel(id).subscribe();
    let (mut sender, mut receiver) = socket.split();
    let Ok((initial, mut last_seq)) = greeting(&state, id, after).await else {
        return;
    };
    if crate::send_frame(&mut sender, Message::Text(initial.to_string().into()))
        .await
        .is_err()
    {
        return;
    }
    loop {
        tokio::select! {
            event=events.recv()=> {
                let value=match event {Ok(value)=>value,Err(broadcast::error::RecvError::Lagged(_))=>match snapshot(&state,id).await{Ok(v)=>v,Err(_)=>break},Err(_)=>break};
                if let Some(seq)=value["seq"].as_u64(){if seq<=last_seq{continue;}last_seq=seq;}
                if value["type"]=="snapshot" && let Some(seq)=value["events"].as_array().and_then(|v|v.last()).and_then(|v|v["seq"].as_u64()){last_seq=seq;}
                if crate::send_frame(&mut sender,Message::Text(value.to_string().into())).await.is_err(){break;}
            }
            incoming=receiver.next()=>match incoming {
                Some(Ok(Message::Ping(bytes)))=>{if crate::send_frame(&mut sender,Message::Pong(bytes)).await.is_err(){break;}},
                Some(Ok(Message::Pong(_)))=>{},
                // A browser cannot send a WebSocket ping, so the view probes
                // with a message of its own to tell a quiet stream from a dead one.
                Some(Ok(Message::Text(text))) if serde_json::from_str::<Value>(&text).is_ok_and(|v| v["type"]=="ping")=>{if crate::send_frame(&mut sender,Message::Text(json!({"type":"pong"}).to_string().into())).await.is_err(){break;}},
                _=>break,
            },
        }
    }
}
