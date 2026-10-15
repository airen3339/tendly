//! The flags a terminal run of Claude Code takes, kept or refused, and the
//! ones structured mode adds.

use super::*;

/// What `claudeLaunch` makes of a job: the flags, and what they named.
#[derive(Debug, Clone, PartialEq)]
pub struct Launch {
    pub args: Vec<String>,
    pub resume: Option<String>,
    pub session_id: Option<String>,
    pub model: Option<String>,
    pub effort: Option<String>,
}

pub(super) const VALUE_FLAGS: [&str; 22] = [
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
    let mut launch = LaunchArgs::new(&init.args, "Claude Code", true);
    while let Some(arg) = launch.next_flag() {
        let flag = arg.name;
        let mut value = || launch.value(&arg);
        if flag == "--resume" || flag == "-r" {
            if !take_resume(&mut resume, value()?) {
                return Err("Invalid or conflicting Claude Code resume ID.".into());
            }
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
