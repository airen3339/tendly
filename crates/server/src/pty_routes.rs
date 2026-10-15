//! A session's terminal stream over a WebSocket: framed output a reconnect
//! can resume from, and input, resizes and signals in.

use super::*;

#[derive(Deserialize, Default)]
pub(super) struct PtyQuery {
    cols: Option<u16>,
    rows: Option<u16>,
    /// The view numbers what it receives: each output frame starts with its
    /// sequence number (8 bytes, big-endian), after a `stream` message naming
    /// the process. That is what lets a reconnect resume instead of replay.
    #[serde(default)]
    framed: bool,
    /// Resume after this sequence number of this process, when the view
    /// already shows everything up to it.
    after: Option<u64>,
    runtime: Option<String>,
}
#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(super) enum PtyCommand {
    Input {
        data: String,
    },
    Resize {
        cols: u16,
        rows: u16,
    },
    /// A liveness probe; answered with `pong`, so a view can tell a quiet
    /// stream from a dead one.
    Ping,
}

/// Where a reconnecting view can pick up, if anywhere.
///
/// Only from the same process, and only while the bounded replay still holds
/// the very next event; otherwise the view has missed output and must be
/// redrawn from the full replay.
pub(super) fn resume_point(snapshot: &[RuntimeEvent], instance: &str, q: &PtyQuery) -> Option<u64> {
    let after = q.after?;
    if q.runtime.as_deref() != Some(instance) {
        return None;
    }
    let seq = |event: &RuntimeEvent| match event {
        RuntimeEvent::Output { seq, .. } | RuntimeEvent::Exited { seq, .. } => *seq,
    };
    match (snapshot.first(), snapshot.last()) {
        (Some(first), Some(last)) if seq(first) <= after + 1 && after <= seq(last) => Some(after),
        (None, None) if after == 0 => Some(0),
        _ => None,
    }
}

pub(super) async fn session_socket(
    State(state): State<AppState>,
    Path(id): Path<SessionId>,
    Query(q): Query<PtyQuery>,
    upgrade: WebSocketUpgrade,
) -> Result<Response> {
    session_record(&state, id).await?;
    if q.cols.is_some_and(|n| n == 0 || n > 1000) || q.rows.is_some_and(|n| n == 0 || n > 500) {
        return Err(ApiError::bad("Invalid terminal size"));
    }
    let runtime = state
        .runtime
        .get(&id.to_string())
        .ok_or_else(|| ApiError::conflict("No live runtime. Start the session explicitly."))?;
    Ok(upgrade
        .max_message_size(65536)
        .max_frame_size(65536)
        .on_upgrade(move |socket| stream_socket(socket, runtime, q)))
}

pub(super) async fn stream_socket(socket: WebSocket, runtime: Arc<RuntimeSession>, q: PtyQuery) {
    let mut events = runtime.subscribe();
    let snapshot = runtime.snapshot();
    let (mut sender, mut receiver) = socket.split();
    let resumed = if q.framed {
        resume_point(&snapshot, runtime.instance(), &q)
    } else {
        None
    };
    if q.framed {
        let hello =
            json!({"type":"stream","runtime":runtime.instance(),"resumed":resumed.is_some()});
        if send_frame(&mut sender, Message::Text(hello.to_string().into()))
            .await
            .is_err()
        {
            return;
        }
    }
    let mut last = resumed.unwrap_or(0);
    let mut exited = false;
    for event in snapshot {
        let (seq, message, is_exit) = wire_event(event, q.framed);
        if seq <= last {
            continue;
        }
        if send_frame(&mut sender, message).await.is_err() {
            return;
        }
        last = seq;
        exited = is_exit;
    }
    if exited {
        let _ = sender.send(Message::Close(None)).await;
        return;
    }
    if let (Some(cols), Some(rows)) = (q.cols, q.rows) {
        let _ = runtime.resize(cols, rows).await;
    }
    loop {
        tokio::select! {
            event=events.recv()=>match event {
                Ok(event)=>{
                    let (seq,message,is_exit)=wire_event(event,q.framed);
                    if seq<=last{continue;} last=seq;
                    if send_frame(&mut sender,message).await.is_err(){break;}
                    if is_exit{let _=sender.send(Message::Close(None)).await;break;}
                },
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_))=>{
                    let _=send_frame(&mut sender,Message::Text(json!({"type":"gap","message":"Output replay limit reached. Reconnect to the bounded history."}).to_string().into())).await;
                    let _=sender.send(Message::Close(None)).await;break;
                },
                Err(_)=>break,
            },
            message=receiver.next()=>match message {
                Some(Ok(Message::Binary(bytes)))=>{if let Err(error)=runtime.input(bytes.to_vec()).await{let _=send_frame(&mut sender,Message::Text(json!({"type":"error","message":error.to_string()}).to_string().into())).await;}},
                Some(Ok(Message::Text(text)))=>{
                    let result=match serde_json::from_str::<PtyCommand>(&text){
                        Ok(PtyCommand::Input{data})=>runtime.input(data.into_bytes()).await,
                        Ok(PtyCommand::Resize{cols,rows})=>runtime.resize(cols,rows).await,
                        Ok(PtyCommand::Ping)=>{if send_frame(&mut sender,Message::Text(json!({"type":"pong"}).to_string().into())).await.is_err(){break;} Ok(())},
                        Err(_)=>Err("Invalid terminal message".into()),
                    };
                    if let Err(error)=result {let _=send_frame(&mut sender,Message::Text(json!({"type":"error","message":error.to_string()}).to_string().into())).await;}
                },
                Some(Ok(Message::Ping(bytes)))=>{let _=sender.send(Message::Pong(bytes)).await;},
                Some(Ok(Message::Pong(_)))=>{},
                Some(Ok(Message::Close(_)))=>{let _=sender.send(Message::Close(None)).await;break;},
                None|Some(Err(_))=>break,
            }
        }
    }
    // A browser tab is a view, not the owner of the native process.
}
pub(super) fn wire_event(event: RuntimeEvent, framed: bool) -> (u64, Message, bool) {
    match event {
        RuntimeEvent::Output { seq, data } if framed => {
            let mut frame = Vec::with_capacity(8 + data.len());
            frame.extend_from_slice(&seq.to_be_bytes());
            frame.extend_from_slice(&data);
            (seq, Message::Binary(frame.into()), false)
        }
        RuntimeEvent::Output { seq, data } => (seq, Message::Binary(data.into()), false),
        RuntimeEvent::Exited { seq, code } => (
            seq,
            Message::Text(json!({"type":"exit","code":code}).to_string().into()),
            true,
        ),
    }
}
pub(super) async fn send_frame(
    sender: &mut futures_util::stream::SplitSink<WebSocket, Message>,
    message: Message,
) -> std::result::Result<(), ()> {
    tokio::time::timeout(Duration::from_secs(5), sender.send(message))
        .await
        .map_err(|_| ())?
        .map_err(|_| ())
}
