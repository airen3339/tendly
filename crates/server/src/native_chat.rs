//! What every structured client driven from this process shares: the Node
//! bridge's `chat-common.mjs` (`ChatBase`, `NativeProcess`, `jsonLines`, `clip`,
//! `redactEvents`) and `chat.mjs`'s command handling, in Rust.
//!
//! A client ([`NativeClient`]) owns a [`Core`] and adds only its protocol: how
//! it starts, what a message from it means, what follows each of its replies
//! and how one of its requests is answered. The core runs as one task (see
//! [`NativeAdapter`]): every input -- a command from the server, a message or a
//! reply from the client, a request timing out, an approval expiring, the
//! client's output ending -- is handled to the end before the next. A request
//! records what follows its answer (`Then`) instead of awaiting it, so no state
//! is held across a wait.
//!
//! `values.rs` has what the bridge does to every string and value, `launch.rs`
//! the launch-argument walk and the approval scope, `base.rs` the core's state
//! and the events it emits (`ChatBase`), `connection.rs` the process and the
//! requests waiting on it (`NativeProcess`), `client.rs` the [`NativeClient`]
//! trait, `run.rs` the line reader and the conversation's loop, and
//! `adapter.rs` the [`NativeAdapter`] the server holds.
use std::time::Duration;

mod adapter;
mod base;
mod client;
mod connection;
mod launch;
mod run;
#[cfg(test)]
mod tests;
mod values;

pub use adapter::*;
pub use base::*;
pub use client::*;
pub use launch::*;
use run::*;
pub use values::*;

pub const MAX_NATIVE_LINE: usize = 4 * 1024 * 1024;
pub const MAX_TEXT: usize = 64 * 1024;
pub const MAX_EVENT_LINE: usize = 192 * 1024;
/// Events sent and not yet taken, past which a conversation stops (chat.mjs).
const MAX_UNSENT: usize = 4 * 1024 * 1024;
const MAX_NATIVE_TOTAL: usize = 128 * 1024 * 1024;
const LINE_TIMEOUT: Duration = Duration::from_secs(10);
pub const RPC_TIMEOUT: Duration = Duration::from_secs(30);
const PROBE_TIMEOUT: Duration = Duration::from_secs(4);
const APPROVAL_TIMEOUT: Duration = Duration::from_secs(600);
const MAX_DIAGNOSTIC: usize = 16 * 1024 * 1024;
/// What a request still waiting is failed with when the connection closes.
pub const CANCELLED: &str = "Native client closed.";
