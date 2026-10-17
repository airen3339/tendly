//! The connection to the client (`NativeProcess`): starting it, writing to
//! it, the requests waiting on its answers, and stopping it.
use super::*;
use serde_json::Value;
use std::{process::Stdio, sync::atomic::Ordering, time::Duration};
use tendly_runtime::process::{self, OwnedGroup};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    sync::mpsc,
    time::Instant,
};

impl<T, R, X: Default> Core<T, R, X> {
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
        if line.len() > MAX_NATIVE_INPUT || self.queued.load(Ordering::Acquire) > MAX_NATIVE_INPUT {
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
        // A model list is asked for in the background and nothing waits on it,
        // while a ChatGPT-signed Codex can take five seconds to fetch one; cut
        // off at the probe's limit, the composer lost its model, depth and
        // permission chips.
        let listing = matches!(method, "model/list" | "get_available_models");
        let timeout = if optional && !listing {
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
    pub(super) async fn stop_process(&mut self) {
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
    pub(super) fn next_deadline(&self) -> Option<Instant> {
        self.pending
            .values()
            .map(|p| p.deadline)
            .chain(self.approvals.values().filter_map(|a| a.deadline))
            .min()
    }
}
