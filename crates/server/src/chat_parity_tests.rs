//! The clients run in this process against the Node bridge they replace.
//!
//! Each scenario runs twice against the same synthetic client
//! (`packages/native-bridge/fixtures/chat-cli.mjs` for Codex, `pi-cli.mjs` for
//! Pi): once through `node chat.mjs` (BridgeAdapter) and once through the
//! client's adapter here. Both must produce
//! the same events, after the server's own validation, and send Codex the same
//! messages. Approval ids are random, so they are compared by order of
//! appearance; the scenario's directory is compared as a placeholder.
use crate::chat_adapter::{BridgeAdapter, ChatCommand, ChatInit, ClientAdapter, dispatch};
use serde_json::{Value, json};
use std::{collections::BTreeMap, path::PathBuf, process::Stdio, time::Duration};

/// One thing a scenario does.
enum Step {
    Send(Value),
    /// Wait until an event matches.
    Wait(fn(&Value) -> bool),
    /// Wait until this many events match.
    WaitCount(fn(&Value) -> bool, usize),
    /// Answer the latest approval: the decision, and answers built from it.
    Answer(&'static str, fn(&Value) -> Value),
    Sleep(u64),
}
use Step::*;

fn bridge_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../packages/native-bridge")
}

/// What the fixture app-server needs in its environment, and a secret the
/// output must never show.
fn environment(log: &std::path::Path) -> BTreeMap<String, String> {
    BTreeMap::from([
        (
            "AGENTDOCK_CHAT_TEST_LOG".to_owned(),
            log.to_string_lossy().into_owned(),
        ),
        (
            "AGENTDOCK_CHAT_TEST_INHERITED".to_owned(),
            "kept".to_owned(),
        ),
        (
            "AGENTDOCK_SECRET_TEST_CREDENTIAL".to_owned(),
            "fixture-secret-value".to_owned(),
        ),
    ])
}

struct Run {
    events: Vec<Value>,
    native: Vec<Value>,
}

async fn drive<A: ClientAdapter>(
    mut adapter: A,
    init: ChatInit,
    steps: &[Step],
    log: &std::path::Path,
) -> Run {
    let mut events = Vec::new();
    adapter.start(&init).await.expect("start");
    let next = async |adapter: &mut A, events: &mut Vec<Value>, wait: Duration| -> bool {
        match tokio::time::timeout(wait, adapter.next_event()).await {
            Ok(Some(Ok(event))) => {
                if let Some(event) = crate::conversations::normalize_event(event) {
                    events.push(event);
                }
                true
            }
            _ => false,
        }
    };
    // Ready, and the models app-server lists, before anything is asked of it.
    while !events
        .iter()
        .any(|e: &Value| e["type"] == "settings" && e.get("models").is_some())
    {
        assert!(
            next(&mut adapter, &mut events, Duration::from_secs(5)).await,
            "no models announced: {events:?}"
        );
    }
    for step in steps {
        match step {
            Send(command) => dispatch(&mut adapter, ChatCommand::parse(command).expect("command"))
                .await
                .expect("sent"),
            Answer(decision, answers) => {
                let approval = events
                    .iter()
                    .rev()
                    .find(|e| e["type"] == "approval")
                    .expect("an approval")
                    .clone();
                let command = json!({"type":"approval","request_id":approval["id"],"decision":decision,"answers":answers(&approval)});
                dispatch(&mut adapter, ChatCommand::parse(&command).expect("answer"))
                    .await
                    .expect("sent");
            }
            Wait(predicate) => {
                while !events.iter().any(predicate) {
                    assert!(
                        next(&mut adapter, &mut events, Duration::from_secs(5)).await,
                        "timed out: {events:?}"
                    );
                }
            }
            WaitCount(predicate, count) => {
                while events.iter().filter(|e| predicate(e)).count() < *count {
                    assert!(
                        next(&mut adapter, &mut events, Duration::from_secs(5)).await,
                        "timed out: {events:?}"
                    );
                }
            }
            Sleep(ms) => {
                let until = tokio::time::Instant::now() + Duration::from_millis(*ms);
                while tokio::time::Instant::now() < until {
                    next(
                        &mut adapter,
                        &mut events,
                        until - tokio::time::Instant::now(),
                    )
                    .await;
                }
            }
        }
    }
    // Whatever follows the last step, so a late event is compared too.
    while next(&mut adapter, &mut events, Duration::from_millis(400)).await {}
    adapter.shutdown().await;
    let native = std::fs::read_to_string(log)
        .unwrap_or_default()
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).expect("log line"))
        .collect();
    Run { events, native }
}

/// Approval ids by order of appearance, and the run's directory as a placeholder.
fn comparable(run: Run, dir: &str) -> (Vec<Value>, Vec<Value>) {
    let mut ids: Vec<String> = Vec::new();
    let mut seen = |id: &Value| -> Value {
        let id = id.as_str().unwrap_or_default().to_owned();
        let index = ids
            .iter()
            .position(|known| *known == id)
            .unwrap_or_else(|| {
                ids.push(id);
                ids.len() - 1
            });
        json!(format!("approval#{index}"))
    };
    fn placeholder(value: Value, dir: &str) -> Value {
        match value {
            Value::String(text) => Value::String(untimed(&text.replace(dir, "<dir>"))),
            Value::Array(items) => {
                Value::Array(items.into_iter().map(|v| placeholder(v, dir)).collect())
            }
            Value::Object(object) => Value::Object(
                object
                    .into_iter()
                    .map(|(k, v)| (k, placeholder(v, dir)))
                    .collect(),
            ),
            other => other,
        }
    }
    let events = run
        .events
        .into_iter()
        .map(|mut event| {
            if matches!(
                event["type"].as_str(),
                Some("approval" | "approval_resolved")
            ) {
                event["id"] = seen(&event["id"]);
            }
            placeholder(event, dir)
        })
        .collect();
    let native = run
        .native
        .into_iter()
        .map(|v| placeholder(v, dir))
        .collect();
    (events, native)
}

/// A client under comparison: its provider id, its synthetic stand-in and
/// the adapter that runs it here.
struct Client {
    provider: &'static str,
    fixture: &'static str,
    adapter: fn(&BTreeMap<String, String>, &[String]) -> crate::native_chat::NativeAdapter,
}
const CODEX: Client = Client {
    provider: "codex",
    fixture: "chat-cli.mjs",
    adapter: crate::codex_chat::adapter,
};
const PI: Client = Client {
    provider: "pi",
    fixture: "pi-cli.mjs",
    adapter: crate::pi_chat::adapter,
};

/// A turn Pi starts by itself is named for the moment it began
/// (`pi-<ms>-<n>`); only the count is compared.
fn untimed(text: &str) -> String {
    let Some(rest) = text.strip_prefix("pi-") else {
        return text.to_owned();
    };
    match rest.split_once('-') {
        Some((millis, count))
            if millis.len() >= 10 && millis.bytes().all(|b| b.is_ascii_digit()) =>
        {
            format!("pi-<t>-{count}")
        }
        _ => text.to_owned(),
    }
}

/// A fresh directory with the client's fixture in it.
fn scenario_dir(name: &str) -> PathBuf {
    scenario_dir_for(&CODEX, name)
}
fn scenario_dir_for(client: &Client, name: &str) -> PathBuf {
    let dir =
        std::env::temp_dir().join(format!("agentdock-parity-{name}-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let program = dir.join(client.fixture);
    std::fs::copy(bridge_dir().join("fixtures").join(client.fixture), &program).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    dir
}

fn init(dir: &std::path::Path, args: &[&str]) -> ChatInit {
    init_for(&CODEX, dir, args)
}
fn init_for(client: &Client, dir: &std::path::Path, args: &[&str]) -> ChatInit {
    ChatInit {
        provider: client.provider.into(),
        program: dir.join(client.fixture).to_string_lossy().into_owned(),
        args: args.iter().map(|a| a.to_string()).collect(),
        cwd: dir.to_string_lossy().into_owned(),
        resume_id: None,
    }
}

async fn both(name: &str, args: &[&str], steps: &[Step]) -> Vec<Value> {
    both_for(&CODEX, name, args, steps).await
}
/// Runs the scenario through the bridge and through the Rust adapter, asserts
/// they match, and returns the events, so a scenario can also check that what
/// both did is right -- they can agree on a bug.
async fn both_for(client: &Client, name: &str, args: &[&str], steps: &[Step]) -> Vec<Value> {
    let bridge = {
        let dir = scenario_dir_for(client, &format!("{name}-bridge"));
        let log = dir.join("native.jsonl");
        let mut command = tokio::process::Command::new(crate::bridge::js_runtime());
        command
            .arg(bridge_dir().join("chat.mjs"))
            .current_dir(&dir)
            .envs(environment(&log))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        agentdock_runtime::process::own_group(command.as_std_mut());
        let adapter = BridgeAdapter::new(command.spawn().expect("node")).unwrap();
        let run = drive(adapter, init_for(client, &dir, args), steps, &log).await;
        let result = comparable(run, &dir.to_string_lossy());
        let _ = std::fs::remove_dir_all(&dir);
        result
    };
    let rust = {
        let dir = scenario_dir_for(client, &format!("{name}-rust"));
        let log = dir.join("native.jsonl");
        let adapter = (client.adapter)(&environment(&log), &[]);
        let run = drive(adapter, init_for(client, &dir, args), steps, &log).await;
        let result = comparable(run, &dir.to_string_lossy());
        let _ = std::fs::remove_dir_all(&dir);
        result
    };
    let secret = |events: &[Value]| {
        events
            .iter()
            .any(|e| e.to_string().contains("fixture-secret-value"))
    };
    assert!(
        !secret(&bridge.0) && !secret(&rust.0),
        "{name}: a secret reached the output"
    );
    assert_eq!(
        rust.0, bridge.0,
        "{name}: events differ (left: rust, right: bridge)"
    );
    assert_eq!(
        rust.1, bridge.1,
        "{name}: what Codex received differs (left: rust, right: bridge)"
    );
    assert!(!bridge.0.is_empty());
    rust.0
}

fn turn(event: &Value, id: &str, status: &str) -> bool {
    event["type"] == "turn" && event["id"] == id && event["status"] == status
}
fn none(_: &Value) -> Value {
    json!({})
}
fn brief(approval: &Value) -> Value {
    json!({ approval["questions"][0]["id"].as_str().unwrap(): ["Brief"] })
}

#[tokio::test]
async fn a_simple_turn() {
    both(
        "simple",
        &[],
        &[
            Send(json!({"type":"message","id":"one","content":"simple"})),
            Wait(|e| turn(e, "one", "completed")),
        ],
    )
    .await;
}

#[tokio::test]
async fn declining_an_approval() {
    both(
        "decline",
        &[],
        &[
            Send(json!({"type":"message","id":"approve","content":"approve"})),
            Wait(|e| e["type"] == "approval"),
            Answer("decline", none),
            Wait(|e| turn(e, "approve", "completed")),
        ],
    )
    .await;
}

#[tokio::test]
async fn accepting_then_cancelling() {
    both(
        "accept-cancel",
        &[],
        &[
            Send(json!({"type":"message","id":"accept","content":"approve"})),
            Wait(|e| e["type"] == "approval"),
            Answer("accept", none),
            Wait(|e| turn(e, "accept", "completed")),
            Send(json!({"type":"message","id":"cancel","content":"approve"})),
            WaitCount(|e| e["type"] == "approval", 2),
            Answer("cancel", none),
            Wait(|e| turn(e, "cancel", "interrupted")),
        ],
    )
    .await;
}

#[tokio::test]
async fn questions_and_their_answers() {
    both(
        "ask",
        &[],
        &[
            Send(json!({"type":"message","id":"ask","content":"ask"})),
            Wait(|e| e["type"] == "approval"),
            Answer("accept", none),
            Wait(|e| e["type"] == "error"),
            Answer("accept", brief),
            Wait(|e| turn(e, "ask", "completed")),
        ],
    )
    .await;
}

#[tokio::test]
async fn permissions_on_a_resumed_thread() {
    both(
        "permissions",
        &["resume", "saved-thread"],
        &[
            Send(json!({"type":"message","id":"permissions","content":"permissions"})),
            Wait(|e| e["type"] == "approval"),
            Answer("decline", none),
            Wait(|e| turn(e, "permissions", "completed")),
        ],
    )
    .await;
}

#[tokio::test]
async fn interrupting_a_turn_and_a_repeated_message() {
    both(
        "interrupt",
        &[],
        &[
            Send(json!({"type":"message","id":"hold","content":"hold"})),
            Wait(|e| e["type"] == "tool" && e["id"] == "hold-1"),
            Send(json!({"type":"message","id":"hold","content":"hold"})),
            Send(json!({"type":"interrupt"})),
            Wait(|e| turn(e, "hold", "interrupted")),
            Send(json!({"type":"message","id":"after","content":"simple"})),
            Wait(|e| turn(e, "after", "completed")),
        ],
    )
    .await;
}

#[tokio::test]
async fn an_unsupported_request_fails_closed() {
    both(
        "unknown",
        &[],
        &[
            Send(json!({"type":"message","id":"unknown","content":"unknown"})),
            Wait(|e| e["type"] == "turn" && e["status"] == "failed"),
        ],
    )
    .await;
}

#[tokio::test]
async fn leaving_danger_restores_the_sandbox() {
    both(
        "sandbox",
        &[],
        &[
            Send(json!({"type":"permission","mode":"danger"})),
            Wait(|e| e["type"] == "settings" && e["permission_mode"] == "danger"),
            Send(json!({"type":"message","id":"free","content":"simple"})),
            Wait(|e| turn(e, "free", "completed")),
            Send(json!({"type":"permission","mode":"ask"})),
            Wait(|e| e["type"] == "settings" && e["permission_mode"] == "ask"),
            Send(json!({"type":"message","id":"guarded","content":"simple"})),
            Wait(|e| turn(e, "guarded", "completed")),
            // A mode Codex has no equivalent of is refused in the bridge's words.
            Send(json!({"type":"permission","mode":"plan"})),
            Wait(|e| e["type"] == "error"),
        ],
    )
    .await;
}

#[tokio::test]
async fn file_changes_and_mcp_tools() {
    both(
        "tools",
        &[],
        &[
            Send(json!({"type":"message","id":"tools","content":"tools"})),
            Wait(|e| turn(e, "tools", "completed")),
        ],
    )
    .await;
}

#[tokio::test]
async fn choosing_a_model_and_a_depth() {
    both(
        "model",
        &[],
        &[
            Send(json!({"type":"message","id":"before","content":"simple"})),
            Wait(|e| turn(e, "before", "completed")),
            Send(json!({"type":"model","model":"gpt-plain","effort":"high"})),
            Wait(|e| e["type"] == "settings" && e["model"] == "gpt-plain"),
            Send(json!({"type":"message","id":"after","content":"simple"})),
            Wait(|e| turn(e, "after", "completed")),
            Send(json!({"type":"message","id":"hold","content":"hold"})),
            Wait(|e| e["type"] == "tool" && e["id"] == "hold-3"),
            Send(json!({"type":"model","model":"gpt-fixture"})),
            Wait(|e| e["type"] == "error"),
            Send(json!({"type":"interrupt"})),
            Wait(|e| turn(e, "hold", "interrupted")),
        ],
    )
    .await;
}

#[tokio::test]
async fn steering_the_running_turn() {
    both(
        "steer",
        &[],
        &[
            Send(json!({"type":"message","id":"steer-a","content":"hold"})),
            Wait(|e| turn(e, "steer-a", "running")),
            Sleep(100),
            Send(json!({"type":"steer","id":"steer-b","content":"also this"})),
            Wait(|e| turn(e, "steer-a", "completed")),
        ],
    )
    .await;
}

#[tokio::test]
async fn a_turn_that_refuses_steering_passes_it_on() {
    both(
        "steer-refused",
        &[],
        &[
            Send(json!({"type":"message","id":"steer-a","content":"review"})),
            Wait(|e| turn(e, "steer-a", "running")),
            Sleep(100),
            Send(json!({"type":"steer","id":"steer-b","content":"afterwards"})),
            Wait(|e| turn(e, "steer-b", "completed")),
        ],
    )
    .await;
}

#[tokio::test]
async fn steering_with_no_turn_running_is_a_message() {
    both(
        "steer-idle",
        &[],
        &[
            Send(json!({"type":"steer","id":"lone","content":"simple"})),
            Wait(|e| turn(e, "lone", "completed")),
        ],
    )
    .await;
}

/// Everything an adapter says when its client cannot even be launched.
async fn unlaunchable<A: ClientAdapter>(mut adapter: A, init: ChatInit) -> Vec<Value> {
    adapter.start(&init).await.expect("start");
    let mut events = Vec::new();
    while let Ok(Some(Ok(event))) =
        tokio::time::timeout(Duration::from_secs(5), adapter.next_event()).await
    {
        let done = event["type"] == "exit";
        events.extend(crate::conversations::normalize_event(event));
        if done {
            break;
        }
    }
    adapter.shutdown().await;
    events
}

#[tokio::test]
async fn a_client_that_cannot_start() {
    let missing = |dir: &std::path::Path| {
        let mut init = init(dir, &[]);
        init.program = dir.join("no-such-program").to_string_lossy().into_owned();
        init
    };
    let dir = scenario_dir("missing");
    let mut command = tokio::process::Command::new(crate::bridge::js_runtime());
    command
        .arg(bridge_dir().join("chat.mjs"))
        .current_dir(&dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let bridge = unlaunchable(
        BridgeAdapter::new(command.spawn().expect("node")).unwrap(),
        missing(&dir),
    )
    .await;
    let rust = unlaunchable(
        crate::codex_chat::adapter(&BTreeMap::new(), &[]),
        missing(&dir),
    )
    .await;
    assert_eq!(rust, bridge);
    assert!(
        bridge
            .iter()
            .any(|e| e["message"] == "Could not launch the configured native client."),
        "{bridge:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn an_unusable_launch_option() {
    let dir = scenario_dir("option");
    let mut command = tokio::process::Command::new(crate::bridge::js_runtime());
    command
        .arg(bridge_dir().join("chat.mjs"))
        .current_dir(&dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let bridge = unlaunchable(
        BridgeAdapter::new(command.spawn().expect("node")).unwrap(),
        init(&dir, &["--full-auto"]),
    )
    .await;
    let rust = unlaunchable(
        crate::codex_chat::adapter(&BTreeMap::new(), &[]),
        init(&dir, &["--full-auto"]),
    )
    .await;
    assert_eq!(rust, bridge);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A steer sent before Codex has said which turn is running waits for the id
/// (`early`), then joins that turn.
#[tokio::test]
async fn steering_before_the_turn_has_an_id() {
    both(
        "steer-early",
        &[],
        &[
            Send(json!({"type":"message","id":"slow-a","content":"slow"})),
            Send(json!({"type":"steer","id":"slow-b","content":"also this"})),
            Wait(|e| turn(e, "slow-a", "completed")),
        ],
    )
    .await;
}

/// An interrupt sent before the turn has an id is held until it has one, and
/// then stops that turn.
/// A steer written before the turn has an id is not lost when that turn
/// never takes it: Codex refusing the turn, or finishing it before answering
/// turn/start. It runs as the next turn, like a steer a turn refused.
#[tokio::test]
async fn an_early_steer_survives_a_turn_that_fails_to_start() {
    both(
        "steer-early-failed",
        &[],
        &[
            Send(json!({"type":"message","id":"first","content":"slowfail"})),
            Send(json!({"type":"steer","id":"steer","content":"simple"})),
            Wait(|e| turn(e, "first", "failed")),
            Wait(|e| turn(e, "steer", "completed")),
        ],
    )
    .await;
}

#[tokio::test]
async fn an_early_steer_survives_a_turn_that_ends_before_its_id() {
    both(
        "steer-early-raced",
        &[],
        &[
            Send(json!({"type":"message","id":"first","content":"racing"})),
            Send(json!({"type":"steer","id":"steer","content":"simple"})),
            Wait(|e| turn(e, "first", "completed")),
            Wait(|e| turn(e, "steer", "completed")),
        ],
    )
    .await;
}

#[tokio::test]
async fn interrupting_before_the_turn_has_an_id() {
    both(
        "interrupt-early",
        &[],
        &[
            Send(json!({"type":"message","id":"slow-a","content":"slow"})),
            Send(json!({"type":"interrupt"})),
            Wait(|e| turn(e, "slow-a", "interrupted")),
            Send(json!({"type":"message","id":"after","content":"simple"})),
            Wait(|e| turn(e, "after", "completed")),
        ],
    )
    .await;
}

/// An adapter dropped without `shutdown` still ends its conversation and the
/// app-server it started.
#[cfg(unix)]
#[tokio::test]
async fn a_dropped_adapter_stops_its_app_server() {
    let dir = scenario_dir("dropped-rust");
    let log = dir.join("native.jsonl");
    let mut adapter = crate::codex_chat::adapter(&environment(&log), &[]);
    adapter.start(&init(&dir, &[])).await.expect("start");
    let mut pid = None;
    for _ in 0..50 {
        pid = adapter.pid();
        if pid.is_some() {
            break;
        }
        let _ = tokio::time::timeout(Duration::from_millis(100), adapter.next_event()).await;
    }
    let pid = pid.expect("a running app-server") as libc::pid_t;
    drop(adapter);
    let alive = || unsafe { libc::kill(pid, 0) } == 0;
    for _ in 0..50 {
        if !alive() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let _ = std::fs::remove_dir_all(&dir);
    assert!(!alive(), "the app-server outlived its dropped adapter");
}

// --- Pi ----------------------------------------------------------------------

#[tokio::test]
async fn pi_a_message_with_its_reply_tools_and_usage() {
    both_for(
        &PI,
        "pi-simple",
        &[],
        &[
            Send(json!({"type":"message","id":"one","content":"hello"})),
            Wait(|e| turn(e, "one", "completed")),
        ],
    )
    .await;
}

#[tokio::test]
async fn pi_confirmations_accepted_and_declined() {
    both_for(
        &PI,
        "pi-confirm",
        &[],
        &[
            Send(json!({"type":"message","id":"yes","content":"confirm"})),
            Wait(|e| e["type"] == "approval"),
            Answer("accept", none),
            Wait(|e| turn(e, "yes", "completed")),
            Send(json!({"type":"message","id":"no","content":"confirm"})),
            WaitCount(|e| e["type"] == "approval", 2),
            Answer("decline", none),
            Wait(|e| turn(e, "no", "completed")),
        ],
    )
    .await;
}

#[tokio::test]
async fn pi_models_by_provider_and_depths() {
    both_for(
        &PI,
        "pi-model",
        &[],
        &[
            Send(json!({"type":"model","model":"kunlun/zai-org/GLM-5.3","effort":"high"})),
            Wait(|e| {
                e["type"] == "settings"
                    && e["model"] == "kunlun/zai-org/GLM-5.3"
                    && e["effort"] == "high"
            }),
            // A name without a provider, and approval modes Pi does not have.
            Send(json!({"type":"model","model":"plain"})),
            Wait(|e| e["type"] == "error"),
            Send(json!({"type":"permission","mode":"ask"})),
            WaitCount(|e| e["type"] == "error", 2),
            Send(json!({"type":"message","id":"after","content":"hello"})),
            Wait(|e| turn(e, "after", "completed")),
        ],
    )
    .await;
}

#[tokio::test]
async fn pi_interrupting_and_steering_a_running_turn() {
    both_for(
        &PI,
        "pi-interrupt",
        &[],
        &[
            Send(json!({"type":"message","id":"hold","content":"hold"})),
            Wait(|e| turn(e, "hold", "running")),
            Sleep(100),
            Send(json!({"type":"steer","id":"more","content":"also this"})),
            Send(json!({"type":"model","model":"agentdock/qwen-local"})),
            Wait(|e| e["type"] == "error"),
            Send(json!({"type":"interrupt"})),
            Wait(|e| turn(e, "hold", "interrupted")),
            // Steering with no turn running is a message of its own.
            Send(json!({"type":"steer","id":"lone","content":"hello"})),
            Wait(|e| turn(e, "lone", "completed")),
        ],
    )
    .await;
}

#[tokio::test]
async fn pi_a_resumed_session_and_an_unusable_option() {
    both_for(
        &PI,
        "pi-resume",
        &["--session", "pi-session-1", "--thinking", "high"],
        &[
            Send(json!({"type":"message","id":"one","content":"hello"})),
            Wait(|e| turn(e, "one", "completed")),
        ],
    )
    .await;
}

/// Pi's conversation ends with its dropped adapter too: the shared core does it.
#[cfg(unix)]
#[tokio::test]
async fn pi_a_dropped_adapter_stops_pi() {
    let dir = scenario_dir_for(&PI, "pi-dropped-rust");
    let log = dir.join("native.jsonl");
    let mut adapter = crate::pi_chat::adapter(&environment(&log), &[]);
    adapter
        .start(&init_for(&PI, &dir, &[]))
        .await
        .expect("start");
    let mut pid = None;
    for _ in 0..50 {
        pid = adapter.pid();
        if pid.is_some() {
            break;
        }
        let _ = tokio::time::timeout(Duration::from_millis(100), adapter.next_event()).await;
    }
    let pid = pid.expect("a running Pi") as libc::pid_t;
    drop(adapter);
    let alive = || unsafe { libc::kill(pid, 0) } == 0;
    for _ in 0..50 {
        if !alive() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let _ = std::fs::remove_dir_all(&dir);
    assert!(!alive(), "Pi outlived its dropped adapter");
}

/// An extension command is run by the extension: Pi answers the prompt and
/// starts no run, so no agent_settled ever comes. The turn must still end, or
/// every later message is refused as "a turn is already running".
#[tokio::test]
async fn pi_an_extension_command_ends_its_turn() {
    both_for(
        &PI,
        "pi-extension-command",
        &[],
        &[
            Send(json!({"type":"message","id":"ext","content":"/skill:review"})),
            Wait(|e| turn(e, "ext", "completed")),
            Send(json!({"type":"message","id":"after","content":"hello"})),
            Wait(|e| turn(e, "after", "completed")),
        ],
    )
    .await;
}

/// Interrupting before Pi has started the run (its preflight) stops the run
/// once it starts, instead of letting it run to the end as "interrupted".
#[tokio::test]
async fn pi_interrupting_before_the_run_starts() {
    both_for(
        &PI,
        "pi-interrupt-early",
        &[],
        &[
            Send(json!({"type":"message","id":"slow","content":"slowstart"})),
            Send(json!({"type":"interrupt"})),
            Wait(|e| turn(e, "slow", "interrupted")),
            Send(json!({"type":"message","id":"after","content":"hello"})),
            Wait(|e| turn(e, "after", "completed")),
        ],
    )
    .await;
}

/// Pi clamps a depth its model cannot use, and a model change re-applies the
/// level that model allows; the depth shown is the one Pi runs, not the one asked.
#[tokio::test]
async fn pi_the_depth_shown_is_the_one_pi_runs() {
    let events = both_for(
        &PI,
        "pi-depth",
        &[],
        &[
            // qwen-local does not reason: Pi runs it at `off`.
            Send(json!({"type":"model","effort":"high"})),
            Sleep(300),
            Send(json!({"type":"model","model":"kunlun/zai-org/GLM-5.3","effort":"high"})),
            Wait(|e| {
                e["type"] == "settings"
                    && e["model"] == "kunlun/zai-org/GLM-5.3"
                    && e["effort"] == "high"
            }),
            Send(json!({"type":"model","model":"agentdock/qwen-local"})),
            Wait(|e| e["type"] == "settings" && e["model"] == "agentdock/qwen-local"),
            Sleep(300),
        ],
    )
    .await;
    let settings: Vec<&Value> = events.iter().filter(|e| e["type"] == "settings").collect();
    assert!(
        !settings
            .iter()
            .any(|e| e["model"] == "agentdock/qwen-local" && e.get("effort").is_some()),
        "a depth was shown for a model Pi runs without one: {settings:?}"
    );
    assert_eq!(
        settings.last().map(|e| e["model"].clone()),
        Some(json!("agentdock/qwen-local"))
    );
}
