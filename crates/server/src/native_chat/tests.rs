use super::*;
use crate::chat_adapter::{Answers, ChatInit, Decision};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use tokio::{sync::mpsc, time::Instant};

/// These mirror JavaScript, which the Node bridge they replace relied on.
#[test]
fn javascript_truthiness_and_string_conversion() {
    for falsy in [
        None,
        Some(json!(null)),
        Some(json!(false)),
        Some(json!(0)),
        Some(json!(0.0)),
        Some(json!("")),
    ] {
        assert!(!truthy(falsy.as_ref()), "{falsy:?}");
    }
    for truthy_value in [json!(true), json!(-1), json!("0"), json!([]), json!({})] {
        assert!(truthy(Some(&truthy_value)), "{truthy_value}");
    }
    assert_eq!(js_string(&json!(null)), "null");
    assert_eq!(js_string(&json!([1, null, "a", [2, 3]])), "1,,a,2,3");
    assert_eq!(js_string(&json!({"a": 1})), "[object Object]");
    assert_eq!(js_string(&json!(2.5)), "2.5");
}

#[test]
fn safe_integers_are_whole_and_within_two_to_the_fifty_three() {
    assert_eq!(safe_integer(Some(&json!(42))), Some(42));
    assert_eq!(safe_integer(Some(&json!(-7.0))), Some(-7));
    assert_eq!(
        safe_integer(Some(&json!(9_007_199_254_740_991_i64))),
        Some(9_007_199_254_740_991)
    );
    assert_eq!(safe_integer(Some(&json!(9_007_199_254_740_993_i64))), None);
    assert_eq!(safe_integer(Some(&json!(1.5))), None);
    assert_eq!(safe_integer(Some(&json!("3"))), None);
    assert_eq!(safe_integer(None), None);
}

#[test]
fn native_ids_start_alphanumeric_and_stay_short_and_plain() {
    for good in ["a", "0b", "thread_1-x", &"a".repeat(128)] {
        assert!(native_id(good), "{good}");
    }
    for bad in ["", "-a", "_a", "a b", "a/b", "a.b", &"a".repeat(129)] {
        assert!(!native_id(bad), "{bad}");
    }
}

/// A resume ID is taken only when it is well formed and agrees with the one
/// already known; a refused one leaves the known ID as it was.
#[test]
fn a_resume_id_is_taken_only_when_valid_and_consistent() {
    let mut resume = None;
    assert!(!take_resume(&mut resume, "not valid".into()));
    assert_eq!(resume, None);
    assert!(take_resume(&mut resume, "thread-1".into()));
    assert!(take_resume(&mut resume, "thread-1".into()));
    assert!(!take_resume(&mut resume, "thread-2".into()));
    assert_eq!(resume.as_deref(), Some("thread-1"));
}

/// Flags carry their value after `=` or take the next argument; a leading `=`
/// belongs to the name, and an empty value counts only where it is allowed.
#[test]
fn launch_arguments_are_walked_like_a_command_line() {
    let args: Vec<String> = ["--a=1", "--b", "2", "=x", "--c=", "--d"]
        .into_iter()
        .map(str::to_owned)
        .collect();
    let mut walk = LaunchArgs::new(&args, "Fixture", false);
    let mut seen = Vec::new();
    while let Some(flag) = walk.next_flag() {
        let value = if flag.name.starts_with("--") {
            Some(walk.value(&flag))
        } else {
            None
        };
        seen.push((flag.name.to_owned(), value));
    }
    let missing = Err("Missing Fixture launch option value.".to_owned());
    assert_eq!(
        seen,
        [
            ("--a".to_owned(), Some(Ok("1".to_owned()))),
            ("--b".to_owned(), Some(Ok("2".to_owned()))),
            ("=x".to_owned(), None),
            ("--c".to_owned(), Some(missing.clone())),
            ("--d".to_owned(), Some(missing)),
        ]
    );
    let mut lenient = LaunchArgs::new(&args[4..5], "Fixture", true);
    let flag = lenient.next_flag().unwrap();
    assert_eq!(lenient.value(&flag), Ok(String::new()));
}

#[test]
fn a_launch_is_valid_only_with_a_program_an_absolute_directory_and_sane_arguments() {
    let base = ChatInit {
        provider: "codex".into(),
        program: "codex".into(),
        args: vec!["--model".into(), "x".into()],
        // Absolute on this platform: "/work" is not, on Windows.
        cwd: std::env::temp_dir().to_string_lossy().into_owned(),
        resume_id: Some("thread-1".into()),
    };
    assert!(valid_init(&base));
    let with = |change: &dyn Fn(&mut ChatInit)| {
        let mut init = base.clone();
        change(&mut init);
        valid_init(&init)
    };
    assert!(!with(&|i| i.program.clear()));
    assert!(!with(&|i| i.program = "co\0dex".into()));
    assert!(!with(&|i| i.cwd = "relative".into()));
    assert!(!with(&|i| i.args = vec!["x".into(); 513]));
    assert!(!with(&|i| i.args = vec!["a\0b".into()]));
    assert!(!with(&|i| i.resume_id = Some("not valid".into())));
    assert!(with(&|i| i.resume_id = None));
}

#[test]
fn paths_resolve_lexically_like_path_resolve() {
    assert_eq!(
        resolve(Path::new("/work"), "src/../lib/./a.rs"),
        PathBuf::from("/work/lib/a.rs")
    );
    assert_eq!(
        resolve(Path::new("/work"), "/etc/hosts"),
        PathBuf::from("/etc/hosts")
    );
    assert_eq!(resolve(Path::new("/work"), "../../.."), PathBuf::from("/"));
    assert_eq!(
        clip_value(Some(&json!(3)), 10),
        "",
        "only strings are clipped"
    );
    assert_eq!(clip_value(None, 10), "");
}

/// A client whose answers never reach it, for what the core does alone.
struct Unanswerable {
    core: Core<(), (), ()>,
}
impl NativeClient for Unanswerable {
    type Then = ();
    type Responder = ();
    type Turn = ();
    fn core(&mut self) -> &mut Core<(), (), ()> {
        &mut self.core
    }
    fn start(&mut self, _: ChatInit) {}
    fn message(&mut self, _: String, _: String) {}
    fn steer(&mut self, _: String, _: String) {}
    fn interrupt(&mut self) {}
    fn select_model(&mut self, _: Option<String>, _: Option<String>) {}
    fn set_permission_mode(&mut self, _: String) {}
    fn notification(&mut self, _: Value) {}
    fn settle(&mut self, _: (), _: Result<Value, String>) {}
    fn respond(&mut self, _: &(), _: Decision, _: &Answers) -> Result<(), ()> {
        Err(())
    }
    fn responder(&mut self, key: &str) -> Option<()> {
        self.core.approvals.contains_key(key).then_some(())
    }
}

/// A timeout whose answer cannot be sent is reported once, like the
/// bridge's one-shot timer, instead of firing again on every loop.
/// An event too large to send fails the turn there and then, as the bridge's
/// `fatal` did: the turn ends "failed" even when the same step goes on to
/// finish it, and nothing it emits afterwards is silently lost.
/// A line left incomplete times out once, from when it began, as the
/// bridge's timer does: a client trickling bytes into it cannot keep it
/// open forever.
#[tokio::test]
async fn an_incomplete_line_times_out_from_when_it_began() {
    use tokio::io::AsyncWriteExt;
    let (mut writer, reader) = tokio::io::duplex(1024);
    let (inputs, mut received) = mpsc::unbounded_channel();
    tokio::spawn(read_lines_within(
        reader,
        inputs,
        Duration::from_millis(300),
    ));
    let started = std::time::Instant::now();
    writer.write_all(b"{").await.unwrap();
    // A byte every 100ms: well inside the timeout each time.
    let trickle = tokio::spawn(async move {
        for _ in 0..30 {
            tokio::time::sleep(Duration::from_millis(100)).await;
            if writer.write_all(b" ").await.is_err() {
                break;
            }
        }
    });
    let broken = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            match received.recv().await {
                Some(Input::Broken(message)) => break message,
                Some(_) => {}
                None => break "",
            }
        }
    })
    .await;
    trickle.abort();
    assert_eq!(
        broken.ok(),
        Some("Incomplete structured message timed out.")
    );
    assert!(started.elapsed() < Duration::from_millis(1000));
}

/// Events nobody takes stop piling up past 4 MB, as the bridge stops when
/// its output backs up: the conversation is shut down instead of the
/// server's memory growing without bound.
#[test]
fn a_backlog_of_unread_events_stops_growing() {
    let (events_tx, mut events) = mpsc::unbounded_channel();
    let (inputs, _inputs_rx) = mpsc::unbounded_channel();
    let protocol = Protocol {
        frame: json_rpc_frame,
        reply: |_| None,
        client_name: "Fixture",
        exited: "Fixture exited.",
        permission_modes: None,
    };
    let mut core: Core<(), (), ()> = Core::new(
        &BTreeMap::new(),
        &[],
        &protocol,
        events_tx,
        inputs,
        Arc::new(std::sync::Mutex::new(None)),
    );
    let chunk = "x".repeat(180 * 1024);
    for _ in 0..32 {
        core.emit(json!({"type":"tool","text":chunk}));
    }
    let mut bytes = 0;
    while let Ok(event) = events.try_recv() {
        bytes += stringify(&event).len() + 1;
    }
    assert!(
        bytes <= 4 * 1024 * 1024 + 192 * 1024,
        "{bytes} bytes queued"
    );
    assert!(core.backlogged, "the backlog did not stop the conversation");
}

#[test]
fn an_oversized_event_fails_its_turn_at_once() {
    let (events_tx, mut events) = mpsc::unbounded_channel();
    let (inputs, _inputs_rx) = mpsc::unbounded_channel();
    let protocol = Protocol {
        frame: json_rpc_frame,
        reply: |_| None,
        client_name: "Fixture",
        exited: "Fixture exited.",
        permission_modes: None,
    };
    let mut core: Core<(), (), ()> = Core::new(
        &BTreeMap::new(),
        &[],
        &protocol,
        events_tx,
        inputs,
        Arc::new(std::sync::Mutex::new(None)),
    );
    core.open_turn("turn".into());
    core.emit(json!({"type":"tool","text":"x".repeat(MAX_EVENT_LINE)}));
    // The same step then finishes the turn, as a handler does.
    core.finish("completed");
    let mut seen = Vec::new();
    while let Ok(event) = events.try_recv() {
        seen.push(event);
    }
    assert!(
        seen.iter()
            .any(|e| e["message"] == "Structured event exceeded its wire size limit."),
        "{seen:?}"
    );
    let turns: Vec<&Value> = seen.iter().filter(|e| e["type"] == "turn").collect();
    assert_eq!(turns.len(), 1, "{seen:?}");
    assert_eq!(turns[0]["status"], "failed");
}

#[test]
fn an_approval_times_out_once_even_when_its_answer_cannot_be_sent() {
    let (events_tx, mut events) = mpsc::unbounded_channel();
    let (inputs, _inputs_rx) = mpsc::unbounded_channel();
    let protocol = Protocol {
        frame: json_rpc_frame,
        reply: |_| None,
        client_name: "Fixture",
        exited: "Fixture exited.",
        permission_modes: None,
    };
    let core = Core::new(
        &BTreeMap::new(),
        &[],
        &protocol,
        events_tx,
        inputs,
        Arc::new(std::sync::Mutex::new(None)),
    );
    let mut client = Unanswerable { core };
    client.core.approvals.insert(
        "request".into(),
        Approval {
            id: "approval".into(),
            responder: (),
            deadline: Some(Instant::now()),
            choices: vec!["decline".into()],
        },
    );
    client.expire(Instant::now());
    let mut first = Vec::new();
    while let Ok(event) = events.try_recv() {
        first.push(event);
    }
    assert!(
        first
            .iter()
            .any(|e| e["message"] == "Native approval timed out and was cancelled."),
        "{first:?}"
    );
    assert_eq!(client.core.next_deadline(), None);
    client.expire(Instant::now() + Duration::from_secs(1));
    assert!(events.try_recv().is_err(), "the timeout fired again");
    // Still open: the person can answer it.
    assert!(client.core.approvals.contains_key("request"));
}

#[test]
fn clipping_matches_the_bridge() {
    assert_eq!(clip("a\u{1b}[31mred\u{1b}[0m\u{7}b", MAX_TEXT), "aredb");
    assert_eq!(clip("tab\tand\nline", MAX_TEXT), "tab\tand\nline");
    // A cut through a multi-byte character leaves the replacement mark.
    assert_eq!(clip("ab🦊", 3), "ab\u{fffd}\n[truncated]");
    assert_eq!(clip("\u{1b}[", MAX_TEXT), "[");
}

#[test]
fn secrets_are_the_values_of_secret_looking_names_longest_first() {
    let secrets = secrets_of(
        [
            ("OPENAI_API_KEY", "sk-long-value"),
            ("MY_TOKEN", "tok1"),
            ("HOME", "/home/u"),
            ("PASSWORD", "abc"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_owned(), v.to_owned())),
    );
    assert_eq!(secrets, ["sk-long-value", "tok1"]);
    assert_eq!(
        scrub(json!({"text":"key sk-long-value!"}), &secrets),
        json!({"text":"key [redacted]!"})
    );
}
