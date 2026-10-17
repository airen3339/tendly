//! The conversation's task: the reader of the client's output, and the loop
//! that handles every input to the end before the next.
use super::*;
use serde_json::{Value, json};
use std::time::Duration;
use tokio::{io::AsyncReadExt, sync::mpsc, time::Instant};

/// `jsonLines` over a client's stdout: bounded UTF-8 JSONL, with a deadline
/// for a line left incomplete.
pub(super) async fn read_lines(
    stdout: tokio::process::ChildStdout,
    inputs: mpsc::UnboundedSender<Input>,
) {
    read_lines_within(stdout, inputs, LINE_TIMEOUT).await;
}
pub(super) async fn read_lines_within<R: tokio::io::AsyncRead + Unpin>(
    mut stdout: R,
    inputs: mpsc::UnboundedSender<Input>,
    line_timeout: Duration,
) {
    let mut buffer: Vec<u8> = Vec::new();
    let mut total = 0usize;
    let mut bytes = vec![0u8; 64 * 1024];
    // How much of `buffer` is known to hold no newline: a long line is looked
    // through once as it arrives, not again with every read.
    let mut scanned = 0usize;
    // When the incomplete line began: the bridge sets its timer once, then,
    // and clears it only when a line is complete.
    let mut began: Option<Instant> = None;
    loop {
        let read = if buffer.is_empty() {
            stdout.read(&mut bytes).await
        } else {
            let deadline = *began.get_or_insert_with(Instant::now) + line_timeout;
            match tokio::time::timeout_at(deadline, stdout.read(&mut bytes)).await {
                Ok(read) => read,
                Err(_) => {
                    let _ = inputs.send(Input::Broken("Incomplete structured message timed out."));
                    return;
                }
            }
        };
        let count = match read {
            Ok(0) | Err(_) => {
                if !String::from_utf8_lossy(&buffer).trim().is_empty() {
                    let _ = inputs.send(Input::Broken("Structured stream ended mid-message."));
                }
                let _ = inputs.send(Input::Ended);
                return;
            }
            Ok(count) => count,
        };
        total += count;
        if total > MAX_NATIVE_TOTAL {
            let _ = inputs.send(Input::Broken("Structured stream exceeded its size limit."));
            return;
        }
        buffer.extend_from_slice(&bytes[..count]);
        while let Some(offset) = buffer[scanned..].iter().position(|b| *b == b'\n') {
            let end = scanned + offset;
            scanned = 0;
            began = None;
            let line: Vec<u8> = buffer.drain(..=end).collect();
            let line = String::from_utf8_lossy(&line[..line.len() - 1]).into_owned();
            if line.len() > MAX_NATIVE_LINE {
                let _ = inputs.send(Input::Broken("Structured message exceeded its size limit."));
                return;
            }
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<Value>(&line) {
                Ok(message) => {
                    if inputs.send(Input::Native(message)).is_err() {
                        return;
                    }
                }
                Err(_) => {
                    let _ = inputs.send(Input::Broken("Invalid structured message received."));
                    return;
                }
            }
        }
        scanned = buffer.len();
        if buffer.len() > MAX_NATIVE_LINE {
            let _ = inputs.send(Input::Broken("Structured message exceeded its size limit."));
            return;
        }
    }
}

/// The conversation's task: every input, handled to the end, in order.
pub(super) async fn run<C: NativeClient>(
    mut client: C,
    mut inputs: mpsc::UnboundedReceiver<Input>,
    exited: &'static str,
) {
    loop {
        // The error and the failed turn were emitted with the oversized event.
        if std::mem::take(&mut client.core().oversized) || client.core().backlogged {
            client.shutdown();
        }
        if client.core().closed {
            break;
        }
        let deadline = client.core().next_deadline();
        let input = tokio::select! {
            input = inputs.recv() => input,
            _ = tokio::time::sleep_until(deadline.unwrap_or_else(Instant::now)), if deadline.is_some() => {
                client.expire(Instant::now());
                continue;
            }
        };
        let Some(input) = input else {
            client.shutdown();
            break;
        };
        match input {
            Input::Start(init, launched) => {
                client.start(init);
                let _ = launched.send(());
            }
            Input::Shutdown => {
                client.shutdown();
                break;
            }
            Input::Native(message) => client.native(message),
            Input::Broken(message) => client.failure(message),
            // The client's output ended: it is gone, or going.
            Input::Ended => client.failure(exited),
            command => client.command(command),
        }
    }
    let core = client.core();
    core.stop_process().await;
    core.oversized = false;
    core.emit(json!({"type":"exit"}));
    core.finished = true;
}
