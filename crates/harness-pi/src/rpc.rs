//! Pi RPC client over LF-delimited JSON on stdio.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{ChildStdin, ChildStdout};
use tokio::sync::{mpsc, oneshot};

use jolt_harness::HarnessError;

#[derive(Debug)]
pub(crate) enum Incoming {
    Event(Value),
    Eof,
}

type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value, String>>>>>;

#[derive(Clone)]
pub(crate) struct RpcClient {
    next_id: Arc<AtomicU64>,
    pending: Pending,
    writer: mpsc::UnboundedSender<String>,
}

impl RpcClient {
    pub(crate) fn new(stdin: ChildStdin, stdout: ChildStdout) -> (Self, mpsc::Receiver<Incoming>) {
        let (writer_tx, writer_rx) = mpsc::unbounded_channel();
        tokio::spawn(write_loop(stdin, writer_rx));
        let pending: Pending = Arc::default();
        let (incoming_tx, incoming_rx) = mpsc::channel(256);
        tokio::spawn(read_loop(stdout, Arc::clone(&pending), incoming_tx));
        (
            Self {
                next_id: Arc::new(AtomicU64::new(0)),
                pending,
                writer: writer_tx,
            },
            incoming_rx,
        )
    }

    pub(crate) async fn request(&self, mut command: Value) -> Result<Value, HarnessError> {
        let name = command
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("command")
            .to_owned();
        let object = command.as_object_mut().ok_or_else(|| {
            HarnessError::Protocol(format!("{name}: RPC command must be an object"))
        })?;
        let id = self.next_id.fetch_add(1, Ordering::Relaxed) + 1;
        object.insert("id".into(), Value::from(id));
        let (tx, rx) = oneshot::channel();
        self.pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(id, tx);
        if self.writer.send(command.to_string()).is_err() {
            self.pending
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(&id);
            return Err(HarnessError::Protocol(format!(
                "{name}: Pi RPC stdin closed"
            )));
        }
        match rx.await {
            Ok(Ok(data)) => Ok(data),
            Ok(Err(error)) => Err(HarnessError::Protocol(format!("{name}: {error}"))),
            Err(_) => Err(HarnessError::Protocol(format!(
                "{name}: Pi exited before responding"
            ))),
        }
    }

    pub(crate) fn send(&self, message: Value) {
        let _ = self.writer.send(message.to_string());
    }
}

async fn write_loop(mut stdin: ChildStdin, mut rx: mpsc::UnboundedReceiver<String>) {
    while let Some(line) = rx.recv().await {
        let write = async {
            stdin.write_all(line.as_bytes()).await?;
            stdin.write_all(b"\n").await?;
            stdin.flush().await
        };
        if let Err(error) = write.await {
            tracing::debug!(target: "jolt_harness::pi", "stdin write failed: {error}");
            return;
        }
    }
}

async fn read_loop(stdout: ChildStdout, pending: Pending, tx: mpsc::Sender<Incoming>) {
    let mut lines = BufReader::new(stdout).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        if line.trim().is_empty() {
            continue;
        }
        let Ok(message) = serde_json::from_str::<Value>(&line) else {
            tracing::debug!(target: "jolt_harness::pi", "non-JSON stdout line skipped");
            continue;
        };
        if message.get("type").and_then(Value::as_str) == Some("response")
            && let Some(id) = message.get("id").and_then(Value::as_u64)
        {
            let sender = pending
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(&id);
            if let Some(sender) = sender {
                let success = message.get("success").and_then(Value::as_bool) == Some(true);
                let outcome = if success {
                    Ok(message.get("data").cloned().unwrap_or(Value::Null))
                } else {
                    Err(message
                        .get("error")
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                        .unwrap_or_else(|| "Pi RPC command failed".into()))
                };
                let _ = sender.send(outcome);
            }
            continue;
        }
        if tx.send(Incoming::Event(message)).await.is_err() {
            return;
        }
    }
    pending
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clear();
    let _ = tx.send(Incoming::Eof).await;
}
