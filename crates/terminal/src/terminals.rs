//! Terminals — PTY sessions owned by this device, implemented with `portable-pty`.
//!
//! - `open` spawns the user's login shell in the chat's cwd; subscriptions replay a
//!   bounded 1 MiB raw-byte window (resumable via `afterSeq`) then tail live output,
//!   batched at [`TERMINAL_OUTPUT_BATCH_MS`] and sent as binary frames.
//! - Live shells survive subscriber detach — a detached session is the user's
//!   running process, kept until its tab is explicitly closed or the engine exits.
//!   Only EXITED sessions expire (30min TTL on their inert replay buffers), and
//!   [`MAX_TERMINALS`] bounds leakage from renderers that lost their tab state.
//! - Ownership: every IPC or relay caller is the device owner.

use std::collections::{HashMap, VecDeque};
use std::io::{Read, Write};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};
use std::time::Duration;

use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use tokio::sync::mpsc;

use jolt_proto::TerminalSession;
use jolt_session_doc::TERMINAL_OUTPUT_BATCH_MS;

use crate::TerminalError;

const MAX_TERMINALS: usize = 32;
const MAX_INPUT_BYTES: usize = 64 * 1024;
const MAX_LAUNCH_COMMAND_BYTES: usize = 8 * 1024;
const MAX_REPLAY_BYTES: usize = 1024 * 1024;
const MAX_OUTPUT_FRAME_BYTES: usize = 64 * 1024;
const RAW_READER_QUEUE_CAP: usize = 64;
const SUBSCRIBER_QUEUE_CAP: usize = 256;
const EXITED_TTL: Duration = Duration::from_secs(30 * 60);
const REAPER_INTERVAL: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TerminalOutput {
    Data {
        seq: u64,
        data: Arc<[u8]>,
    },
    Exit {
        seq: u64,
        exit_code: i32,
        signal: Option<String>,
    },
    ReplayGap {
        requested_after: u64,
        oldest_available: u64,
    },
}

impl TerminalOutput {
    pub fn seq(&self) -> u64 {
        match self {
            Self::Data { seq, .. } | Self::Exit { seq, .. } => *seq,
            Self::ReplayGap {
                oldest_available, ..
            } => oldest_available.saturating_sub(1),
        }
    }

    fn replay_bytes(&self) -> usize {
        match self {
            Self::Data { data, .. } => data.len(),
            Self::Exit { signal, .. } => 16 + signal.as_ref().map_or(0, String::len),
            Self::ReplayGap { .. } => 0,
        }
    }
}

struct LiveTerminal {
    master: Box<dyn portable_pty::MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    killer: Box<dyn portable_pty::ChildKiller + Send + Sync>,
    subscribers: Vec<mpsc::Sender<TerminalOutput>>,
    replay: VecDeque<TerminalOutput>,
    replay_bytes: usize,
    seq: u64,
    last_active_at: std::time::Instant,
    exited: bool,
}

impl LiveTerminal {
    /// Stamp a seq, append to the bounded replay window, and fan out to live
    /// subscribers. On `Exit` the subscriber senders are dropped so every
    /// attached stream ends after delivering the event.
    fn emit(&mut self, event: TerminalOutput) {
        self.last_active_at = std::time::Instant::now();
        self.replay_bytes += event.replay_bytes();
        self.replay.push_back(event.clone());
        while self.replay_bytes > MAX_REPLAY_BYTES && self.replay.len() > 1 {
            if let Some(dropped) = self.replay.pop_front() {
                self.replay_bytes -= dropped.replay_bytes();
            }
        }
        // A slow viewport detaches instead of growing without bound or blocking
        // the PTY. Its sequence cursor reconnects through the replay window.
        self.subscribers
            .retain(|tx| tx.try_send(event.clone()).is_ok());
        if matches!(event, TerminalOutput::Exit { .. }) {
            self.exited = true;
            self.subscribers.clear();
        }
    }

    fn next_seq(&mut self) -> u64 {
        self.seq += 1;
        self.seq
    }
}

struct TerminalsInner {
    sessions: Mutex<HashMap<String, Arc<Mutex<LiveTerminal>>>>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[derive(Clone)]
pub struct Terminals {
    inner: Arc<TerminalsInner>,
}

impl Default for Terminals {
    fn default() -> Self {
        Self::new()
    }
}

fn clamp_size(cols: u16, rows: u16) -> PtySize {
    PtySize {
        cols: cols.clamp(2, 500),
        rows: rows.clamp(1, 300),
        pixel_width: 0,
        pixel_height: 0,
    }
}

/// The user's interactive shell: `$SHELL`, else the platform default.
fn selected_shell() -> String {
    if cfg!(windows) {
        return std::env::var("COMSPEC").unwrap_or_else(|_| "powershell.exe".to_string());
    }
    std::env::var("SHELL")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| {
            if cfg!(target_os = "macos") {
                "/bin/zsh".to_string()
            } else {
                "/bin/bash".to_string()
            }
        })
}

impl Terminals {
    /// Requires a tokio runtime (spawns the exited-session reaper).
    pub fn new() -> Self {
        let terminals = Self {
            inner: Arc::new(TerminalsInner {
                sessions: Mutex::new(HashMap::new()),
            }),
        };
        tokio::spawn(reaper_task(Arc::downgrade(&terminals.inner)));
        terminals
    }

    /// Open a login shell in `cwd`. The PTY outlives every subscriber; it dies on
    /// [`Self::close`], shell exit + TTL, or engine shutdown.
    pub fn open(&self, cwd: &str, cols: u16, rows: u16) -> Result<TerminalSession, TerminalError> {
        self.open_configured(cwd, cols, rows, None, None)
    }

    /// Open a terminal with an optional command interpreted by the user's
    /// login shell. Blank commands retain the default interactive shell.
    pub fn open_with_command(
        &self,
        cwd: &str,
        cols: u16,
        rows: u16,
        command: Option<&str>,
    ) -> Result<TerminalSession, TerminalError> {
        self.open_configured(cwd, cols, rows, None, command)
    }

    /// Explicit shell override (tests use `/bin/sh`).
    pub fn open_with_shell(
        &self,
        cwd: &str,
        cols: u16,
        rows: u16,
        shell: Option<&str>,
    ) -> Result<TerminalSession, TerminalError> {
        self.open_configured(cwd, cols, rows, shell, None)
    }

    fn open_configured(
        &self,
        cwd: &str,
        cols: u16,
        rows: u16,
        shell: Option<&str>,
        command: Option<&str>,
    ) -> Result<TerminalSession, TerminalError> {
        if lock(&self.inner.sessions).len() >= MAX_TERMINALS {
            return Err(TerminalError::new(format!(
                "Too many open terminals (maximum {MAX_TERMINALS})"
            )));
        }
        if !std::fs::metadata(cwd).map(|m| m.is_dir()).unwrap_or(false) {
            return Err(TerminalError::new(
                "Session working directory is unavailable",
            ));
        }

        let shell = shell.map(str::to_string).unwrap_or_else(selected_shell);
        let command = command.map(str::trim).filter(|command| !command.is_empty());
        if command.is_some_and(|command| command.len() > MAX_LAUNCH_COMMAND_BYTES) {
            return Err(TerminalError::new("Terminal launch command is too long"));
        }
        let shell_name = std::path::Path::new(&shell)
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| shell.clone());

        let pty = native_pty_system();
        let pair = pty
            .openpty(clamp_size(cols, rows))
            .map_err(|e| TerminalError::new(format!("could not open a pty: {e}")))?;
        let mut cmd = CommandBuilder::new(&shell);
        if let Some(command) = command {
            if cfg!(windows) {
                let powershell = shell_name.eq_ignore_ascii_case("powershell.exe")
                    || shell_name.eq_ignore_ascii_case("pwsh.exe");
                cmd.arg(if powershell { "-Command" } else { "/C" });
            } else {
                cmd.arg("-lc");
            }
            cmd.arg(command);
        } else if !cfg!(windows) {
            cmd.arg("-l"); // interactive login shell — the user's real PATH/profile
        }
        cmd.cwd(cwd);
        cmd.env("TERM", "xterm-256color");
        cmd.env("COLORTERM", "truecolor");
        cmd.env("TERM_PROGRAM", "Jolt");
        let mut child = pair
            .slave
            .spawn_command(cmd)
            .map_err(|e| TerminalError::new(format!("could not spawn {shell_name}: {e}")))?;
        drop(pair.slave);
        let killer = child.clone_killer();
        let reader = pair
            .master
            .try_clone_reader()
            .map_err(|e| TerminalError::new(format!("pty reader: {e}")))?;
        let writer = pair
            .master
            .take_writer()
            .map_err(|e| TerminalError::new(format!("pty writer: {e}")))?;

        let id = uuid::Uuid::new_v4().to_string();
        let session = Arc::new(Mutex::new(LiveTerminal {
            master: pair.master,
            writer,
            killer,
            subscribers: Vec::new(),
            replay: VecDeque::new(),
            replay_bytes: 0,
            seq: 0,
            last_active_at: std::time::Instant::now(),
            exited: false,
        }));
        lock(&self.inner.sessions).insert(id.clone(), session.clone());

        // Raw PTY bytes: blocking reader thread → batcher task (12ms windows).
        let (raw_tx, raw_rx) = mpsc::channel::<Vec<u8>>(RAW_READER_QUEUE_CAP);
        std::thread::Builder::new()
            .name(format!("pty-read-{id}"))
            .spawn(move || read_pty(reader, raw_tx))
            .map_err(|e| TerminalError::new(format!("pty reader thread: {e}")))?;
        let wait = tokio::task::spawn_blocking(move || child.wait());
        tokio::spawn(pump_output(Arc::downgrade(&session), raw_rx, wait));

        Ok(TerminalSession {
            id,
            cwd: cwd.to_string(),
            shell: shell_name,
        })
    }

    fn session(&self, terminal_id: &str) -> Result<Arc<Mutex<LiveTerminal>>, TerminalError> {
        lock(&self.inner.sessions)
            .get(terminal_id)
            .cloned()
            .ok_or_else(|| TerminalError::new("Terminal not found"))
    }

    /// Replay raw events from `after_seq`, then tail live output. The receiver
    /// is bounded after its initial replay allowance; a lagging subscriber is
    /// detached and can reconnect by sequence.
    pub fn subscribe_output(
        &self,
        terminal_id: &str,
        after_seq: Option<u64>,
    ) -> Result<mpsc::Receiver<TerminalOutput>, TerminalError> {
        let session = self.session(terminal_id)?;
        let mut session = lock(&session);
        session.last_active_at = std::time::Instant::now();
        let after = after_seq.unwrap_or(0);
        let replay_count = session
            .replay
            .iter()
            .filter(|event| event.seq() > after)
            .count();
        let oldest_available = session.replay.front().map(TerminalOutput::seq);
        let has_gap = oldest_available.is_some_and(|oldest| oldest > after.saturating_add(1));
        let (tx, rx) = mpsc::channel(
            replay_count
                .saturating_add(usize::from(has_gap))
                .saturating_add(SUBSCRIBER_QUEUE_CAP),
        );
        if let Some(oldest_available) = oldest_available.filter(|_| has_gap) {
            tx.try_send(TerminalOutput::ReplayGap {
                requested_after: after,
                oldest_available,
            })
            .expect("receiver capacity includes the replay gap marker");
        }
        for event in session.replay.iter().filter(|event| event.seq() > after) {
            tx.try_send(event.clone())
                .expect("receiver capacity includes the complete replay");
        }
        if !session.exited {
            session.subscribers.push(tx);
        }
        // On an exited session `tx` drops here: the stream ends after the replay.
        Ok(rx)
    }

    /// Write input bytes; `data` is base64 (matching `Data` events), with a plain
    /// UTF-8 fallback for lenient callers.
    pub fn write(&self, terminal_id: &str, data: &str) -> Result<(), TerminalError> {
        let bytes = crate::simd_base64::decode(data.as_bytes())
            .unwrap_or_else(|_| data.as_bytes().to_vec());
        if bytes.len() > MAX_INPUT_BYTES {
            return Err(TerminalError::new("Terminal input is too large"));
        }
        let session = self.session(terminal_id)?;
        let mut session = lock(&session);
        if session.exited {
            return Err(TerminalError::new("Terminal has exited"));
        }
        session.last_active_at = std::time::Instant::now();
        session
            .writer
            .write_all(&bytes)
            .and_then(|_| session.writer.flush())
            .map_err(|e| TerminalError::new(format!("Terminal write failed: {e}")))
    }

    pub fn resize(&self, terminal_id: &str, cols: u16, rows: u16) -> Result<(), TerminalError> {
        let session = self.session(terminal_id)?;
        let mut session = lock(&session);
        session.last_active_at = std::time::Instant::now();
        if session.exited {
            return Ok(());
        }
        session
            .master
            .resize(clamp_size(cols, rows))
            .map_err(|e| TerminalError::new(format!("Terminal resize failed: {e}")))
    }

    /// Kill the shell (if still running) and drop the session + replay buffer.
    pub fn close(&self, terminal_id: &str) -> Result<(), TerminalError> {
        let session = lock(&self.inner.sessions)
            .remove(terminal_id)
            .ok_or_else(|| TerminalError::new("Terminal not found"))?;
        dispose(&session, true);
        Ok(())
    }

    /// Any live PTY (the reaper prunes exited ones) — restarts kill shells, so
    /// the auto-updater waits for none.
    pub fn any_open(&self) -> bool {
        !lock(&self.inner.sessions).is_empty()
    }

    /// Engine shutdown: kill every live shell.
    pub fn shutdown(&self) {
        let sessions: Vec<_> = lock(&self.inner.sessions).drain().map(|(_, s)| s).collect();
        for session in sessions {
            dispose(&session, true);
        }
    }
}

fn dispose(session: &Arc<Mutex<LiveTerminal>>, kill: bool) {
    let mut session = lock(session);
    session.subscribers.clear();
    if kill
        && !session.exited
        && let Err(err) = session.killer.kill()
    {
        tracing::debug!(error = %err, "terminal kill failed (already exited?)");
    }
}

/// Blocking PTY reader: forwards raw chunks until EOF. A closed PTY reads as an
/// error on some platforms (EIO on Linux once the shell exits) — both end the loop.
fn read_pty(mut reader: Box<dyn Read + Send>, tx: mpsc::Sender<Vec<u8>>) {
    let mut buf = [0u8; 8192];
    loop {
        match reader.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if tx.blocking_send(buf[..n].to_vec()).is_err() {
                    break;
                }
            }
        }
    }
}

/// Batches raw chunks into `Data` events every [`TERMINAL_OUTPUT_BATCH_MS`], then —
/// once the reader hits EOF (shell gone) — emits the final `Exit` event. Holds only
/// a weak session handle so a closed terminal tears this task down.
async fn pump_output(
    session: Weak<Mutex<LiveTerminal>>,
    mut raw_rx: mpsc::Receiver<Vec<u8>>,
    wait: tokio::task::JoinHandle<Result<portable_pty::ExitStatus, std::io::Error>>,
) {
    let batch = Duration::from_millis(TERMINAL_OUTPUT_BATCH_MS);
    let emit = |buffer: Vec<u8>| -> bool {
        let Some(session) = session.upgrade() else {
            return false;
        };
        let mut session = lock(&session);
        for chunk in buffer.chunks(MAX_OUTPUT_FRAME_BYTES) {
            let seq = session.next_seq();
            session.emit(TerminalOutput::Data {
                seq,
                data: Arc::from(chunk),
            });
        }
        true
    };
    'outer: while let Some(first) = raw_rx.recv().await {
        let mut buffer = first;
        let deadline = tokio::time::Instant::now() + batch;
        loop {
            match tokio::time::timeout_at(deadline, raw_rx.recv()).await {
                Ok(Some(chunk)) => buffer.extend_from_slice(&chunk),
                Ok(None) => {
                    // Reader gone: flush, then fall through to the exit stamp.
                    emit(buffer);
                    break 'outer;
                }
                Err(_) => break, // batch window elapsed
            }
        }
        if !emit(buffer) {
            return; // terminal closed underneath us
        }
    }
    let exit_code = match wait.await {
        Ok(Ok(status)) => status.exit_code() as i32,
        Ok(Err(err)) => {
            tracing::debug!(error = %err, "terminal wait failed");
            -1
        }
        Err(err) => {
            tracing::debug!(error = %err, "terminal wait task failed");
            -1
        }
    };
    if let Some(session) = session.upgrade() {
        let mut session = lock(&session);
        let seq = session.next_seq();
        session.emit(TerminalOutput::Exit {
            seq,
            exit_code,
            signal: None,
        });
    }
}

/// Live shells never expire on idleness — a detached session is the user's running
/// process. Only EXITED sessions are swept after [`EXITED_TTL`]: they're inert
/// replay buffers held so a returning viewer can show the tail + exit status.
async fn reaper_task(inner: Weak<TerminalsInner>) {
    let mut tick = tokio::time::interval(REAPER_INTERVAL);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    tick.tick().await; // consume the immediate first tick
    loop {
        tick.tick().await;
        let Some(inner) = inner.upgrade() else { break };
        let mut sessions = lock(&inner.sessions);
        sessions.retain(|_, session| {
            let session = lock(session);
            !(session.exited && session.last_active_at.elapsed() > EXITED_TTL)
        });
    }
}
