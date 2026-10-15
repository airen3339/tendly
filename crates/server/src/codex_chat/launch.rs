//! The app-server arguments, the thread's own settings and any thread to
//! resume, from a session's launch arguments.

use super::*;

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
    let mut launch = LaunchArgs::new(&init.args, "Codex", false);
    while let Some(arg) = launch.next_flag() {
        let flag = arg.name;
        let mut value = || launch.value(&arg);
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
                if !take_resume(&mut resume, value()?) {
                    return Err("Invalid or conflicting Codex resume ID.".into());
                }
            }
            "--no-alt-screen" | "app-server" => {}
            _ => return Err("Unsupported Codex launch option in structured mode.".into()),
        }
    }
    Ok(Launch {
        args,
        thread,
        resume,
    })
}
