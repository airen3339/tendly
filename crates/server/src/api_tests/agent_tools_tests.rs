//! Tendly's agent tools: who may call them, session tokens, approvals and the canvas tabs an agent manages.

use super::*;

#[tokio::test]
async fn every_client_launch_carries_tendly_tools_unless_turned_off() {
    let f = Fixture::new("127.0.0.1:8787".parse().unwrap(), None);
    let workspace = f
        .state
        .store
        .create_workspace("equip", f.path.join("repo").to_str().unwrap())
        .unwrap();
    let cwd = f.path.join("repo");
    let claude = f
        .state
        .store
        .create_session(workspace.id, ProviderKind::ClaudeCode, "claude")
        .unwrap();
    let spec = launch::build(&f.state, &claude, cwd.clone()).unwrap();
    let token = spec.env["TENDLY_AGENT_TOKEN"].clone();
    let config = spec
        .args
        .iter()
        .find_map(|arg| arg.strip_prefix("--mcp-config="))
        .expect("mcp config flag");
    assert!(spec.args.contains(&"--allowedTools=mcp__tendly".to_owned()));
    let written: Value = serde_json::from_str(&fs::read_to_string(config).unwrap()).unwrap();
    let server = &written["mcpServers"]["tendly"];
    assert_eq!(server["args"], json!(["mcp"]));
    assert_eq!(server["env"]["TENDLY_AGENT_TOKEN"], token);
    assert_eq!(server["env"]["TENDLY_SESSION_ID"], claude.id.to_string());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(config).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
    // A relaunch replaces the token; the old one names no one.
    let again = launch::build(&f.state, &claude, cwd.clone()).unwrap();
    assert_ne!(again.env["TENDLY_AGENT_TOKEN"], token);

    let codex = f
        .state
        .store
        .create_session(workspace.id, ProviderKind::Codex, "codex")
        .unwrap();
    let spec = launch::build(&f.state, &codex, cwd.clone()).unwrap();
    assert_eq!(spec.args[0], "-c");
    assert_eq!(
        spec.args[1],
        "mcp_servers.tendly.command=".to_owned()
            + &serde_json::to_string(&std::env::current_exe().unwrap().to_string_lossy()).unwrap()
    );
    let env = spec
        .args
        .iter()
        .find(|arg| arg.starts_with("mcp_servers.tendly.env="))
        .unwrap();
    assert!(env.contains(&format!(
        "TENDLY_AGENT_TOKEN={}",
        serde_json::to_string(&spec.env["TENDLY_AGENT_TOKEN"]).unwrap()
    )));
    assert!(
        spec.args
            .contains(&"mcp_servers.tendly.tool_timeout_sec=600".to_owned())
    );

    let terminal = f
        .state
        .store
        .create_session(workspace.id, ProviderKind::Terminal, "shell")
        .unwrap();
    let spec = launch::build(&f.state, &terminal, cwd.clone()).unwrap();
    assert!(spec.env.contains_key("TENDLY_AGENT_TOKEN"));
    assert_eq!(without_tendly(&spec.args), spec.args);

    let (status, _) = call(
        f.app(),
        "PUT",
        "/api/preferences",
        json!({"agent_tools": false}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (_, preferences) = call(f.app(), "GET", "/api/preferences", Value::Null).await;
    assert_eq!(preferences["agent_tools"], false);
    let spec = launch::build(&f.state, &claude, cwd).unwrap();
    assert!(!spec.env.keys().any(|key| key.starts_with("TENDLY_")));
    assert_eq!(without_tendly(&spec.args), spec.args);
}

#[tokio::test]
async fn agent_tools_know_their_caller_and_a_session_token_opens_nothing_else() {
    const DEPLOYMENT: &str = "deployment-token-for-agent-tests-0123456789";
    let f = Fixture::new("127.0.0.1:8787".parse().unwrap(), Some(DEPLOYMENT));
    let workspace = f
        .state
        .store
        .create_workspace("agents", f.path.join("repo").to_str().unwrap())
        .unwrap();
    let session = f
        .state
        .store
        .create_session(workspace.id, ProviderKind::Terminal, "worker")
        .unwrap();
    let spec = launch::build(&f.state, &session, f.path.join("repo")).unwrap();
    let token = spec.env["TENDLY_AGENT_TOKEN"].clone();
    assert_eq!(spec.env["TENDLY_SESSION_ID"], session.id.to_string());
    assert_eq!(spec.env["TENDLY_URL"], "http://127.0.0.1:8787");
    let status = json!({"name":"tendly_status"});

    // Issued but not running: the token identifies no one yet.
    let (code, _) = agent_request(
        f.app(),
        "POST",
        "/api/agent/call",
        Some(&token),
        Some("agent"),
        status.clone(),
    )
    .await;
    assert_eq!(code, StatusCode::UNAUTHORIZED);

    f.state
        .runtime
        .start(
            session.id.to_string(),
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

    let (code, tools) = agent_request(
        f.app(),
        "GET",
        "/api/agent/tools",
        Some(&token),
        None,
        Value::Null,
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    assert!(
        tools["instructions"]
            .as_str()
            .unwrap()
            .contains("tendly_status")
    );
    let names: Vec<_> = tools["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"tendly_status") && names.contains(&"tendly_list"));

    let (code, result) = agent_request(
        f.app(),
        "POST",
        "/api/agent/call",
        Some(&token),
        Some("agent"),
        status.clone(),
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(result["isError"], false);
    let you = &tool_text(&result)["you"];
    assert_eq!(you["kind"], "session");
    assert_eq!(you["session"]["id"], session.id.to_string());
    assert_eq!(you["workspace"]["name"], "agents");

    // The session's token reaches the agent tools and nothing else.
    for (method, path) in [
        ("GET", "/api/sessions"),
        ("GET", "/api/secrets"),
        ("GET", "/api/endpoint-profiles"),
    ] {
        let (code, _) = agent_request(
            f.app(),
            method,
            path,
            Some(&token),
            Some("web"),
            Value::Null,
        )
        .await;
        assert_eq!(code, StatusCode::UNAUTHORIZED, "{path}");
    }
    // A mutation still has to say it comes from an agent (or the web app).
    let (code, _) = agent_request(
        f.app(),
        "POST",
        "/api/agent/call",
        Some(&token),
        None,
        status.clone(),
    )
    .await;
    assert_eq!(code, StatusCode::FORBIDDEN);
    // `agent` is no password for the rest of the API.
    let (code, _) = agent_request(
        f.app(),
        "POST",
        "/api/workspaces",
        Some(DEPLOYMENT),
        Some("agent"),
        json!({"name":"x","root_path":"/"}),
    )
    .await;
    assert_eq!(code, StatusCode::FORBIDDEN);

    // The deployment's own token is an external caller.
    let (_, result) = agent_request(
        f.app(),
        "POST",
        "/api/agent/call",
        Some(DEPLOYMENT),
        Some("agent"),
        status.clone(),
    )
    .await;
    assert_eq!(tool_text(&result)["you"]["kind"], "external");
    let (_, unknown) = agent_request(
        f.app(),
        "POST",
        "/api/agent/call",
        Some(DEPLOYMENT),
        Some("agent"),
        json!({"name":"tendly_nothing"}),
    )
    .await;
    assert_eq!(unknown["isError"], true);

    // Listing endpoints says how a key is set, never what it is.
    crate::secrets::store(
        &f.state.state_dir,
        "TENDLY_SECRET_AGENT_LIST",
        "sk-never-listed",
    )
    .unwrap();
    let (_, profile) = agent_request(f.app(), "POST", "/api/endpoint-profiles", Some(DEPLOYMENT), Some("web"), json!({"name":"listed","provider":"codex","endpoint_url":"https://example.test/v1","secret_ref":"env:TENDLY_SECRET_AGENT_LIST"})).await;
    assert_eq!(profile.get("error"), None, "{profile}");
    let (_, listed) = agent_request(
        f.app(),
        "POST",
        "/api/agent/call",
        Some(&token),
        Some("agent"),
        json!({"name":"tendly_list","arguments":{"what":"endpoints"}}),
    )
    .await;
    assert!(!listed.to_string().contains("sk-never-listed"));
    assert_eq!(tool_text(&listed)[0]["key"], "saved");
    let (_, sessions) = agent_request(
        f.app(),
        "POST",
        "/api/agent/call",
        Some(&token),
        Some("agent"),
        json!({"name":"tendly_list","arguments":{"what":"sessions"}}),
    )
    .await;
    assert_eq!(tool_text(&sessions)[0]["title"], "worker");

    // Once the process ends, so does the token.
    f.state.runtime.stop(&session.id.to_string()).await.unwrap();
    for _ in 0..50 {
        if !f
            .state
            .runtime
            .get(&session.id.to_string())
            .is_some_and(|r| r.running())
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let (code, _) = agent_request(
        f.app(),
        "POST",
        "/api/agent/call",
        Some(&token),
        Some("agent"),
        status,
    )
    .await;
    assert_eq!(code, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn an_agent_endpoint_waits_for_the_person_and_its_key_never_passes_through_the_agent() {
    let f = Fixture::new("127.0.0.1:8787".parse().unwrap(), None);
    let create = json!({"name":"tendly_endpoint","arguments":{"action":"create","name":"Team relay","provider":"codex","endpoint_url":"https://relay.example.test/v1","model":"gpt-test"}});

    // No window open: nobody can answer, so the agent is told at once.
    let (_, result) = agent_request(
        f.app(),
        "POST",
        "/api/agent/call",
        None,
        Some("agent"),
        create.clone(),
    )
    .await;
    assert_eq!(result["isError"], true);
    assert!(
        result["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("No Tendly window")
    );

    let mut events = f.state.activity.subscribe();
    let app = f.app();
    let pending = tokio::spawn({
        let create = create.clone();
        async move { agent_request(app, "POST", "/api/agent/call", None, Some("agent"), create).await }
    });
    let request = next_event(&mut events, "request").await;
    assert_eq!(request["title"], "Create endpoint profile “{name}”");
    assert_eq!(request["values"]["name"], "Team relay");
    assert_eq!(request["key"]["optional"], false);
    assert!(
        request["rows"].as_array().unwrap().iter().any(
            |row| row["label"] == "Endpoint" && row["value"] == "https://relay.example.test/v1"
        )
    );
    let id = request["id"].as_str().unwrap().to_owned();
    // A key it asked for is required to approve.
    let (status, _) = call(
        f.app(),
        "POST",
        &format!("/api/agent-activity/requests/{id}"),
        json!({"approve":true}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = call(
        f.app(),
        "POST",
        &format!("/api/agent-activity/requests/{id}"),
        json!({"approve":true,"secret":"sk-typed-in-tendly"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (_, result) = pending.await.unwrap();
    assert_eq!(result["isError"], false, "{result}");
    assert!(!result.to_string().contains("sk-typed-in-tendly"));
    let created = &tool_text(&result)["created"];
    assert_eq!(created["key"], "saved");
    assert_eq!(created["model"], "gpt-test");
    assert_eq!(
        crate::secrets::resolve(&f.state.state_dir, "TENDLY_SECRET_TEAM_RELAY").as_deref(),
        Some("sk-typed-in-tendly")
    );
    let notice = next_event(&mut events, "notice").await;
    assert_eq!(notice["message"], "Created endpoint profile “{name}”");
    assert_eq!(notice["undoable"], true);
    assert_eq!(notice["who"]["kind"], "external");

    // Undo takes the profile and its key away again.
    let (status, _) = call(
        f.app(),
        "POST",
        &format!(
            "/api/agent-activity/notices/{}/undo",
            notice["id"].as_str().unwrap()
        ),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (_, profiles) = call(f.app(), "GET", "/api/endpoint-profiles", Value::Null).await;
    assert_eq!(profiles, json!([]));
    assert!(crate::secrets::resolve(&f.state.state_dir, "TENDLY_SECRET_TEAM_RELAY").is_none());

    // Declining changes nothing, and says so.
    let app = f.app();
    let call_again = tokio::spawn({
        let create = create.clone();
        async move { agent_request(app, "POST", "/api/agent/call", None, Some("agent"), create).await }
    });
    let request = next_event(&mut events, "request").await;
    call(
        f.app(),
        "POST",
        &format!(
            "/api/agent-activity/requests/{}",
            request["id"].as_str().unwrap()
        ),
        json!({"approve":false}),
    )
    .await;
    let (_, result) = call_again.await.unwrap();
    assert_eq!(result["isError"], true);
    assert!(
        result["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("declined")
    );
    let (_, profiles) = call(f.app(), "GET", "/api/endpoint-profiles", Value::Null).await;
    assert_eq!(profiles, json!([]));
}

#[tokio::test]
async fn defaults_change_at_once_with_an_undo_and_only_running_unasked_is_confirmed() {
    let f = Fixture::new("127.0.0.1:8787".parse().unwrap(), None);
    let mut events = f.state.activity.subscribe();
    let (_, result) = agent_request(f.app(), "POST", "/api/agent/call", None, Some("agent"), json!({"name":"tendly_preferences","arguments":{"action":"set","provider":"claude_code","effort":"high"}})).await;
    assert_eq!(result["isError"], false, "{result}");
    assert_eq!(tool_text(&result)["saved"]["claude_code"]["effort"], "high");
    let notice = next_event(&mut events, "notice").await;
    call(
        f.app(),
        "POST",
        &format!(
            "/api/agent-activity/notices/{}/undo",
            notice["id"].as_str().unwrap()
        ),
        Value::Null,
    )
    .await;
    let (_, preferences) = call(f.app(), "GET", "/api/preferences", Value::Null).await;
    assert_eq!(preferences["claude_code"].get("effort"), None);

    let app = f.app();
    let danger = tokio::spawn(async move {
        agent_request(app, "POST", "/api/agent/call", None, Some("agent"), json!({"name":"tendly_preferences","arguments":{"action":"set","provider":"codex","permission":"danger"}})).await
    });
    let request = next_event(&mut events, "request").await;
    assert_eq!(request["values"]["client"], "Codex");
    call(
        f.app(),
        "POST",
        &format!(
            "/api/agent-activity/requests/{}",
            request["id"].as_str().unwrap()
        ),
        json!({"approve":true}),
    )
    .await;
    assert_eq!(danger.await.unwrap().1["isError"], false);

    // The appearance needs no client, and following is stored as nothing.
    let (_, themed) = agent_request(f.app(), "POST", "/api/agent/call", None, Some("agent"), json!({"name":"tendly_preferences","arguments":{"action":"set","terminal_appearance":"dark","appearance":"system"}})).await;
    assert_eq!(themed["isError"], false, "{themed}");
    assert_eq!(tool_text(&themed)["saved"]["terminal_appearance"], "dark");
    assert_eq!(tool_text(&themed)["saved"]["appearance"], "system");
    let (_, preferences) = call(f.app(), "GET", "/api/preferences", Value::Null).await;
    assert_eq!(preferences["terminal_appearance"], "dark");
    assert_eq!(preferences.get("appearance"), None);
    let (_, wrong) = agent_request(f.app(), "POST", "/api/agent/call", None, Some("agent"), json!({"name":"tendly_preferences","arguments":{"action":"set","terminal_appearance":"sepia"}})).await;
    assert_eq!(wrong["isError"], true);

    let (_, added) = agent_request(
        f.app(),
        "POST",
        "/api/agent/call",
        None,
        Some("agent"),
        json!({"name":"tendly_workspace","arguments":{"action":"add","path":f.path.join("repo")}}),
    )
    .await;
    assert_eq!(tool_text(&added)["added"]["name"], "repo");
}

#[tokio::test]
async fn a_session_token_can_neither_see_nor_answer_the_persons_questions() {
    const DEPLOYMENT: &str = "deployment-token-for-activity-tests-012345";
    let f = Fixture::new("127.0.0.1:8787".parse().unwrap(), Some(DEPLOYMENT));
    let workspace = f
        .state
        .store
        .create_workspace("q", f.path.join("repo").to_str().unwrap())
        .unwrap();
    let session = f
        .state
        .store
        .create_session(workspace.id, ProviderKind::Terminal, "asker")
        .unwrap();
    let token = launch::build(&f.state, &session, f.path.join("repo"))
        .unwrap()
        .env["TENDLY_AGENT_TOKEN"]
        .clone();
    f.state
        .runtime
        .start(
            session.id.to_string(),
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
    let mut events = f.state.activity.subscribe();
    let app = f.app();
    let asking = tokio::spawn({
        let token = token.clone();
        async move {
            agent_request(app, "POST", "/api/agent/call", Some(&token), Some("agent"), json!({"name":"tendly_endpoint","arguments":{"action":"create","name":"x","provider":"codex","key":"none"}})).await
        }
    });
    let request = next_event(&mut events, "request").await;
    assert_eq!(request["who"]["kind"], "session");
    assert_eq!(request["who"]["title"], "asker");
    let id = request["id"].as_str().unwrap();
    for client in ["agent", "web"] {
        let (status, _) = agent_request(
            f.app(),
            "POST",
            &format!("/api/agent-activity/requests/{id}"),
            Some(&token),
            Some(client),
            json!({"approve":true}),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{client}");
    }
    let (status, _) = agent_request(
        f.app(),
        "GET",
        "/api/agent-activity/ws",
        Some(&token),
        None,
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    // The person can.
    let (status, _) = agent_request(
        f.app(),
        "POST",
        &format!("/api/agent-activity/requests/{id}"),
        Some(DEPLOYMENT),
        Some("web"),
        json!({"approve":true}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(asking.await.unwrap().1["isError"], false);
    f.state.runtime.stop(&session.id.to_string()).await.unwrap();
}

#[tokio::test]
async fn an_agent_lists_and_closes_canvas_tabs_and_asks_before_discarding_a_working_one() {
    let f = Fixture::new("127.0.0.1:8787".parse().unwrap(), None);
    let workspace = f
        .state
        .store
        .create_workspace("tabs", f.path.join("repo").to_str().unwrap())
        .unwrap();
    let kept = f
        .state
        .store
        .create_session(workspace.id, ProviderKind::Terminal, "kept")
        .unwrap();
    let temporary = f
        .state
        .store
        .create_session_with_options(
            workspace.id,
            ProviderKind::Terminal,
            "scratch",
            None,
            None,
            None,
            Default::default(),
            true,
            None,
            None,
        )
        .unwrap();
    let pane = |id: &str, session: &Session| json!({"type":"pane","id":id,"kind":"terminal","title":session.title,"metadata":{"session_id":session.id,"workspace_id":workspace.id}});
    let layout = json!({"version":1,"root":{"type":"split","id":"root","direction":"horizontal","ratio":0.5,
        "first":{"type":"stack","kind":"stack","id":"left","activePaneId":"one","panes":[pane("one", &kept), pane("two", &temporary)]},
        "second":{"type":"pane","id":"changes","kind":"git_diff","title":"Changes","metadata":{"workspace_id":workspace.id}}}});
    let (status, saved) = call(
        f.app(),
        "PUT",
        "/api/canvas/layout",
        json!({"layout":layout,"expected_revision":0}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{saved}");
    f.state
        .runtime
        .start(
            temporary.id.to_string(),
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

    let canvas = |arguments: Value| {
        agent_request(
            f.app(),
            "POST",
            "/api/agent/call",
            None,
            Some("agent"),
            json!({"name":"tendly_canvas","arguments":arguments}),
        )
    };
    let listed = tool_text(&canvas(json!({"action":"list"})).await.1);
    let tabs = listed["tabs"].as_array().unwrap();
    assert_eq!(tabs.len(), 3);
    assert_eq!(tabs[1]["temporary"], true);
    assert_eq!(tabs[1]["running"], true);

    let mut events = f.state.activity.subscribe();
    // A plain tab closes at once; the session behind it is untouched.
    let closed = canvas(json!({"action":"close","pane_ids":["one"]})).await.1;
    assert_eq!(tool_text(&closed)["closed"], 1, "{closed}");
    assert_eq!(next_event(&mut events, "canvas").await["revision"], 2);
    let notice = next_event(&mut events, "notice").await;
    assert!(f.state.store.get_session(kept.id).unwrap().is_some());
    let (_, current) = call(f.app(), "GET", "/api/canvas/layout", Value::Null).await;
    assert_eq!(current["layout"]["root"]["first"]["activePaneId"], "two");
    // Undo puts it back.
    call(
        f.app(),
        "POST",
        &format!(
            "/api/agent-activity/notices/{}/undo",
            notice["id"].as_str().unwrap()
        ),
        Value::Null,
    )
    .await;
    let (_, current) = call(f.app(), "GET", "/api/canvas/layout", Value::Null).await;
    assert_eq!(
        current["layout"]["root"]["first"]["panes"]
            .as_array()
            .unwrap()
            .len(),
        2
    );

    let unknown = canvas(json!({"action":"close","pane_ids":["nope"]}))
        .await
        .1;
    assert_eq!(unknown["isError"], true);

    // Closing everything takes a working temporary session with it: asked first.
    let app = f.app();
    let all = tokio::spawn(async move {
        agent_request(
            app,
            "POST",
            "/api/agent/call",
            None,
            Some("agent"),
            json!({"name":"tendly_canvas","arguments":{"action":"close","all":true}}),
        )
        .await
        .1
    });
    let request = next_event(&mut events, "request").await;
    assert_eq!(request["values"]["count"], 3);
    assert_eq!(request["rows"][0]["value"], "scratch");
    call(
        f.app(),
        "POST",
        &format!(
            "/api/agent-activity/requests/{}",
            request["id"].as_str().unwrap()
        ),
        json!({"approve":true}),
    )
    .await;
    let result = tool_text(&all.await.unwrap());
    assert_eq!(result["discarded_temporary_sessions"], 1);
    assert!(f.state.store.get_session(temporary.id).unwrap().is_none());
    let (_, current) = call(f.app(), "GET", "/api/canvas/layout", Value::Null).await;
    assert_eq!(current["layout"]["root"]["panes"], json!([]));
}

#[tokio::test]
async fn a_session_can_have_the_agent_tools_on_or_off_whatever_the_preference() {
    const DEPLOYMENT: &str = "deployment-token-for-session-switch-012345";
    let f = Fixture::new("127.0.0.1:8787".parse().unwrap(), Some(DEPLOYMENT));
    let workspace = f
        .state
        .store
        .create_workspace("switch", f.path.join("repo").to_str().unwrap())
        .unwrap();
    let claude = f
        .state
        .store
        .create_session(workspace.id, ProviderKind::ClaudeCode, "claude")
        .unwrap();
    let cwd = f.path.join("repo");
    let equipped = |spec: &tendly_runtime::SpawnSpec| {
        spec.args.iter().any(|arg| arg.starts_with("--mcp-config="))
    };
    let web = |method: &'static str, path: String, body: Value| {
        agent_request(
            f.app(),
            method,
            path.leak(),
            Some(DEPLOYMENT),
            Some("web"),
            body,
        )
    };
    let path = format!("/api/sessions/{}/agent-tools", claude.id);

    let (status, view) = web("GET", path.clone(), Value::Null).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(view, json!({"choice":null,"default":true,"enabled":true}));

    // Off for this session, though the preference is on.
    let (_, view) = web("PUT", path.clone(), json!({"choice":false})).await;
    assert_eq!(view["enabled"], false);
    let spec = launch::build(&f.state, &claude, cwd.clone()).unwrap();
    assert!(!equipped(&spec) && !spec.env.contains_key("TENDLY_AGENT_TOKEN"));

    // On for this session, though the preference is off.
    web(
        "PUT",
        "/api/preferences".into(),
        json!({"claude_code":{},"codex":{},"agent_tools":false}),
    )
    .await;
    let (_, view) = web("PUT", path.clone(), json!({"choice":true})).await;
    assert_eq!(view, json!({"choice":true,"default":false,"enabled":true}));
    assert!(equipped(
        &launch::build(&f.state, &claude, cwd.clone()).unwrap()
    ));

    // Back to following the preference.
    let (_, view) = web("PUT", path.clone(), json!({"choice":null})).await;
    assert_eq!(view["enabled"], false);
    assert!(!equipped(
        &launch::build(&f.state, &claude, cwd.clone()).unwrap()
    ));

    let (status, _) = web(
        "PUT",
        format!("/api/sessions/{}/agent-tools", Uuid::new_v4()),
        json!({"choice":true}),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // A session cannot switch its own tools: its token opens /api/agent/ only.
    web("PUT", path.clone(), json!({"choice":true})).await;
    let token =
        launch::build(&f.state, &claude, cwd.clone()).unwrap().env["TENDLY_AGENT_TOKEN"].clone();
    f.state
        .runtime
        .start(
            claude.id.to_string(),
            tendly_runtime::SpawnSpec {
                program: waiting_process(&["-i"]).0,
                args: waiting_process(&["-i"]).1,
                cwd: cwd.clone(),
                env: BTreeMap::new(),
                env_remove: Vec::new(),
            },
        )
        .await
        .unwrap();
    let (status, _) = agent_request(
        f.app(),
        "PUT",
        &path,
        Some(&token),
        Some("web"),
        json!({"choice":false}),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    f.state.runtime.stop(&claude.id.to_string()).await.unwrap();
}

#[tokio::test]
async fn a_pending_approval_marks_the_session_waiting_until_the_last_is_resolved() {
    let f = Fixture::new("127.0.0.1:8787".parse().unwrap(), None);
    let workspace = f
        .state
        .store
        .create_workspace("approvals", f.path.join("repo").to_str().unwrap())
        .unwrap();
    let session = f
        .state
        .store
        .create_session(workspace.id, ProviderKind::ClaudeCode, "asks")
        .unwrap();
    let runtime = conversations::ChatRuntime::for_test();
    let status = || {
        f.state
            .store
            .get_session(session.id)
            .unwrap()
            .unwrap()
            .status
    };
    let approval = |id: &str| json!({"type":"approval","id":id,"title":"Run","text":"Fixture","choices":["accept","decline"]});
    let resolved = |id: &str| json!({"type":"approval_resolved","id":id});
    for (event, expected) in [
        (approval("a"), SessionStatus::Waiting),
        (approval("b"), SessionStatus::Waiting),
        (resolved("a"), SessionStatus::Waiting),
        (resolved("b"), SessionStatus::Running),
        (resolved("b"), SessionStatus::Running),
    ] {
        conversations::handle_event_for_test(&f.state, session.id, &runtime, event.clone())
            .await
            .unwrap();
        assert_eq!(
            format!("{:?}", status()),
            format!("{expected:?}"),
            "after {event}"
        );
    }
}
