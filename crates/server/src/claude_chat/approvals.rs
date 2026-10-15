//! Permission requests and questions from Claude Code, and the answers sent
//! back to them.

use super::*;

impl Claude {
    pub(super) fn reject(&mut self, message: &Value, text: &str) {
        let request_id = message.get("request_id").cloned().unwrap_or(Value::Null);
        let _ = self.core.send(json!({"type":"control_response","response":{"subtype":"error","request_id":request_id,"error":text}}));
        self.core.error(text);
    }

    pub(super) fn request(&mut self, message: &Value) {
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
    pub(super) fn ask(&mut self, message: &Value, key: &str, input: Value) {
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
    pub(super) fn permission_response(
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

    pub(super) fn answer_request(
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
