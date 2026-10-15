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

/// The routes this module serves.
pub(super) fn routes() -> Router<AppState> {
    Router::new().route("/api/sessions/{id}/pty/ws", get(session_socket))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn output(seq: u64) -> RuntimeEvent {
        RuntimeEvent::Output {
            seq,
            data: vec![b'x'],
        }
    }
    fn query(runtime: &str, after: Option<u64>) -> PtyQuery {
        PtyQuery {
            runtime: Some(runtime.into()),
            after,
            framed: true,
            ..PtyQuery::default()
        }
    }

    #[test]
    fn a_reconnect_resumes_only_when_the_replay_still_reaches_back_far_enough() {
        let replay = [output(5), output(6), output(7)];
        // The view drew up to 4: the replay starts at 5, so nothing is missing.
        assert_eq!(resume_point(&replay, "r1", &query("r1", Some(4))), Some(4));
        assert_eq!(resume_point(&replay, "r1", &query("r1", Some(7))), Some(7));
        // Up to 3: frame 4 has already left the replay, so the view must redraw.
        assert_eq!(resume_point(&replay, "r1", &query("r1", Some(3))), None);
        // Ahead of what was ever sent.
        assert_eq!(resume_point(&replay, "r1", &query("r1", Some(8))), None);
        // A different process, or no resume point at all.
        assert_eq!(resume_point(&replay, "r2", &query("r1", Some(5))), None);
        assert_eq!(resume_point(&replay, "r1", &query("r1", None)), None);
        // Nothing sent yet: resuming from the start is resuming.
        assert_eq!(resume_point(&[], "r1", &query("r1", Some(0))), Some(0));
        assert_eq!(resume_point(&[], "r1", &query("r1", Some(1))), None);
    }

    #[test]
    fn framed_output_carries_its_sequence_number_and_exit_ends_the_stream() {
        let (seq, message, last) = wire_event(
            RuntimeEvent::Output {
                seq: 258,
                data: b"hi".to_vec(),
            },
            true,
        );
        assert_eq!((seq, last), (258, false));
        let Message::Binary(frame) = message else {
            panic!("binary expected")
        };
        assert_eq!(&frame[..], &[0, 0, 0, 0, 0, 0, 1, 2, b'h', b'i']);
        let (_, message, _) = wire_event(
            RuntimeEvent::Output {
                seq: 1,
                data: b"hi".to_vec(),
            },
            false,
        );
        let Message::Binary(plain) = message else {
            panic!("binary expected")
        };
        assert_eq!(&plain[..], b"hi", "an unframed view gets the bytes alone");
        let (seq, message, last) = wire_event(
            RuntimeEvent::Exited {
                seq: 9,
                code: Some(3),
            },
            true,
        );
        assert_eq!((seq, last), (9, true));
        let Message::Text(text) = message else {
            panic!("text expected")
        };
        assert_eq!(
            serde_json::from_str::<Value>(&text).unwrap(),
            json!({"type": "exit", "code": 3})
        );
    }
}
