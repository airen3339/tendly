// Reached only through the parity tests until the switch selects it.
#![allow(dead_code)]
//! Codex, driven from this process: `codex app-server` over JSON-RPC, with no
//! Node bridge in between.
//!
//! This is `packages/native-bridge/chat-codex.mjs` (with the parts of
//! `chat-common.mjs` and `chat.mjs` it relies on) in Rust, and it produces the
//! same events: the same fields, the same clipping and redaction, the same
//! error wording. Behaviour that differs from the bridge is a bug here.
//!
//! One task owns the conversation. Everything that can happen to it -- a
//! command from the server, a message or a reply from Codex, a request timing
//! out, an approval expiring, Codex exiting -- arrives on one channel and is
//! handled to the end before the next. A request to Codex records what should
//! follow its answer (a [`Then`]) instead of awaiting it, so no state is held
//! across a wait. The bridge's checks that a reply still belongs to the turn
//! that asked (`this.active === active`) are kept, as turn indexes.
use crate::chat_adapter::{Answers, ChatInit, ClientAdapter, Decision, Failure};
use agentdock_runtime::process::{self, OwnedGroup};
use indexmap::IndexMap;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, HashSet},
    path::{Component, Path, PathBuf},
    process::Stdio,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    sync::mpsc,
    time::Instant,
};

pub const MAX_NATIVE_LINE: usize = 4 * 1024 * 1024;
pub const MAX_TEXT: usize = 64 * 1024;
pub const MAX_EVENT_LINE: usize = 192 * 1024;
const MAX_NATIVE_TOTAL: usize = 128 * 1024 * 1024;
const LINE_TIMEOUT: Duration = Duration::from_secs(10);
const RPC_TIMEOUT: Duration = Duration::from_secs(30);
const PROBE_TIMEOUT: Duration = Duration::from_secs(4);
const APPROVAL_TIMEOUT: Duration = Duration::from_secs(600);
const MAX_DIAGNOSTIC: usize = 16 * 1024 * 1024;

// --- what the bridge does to every string and event --------------------------

/// `clip` (chat-common.mjs): no terminal escapes or control characters, and at
/// most `limit` bytes, cut where a multi-byte character may be split.
pub fn clip(value: &str, limit: usize) -> String {
    let mut cleaned = String::with_capacity(value.len());
    let chars: Vec<char> = value.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        // \x1b[ params intermediates final, as /\u001b\[[0-?]*[ -/]*[@-~]/.
        if chars[i] == '\u{1b}' && chars.get(i + 1) == Some(&'[') {
            let mut j = i + 2;
            while j < chars.len() && ('0'..='?').contains(&chars[j]) {
                j += 1;
            }
            while j < chars.len() && (' '..='/').contains(&chars[j]) {
                j += 1;
            }
            if j < chars.len() && ('@'..='~').contains(&chars[j]) {
                i = j + 1;
                continue;
            }
        }
        cleaned.push(chars[i]);
        i += 1;
    }
    cleaned.retain(|c| !matches!(c as u32, 0x00..=0x08 | 0x0b | 0x0c | 0x0e..=0x1f));
    if cleaned.len() <= limit {
        return cleaned;
    }
    let mut cut = String::from_utf8_lossy(&cleaned.as_bytes()[..limit]).into_owned();
    cut.push_str("\n[truncated]");
    cut
}

/// `clip` of anything: what is not a string becomes empty.
fn clip_value(value: Option<&Value>, limit: usize) -> String {
    value
        .and_then(Value::as_str)
        .map(|v| clip(v, limit))
        .unwrap_or_default()
}

/// `nativeId`: an id a client may use for a session or thread.
pub fn native_id(value: &str) -> bool {
    let mut bytes = value.bytes();
    bytes.next().is_some_and(|b| b.is_ascii_alphanumeric())
        && value.len() <= 128
        && bytes.all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// JavaScript truthiness of a JSON value.
fn truthy(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().is_some_and(|n| n != 0.0 && !n.is_nan()),
        Some(Value::String(s)) => !s.is_empty(),
        Some(_) => true,
    }
}

/// `String(value)`, for the template strings the bridge builds.
fn js_string(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Null => "null".into(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        Value::Array(items) => items
            .iter()
            .map(|item| match item {
                Value::Null => String::new(),
                other => js_string(other),
            })
            .collect::<Vec<_>>()
            .join(","),
        Value::Object(_) => "[object Object]".into(),
    }
}

/// `JSON.stringify`, which keeps a value's own key order (serde_json is built
/// with `preserve_order` for exactly this).
fn stringify(value: &Value) -> String {
    serde_json::to_string(value).unwrap_or_default()
}

/// An integer JavaScript would count as `Number.isSafeInteger`.
fn safe_integer(value: Option<&Value>) -> Option<i64> {
    let number = value?.as_f64()?;
    (number.fract() == 0.0 && number.abs() <= 9_007_199_254_740_991.0).then_some(number as i64)
}

/// `redactEvents`: every secret-looking variable's value in the client's
/// environment, longest first, becomes `[redacted]` wherever it appears.
pub fn secrets_of(environment: impl Iterator<Item = (String, String)>) -> Vec<String> {
    const NAMES: [&str; 6] = [
        "TOKEN",
        "SECRET",
        "PASSWORD",
        "PRIVATE_KEY",
        "API_KEY",
        "AUTHORIZATION",
    ];
    let mut secrets: Vec<String> = environment
        .filter(|(key, value)| {
            let key = key.to_uppercase();
            NAMES.iter().any(|name| key.contains(name)) && value.encode_utf16().count() >= 4
        })
        .map(|(_, value)| value)
        .collect();
    secrets.sort_by_key(|value| std::cmp::Reverse(value.encode_utf16().count()));
    secrets
}

fn scrub(value: Value, secrets: &[String]) -> Value {
    match value {
        Value::String(text) => Value::String(secrets.iter().fold(text, |text, secret| {
            text.replace(secret.as_str(), "[redacted]")
        })),
        Value::Array(items) => Value::Array(items.into_iter().map(|v| scrub(v, secrets)).collect()),
        Value::Object(object) => Value::Object(
            object
                .into_iter()
                .map(|(key, item)| (key, scrub(item, secrets)))
                .collect(),
        ),
        other => other,
    }
}

/// `path.resolve(root, value)`, lexically, as the bridge reports paths.
fn resolve(root: &Path, value: &str) -> PathBuf {
    let joined = root.join(value);
    let mut out = PathBuf::new();
    for component in joined.components() {
        match component {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// `approvalScope`: the kind of access a request asks for, and any path in it
/// that lies outside the working directory.
pub fn approval_scope(
    cwd: &str,
    access: Option<&str>,
    paths: &[Value],
    command: Option<&Value>,
) -> Option<Value> {
    let root = (!cwd.is_empty()).then(|| resolve(Path::new("/"), cwd));
    let mut outside: Vec<String> = Vec::new();
    if let Some(root) = &root {
        let mut seen = HashSet::new();
        let prefix = {
            let text = root.to_string_lossy().into_owned();
            if text.ends_with(std::path::MAIN_SEPARATOR) {
                text
            } else {
                format!("{text}{}", std::path::MAIN_SEPARATOR)
            }
        };
        for path in paths.iter().filter_map(Value::as_str) {
            let length = path.encode_utf16().count();
            if length == 0 || length > 1024 || path.contains('\0') {
                continue;
            }
            let resolved = resolve(root, path).to_string_lossy().into_owned();
            if !seen.insert(resolved.clone()) {
                continue;
            }
            if resolved != root.to_string_lossy() && !resolved.starts_with(&prefix) {
                outside.push(resolved);
            }
        }
        outside.truncate(8);
    }
    let mut scope = Map::new();
    if let Some(access) = access.filter(|a| ["read", "write", "execute", "network"].contains(a)) {
        scope.insert("access".into(), json!(access));
    }
    if !outside.is_empty() {
        scope.insert("outside".into(), json!(outside));
    }
    if let Some(command) = command
        .and_then(Value::as_str)
        .filter(|c| !c.trim().is_empty())
    {
        let chars: Vec<u16> = command.encode_utf16().collect();
        let text = if chars.len() > 600 {
            format!("{}…", String::from_utf16_lossy(&chars[..599]))
        } else {
            command.to_owned()
        };
        scope.insert("command".into(), json!(text));
    }
    (!scope.is_empty()).then_some(Value::Object(scope))
}

/// `pathsIn`: absolute paths anywhere in a permission request.
fn paths_in(value: &Value, found: &mut Vec<Value>, depth: usize) {
    if depth > 6 || found.len() >= 16 {
        return;
    }
    match value {
        Value::String(text) => {
            let bytes = text.as_bytes();
            let windows = bytes.len() >= 3
                && bytes[0].is_ascii_alphabetic()
                && bytes[1] == b':'
                && matches!(bytes[2], b'\\' | b'/');
            if text.starts_with('/') || windows {
                found.push(value.clone());
            }
        }
        Value::Array(items) => items
            .iter()
            .for_each(|item| paths_in(item, found, depth + 1)),
        Value::Object(object) => object
            .values()
            .for_each(|item| paths_in(item, found, depth + 1)),
        _ => {}
    }
}

// --- launch ------------------------------------------------------------------

/// `codexLaunch`: the app-server arguments, the thread's own settings and any
/// thread to resume, from the session's launch arguments.
#[derive(Debug, Clone, PartialEq)]
pub struct Launch {
    pub args: Vec<String>,
    /// `{cwd, model?}`, in that order, as thread/start receives it.
    pub thread: Map<String, Value>,
    pub resume: Option<String>,
}

pub fn codex_launch(init: &ChatInit) -> Result<Launch, String> {
    let mut args = vec!["app-server".to_owned()];
    let mut thread = Map::new();
    thread.insert("cwd".into(), json!(init.cwd));
    let mut resume = init.resume_id.clone();
    let mut i = 0;
    while i < init.args.len() {
        let arg = &init.args[i];
        let (flag, inline) = match arg.find('=') {
            Some(at) if at > 0 => (&arg[..at], Some(arg[at + 1..].to_owned())),
            _ => (arg.as_str(), None),
        };
        let mut value = || -> Result<String, String> {
            let result = match &inline {
                Some(value) => Some(value.clone()),
                None => {
                    i += 1;
                    init.args.get(i).cloned()
                }
            };
            result
                .filter(|v| !v.is_empty())
                .ok_or_else(|| "Missing Codex launch option value.".to_owned())
        };
        match flag {
            "-c" | "--config" => {
                let v = value()?;
                args.extend(["-c".to_owned(), v]);
            }
            "--model" | "-m" => {
                let v = value()?;
                thread.insert("model".into(), json!(v));
            }
            "--ask-for-approval" | "-a" => {
                let v = value()?;
                args.extend([
                    "-c".to_owned(),
                    format!("approval_policy={}", stringify(&json!(v))),
                ]);
            }
            "--sandbox" | "-s" => {
                let v = value()?;
                args.extend([
                    "-c".to_owned(),
                    format!("sandbox_mode={}", stringify(&json!(v))),
                ]);
            }
            "--enable" | "--disable" => {
                let v = value()?;
                args.extend([flag.to_owned(), v]);
            }
            "--cd" | "-C" => {
                let v = value()?;
                if resolve(Path::new(&init.cwd), &v) != resolve(Path::new("/"), &init.cwd) {
                    return Err("Codex launch directory does not match the workspace.".into());
                }
            }
            "resume" | "--resume" => {
                let id = value()?;
                if !native_id(&id) || resume.as_ref().is_some_and(|r| *r != id) {
                    return Err("Invalid or conflicting Codex resume ID.".into());
                }
                resume = Some(id);
            }
            "--no-alt-screen" | "app-server" => {}
            _ => return Err("Unsupported Codex launch option in structured mode.".into()),
        }
        i += 1;
    }
    Ok(Launch {
        args,
        thread,
        resume,
    })
}

/// `validateChatInput(init, true)`.
fn valid_init(init: &ChatInit) -> bool {
    !init.program.is_empty()
        && !init.program.contains('\0')
        && Path::new(&init.cwd).is_absolute()
        && init.args.len() <= 512
        && !init.args.iter().any(|arg| arg.contains('\0'))
        && serde_json::to_string(&init.args).is_ok_and(|s| s.len() <= 64 * 1024)
        && init.resume_id.as_deref().is_none_or(native_id)
}

/// `codexModels`: the models app-server's model/list offers, as it describes them.
fn codex_models(result: &Value) -> Option<Vec<Value>> {
    let rows = result
        .get("data")
        .and_then(Value::as_array)
        .or_else(|| result.get("models").and_then(Value::as_array))
        .or_else(|| result.as_array())?;
    Some(
        rows.iter()
            .filter_map(|row| {
                let object = row.as_object()?;
                let id = ["id", "model", "name"]
                    .iter()
                    .map(|key| object.get(*key))
                    .find(|value| value.is_some_and(|v| !v.is_null()))
                    .flatten()
                    .and_then(Value::as_str)
                    .filter(|id| !id.is_empty())?;
                if object.get("hidden") == Some(&Value::Bool(true)) {
                    return None;
                }
                let efforts: Option<Vec<Value>> = object
                    .get("supportedReasoningEfforts")
                    .and_then(Value::as_array)
                    .map(|levels| {
                        levels
                            .iter()
                            .filter_map(|level| {
                                level
                                    .as_str()
                                    .or_else(|| {
                                        level.get("reasoningEffort").and_then(Value::as_str)
                                    })
                                    .map(|level| json!(level))
                            })
                            .collect()
                    });
                let mut entry = Map::new();
                entry.insert("id".into(), json!(id));
                entry.insert(
                    "name".into(),
                    json!(
                        object
                            .get("displayName")
                            .and_then(Value::as_str)
                            .unwrap_or(id)
                    ),
                );
                if object.get("isDefault") == Some(&Value::Bool(true)) {
                    entry.insert("isDefault".into(), json!(true));
                }
                if let Some(description) = object.get("description").and_then(Value::as_str) {
                    entry.insert("description".into(), json!(description));
                }
                if let Some(efforts) = efforts.filter(|e| !e.is_empty()) {
                    entry.insert("efforts".into(), Value::Array(efforts));
                }
                Some(Value::Object(entry))
            })
            .collect(),
    )
}

fn item_status(item: &Value) -> &'static str {
    let status = item.get("status").and_then(Value::as_str);
    if matches!(status, Some("failed" | "declined" | "cancelled"))
        || item.get("success") == Some(&Value::Bool(false))
    {
        "failed"
    } else if status == Some("inProgress") {
        "running"
    } else {
        "completed"
    }
}

/// `requestKey`: a native request id, keyed by type so `1` and `"1"` differ.
fn request_key(id: &Value) -> String {
    match id {
        Value::String(s) => format!("string:{s}"),
        Value::Number(n) => format!("number:{n}"),
        Value::Null => "object:null".into(),
        other => format!(
            "{}:{}",
            if other.is_boolean() {
                "boolean"
            } else {
                "object"
            },
            js_string(other)
        ),
    }
}

// --- the conversation --------------------------------------------------------

/// What reaches the conversation's task.
enum Input {
    /// Start Codex; the sender learns once the launch has been tried.
    Start(ChatInit, tokio::sync::oneshot::Sender<()>),
    Message(String, String),
    Steer(String, String),
    Approve(String, Decision, Option<Answers>),
    Interrupt,
    Model(Option<String>, Option<String>),
    Permission(String),
    Shutdown,
    /// A message Codex wrote, parsed.
    Native(Value),
    /// The connection to Codex broke, in the bridge's words.
    Broken(&'static str),
    /// Codex's stdout ended.
    Ended,
}

/// A message written into the conversation: its id and content.
#[derive(Debug, Clone)]
struct Written {
    id: String,
    content: String,
}

/// The turn a chat is running (`Turn` in chat-common.mjs).
struct Turn {
    id: String,
    interrupted: bool,
    early: Vec<Written>,
    native_turn: Option<Value>,
    interrupt_sent: bool,
}

/// What follows a request's answer: the code after an `await` in the bridge.
enum Then {
    Initialize,
    ModelList,
    OpenThread {
        turn: usize,
        content: String,
    },
    StartTurn {
        turn: usize,
    },
    Steer {
        turn: usize,
        message: Written,
        then: Vec<Written>,
    },
    Interrupt {
        turn: usize,
    },
}

struct Pending {
    then: Then,
    deadline: Instant,
    /// A capability probe, which may simply never be answered.
    optional: bool,
    /// An interrupt that times out is not a broken connection.
    interrupt: bool,
}

/// How a native request is answered once the person has decided.
enum Responder {
    Decision { id: Value },
    Questions { id: Value, questions: Vec<Value> },
    Permissions { id: Value, permissions: Value },
    Elicitation { id: Value },
}

struct Approval {
    id: String,
    responder: Responder,
    deadline: Instant,
    choices: Vec<String>,
}

const CANCELLED: &str = "Native client closed.";

struct Conversation {
    events: mpsc::UnboundedSender<Value>,
    secrets: Vec<String>,
    cwd: String,
    program: String,
    env: Vec<(String, String)>,
    env_remove: Vec<String>,
    launch: Option<Launch>,
    inputs: mpsc::UnboundedSender<Input>,
    writer: Option<mpsc::UnboundedSender<Vec<u8>>>,
    queued: Arc<AtomicUsize>,
    group: OwnedGroup,
    child: Option<tokio::process::Child>,
    pid: Arc<std::sync::Mutex<Option<u32>>>,
    sequence: u64,
    pending: HashMap<String, Pending>,
    seen: HashMap<String, String>,
    approvals: IndexMap<String, Approval>,
    turns: Vec<Turn>,
    active: Option<usize>,
    deferred: Vec<Written>,
    tools: IndexMap<String, (String, String)>,
    ready: bool,
    closed: bool,
    port_closed: bool,
    initializing: bool,
    initialized: bool,
    finished: bool,
    native_session_id: Option<String>,
    permission_mode: Option<String>,
    models: Option<Vec<Value>>,
    default_model: Option<String>,
    effort: Option<String>,
    approval_policy: Option<String>,
    sandbox_policy: Option<Value>,
    sandbox_overridden: bool,
    base_sandbox: Option<Value>,
}

impl Conversation {
    // -- emitting --

    /// `emit` in chat.mjs: redacted, and refused when the line is too long.
    fn emit(&mut self, event: Value) {
        if self.finished {
            return;
        }
        let event = scrub(event, &self.secrets);
        if stringify(&event).len() + 1 > MAX_EVENT_LINE {
            self.fatal("Structured event exceeded its wire size limit.");
            return;
        }
        let _ = self.events.send(event);
    }
    /// `ChatBase.error`: clipped to 2 KB.
    fn error(&mut self, message: &str) {
        let message = clip(message, 2048);
        self.emit(json!({"type":"error","message":message}));
    }
    /// An error chat.mjs reports itself, unclipped.
    fn raw_error(&mut self, message: &str) {
        self.emit(json!({"type":"error","message":message}));
    }

    // -- the connection to Codex (NativeProcess) --

    fn spawn(&mut self, args: &[String]) -> Result<(), ()> {
        let node = crate::bridge::js_runtime();
        let (program, args) =
            process::launcher_with_node(&self.program, args, &node.to_string_lossy())
                .map_err(|_| ())?;
        let mut command = tokio::process::Command::new(program);
        command
            .args(args)
            .current_dir(&self.cwd)
            .envs(self.env.iter().cloned())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        for key in self
            .env_remove
            .iter()
            .cloned()
            .chain(crate::bridge::agentdock_secrets())
        {
            command.env_remove(key);
        }
        process::own_group(command.as_std_mut());
        let mut child = command.spawn().map_err(|_| ())?;
        let (Some(mut stdin), Some(stdout), Some(mut stderr)) =
            (child.stdin.take(), child.stdout.take(), child.stderr.take())
        else {
            return Err(());
        };
        self.group = OwnedGroup::new(child.id());
        *self.pid.lock().expect("codex pid") = child.id();
        self.child = Some(child);
        // Writing never holds up the conversation, as Node's buffered stdin did not.
        let (writer, mut lines) = mpsc::unbounded_channel::<Vec<u8>>();
        let (inputs, queued) = (self.inputs.clone(), self.queued.clone());
        tokio::spawn(async move {
            while let Some(line) = lines.recv().await {
                let length = line.len();
                if stdin.write_all(&line).await.is_err() {
                    let _ = inputs.send(Input::Broken("Native client input closed."));
                    return;
                }
                queued.fetch_sub(length, Ordering::AcqRel);
            }
            let _ = stdin.shutdown().await;
        });
        self.writer = Some(writer);
        tokio::spawn(read_lines(stdout, self.inputs.clone()));
        let inputs = self.inputs.clone();
        tokio::spawn(async move {
            let mut bytes = [0u8; 8192];
            let mut total = 0usize;
            while let Ok(count) = stderr.read(&mut bytes).await {
                if count == 0 {
                    return;
                }
                // What Codex writes here is never kept: it can carry
                // configuration and credentials.
                total += count;
                if total > MAX_DIAGNOSTIC {
                    let _ = inputs.send(Input::Broken(
                        "Native diagnostic output exceeded its size limit.",
                    ));
                    return;
                }
            }
        });
        Ok(())
    }
    /// `NativeProcess.send`.
    fn send(&mut self, message: Value) -> Result<(), &'static str> {
        let Some(writer) = self.writer.as_ref().filter(|_| !self.port_closed) else {
            return Err("Native client input is unavailable.");
        };
        let line = format!("{}\n", stringify(&message)).into_bytes();
        if line.len() > MAX_NATIVE_LINE || self.queued.load(Ordering::Acquire) > MAX_NATIVE_LINE {
            return Err("Native input exceeded its buffer limit.");
        }
        self.queued.fetch_add(line.len(), Ordering::AcqRel);
        writer
            .send(line)
            .map_err(|_| "Native client input is unavailable.")
    }
    /// `NativeProcess.rpc`: a request whose answer runs `then`.
    fn rpc(&mut self, method: &str, params: Value, then: Then, optional: bool) {
        self.sequence += 1;
        let id = format!("agentdock-{}", self.sequence);
        let timeout = if optional {
            PROBE_TIMEOUT.min(RPC_TIMEOUT)
        } else {
            RPC_TIMEOUT
        };
        let interrupt = matches!(method, "interrupt" | "turn/interrupt" | "abort");
        match self.send(json!({"id":id,"method":method,"params":params})) {
            Ok(()) => {
                self.pending.insert(
                    id,
                    Pending {
                        then,
                        deadline: Instant::now() + timeout,
                        optional,
                        interrupt,
                    },
                );
            }
            Err(message) => self.settle(then, Err(message.to_owned())),
        }
    }
    /// The connection's own failure: `NativeProcess` hands it to `fatal`.
    fn failure(&mut self, message: &str) {
        if !self.port_closed {
            self.fatal(message);
        }
    }
    /// `NativeProcess.close`: no more lines in or out, every waiting request
    /// failed, and Codex asked to stop.
    fn close_port(&mut self) {
        if self.port_closed {
            return;
        }
        self.port_closed = true;
        self.writer = None;
        self.reject_pending();
    }
    fn reject_pending(&mut self) {
        let pending: Vec<_> = self.pending.drain().map(|(_, p)| p.then).collect();
        for then in pending {
            self.settle(then, Err(CANCELLED.to_owned()));
        }
    }

    // -- ChatBase --

    fn begin(&mut self, id: String, content: String) -> Option<usize> {
        if !self.ready || self.closed {
            self.error("The native client is not ready.");
            return None;
        }
        let digest = hex(Sha256::digest(content.as_bytes()).as_slice());
        if let Some(previous) = self.seen.get(&id) {
            if *previous != digest {
                self.error("Message ID was already used for different content.");
            }
            return None;
        }
        if self.active.is_some() {
            self.error(
                "A turn is already running. Interrupt it or wait before sending another message.",
            );
            return None;
        }
        if self.seen.len() >= 4096 {
            self.error("This bridge reached its message limit. Reopen the saved native session.");
            self.emit(json!({"type":"turn","id":id,"status":"failed"}));
            return None;
        }
        self.seen.insert(id.clone(), digest);
        let turn = self.new_turn(id.clone());
        let text = clip(&content, MAX_TEXT);
        self.emit(json!({"type":"message","id":id,"role":"user","text":text,"delta":false}));
        self.emit(json!({"type":"turn","id":id,"status":"running"}));
        Some(turn)
    }
    fn new_turn(&mut self, id: String) -> usize {
        self.turns.push(Turn {
            id,
            interrupted: false,
            early: Vec::new(),
            native_turn: None,
            interrupt_sent: false,
        });
        let turn = self.turns.len() - 1;
        self.active = Some(turn);
        turn
    }
    fn accept_steer(&mut self, id: &str, content: &str) -> bool {
        if !self.ready || self.closed {
            self.error("The native client is not ready.");
            return false;
        }
        let digest = hex(Sha256::digest(content.as_bytes()).as_slice());
        if let Some(previous) = self.seen.get(id) {
            if *previous != digest {
                self.error("Message ID was already used for different content.");
            }
            return false;
        }
        if self.seen.len() >= 4096 {
            self.error("This bridge reached its message limit. Reopen the saved native session.");
            return false;
        }
        self.seen.insert(id.to_owned(), digest);
        true
    }
    fn finish(&mut self, status: &str) {
        let Some(turn) = self.active.take() else {
            return;
        };
        let id = self.turns[turn].id.clone();
        self.clear_approvals(false);
        self.emit(json!({"type":"turn","id":id,"status":status}));
    }
    fn announce(&mut self, id: Option<&str>) {
        let valid = id.filter(|id| native_id(id));
        if let (Some(id), Some(current)) = (valid, &self.native_session_id)
            && id != current
        {
            self.emit(json!({"type":"cleared"}));
        }
        if let Some(id) = valid {
            self.native_session_id = Some(id.to_owned());
        }
        self.ready = true;
        let mut event = json!({"type":"ready"});
        if let Some(id) = &self.native_session_id {
            event["native_session_id"] = json!(id);
        }
        self.emit(event);
    }
    /// `ChatBase.settings`.
    fn settings(&mut self, models: Option<&[Value]>, model: Option<&str>, effort: Option<&str>) {
        const EFFORTS: [&str; 5] = ["low", "medium", "high", "xhigh", "max"];
        let list: Option<Vec<Value>> = models.map(|models| {
            models
                .iter()
                .filter_map(|entry| {
                    let id = entry
                        .get("id")?
                        .as_str()
                        .filter(|id| !id.is_empty() && id.encode_utf16().count() <= 128)?;
                    let mut row = Map::new();
                    row.insert("id".into(), json!(id));
                    let name = entry
                        .get("name")
                        .and_then(Value::as_str)
                        .filter(|n| !n.is_empty())
                        .unwrap_or(id);
                    row.insert("name".into(), json!(clip(name, 128)));
                    if let Some(description) = entry
                        .get("description")
                        .and_then(Value::as_str)
                        .filter(|d| !d.is_empty())
                    {
                        row.insert("description".into(), json!(clip(description, 256)));
                    }
                    if entry.get("isDefault") == Some(&Value::Bool(true)) {
                        row.insert("isDefault".into(), json!(true));
                    }
                    if let Some(efforts) = entry.get("efforts").and_then(Value::as_array) {
                        let efforts: Vec<Value> = efforts
                            .iter()
                            .filter(|e| e.as_str().is_some_and(|e| EFFORTS.contains(&e)))
                            .take(16)
                            .cloned()
                            .collect();
                        if !efforts.is_empty() {
                            row.insert("efforts".into(), Value::Array(efforts));
                        }
                    }
                    Some(Value::Object(row))
                })
                .take(64)
                .collect()
        });
        let selected = model.filter(|m| !m.is_empty() && m.encode_utf16().count() <= 128);
        let depth = effort
            .filter(|e| (1..=16).contains(&e.len()) && e.bytes().all(|b| b.is_ascii_lowercase()));
        let permission = self.permission_mode.clone().filter(|m| {
            (1..=24).contains(&m.len()) && m.bytes().all(|b| b.is_ascii_lowercase() || b == b'_')
        });
        let list = list.filter(|list| !list.is_empty());
        if list.is_none() && selected.is_none() && depth.is_none() && permission.is_none() {
            return;
        }
        let mut event = Map::new();
        event.insert("type".into(), json!("settings"));
        if let Some(model) = selected {
            event.insert("model".into(), json!(model));
        }
        if let Some(effort) = depth {
            event.insert("effort".into(), json!(effort));
        }
        if let Some(mode) = permission {
            event.insert("permission_mode".into(), json!(mode));
        }
        // Codex approves per command, so only the two ends of the range exist.
        event.insert("permission_modes".into(), json!(["ask", "danger"]));
        if let Some(list) = list {
            event.insert("models".into(), Value::Array(list));
        }
        self.emit(Value::Object(event));
    }
    fn restate_settings(&mut self) {
        let models = self.models.clone();
        let model = self.selected_model();
        let effort = self.effort.clone();
        self.settings(models.as_deref(), model.as_deref(), effort.as_deref());
    }
    fn selected_model(&self) -> Option<String> {
        self.launch
            .as_ref()
            .and_then(|l| l.thread.get("model"))
            .and_then(Value::as_str)
            .map(str::to_owned)
            .or_else(|| self.default_model.clone())
    }
    fn usage(&mut self, usage: Option<&Value>, context: Option<&Value>, window: Option<&Value>) {
        let pick = |a: &str, b: &str| {
            let value = usage.and_then(|u| u.get(a)).filter(|v| !v.is_null());
            safe_integer(value.or_else(|| usage.and_then(|u| u.get(b))))
        };
        let mut event = Map::new();
        event.insert("type".into(), json!("usage"));
        for (key, value) in [
            ("input_tokens", pick("inputTokens", "input_tokens")),
            ("output_tokens", pick("outputTokens", "output_tokens")),
            ("context_tokens", safe_integer(context)),
            ("context_window", safe_integer(window)),
        ] {
            if let Some(value) = value.filter(|v| {
                if key == "context_window" {
                    *v > 0
                } else {
                    *v >= 0
                }
            }) {
                event.insert(key.into(), json!(value));
            }
        }
        if event.len() > 1 {
            self.emit(Value::Object(event));
        }
    }
    fn approval(&mut self, key: String, description: Map<String, Value>, responder: Responder) {
        if self.approvals.contains_key(&key) {
            return;
        }
        let choices: Vec<String> = description
            .get("choices")
            .and_then(Value::as_array)
            .map(|c| {
                c.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default();
        let fallback = if choices.iter().any(|c| c == "cancel") {
            Decision::Cancel
        } else {
            Decision::Decline
        };
        if self.approvals.len() >= 32 {
            let _ = self.respond(&responder, fallback, &Map::new());
            self.error("Too many pending native approvals.");
            return;
        }
        let id = uuid::Uuid::new_v4().to_string();
        self.approvals.insert(
            key,
            Approval {
                id: id.clone(),
                responder,
                deadline: Instant::now() + APPROVAL_TIMEOUT,
                choices,
            },
        );
        let mut event = Map::new();
        event.insert("type".into(), json!("approval"));
        event.insert("id".into(), json!(id));
        event.extend(description);
        self.emit(Value::Object(event));
    }
    fn resolve_approval(&mut self, key: &str, decision: Decision, answers: &Answers) {
        let Some(pending) = self.approvals.get(key) else {
            return;
        };
        let responder = match &pending.responder {
            Responder::Decision { id } => Responder::Decision { id: id.clone() },
            Responder::Questions { id, questions } => Responder::Questions {
                id: id.clone(),
                questions: questions.clone(),
            },
            Responder::Permissions { id, permissions } => Responder::Permissions {
                id: id.clone(),
                permissions: permissions.clone(),
            },
            Responder::Elicitation { id } => Responder::Elicitation { id: id.clone() },
        };
        if self.respond(&responder, decision, answers).is_err() {
            self.error("Invalid or unavailable native approval response.");
            return;
        }
        if let Some(pending) = self.approvals.shift_remove(key) {
            self.emit(json!({"type":"approval_resolved","id":pending.id}));
        }
    }
    fn answer(&mut self, request_id: &str, decision: Decision, answers: Option<Answers>) {
        let Some((key, pending)) = self.approvals.iter().find(|(_, p)| p.id == request_id) else {
            self.error("This approval is no longer pending.");
            return;
        };
        if !pending.choices.iter().any(|c| c == decision.as_str()) {
            self.error("This decision is not offered by the native request.");
            return;
        }
        let key = key.clone();
        self.resolve_approval(&key, decision, &answers.unwrap_or_default());
    }
    fn resolve_native(&mut self, key: &str) {
        if let Some(pending) = self.approvals.shift_remove(key) {
            self.emit(json!({"type":"approval_resolved","id":pending.id}));
        }
    }
    fn clear_approvals(&mut self, send_cancellation: bool) {
        let keys: Vec<String> = self.approvals.keys().cloned().collect();
        for key in keys {
            if send_cancellation {
                let cancel = self
                    .approvals
                    .get(&key)
                    .is_some_and(|p| p.choices.iter().any(|c| c == "cancel"));
                self.resolve_approval(
                    &key,
                    if cancel {
                        Decision::Cancel
                    } else {
                        Decision::Decline
                    },
                    &Map::new(),
                );
            } else {
                self.resolve_native(&key);
            }
        }
    }
    fn shutdown(&mut self) {
        if self.closed {
            return;
        }
        self.closed = true;
        self.clear_approvals(true);
        self.finish("interrupted");
        self.close_port();
    }
    fn fatal(&mut self, message: &str) {
        if self.closed {
            return;
        }
        self.error(message);
        self.finish("failed");
        self.shutdown();
    }

    // -- chat.mjs: what to do with each command --

    fn start(&mut self, init: ChatInit) {
        if !valid_init(&init) {
            self.raw_error("Invalid native chat initialization.");
            self.shutdown();
            return;
        }
        match std::fs::metadata(&init.cwd) {
            Ok(meta) if meta.is_dir() => {}
            Ok(_) => {
                self.raw_error("Chat workspace is not a directory.");
                self.shutdown();
                return;
            }
            Err(_) => {
                let message = format!("ENOENT: no such file or directory, stat '{}'", init.cwd);
                self.raw_error(&message);
                self.shutdown();
                return;
            }
        }
        let launch = match codex_launch(&init) {
            Ok(launch) => launch,
            Err(message) => {
                self.raw_error(&message);
                self.shutdown();
                return;
            }
        };
        self.cwd = init.cwd.clone();
        self.program = init.program.clone();
        let args = launch.args.clone();
        self.launch = Some(launch);
        self.initializing = true;
        if self.spawn(&args).is_err() {
            // A process that never started fails as the bridge's does: its
            // first request is already waiting when the launch error arrives,
            // and is failed by the close that follows.
            self.sequence += 1;
            self.pending.insert(
                format!("agentdock-{}", self.sequence),
                Pending {
                    then: Then::Initialize,
                    deadline: Instant::now() + RPC_TIMEOUT,
                    optional: false,
                    interrupt: false,
                },
            );
            self.failure("Could not launch the configured native client.");
            return;
        }
        self.rpc(
            "initialize",
            json!({"clientInfo":{"name":"agentdock","title":"AgentDock","version":"0.1.0"}}),
            Then::Initialize,
            false,
        );
    }
    fn command(&mut self, input: Input) {
        if self.closed {
            return;
        }
        if self.initializing {
            self.raw_error("Chat initialization is still in progress.");
            return;
        }
        if !self.initialized {
            self.raw_error("Invalid native chat initialization.");
            self.shutdown();
            return;
        }
        match input {
            Input::Message(id, content) => self.message(id, content),
            Input::Steer(id, content) => self.steer(id, content),
            Input::Approve(request_id, decision, answers) => {
                self.answer(&request_id, decision, answers)
            }
            Input::Interrupt => self.interrupt(),
            Input::Model(model, effort) => self.select_model(model, effort),
            Input::Permission(mode) => self.set_permission_mode(&mode),
            _ => {}
        }
    }

    // -- CodexChat --

    fn select_model(&mut self, model: Option<String>, effort: Option<String>) {
        if self.active.is_some() {
            self.raw_error("Wait for the current turn to finish before changing the model.");
            return;
        }
        if let Some(model) = model.filter(|m| !m.is_empty())
            && let Some(launch) = self.launch.as_mut()
        {
            launch.thread.insert("model".into(), json!(model));
        }
        if let Some(effort) = effort.filter(|e| !e.is_empty()) {
            self.effort = Some(effort);
        }
        self.restate_settings();
    }
    fn set_permission_mode(&mut self, mode: &str) {
        if self.active.is_some() {
            self.raw_error("Wait for the current turn to finish before changing permissions.");
            return;
        }
        let (policy, sandbox) = match mode {
            "ask" => ("on-request", None),
            "danger" => ("never", Some(json!({"type":"dangerFullAccess"}))),
            _ => {
                self.raw_error(
                    "Codex approves each command rather than switching between review modes.",
                );
                return;
            }
        };
        self.approval_policy = Some(policy.into());
        // A turn's sandbox stays the thread's for the turns after it, so
        // leaving `danger` has to name the sandbox to return to.
        self.sandbox_policy =
            sandbox.or_else(|| self.sandbox_overridden.then(|| self.return_sandbox()));
        self.permission_mode = Some(mode.into());
        self.restate_settings();
    }
    fn return_sandbox(&self) -> Value {
        let chosen = self
            .launch
            .as_ref()
            .is_some_and(|l| l.args.iter().any(|arg| arg.starts_with("sandbox_mode=")));
        match &self.base_sandbox {
            Some(base)
                if base.get("type").and_then(Value::as_str) != Some("dangerFullAccess")
                    || chosen =>
            {
                base.clone()
            }
            _ => json!({"type":"workspaceWrite"}),
        }
    }
    fn message(&mut self, id: String, content: String) {
        if let Some(turn) = self.begin(id, content.clone()) {
            self.run(turn, content);
        }
    }
    fn run(&mut self, turn: usize, content: String) {
        if self.native_session_id.is_none() {
            let launch = self.launch.clone().expect("launched");
            let mut params = launch.thread.clone();
            let method = match &launch.resume {
                Some(resume) => {
                    params.insert("threadId".into(), json!(resume));
                    "thread/resume"
                }
                None => "thread/start",
            };
            self.rpc(
                method,
                Value::Object(params),
                Then::OpenThread { turn, content },
                false,
            );
            return;
        }
        self.start_turn(turn, content);
    }
    fn start_turn(&mut self, turn: usize, content: String) {
        if self.active != Some(turn) {
            return;
        }
        if self.turns[turn].interrupted {
            self.finish("interrupted");
            return;
        }
        if self.models.is_some() {
            self.restate_settings();
        }
        if self.sandbox_policy.is_some() {
            self.sandbox_overridden = true;
        }
        let mut params = Map::new();
        params.insert("threadId".into(), json!(self.native_session_id));
        params.insert("input".into(), json!([{"type":"text","text":content}]));
        if let Some(model) = self
            .launch
            .as_ref()
            .and_then(|l| l.thread.get("model"))
            .cloned()
        {
            params.insert("model".into(), model);
        }
        if let Some(effort) = &self.effort {
            params.insert("effort".into(), json!(effort));
        }
        if let Some(policy) = &self.approval_policy {
            params.insert("approvalPolicy".into(), json!(policy));
        }
        if let Some(sandbox) = &self.sandbox_policy {
            params.insert("sandboxPolicy".into(), sandbox.clone());
        }
        self.rpc(
            "turn/start",
            Value::Object(params),
            Then::StartTurn { turn },
            false,
        );
    }
    /// The catch around `run`: a failed request ends the turn it was for.
    fn turn_failed(&mut self, turn: usize, message: &str) {
        if self.active == Some(turn) {
            self.error(message);
            let status = if self.turns[turn].interrupted {
                "interrupted"
            } else {
                "failed"
            };
            self.finish(status);
        }
    }
    fn steer(&mut self, id: String, content: String) {
        let Some(turn) = self.active else {
            self.message(id, content);
            return;
        };
        if !self.accept_steer(&id, &content) {
            return;
        }
        let message = Written { id, content };
        if self.turns[turn].native_turn.is_none() {
            self.turns[turn].early.push(message);
            return;
        }
        self.send_steer(turn, message, Vec::new());
    }
    fn send_steer(&mut self, turn: usize, message: Written, then: Vec<Written>) {
        let params = json!({
            "threadId": self.native_session_id,
            "expectedTurnId": self.turns[turn].native_turn,
            "input": [{"type":"text","text":message.content}],
        });
        self.rpc(
            "turn/steer",
            params,
            Then::Steer {
                turn,
                message,
                then,
            },
            false,
        );
    }
    fn continue_deferred(&mut self) {
        if self.active.is_some() || self.deferred.is_empty() {
            return;
        }
        let list: Vec<Written> = std::mem::take(&mut self.deferred);
        let id = list[0].id.clone();
        let turn = self.new_turn(id.clone());
        self.emit(json!({"type":"turn","id":id,"status":"running"}));
        let content = list
            .iter()
            .map(|m| m.content.as_str())
            .collect::<Vec<_>>()
            .join("\n\n");
        self.run(turn, content);
    }
    fn send_interrupt(&mut self, turn: usize) {
        let state = &mut self.turns[turn];
        if !truthy(state.native_turn.as_ref()) || state.interrupt_sent {
            return;
        }
        state.interrupt_sent = true;
        let params = json!({"threadId": self.native_session_id, "turnId": state.native_turn});
        self.rpc("turn/interrupt", params, Then::Interrupt { turn }, false);
    }
    fn interrupt(&mut self) {
        let Some(turn) = self.active else {
            return;
        };
        self.turns[turn].interrupted = true;
        self.deferred.clear();
        self.clear_approvals(true);
        self.send_interrupt(turn);
    }

    /// What follows a request's answer.
    fn settle(&mut self, then: Then, result: Result<Value, String>) {
        match then {
            Then::Initialize => match result {
                Ok(_) => {
                    if let Err(message) = self.send(json!({"method":"initialized","params":{}})) {
                        self.initializing = false;
                        self.raw_error(message);
                        self.shutdown();
                        return;
                    }
                    self.announce(None);
                    let model = self
                        .launch
                        .as_ref()
                        .and_then(|l| l.thread.get("model"))
                        .and_then(Value::as_str)
                        .map(str::to_owned);
                    self.settings(None, model.as_deref(), None);
                    self.initializing = false;
                    self.initialized = true;
                    // Asking for models must not hold up the session.
                    self.rpc(
                        "model/list",
                        json!({"limit":100,"includeHidden":false}),
                        Then::ModelList,
                        true,
                    );
                }
                Err(message) => {
                    self.initializing = false;
                    self.raw_error(&message);
                    self.shutdown();
                }
            },
            Then::ModelList => {
                if let Ok(result) = result {
                    self.models = codex_models(&result);
                    self.default_model = self.models.as_ref().and_then(|models| {
                        models
                            .iter()
                            .find(|m| m.get("isDefault") == Some(&Value::Bool(true)))
                            .and_then(|m| m.get("id"))
                            .and_then(Value::as_str)
                            .map(str::to_owned)
                    });
                    self.restate_settings();
                }
            }
            Then::OpenThread { turn, content } => {
                let result = match result {
                    Ok(result) => result,
                    Err(message) => return self.turn_failed(turn, &message),
                };
                let Some(id) = result
                    .get("thread")
                    .and_then(|t| t.get("id"))
                    .and_then(Value::as_str)
                    .filter(|id| native_id(id))
                    .map(str::to_owned)
                else {
                    return self
                        .turn_failed(turn, "Codex did not return a usable native thread ID.");
                };
                if self.base_sandbox.is_none()
                    && let Some(sandbox) = result
                        .get("sandbox")
                        .filter(|s| s.is_object() || s.is_array())
                {
                    self.base_sandbox = Some(sandbox.clone());
                }
                self.announce(Some(&id));
                self.start_turn(turn, content);
            }
            Then::StartTurn { turn } => match result {
                Ok(result) => {
                    if self.active != Some(turn) {
                        return;
                    }
                    self.turns[turn].native_turn =
                        result.get("turn").and_then(|t| t.get("id")).cloned();
                    if self.turns[turn].interrupted {
                        self.send_interrupt(turn);
                    } else {
                        // Steering written before the turn had an id waited for it.
                        let mut early = std::mem::take(&mut self.turns[turn].early);
                        if !early.is_empty() {
                            let first = early.remove(0);
                            self.send_steer(turn, first, early);
                        }
                    }
                }
                Err(message) => self.turn_failed(turn, &message),
            },
            Then::Steer {
                turn,
                message,
                then,
            } => {
                if result.is_err() && !self.turns[turn].interrupted {
                    // A turn that would not take it leaves it for the next one.
                    self.deferred.push(message);
                    if self.active != Some(turn) {
                        self.continue_deferred();
                    }
                }
                let mut rest = then;
                if !rest.is_empty() {
                    let next = rest.remove(0);
                    self.send_steer(turn, next, rest);
                }
            }
            Then::Interrupt { turn } => {
                if result.is_err() && self.active == Some(turn) {
                    self.turns[turn].interrupt_sent = false;
                    self.error("Codex could not confirm turn interruption.");
                }
            }
        }
    }

    /// A message Codex wrote: an answer, a request or a notification.
    fn native(&mut self, message: Value) {
        if self.port_closed {
            return;
        }
        if message.get("method").is_none_or(|m| !truthy(Some(m)))
            && let Some(id) = message.get("id")
        {
            let key = match id {
                Value::String(s) => Some(s.clone()),
                _ => None,
            };
            if let Some(pending) = key.and_then(|key| self.pending.remove(&key)) {
                let result = match message.get("error") {
                    Some(error) if truthy(Some(error)) => {
                        let stated = error
                            .get("message")
                            .filter(|m| !m.is_null())
                            .unwrap_or(error);
                        let detail = stated.as_str().map(str::trim).filter(|d| !d.is_empty());
                        Err(match detail {
                            Some(detail) => format!("Codex refused: {}", clip(detail, 400)),
                            None => "Native client rejected a control request. Check client settings and version.".into(),
                        })
                    }
                    _ => Ok(message.get("result").cloned().unwrap_or(Value::Null)),
                };
                self.settle(pending.then, result);
                return;
            }
        }
        self.notification(message);
    }

    fn notification(&mut self, message: Value) {
        if !message.is_object() {
            return;
        }
        if truthy(message.get("method")) && message.get("id").is_some() {
            self.request(message);
            return;
        }
        let empty = Value::Object(Map::new());
        let p = message
            .get("params")
            .filter(|v| !v.is_null())
            .unwrap_or(&empty)
            .clone();
        let method = message.get("method").and_then(Value::as_str).unwrap_or("");
        if truthy(p.get("threadId"))
            && let Some(current) = &self.native_session_id
            && p.get("threadId") != Some(&json!(current))
        {
            return;
        }
        match method {
            "serverRequest/resolved" => {
                let key = request_key(p.get("requestId").unwrap_or(&Value::Null));
                self.resolve_native(&key);
                return;
            }
            "thread/tokenUsage/updated" => {
                let usage = p.get("tokenUsage");
                let last = usage.and_then(|u| u.get("last")).filter(|v| !v.is_null());
                let chosen = last.or_else(|| usage.and_then(|u| u.get("total")));
                let context = last.and_then(|l| l.get("totalTokens")).cloned();
                let window = usage.and_then(|u| u.get("modelContextWindow")).cloned();
                self.usage(chosen.cloned().as_ref(), context.as_ref(), window.as_ref());
                return;
            }
            "error" => {
                self.error("Codex reported a native turn error. Check its account, endpoint and permission settings.");
                return;
            }
            _ => {}
        }
        let Some(turn) = self.active else {
            return;
        };
        if truthy(p.get("turnId"))
            && truthy(self.turns[turn].native_turn.as_ref())
            && p.get("turnId") != self.turns[turn].native_turn.as_ref()
        {
            return;
        }
        match method {
            "turn/started" => {
                self.turns[turn].native_turn = p.get("turn").and_then(|t| t.get("id")).cloned();
                if self.turns[turn].interrupted {
                    self.send_interrupt(turn);
                }
            }
            "turn/completed" => {
                let status = match p
                    .get("turn")
                    .and_then(|t| t.get("status"))
                    .and_then(Value::as_str)
                {
                    Some("interrupted") => "interrupted",
                    Some("failed") => "failed",
                    _ => "completed",
                };
                self.finish(status);
                if status == "interrupted" {
                    self.deferred.clear();
                } else {
                    self.continue_deferred();
                }
            }
            "item/agentMessage/delta" | "item/plan/delta" => {
                if let (Some(id), Some(delta)) = (
                    p.get("itemId").and_then(Value::as_str),
                    p.get("delta").and_then(Value::as_str),
                ) {
                    let text = clip(delta, MAX_TEXT);
                    self.emit(json!({"type":"message","id":id,"role":"assistant","text":text,"delta":true}));
                }
            }
            "item/commandExecution/outputDelta" => {
                let Some(id) = p.get("itemId").and_then(Value::as_str).map(str::to_owned) else {
                    return;
                };
                let (name, text) = self
                    .tools
                    .get(&id)
                    .cloned()
                    .unwrap_or_else(|| ("Command".into(), String::new()));
                let delta = p.get("delta").and_then(Value::as_str).unwrap_or("");
                let text = clip(&format!("{text}{delta}"), MAX_TEXT);
                self.tools.insert(id.clone(), (name.clone(), text.clone()));
                self.emit(
                    json!({"type":"tool","id":id,"name":name,"status":"running","text":text}),
                );
            }
            "item/started" | "item/completed" => self.item(method, &p),
            _ => {}
        }
    }

    fn item(&mut self, method: &str, p: &Value) {
        let Some(item) = p.get("item") else {
            return;
        };
        let Some(id) = item.get("id").and_then(Value::as_str).map(str::to_owned) else {
            return;
        };
        let kind = item.get("type").and_then(Value::as_str);
        if matches!(kind, Some("agentMessage" | "plan")) {
            if let Some(text) = item.get("text").and_then(Value::as_str) {
                let text = clip(text, MAX_TEXT);
                self.emit(
                    json!({"type":"message","id":id,"role":"assistant","text":text,"delta":false}),
                );
            }
            return;
        }
        if matches!(kind, Some("userMessage" | "reasoning")) {
            return;
        }
        let mut name = [item.get("tool"), item.get("type")]
            .into_iter()
            .find(|v| truthy(*v))
            .flatten()
            .cloned()
            .unwrap_or_else(|| json!("Native tool"));
        let text = match kind {
            Some("commandExecution") => {
                name = json!("Command");
                [item.get("command"), item.get("aggregatedOutput")]
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join("\n")
            }
            Some("fileChange") => {
                name = json!("File changes");
                let changes = match item.get("changes") {
                    None | Some(Value::Null) => Vec::new(),
                    Some(Value::Array(changes)) => changes.clone(),
                    // The bridge's `.map` throws on anything else, and a throw
                    // in a handler breaks the stream.
                    Some(_) => return self.failure("Invalid structured message received."),
                };
                let mut parts = Vec::new();
                for change in &changes {
                    if !change.is_object() {
                        return self.failure("Invalid structured message received.");
                    }
                    let part = |key: &str| {
                        change
                            .get(key)
                            .filter(|v| !v.is_null())
                            .map(js_string)
                            .unwrap_or_default()
                    };
                    parts.push(format!("{}\n{}", part("path"), part("diff")));
                }
                parts.join("\n")
            }
            Some("mcpToolCall") => {
                name = json!(
                    [item.get("server"), item.get("tool")]
                        .into_iter()
                        .filter(|v| truthy(*v))
                        .flatten()
                        .map(js_string)
                        .collect::<Vec<_>>()
                        .join(" / ")
                );
                stringify(
                    item.get("arguments")
                        .filter(|v| !v.is_null())
                        .unwrap_or(&json!({})),
                )
            }
            _ => item
                .get("query")
                .and_then(Value::as_str)
                .or_else(|| item.get("path").and_then(Value::as_str))
                .unwrap_or("")
                .to_owned(),
        };
        let text = clip(&text, MAX_TEXT);
        let name = clip_value(Some(&name), 256);
        self.tools.insert(id.clone(), (name.clone(), text.clone()));
        if self.tools.len() > 512 {
            self.tools.shift_remove_index(0);
        }
        let status = if method == "item/started" {
            "running"
        } else {
            item_status(item)
        };
        self.emit(json!({"type":"tool","id":id,"name":name,"status":status,"text":text}));
    }

    /// `reject`: a request AgentDock cannot answer, refused as unsupported.
    fn reject(&mut self, message: &Value, reason: &str) {
        let id = message.get("id").cloned().unwrap_or(Value::Null);
        if self
            .send(json!({"id":id,"error":{"code":-32601,"message":reason}}))
            .is_err()
        {
            // The bridge's `send` throws here, inside a handler: the stream breaks.
            return self.failure("Invalid structured message received.");
        }
        self.error(reason);
    }

    fn request(&mut self, message: Value) {
        const UNSUPPORTED: &str = "This native request is not supported by AgentDock.";
        let empty = Value::Object(Map::new());
        let p = message
            .get("params")
            .filter(|v| !v.is_null())
            .unwrap_or(&empty)
            .clone();
        let method = message
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        let id = message.get("id").cloned().unwrap_or(Value::Null);
        let valid = match &id {
            Value::String(s) => !s.is_empty() && s.encode_utf16().count() <= 256,
            Value::Number(n) => {
                n.is_i64()
                    || n.is_u64()
                    || n.as_f64()
                        .is_some_and(|f| f.fract() == 0.0 && f.abs() <= 9_007_199_254_740_991.0)
            }
            _ => false,
        };
        if !valid {
            return self.fatal("Invalid native request identifier.");
        }
        let key = request_key(&id);
        if self.active.is_none()
            || (truthy(p.get("threadId"))
                && p.get("threadId") != self.native_session_id.as_ref().map(|s| json!(s)).as_ref())
        {
            return self.reject(
                &message,
                "Native request does not belong to the active turn.",
            );
        }
        let cwd = self.cwd.clone();
        match method.as_str() {
            "item/commandExecution/requestApproval" | "item/fileChange/requestApproval" => {
                let mut body = Map::new();
                for (key, field) in [
                    ("reason", "reason"),
                    ("command", "command"),
                    ("cwd", "cwd"),
                    ("grantRoot", "grantRoot"),
                    ("network", "networkApprovalContext"),
                    ("additionalPermissions", "additionalPermissions"),
                ] {
                    if let Some(value) = p.get(field) {
                        body.insert(key.into(), value.clone());
                    }
                }
                let text = stringify(&Value::Object(body));
                if text.len() > MAX_TEXT {
                    return self
                        .reject(&message, "Native approval is too large to display safely.");
                }
                let offered: Vec<String> =
                    match p.get("availableDecisions").and_then(Value::as_array) {
                        Some(list) => list
                            .iter()
                            .filter_map(Value::as_str)
                            .map(str::to_owned)
                            .collect(),
                        None => vec!["accept".into(), "decline".into(), "cancel".into()],
                    };
                let choices: Vec<&str> = [
                    ("accept", "accept"),
                    ("accept_session", "acceptForSession"),
                    ("decline", "decline"),
                    ("cancel", "cancel"),
                ]
                .into_iter()
                .filter(|(_, native)| offered.iter().any(|o| o == native))
                .map(|(choice, _)| choice)
                .collect();
                if !choices.contains(&"decline") && !choices.contains(&"cancel") {
                    return self.reject(&message, UNSUPPORTED);
                }
                let command = method.contains("commandExecution");
                let access = if truthy(p.get("networkApprovalContext")) {
                    "network"
                } else if command {
                    "execute"
                } else {
                    "write"
                };
                let mut paths = vec![
                    p.get("grantRoot").cloned().unwrap_or(Value::Null),
                    p.get("cwd").cloned().unwrap_or(Value::Null),
                ];
                if let Some(extra) = p.get("additionalPermissions") {
                    paths_in(extra, &mut paths, 0);
                }
                // `Array.isArray(p.command) ? p.command.join(' ') : p.command`.
                let shown = match p.get("command") {
                    Some(Value::Array(parts)) => Some(json!(
                        parts
                            .iter()
                            .map(|part| match part {
                                Value::Null => String::new(),
                                other => js_string(other),
                            })
                            .collect::<Vec<_>>()
                            .join(" ")
                    )),
                    other => other.cloned(),
                };
                let scope = approval_scope(&cwd, Some(access), &paths, shown.as_ref());
                let mut description = Map::new();
                description.insert(
                    "title".into(),
                    json!(if command {
                        "Approve native command"
                    } else {
                        "Approve native file changes"
                    }),
                );
                description.insert("text".into(), json!(clip(&text, MAX_TEXT)));
                description.insert("choices".into(), json!(choices));
                if let Some(scope) = scope {
                    description.insert("scope".into(), scope);
                }
                self.approval(key, description, Responder::Decision { id });
            }
            "item/tool/requestUserInput" => {
                let questions: Vec<Value> = p
                    .get("questions")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                let mut ids: Vec<&Value> = Vec::new();
                for q in &questions {
                    let qid = q.get("id").unwrap_or(&Value::Null);
                    if !ids.contains(&qid) {
                        ids.push(qid);
                    }
                }
                let invalid = questions.is_empty()
                    || questions.len() > 16
                    || stringify(&Value::Array(questions.clone())).len() > MAX_TEXT
                    || ids.len() != questions.len()
                    || questions.iter().any(|q| {
                        let options = q.get("options").filter(|o| !o.is_null());
                        !q.get("id")
                            .and_then(Value::as_str)
                            .is_some_and(|id| id.encode_utf16().count() <= 128)
                            || !q.get("question").is_some_and(Value::is_string)
                            || options.is_some_and(|options| {
                                !options.as_array().is_some_and(|options| {
                                    options.len() <= 32
                                        && options
                                            .iter()
                                            .all(|o| o.get("label").is_some_and(Value::is_string))
                                })
                            })
                    });
                if invalid {
                    return self.reject(&message, UNSUPPORTED);
                }
                let shown: Vec<Value> = questions
                    .iter()
                    .map(|q| {
                        let options: Vec<Value> = q
                            .get("options")
                            .and_then(Value::as_array)
                            .map(|options| {
                                options
                                    .iter()
                                    .map(|o| json!({"label":clip_value(o.get("label"),256),"description":clip_value(o.get("description").filter(|d| !d.is_null()).or(Some(&json!(""))),2048)}))
                                    .collect()
                            })
                            .unwrap_or_default();
                        json!({
                            "id": q.get("id"),
                            "header": clip_value(q.get("header").filter(|h| !h.is_null()).or(Some(&json!(""))), 128),
                            "question": clip_value(q.get("question"), MAX_TEXT),
                            "options": options,
                            "isSecret": truthy(q.get("isSecret")),
                            "isOther": truthy(q.get("isOther")),
                            "multiSelect": truthy(q.get("multiSelect")),
                        })
                    })
                    .collect();
                let mut description = Map::new();
                description.insert("title".into(), json!("Native client needs your input"));
                description.insert("text".into(), json!("Answer the native client questions."));
                description.insert("choices".into(), json!(["accept", "decline", "cancel"]));
                description.insert("questions".into(), Value::Array(shown));
                self.approval(key, description, Responder::Questions { id, questions });
            }
            "item/permissions/requestApproval" => {
                let Some(permissions) = p
                    .get("permissions")
                    .filter(|v| truthy(Some(v)) && (v.is_object() || v.is_array()))
                    .cloned()
                else {
                    return self.reject(&message, UNSUPPORTED);
                };
                let encoded = stringify(&permissions);
                if encoded.len() > MAX_TEXT {
                    return self.reject(&message, UNSUPPORTED);
                }
                let mut paths = Vec::new();
                paths_in(&permissions, &mut paths, 0);
                let access = if encoded.contains("write") {
                    "write"
                } else if truthy(permissions.get("network")) {
                    "network"
                } else {
                    "read"
                };
                let scope = approval_scope(&cwd, Some(access), &paths, None);
                let mut body = Map::new();
                if let Some(reason) = p.get("reason") {
                    body.insert("reason".into(), reason.clone());
                }
                body.insert("permissions".into(), permissions.clone());
                let mut description = Map::new();
                description.insert("title".into(), json!("Approve native permission request"));
                description.insert(
                    "text".into(),
                    json!(clip(&stringify(&Value::Object(body)), MAX_TEXT)),
                );
                description.insert(
                    "choices".into(),
                    json!(["accept", "accept_session", "decline", "cancel"]),
                );
                if let Some(scope) = scope {
                    description.insert("scope".into(), scope);
                }
                self.approval(key, description, Responder::Permissions { id, permissions });
            }
            // Codex asks before an MCP tool runs, as an MCP elicitation. AgentDock's
            // own tools already ask the person in AgentDock for anything that
            // matters, so those pass; any other server's call is the person's.
            "mcpServer/elicitation/request" => {
                let fields = p
                    .get("requestedSchema")
                    .filter(|s| s.is_object() || s.is_array())
                    .and_then(|s| s.get("properties"))
                    .filter(|props| truthy(Some(props)) && (props.is_object() || props.is_array()))
                    .map(|props| match props {
                        Value::Object(o) => o.len(),
                        Value::Array(a) => a.len(),
                        _ => 0,
                    })
                    .unwrap_or(0);
                let mode = p.get("mode").and_then(Value::as_str);
                if !matches!(mode, Some("form" | "openai/form" | "openaiForm")) || fields > 0 {
                    return self.reject(
                        &message,
                        "This MCP server asked for a form AgentDock cannot show.",
                    );
                }
                if p.get("serverName") == Some(&json!("agentdock")) {
                    if self
                        .send(json!({"id":id,"result":{"action":"accept","content":{}}}))
                        .is_err()
                    {
                        self.failure("Invalid structured message received.");
                    }
                    return;
                }
                let mut body = Map::new();
                if let Some(server) = p.get("serverName") {
                    body.insert("server".into(), server.clone());
                }
                if let Some(text) = p.get("message") {
                    body.insert("message".into(), text.clone());
                }
                let text = stringify(&Value::Object(body));
                if text.len() > MAX_TEXT {
                    return self
                        .reject(&message, "Native approval is too large to display safely.");
                }
                let mut description = Map::new();
                description.insert("title".into(), json!("Approve MCP tool call"));
                description.insert("text".into(), json!(clip(&text, MAX_TEXT)));
                description.insert("choices".into(), json!(["accept", "decline", "cancel"]));
                self.approval(key, description, Responder::Elicitation { id });
            }
            _ => self.reject(&message, UNSUPPORTED),
        }
    }

    /// Answer a native request; an `Err` is the bridge's throw inside `answer`.
    fn respond(
        &mut self,
        responder: &Responder,
        decision: Decision,
        answers: &Answers,
    ) -> Result<(), ()> {
        let cancel = decision == Decision::Cancel;
        match responder {
            Responder::Decision { id } => {
                let native = if decision == Decision::AcceptSession {
                    "acceptForSession"
                } else {
                    decision.as_str()
                };
                self.send(json!({"id":id,"result":{"decision":native}}))
                    .map_err(|_| ())
            }
            Responder::Questions { id, questions } => {
                if decision == Decision::Accept {
                    let invalid = questions.iter().any(|q| {
                        let given = q
                            .get("id")
                            .and_then(Value::as_str)
                            .and_then(|id| answers.get(id))
                            .and_then(Value::as_array);
                        let Some(given) = given.filter(|g| !g.is_empty()) else {
                            return true;
                        };
                        let options = q.get("options").and_then(Value::as_array);
                        (!truthy(q.get("multiSelect")) && given.len() > 1)
                            || (!truthy(q.get("isOther"))
                                && !truthy(q.get("isSecret"))
                                && options.is_some_and(|o| !o.is_empty())
                                && given.iter().any(|answer| {
                                    !options.is_some_and(|o| {
                                        o.iter().any(|option| option.get("label") == Some(answer))
                                    })
                                }))
                    });
                    if invalid {
                        return Err(());
                    }
                }
                let mut values = Map::new();
                if decision == Decision::Accept {
                    for q in questions {
                        if let Some(qid) = q.get("id").and_then(Value::as_str) {
                            values.insert(qid.to_owned(), json!({"answers": answers.get(qid)}));
                        }
                    }
                }
                self.send(json!({"id":id,"result":{"answers":values}}))
                    .map_err(|_| ())?;
                self.cancel_turn(cancel);
                Ok(())
            }
            Responder::Permissions { id, permissions } => {
                let granted = matches!(decision, Decision::Accept | Decision::AcceptSession);
                let scope = if decision == Decision::AcceptSession {
                    "session"
                } else {
                    "turn"
                };
                let given = if granted {
                    permissions.clone()
                } else {
                    json!({})
                };
                self.send(json!({"id":id,"result":{"permissions":given,"scope":scope}}))
                    .map_err(|_| ())?;
                self.cancel_turn(cancel);
                Ok(())
            }
            Responder::Elicitation { id } => {
                let content = if decision == Decision::Accept {
                    json!({})
                } else {
                    Value::Null
                };
                self.send(json!({"id":id,"result":{"action":decision.as_str(),"content":content}}))
                    .map_err(|_| ())?;
                self.cancel_turn(cancel);
                Ok(())
            }
        }
    }
    /// A cancelled question or permission stops the turn it belongs to.
    fn cancel_turn(&mut self, cancel: bool) {
        if cancel && let Some(turn) = self.active {
            self.turns[turn].interrupted = true;
            self.send_interrupt(turn);
        }
    }

    // -- timers --

    fn next_deadline(&self) -> Option<Instant> {
        self.pending
            .values()
            .map(|p| p.deadline)
            .chain(self.approvals.values().map(|a| a.deadline))
            .min()
    }
    fn expire(&mut self, now: Instant) {
        let expired: Vec<String> = self
            .pending
            .iter()
            .filter(|(_, p)| p.deadline <= now)
            .map(|(id, _)| id.clone())
            .collect();
        for id in expired {
            let Some(pending) = self.pending.remove(&id) else {
                continue;
            };
            // A timed-out start may have executed: the connection is not reused.
            if !pending.optional && !pending.interrupt {
                self.failure(
                    "Native control request timed out; the structured connection was stopped.",
                );
            }
            self.settle(
                pending.then,
                Err("Native control request timed out.".into()),
            );
        }
        let expired: Vec<String> = self
            .approvals
            .iter()
            .filter(|(_, a)| a.deadline <= now)
            .map(|(key, _)| key.clone())
            .collect();
        for key in expired {
            let cancel = self
                .approvals
                .get(&key)
                .is_some_and(|a| a.choices.iter().any(|c| c == "cancel"));
            self.resolve_approval(
                &key,
                if cancel {
                    Decision::Cancel
                } else {
                    Decision::Decline
                },
                &Map::new(),
            );
            self.error("Native approval timed out and was cancelled.");
        }
    }

    /// Stop Codex and everything it started, as `NativeProcess.close` did.
    async fn stop_process(&mut self) {
        self.writer = None;
        let Some(mut child) = self.child.take() else {
            return;
        };
        self.group.terminate();
        if tokio::time::timeout(Duration::from_millis(1500), child.wait())
            .await
            .is_err()
        {
            self.group.kill();
            let _ = tokio::time::timeout(Duration::from_secs(1), child.wait()).await;
        }
        self.group.finish();
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// `jsonLines` over Codex's stdout: bounded UTF-8 JSONL, with a deadline for a
/// line left incomplete.
async fn read_lines(mut stdout: tokio::process::ChildStdout, inputs: mpsc::UnboundedSender<Input>) {
    let mut buffer: Vec<u8> = Vec::new();
    let mut total = 0usize;
    let mut bytes = vec![0u8; 64 * 1024];
    loop {
        let read = if buffer.is_empty() {
            stdout.read(&mut bytes).await
        } else {
            match tokio::time::timeout(LINE_TIMEOUT, stdout.read(&mut bytes)).await {
                Ok(read) => read,
                Err(_) => {
                    let _ = inputs.send(Input::Broken("Incomplete structured message timed out."));
                    return;
                }
            }
        };
        let count = match read {
            Ok(0) | Err(_) => {
                if !String::from_utf8_lossy(&buffer).trim().is_empty() {
                    let _ = inputs.send(Input::Broken("Structured stream ended mid-message."));
                }
                let _ = inputs.send(Input::Ended);
                return;
            }
            Ok(count) => count,
        };
        total += count;
        if total > MAX_NATIVE_TOTAL {
            let _ = inputs.send(Input::Broken("Structured stream exceeded its size limit."));
            return;
        }
        buffer.extend_from_slice(&bytes[..count]);
        while let Some(end) = buffer.iter().position(|b| *b == b'\n') {
            let line: Vec<u8> = buffer.drain(..=end).collect();
            let line = String::from_utf8_lossy(&line[..line.len() - 1]).into_owned();
            if line.len() > MAX_NATIVE_LINE {
                let _ = inputs.send(Input::Broken("Structured message exceeded its size limit."));
                return;
            }
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<Value>(&line) {
                Ok(message) => {
                    if inputs.send(Input::Native(message)).is_err() {
                        return;
                    }
                }
                Err(_) => {
                    let _ = inputs.send(Input::Broken("Invalid structured message received."));
                    return;
                }
            }
        }
        if buffer.len() > MAX_NATIVE_LINE {
            let _ = inputs.send(Input::Broken("Structured message exceeded its size limit."));
            return;
        }
    }
}

/// The conversation's task: every input, handled to the end, in order.
async fn run(mut conversation: Conversation, mut inputs: mpsc::UnboundedReceiver<Input>) {
    loop {
        let deadline = conversation.next_deadline();
        let input = tokio::select! {
            input = inputs.recv() => input,
            _ = tokio::time::sleep_until(deadline.unwrap_or_else(Instant::now)), if deadline.is_some() => {
                conversation.expire(Instant::now());
                if conversation.closed { break; }
                continue;
            }
        };
        let Some(input) = input else {
            conversation.shutdown();
            break;
        };
        match input {
            Input::Start(init, launched) => {
                conversation.start(init);
                let _ = launched.send(());
            }
            Input::Shutdown => {
                conversation.shutdown();
                break;
            }
            Input::Native(message) => conversation.native(message),
            Input::Broken(message) => conversation.failure(message),
            Input::Ended => {
                // The app-server's output ended: it is gone, or going.
                conversation.failure("Codex app-server exited.");
            }
            command => conversation.command(command),
        }
        if conversation.closed {
            break;
        }
    }
    conversation.stop_process().await;
    conversation.emit(json!({"type":"exit"}));
    conversation.finished = true;
}

/// Codex in this process: see the module documentation.
pub struct CodexAdapter {
    inputs: mpsc::UnboundedSender<Input>,
    events: mpsc::UnboundedReceiver<Value>,
    pid: Arc<std::sync::Mutex<Option<u32>>>,
    task: Option<tokio::task::JoinHandle<()>>,
    done: bool,
}

impl CodexAdapter {
    /// A conversation with `env` applied over this server's environment and
    /// `env_remove` taken out of it, as the bridge's child process had.
    pub fn new(env: &std::collections::BTreeMap<String, String>, env_remove: &[String]) -> Self {
        let (events_tx, events) = mpsc::unbounded_channel();
        let (inputs, inputs_rx) = mpsc::unbounded_channel();
        let mut environment: IndexMap<String, String> = std::env::vars().collect();
        for (key, value) in env {
            environment.insert(key.clone(), value.clone());
        }
        for key in env_remove
            .iter()
            .cloned()
            .chain(crate::bridge::agentdock_secrets())
        {
            environment.shift_remove(&key);
        }
        let pid = Arc::new(std::sync::Mutex::new(None));
        let conversation = Conversation {
            events: events_tx,
            secrets: secrets_of(environment.into_iter()),
            cwd: String::new(),
            program: String::new(),
            env: env.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
            env_remove: env_remove.to_vec(),
            launch: None,
            inputs: inputs.clone(),
            writer: None,
            queued: Arc::new(AtomicUsize::new(0)),
            group: OwnedGroup::new(None),
            child: None,
            pid: pid.clone(),
            sequence: 0,
            pending: HashMap::new(),
            seen: HashMap::new(),
            approvals: IndexMap::new(),
            turns: Vec::new(),
            active: None,
            deferred: Vec::new(),
            tools: IndexMap::new(),
            ready: false,
            closed: false,
            port_closed: false,
            initializing: false,
            initialized: false,
            finished: false,
            native_session_id: None,
            permission_mode: None,
            models: None,
            default_model: None,
            effort: None,
            approval_policy: None,
            sandbox_policy: None,
            sandbox_overridden: false,
            base_sandbox: None,
        };
        let task = tokio::spawn(run(conversation, inputs_rx));
        Self {
            inputs,
            events,
            pid,
            task: Some(task),
            done: false,
        }
    }
    fn input(&self, input: Input) -> Result<(), Failure> {
        // A conversation that has ended answers nothing; its exit is already
        // on its way, as the bridge's was after its last line.
        let _ = self.inputs.send(input);
        Ok(())
    }
}

impl ClientAdapter for CodexAdapter {
    fn pid(&self) -> Option<u32> {
        *self.pid.lock().expect("codex pid")
    }
    async fn start(&mut self, init: &ChatInit) -> Result<(), Failure> {
        // Waits for the launch, so the process is known before anything asks for it.
        let (launched, tried) = tokio::sync::oneshot::channel();
        self.input(Input::Start(init.clone(), launched))?;
        let _ = tokio::time::timeout(Duration::from_secs(5), tried).await;
        Ok(())
    }
    async fn message(&mut self, id: String, content: String) -> Result<(), Failure> {
        self.input(Input::Message(id, content))
    }
    async fn steer(&mut self, id: String, content: String) -> Result<(), Failure> {
        self.input(Input::Steer(id, content))
    }
    async fn approve(
        &mut self,
        request_id: String,
        decision: Decision,
        answers: Option<Answers>,
    ) -> Result<(), Failure> {
        self.input(Input::Approve(request_id, decision, answers))
    }
    async fn interrupt(&mut self) -> Result<(), Failure> {
        self.input(Input::Interrupt)
    }
    async fn set_model(
        &mut self,
        model: Option<String>,
        effort: Option<String>,
    ) -> Result<(), Failure> {
        self.input(Input::Model(model, effort))
    }
    async fn set_permission_mode(&mut self, mode: String) -> Result<(), Failure> {
        self.input(Input::Permission(mode))
    }
    async fn next_event(&mut self) -> Option<Result<Value, Failure>> {
        if self.done {
            return None;
        }
        let event = self.events.recv().await;
        if event.is_none() {
            self.done = true;
        }
        event.map(Ok)
    }
    async fn shutdown(mut self) {
        let _ = self.inputs.send(Input::Shutdown);
        if let Some(task) = self.task.take() {
            let _ = tokio::time::timeout(Duration::from_secs(6), task).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clipping_matches_the_bridge() {
        assert_eq!(clip("a\u{1b}[31mred\u{1b}[0m\u{7}b", MAX_TEXT), "aredb");
        assert_eq!(clip("tab\tand\nline", MAX_TEXT), "tab\tand\nline");
        // A cut through a multi-byte character leaves the replacement mark.
        assert_eq!(clip("ab🦊", 3), "ab\u{fffd}\n[truncated]");
        assert_eq!(clip("\u{1b}[", MAX_TEXT), "[");
    }

    #[test]
    fn launch_arguments_translate_as_the_bridge_translates_them() {
        let init = |args: &[&str]| ChatInit {
            provider: "codex".into(),
            program: "codex".into(),
            args: args.iter().map(|a| a.to_string()).collect(),
            cwd: "/fixture".into(),
            resume_id: None,
        };
        let launch = codex_launch(&init(&[
            "resume",
            "saved-thread",
            "-c",
            "approval_policy=\"on-request\"",
            "-c",
            "model_reasoning_effort=\"high\"",
            "--model",
            "chosen-model",
            "--no-alt-screen",
        ]))
        .unwrap();
        assert_eq!(
            launch.args,
            [
                "app-server",
                "-c",
                "approval_policy=\"on-request\"",
                "-c",
                "model_reasoning_effort=\"high\""
            ]
        );
        assert_eq!(launch.thread.get("model"), Some(&json!("chosen-model")));
        assert_eq!(launch.resume.as_deref(), Some("saved-thread"));
        assert_eq!(
            codex_launch(&init(&["--full-auto"])).unwrap_err(),
            "Unsupported Codex launch option in structured mode."
        );
        assert_eq!(
            codex_launch(&init(&["-a", "never"])).unwrap().args,
            ["app-server", "-c", "approval_policy=\"never\""]
        );
    }

    #[test]
    fn scopes_report_access_and_what_lies_outside_the_workspace() {
        let scope = approval_scope(
            "/work",
            Some("write"),
            &[
                json!("/work/a"),
                json!("../etc/passwd"),
                json!("/tmp/x"),
                json!("/tmp/x"),
            ],
            None,
        )
        .unwrap();
        assert_eq!(
            scope,
            json!({"access":"write","outside":["/etc/passwd","/tmp/x"]})
        );
        assert_eq!(approval_scope("/work", Some("teleport"), &[], None), None);
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

    #[test]
    fn models_are_listed_as_codex_describes_them() {
        let models = codex_models(&json!({"data":[{"id":"a","displayName":"A","isDefault":true,"supportedReasoningEfforts":[{"reasoningEffort":"low"},"high"]},{"id":"b"},{"id":"c","hidden":true}]})).unwrap();
        assert_eq!(
            models,
            vec![
                json!({"id":"a","name":"A","isDefault":true,"efforts":["low","high"]}),
                json!({"id":"b","name":"b"})
            ]
        );
    }
}
