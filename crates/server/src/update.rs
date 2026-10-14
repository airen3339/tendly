//! Updating an npm install in place: `tendly update`, and the same from the
//! settings page.
//!
//! Only an install npm made can be updated here, because only there is
//! "update" one well-understood command whose result lands where this binary
//! already is. A single-file release or a source build is told the command for
//! its kind of install instead of being guessed at.
//!
//! Installing and restarting are separate steps on purpose. A restart stops
//! every running session (`reconcile_after_restart` marks them stopped), so the
//! new version waits on disk until someone chooses when to pay that.
use crate::{ApiError, AppState, Result, daemon, db};
use agentdock_domain::SessionStatus;
use axum::{
    Json, Router,
    extract::{Query, State},
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use std::{
    env,
    net::SocketAddr,
    path::{Path, PathBuf},
    process::Stdio,
    sync::{
        Mutex, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::process::Command;

pub const PACKAGE: &str = "@wnzzer/tendly";
/// The name Tendly was published under before. Installs made with it are still
/// updated through it: the package is still published, on the same binaries.
const LEGACY_PACKAGE: &str = "@wnzzer/agentdock";
/// How long a registry answer stands. Opening settings repeatedly should not
/// cost a registry round trip each time; a deliberate check skips this.
const CHECK_TTL: Duration = Duration::from_secs(10 * 60);
const CHECK_TIMEOUT: Duration = Duration::from_secs(30);
const INSTALL_TIMEOUT: Duration = Duration::from_secs(10 * 60);
/// What a supervised gateway exits with after an update restart: EX_TEMPFAIL,
/// so a unit with `Restart=on-failure` brings it back as `Restart=always` does.
pub const RESTART_EXIT_CODE: i32 = 75;

/// This executable's path, taken before anything can replace it.
///
/// npm swaps the package directory during an update, and afterwards Linux
/// reports the running executable as `… (deleted)`. The path that matters for
/// a restart is where the new binary now is, which is where this one started.
static EXECUTABLE: OnceLock<Option<PathBuf>> = OnceLock::new();
static RESTART_REQUESTED: AtomicBool = AtomicBool::new(false);
/// The version an update put on disk, until a restart runs it.
static INSTALLED: Mutex<Option<String>> = Mutex::new(None);
static LATEST: Mutex<Option<(Instant, String)>> = Mutex::new(None);
/// One install at a time: two npm runs over the same prefix corrupt it.
static INSTALLING: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

pub fn remember_executable() {
    EXECUTABLE.get_or_init(|| {
        env::current_exe()
            .ok()
            .and_then(|path| path.canonicalize().ok())
    });
}

fn executable() -> Option<&'static Path> {
    remember_executable();
    EXECUTABLE.get().and_then(|path| path.as_deref())
}

pub fn restart_requested() -> bool {
    RESTART_REQUESTED.load(Ordering::SeqCst)
}

/// Where npm keeps this install, if npm made it.
#[derive(Debug, Clone, PartialEq)]
pub struct NpmInstall {
    /// The global `node_modules` directory the package sits in.
    pub node_modules: PathBuf,
    /// What `npm --prefix` must be for an install to land in `node_modules`.
    pub prefix: PathBuf,
    /// The package this install was made with, and so the one to reinstall.
    pub package: &'static str,
}

/// Recognise an npm global install from the binary's own path.
///
/// The platform binary lives at
/// `…/node_modules/@wnzzer/tendly[/node_modules]/@wnzzer/tendly-<platform>/bin/`
/// (or under `@wnzzer/agentdock`, Tendly's former name),
/// so the outermost `node_modules` is the global one. Its prefix is two levels
/// up on Unix (`<prefix>/lib/node_modules`) and one on Windows
/// (`<prefix>\node_modules`). Passing that prefix explicitly is what makes the
/// update replace this install rather than add one wherever the user's npm
/// happens to point now.
pub fn npm_install(executable: &Path) -> Option<NpmInstall> {
    let mut node_modules = PathBuf::new();
    let mut components = executable.components();
    let mut found = false;
    for component in components.by_ref() {
        node_modules.push(component);
        if component.as_os_str() == "node_modules" {
            found = true;
            break;
        }
    }
    if !found {
        return None;
    }
    // The package directory the binary sits under says which name made this
    // install; a flattened layout leaves only what exists beside it.
    let under: Vec<_> = components.take(2).collect();
    let named = |package: &str| {
        under
            .iter()
            .map(|part| part.as_os_str())
            .eq(package.split('/').map(std::ffi::OsStr::new))
    };
    let package = [PACKAGE, LEGACY_PACKAGE]
        .into_iter()
        .find(|package| named(package))
        .or_else(|| {
            [PACKAGE, LEGACY_PACKAGE]
                .into_iter()
                .find(|package| node_modules.join(package).is_dir())
        })?;
    if !node_modules.join(package).is_dir() {
        return None;
    }
    let parent = node_modules.parent()?;
    let prefix = if !cfg!(windows) && parent.file_name().is_some_and(|name| name == "lib") {
        parent.parent()?
    } else {
        parent
    };
    Some(NpmInstall {
        prefix: prefix.to_path_buf(),
        node_modules,
        package,
    })
}

/// Whether this user can replace the package, decided before npm is asked.
///
/// A global install made with `sudo` is the common case where it cannot, and
/// npm's own EACCES report arrives after a long wait and reads as a crash.
fn writable(install: &NpmInstall) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        let scope = install.node_modules.join("@wnzzer");
        [scope.as_path(), install.node_modules.as_path()]
            .iter()
            .filter(|path| path.exists())
            .all(|path| {
                let Ok(raw) = std::ffi::CString::new(path.as_os_str().as_bytes()) else {
                    return false;
                };
                // SAFETY: a NUL-terminated path that lives for the call.
                unsafe { libc::access(raw.as_ptr(), libc::W_OK) == 0 }
            })
    }
    #[cfg(not(unix))]
    {
        let _ = install;
        true
    }
}

/// The command a person would run for this install, shown where the button
/// cannot do it for them.
fn manual_command(install: Option<&NpmInstall>, writable: bool) -> String {
    match install {
        Some(install) if !writable && cfg!(unix) => {
            format!("sudo npm install -g {}@latest", install.package)
        }
        Some(install) => format!("npm install -g {}@latest", install.package),
        None => "https://github.com/wnzzer/tendly/releases/latest".into(),
    }
}

fn npm() -> Command {
    let mut command = Command::new(if cfg!(windows) { "npm.cmd" } else { "npm" });
    command.stdin(Stdio::null()).kill_on_drop(true);
    command
}

/// `major.minor.patch` as numbers. A pre-release suffix is ignored, so a
/// pre-release never counts as newer than its release.
fn version_key(version: &str) -> Option<(u64, u64, u64)> {
    let core = version.trim().trim_start_matches('v');
    let core = core.split(['-', '+']).next()?;
    let mut parts = core.split('.').map(|part| part.parse::<u64>().ok());
    Some((parts.next()??, parts.next()??, parts.next()??))
}

pub fn newer(candidate: &str, than: &str) -> bool {
    matches!((version_key(candidate), version_key(than)), (Some(a), Some(b)) if a > b)
}

/// The package holding this machine's binary, `@wnzzer/tendly-<platform>`,
/// read from where this executable sits.
fn platform_package(executable: &Path) -> Option<String> {
    let mut parts = executable
        .components()
        .map(|part| part.as_os_str().to_string_lossy());
    while let Some(part) = parts.next() {
        if part == "@wnzzer"
            && let Some(name) = parts.clone().next()
            && (name.starts_with("tendly-") || name.starts_with("agentdock-"))
        {
            return Some(format!("@wnzzer/{name}"));
        }
    }
    None
}

/// Whether npm can see `package@version` yet.
///
/// The binary is an optional dependency, and npm skips one it cannot find
/// without failing the install. For a few minutes after a release the
/// registry can serve the new main package and not yet the new platform one;
/// an update then replaced a working install with one that had no binary.
async fn published(package: &str, version: &str) -> bool {
    let answer = tokio::time::timeout(
        CHECK_TIMEOUT,
        npm()
            .args(["view", &format!("{package}@{version}"), "version"])
            .output(),
    )
    .await;
    matches!(answer, Ok(Ok(output)) if output.status.success()
        && String::from_utf8_lossy(&output.stdout).trim() == version)
}

/// The newest published version, as this machine's npm sees it.
///
/// Asking npm rather than the registry directly keeps whatever registry,
/// mirror and proxy the user configured for npm, which is also what the
/// install itself will use. A version whose binary for this machine npm
/// cannot see yet is not offered: it is still arriving, and installing it
/// would leave no binary at all.
pub async fn latest(force: bool) -> std::result::Result<String, String> {
    if !force
        && let Some((at, version)) = LATEST.lock().expect("update cache").as_ref()
        && at.elapsed() < CHECK_TTL
    {
        return Ok(version.clone());
    }
    let output = tokio::time::timeout(
        CHECK_TIMEOUT,
        npm()
            .args(["view", &format!("{PACKAGE}@latest"), "version"])
            .output(),
    )
    .await
    .map_err(|_| "npm did not answer within 30 seconds".to_owned())?
    .map_err(|error| format!("Could not run npm: {error}"))?;
    if !output.status.success() {
        return Err(tail(
            &output.stderr,
            "npm could not look up the latest version",
        ));
    }
    let version = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if version_key(&version).is_none() {
        return Err(format!("npm reported an unreadable version: {version:?}"));
    }
    if let Some(package) = executable().and_then(platform_package)
        && !published(&package, &version).await
    {
        // Not cached, so it is offered as soon as it arrives.
        return Ok(env!("CARGO_PKG_VERSION").to_owned());
    }
    *LATEST.lock().expect("update cache") = Some((Instant::now(), version.clone()));
    Ok(version)
}

/// The last lines of a tool's error output, or `fallback` when it said nothing.
fn tail(output: &[u8], fallback: &str) -> String {
    let text = String::from_utf8_lossy(output);
    let lines: Vec<&str> = text
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect();
    if lines.is_empty() {
        return fallback.into();
    }
    lines[lines.len().saturating_sub(12)..].join("\n")
}

/// Install `version` over this npm install and return the version now on disk.
pub async fn install(install: &NpmInstall, version: &str) -> std::result::Result<String, String> {
    let _only = INSTALLING
        .try_lock()
        .map_err(|_| "An update is already being installed".to_owned())?;
    let program = executable().ok_or("Cannot locate the Tendly executable")?;
    if let Some(package) = platform_package(program)
        && !published(&package, version).await
    {
        return Err(format!(
            "{version} is still being published: npm cannot see {package}@{version} yet. Try again in a few minutes."
        ));
    }
    let installed = npm_install_version(install, program, version).await;
    if let Ok(on_disk) = &installed {
        *INSTALLED.lock().expect("update state") = Some(on_disk.clone());
        return installed;
    }
    // The tree npm left may have no working binary. Putting back the running
    // version is what keeps the next start from failing.
    let running = env!("CARGO_PKG_VERSION");
    if running == version {
        return installed;
    }
    let reason = installed.unwrap_err();
    match npm_install_version(install, program, running).await {
        Ok(_) => Err(format!(
            "{reason}. {running} was put back, so Tendly still starts."
        )),
        Err(again) => Err(format!(
            "{reason}. Putting {running} back failed too ({again}); run `npm install -g {}@{running}` before restarting.",
            install.package
        )),
    }
}

/// One `npm install` of `version`, confirmed by asking the binary it left.
async fn npm_install_version(
    install: &NpmInstall,
    program: &Path,
    version: &str,
) -> std::result::Result<String, String> {
    let output = tokio::time::timeout(
        INSTALL_TIMEOUT,
        npm()
            .arg("install")
            .arg("--global")
            .arg("--prefix")
            .arg(&install.prefix)
            .args(["--no-fund", "--no-audit"])
            .arg(format!("{}@{version}", install.package))
            .output(),
    )
    .await
    .map_err(|_| "npm install did not finish within 10 minutes".to_owned())?
    .map_err(|error| format!("Could not run npm: {error}"))?;
    if !output.status.success() {
        return Err(tail(&output.stderr, "npm install failed"));
    }
    // Trust the binary, not npm's exit status: the version that a restart
    // would run is whatever now answers at this path.
    let answer = Command::new(program)
        .arg("--version")
        .stdin(Stdio::null())
        .output()
        .await
        .map_err(|error| format!("The updated binary does not run: {error}"))?;
    let installed = String::from_utf8_lossy(&answer.stdout)
        .trim()
        .trim_start_matches("agentdock ")
        .to_owned();
    if installed != version {
        return Err(format!(
            "npm finished, but {} still reports {installed:?} rather than {version}",
            program.display()
        ));
    }
    Ok(installed)
}

/// How this gateway can be brought back after an update.
#[derive(Debug, Clone, Copy, Serialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum RestartMode {
    /// Started by `agentdock` / `tendly start`: a detached helper runs
    /// `tendly restart` with the new binary.
    Daemon,
    /// A service manager runs `serve` and restarts it when it exits.
    Supervisor,
    /// Nothing would start it again; a person has to.
    Manual,
}

fn restart_mode(state_dir: &Path) -> RestartMode {
    if daemon::running(state_dir) == Some(std::process::id() as i32) {
        RestartMode::Daemon
    } else if supervised() {
        RestartMode::Supervisor
    } else {
        RestartMode::Manual
    }
}

/// Started by systemd as a service, directly or through the npm shim.
///
/// `INVOCATION_ID` alone is not enough: a terminal emulator that runs as a
/// user service passes it to every shell, and a `serve` typed into one of those
/// would exit on a restart with nothing to bring it back. The parent has to be
/// systemd itself.
fn supervised() -> bool {
    #[cfg(target_os = "linux")]
    {
        if env::var_os("INVOCATION_ID").is_none() {
            return false;
        }
        let parent = |pid: u32| -> Option<(u32, String)> {
            let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
            // The command name is parenthesised and may itself contain spaces.
            let after = &stat[stat.rfind(')')? + 2..];
            let ppid: u32 = after.split_whitespace().nth(1)?.parse().ok()?;
            let name = std::fs::read_to_string(format!("/proc/{ppid}/comm")).ok()?;
            Some((ppid, name.trim().to_owned()))
        };
        let Some((ppid, name)) = parent(std::process::id()) else {
            return false;
        };
        if name == "systemd" || ppid == 1 {
            return true;
        }
        name == "node" && parent(ppid).is_some_and(|(pid, name)| name == "systemd" || pid == 1)
    }
    #[cfg(not(target_os = "linux"))]
    {
        false
    }
}

#[derive(Debug, Serialize)]
pub struct UpdateStatus {
    current: &'static str,
    /// `None` when the check failed or was not possible; `check_error` says why.
    latest: Option<String>,
    available: bool,
    /// Installed by an update and waiting for a restart.
    installed: Option<String>,
    /// `npm` when this server can update itself; `manual` otherwise.
    method: &'static str,
    /// For `npm`: whether this user may replace the install.
    writable: bool,
    /// What to run by hand when the button cannot.
    command: String,
    restart: RestartMode,
    /// Sessions a restart would stop.
    running_sessions: usize,
    check_error: Option<String>,
}

async fn status(state: &AppState, force: bool) -> Result<UpdateStatus> {
    let current = env!("CARGO_PKG_VERSION");
    let install = executable().and_then(npm_install);
    let writable = install.as_ref().is_some_and(writable);
    let (latest, check_error) = match &install {
        Some(_) => match latest(force).await {
            Ok(version) => (Some(version), None),
            Err(error) => (None, Some(error)),
        },
        None => (None, None),
    };
    let installed = INSTALLED.lock().expect("update state").clone();
    let running_sessions = db(state, |store| store.list_sessions(None))
        .await?
        .iter()
        .filter(|session| {
            matches!(
                session.status,
                SessionStatus::Running | SessionStatus::Starting | SessionStatus::Waiting
            )
        })
        .count();
    Ok(UpdateStatus {
        current,
        available: latest
            .as_deref()
            .is_some_and(|latest| newer(latest, installed.as_deref().unwrap_or(current))),
        latest,
        installed,
        method: if install.is_some() { "npm" } else { "manual" },
        writable,
        command: manual_command(install.as_ref(), writable),
        restart: restart_mode(&state.state_dir),
        running_sessions,
        check_error,
    })
}

#[derive(Deserialize, Default)]
struct CheckQuery {
    #[serde(default)]
    refresh: bool,
}

async fn read(
    State(state): State<AppState>,
    Query(query): Query<CheckQuery>,
) -> Result<Json<UpdateStatus>> {
    Ok(Json(status(&state, query.refresh).await?))
}

async fn apply(State(state): State<AppState>) -> Result<Json<UpdateStatus>> {
    let install = executable().and_then(npm_install).ok_or_else(|| {
        ApiError::conflict("This install was not made by npm; update it the way it was installed")
    })?;
    if !writable(&install) {
        return Err(ApiError::conflict(format!(
            "{} is not writable by this user. Run `{}` on the server instead",
            install.node_modules.display(),
            manual_command(Some(&install), false)
        )));
    }
    let version = latest(true).await.map_err(ApiError::conflict)?;
    if !newer(&version, env!("CARGO_PKG_VERSION")) {
        return Err(ApiError::conflict(format!(
            "Already up to date ({version})"
        )));
    }
    let installed = INSTALLED.lock().expect("update state").clone();
    if installed
        .as_deref()
        .is_some_and(|installed| !newer(&version, installed))
    {
        return Err(ApiError::conflict(format!(
            "{version} is already installed; restart Tendly to use it"
        )));
    }
    tracing::info!(%version, prefix = %install.prefix.display(), "installing update");
    install_or_report(&install, &version).await?;
    Ok(Json(status(&state, false).await?))
}

async fn install_or_report(npm_install: &NpmInstall, version: &str) -> Result<String> {
    install(npm_install, version).await.map_err(|error| {
        tracing::warn!(%error, "update failed");
        ApiError {
            status: axum::http::StatusCode::BAD_GATEWAY,
            message: error,
        }
    })
}

#[derive(Serialize)]
struct Restarting {
    restart: RestartMode,
}

/// Answer first, then go: the page needs the reply to know the gap in service
/// that follows is the restart it asked for and not a crash.
async fn restart(State(state): State<AppState>) -> Result<Json<Restarting>> {
    let mode = restart_mode(&state.state_dir);
    if mode == RestartMode::Manual {
        return Err(ApiError::conflict(
            "Nothing would start Tendly again after it stops here; restart it where it was started",
        ));
    }
    let program = executable()
        .ok_or_else(|| ApiError::internal("cannot locate the Tendly executable"))?
        .to_path_buf();
    let address: SocketAddr = env::var("AGENTDOCK_ADDR")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or_else(|| ([127, 0, 0, 1], 28789).into());
    let state_dir = state.state_dir.clone();
    tracing::info!(?mode, "restarting for update");
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(300)).await;
        match mode {
            RestartMode::Daemon => {
                if let Err(error) = daemon::relaunch(&state_dir, &program, address) {
                    tracing::error!(%error, "could not start the restart helper");
                }
            }
            RestartMode::Supervisor => {
                RESTART_REQUESTED.store(true, Ordering::SeqCst);
                // The same graceful path a service manager's stop takes, so
                // agents and the database close before the process exits.
                #[cfg(unix)]
                unsafe {
                    libc::kill(libc::getpid(), libc::SIGTERM);
                }
            }
            RestartMode::Manual => {}
        }
    });
    Ok(Json(Restarting { restart: mode }))
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/update", get(read).post(apply))
        .route("/api/update/restart", post(restart))
}

/// `tendly update [--check]`.
///
/// Installs but does not restart, like the page: a running gateway keeps its
/// sessions until someone restarts it, and this says how.
pub async fn command(check_only: bool) -> std::result::Result<(), Box<dyn std::error::Error>> {
    let current = env!("CARGO_PKG_VERSION");
    let program = executable().ok_or("Cannot locate the Tendly executable")?;
    let Some(found) = npm_install(program) else {
        println!(
            "Tendly {current} at {}\nThis install was not made by npm, so it cannot update itself.\nDownload the latest release from {}",
            program.display(),
            manual_command(None, false)
        );
        return Ok(());
    };
    let version = latest(true).await?;
    if !newer(&version, current) {
        println!("Tendly {current} is up to date");
        return Ok(());
    }
    if check_only {
        println!(
            "Tendly {version} is available (this is {current}). Run `tendly update` to install it."
        );
        return Ok(());
    }
    if !writable(&found) {
        return Err(format!(
            "{} is not writable by this user. Run:\n\n  {}",
            found.node_modules.display(),
            manual_command(Some(&found), false)
        )
        .into());
    }
    println!("Installing Tendly {version} (this is {current})…");
    let installed = install(&found, &version).await?;
    println!(
        "Installed Tendly {installed}.\n\nA running gateway keeps the old version until it restarts, which stops its\nrunning sessions. When ready: tendly restart (or restart the service that\nruns it, or use Settings → Updates in the page)."
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_compare_numerically_and_a_prerelease_is_not_newer() {
        assert!(newer("0.1.30", "0.1.27"));
        assert!(newer("0.2.0", "0.1.99"));
        assert!(newer("0.1.10", "0.1.9"));
        assert!(!newer("0.1.27", "0.1.27"));
        assert!(!newer("0.1.26", "0.1.27"));
        assert!(!newer("0.1.27-beta.1", "0.1.27"));
        assert!(!newer("garbage", "0.1.27"));
    }

    #[test]
    fn an_npm_global_install_is_recognised_with_the_prefix_npm_needs() {
        let root = env::temp_dir().join(format!("tendly-update-{}", std::process::id()));
        let node_modules = root.join("lib").join("node_modules");
        let binary_dir =
            node_modules.join("@wnzzer/tendly/node_modules/@wnzzer/tendly-linux-x64/bin");
        std::fs::create_dir_all(&binary_dir).unwrap();
        let found = npm_install(&binary_dir.join("tendly-server")).unwrap();
        assert_eq!(found.node_modules, node_modules);
        assert_eq!(found.package, "@wnzzer/tendly");
        if cfg!(windows) {
            assert_eq!(found.prefix, root.join("lib"));
        } else {
            assert_eq!(found.prefix, root);
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn an_install_made_under_the_old_name_is_updated_under_it() {
        let root = env::temp_dir().join(format!("tendly-update-legacy-{}", std::process::id()));
        let node_modules = root.join("node_modules");
        let binary_dir =
            node_modules.join("@wnzzer/agentdock/node_modules/@wnzzer/tendly-linux-x64/bin");
        std::fs::create_dir_all(&binary_dir).unwrap();
        let found = npm_install(&binary_dir.join("tendly-server")).unwrap();
        assert_eq!(found.package, "@wnzzer/agentdock");
        assert_eq!(
            manual_command(Some(&found), true),
            "npm install -g @wnzzer/agentdock@latest"
        );
        // Flattened: the platform package beside the main one says nothing.
        let flat = root.join("flat/node_modules");
        std::fs::create_dir_all(flat.join("@wnzzer/agentdock")).unwrap();
        std::fs::create_dir_all(flat.join("@wnzzer/tendly-linux-x64/bin")).unwrap();
        let found = npm_install(&flat.join("@wnzzer/tendly-linux-x64/bin/tendly-server")).unwrap();
        assert_eq!(found.package, "@wnzzer/agentdock");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_platform_package_is_read_from_where_the_binary_sits() {
        assert_eq!(
            platform_package(Path::new(
                "/home/u/.local/lib/node_modules/@wnzzer/tendly/node_modules/@wnzzer/tendly-linux-x64/bin/tendly-server"
            ))
            .as_deref(),
            Some("@wnzzer/tendly-linux-x64")
        );
        assert_eq!(
            platform_package(Path::new(
                "/usr/lib/node_modules/@wnzzer/agentdock-darwin-arm64/bin/tendly-server"
            ))
            .as_deref(),
            Some("@wnzzer/agentdock-darwin-arm64")
        );
        assert_eq!(
            platform_package(Path::new("/usr/local/bin/tendly-server")),
            None
        );
    }

    #[test]
    fn a_binary_outside_node_modules_is_not_an_npm_install() {
        assert_eq!(npm_install(Path::new("/usr/local/bin/tendly-server")), None);
        // A node_modules that does not hold this package is someone else's.
        let root = env::temp_dir().join(format!("tendly-update-other-{}", std::process::id()));
        let elsewhere = root.join("node_modules/other/bin");
        std::fs::create_dir_all(&elsewhere).unwrap();
        assert_eq!(npm_install(&elsewhere.join("tendly-server")), None);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_manual_command_asks_for_sudo_only_where_the_install_needs_it() {
        let install = NpmInstall {
            node_modules: PathBuf::from("/usr/lib/node_modules"),
            prefix: PathBuf::from("/usr"),
            package: PACKAGE,
        };
        assert_eq!(
            manual_command(Some(&install), true),
            "npm install -g @wnzzer/tendly@latest"
        );
        if cfg!(unix) {
            assert!(manual_command(Some(&install), false).starts_with("sudo "));
        }
        assert!(manual_command(None, true).starts_with("https://"));
    }
}
