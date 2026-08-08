//! jolt-harness — the common interface and runtime controls for agent CLI adapters.
//!
//! Integration details (docs/harnesses.md):
//! - Claude Code: spawn the installed `claude` CLI with
//!   `--input-format stream-json --output-format stream-json --verbose
//!    --include-partial-messages`, implement the control channel (can_use_tool →
//!   requestInput, interrupt, set_model), steer by writing user lines mid-run.
//! - Codex: spawn `codex app-server`, JSON-RPC 2.0 over stdio (thread/start, turn/start,
//!   turn/steer{expectedTurnId}, turn/interrupt, item/* + delta notifications).
//! - Pi: spawn `pi --mode rpc`, use its LF-delimited command/event protocol for dynamic
//!   provider models, persistent sessions, steering, extension input, and abort.

use async_trait::async_trait;
use futures::stream::BoxStream;
use tokio::sync::{mpsc, oneshot};
pub use tokio_util::sync::CancellationToken;

use jolt_proto::{
    AgentCommand, AgentEvent, CommandContext, HarnessId, Model, ReasoningLevel, RunRequest,
    SteeringMode, UserInputAnswer, UserInputQuestion,
};

#[derive(Debug, thiserror::Error)]
pub enum HarnessError {
    #[error("harness binary not found: {0}")]
    NotInstalled(String),
    #[error("harness protocol error: {0}")]
    Protocol(String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("harness environment: {0}")]
    Environment(String),
}

/// A steer prompt pushed into a live run; delivered at the harness's steering boundary.
pub struct SteerMessage {
    pub prompt: String,
    pub message_id: Option<String>,
}

/// A user-invoked shell command (`!` / `!!`) executed without starting an
/// agent turn. Native harnesses consume this directly; Jolt provides the
/// fallback for others.
#[derive(Debug, Clone)]
pub struct BashRequest {
    pub command: String,
    pub cwd: String,
    pub resume: Option<String>,
    pub model_options: serde_json::Map<String, serde_json::Value>,
    pub exclude_from_context: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BashResult {
    pub output: String,
    pub exit_code: Option<i32>,
    pub cancelled: bool,
    pub truncated: bool,
    pub full_output_path: Option<String>,
    pub session_id: Option<String>,
}

pub struct BashMessage {
    pub request: BashRequest,
    pub response: oneshot::Sender<Result<BashResult, HarnessError>>,
}

/// Ephemeral configuration for Jolt's product-owned MCP server.
#[derive(Clone)]
pub struct McpServerConfig {
    pub name: String,
    pub url: String,
    pub bearer_token: String,
}

/// Environment variables reserved for the product-owned MCP connection.
pub const MCP_BEARER_TOKEN_ENV: &str = "JOLT_MCP_BEARER_TOKEN";
pub const MCP_URL_ENV: &str = "JOLT_MCP_URL";

/// Host-side controls handed to a run: input-request bridge + steering mailbox.
pub struct RunControls {
    /// Whether the harness should retain this run in its native session store.
    /// Disable only for internal one-shot work such as title generation.
    pub persist_session: bool,
    /// Product-owned MCP configuration scoped to this live harness process.
    pub mcp: Option<McpServerConfig>,
    /// The run sends questions and awaits answers (blocks the agent, mirrors jolt).
    pub request_input: Box<
        dyn Fn(Vec<UserInputQuestion>) -> oneshot::Receiver<Vec<UserInputAnswer>> + Send + Sync,
    >,
    /// Steer prompts consumed at step/turn boundaries.
    pub steering: mpsc::Receiver<SteerMessage>,
    /// Harness-native shell commands consumed independently of agent turns.
    /// Harnesses without native support may leave the receiver unread.
    pub bash: mpsc::Receiver<BashMessage>,
    /// Cancel to interrupt the live run: the harness sends its protocol-level
    /// interrupt, then escalates to SIGTERM/SIGKILL on the child after a grace
    /// period. The run's stream ends with `Done { status: Interrupted }`.
    pub interrupt: CancellationToken,
}

#[async_trait]
pub trait Harness: Send + Sync {
    fn id(&self) -> HarnessId;
    fn display_name(&self) -> &str;
    /// Resolve the executable this adapter would launch. Maintenance features
    /// use the exact same path as model discovery and agent runs.
    fn executable_path(&self) -> Result<std::path::PathBuf, HarnessError> {
        Err(HarnessError::NotInstalled(self.display_name().to_string()))
    }
    fn supports_steering(&self) -> bool;
    /// Whether this harness accepts additive Streamable HTTP MCP configuration.
    fn supports_mcp(&self) -> bool {
        false
    }
    /// Whether shell execution is native and records included output in the
    /// harness session. Other harnesses use Jolt's local fallback.
    fn supports_native_bash(&self) -> bool {
        false
    }
    fn steering_mode(&self) -> SteeringMode;
    fn reasoning_levels(&self) -> &[ReasoningLevel];
    async fn models(&self) -> Result<Vec<Model>, HarnessError>;
    /// Commands available in this harness for the given project directory.
    /// Discovery is lazy because loading harness resources may execute startup code.
    async fn commands(&self, _context: CommandContext) -> Result<Vec<AgentCommand>, HarnessError> {
        Ok(Vec::new())
    }
    async fn bash(&self, _request: BashRequest) -> Result<BashResult, HarnessError> {
        Err(HarnessError::Protocol(format!(
            "{} does not support direct shell commands",
            self.display_name()
        )))
    }
    /// Run one session; the stream ends with `AgentEvent::Done`. Persistence is
    /// controlled by [`RunControls::persist_session`].
    async fn run(
        &self,
        request: RunRequest,
        controls: RunControls,
    ) -> Result<BoxStream<'static, Result<AgentEvent, HarnessError>>, HarnessError>;
}

pub mod environment;
pub mod mock;
#[doc(hidden)]
pub mod simd_base64;

/// Rolling tail of a child's stderr, shared between the reader task and the
/// crash-message composer: an unexpected exit surfaces "<name> exited
/// unexpectedly (<status>): <last stderr lines>" instead of a bare shrug —
/// a useful background-crash message.
#[derive(Clone, Default)]
#[doc(hidden)]
pub struct StderrTail(std::sync::Arc<std::sync::Mutex<std::collections::VecDeque<String>>>);

impl StderrTail {
    const KEEP_LINES: usize = 6;
    const KEEP_BYTES: usize = 700;

    pub fn push(&self, line: &str) {
        let line = line.trim();
        if line.is_empty() {
            return;
        }
        let mut tail = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        tail.push_back(line.chars().take(Self::KEEP_BYTES).collect());
        while tail.len() > Self::KEEP_LINES {
            tail.pop_front();
        }
    }

    /// The captured tail as one display string, `None` when nothing arrived.
    pub fn snapshot(&self) -> Option<String> {
        let tail = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if tail.is_empty() {
            return None;
        }
        let mut joined = tail.iter().cloned().collect::<Vec<_>>().join("\n");
        joined.truncate(Self::KEEP_BYTES * 2);
        Some(joined)
    }
}

/// "exit code 137" / "signal 9 (killed)" / "unknown" — the status half of a
/// crash message, from a `try_wait` result after the stream ended.
#[doc(hidden)]
pub fn describe_exit(status: Option<std::process::ExitStatus>) -> String {
    let Some(status) = status else {
        return "still running".into();
    };
    if let Some(code) = status.code() {
        return format!("exit code {code}");
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(signal) = status.signal() {
            return format!("killed by signal {signal}");
        }
    }
    "unknown exit".into()
}

/// The full crash message: status plus the stderr tail when there is one.
#[doc(hidden)]
pub fn crash_message(
    name: &str,
    status: Option<std::process::ExitStatus>,
    stderr: &StderrTail,
) -> String {
    let status = describe_exit(status);
    match stderr.snapshot() {
        Some(tail) => format!("{name} exited unexpectedly ({status}): {tail}"),
        None => format!("{name} exited unexpectedly ({status})"),
    }
}
