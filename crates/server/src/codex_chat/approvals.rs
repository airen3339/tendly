//! Requests app-server makes of the user -- command and file-change
//! approvals, questions, permissions -- and the answers sent back.

use super::*;

impl Codex {
    /// `reject`: a request Tendly cannot answer, refused as unsupported.
    pub(super) fn reject(&mut self, message: &Value, reason: &str) {
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

    /// A request app-server makes of the user: approving a command or a file
    /// change, answering a tool's question, granting permissions, or an MCP
    /// server asking to go ahead. Each becomes an approval the page shows,
    /// remembered with how to answer it. An MCP request for a form with fields
    /// is refused, since the page cannot show one, and Tendly's own server is
    /// answered at once; any other request is refused as unsupported.
    pub(super) fn request(&mut self, message: Value) {
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
    pub(super) fn answer_request(
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
    pub(super) fn cancel_turn(&mut self, cancel: bool) {
        if cancel && let Some(turn) = self.core.active {
            self.core.turns[turn].interrupted = true;
            self.send_interrupt(turn);
        }
    }
}
