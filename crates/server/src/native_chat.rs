//! What every structured client driven from this process shares: the Node
//! bridge's `chat-common.mjs` (`ChatBase`, `NativeProcess`, `jsonLines`, `clip`,
//! `redactEvents`) and `chat.mjs`'s command handling, in Rust.
//!
//! A client ([`NativeClient`]) owns a [`Core`] and adds only its protocol: how
//! it starts, what a message from it means, what follows each of its replies
//! and how one of its requests is answered. The core runs as one task (see
//! [`NativeAdapter`]): every input -- a command from the server, a message or a
//! reply from the client, a request timing out, an approval expiring, the
//! client's output ending -- is handled to the end before the next. A request
//! records what follows its answer (`Then`) instead of awaiting it, so no state
//! is held across a wait.
use crate::chat_adapter::{Answers, ChatInit, ClientAdapter, Decision, Failure};
use indexmap::IndexMap;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    path::{Component, Path, PathBuf},
    process::Stdio,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tendly_runtime::process::{self, OwnedGroup};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    sync::mpsc,
    time::Instant,
};

pub const MAX_NATIVE_LINE: usize = 4 * 1024 * 1024;
pub const MAX_TEXT: usize = 64 * 1024;
pub const MAX_EVENT_LINE: usize = 192 * 1024;
/// Events sent and not yet taken, past which a conversation stops (chat.mjs).
const MAX_UNSENT: usize = 4 * 1024 * 1024;
const MAX_NATIVE_TOTAL: usize = 128 * 1024 * 1024;
const LINE_TIMEOUT: Duration = Duration::from_secs(10);
pub const RPC_TIMEOUT: Duration = Duration::from_secs(30);
const PROBE_TIMEOUT: Duration = Duration::from_secs(4);
const APPROVAL_TIMEOUT: Duration = Duration::from_secs(600);
const MAX_DIAGNOSTIC: usize = 16 * 1024 * 1024;
/// What a request still waiting is failed with when the connection closes.
pub const CANCELLED: &str = "Native client closed.";

// --- what the bridge does to every string and event --------------------------

/// `clip`: no terminal escapes or control characters, and at most `limit`
/// bytes, cut where a multi-byte character may be split.
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
pub fn clip_value(value: Option<&Value>, limit: usize) -> String {
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

/// A session's launch arguments, read the way the client's own command line
/// reads them: a flag either carries its value after `=` or takes the next
/// argument. Each client decides what a flag means; this only walks them.
pub struct LaunchArgs<'a> {
    args: &'a [String],
    next: usize,
    client: &'static str,
    empty_values: bool,
}

/// One flag, and the value written into it with `=`, if any.
pub struct LaunchFlag<'a> {
    pub name: &'a str,
    inline: Option<&'a str>,
}

impl<'a> LaunchArgs<'a> {
    /// `client` names the program in errors. Claude Code accepts an empty
    /// value; Codex and Pi treat one as missing.
    pub fn new(args: &'a [String], client: &'static str, empty_values: bool) -> Self {
        Self {
            args,
            next: 0,
            client,
            empty_values,
        }
    }

    pub fn next_flag(&mut self) -> Option<LaunchFlag<'a>> {
        let arg = self.args.get(self.next)?;
        self.next += 1;
        // A leading `=` is part of the name, not a separator.
        Some(match arg.find('=') {
            Some(at) if at > 0 => LaunchFlag {
                name: &arg[..at],
                inline: Some(&arg[at + 1..]),
            },
            _ => LaunchFlag {
                name: arg,
                inline: None,
            },
        })
    }

    /// The flag's value: what follows its `=`, or else the next argument.
    pub fn value(&mut self, flag: &LaunchFlag<'a>) -> Result<String, String> {
        let value = match flag.inline {
            Some(inline) => Some(inline.to_owned()),
            None => {
                let next = self.args.get(self.next).cloned();
                self.next += 1;
                next
            }
        };
        value
            .filter(|value| self.empty_values || !value.is_empty())
            .ok_or_else(|| format!("Missing {} launch option value.", self.client))
    }
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

/// Take a resume ID from the launch arguments, refusing a malformed one or one
/// that disagrees with the ID already known. Returns whether it was accepted.
pub fn take_resume(resume: &mut Option<String>, id: String) -> bool {
    if !native_id(&id) || resume.as_ref().is_some_and(|known| *known != id) {
        return false;
    }
    *resume = Some(id);
    true
}

/// JavaScript truthiness of a JSON value.
pub fn truthy(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().is_some_and(|n| n != 0.0 && !n.is_nan()),
        Some(Value::String(s)) => !s.is_empty(),
        Some(_) => true,
    }
}

/// `String(value)`, for the template strings the bridge builds.
pub fn js_string(value: &Value) -> String {
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
pub fn stringify(value: &Value) -> String {
    serde_json::to_string(value).unwrap_or_default()
}

/// An integer JavaScript would count as `Number.isSafeInteger`.
pub fn safe_integer(value: Option<&Value>) -> Option<i64> {
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

pub fn scrub(value: Value, secrets: &[String]) -> Value {
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
pub fn resolve(root: &Path, value: &str) -> PathBuf {
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

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// `validateChatInput(init, true)`.
pub fn valid_init(init: &ChatInit) -> bool {
    !init.program.is_empty()
        && !init.program.contains('\0')
        && Path::new(&init.cwd).is_absolute()
        && init.args.len() <= 512
        && !init.args.iter().any(|arg| arg.contains('\0'))
        && serde_json::to_string(&init.args).is_ok_and(|s| s.len() <= 64 * 1024)
        && init.resume_id.as_deref().is_none_or(native_id)
}

// --- inputs ------------------------------------------------------------------

/// What reaches a conversation's task.
pub enum Input {
    /// Start the client; the sender learns once the launch has been tried.
    Start(ChatInit, tokio::sync::oneshot::Sender<()>),
    Message(String, String),
    Steer(String, String),
    Approve(String, Decision, Option<Answers>),
    Interrupt,
    Model(Option<String>, Option<String>),
    Permission(String),
    Shutdown,
    /// A message the client wrote, parsed.
    Native(Value),
    /// The connection to the client broke, in the bridge's words.
    Broken(&'static str),
    /// The client's output ended.
    Ended,
}

// --- the core: ChatBase and NativeProcess ------------------------------------

/// The turn a chat is running, with what a client keeps per turn.
pub struct Turn<X> {
    pub id: String,
    pub interrupted: bool,
    pub extra: X,
}

pub struct Pending<T> {
    then: T,
    deadline: Instant,
    /// A capability probe, which may simply never be answered.
    optional: bool,
    /// An interrupt that times out is not a broken connection.
    interrupt: bool,
}

pub struct Approval<R> {
    pub id: String,
    pub responder: R,
    /// When it times out: once, like the bridge's timer. A timeout whose answer
    /// could not be sent leaves the approval open but never fires again.
    deadline: Option<Instant>,
    pub choices: Vec<String>,
}

/// A client's wire envelope for one request.
pub type Frame = fn(&str, &str, Value) -> Value;
/// Which waiting request a message answers, and how: the reply's id, then
/// its result or what the client said in refusing. `None` is not a reply.
pub type Reply = fn(&Value) -> Option<(Value, Result<Value, Value>)>;

/// `jsonRpcRequest`.
pub fn json_rpc_frame(id: &str, method: &str, params: Value) -> Value {
    json!({"id":id,"method":method,"params":params})
}

/// What every client driven from this process shares.
pub struct Core<T, R, X> {
    events: mpsc::UnboundedSender<Value>,
    secrets: Vec<String>,
    env: Vec<(String, String)>,
    env_remove: Vec<String>,
    inputs: mpsc::UnboundedSender<Input>,
    writer: Option<mpsc::UnboundedSender<Vec<u8>>>,
    queued: Arc<AtomicUsize>,
    group: OwnedGroup,
    child: Option<tokio::process::Child>,
    pid: Arc<std::sync::Mutex<Option<u32>>>,
    sequence: u64,
    pending: HashMap<String, Pending<T>>,
    frame: Frame,
    reply: Reply,
    /// The client's name in a refusal: "Codex refused: …".
    client_name: &'static str,
    seen: HashMap<String, String>,
    pub approvals: IndexMap<String, Approval<R>>,
    pub turns: Vec<Turn<X>>,
    pub active: Option<usize>,
    pub cwd: String,
    pub ready: bool,
    pub closed: bool,
    pub port_closed: bool,
    pub initializing: bool,
    pub initialized: bool,
    finished: bool,
    /// An event too long for the wire: the bridge's `emit` called `fatal`.
    oversized: bool,
    /// More than 4 MB of events sent and not yet taken: the conversation stops.
    pub(crate) backlogged: bool,
    /// Bytes of events sent and not yet taken by the adapter.
    unsent: Arc<AtomicUsize>,
    pub native_session_id: Option<String>,
    pub permission_mode: Option<String>,
    /// The approval modes this client offers, reported with its settings.
    pub permission_modes: Option<&'static [&'static str]>,
}

impl<T, R, X: Default> Core<T, R, X> {
    /// A conversation not yet started: `env` applied over this server's
    /// environment and `env_remove` taken out of it, as the bridge's child
    /// process had.
    pub fn new(
        env: &BTreeMap<String, String>,
        env_remove: &[String],
        protocol: &Protocol,
        events: mpsc::UnboundedSender<Value>,
        inputs: mpsc::UnboundedSender<Input>,
        pid: Arc<std::sync::Mutex<Option<u32>>>,
    ) -> Self {
        let mut environment: IndexMap<String, String> = std::env::vars().collect();
        for (key, value) in env {
            environment.insert(key.clone(), value.clone());
        }
        for key in env_remove
            .iter()
            .cloned()
            .chain(crate::bridge::tendly_secrets())
        {
            environment.shift_remove(&key);
        }
        Core {
            events,
            secrets: secrets_of(environment.into_iter()),
            env: env.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
            env_remove: env_remove.to_vec(),
            inputs,
            writer: None,
            queued: Arc::new(AtomicUsize::new(0)),
            group: OwnedGroup::new(None),
            child: None,
            pid,
            sequence: 0,
            pending: HashMap::new(),
            frame: protocol.frame,
            reply: protocol.reply,
            client_name: protocol.client_name,
            seen: HashMap::new(),
            approvals: IndexMap::new(),
            turns: Vec::new(),
            active: None,
            cwd: String::new(),
            ready: false,
            closed: false,
            port_closed: false,
            initializing: false,
            initialized: false,
            finished: false,
            oversized: false,
            backlogged: false,
            unsent: Arc::new(AtomicUsize::new(0)),
            native_session_id: None,
            permission_mode: None,
            permission_modes: protocol.permission_modes,
        }
    }

    // -- emitting --

    /// `emit` in chat.mjs: redacted, and refused when the line is too long.
    pub fn emit(&mut self, event: Value) {
        if self.finished {
            return;
        }
        let event = scrub(event, &self.secrets);
        if stringify(&event).len() + 1 > MAX_EVENT_LINE {
            // `fatal` in the bridge: the error and the failed turn go out now,
            // so a step that goes on to finish the turn cannot take its place;
            // the loop closes the connection once this step is done.
            if !self.oversized {
                self.oversized = true;
                self.error("Structured event exceeded its wire size limit.");
                self.finish("failed");
            }
            return;
        }
        // The bridge stops when more than 4 MB of its output is unread, rather
        // than buffer without bound; the loop shuts the conversation down.
        if self.backlogged || self.unsent.load(Ordering::Acquire) > MAX_UNSENT {
            self.backlogged = true;
            return;
        }
        self.unsent
            .fetch_add(stringify(&event).len() + 1, Ordering::AcqRel);
        let _ = self.events.send(event);
    }
    /// `ChatBase.error`: clipped to 2 KB.
    pub fn error(&mut self, message: &str) {
        let message = clip(message, 2048);
        self.emit(json!({"type":"error","message":message}));
    }
    /// An error chat.mjs reports itself, unclipped.
    pub fn raw_error(&mut self, message: &str) {
        self.emit(json!({"type":"error","message":message}));
    }

    // -- the connection (NativeProcess) --

    /// Start the client with its own arguments, in the session's directory.
    pub fn spawn(&mut self, program: &str, args: &[String]) -> Result<(), ()> {
        let node = crate::bridge::js_runtime();
        let (program, args) =
            process::launcher_with_node(program, args, &node.to_string_lossy()).map_err(|_| ())?;
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
            .chain(crate::bridge::tendly_secrets())
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
        *self.pid.lock().expect("client pid") = child.id();
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
                // What a client writes here is never kept: it can carry
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
    pub fn send(&mut self, message: Value) -> Result<(), &'static str> {
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
    /// `NativeProcess.rpc`: write a request whose answer runs `then`. Should
    /// it not go out, `then` comes back to be failed with the reason.
    pub fn request(
        &mut self,
        method: &str,
        params: Value,
        then: T,
        optional: bool,
    ) -> Result<(), (T, String)> {
        self.sequence += 1;
        let id = format!("tendly-{}", self.sequence);
        let timeout = if optional {
            PROBE_TIMEOUT.min(RPC_TIMEOUT)
        } else {
            RPC_TIMEOUT
        };
        let interrupt = matches!(method, "interrupt" | "turn/interrupt" | "abort");
        let frame = (self.frame)(&id, method, params);
        match self.send(frame) {
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
                Ok(())
            }
            Err(message) => Err((then, message.to_owned())),
        }
    }
    /// A request already waiting when the client could not be launched: the
    /// bridge wrote it to a process that never started.
    pub fn waiting(&mut self, then: T) {
        self.sequence += 1;
        self.pending.insert(
            format!("tendly-{}", self.sequence),
            Pending {
                then,
                deadline: Instant::now() + RPC_TIMEOUT,
                optional: false,
                interrupt: false,
            },
        );
    }
    /// The request a message answers, with what the client answered.
    pub fn answered(&mut self, message: &Value) -> Option<(T, Result<Value, String>)> {
        let (id, result) = (self.reply)(message)?;
        let pending = self.pending.remove(id.as_str()?)?;
        let result = result.map_err(|stated| {
            let detail = stated.as_str().map(str::trim).filter(|d| !d.is_empty());
            match detail {
                Some(detail) => format!("{} refused: {}", self.client_name, clip(detail, 400)),
                None => {
                    "Native client rejected a control request. Check client settings and version."
                        .into()
                }
            }
        });
        Some((pending.then, result))
    }
    /// `NativeProcess.close`: no more lines either way. What was waiting
    /// comes back, to be failed with [`CANCELLED`].
    #[must_use]
    pub fn close_port(&mut self) -> Vec<T> {
        if self.port_closed {
            return Vec::new();
        }
        self.port_closed = true;
        self.writer = None;
        self.pending.drain().map(|(_, p)| p.then).collect()
    }
    /// Stop the client and everything it started, as `NativeProcess.close` did.
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
    /// Requests whose time is up: each with whether its timing out breaks the
    /// connection (a request that may have run is not retried).
    pub fn expired_requests(&mut self, now: Instant) -> Vec<(T, bool)> {
        let ids: Vec<String> = self
            .pending
            .iter()
            .filter(|(_, p)| p.deadline <= now)
            .map(|(id, _)| id.clone())
            .collect();
        ids.into_iter()
            .filter_map(|id| self.pending.remove(&id))
            .map(|p| {
                let breaks = !p.optional && !p.interrupt;
                (p.then, breaks)
            })
            .collect()
    }
    /// Approvals whose time is up. Each is reported once: its deadline is
    /// spent here, whether or not its answer then reaches the client.
    pub fn expired_approvals(&mut self, now: Instant) -> Vec<String> {
        let keys: Vec<String> = self
            .approvals
            .iter()
            .filter(|(_, a)| a.deadline.is_some_and(|deadline| deadline <= now))
            .map(|(key, _)| key.clone())
            .collect();
        for key in &keys {
            if let Some(approval) = self.approvals.get_mut(key) {
                approval.deadline = None;
            }
        }
        keys
    }
    fn next_deadline(&self) -> Option<Instant> {
        self.pending
            .values()
            .map(|p| p.deadline)
            .chain(self.approvals.values().filter_map(|a| a.deadline))
            .min()
    }

    // -- ChatBase --

    /// `ChatBase.begin`: a message that opens a turn, if it may.
    pub fn begin(&mut self, id: String, content: &str) -> Option<usize> {
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
        let turn = self.open_turn(id.clone());
        let text = clip(content, MAX_TEXT);
        self.emit(json!({"type":"message","id":id,"role":"user","text":text,"delta":false}));
        self.emit(json!({"type":"turn","id":id,"status":"running"}));
        Some(turn)
    }
    /// A turn without the user message `begin` writes.
    /// Whether a message id was already taken (`seen` in the bridge).
    pub fn has_seen(&self, id: &str) -> bool {
        self.seen.contains_key(id)
    }
    pub fn open_turn(&mut self, id: String) -> usize {
        self.turns.push(Turn {
            id,
            interrupted: false,
            extra: X::default(),
        });
        let turn = self.turns.len() - 1;
        self.active = Some(turn);
        turn
    }
    /// `ChatBase.acceptSteer`.
    pub fn accept_steer(&mut self, id: &str, content: &str) -> bool {
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
    /// `ChatBase.finish`: the turn ends, and its open requests with it.
    pub fn finish(&mut self, status: &str) {
        let Some(turn) = self.active.take() else {
            return;
        };
        let id = self.turns[turn].id.clone();
        let keys: Vec<String> = self.approvals.keys().cloned().collect();
        for key in keys {
            self.resolve_native(&key);
        }
        self.emit(json!({"type":"turn","id":id,"status":status}));
    }
    /// `ChatBase.announce`: ready, with the session's id and the commands the
    /// client itself advertised.
    pub fn announce(&mut self, id: Option<&str>, commands: Option<Vec<String>>) {
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
        let names: Vec<String> = commands
            .map(|commands| {
                let mut unique: Vec<String> = Vec::new();
                for name in commands {
                    let mut chars = name.chars();
                    let valid = chars.next().is_some_and(|c| c.is_ascii_alphanumeric())
                        && name.chars().count() <= 64
                        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | ':' | '-'));
                    if valid && !unique.contains(&name) {
                        unique.push(name);
                    }
                }
                unique.truncate(400);
                unique
            })
            .unwrap_or_default();
        let mut event = json!({"type":"ready"});
        if let Some(id) = &self.native_session_id {
            event["native_session_id"] = json!(id);
        }
        if !names.is_empty() {
            event["commands"] = json!(names);
        }
        self.emit(event);
    }
    /// `ChatBase.settings`.
    pub fn settings(
        &mut self,
        models: Option<&[Value]>,
        model: Option<&str>,
        effort: Option<&str>,
    ) {
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
        if let Some(modes) = self.permission_modes.filter(|m| !m.is_empty()) {
            event.insert("permission_modes".into(), json!(modes));
        }
        if let Some(list) = list {
            event.insert("models".into(), Value::Array(list));
        }
        self.emit(Value::Object(event));
    }
    /// `ChatBase.usage`.
    pub fn usage(
        &mut self,
        usage: Option<&Value>,
        context: Option<&Value>,
        window: Option<&Value>,
    ) {
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
    /// `ChatBase.approval`, once room for it is known: the card the person sees.
    fn register(
        &mut self,
        key: String,
        description: Map<String, Value>,
        responder: R,
        choices: Vec<String>,
    ) {
        let id = uuid::Uuid::new_v4().to_string();
        self.approvals.insert(
            key,
            Approval {
                id: id.clone(),
                responder,
                deadline: Some(Instant::now() + APPROVAL_TIMEOUT),
                choices,
            },
        );
        let mut event = Map::new();
        event.insert("type".into(), json!("approval"));
        event.insert("id".into(), json!(id));
        event.extend(description);
        self.emit(Value::Object(event));
    }
    /// `ChatBase.resolveNative`: the client settled it itself.
    pub fn resolve_native(&mut self, key: &str) {
        if let Some(pending) = self.approvals.shift_remove(key) {
            self.emit(json!({"type":"approval_resolved","id":pending.id}));
        }
    }
}

/// The decision an approval falls back to when nobody answers it.
fn fallback(choices: &[String]) -> Decision {
    if choices.iter().any(|c| c == "cancel") {
        Decision::Cancel
    } else {
        Decision::Decline
    }
}

/// A structured client: its protocol on top of the [`Core`].
pub trait NativeClient: Send + 'static {
    /// What follows one of its replies.
    type Then: Send;
    /// How one of its requests is answered once the person has decided.
    type Responder: Send;
    /// What it keeps per turn.
    type Turn: Default + Send;
    fn core(&mut self) -> &mut Core<Self::Then, Self::Responder, Self::Turn>;
    /// Start the client (`initialize`).
    fn start(&mut self, init: ChatInit);
    fn message(&mut self, id: String, content: String);
    fn steer(&mut self, id: String, content: String);
    fn interrupt(&mut self);
    fn select_model(&mut self, model: Option<String>, effort: Option<String>);
    fn set_permission_mode(&mut self, mode: String);
    /// A message from the client that answers nothing it asked.
    fn notification(&mut self, message: Value);
    /// What follows a reply, or a request that failed.
    fn settle(&mut self, then: Self::Then, result: Result<Value, String>);
    /// Answer a request; an `Err` is the bridge's throw inside its answer.
    fn respond(
        &mut self,
        responder: &Self::Responder,
        decision: Decision,
        answers: &Answers,
    ) -> Result<(), ()>;
    /// The responder of the approval at `key`, to answer it with.
    fn responder(&mut self, key: &str) -> Option<Self::Responder>;

    // -- provided: the parts of ChatBase that answer the client --

    /// `NativeProcess.rpc`.
    fn rpc(&mut self, method: &str, params: Value, then: Self::Then, optional: bool) {
        if let Err((then, message)) = self.core().request(method, params, then, optional) {
            self.settle(then, Err(message));
        }
    }
    /// `ChatBase.approval`.
    fn approval(
        &mut self,
        key: String,
        description: Map<String, Value>,
        responder: Self::Responder,
    ) {
        if self.core().approvals.contains_key(&key) {
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
        if self.core().approvals.len() >= 32 {
            let _ = self.respond(&responder, fallback(&choices), &Map::new());
            self.core().error("Too many pending native approvals.");
            return;
        }
        self.core().register(key, description, responder, choices);
    }
    /// `ChatBase.resolveApproval`.
    fn resolve_approval(&mut self, key: &str, decision: Decision, answers: &Answers) {
        let Some(responder) = self.responder(key) else {
            return;
        };
        if self.respond(&responder, decision, answers).is_err() {
            self.core()
                .error("Invalid or unavailable native approval response.");
            return;
        }
        self.core().resolve_native(key);
    }
    /// `ChatBase.answer`.
    fn answer(&mut self, request_id: &str, decision: Decision, answers: Option<Answers>) {
        let core = self.core();
        let Some((key, pending)) = core.approvals.iter().find(|(_, p)| p.id == request_id) else {
            core.error("This approval is no longer pending.");
            return;
        };
        if !pending.choices.iter().any(|c| c == decision.as_str()) {
            core.error("This decision is not offered by the native request.");
            return;
        }
        let key = key.clone();
        self.resolve_approval(&key, decision, &answers.unwrap_or_default());
    }
    /// `ChatBase.clearApprovals(true)`: every open request answered with its
    /// fallback.
    fn cancel_approvals(&mut self) {
        let keys: Vec<(String, Decision)> = self
            .core()
            .approvals
            .iter()
            .map(|(key, a)| (key.clone(), fallback(&a.choices)))
            .collect();
        for (key, decision) in keys {
            self.resolve_approval(&key, decision, &Map::new());
        }
    }
    /// `NativeProcess.close`, with what was waiting failed.
    fn close_port(&mut self) {
        for then in self.core().close_port() {
            self.settle(then, Err(CANCELLED.to_owned()));
        }
    }
    /// `ChatBase.shutdown`.
    fn shutdown(&mut self) {
        if self.core().closed {
            return;
        }
        self.core().closed = true;
        self.cancel_approvals();
        self.core().finish("interrupted");
        self.close_port();
    }
    /// `ChatBase.fatal`.
    fn fatal(&mut self, message: &str) {
        if self.core().closed {
            return;
        }
        self.core().error(message);
        self.core().finish("failed");
        self.shutdown();
    }
    /// The connection's own failure, which `NativeProcess` hands to `fatal`.
    fn failure(&mut self, message: &str) {
        if !self.core().port_closed {
            self.fatal(message);
        }
    }
    /// What arrived from the client: a reply to a request, or anything else.
    fn native(&mut self, message: Value) {
        if self.core().port_closed {
            return;
        }
        if let Some((then, result)) = self.core().answered(&message) {
            self.settle(then, result);
            return;
        }
        self.notification(message);
    }
    /// Requests and approvals whose time is up.
    fn expire(&mut self, now: Instant) {
        for (then, breaks) in self.core().expired_requests(now) {
            // A timed-out start may have executed: the connection is not reused.
            if breaks {
                self.failure(
                    "Native control request timed out; the structured connection was stopped.",
                );
            }
            self.settle(then, Err("Native control request timed out.".into()));
        }
        for key in self.core().expired_approvals(now) {
            let decision = self
                .core()
                .approvals
                .get(&key)
                .map_or(Decision::Decline, |a| fallback(&a.choices));
            self.resolve_approval(&key, decision, &Map::new());
            self.core()
                .error("Native approval timed out and was cancelled.");
        }
    }
    /// chat.mjs: what to do with each command.
    fn command(&mut self, input: Input) {
        if self.core().closed {
            return;
        }
        if self.core().initializing {
            self.core()
                .raw_error("Chat initialization is still in progress.");
            return;
        }
        if !self.core().initialized {
            self.core().raw_error("Invalid native chat initialization.");
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
            Input::Permission(mode) => self.set_permission_mode(mode),
            _ => {}
        }
    }
}

/// `jsonLines` over a client's stdout: bounded UTF-8 JSONL, with a deadline
/// for a line left incomplete.
async fn read_lines(stdout: tokio::process::ChildStdout, inputs: mpsc::UnboundedSender<Input>) {
    read_lines_within(stdout, inputs, LINE_TIMEOUT).await;
}
async fn read_lines_within<R: tokio::io::AsyncRead + Unpin>(
    mut stdout: R,
    inputs: mpsc::UnboundedSender<Input>,
    line_timeout: Duration,
) {
    let mut buffer: Vec<u8> = Vec::new();
    let mut total = 0usize;
    let mut bytes = vec![0u8; 64 * 1024];
    // When the incomplete line began: the bridge sets its timer once, then,
    // and clears it only when a line is complete.
    let mut began: Option<Instant> = None;
    loop {
        let read = if buffer.is_empty() {
            stdout.read(&mut bytes).await
        } else {
            let deadline = *began.get_or_insert_with(Instant::now) + line_timeout;
            match tokio::time::timeout_at(deadline, stdout.read(&mut bytes)).await {
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
            began = None;
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
async fn run<C: NativeClient>(
    mut client: C,
    mut inputs: mpsc::UnboundedReceiver<Input>,
    exited: &'static str,
) {
    loop {
        // The error and the failed turn were emitted with the oversized event.
        if std::mem::take(&mut client.core().oversized) || client.core().backlogged {
            client.shutdown();
        }
        if client.core().closed {
            break;
        }
        let deadline = client.core().next_deadline();
        let input = tokio::select! {
            input = inputs.recv() => input,
            _ = tokio::time::sleep_until(deadline.unwrap_or_else(Instant::now)), if deadline.is_some() => {
                client.expire(Instant::now());
                continue;
            }
        };
        let Some(input) = input else {
            client.shutdown();
            break;
        };
        match input {
            Input::Start(init, launched) => {
                client.start(init);
                let _ = launched.send(());
            }
            Input::Shutdown => {
                client.shutdown();
                break;
            }
            Input::Native(message) => client.native(message),
            Input::Broken(message) => client.failure(message),
            // The client's output ended: it is gone, or going.
            Input::Ended => client.failure(exited),
            command => client.command(command),
        }
    }
    let core = client.core();
    core.stop_process().await;
    core.oversized = false;
    core.emit(json!({"type":"exit"}));
    core.finished = true;
}

/// How a client's protocol frames requests and replies, and names itself.
pub struct Protocol {
    pub frame: Frame,
    pub reply: Reply,
    pub client_name: &'static str,
    /// What the conversation says when the client's output ends.
    pub exited: &'static str,
    pub permission_modes: Option<&'static [&'static str]>,
}

/// A client run in this process, behind the server's [`ClientAdapter`].
pub struct NativeAdapter {
    inputs: mpsc::UnboundedSender<Input>,
    events: mpsc::UnboundedReceiver<Value>,
    unsent: Arc<AtomicUsize>,
    pid: Arc<std::sync::Mutex<Option<u32>>>,
    task: Option<tokio::task::JoinHandle<()>>,
    done: bool,
}

impl NativeAdapter {
    /// A conversation with `env` applied over this server's environment and
    /// `env_remove` taken out of it, as the bridge's child process had.
    pub fn new<C: NativeClient>(
        env: &BTreeMap<String, String>,
        env_remove: &[String],
        protocol: Protocol,
        client: impl FnOnce(Core<C::Then, C::Responder, C::Turn>) -> C,
    ) -> Self {
        let (events_tx, events) = mpsc::unbounded_channel();
        let (inputs, inputs_rx) = mpsc::unbounded_channel();
        let pid = Arc::new(std::sync::Mutex::new(None));
        let core = Core::new(
            env,
            env_remove,
            &protocol,
            events_tx,
            inputs.clone(),
            pid.clone(),
        );
        let unsent = core.unsent.clone();
        let task = tokio::spawn(run(client(core), inputs_rx, protocol.exited));
        Self {
            inputs,
            events,
            unsent,
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

impl ClientAdapter for NativeAdapter {
    fn pid(&self) -> Option<u32> {
        *self.pid.lock().expect("client pid")
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
        match &event {
            Some(event) => {
                self.unsent
                    .fetch_sub(stringify(event).len() + 1, Ordering::AcqRel);
            }
            None => self.done = true,
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

/// An adapter dropped without `shutdown` -- a start that failed after it was
/// made, or a supervisor that panicked -- still ends its conversation. The
/// task holds a sender of its own, so its input channel never closes by
/// itself; without this the task, and the process group it owns, would
/// outlive the conversation.
impl Drop for NativeAdapter {
    fn drop(&mut self) {
        if self.task.is_some() {
            let _ = self.inputs.send(Input::Shutdown);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn a_launch_is_valid_only_with_a_program_an_absolute_directory_and_sane_arguments() {
        let base = ChatInit {
            provider: "codex".into(),
            program: "codex".into(),
            args: vec!["--model".into(), "x".into()],
            cwd: "/work".into(),
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
}
