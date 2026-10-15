//! A session's launch arguments, walked as the client's command line reads
//! them, and the scope of access a request asks for.
use super::*;
use serde_json::{Map, Value, json};
use std::{collections::HashSet, path::Path};

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
