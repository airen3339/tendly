//! The two layers a structured conversation can run on.
//!
//! A session's client speaks to the server through one internal protocol:
//! commands in (`init`, `message`, `steer`, `approval`, `interrupt`, `model`,
//! `permission`, `shutdown`) and events out (`ready`, `settings`, `cleared`,
//! `message`, `tool`, `approval`, `approval_resolved`, `turn`, `usage`,
//! `error`, `exit`; see `conversations::normalize_event`). [`ClientAdapter`]
//! is that protocol as an interface. [`BridgeAdapter`] runs it over the Node
//! chat bridge exactly as before; [`crate::codex_chat::CodexAdapter`] runs it
//! in this process for Codex, against `codex app-server` directly.
use agentdock_runtime::process::OwnedGroup;
use serde_json::{Map, Value, json};
use std::{future::Future, time::Duration};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// A failure that ends the conversation, worded for the transcript.
pub type Failure = &'static str;

/// What starts a client: the `init` line, as the bridge has always received it.
#[derive(Debug, Clone)]
pub struct ChatInit {
    pub provider: String,
    pub program: String,
    pub args: Vec<String>,
    pub cwd: String,
    pub resume_id: Option<String>,
}

impl ChatInit {
    pub fn to_value(&self) -> Value {
        let mut init = json!({"type":"init","provider":self.provider,"program":self.program,"args":self.args,"cwd":self.cwd});
        if let Some(id) = &self.resume_id {
            init["resume_id"] = json!(id);
        }
        init
    }
}

/// Whether Codex runs in this process ([`crate::codex_chat::CodexAdapter`])
/// rather than under the Node bridge: `AGENTDOCK_CODEX_ADAPTER=rust`, which
/// `codex_adapter = "rust"` in config.toml sets. Anything else, or nothing,
/// is the bridge, so a problem in the Rust adapter is one setting from undone.
/// Read at each start: a running conversation keeps the layer it began on.
pub fn codex_in_process(provider: agentdock_domain::ProviderKind) -> bool {
    provider == agentdock_domain::ProviderKind::Codex
        && adapter_choice(std::env::var("AGENTDOCK_CODEX_ADAPTER").ok().as_deref())
}

fn adapter_choice(value: Option<&str>) -> bool {
    value.is_some_and(|value| value.trim().eq_ignore_ascii_case("rust"))
}

/// One decision a person can give a native request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Accept,
    AcceptSession,
    Decline,
    Cancel,
}

impl Decision {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Accept => "accept",
            Self::AcceptSession => "accept_session",
            Self::Decline => "decline",
            Self::Cancel => "cancel",
        }
    }
    fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "accept" => Self::Accept,
            "accept_session" => Self::AcceptSession,
            "decline" => Self::Decline,
            "cancel" => Self::Cancel,
            _ => return None,
        })
    }
}

/// Answers to a native question, by question id.
pub type Answers = Map<String, Value>;

/// A command after the same checks `chat.mjs` (`validateChatInput`) makes.
#[derive(Debug, Clone, PartialEq)]
pub enum ChatCommand {
    Message {
        id: String,
        content: String,
    },
    Steer {
        id: String,
        content: String,
    },
    Approval {
        request_id: String,
        decision: Decision,
        answers: Option<Answers>,
    },
    Interrupt,
    Model {
        model: Option<String>,
        effort: Option<String>,
    },
    Permission {
        mode: String,
    },
    Shutdown,
}

fn has_control(value: &str) -> bool {
    value.chars().any(|c| (c as u32) < 0x20)
}

impl ChatCommand {
    /// Parse a command, refusing what the bridge refuses with its own words.
    pub fn parse(value: &Value) -> Result<Self, &'static str> {
        let Some(object) = value.as_object() else {
            return Err("Expected a structured chat object.");
        };
        let text = |key: &str| object.get(key).and_then(Value::as_str);
        match text("type") {
            Some(kind @ ("message" | "steer")) => {
                let (Some(id), Some(content)) = (text("id"), text("content")) else {
                    return Err("Invalid or oversized chat message.");
                };
                if id.is_empty()
                    || id.chars().count() > 128
                    || has_control(id)
                    || content.trim().is_empty()
                    || content.contains('\0')
                    || content.len() > 256 * 1024
                {
                    return Err("Invalid or oversized chat message.");
                }
                let (id, content) = (id.to_owned(), content.to_owned());
                Ok(if kind == "message" {
                    Self::Message { id, content }
                } else {
                    Self::Steer { id, content }
                })
            }
            Some("approval") => {
                let request_id = text("request_id")
                    .filter(|id| id.chars().count() <= 256)
                    .ok_or("Invalid approval response.")?;
                let decision = text("decision")
                    .and_then(Decision::parse)
                    .ok_or("Invalid approval response.")?;
                let answers = match object.get("answers") {
                    None => None,
                    Some(Value::Object(answers))
                        if answers.len() <= 16
                            && answers.iter().all(|(key, values)| {
                                key.chars().count() <= 256
                                    && values.as_array().is_some_and(|values| {
                                        values.len() <= 32
                                            && values.iter().all(|value| {
                                                value.as_str().is_some_and(|v| v.len() <= 8192)
                                            })
                                    })
                            }) =>
                    {
                        Some(answers.clone())
                    }
                    Some(_) => return Err("Invalid native question answers."),
                };
                Ok(Self::Approval {
                    request_id: request_id.to_owned(),
                    decision,
                    answers,
                })
            }
            Some("model") => {
                let model = match object.get("model") {
                    None => None,
                    Some(Value::String(model))
                        if !model.trim().is_empty()
                            && model.chars().count() <= 128
                            && !has_control(model) =>
                    {
                        Some(model.clone())
                    }
                    Some(_) => return Err("Invalid model selection."),
                };
                let effort = match object.get("effort") {
                    None => None,
                    Some(Value::String(effort))
                        if (1..=16).contains(&effort.len())
                            && effort.bytes().all(|b| b.is_ascii_lowercase()) =>
                    {
                        Some(effort.clone())
                    }
                    Some(_) => return Err("Invalid reasoning effort."),
                };
                if model.is_none() && effort.is_none() {
                    return Err("Choose a model or a thinking depth.");
                }
                Ok(Self::Model { model, effort })
            }
            Some("permission") => match text("mode") {
                Some(mode @ ("ask" | "plan" | "accept_edits" | "danger")) => Ok(Self::Permission {
                    mode: mode.to_owned(),
                }),
                _ => Err("Unsupported permission mode."),
            },
            Some("interrupt") => Ok(Self::Interrupt),
            Some("shutdown") => Ok(Self::Shutdown),
            _ => Err("Unsupported chat control message."),
        }
    }
}

/// One structured conversation's client, behind the internal protocol.
///
/// The methods are `ChatBase`'s (packages/native-bridge/chat-common.mjs) and
/// nothing more. Each returns once its command has been taken; what follows
/// arrives as events from [`ClientAdapter::next_event`]. A `Failure` from any
/// of them ends the conversation the way a broken bridge pipe always has.
pub trait ClientAdapter: Send + Sized + 'static {
    /// The process this conversation owns, for resource reporting.
    fn pid(&self) -> Option<u32>;
    /// Start the client: the `init` line.
    fn start(&mut self, init: &ChatInit) -> impl Future<Output = Result<(), Failure>> + Send;
    fn message(
        &mut self,
        id: String,
        content: String,
    ) -> impl Future<Output = Result<(), Failure>> + Send;
    fn steer(
        &mut self,
        id: String,
        content: String,
    ) -> impl Future<Output = Result<(), Failure>> + Send;
    fn approve(
        &mut self,
        request_id: String,
        decision: Decision,
        answers: Option<Answers>,
    ) -> impl Future<Output = Result<(), Failure>> + Send;
    fn interrupt(&mut self) -> impl Future<Output = Result<(), Failure>> + Send;
    fn set_model(
        &mut self,
        model: Option<String>,
        effort: Option<String>,
    ) -> impl Future<Output = Result<(), Failure>> + Send;
    fn set_permission_mode(
        &mut self,
        mode: String,
    ) -> impl Future<Output = Result<(), Failure>> + Send;
    /// The next event, unvalidated. `None` once the client has nothing more
    /// to say. Must be safe to drop mid-way: the caller selects over it.
    fn next_event(&mut self) -> impl Future<Output = Option<Result<Value, Failure>>> + Send;
    /// Stop the client and everything it started.
    fn shutdown(self) -> impl Future<Output = ()> + Send;
}

/// Hand a parsed command to the adapter. `Shutdown` is the caller's to act on.
pub async fn dispatch<A: ClientAdapter>(
    adapter: &mut A,
    command: ChatCommand,
) -> Result<(), Failure> {
    match command {
        ChatCommand::Message { id, content } => adapter.message(id, content).await,
        ChatCommand::Steer { id, content } => adapter.steer(id, content).await,
        ChatCommand::Approval {
            request_id,
            decision,
            answers,
        } => adapter.approve(request_id, decision, answers).await,
        ChatCommand::Interrupt => adapter.interrupt().await,
        ChatCommand::Model { model, effort } => adapter.set_model(model, effort).await,
        ChatCommand::Permission { mode } => adapter.set_permission_mode(mode).await,
        ChatCommand::Shutdown => Ok(()),
    }
}

/// The Node chat bridge (`chat.mjs`), unchanged: one JSON line per command on
/// its stdin, one per event on its stdout.
pub struct BridgeAdapter {
    child: tokio::process::Child,
    stdin: tokio::process::ChildStdin,
    stdout: tokio::process::ChildStdout,
    buffer: Vec<u8>,
    failed: bool,
}

impl BridgeAdapter {
    pub fn new(mut child: tokio::process::Child) -> Result<Self, crate::ApiError> {
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| crate::ApiError::conflict("Cannot open native chat input"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| crate::ApiError::conflict("Cannot open native chat output"))?;
        Ok(Self {
            child,
            stdin,
            stdout,
            buffer: Vec::new(),
            failed: false,
        })
    }
    async fn write(&mut self, line: Value, failure: Failure) -> Result<(), Failure> {
        match tokio::time::timeout(
            Duration::from_secs(3),
            self.stdin.write_all(format!("{line}\n").as_bytes()),
        )
        .await
        {
            Ok(Ok(())) => Ok(()),
            _ => Err(failure),
        }
    }
    /// The next complete line already read, as an event.
    fn take_line(&mut self) -> Option<Result<Value, Failure>> {
        let end = self.buffer.iter().position(|b| *b == b'\n')?;
        if end > 256 * 1024 {
            return Some(Err("Native chat event exceeded the size limit"));
        }
        let line: Vec<_> = self.buffer.drain(..=end).collect();
        Some(serde_json::from_slice::<Value>(&line).map_err(
            |_| "Native chat returned an unsupported event; update the installed bridge/client",
        ))
    }
}

const INPUT_UNAVAILABLE: Failure = "Native chat input is unavailable";

impl ClientAdapter for BridgeAdapter {
    fn pid(&self) -> Option<u32> {
        self.child.id()
    }
    async fn start(&mut self, init: &ChatInit) -> Result<(), Failure> {
        self.write(
            init.to_value(),
            "Native chat input closed during initialization",
        )
        .await
    }
    async fn message(&mut self, id: String, content: String) -> Result<(), Failure> {
        self.write(
            json!({"type":"message","id":id,"content":content}),
            INPUT_UNAVAILABLE,
        )
        .await
    }
    async fn steer(&mut self, id: String, content: String) -> Result<(), Failure> {
        self.write(
            json!({"type":"steer","id":id,"content":content}),
            INPUT_UNAVAILABLE,
        )
        .await
    }
    async fn approve(
        &mut self,
        request_id: String,
        decision: Decision,
        answers: Option<Answers>,
    ) -> Result<(), Failure> {
        let mut line =
            json!({"type":"approval","request_id":request_id,"decision":decision.as_str()});
        if let Some(answers) = answers {
            line["answers"] = Value::Object(answers);
        }
        self.write(line, INPUT_UNAVAILABLE).await
    }
    async fn interrupt(&mut self) -> Result<(), Failure> {
        self.write(json!({"type":"interrupt"}), INPUT_UNAVAILABLE)
            .await
    }
    async fn set_model(
        &mut self,
        model: Option<String>,
        effort: Option<String>,
    ) -> Result<(), Failure> {
        let mut line = json!({"type":"model"});
        if let Some(model) = model {
            line["model"] = json!(model);
        }
        if let Some(effort) = effort {
            line["effort"] = json!(effort);
        }
        self.write(line, INPUT_UNAVAILABLE).await
    }
    async fn set_permission_mode(&mut self, mode: String) -> Result<(), Failure> {
        self.write(json!({"type":"permission","mode":mode}), INPUT_UNAVAILABLE)
            .await
    }
    async fn next_event(&mut self) -> Option<Result<Value, Failure>> {
        if self.failed {
            return None;
        }
        loop {
            if let Some(event) = self.take_line() {
                self.failed = event.is_err();
                return Some(event);
            }
            if self.buffer.len() > 256 * 1024 {
                self.failed = true;
                return Some(Err("Native chat event exceeded the size limit"));
            }
            let mut bytes = [0u8; 8192];
            // Cancel-safe: a read that is dropped has consumed nothing.
            match self.stdout.read(&mut bytes).await {
                Ok(0) => return None,
                Ok(count) => self.buffer.extend_from_slice(&bytes[..count]),
                Err(_) => {
                    self.failed = true;
                    return Some(Err("Native chat output closed unexpectedly"));
                }
            }
        }
    }
    async fn shutdown(mut self) {
        let mut group = OwnedGroup::new(self.child.id());
        let _ = tokio::time::timeout(
            Duration::from_millis(250),
            self.stdin.write_all(b"{\"type\":\"shutdown\"}\n"),
        )
        .await;
        // A dedicated, positively identified process group contains only this bridge
        // and its native CLI descendants. Never signal the host or unrelated Agents.
        // Windows has no polite request, so there the bridge gets the time it had to
        // shut down on its own after the message above before its tree is stopped.
        #[cfg(windows)]
        let _ = tokio::time::timeout(Duration::from_millis(1600), self.child.wait()).await;
        group.terminate();
        // Keep the leader unreaped until its owned group has been cleaned up.
        // Waiting for only the Node PID can leave TERM-ignoring descendants.
        #[cfg(unix)]
        tokio::time::sleep(Duration::from_millis(1600)).await;
        group.finish();
        if tokio::time::timeout(Duration::from_secs(2), self.child.wait())
            .await
            .is_err()
        {
            let _ = self.child.start_kill();
            let _ = tokio::time::timeout(Duration::from_secs(1), self.child.wait()).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_bridge_runs_codex_unless_rust_is_asked_for() {
        assert!(!adapter_choice(None));
        assert!(!adapter_choice(Some("bridge")));
        assert!(!adapter_choice(Some("")));
        assert!(!adapter_choice(Some("rusty")));
        assert!(adapter_choice(Some("rust")));
        assert!(adapter_choice(Some(" Rust ")));
    }

    #[test]
    fn commands_are_refused_in_the_bridges_own_words() {
        let parse = |value: Value| ChatCommand::parse(&value);
        assert_eq!(
            parse(json!({"type":"message","id":"a","content":"hi"})),
            Ok(ChatCommand::Message {
                id: "a".into(),
                content: "hi".into()
            })
        );
        assert_eq!(
            parse(json!({"type":"message","id":"a","content":"   "})),
            Err("Invalid or oversized chat message.")
        );
        assert_eq!(
            parse(json!({"type":"approval","request_id":"r","decision":"acceptForSession"})),
            Err("Invalid approval response.")
        );
        assert_eq!(
            parse(
                json!({"type":"approval","request_id":"r","decision":"accept","answers":{"q":"x"}})
            ),
            Err("Invalid native question answers.")
        );
        assert_eq!(
            parse(json!({"type":"model"})),
            Err("Choose a model or a thinking depth.")
        );
        assert_eq!(
            parse(json!({"type":"model","effort":"High"})),
            Err("Invalid reasoning effort.")
        );
        assert_eq!(
            parse(json!({"type":"permission","mode":"bypass"})),
            Err("Unsupported permission mode.")
        );
        assert_eq!(
            parse(json!({"type":"dance"})),
            Err("Unsupported chat control message.")
        );
        assert_eq!(parse(json!("x")), Err("Expected a structured chat object."));
        assert_eq!(
            parse(json!({"type":"interrupt"})),
            Ok(ChatCommand::Interrupt)
        );
    }
}
