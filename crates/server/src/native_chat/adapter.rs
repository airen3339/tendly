//! [`NativeAdapter`]: a client run in this process, behind the server's
//! [`ClientAdapter`].
use super::*;
use crate::chat_adapter::{Answers, ChatInit, ClientAdapter, Decision, Failure};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::sync::mpsc;

/// A client run in this process, behind the server's [`ClientAdapter`].
pub struct NativeAdapter {
    inputs: mpsc::UnboundedSender<Input>,
    events: mpsc::UnboundedReceiver<Value>,
    unsent: Arc<AtomicUsize>,
    pid: Arc<std::sync::Mutex<Option<u32>>>,
    task: Option<tokio::task::JoinHandle<()>>,
    done: bool,
}

impl NativeAdapter {
    /// A conversation with `env` applied over this server's environment and
    /// `env_remove` taken out of it, as the bridge's child process had.
    pub fn new<C: NativeClient>(
        env: &BTreeMap<String, String>,
        env_remove: &[String],
        protocol: Protocol,
        client: impl FnOnce(Core<C::Then, C::Responder, C::Turn>) -> C,
    ) -> Self {
        let (events_tx, events) = mpsc::unbounded_channel();
        let (inputs, inputs_rx) = mpsc::unbounded_channel();
        let pid = Arc::new(std::sync::Mutex::new(None));
        let core = Core::new(
            env,
            env_remove,
            &protocol,
            events_tx,
            inputs.clone(),
            pid.clone(),
        );
        let unsent = core.unsent.clone();
        let task = tokio::spawn(run(client(core), inputs_rx, protocol.exited));
        Self {
            inputs,
            events,
            unsent,
            pid,
            task: Some(task),
            done: false,
        }
    }
    fn input(&self, input: Input) -> Result<(), Failure> {
        // A conversation that has ended answers nothing; its exit is already
        // on its way, as the bridge's was after its last line.
        let _ = self.inputs.send(input);
        Ok(())
    }
}

impl ClientAdapter for NativeAdapter {
    fn pid(&self) -> Option<u32> {
        *self.pid.lock().expect("client pid")
    }
    async fn start(&mut self, init: &ChatInit) -> Result<(), Failure> {
        // Waits for the launch, so the process is known before anything asks for it.
        let (launched, tried) = tokio::sync::oneshot::channel();
        self.input(Input::Start(init.clone(), launched))?;
        let _ = tokio::time::timeout(Duration::from_secs(5), tried).await;
        Ok(())
    }
    async fn message(&mut self, id: String, content: String) -> Result<(), Failure> {
        self.input(Input::Message(id, content))
    }
    async fn steer(&mut self, id: String, content: String) -> Result<(), Failure> {
        self.input(Input::Steer(id, content))
    }
    async fn approve(
        &mut self,
        request_id: String,
        decision: Decision,
        answers: Option<Answers>,
    ) -> Result<(), Failure> {
        self.input(Input::Approve(request_id, decision, answers))
    }
    async fn interrupt(&mut self) -> Result<(), Failure> {
        self.input(Input::Interrupt)
    }
    async fn set_model(
        &mut self,
        model: Option<String>,
        effort: Option<String>,
    ) -> Result<(), Failure> {
        self.input(Input::Model(model, effort))
    }
    async fn set_permission_mode(&mut self, mode: String) -> Result<(), Failure> {
        self.input(Input::Permission(mode))
    }
    async fn next_event(&mut self) -> Option<Result<Value, Failure>> {
        if self.done {
            return None;
        }
        let event = self.events.recv().await;
        match &event {
            Some(event) => {
                self.unsent
                    .fetch_sub(stringify(event).len() + 1, Ordering::AcqRel);
            }
            None => self.done = true,
        }
        event.map(Ok)
    }
    async fn shutdown(mut self) {
        let _ = self.inputs.send(Input::Shutdown);
        if let Some(task) = self.task.take() {
            let _ = tokio::time::timeout(Duration::from_secs(6), task).await;
        }
    }
}

/// An adapter dropped without `shutdown` -- a start that failed after it was
/// made, or a supervisor that panicked -- still ends its conversation. The
/// task holds a sender of its own, so its input channel never closes by
/// itself; without this the task, and the process group it owns, would
/// outlive the conversation.
impl Drop for NativeAdapter {
    fn drop(&mut self) {
        if self.task.is_some() {
            let _ = self.inputs.send(Input::Shutdown);
        }
    }
}
