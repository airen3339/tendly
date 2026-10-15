//! Terminals: reconnecting to a running process, opening a structured session in a terminal, and pasting files into one.

use super::*;

#[tokio::test]
async fn websocket_detach_reconnect_retains_native_process() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let f = Fixture::new(addr, None);
    let w = f
        .state
        .store
        .create_workspace("pty", f.path.join("repo").to_str().unwrap())
        .unwrap();
    let s = f
        .state
        .store
        .create_session(w.id, ProviderKind::Terminal, "shell")
        .unwrap();
    let native = f
        .state
        .runtime
        .start(
            s.id.to_string(),
            tendly_runtime::SpawnSpec {
                program: waiting_process(&["-i"]).0,
                args: waiting_process(&["-i"]).1,
                cwd: f.path.join("repo"),
                env: BTreeMap::new(),
                env_remove: Vec::new(),
            },
        )
        .await
        .unwrap();
    let app = f.app();
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let url = format!("ws://{addr}/api/sessions/{}/pty/ws", s.id);
    let mut req = url.clone().into_client_request().unwrap();
    req.headers_mut()
        .insert("origin", format!("http://{addr}").parse().unwrap());
    let (mut ws, _) = connect_async(req).await.unwrap();
    ws.send(WsMessage::Text(
        json!({"type":"input","data":if cfg!(windows) { announce("AD", "SOCKET") } else { format!("stty -echo; {}", announce("AD", "SOCKET")) }})
            .to_string()
            .into(),
    ))
    .await
    .unwrap();
    let mut bytes = Vec::new();
    tokio::time::timeout(Duration::from_secs(4), async {
        while let Some(Ok(event)) = ws.next().await {
            if let WsMessage::Binary(data) = event {
                bytes.extend_from_slice(&data);
                if String::from_utf8_lossy(&bytes).contains("AD_SOCKET_READY") {
                    break;
                }
            }
        }
    })
    .await
    .unwrap();
    assert!(String::from_utf8_lossy(&bytes).contains("AD_SOCKET_READY"));
    ws.close(None).await.unwrap();
    drop(ws);
    assert!(native.running());
    let (mut ws, _) = connect_async(url.clone()).await.unwrap();
    let mut replay = Vec::new();
    tokio::time::timeout(Duration::from_secs(4), async {
        while let Some(Ok(event)) = ws.next().await {
            if let WsMessage::Binary(data) = event {
                replay.extend_from_slice(&data);
                if String::from_utf8_lossy(&replay).contains("AD_SOCKET_READY") {
                    break;
                }
            }
        }
    })
    .await
    .unwrap();
    assert!(String::from_utf8_lossy(&replay).contains("AD_SOCKET_READY"));
    assert!(Arc::ptr_eq(
        &native,
        &f.state.runtime.get(&s.id.to_string()).unwrap()
    ));
    ws.close(None).await.ok();

    // A framed view numbers what it receives and can resume after it.
    let control = |message: &WsMessage| match message {
        WsMessage::Text(text) => serde_json::from_str::<Value>(text).ok(),
        _ => None,
    };
    let (mut ws, _) = connect_async(format!("{url}?framed=true")).await.unwrap();
    let hello = control(&ws.next().await.unwrap().unwrap()).unwrap();
    assert_eq!(hello["type"], "stream");
    assert_eq!(hello["resumed"], false);
    let instance = hello["runtime"].as_str().unwrap().to_owned();
    let mut seen = 0u64;
    let mut framed = Vec::new();
    tokio::time::timeout(Duration::from_secs(4), async {
        while let Some(Ok(WsMessage::Binary(data))) = ws.next().await {
            let seq = u64::from_be_bytes(data[..8].try_into().unwrap());
            assert!(seq > seen, "frames arrive in sequence order");
            seen = seq;
            framed.extend_from_slice(&data[8..]);
            if String::from_utf8_lossy(&framed).contains("AD_SOCKET_READY") {
                break;
            }
        }
    })
    .await
    .unwrap();
    ws.close(None).await.ok();
    drop(ws);
    let resume = format!("{url}?framed=true&runtime={instance}&after={seen}");
    let (mut ws, _) = connect_async(resume).await.unwrap();
    let hello = control(&ws.next().await.unwrap().unwrap()).unwrap();
    assert_eq!(hello["resumed"], true);
    ws.send(WsMessage::Text(json!({"type":"ping"}).to_string().into()))
        .await
        .unwrap();
    ws.send(WsMessage::Text(
        json!({"type":"input","data":announce("AD", "RESUME")})
            .to_string()
            .into(),
    ))
    .await
    .unwrap();
    let mut ponged = false;
    let mut after = Vec::new();
    tokio::time::timeout(Duration::from_secs(4), async {
        while let Some(Ok(event)) = ws.next().await {
            match event {
                WsMessage::Binary(data) => {
                    let seq = u64::from_be_bytes(data[..8].try_into().unwrap());
                    assert!(seq > seen, "a resumed view is not sent what it already has");
                    after.extend_from_slice(&data[8..]);
                    if String::from_utf8_lossy(&after).contains("AD_RESUME_READY") {
                        break;
                    }
                }
                other => ponged |= control(&other).is_some_and(|v| v["type"] == "pong"),
            }
        }
    })
    .await
    .unwrap();
    assert!(ponged);
    // The sequence numbers above are the guarantee. ConPTY repaints the whole
    // screen on Windows, so earlier text can reappear there as new output.
    if !cfg!(windows) {
        assert!(!String::from_utf8_lossy(&after).contains("AD_SOCKET_READY"));
    }
    ws.close(None).await.ok();
    drop(ws);
    // Another process's numbering is not this one's: the view starts over.
    let (mut ws, _) = connect_async(format!("{url}?framed=true&runtime=other&after={seen}"))
        .await
        .unwrap();
    let hello = control(&ws.next().await.unwrap().unwrap()).unwrap();
    assert_eq!(hello["resumed"], false);
    native.stop().await.unwrap();
    ws.close(None).await.ok();
    server.abort();
}

#[tokio::test]
async fn opening_a_structured_session_in_a_terminal_resumes_its_own_configuration_home() {
    let f = Fixture::new("127.0.0.1:8787".parse().unwrap(), None);
    let cwd = dunce::canonicalize(f.path.join("repo")).unwrap();
    let workspace = f
        .state
        .store
        .create_workspace("fixture", cwd.to_str().unwrap())
        .unwrap();
    // A structured session that has already reported the conversation it owns.
    let source = f
        .state
        .store
        .create_session_with_configuration(
            workspace.id,
            ProviderKind::ClaudeCode,
            "Chat session",
            None,
            None,
            None,
            Default::default(),
            false,
        )
        .unwrap();
    f.state
        .store
        .set_interaction_mode(source.id, InteractionMode::Structured)
        .unwrap();
    f.state
        .store
        .set_native_conversation_id(source.id, "11111111-2222-3333-4444-555555555555")
        .unwrap();
    let (status, terminal) = call(
        f.app(),
        "POST",
        &format!("/api/sessions/{}/terminal", source.id),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(terminal["resume_source_id"], source.id.to_string());
    assert_eq!(
        terminal["ephemeral"], true,
        "closing the escape hatch leaves nothing behind; its conversation lives in the session it reopened"
    );
    assert_eq!(
        terminal["status"], "stopped",
        "creating a session launches nothing; the caller starts it through the ordinary route"
    );
    assert_eq!(
        terminal["provider_session_id"],
        "11111111-2222-3333-4444-555555555555"
    );
    assert_eq!(terminal["provider"], "claude_code");
    assert_eq!(terminal["interaction_mode"], "pty");
    assert!(
        terminal["title"]
            .as_str()
            .unwrap()
            .starts_with("Chat session"),
        "the reopen is named after the conversation it reopens"
    );
    let record: Session = serde_json::from_value(terminal).unwrap();
    let spec = launch::build(&f.state, &record, cwd).unwrap();
    assert_eq!(
        without_tendly(&spec.args),
        vec!["--resume", "11111111-2222-3333-4444-555555555555"]
    );
    // The whole point of the escape hatch: the resume points at the home the
    // structured run wrote into, not one this new session would have created.
    assert_eq!(
        spec.env.get("CLAUDE_CONFIG_DIR").map(String::as_str),
        Some(
            f.state
                .state_dir
                .join("sessions")
                .join(source.id.to_string())
                .canonicalize()
                .map(|path| dunce::simplified(&path).to_path_buf())
                .unwrap()
                .to_str()
                .unwrap()
        )
    );
    assert_ne!(
        spec.env.get("CLAUDE_CONFIG_DIR").map(String::as_str),
        Some(
            f.state
                .state_dir
                .join("sessions")
                .join(record.id.to_string())
                .to_str()
                .unwrap()
        ),
        "resuming in the new session's own home would find no conversation"
    );
}

#[tokio::test]
async fn opening_a_terminal_before_a_conversation_exists_is_refused() {
    let f = Fixture::new("127.0.0.1:8787".parse().unwrap(), None);
    let cwd = dunce::canonicalize(f.path.join("repo")).unwrap();
    let workspace = f
        .state
        .store
        .create_workspace("fixture", cwd.to_str().unwrap())
        .unwrap();
    let source = f
        .state
        .store
        .create_session_with_configuration(
            workspace.id,
            ProviderKind::Codex,
            "Not started",
            None,
            None,
            None,
            Default::default(),
            false,
        )
        .unwrap();
    f.state
        .store
        .set_interaction_mode(source.id, InteractionMode::Structured)
        .unwrap();
    let (status, body) = call(
        f.app(),
        "POST",
        &format!("/api/sessions/{}/terminal", source.id),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(body["error"].as_str().unwrap().contains("no conversation"));
}

/// Usage capture rides along with the launch instead of editing settings files
/// the user owns, and it must not silently replace a status line they already
/// configured.
#[tokio::test]
async fn claude_usage_capture_is_a_launch_overlay_that_preserves_an_existing_status_line() {
    let f = Fixture::new("127.0.0.1:8787".parse().unwrap(), None);
    let home = dunce::canonicalize(&f.path).unwrap().join("claude-home");
    fs::create_dir_all(&home).unwrap();
    let config = home.to_str().unwrap();

    // Off by default: a session launches exactly as the native client would.
    assert!(!native_config::usage_capture_enabled());
    assert_eq!(
        native_config::claude_statusline(&f.state, Some(config)),
        (Vec::new(), std::collections::BTreeMap::new())
    );

    // SAFETY: single-threaded assertion of this process-wide opt-in.
    unsafe { std::env::set_var("TENDLY_CLAUDE_USAGE_CAPTURE", "1") };
    let (args, environment) = native_config::claude_statusline(&f.state, Some(config));
    assert_eq!(args[0], "--settings");
    let overlay: Value = serde_json::from_str(&args[1]).unwrap();
    let command = overlay["statusLine"]["command"].as_str().unwrap();
    assert!(command.contains("statusline-capture.mjs"));
    assert_eq!(overlay["statusLine"]["type"], "command");
    // The overlay is the only key, so nothing else about the session's settings
    // is asserted or overridden by Tendly.
    assert_eq!(overlay.as_object().unwrap().len(), 1);
    // No status line of their own yet, so there is nothing to delegate to.
    assert!(!environment.contains_key("TENDLY_STATUSLINE_DELEGATE"));
    // No settings file belonging to the account is created or modified.
    assert!(!home.join("settings.json").exists());

    fs::write(
        home.join("settings.json"),
        r#"{"statusLine":{"type":"command","command":"my-own-status-line"},"model":"keep-me"}"#,
    )
    .unwrap();
    let (_, environment) = native_config::claude_statusline(&f.state, Some(config));
    assert_eq!(
        environment["TENDLY_STATUSLINE_DELEGATE"],
        "my-own-status-line"
    );
    let untouched = fs::read_to_string(home.join("settings.json")).unwrap();
    assert!(untouched.contains("my-own-status-line") && untouched.contains("keep-me"));

    // A capture command already in place must not be delegated back into itself.
    fs::write(
        home.join("settings.json"),
        format!(
            r#"{{"statusLine":{{"type":"command","command":{}}}}}"#,
            Value::from(command)
        ),
    )
    .unwrap();
    let (_, environment) = native_config::claude_statusline(&f.state, Some(config));
    assert!(!environment.contains_key("TENDLY_STATUSLINE_DELEGATE"));

    for broken in [
        "not json",
        r#"{"statusLine":{"type":"command"}}"#,
        r#"{"statusLine":{"type":"command","command":"   "}}"#,
    ] {
        fs::write(home.join("settings.json"), broken).unwrap();
        let (args, environment) = native_config::claude_statusline(&f.state, Some(config));
        assert_eq!(args[0], "--settings", "{broken}");
        assert!(
            !environment.contains_key("TENDLY_STATUSLINE_DELEGATE"),
            "{broken}"
        );
    }
    // SAFETY: restores the default for every other test in this process.
    unsafe { std::env::remove_var("TENDLY_CLAUDE_USAGE_CAPTURE") };
}

#[tokio::test]
async fn a_file_pasted_into_a_terminal_lands_in_that_sessions_directory() {
    let f = Fixture::new("127.0.0.1:8787".parse().unwrap(), None);
    let id = register(&f).await;
    let (status, session) = call(
        f.app(),
        "POST",
        &format!("/api/workspaces/{id}/sessions"),
        json!({"title":"Shell","provider":"terminal"}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let session = session["id"].as_str().unwrap();
    let (status, saved) = call(
        f.app(),
        "POST",
        &format!("/api/sessions/{session}/uploads?name=image.png"),
        json!("pixels"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{saved}");
    let absolute = saved["absolute"].as_str().unwrap();
    let repo = dunce::canonicalize(f.path.join("repo")).unwrap();
    assert!(
        std::path::Path::new(absolute).starts_with(&repo),
        "{absolute}"
    );
    assert!(
        saved["path"]
            .as_str()
            .unwrap()
            .starts_with(".tendly-files/pastes/")
    );
    let (status, _) = call(
        f.app(),
        "POST",
        &format!("/api/sessions/{}/uploads?name=a.png", uuid::Uuid::new_v4()),
        json!("x"),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "an unknown session has nowhere to put it"
    );
}
