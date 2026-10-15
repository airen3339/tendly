//! Claude Code, driven from this process: the user's installed `claude` in
//! stream-json mode with no Node bridge in between. This is
//! `packages/native-bridge/chat-claude.mjs` in Rust, on the shared core
//! ([`crate::native_chat`]), and it produces the same events.
//!
//! Claude's protocol is its stream-json control channel: user messages and
//! `control_request`s in, a stream of `system`, `assistant`, `user`,
//! `stream_event` and `result` messages out, with `control_response`s answering
//! requests either way. Tool approvals arrive as the client's own
//! `can_use_tool` requests and are answered on the same channel.
use crate::chat_adapter::{Answers, ChatInit, Decision};
use crate::codex_chat::approval_scope;
use crate::native_chat::{
    Core, MAX_TEXT, NativeAdapter, NativeClient, Protocol, clip, clip_value, js_string, native_id,
    safe_integer, stringify, truthy, valid_init,
};
use indexmap::{IndexMap, IndexSet};
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, HashMap};

/// Tendly's names for how tools get approved, and Claude Code's.
const PERMISSIONS: [(&str, &str); 4] = [
    ("ask", "default"),
    ("plan", "plan"),
    ("accept_edits", "acceptEdits"),
    ("danger", "bypassPermissions"),
];

fn claude_mode(ours: &str) -> Option<&'static str> {
    PERMISSIONS
        .iter()
        .find(|(name, _)| *name == ours)
        .map(|(_, theirs)| *theirs)
}

/// The client calls the ask-every-time mode `default` on current builds and
/// `manual` on older ones. Both mean the same thing to a person.
fn our_mode(theirs: &str) -> Option<&'static str> {
    if theirs == "manual" {
        return Some("ask");
    }
    PERMISSIONS
        .iter()
        .find(|(_, name)| *name == theirs)
        .map(|(ours, _)| *ours)
}

/// What `claudeLaunch` makes of a job: the flags, and what they named.
#[derive(Debug, Clone, PartialEq)]
pub struct Launch {
    pub args: Vec<String>,
    pub resume: Option<String>,
    pub session_id: Option<String>,
    pub model: Option<String>,
    pub effort: Option<String>,
}

const VALUE_FLAGS: [&str; 22] = [
    "--model",
    "--permission-mode",
    "--permission-prompts",
    "--settings",
    "--setting-sources",
    "--append-system-prompt",
    "--system-prompt",
    "--agent",
    "--agents",
    "--effort",
    "--max-budget-usd",
    "--fallback-model",
    "--allowedTools",
    "--allowed-tools",
    "--disallowedTools",
    "--disallowed-tools",
    "--tools",
    "--add-dir",
    "--plugin-dir",
    "--mcp-config",
    "--name",
    "-n",
];

/// `claudeLaunch`: the flags a terminal run takes, kept or refused, then the
/// ones structured mode needs.
pub fn claude_launch(init: &ChatInit) -> Result<Launch, String> {
    let mut args: Vec<String> = Vec::new();
    let mut resume = init.resume_id.clone().filter(|id| !id.is_empty());
    let mut setting_sources = false;
    let mut session_id: Option<String> = None;
    let mut model = None;
    let mut effort = None;
    let mut i = 0;
    while i < init.args.len() {
        let arg = &init.args[i];
        let equal = arg.find('=').filter(|at| *at > 0);
        let flag = equal.map_or(arg.as_str(), |at| &arg[..at]);
        let mut value = || -> Result<String, String> {
            match equal {
                Some(at) => Ok(arg[at + 1..].to_owned()),
                None => {
                    i += 1;
                    init.args
                        .get(i)
                        .cloned()
                        .ok_or_else(|| "Missing Claude Code launch option value.".to_owned())
                }
            }
        };
        if flag == "--resume" || flag == "-r" {
            let id = value()?;
            if !native_id(&id) || resume.as_ref().is_some_and(|known| *known != id) {
                return Err("Invalid or conflicting Claude Code resume ID.".into());
            }
            resume = Some(id);
        } else if flag == "--session-id" {
            let id = value()?;
            if !native_id(&id) {
                return Err("Invalid Claude Code session ID.".into());
            }
            args.push(flag.to_owned());
            args.push(id.clone());
            session_id = Some(id);
        } else if flag == "--input-format" || flag == "--output-format" {
            value()?;
        } else if flag == "--permission-prompt-tool" {
            if value()? != "stdio" {
                return Err(
                    "Custom permission prompt tools are unsupported in structured mode.".into(),
                );
            }
        } else if [
            "--print",
            "-p",
            "--verbose",
            "--include-partial-messages",
            "--replay-user-messages",
        ]
        .contains(&flag)
        {
        } else if VALUE_FLAGS.contains(&flag) {
            let next = value()?;
            if flag == "--permission-mode" && next == "bypassPermissions" {
                return Err("Structured mode does not enable permission bypass.".into());
            }
            if flag == "--model" {
                model = Some(next.clone());
            }
            if flag == "--effort" {
                effort = Some(next.clone());
            }
            if flag == "--setting-sources" {
                setting_sources = true;
            }
            args.push(flag.to_owned());
            args.push(next);
        } else if flag == "--strict-mcp-config" || flag == "--disable-slash-commands" {
            args.push(flag.to_owned());
        } else {
            return Err("Unsupported Claude Code launch option in structured mode.".into());
        }
        i += 1;
    }
    if let (Some(resume), Some(session)) = (&resume, &session_id)
        && resume != session
    {
        return Err("Conflicting Claude Code session identifiers.".into());
    }
    if let Some(resume) = &resume {
        args.push("--resume".into());
        args.push(resume.clone());
    }
    // Permit switching into the unattended mode later without permitting it
    // now: the client refuses set_permission_mode to bypassPermissions unless
    // it was launched this way, and this flag allows rather than enables.
    args.push("--allow-dangerously-skip-permissions".into());
    if !setting_sources {
        args.push("--setting-sources".into());
        args.push("user,project,local".into());
    }
    // Each message written to the client is echoed back when the model reads
    // it: how a message sent mid-turn is known to have joined the turn.
    args.push("--replay-user-messages".into());
    for flag in [
        "--print",
        "--verbose",
        "--input-format",
        "stream-json",
        "--output-format",
        "stream-json",
        "--include-partial-messages",
        "--permission-prompt-tool",
        "stdio",
    ] {
        args.push(flag.into());
    }
    Ok(Launch {
        args,
        resume,
        session_id,
        model,
        effort,
    })
}

fn non_negative(value: Option<&Value>) -> Option<i64> {
    safe_integer(value).filter(|n| *n >= 0)
}

/// `contextTokens`: the input tokens in the window, cache reads and writes
/// included, from the client's own fields.
fn context_tokens(usage: Option<&Value>) -> Option<Value> {
    let parts = [
        "input_tokens",
        "cache_read_input_tokens",
        "cache_creation_input_tokens",
    ]
    .map(|key| non_negative(usage.and_then(|u| u.get(key))));
    if parts.iter().all(Option::is_none) {
        return None;
    }
    Some(json!(parts.iter().map(|p| p.unwrap_or(0)).sum::<i64>()))
}

/// `modelEntries`: the models the client lists at initialize time, with the
/// effort levels each states it supports.
fn model_entries(value: Option<&Value>) -> Option<Vec<Value>> {
    let entries = value?.as_array()?;
    Some(
        entries
            .iter()
            .filter_map(|entry| {
                let id = entry.get("value")?.as_str().filter(|v| !v.is_empty())?;
                let mut row = Map::new();
                row.insert("id".into(), json!(id));
                row.insert(
                    "name".into(),
                    json!(
                        entry
                            .get("displayName")
                            .and_then(Value::as_str)
                            .unwrap_or(id)
                    ),
                );
                if let Some(description) = entry.get("description").and_then(Value::as_str) {
                    row.insert("description".into(), json!(description));
                }
                if entry.get("supportsEffort") == Some(&Value::Bool(true))
                    && let Some(levels) =
                        entry.get("supportedEffortLevels").filter(|l| l.is_array())
                {
                    row.insert("efforts".into(), levels.clone());
                }
                Some(Value::Object(row))
            })
            .collect(),
    )
}

/// `commandNames`: commands as objects at initialize time and as names in the
/// later init message, reduced to names.
fn command_names(value: Option<&Value>) -> Option<Vec<String>> {
    let entries = value?.as_array()?;
    Some(
        entries
            .iter()
            .filter_map(|entry| match entry {
                Value::String(name) => Some(name.clone()),
                Value::Object(object) => object.get("name")?.as_str().map(str::to_owned),
                _ => None,
            })
            .collect(),
    )
}

fn is_uuid(id: &str) -> bool {
    let bytes = id.as_bytes();
    bytes.len() == 36
        && bytes.iter().enumerate().all(|(index, byte)| match index {
            8 | 13 | 18 | 23 => *byte == b'-',
            _ => byte.is_ascii_hexdigit(),
        })
}

/// The client echoes `uuid` back when it reads a message; Tendly's ids are
/// UUIDs already, and any other id goes out as a fresh one.
fn wire_id(id: &str) -> String {
    if is_uuid(id) {
        id.to_owned()
    } else {
        uuid::Uuid::new_v4().to_string()
    }
}

/// `keepsSelection`: whether a listed selection still describes the model
/// the client resolved it to.
pub fn keeps_selection(models: Option<&[Value]>, selected: Option<&str>, reported: &str) -> bool {
    let listed = selected.is_some_and(|selected| {
        models.is_some_and(|models| {
            models
                .iter()
                .any(|entry| entry.get("id").and_then(Value::as_str) == Some(selected))
        })
    });
    let Some(selected) = selected.filter(|s| !s.is_empty() && *s != reported && listed) else {
        return selected == Some(reported);
    };
    if selected == "default" {
        return true;
    }
    let base = match (selected.find('['), selected.ends_with(']')) {
        (Some(at), true) => &selected[..at],
        _ => selected,
    };
    reported.to_lowercase().contains(&base.to_lowercase())
}

fn first_line_short(line: &str) -> String {
    let first = line.split('\n').next().unwrap_or("");
    if first.encode_utf16().count() > 80 {
        let units: Vec<u16> = first.encode_utf16().take(79).collect();
        format!("{}\u{2026}", String::from_utf16_lossy(&units))
    } else {
        first.to_owned()
    }
}

/// `callSummary`: a few words on what a call does, from what its input says.
pub fn call_summary(input: Option<&Value>) -> Option<String> {
    let input = input?.as_object()?;
    let text = |key: &str| {
        input
            .get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .map(str::to_owned)
    };
    let file = |path: &str| {
        let parts: Vec<&str> = path.split(['/', '\\']).filter(|p| !p.is_empty()).collect();
        parts[parts.len().saturating_sub(2)..].join("/")
    };
    let path = text("file_path")
        .or_else(|| text("notebook_path"))
        .or_else(|| text("path"));
    let pattern = text("pattern");
    let url = text("url");
    if let Some(description) = text("description") {
        return Some(first_line_short(&description));
    }
    if let Some(command) = text("command") {
        return Some(first_line_short(&command));
    }
    if let Some(pattern) = pattern {
        return Some(first_line_short(&match &path {
            Some(path) => format!("{pattern} \u{00b7} {}", file(path)),
            None => pattern,
        }));
    }
    if let Some(path) = path {
        return Some(file(&path));
    }
    if let Some(query) = text("query") {
        return Some(first_line_short(&query));
    }
    let url = url?;
    let bare = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .unwrap_or(&url);
    Some(first_line_short(bare))
}

/// `turnUsage`: a turn's tokens from its result.
pub fn turn_usage(usage: Option<&Value>) -> Option<Value> {
    let usage = usage.filter(|u| u.is_object())?;
    let n = |key: &str| non_negative(usage.get(key)).unwrap_or(0);
    let cached = n("cache_read_input_tokens");
    let input = n("input_tokens") + cached + n("cache_creation_input_tokens");
    let output = n("output_tokens");
    if input == 0 && output == 0 {
        return None;
    }
    Some(json!({"input": input, "output": output, "cached": cached}))
}

/// `resultTokens`: roughly how many tokens a tool result puts into context.
pub fn result_tokens(text: &str) -> i64 {
    let (mut ascii, mut other) = (0f64, 0f64);
    for c in text.chars() {
        if (c as u32) < 128 {
            ascii += 1.0;
        } else {
            other += 1.0;
        }
    }
    (ascii / 4.0 + other).round() as i64
}

/// The one case-insensitive test of a failed result's diagnostic:
/// `/oauth|subscription.*(?:not|unsupported)|authentication.*(?:not|unsupported)/i`.
fn unsupported_authentication(diagnostic: &str) -> bool {
    let lower = diagnostic.to_lowercase();
    let followed = |head: &str| {
        lower.find(head).is_some_and(|at| {
            let rest = &lower[at + head.len()..];
            rest.contains("not") || rest.contains("unsupported")
        })
    };
    lower.contains("oauth") || followed("subscription") || followed("authentication")
}

fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

/// `controlRequest`: Claude Code's stream-json control channel.
fn control_frame(id: &str, method: &str, params: Value) -> Value {
    let mut request = Map::new();
    request.insert("subtype".into(), json!(method));
    if let Value::Object(params) = params {
        request.extend(params);
    }
    json!({"type":"control_request","request_id":id,"request":request})
}

/// The default `knownReply` for a Claude `control_response`.
fn control_reply(message: &Value) -> Option<(Value, Result<Value, Value>)> {
    if message.get("type").and_then(Value::as_str) != Some("control_response") {
        return None;
    }
    let response = message.get("response").filter(|r| !r.is_null());
    let id = response
        .and_then(|r| r.get("request_id"))
        .cloned()
        .unwrap_or(Value::Null);
    if response
        .and_then(|r| r.get("subtype"))
        .and_then(Value::as_str)
        == Some("error")
    {
        let stated = response
            .and_then(|r| r.get("error"))
            .filter(|e| !e.is_null())
            .cloned()
            .unwrap_or(Value::Bool(true));
        return Some((id, Err(stated)));
    }
    let result = response
        .and_then(|r| r.get("response"))
        .filter(|r| !r.is_null())
        .cloned()
        .unwrap_or_else(|| json!({}));
    Some((id, Ok(result)))
}

/// What follows the answer to one of this client's requests.
enum Then {
    Initialize,
    SetModel {
        model: String,
        effort: Option<String>,
    },
    SetPermission {
        mode: &'static str,
    },
    Interrupt {
        turn: usize,
    },
}

/// How a `can_use_tool` request is answered.
#[derive(Clone)]
enum Responder {
    Permission {
        request: Value,
        input: Value,
        suggestions: Vec<Value>,
    },
    Questions {
        request: Value,
        input: Value,
        questions: Vec<Value>,
    },
}

struct Claude {
    core: Core<Then, Responder, ()>,
    launch: Option<Launch>,
    models: Option<Vec<Value>>,
    model: Option<String>,
    effort: Option<String>,
    /// The id of the assistant message being written, as the client gave it.
    assistant_id: Option<Value>,
    /// Whether the turn has a complete assistant reply.
    final_assistant: bool,
    /// Tool call id to its name, for the calls announced as cards.
    tools: IndexMap<String, String>,
    /// Steered message ids Claude Code has not read back yet.
    unconsumed: IndexSet<String>,
    /// Background task id to the tool call that started it.
    tasks: IndexMap<String, String>,
    /// Tool calls whose task runs on after their placeholder result.
    background: IndexSet<String>,
    /// The last progress summary logged per tool call.
    summary_of: HashMap<String, String>,
}

/// Inserting into a map capped at 512 entries, oldest out first.
fn remember(map: &mut IndexMap<String, String>, key: String, value: String) {
    map.insert(key, value);
    if map.len() > 512 {
        map.shift_remove_index(0);
    }
}

impl Claude {
    fn report(&mut self) {
        let (models, model, effort) =
            (self.models.clone(), self.model.clone(), self.effort.clone());
        self.core
            .settings(models.as_deref(), model.as_deref(), effort.as_deref());
    }
    fn assistant(&self) -> Value {
        match &self.assistant_id {
            Some(id) => id.clone(),
            None => {
                let turn = self.core.active.map(|t| self.core.turns[t].id.clone());
                json!(format!("assistant-{}", turn.unwrap_or_default()))
            }
        }
    }

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
        let launch = match claude_launch(&init) {
            Ok(launch) => launch,
            Err(message) => {
                self.core.raw_error(&message);
                self.shutdown();
                return;
            }
        };
        self.core.cwd = init.cwd.clone();
        self.core.initializing = true;
        let args = launch.args.clone();
        self.launch = Some(launch);
        if self.core.spawn(&init.program, &args).is_err() {
            self.core.waiting(Then::Initialize);
            self.failure("Could not launch the configured native client.");
            return;
        }
        // Without forwardSubagentText the client sends only a subagent's tool
        // calls; the nested flag adds those of subagents that subagents start.
        self.rpc(
            "initialize",
            json!({"promptSuggestions":false,"forwardSubagentText":true,"forwardNestedSubagentToolCalls":true}),
            Then::Initialize,
            false,
        );
    }

    fn initialized(&mut self, result: Value) {
        let launch = self.launch.clone().unwrap_or(Launch {
            args: Vec::new(),
            resume: None,
            session_id: None,
            model: None,
            effort: None,
        });
        // The client lists its own commands here, before any turn has run.
        let session = launch.resume.clone().or(launch.session_id.clone());
        self.core
            .announce(session.as_deref(), command_names(result.get("commands")));
        self.models = model_entries(result.get("models"));
        // `default` means "whatever this configuration resolves to": a truthful
        // start, when the client listed it. A launch flag is more specific.
        let lists_default = self.models.as_ref().is_some_and(|models| {
            models
                .iter()
                .any(|entry| entry.get("id").and_then(Value::as_str) == Some("default"))
        });
        self.model = launch
            .model
            .clone()
            .or_else(|| lists_default.then(|| "default".to_owned()));
        // The depth it was launched with is the one in force.
        self.effort = launch.effort.clone();
        self.report();
        self.core.initializing = false;
        self.core.initialized = true;
    }

    fn choose_model(&mut self, model: Option<String>, effort: Option<String>) {
        if self.core.active.is_some() {
            self.core
                .raw_error("Wait for the current turn to finish before changing the model.");
            return;
        }
        // A depth-only change keeps the current model: the client has no
        // separate control for it, and guessing one would switch the user's.
        let Some(model) = model.or_else(|| self.model.clone()) else {
            self.core.raw_error("Claude Code has not reported which model this session uses yet. Send a message first, then adjust the thinking depth.");
            return;
        };
        let effort = effort.filter(|e| !e.is_empty());
        let mut params = Map::new();
        params.insert("model".into(), json!(model));
        if let Some(effort) = &effort {
            params.insert("effort".into(), json!(effort));
        }
        self.rpc(
            "set_model",
            Value::Object(params),
            Then::SetModel { model, effort },
            false,
        );
    }

    fn choose_permission(&mut self, mode: String) {
        if self.core.active.is_some() {
            self.core
                .raw_error("Wait for the current turn to finish before changing permissions.");
            return;
        }
        let Some(mode) = claude_mode(&mode) else {
            self.core.raw_error("Unsupported permission mode.");
            return;
        };
        self.rpc(
            "set_permission_mode",
            json!({"mode": mode}),
            Then::SetPermission { mode },
            false,
        );
    }

    fn user_frame(&self, id: &str, content: &str) -> Value {
        json!({
            "type": "user",
            "uuid": wire_id(id),
            "session_id": self.core.native_session_id.clone().unwrap_or_default(),
            "message": {"role": "user", "content": content},
            "parent_tool_use_id": null,
        })
    }

    fn send_message(&mut self, id: String, content: String) {
        if self.core.begin(id.clone(), &content).is_none() {
            return;
        }
        self.assistant_id = None;
        self.final_assistant = false;
        let frame = self.user_frame(&id, &content);
        if self.core.send(frame).is_err() {
            self.core
                .error("Could not send the message to Claude Code.");
            self.core.finish("failed");
        }
    }

    /// Steering needs nothing of Claude Code but the message: written while a
    /// turn runs, the model reads it at its next step. If the turn ends first
    /// it becomes a turn of its own (`continue_steered`).
    fn steer_turn(&mut self, id: String, content: String) {
        if self.core.active.is_none() {
            self.send_message(id, content);
            return;
        }
        if !self.core.accept_steer(&id, &content) {
            return;
        }
        self.unconsumed.insert(id.clone());
        let frame = self.user_frame(&id, &content);
        if self.core.send(frame).is_err() {
            self.unconsumed.shift_remove(&id);
            self.core
                .error("Could not send the message to Claude Code.");
        }
    }

    /// A steered message the turn ended without reading runs next, under its own id.
    fn continue_steered(&mut self) {
        let Some(next) = self.unconsumed.shift_remove_index(0) else {
            return;
        };
        self.assistant_id = None;
        self.final_assistant = false;
        self.core.open_turn(next.clone());
        self.core
            .emit(json!({"type":"turn","id":next,"status":"running"}));
    }

    fn interrupt_turn(&mut self) {
        let Some(turn) = self.core.active else {
            return;
        };
        self.core.turns[turn].interrupted = true;
        self.cancel_approvals();
        self.unconsumed.clear();
        self.rpc("interrupt", json!({}), Then::Interrupt { turn }, false);
    }

    fn begin_turn(&mut self, id: String) {
        self.assistant_id = None;
        self.final_assistant = false;
        self.core.open_turn(id.clone());
        self.core
            .emit(json!({"type":"turn","id":id,"status":"running"}));
    }

    fn after(&mut self, then: Then, result: Result<Value, String>) {
        match then {
            Then::Initialize => match result {
                Ok(result) => self.initialized(result),
                Err(message) => {
                    self.core.initializing = false;
                    self.core.raw_error(&message);
                    self.shutdown();
                }
            },
            Then::SetModel { model, effort } => match result {
                Ok(_) => {
                    self.model = Some(model);
                    if effort.is_some() {
                        self.effort = effort;
                    }
                    self.report();
                }
                Err(message) => self.core.raw_error(&message),
            },
            Then::SetPermission { mode } => match result {
                Ok(result) => {
                    // The answer comes back in the client's vocabulary; one this
                    // bridge cannot name is left to the status message.
                    let theirs = result.get("mode").and_then(Value::as_str).unwrap_or(mode);
                    if let Some(applied) = our_mode(theirs) {
                        self.core.permission_mode = Some(applied.to_owned());
                    }
                    self.report();
                }
                Err(message) => self.core.raw_error(&message),
            },
            Then::Interrupt { turn } => {
                if result.is_err() && self.core.active == Some(turn) {
                    self.core
                        .error("Claude Code could not confirm turn interruption.");
                }
            }
        }
    }

    fn on_notification(&mut self, message: Value) {
        if !message.is_object() {
            return;
        }
        let kind = message
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        let subtype = message
            .get("subtype")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        if kind == "control_request" {
            self.request(&message);
            return;
        }
        if kind == "control_cancel_request" {
            let key = js_string(message.get("request_id").unwrap_or(&Value::Null));
            let key = if message.get("request_id").is_none() {
                "undefined".to_owned()
            } else {
                key
            };
            self.core.resolve_native(&key);
            return;
        }
        // The client reports the mode it is actually in, including after a
        // change it made itself.
        if kind == "system"
            && let Some(theirs) = message.get("permissionMode").and_then(Value::as_str)
            && let Some(mode) = our_mode(theirs)
            && self.core.permission_mode.as_deref() != Some(mode)
        {
            self.core.permission_mode = Some(mode.to_owned());
            self.report();
        }
        if kind == "system" && subtype == "init" {
            self.core.announce(
                message.get("session_id").and_then(Value::as_str),
                command_names(message.get("slash_commands")),
            );
            // The init message names the resolved model; the entry that still
            // describes it is kept, so the chip stays on "Sonnet".
            if let Some(reported) = message
                .get("model")
                .and_then(Value::as_str)
                .filter(|m| !m.is_empty())
                && !keeps_selection(self.models.as_deref(), self.model.as_deref(), reported)
            {
                self.model = Some(reported.to_owned());
            }
            let (models, model) = (self.models.clone(), self.model.clone());
            self.core
                .settings(models.as_deref(), model.as_deref(), None);
            return;
        }
        // The echo of a message the model just read. A steered one is no
        // longer waiting; one read with no turn open opens one.
        if kind == "user" && message.get("isReplay") == Some(&Value::Bool(true)) {
            if let Some(id) = message
                .get("uuid")
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty())
            {
                self.unconsumed.shift_remove(id);
                if self.core.active.is_none() {
                    let id = if self.core.has_seen(id) {
                        id.to_owned()
                    } else {
                        format!("background-{id}")
                    };
                    self.begin_turn(id);
                }
            }
            return;
        }
        let parent = message.get("parent_tool_use_id");
        // A turn the client starts by itself: a background command it waited
        // on finished, and it carries on.
        if self.core.active.is_none()
            && (kind == "assistant" || kind == "stream_event")
            && !truthy(parent)
        {
            self.begin_turn(format!("background-{}", uuid::Uuid::new_v4()));
        }
        // A subagent's output belongs to the tool call that started it, and
        // usually arrives after the turn that started it has ended.
        if let Some(parent) = parent.and_then(Value::as_str).filter(|p| !p.is_empty()) {
            let parent = parent.to_owned();
            self.subagent(&message, &parent);
            return;
        }
        if kind == "system"
            && [
                "task_started",
                "task_progress",
                "task_updated",
                "task_notification",
            ]
            .contains(&subtype.as_str())
        {
            self.task(&message, &subtype);
            return;
        }
        if self.core.active.is_none() {
            return;
        }
        match kind.as_str() {
            "stream_event" => {
                let event = message
                    .get("event")
                    .filter(|e| !e.is_null())
                    .cloned()
                    .unwrap_or_else(|| json!({}));
                let event_type = event.get("type").and_then(Value::as_str).unwrap_or("");
                if event_type == "message_start" {
                    self.assistant_id = event
                        .get("message")
                        .and_then(|m| m.get("id"))
                        .filter(|id| !id.is_null())
                        .cloned();
                } else if event_type == "content_block_delta"
                    && event
                        .get("delta")
                        .and_then(|d| d.get("type"))
                        .and_then(Value::as_str)
                        == Some("text_delta")
                {
                    let text = clip_value(event.get("delta").and_then(|d| d.get("text")), MAX_TEXT);
                    let id = self.assistant();
                    self.core.emit(json!({"type":"message","id":id,"role":"assistant","text":text,"delta":true}));
                } else if event_type == "content_block_start"
                    && event
                        .get("content_block")
                        .and_then(|b| b.get("type"))
                        .and_then(Value::as_str)
                        == Some("tool_use")
                {
                    let block = event.get("content_block").cloned().unwrap_or(Value::Null);
                    self.tool(&block, "running", None);
                }
            }
            "assistant" => {
                let body = message
                    .get("message")
                    .filter(|b| !b.is_null())
                    .cloned()
                    .unwrap_or_else(|| json!({}));
                let blocks: Vec<Value> = body
                    .get("content")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                if let Some(id) = body.get("id").filter(|id| id.is_string()) {
                    self.assistant_id = Some(id.clone());
                }
                let text: String = blocks
                    .iter()
                    .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
                    .filter_map(|b| b.get("text").and_then(Value::as_str))
                    .collect();
                if !text.is_empty() {
                    self.final_assistant = true;
                    let id = self.assistant();
                    self.core.emit(json!({"type":"message","id":id,"role":"assistant","text":clip(&text, MAX_TEXT),"delta":false}));
                }
                for block in &blocks {
                    if block.get("type").and_then(Value::as_str) == Some("tool_use") {
                        self.tool(block, "running", None);
                    }
                }
                let usage = body.get("usage").filter(|u| !u.is_null());
                let context = context_tokens(usage);
                self.core.usage(usage, context.as_ref(), None);
            }
            "user" => {
                if let Some(content) = message
                    .get("message")
                    .and_then(|m| m.get("content"))
                    .and_then(Value::as_array)
                    .cloned()
                {
                    for result in &content {
                        if result.get("type").and_then(Value::as_str) == Some("tool_result")
                            && result.get("tool_use_id").is_some_and(Value::is_string)
                        {
                            self.result(result, None);
                        }
                    }
                }
            }
            "tool_progress" => {
                if let Some(id) = message.get("tool_use_id").and_then(Value::as_str) {
                    let name = match message.get("tool_name").filter(|n| !n.is_null()) {
                        Some(name) => clip_value(Some(name), 256),
                        None => "Native tool".to_owned(),
                    };
                    self.core
                        .emit(json!({"type":"tool","id":id,"name":name,"status":"running"}));
                }
            }
            "result" => self.finished(&message),
            _ => {}
        }
    }

    fn finished(&mut self, message: &Value) {
        let Some(turn) = self.core.active else {
            return;
        };
        // Claude reports the window per model it used this turn; only a single
        // agreed size is forwarded.
        let mut windows: Vec<i64> = Vec::new();
        let values: Vec<&Value> = match message.get("modelUsage") {
            Some(Value::Object(map)) => map.values().collect(),
            Some(Value::Array(items)) => items.iter().collect(),
            _ => Vec::new(),
        };
        for entry in values {
            if let Some(window) = safe_integer(entry.get("contextWindow")).filter(|w| *w > 0)
                && !windows.contains(&window)
            {
                windows.push(window);
            }
        }
        let window = (windows.len() == 1).then(|| json!(windows[0]));
        let usage = message.get("usage").filter(|u| !u.is_null());
        self.core.usage(usage, None, window.as_ref());
        // An empty result is not an answer.
        if !self.final_assistant
            && let Some(result) = message
                .get("result")
                .and_then(Value::as_str)
                .filter(|r| !r.trim().is_empty())
        {
            let id = self.assistant();
            self.core.emit(json!({"type":"message","id":id,"role":"assistant","text":clip(result, MAX_TEXT),"delta":false}));
        }
        // What the turn cost, after its reply.
        if let Some(spent) = turn_usage(usage) {
            self.core.emit(json!({"type":"usage","turn":spent}));
        }
        let failed = truthy(message.get("is_error"))
            || message.get("subtype").and_then(Value::as_str) != Some("success");
        let interrupted = self.core.turns[turn].interrupted;
        if failed && !interrupted {
            let errors = message
                .get("errors")
                .filter(|e| !e.is_null())
                .cloned()
                .unwrap_or_else(|| json!([]));
            let reason = if unsupported_authentication(&stringify(&errors)) {
                "Claude Code does not support this authentication mode for this interface. Use its native client or an approved API configuration."
            } else {
                "Claude Code could not complete the turn. Check its native account, endpoint and permission settings."
            };
            self.core.error(reason);
        }
        let status = if interrupted {
            "interrupted"
        } else if failed {
            "failed"
        } else {
            "completed"
        };
        self.core.finish(status);
        if !interrupted {
            self.continue_steered();
        }
    }

    /// Progress only: an activity line names what the subagent is doing; its
    /// own tool calls become cards nested under it.
    fn subagent(&mut self, message: &Value, parent: &str) {
        let Some(parent_name) = self.tools.get(parent).cloned() else {
            return;
        };
        let mut lines: Vec<String> = Vec::new();
        let blocks: Vec<Value> = message
            .get("message")
            .and_then(|m| m.get("content"))
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        match message.get("type").and_then(Value::as_str) {
            Some("assistant") => {
                for block in &blocks {
                    match block.get("type").and_then(Value::as_str) {
                        Some("text") => {
                            if let Some(text) = block
                                .get("text")
                                .and_then(Value::as_str)
                                .map(str::trim)
                                .filter(|t| !t.is_empty())
                            {
                                lines.push(text.to_owned());
                            }
                        }
                        Some("tool_use") => {
                            let what = call_summary(block.get("input"));
                            let name = match block.get("name").filter(|n| !n.is_null()) {
                                Some(name) => clip_value(Some(name), 120),
                                None => "tool".to_owned(),
                            };
                            lines.push(match what {
                                Some(what) => format!("\u{2192} {name} \u{00b7} {what}"),
                                None => format!("\u{2192} {name}"),
                            });
                            self.tool(block, "running", Some(parent));
                        }
                        _ => {}
                    }
                }
            }
            Some("user") => {
                for result in &blocks {
                    if result.get("type").and_then(Value::as_str) == Some("tool_result")
                        && let Some(id) = result.get("tool_use_id").and_then(Value::as_str)
                        && self.tools.contains_key(id)
                    {
                        self.result(result, Some(parent));
                    }
                }
            }
            _ => {}
        }
        if lines.is_empty() {
            return;
        }
        let name = self.tools.get(parent).cloned().unwrap_or(parent_name);
        self.core.emit(json!({"type":"tool","id":parent,"name":name,"status":"running","activity":clip(&lines.join("\n"), 4096),"at":now_millis()}));
    }

    fn result(&mut self, result: &Value, parent: Option<&str>) {
        let id = result
            .get("tool_use_id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let text = match result.get("content") {
            Some(Value::String(text)) => text.clone(),
            Some(Value::Array(blocks)) => blocks
                .iter()
                .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
                .map(|b| match b.get("text") {
                    None | Some(Value::Null) => String::new(),
                    Some(text) => js_string(text),
                })
                .collect::<Vec<_>>()
                .join("\n"),
            _ => String::new(),
        };
        // A backgrounded task answers at once with a placeholder; the card
        // stays running until its task_notification says how it ended.
        let background = self.background.contains(&id);
        let status = if truthy(result.get("is_error")) {
            "failed"
        } else if background {
            "running"
        } else {
            "completed"
        };
        let tokens = result_tokens(&text);
        let mut event = Map::new();
        event.insert("type".into(), json!("tool"));
        event.insert("id".into(), json!(id));
        event.insert(
            "name".into(),
            json!(self.tools.get(&id).cloned().unwrap_or("Native tool".into())),
        );
        event.insert("status".into(), json!(status));
        event.insert("text".into(), json!(clip(&text, MAX_TEXT)));
        if tokens != 0 {
            event.insert("result_tokens".into(), json!(tokens));
        }
        if background {
            event.insert("background".into(), json!(true));
        }
        if let Some(parent) = parent {
            event.insert("parent".into(), json!(parent));
        }
        self.core.emit(Value::Object(event));
    }

    /// A background task (subagent or command) reports its own start and end.
    fn task(&mut self, message: &Value, subtype: &str) {
        let task_id = message.get("task_id").and_then(Value::as_str);
        let id = match message.get("tool_use_id").and_then(Value::as_str) {
            Some(id) => Some(id.to_owned()),
            None => task_id.and_then(|task| self.tasks.get(task).cloned()),
        };
        let Some(id) = id.filter(|id| !id.is_empty() && self.tools.contains_key(id)) else {
            return;
        };
        let usage = message.get("usage").filter(|u| u.is_object());
        let mut stats = Map::new();
        for (key, field) in [
            ("tokens", "total_tokens"),
            ("tools", "tool_uses"),
            ("ms", "duration_ms"),
        ] {
            if let Some(count) = non_negative(usage.and_then(|u| u.get(field))) {
                stats.insert(key.into(), json!(count));
            }
        }
        match subtype {
            "task_started" => {
                if let Some(task) = task_id {
                    remember(&mut self.tasks, task.to_owned(), id.clone());
                }
                if truthy(message.get("is_backgrounded")) {
                    self.background.insert(id.clone());
                    if self.background.len() > 512 {
                        self.background.shift_remove_index(0);
                    }
                }
                // A command's description is the command itself, already the
                // card's input; a subagent's is what it was asked to do.
                let line = if message.get("task_type").and_then(Value::as_str) == Some("local_bash")
                {
                    None
                } else {
                    message
                        .get("description")
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                };
                self.card(&id, "running", line.as_deref(), &stats);
            }
            "task_updated" => {
                if truthy(message.get("patch").and_then(|p| p.get("is_backgrounded"))) {
                    self.background.insert(id.clone());
                    self.card(&id, "running", None, &stats);
                }
            }
            "task_progress" => {
                // A one-line status that repeats until it changes; logged once.
                let summary = match message.get("summary") {
                    Some(Value::String(summary)) if self.summary_of.get(&id) == Some(summary) => {
                        None
                    }
                    other => other.cloned(),
                };
                if let Some(Value::String(summary)) = &summary {
                    self.summary_of.insert(id.clone(), summary.clone());
                }
                let line = summary.as_ref().and_then(Value::as_str).map(str::to_owned);
                self.card(&id, "running", line.as_deref(), &stats);
            }
            _ => {
                self.background.shift_remove(&id);
                if let Some(task) = task_id {
                    self.tasks.shift_remove(task);
                }
                self.summary_of.remove(&id);
                let status = if message.get("status").and_then(Value::as_str) == Some("completed") {
                    "completed"
                } else {
                    "failed"
                };
                let line = message
                    .get("summary")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                self.card(&id, status, line.as_deref(), &stats);
            }
        }
    }

    fn card(&mut self, id: &str, status: &str, line: Option<&str>, stats: &Map<String, Value>) {
        let mut event = Map::new();
        event.insert("type".into(), json!("tool"));
        event.insert("id".into(), json!(id));
        event.insert(
            "name".into(),
            self.tools.get(id).map_or(Value::Null, |name| json!(name)),
        );
        event.insert("status".into(), json!(status));
        event.insert("at".into(), json!(now_millis()));
        if self.background.contains(id) {
            event.insert("background".into(), json!(true));
        }
        if !stats.is_empty() {
            event.insert("stats".into(), Value::Object(stats.clone()));
        }
        if let Some(line) = line.map(str::trim).filter(|l| !l.is_empty()) {
            event.insert("activity".into(), json!(clip(line, 4096)));
        }
        self.core.emit(Value::Object(event));
    }

    fn tool(&mut self, block: &Value, status: &str, parent: Option<&str>) {
        let Some(id) = block.get("id").and_then(Value::as_str).map(str::to_owned) else {
            return;
        };
        let name = match block.get("name").filter(|n| !n.is_null()) {
            Some(name) => clip_value(Some(name), 256),
            None => "Native tool".to_owned(),
        };
        remember(&mut self.tools, id.clone(), name.clone());
        let input = block
            .get("input")
            .filter(|i| !i.is_null())
            .cloned()
            .unwrap_or_else(|| json!({}));
        let mut event = Map::new();
        event.insert("type".into(), json!("tool"));
        event.insert("id".into(), json!(id));
        event.insert("name".into(), json!(name));
        event.insert("status".into(), json!(status));
        event.insert("text".into(), json!(clip(&stringify(&input), MAX_TEXT)));
        if let Some(parent) = parent {
            event.insert("parent".into(), json!(parent));
        }
        self.core.emit(Value::Object(event));
    }

    fn reject(&mut self, message: &Value, text: &str) {
        let request_id = message.get("request_id").cloned().unwrap_or(Value::Null);
        let _ = self.core.send(json!({"type":"control_response","response":{"subtype":"error","request_id":request_id,"error":text}}));
        self.core.error(text);
    }

    fn request(&mut self, message: &Value) {
        let request = message
            .get("request")
            .filter(|r| !r.is_null())
            .cloned()
            .unwrap_or_else(|| json!({}));
        let Some(key) = message
            .get("request_id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty() && id.encode_utf16().count() <= 256)
            .map(str::to_owned)
        else {
            self.fatal("Invalid native request identifier.");
            return;
        };
        const UNSUPPORTED: &str = "This Claude Code interaction is unsupported in Tendly.";
        let subtype = request.get("subtype").and_then(Value::as_str).unwrap_or("");
        if subtype == "request_user_dialog" {
            // Undeclared dialog kinds must not be answered, even with an error.
            self.core
                .error("This native dialog must be handled in Claude Code directly.");
            self.interrupt_turn();
            return;
        }
        if subtype != "can_use_tool" || self.core.active.is_none() {
            self.reject(message, UNSUPPORTED);
            return;
        }
        let input = request
            .get("input")
            .filter(|i| i.is_object() || i.is_array())
            .cloned()
            .unwrap_or_else(|| json!({}));
        if stringify(&input).len() > MAX_TEXT {
            self.reject(message, "Native approval is too large to display safely.");
            return;
        }
        if request.get("tool_name").and_then(Value::as_str) == Some("AskUserQuestion") {
            self.ask(message, &key, input);
            return;
        }
        if truthy(request.get("requires_user_interaction")) {
            self.reject(
                message,
                "This approval requires a native Claude Code dialog.",
            );
            return;
        }
        let mut shown = Map::new();
        if let Some(reason) = request.get("decision_reason") {
            shown.insert("reason".into(), reason.clone());
        }
        shown.insert("input".into(), input.clone());
        let text = stringify(&Value::Object(shown));
        if text.len() > MAX_TEXT {
            self.reject(message, "Native approval is too large to display safely.");
            return;
        }
        // Claude's own "don't ask again" updates, kept to this session:
        // Tendly never writes them into the user's settings files.
        let suggestions: Vec<Value> = request
            .get("permission_suggestions")
            .and_then(Value::as_array)
            .map(|updates| {
                updates
                    .iter()
                    .filter(|u| u.is_object() && u.get("type").is_some_and(Value::is_string))
                    .take(8)
                    .map(|update| {
                        let mut update = update.as_object().cloned().unwrap_or_default();
                        update.insert("destination".into(), json!("session"));
                        Value::Object(update)
                    })
                    .collect()
            })
            .unwrap_or_default();
        let tool = match request.get("tool_name").filter(|t| !t.is_null()) {
            Some(name) => js_string(name),
            None => String::new(),
        };
        let access = match tool.as_str() {
            "Read" | "Glob" | "Grep" | "LS" | "NotebookRead" => Some("read"),
            "Write" | "Edit" | "MultiEdit" | "NotebookEdit" => Some("write"),
            "Bash" => Some("execute"),
            "WebFetch" | "WebSearch" => Some("network"),
            _ => None,
        };
        let field = |value: Option<&Value>| value.cloned().unwrap_or(Value::Null);
        let paths = [
            field(request.get("blocked_path")),
            field(input.get("file_path")),
            field(input.get("path")),
            field(input.get("notebook_path")),
        ];
        let command = input.get("command").filter(|c| c.is_string());
        let cwd = self.core.cwd.clone();
        let scope = approval_scope(&cwd, access, &paths, command);
        let choices: Vec<&str> = if suggestions.is_empty() {
            vec!["accept", "decline", "cancel"]
        } else {
            vec!["accept", "accept_session", "decline", "cancel"]
        };
        let title = match request.get("title").filter(|t| !t.is_null()) {
            Some(title) => clip_value(Some(title), 512),
            None => {
                let tool = match request.get("tool_name").filter(|t| !t.is_null()) {
                    Some(name) => js_string(name),
                    None => "native tool".into(),
                };
                clip(&format!("Allow {tool}?"), 512)
            }
        };
        let mut description = Map::new();
        description.insert("title".into(), json!(title));
        description.insert("text".into(), json!(clip(&text, MAX_TEXT)));
        description.insert("choices".into(), json!(choices));
        if let Some(scope) = scope {
            description.insert("scope".into(), scope);
        }
        self.approval(
            key,
            description,
            Responder::Permission {
                request: message.clone(),
                input,
                suggestions,
            },
        );
    }

    /// `AskUserQuestion`: the client's questions, answered in the interface.
    fn ask(&mut self, message: &Value, key: &str, input: Value) {
        const UNSUPPORTED: &str = "This Claude Code interaction is unsupported in Tendly.";
        let questions: Vec<Value> = input
            .get("questions")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let mut asked: Vec<&str> = Vec::new();
        let valid = !questions.is_empty()
            && questions.len() <= 16
            && questions.iter().all(|q| {
                let Some(question) = q.get("question").and_then(Value::as_str) else {
                    return false;
                };
                if asked.contains(&question) {
                    return false;
                }
                asked.push(question);
                match q.get("options") {
                    None | Some(Value::Null) => true,
                    Some(Value::Array(options)) => {
                        options.len() <= 32
                            && options
                                .iter()
                                .all(|o| o.get("label").is_some_and(Value::is_string))
                    }
                    Some(_) => false,
                }
            });
        if !valid {
            self.reject(message, UNSUPPORTED);
            return;
        }
        let shown: Vec<Value> = questions
            .iter()
            .enumerate()
            .map(|(index, q)| {
                let options: Vec<Value> = q
                    .get("options")
                    .and_then(Value::as_array)
                    .map(|options| {
                        options
                            .iter()
                            .map(|o| json!({"label": clip_value(o.get("label"), 256), "description": clip_value(o.get("description"), 2048)}))
                            .collect()
                    })
                    .unwrap_or_default();
                json!({
                    "id": format!("question-{index}"),
                    "header": clip_value(q.get("header"), 128),
                    "question": clip_value(q.get("question"), MAX_TEXT),
                    "options": options,
                    "multiSelect": truthy(q.get("multiSelect")),
                    "isOther": true,
                })
            })
            .collect();
        let mut description = Map::new();
        description.insert("title".into(), json!("Claude Code needs your input"));
        description.insert("text".into(), json!("Answer the native client questions."));
        description.insert("choices".into(), json!(["accept", "decline", "cancel"]));
        description.insert("questions".into(), json!(shown));
        self.approval(
            key.to_owned(),
            description,
            Responder::Questions {
                request: message.clone(),
                input,
                questions,
            },
        );
    }

    /// `permissionResponse`.
    fn permission_response(
        &mut self,
        message: &Value,
        decision: Decision,
        input: Value,
        suggestions: &[Value],
    ) {
        let tool_use_id = message.get("request").and_then(|r| r.get("tool_use_id"));
        let mut result = Map::new();
        if matches!(decision, Decision::Accept | Decision::AcceptSession) {
            result.insert("behavior".into(), json!("allow"));
            result.insert("updatedInput".into(), input);
            if let Some(id) = tool_use_id {
                result.insert("toolUseID".into(), id.clone());
            }
            if decision == Decision::AcceptSession && !suggestions.is_empty() {
                result.insert("updatedPermissions".into(), json!(suggestions));
            }
        } else {
            result.insert("behavior".into(), json!("deny"));
            result.insert(
                "message".into(),
                json!("The user declined this tool request."),
            );
            result.insert("interrupt".into(), json!(decision == Decision::Cancel));
            if let Some(id) = tool_use_id {
                result.insert("toolUseID".into(), id.clone());
            }
        }
        let request_id = message.get("request_id").cloned().unwrap_or(Value::Null);
        let _ = self.core.send(json!({"type":"control_response","response":{"subtype":"success","request_id":request_id,"response":result}}));
        if decision == Decision::Cancel
            && let Some(turn) = self.core.active
        {
            self.core.turns[turn].interrupted = true;
        }
    }

    fn answer_request(
        &mut self,
        responder: &Responder,
        decision: Decision,
        answers: &Answers,
    ) -> Result<(), ()> {
        match responder {
            Responder::Permission {
                request,
                input,
                suggestions,
            } => {
                let (request, input, suggestions) =
                    (request.clone(), input.clone(), suggestions.clone());
                self.permission_response(&request, decision, input, &suggestions);
            }
            Responder::Questions {
                request,
                input,
                questions,
            } => {
                let answered = |index: usize| -> Vec<String> {
                    answers
                        .get(&format!("question-{index}"))
                        .and_then(Value::as_array)
                        .map(|values| values.iter().map(js_string).collect())
                        .unwrap_or_default()
                };
                if decision == Decision::Accept
                    && questions.iter().enumerate().any(|(index, q)| {
                        let given = answers
                            .get(&format!("question-{index}"))
                            .and_then(Value::as_array);
                        match given {
                            None => true,
                            Some(given) => {
                                given.is_empty()
                                    || (!truthy(q.get("multiSelect")) && given.len() > 1)
                            }
                        }
                    })
                {
                    return Err(());
                }
                let mut updated = input.as_object().cloned().unwrap_or_default();
                let mut chosen = Map::new();
                for (index, q) in questions.iter().enumerate() {
                    let question = q.get("question").and_then(Value::as_str).unwrap_or("");
                    chosen.insert(question.to_owned(), json!(answered(index).join(", ")));
                }
                updated.insert("answers".into(), Value::Object(chosen));
                let request = request.clone();
                self.permission_response(&request, decision, Value::Object(updated), &[]);
            }
        }
        Ok(())
    }
}

/// Claude Code, run in this process (see the module documentation).
pub fn adapter(env: &BTreeMap<String, String>, env_remove: &[String]) -> NativeAdapter {
    NativeAdapter::new(
        env,
        env_remove,
        Protocol {
            frame: control_frame,
            reply: control_reply,
            client_name: "Claude Code",
            exited: "Claude Code exited.",
            permission_modes: Some(&["ask", "plan", "accept_edits", "danger"]),
        },
        |core| Claude {
            core,
            launch: None,
            models: None,
            model: None,
            effort: None,
            assistant_id: None,
            final_assistant: false,
            tools: IndexMap::new(),
            unconsumed: IndexSet::new(),
            tasks: IndexMap::new(),
            background: IndexSet::new(),
            summary_of: HashMap::new(),
        },
    )
}

impl NativeClient for Claude {
    type Then = Then;
    type Responder = Responder;
    type Turn = ();
    fn core(&mut self) -> &mut Core<Then, Responder, ()> {
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
        self.choose_permission(mode);
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

    /// How a launch argument list is read: `--flag value` or `--flag=value`.
    /// Claude Code accepts an empty value; Codex and Pi do not.
    #[test]
    fn claude_reads_flags_inline_or_separate_and_accepts_empty_values() {
        let launch = claude_launch(&init(&["--model=opus", "--effort", ""])).unwrap();
        assert_eq!(launch.model.as_deref(), Some("opus"));
        assert_eq!(launch.effort.as_deref(), Some(""));
        assert_eq!(
            claude_launch(&init(&["--model="]))
                .unwrap()
                .model
                .as_deref(),
            Some("")
        );
        assert_eq!(
            claude_launch(&init(&["--model"])).err().as_deref(),
            Some("Missing Claude Code launch option value.")
        );
        // A leading `=` is not a flag with a value.
        assert_eq!(
            claude_launch(&init(&["=opus"])).err().as_deref(),
            Some("Unsupported Claude Code launch option in structured mode.")
        );
        assert_eq!(
            claude_launch(&init(&["-r=abc"])).unwrap().resume.as_deref(),
            Some("abc")
        );
        assert_eq!(
            claude_launch(&init(&["--resume", "abc", "-r", "abd"]))
                .err()
                .as_deref(),
            Some("Invalid or conflicting Claude Code resume ID.")
        );
        assert_eq!(
            claude_launch(&init(&["--resume", "not valid"]))
                .err()
                .as_deref(),
            Some("Invalid or conflicting Claude Code resume ID.")
        );
        // An empty resume ID from the session is no resume at all.
        let mut empty = init(&[]);
        empty.resume_id = Some(String::new());
        assert_eq!(claude_launch(&empty).unwrap().resume, None);
    }

    fn init(args: &[&str]) -> ChatInit {
        ChatInit {
            provider: "claude_code".into(),
            program: "claude".into(),
            args: args.iter().map(|a| a.to_string()).collect(),
            cwd: "/work".into(),
            resume_id: None,
        }
    }

    #[test]
    fn a_launch_keeps_its_options_and_adds_structured_mode() {
        let launch = claude_launch(&init(&[
            "--model",
            "sonnet",
            "--effort=high",
            "--permission-mode",
            "plan",
            "--strict-mcp-config",
            "--print",
            "--output-format",
            "text",
        ]))
        .unwrap();
        assert_eq!(launch.model.as_deref(), Some("sonnet"));
        assert_eq!(launch.effort.as_deref(), Some("high"));
        assert_eq!(
            launch.args,
            [
                "--model",
                "sonnet",
                "--effort",
                "high",
                "--permission-mode",
                "plan",
                "--strict-mcp-config",
                "--allow-dangerously-skip-permissions",
                "--setting-sources",
                "user,project,local",
                "--replay-user-messages",
                "--print",
                "--verbose",
                "--input-format",
                "stream-json",
                "--output-format",
                "stream-json",
                "--include-partial-messages",
                "--permission-prompt-tool",
                "stdio",
            ]
        );
    }

    #[test]
    fn a_launch_refuses_what_structured_mode_cannot_honour() {
        let refused = |args: &[&str]| claude_launch(&init(args)).unwrap_err();
        assert_eq!(
            refused(&["--permission-mode", "bypassPermissions"]),
            "Structured mode does not enable permission bypass."
        );
        assert_eq!(
            refused(&["--permission-prompt-tool", "mcp__x"]),
            "Custom permission prompt tools are unsupported in structured mode."
        );
        assert_eq!(
            refused(&["--dangerously-skip-permissions"]),
            "Unsupported Claude Code launch option in structured mode."
        );
        assert_eq!(
            refused(&["--model"]),
            "Missing Claude Code launch option value."
        );
        assert_eq!(
            refused(&["--resume", "a", "--session-id", "b"]),
            "Conflicting Claude Code session identifiers."
        );
        assert_eq!(
            refused(&["--resume", "../x"]),
            "Invalid or conflicting Claude Code resume ID."
        );
        let mut resumed = init(&["--resume", "session-1"]);
        resumed.resume_id = Some("session-2".into());
        assert_eq!(
            claude_launch(&resumed).unwrap_err(),
            "Invalid or conflicting Claude Code resume ID."
        );
    }

    #[test]
    fn a_launch_resumes_and_keeps_its_own_setting_sources() {
        let launch =
            claude_launch(&init(&["-r", "session-1", "--setting-sources", "user"])).unwrap();
        assert_eq!(launch.resume.as_deref(), Some("session-1"));
        assert!(
            launch
                .args
                .windows(2)
                .any(|w| w == ["--resume", "session-1"])
        );
        assert!(!launch.args.iter().any(|a| a == "user,project,local"));
    }

    #[test]
    fn a_selection_is_kept_while_it_describes_the_resolved_model() {
        let models = vec![
            json!({"id":"sonnet"}),
            json!({"id":"default"}),
            json!({"id":"opus[1m]"}),
        ];
        let keeps = |selected: Option<&str>, reported: &str| {
            keeps_selection(Some(&models), selected, reported)
        };
        assert!(keeps(Some("sonnet"), "claude-sonnet-5"));
        assert!(keeps(Some("default"), "claude-opus-5"));
        assert!(keeps(Some("opus[1m]"), "claude-opus-5-5[1m]"));
        assert!(!keeps(Some("sonnet"), "claude-opus-5"));
        assert!(keeps(Some("claude-x"), "claude-x"));
        assert!(!keeps(Some("unlisted"), "claude-x"));
        assert!(!keeps(None, "claude-x"));
    }

    #[test]
    fn calls_are_summarised_from_their_input() {
        let summary = |input: Value| call_summary(Some(&input));
        assert_eq!(
            summary(json!({"description":"List files","command":"ls"})).as_deref(),
            Some("List files")
        );
        assert_eq!(
            summary(json!({"command":"ls -la\nmore"})).as_deref(),
            Some("ls -la")
        );
        assert_eq!(
            summary(json!({"pattern":"TODO","path":"/a/b/c.rs"})).as_deref(),
            Some("TODO \u{00b7} b/c.rs")
        );
        assert_eq!(
            summary(json!({"file_path":"/a/b/c.rs"})).as_deref(),
            Some("b/c.rs")
        );
        assert_eq!(
            summary(json!({"url":"https://example.com/x"})).as_deref(),
            Some("example.com/x")
        );
        assert_eq!(summary(json!({"query":"rust"})).as_deref(), Some("rust"));
        let long = "x".repeat(100);
        assert_eq!(
            summary(json!({"command":long})).unwrap().chars().count(),
            80
        );
        assert_eq!(summary(json!({})), None);
        assert_eq!(call_summary(Some(&json!("text"))), None);
    }

    #[test]
    fn usage_and_tokens_are_counted_as_the_bridge_counts_them() {
        assert_eq!(
            turn_usage(Some(
                &json!({"input_tokens":10,"cache_read_input_tokens":5,"cache_creation_input_tokens":2,"output_tokens":3})
            )),
            Some(json!({"input":17,"output":3,"cached":5}))
        );
        assert_eq!(turn_usage(Some(&json!({"input_tokens":0}))), None);
        assert_eq!(
            context_tokens(Some(
                &json!({"input_tokens":10,"cache_read_input_tokens":-1})
            )),
            Some(json!(10))
        );
        assert_eq!(context_tokens(Some(&json!({"output_tokens":4}))), None);
        assert_eq!(result_tokens("abcd"), 1);
        assert_eq!(result_tokens("abcdef"), 2);
        assert_eq!(result_tokens("你好"), 2);
        assert_eq!(result_tokens(""), 0);
    }

    #[test]
    fn a_failed_turn_names_unsupported_authentication() {
        assert!(unsupported_authentication("[\"OAuth token rejected\"]"));
        assert!(unsupported_authentication("subscription is not supported"));
        assert!(unsupported_authentication(
            "Authentication mode unsupported"
        ));
        assert!(!unsupported_authentication("[\"rate limited\"]"));
        assert!(!unsupported_authentication("not a subscription"));
    }

    #[test]
    fn replies_are_control_responses_matched_by_request_id() {
        assert_eq!(
            control_reply(
                &json!({"type":"control_response","response":{"subtype":"success","request_id":"a","response":{"x":1}}})
            ),
            Some((json!("a"), Ok(json!({"x":1}))))
        );
        assert_eq!(
            control_reply(
                &json!({"type":"control_response","response":{"subtype":"success","request_id":"a"}})
            ),
            Some((json!("a"), Ok(json!({}))))
        );
        assert_eq!(
            control_reply(
                &json!({"type":"control_response","response":{"subtype":"error","request_id":"a","error":"no"}})
            ),
            Some((json!("a"), Err(json!("no"))))
        );
        assert_eq!(
            control_reply(
                &json!({"type":"control_response","response":{"subtype":"error","request_id":"a"}})
            ),
            Some((json!("a"), Err(json!(true))))
        );
        assert_eq!(control_reply(&json!({"type":"assistant"})), None);
        assert_eq!(
            control_frame("a", "set_model", json!({"model":"opus"})),
            json!({"type":"control_request","request_id":"a","request":{"subtype":"set_model","model":"opus"}})
        );
    }

    #[test]
    fn message_ids_go_out_as_uuids() {
        let id = "0f8fad5b-d9cb-469f-a165-70867728950e";
        assert_eq!(wire_id(id), id);
        assert!(is_uuid(&wire_id("not-a-uuid")));
        assert_ne!(wire_id("not-a-uuid"), "not-a-uuid");
    }
}
