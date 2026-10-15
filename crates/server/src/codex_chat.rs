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
use crate::chat_adapter::{Answers, ChatInit, Decision};
use crate::native_chat::{
    Core, MAX_TEXT, NativeAdapter, NativeClient, Protocol, clip, clip_value, js_string,
    json_rpc_frame, native_id, resolve, stringify, truthy, valid_init,
};
use indexmap::IndexMap;
use serde_json::{Map, Value, json};
use std::{
    collections::{BTreeMap, HashSet},
    path::Path,
};

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

/// A message written into the conversation: its id and content.
#[derive(Debug, Clone)]
struct Written {
    id: String,
    content: String,
}

/// What Codex keeps per turn (`Turn` in chat-common.mjs): steering written
/// before the turn had an id, that id, and whether an interrupt went out.
#[derive(Default)]
struct TurnExtra {
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

/// How a native request is answered once the person has decided.
#[derive(Clone)]
enum Responder {
    Decision { id: Value },
    Questions { id: Value, questions: Vec<Value> },
    Permissions { id: Value, permissions: Value },
    Elicitation { id: Value },
}

/// Codex, on the shared core.
struct Codex {
    core: Core<Then, Responder, TurnExtra>,
    program: String,
    launch: Option<Launch>,
    deferred: Vec<Written>,
    tools: IndexMap<String, (String, String)>,
    models: Option<Vec<Value>>,
    default_model: Option<String>,
    effort: Option<String>,
    approval_policy: Option<String>,
    sandbox_policy: Option<Value>,
    sandbox_overridden: bool,
    base_sandbox: Option<Value>,
}

impl Codex {
    fn restate_settings(&mut self) {
        let models = self.models.clone();
        let model = self.selected_model();
        let effort = self.effort.clone();
        self.core
            .settings(models.as_deref(), model.as_deref(), effort.as_deref());
    }
    fn selected_model(&self) -> Option<String> {
        self.launch
            .as_ref()
            .and_then(|l| l.thread.get("model"))
            .and_then(Value::as_str)
            .map(str::to_owned)
            .or_else(|| self.default_model.clone())
    }

    // -- chat.mjs: what to do with each command --

    fn open(&mut self, init: ChatInit) {
        if !valid_init(&init) {
            self.core.raw_error("Invalid native chat initialization.");
            self.shutdown();
            return;
        }
        match std::fs::metadata(&init.cwd) {
            Ok(meta) if meta.is_dir() => {}
            Ok(_) => {
                self.core.raw_error("Chat workspace is not a directory.");
                self.shutdown();
                return;
            }
            Err(_) => {
                let message = format!("ENOENT: no such file or directory, stat '{}'", init.cwd);
                self.core.raw_error(&message);
                self.shutdown();
                return;
            }
        }
        let launch = match codex_launch(&init) {
            Ok(launch) => launch,
            Err(message) => {
                self.core.raw_error(&message);
                self.shutdown();
                return;
            }
        };
        self.core.cwd = init.cwd.clone();
        self.program = init.program.clone();
        let args = launch.args.clone();
        self.launch = Some(launch);
        self.core.initializing = true;
        let program = self.program.clone();
        if self.core.spawn(&program, &args).is_err() {
            // A process that never started fails as the bridge's does: its
            // first request is already waiting when the launch error arrives,
            // and is failed by the close that follows.
            self.core.waiting(Then::Initialize);
            self.failure("Could not launch the configured native client.");
            return;
        }
        self.rpc(
            "initialize",
            json!({"clientInfo":{"name":"tendly","title":"Tendly","version":"0.1.0"}}),
            Then::Initialize,
            false,
        );
    }

    // -- CodexChat --

    fn choose_model(&mut self, model: Option<String>, effort: Option<String>) {
        if self.core.active.is_some() {
            self.core
                .raw_error("Wait for the current turn to finish before changing the model.");
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
    fn choose_permission(&mut self, mode: &str) {
        if self.core.active.is_some() {
            self.core
                .raw_error("Wait for the current turn to finish before changing permissions.");
            return;
        }
        let (policy, sandbox) = match mode {
            "ask" => ("on-request", None),
            "danger" => ("never", Some(json!({"type":"dangerFullAccess"}))),
            _ => {
                self.core.raw_error(
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
        self.core.permission_mode = Some(mode.into());
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
    fn send_message(&mut self, id: String, content: String) {
        if let Some(turn) = self.core.begin(id, &content) {
            self.run_turn(turn, content);
        }
    }
    fn run_turn(&mut self, turn: usize, content: String) {
        if self.core.native_session_id.is_none() {
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
        if self.core.active != Some(turn) {
            return;
        }
        if self.core.turns[turn].interrupted {
            self.core.finish("interrupted");
            return;
        }
        if self.models.is_some() {
            self.restate_settings();
        }
        if self.sandbox_policy.is_some() {
            self.sandbox_overridden = true;
        }
        let mut params = Map::new();
        params.insert("threadId".into(), json!(self.core.native_session_id));
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
        if self.core.active == Some(turn) {
            self.core.error(message);
            let status = if self.core.turns[turn].interrupted {
                "interrupted"
            } else {
                "failed"
            };
            self.core.finish(status);
        }
        self.pass_on(turn);
    }
    /// Steering written before a turn had an id, left over when that turn will
    /// never take it -- Codex refused to start it, or finished it before
    /// saying its id -- runs as the next turn, like steering a turn refused.
    fn pass_on(&mut self, turn: usize) {
        if self.core.turns[turn].interrupted || self.core.turns[turn].extra.early.is_empty() {
            return;
        }
        let early = std::mem::take(&mut self.core.turns[turn].extra.early);
        self.deferred.extend(early);
        self.continue_deferred();
    }
    fn steer_turn(&mut self, id: String, content: String) {
        let Some(turn) = self.core.active else {
            self.send_message(id, content);
            return;
        };
        if !self.core.accept_steer(&id, &content) {
            return;
        }
        let message = Written { id, content };
        if self.core.turns[turn].extra.native_turn.is_none() {
            self.core.turns[turn].extra.early.push(message);
            return;
        }
        self.send_steer(turn, message, Vec::new());
    }
    fn send_steer(&mut self, turn: usize, message: Written, then: Vec<Written>) {
        let params = json!({
            "threadId": self.core.native_session_id,
            "expectedTurnId": self.core.turns[turn].extra.native_turn,
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
        if self.core.active.is_some() || self.deferred.is_empty() {
            return;
        }
        let list: Vec<Written> = std::mem::take(&mut self.deferred);
        let id = list[0].id.clone();
        let turn = self.core.open_turn(id.clone());
        self.core
            .emit(json!({"type":"turn","id":id,"status":"running"}));
        let content = list
            .iter()
            .map(|m| m.content.as_str())
            .collect::<Vec<_>>()
            .join("\n\n");
        self.run_turn(turn, content);
    }
    fn send_interrupt(&mut self, turn: usize) {
        let state = &mut self.core.turns[turn].extra;
        if !truthy(state.native_turn.as_ref()) || state.interrupt_sent {
            return;
        }
        state.interrupt_sent = true;
        let native_turn = state.native_turn.clone();
        let params = json!({"threadId": self.core.native_session_id, "turnId": native_turn});
        self.rpc("turn/interrupt", params, Then::Interrupt { turn }, false);
    }
    fn interrupt_turn(&mut self) {
        let Some(turn) = self.core.active else {
            return;
        };
        self.core.turns[turn].interrupted = true;
        self.deferred.clear();
        self.cancel_approvals();
        self.send_interrupt(turn);
    }

    /// What follows a request's answer.
    fn after(&mut self, then: Then, result: Result<Value, String>) {
        match then {
            Then::Initialize => match result {
                Ok(_) => {
                    if let Err(message) =
                        self.core.send(json!({"method":"initialized","params":{}}))
                    {
                        self.core.initializing = false;
                        self.core.raw_error(message);
                        self.shutdown();
                        return;
                    }
                    self.core.announce(None, None);
                    let model = self
                        .launch
                        .as_ref()
                        .and_then(|l| l.thread.get("model"))
                        .and_then(Value::as_str)
                        .map(str::to_owned);
                    self.core.settings(None, model.as_deref(), None);
                    self.core.initializing = false;
                    self.core.initialized = true;
                    // Asking for models must not hold up the session.
                    self.rpc(
                        "model/list",
                        json!({"limit":100,"includeHidden":false}),
                        Then::ModelList,
                        true,
                    );
                }
                Err(message) => {
                    self.core.initializing = false;
                    self.core.raw_error(&message);
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
                self.core.announce(Some(&id), None);
                self.start_turn(turn, content);
            }
            Then::StartTurn { turn } => match result {
                Ok(result) => {
                    if self.core.active != Some(turn) {
                        self.pass_on(turn);
                        return;
                    }
                    self.core.turns[turn].extra.native_turn =
                        result.get("turn").and_then(|t| t.get("id")).cloned();
                    if self.core.turns[turn].interrupted {
                        self.send_interrupt(turn);
                    } else {
                        // Steering written before the turn had an id waited for it.
                        let mut early = std::mem::take(&mut self.core.turns[turn].extra.early);
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
                if result.is_err() && !self.core.turns[turn].interrupted {
                    // A turn that would not take it leaves it for the next one.
                    self.deferred.push(message);
                    if self.core.active != Some(turn) {
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
                if result.is_err() && self.core.active == Some(turn) {
                    self.core.turns[turn].extra.interrupt_sent = false;
                    self.core
                        .error("Codex could not confirm turn interruption.");
                }
            }
        }
    }

    fn on_notification(&mut self, message: Value) {
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
            && let Some(current) = &self.core.native_session_id
            && p.get("threadId") != Some(&json!(current))
        {
            return;
        }
        match method {
            "serverRequest/resolved" => {
                let key = request_key(p.get("requestId").unwrap_or(&Value::Null));
                self.core.resolve_native(&key);
                return;
            }
            "thread/tokenUsage/updated" => {
                let usage = p.get("tokenUsage");
                let last = usage.and_then(|u| u.get("last")).filter(|v| !v.is_null());
                let chosen = last.or_else(|| usage.and_then(|u| u.get("total")));
                let context = last.and_then(|l| l.get("totalTokens")).cloned();
                let window = usage.and_then(|u| u.get("modelContextWindow")).cloned();
                self.core
                    .usage(chosen.cloned().as_ref(), context.as_ref(), window.as_ref());
                return;
            }
            "error" => {
                self.core.error("Codex reported a native turn error. Check its account, endpoint and permission settings.");
                return;
            }
            _ => {}
        }
        let Some(turn) = self.core.active else {
            return;
        };
        if truthy(p.get("turnId"))
            && truthy(self.core.turns[turn].extra.native_turn.as_ref())
            && p.get("turnId") != self.core.turns[turn].extra.native_turn.as_ref()
        {
            return;
        }
        match method {
            "turn/started" => {
                self.core.turns[turn].extra.native_turn =
                    p.get("turn").and_then(|t| t.get("id")).cloned();
                if self.core.turns[turn].interrupted {
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
                self.core.finish(status);
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
                    self.core.emit(json!({"type":"message","id":id,"role":"assistant","text":text,"delta":true}));
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
                self.core.emit(
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
                self.core.emit(
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
        self.core
            .emit(json!({"type":"tool","id":id,"name":name,"status":status,"text":text}));
    }

    /// `reject`: a request Tendly cannot answer, refused as unsupported.
    fn reject(&mut self, message: &Value, reason: &str) {
        let id = message.get("id").cloned().unwrap_or(Value::Null);
        if self
            .core
            .send(json!({"id":id,"error":{"code":-32601,"message":reason}}))
            .is_err()
        {
            // The bridge's `send` throws here, inside a handler: the stream breaks.
            return self.failure("Invalid structured message received.");
        }
        self.core.error(reason);
    }

    fn request(&mut self, message: Value) {
        const UNSUPPORTED: &str = "This native request is not supported by Tendly.";
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
        if self.core.active.is_none()
            || (truthy(p.get("threadId"))
                && p.get("threadId")
                    != self
                        .core
                        .native_session_id
                        .as_ref()
                        .map(|s| json!(s))
                        .as_ref())
        {
            return self.reject(
                &message,
                "Native request does not belong to the active turn.",
            );
        }
        let cwd = self.core.cwd.clone();
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
                        q.get("id")
                            .and_then(Value::as_str)
                            .is_none_or(|id| id.encode_utf16().count() > 128)
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
            // Codex asks before an MCP tool runs, as an MCP elicitation. Tendly's
            // own tools already ask the person in Tendly for anything that
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
                        "This MCP server asked for a form Tendly cannot show.",
                    );
                }
                if p.get("serverName") == Some(&json!("tendly")) {
                    if self
                        .core
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
    fn answer_request(
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
                self.core
                    .send(json!({"id":id,"result":{"decision":native}}))
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
                self.core
                    .send(json!({"id":id,"result":{"answers":values}}))
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
                self.core
                    .send(json!({"id":id,"result":{"permissions":given,"scope":scope}}))
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
                self.core
                    .send(json!({"id":id,"result":{"action":decision.as_str(),"content":content}}))
                    .map_err(|_| ())?;
                self.cancel_turn(cancel);
                Ok(())
            }
        }
    }
    /// A cancelled question or permission stops the turn it belongs to.
    fn cancel_turn(&mut self, cancel: bool) {
        if cancel && let Some(turn) = self.core.active {
            self.core.turns[turn].interrupted = true;
            self.send_interrupt(turn);
        }
    }
}

/// Codex, run in this process (see the module documentation).
pub fn adapter(env: &BTreeMap<String, String>, env_remove: &[String]) -> NativeAdapter {
    NativeAdapter::new(
        env,
        env_remove,
        Protocol {
            frame: json_rpc_frame,
            reply: json_rpc_reply,
            client_name: "Codex",
            exited: "Codex app-server exited.",
            // Codex approves per command, so only the two ends of the range exist.
            permission_modes: Some(&["ask", "danger"]),
        },
        |core| Codex {
            core,
            program: String::new(),
            launch: None,
            deferred: Vec::new(),
            tools: IndexMap::new(),
            models: None,
            default_model: None,
            effort: None,
            approval_policy: None,
            sandbox_policy: None,
            sandbox_overridden: false,
            base_sandbox: None,
        },
    )
}

/// `knownReply` for JSON-RPC: a message with an id and no method answers the
/// request with that id.
fn json_rpc_reply(message: &Value) -> Option<(Value, Result<Value, Value>)> {
    if truthy(message.get("method")) {
        return None;
    }
    let id = message.get("id")?.clone();
    Some(match message.get("error") {
        Some(error) if truthy(Some(error)) => {
            let stated = error
                .get("message")
                .filter(|m| !m.is_null())
                .unwrap_or(error);
            (id, Err(stated.clone()))
        }
        _ => (
            id,
            Ok(message.get("result").cloned().unwrap_or(Value::Null)),
        ),
    })
}

impl NativeClient for Codex {
    type Then = Then;
    type Responder = Responder;
    type Turn = TurnExtra;
    fn core(&mut self) -> &mut Core<Then, Responder, TurnExtra> {
        &mut self.core
    }
    fn start(&mut self, init: ChatInit) {
        self.open(init);
    }
    fn message(&mut self, id: String, content: String) {
        self.send_message(id, content);
    }
    fn steer(&mut self, id: String, content: String) {
        self.steer_turn(id, content);
    }
    fn interrupt(&mut self) {
        self.interrupt_turn();
    }
    fn select_model(&mut self, model: Option<String>, effort: Option<String>) {
        self.choose_model(model, effort);
    }
    fn set_permission_mode(&mut self, mode: String) {
        self.choose_permission(&mode);
    }
    fn notification(&mut self, message: Value) {
        self.on_notification(message);
    }
    fn settle(&mut self, then: Then, result: Result<Value, String>) {
        self.after(then, result);
    }
    fn respond(
        &mut self,
        responder: &Responder,
        decision: Decision,
        answers: &Answers,
    ) -> Result<(), ()> {
        self.answer_request(responder, decision, answers)
    }
    fn responder(&mut self, key: &str) -> Option<Responder> {
        self.core
            .approvals
            .get(key)
            .map(|pending| pending.responder.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codex_reads_flags_inline_or_separate_and_refuses_empty_values() {
        let init = |args: &[&str]| ChatInit {
            provider: "codex".into(),
            program: "codex".into(),
            args: args.iter().map(|a| a.to_string()).collect(),
            cwd: "/fixture".into(),
            resume_id: None,
        };
        let launch = codex_launch(&init(&["-m=gpt-5", "--enable", "x"])).unwrap();
        assert_eq!(launch.thread.get("model"), Some(&json!("gpt-5")));
        assert_eq!(launch.args, ["app-server", "--enable", "x"]);
        for args in [&["-m", ""][..], &["-m="][..], &["--model"][..]] {
            assert_eq!(
                codex_launch(&init(args)).err().as_deref(),
                Some("Missing Codex launch option value."),
                "{args:?}"
            );
        }
        assert_eq!(
            codex_launch(&init(&["=x"])).err().as_deref(),
            Some("Unsupported Codex launch option in structured mode.")
        );
        assert_eq!(
            codex_launch(&init(&["resume", "abc", "--resume=abd"]))
                .err()
                .as_deref(),
            Some("Invalid or conflicting Codex resume ID.")
        );
        // Unlike Claude Code, an empty resume ID from the session is kept as given.
        let mut empty = init(&[]);
        empty.resume_id = Some(String::new());
        assert_eq!(codex_launch(&empty).unwrap().resume.as_deref(), Some(""));
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

    // Paths here are resolved as the platform does, as Node's `path.resolve`
    // does for the bridge: these expectations are POSIX ones.
    #[cfg(unix)]
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
