//! SessionsEngine — per-chat agent runs: dispatch, steering, interrupts, input bridging,
//! journal + broadcast fan-out, and 120ms coalesced doc streaming.
//!
//! Run processing guarantees:
//! - every `AgentEvent` is (a) appended to the on-disk run journal, (b) broadcast to
//!   in-process subscribers, (c) folded via `fold_event_into_parts` and diffed into the
//!   chat's `SessionDoc` through `SegmentWriter` on a coalesced `STREAM_COMMIT_MS` timer;
//! - the user message entry is pushed to the doc immediately on dispatch (id = the
//!   command's client-minted message id, so optimistic echoes never flicker);
//! - a `Steered` event splits the assistant entry at the exact boundary;
//! - recovery (interrupt or a stale journal at boot) stamps the streaming entry `aborted`.
//!
//! Scope notes: sessions are keyed by chat id (one live run per chat). A 15s liveness
//! heartbeat runs in `drive_run`; there is deliberately no stall watchdog because agents
//! may legitimately wait far longer than any fixed timeout, and a live child is the
//! working signal.
//! Every dying path must instead carry its own visible error (child crash with stderr,
//! spawn failure, stream error, engine-restart recovery).

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};

use chrono::Utc;
use futures::StreamExt;
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio::process::Command;
use tokio::sync::{broadcast, mpsc, oneshot, watch};

use jolt_harness::{
    BashMessage, BashRequest, BashResult, CancellationToken, Harness, RunControls, SteerMessage,
};
use jolt_proto::{
    AgentEvent, DoneStatus, HarnessId, RunRequest, Session, SessionStatus, ToolCall,
    UserInputAnswer, UserInputQuestion,
};
use jolt_session_doc::{
    DocError, MessagePart, MessageRole, MessageStatus, STREAM_COMMIT_MS, SegmentWriter, SessionDoc,
    fold_event_into_parts, sanitize_tool_call,
};

use crate::doc_host::{ChatDocHandle, DocHost};
use crate::mcp::{McpHost, McpLease};
use crate::registry::HarnessRegistry;
use crate::run_journal::RunJournal;
use crate::usage::{UsageContext, UsageStore};
use crate::{EngineError, new_id, now_ms};

/// One journaled event: the durable seq plus the event, as broadcast to subscribers.
#[derive(Debug, Clone)]
pub struct JournaledEvent {
    pub seq: u64,
    pub event: AgentEvent,
}

/// Outcome of a steer attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SteerOutcome {
    /// Delivered into the live run's steering mailbox.
    Accepted,
    /// No live steerable run — the caller should dispatch the prompt as a new turn.
    NotSteerable,
}

type PendingInputs = Arc<Mutex<HashMap<String, oneshot::Sender<Vec<UserInputAnswer>>>>>;

fn begin_input_request(
    pending: &PendingInputs,
    engine_tx: &mpsc::UnboundedSender<AgentEvent>,
    questions: Vec<UserInputQuestion>,
) -> (String, oneshot::Receiver<Vec<UserInputAnswer>>) {
    let (tx, rx) = oneshot::channel();
    let request_id = new_id();
    lock(pending).insert(request_id.clone(), tx);
    let _ = engine_tx.send(AgentEvent::InputRequested {
        request_id: request_id.clone(),
        questions,
    });
    (request_id, rx)
}

struct PendingInputGuard {
    pending: PendingInputs,
    engine_tx: mpsc::UnboundedSender<AgentEvent>,
    request_id: String,
}

impl PendingInputGuard {
    fn new(
        pending: PendingInputs,
        engine_tx: mpsc::UnboundedSender<AgentEvent>,
        request_id: String,
    ) -> Self {
        Self {
            pending,
            engine_tx,
            request_id,
        }
    }
}

impl Drop for PendingInputGuard {
    fn drop(&mut self) {
        if lock(&self.pending).remove(&self.request_id).is_some() {
            let _ = self.engine_tx.send(AgentEvent::InputResolved {
                request_id: self.request_id.clone(),
            });
        }
    }
}

mod inner;
mod runs;

use inner::*;

const CONTINUE_AFTER_COMPACTION_PROMPT: &str = "Compaction has just completed, but the session stopped before work resumed. Resume the existing task rather than waiting for another user prompt.\n\nBefore continuing:\n\n1. Reconstruct the original goal, user constraints, decisions made, files changed, commands and tests run, unresolved issues, and intended next action from the compacted conversation.\n2. Reconcile that context with the current repository state. Treat the worktree as authoritative for file state and the conversation as authoritative for user intent.\n3. Briefly state the context you recovered.\n4. Immediately perform the next unfinished step. Do not stop after the recap or ask the user to repeat context unless it is genuinely unavailable or ambiguous.";

#[derive(Default)]
struct CompactionFollowUp {
    armed: AtomicBool,
}

impl CompactionFollowUp {
    fn observe_agent_event(&self, event: &AgentEvent) {
        match event {
            AgentEvent::CompactionFinished => self.armed.store(true, Ordering::SeqCst),
            AgentEvent::TextDelta { .. }
            | AgentEvent::ReasoningDelta { .. }
            | AgentEvent::AssistantMessageCompleted { .. }
            | AgentEvent::ToolCall { .. }
            | AgentEvent::Steered { .. } => self.cancel_for_user_message(),
            _ => {}
        }
    }

    fn cancel_for_user_message(&self) {
        self.armed.store(false, Ordering::SeqCst);
    }

    fn take_on_shutdown(&self) -> bool {
        self.armed.swap(false, Ordering::SeqCst)
    }
}

/// Harness-native continuations are scoped by chat, harness, and cwd.
type HarnessSessionKey = (String, HarnessId, String);

struct RunHandle {
    run_id: String,
    harness: HarnessId,
    steerable: bool,
    steer_tx: mpsc::Sender<SteerMessage>,
    bash_tx: Option<mpsc::Sender<BashMessage>>,
    /// Harness-level cancellation (protocol interrupt + child teardown).
    interrupt_token: CancellationToken,
    /// Engine-level cancel: arms the run task's grace deadline so a harness that
    /// ignores its token can never strand the run.
    cancel: watch::Sender<bool>,
    engine_tx: mpsc::UnboundedSender<AgentEvent>,
    pending_inputs: PendingInputs,
    compaction_follow_up: Arc<CompactionFollowUp>,
    pending_external_turns: Arc<AtomicUsize>,
    turn_diff_tracker: Arc<Mutex<TurnDiffTracker>>,
    /// True only while the persistent process is parked with no internal
    /// continuation already queued.
    idle: Arc<AtomicBool>,
    /// Maintenance retirement is clean at an idle boundary: unlike interrupt,
    /// it does not abort a transcript entry or pause an active goal.
    retire: CancellationToken,
}

/// Owns baseline transitions for one persistent harness run. Keeping queue
/// mechanics behind named boundary operations prevents dispatch, steering and
/// shutdown paths from independently manipulating positional state.
#[derive(Default)]
struct TurnDiffTracker {
    active: Option<crate::TurnDiffBaseline>,
    queued: VecDeque<Option<crate::TurnDiffBaseline>>,
}

impl TurnDiffTracker {
    fn new(initial: Option<crate::TurnDiffBaseline>) -> Self {
        Self {
            active: initial,
            queued: VecDeque::new(),
        }
    }

    fn queue(&mut self, baseline: Option<crate::TurnDiffBaseline>) {
        self.queued.push_back(baseline);
    }

    fn rollback_last_queue(&mut self) {
        self.queued.pop_back();
    }

    fn active(&self) -> Option<crate::TurnDiffBaseline> {
        self.active.clone()
    }

    fn activate_queued_if_needed(&mut self) {
        if self.active.is_none() {
            self.active = self.queued.pop_front().flatten();
        }
    }

    /// A Steered event is the authoritative boundary. The queued snapshot was
    /// taken before the old segment settled, so discard it in favor of the
    /// boundary snapshot.
    fn observe_boundary(&mut self, baseline: Option<crate::TurnDiffBaseline>) {
        self.queued.pop_front();
        self.active = baseline;
    }

    fn advance_after_done(&mut self) {
        self.active = self.queued.pop_front().flatten();
    }

    fn install_if_missing(&mut self, baseline: Option<crate::TurnDiffBaseline>) {
        if self.active.is_none() {
            self.active = baseline;
        }
    }
}

struct Inner {
    device_id: String,
    journal: Arc<RunJournal>,
    registry: Arc<HarnessRegistry>,
    mcp: McpHost,
    usage: UsageStore,
    usage_contexts: Mutex<HashMap<String, UsageContext>>,
    doc_host: OnceLock<DocHost>,
    /// chat_id → live run.
    runs: Mutex<HashMap<String, RunHandle>>,
    /// Harnesses fenced while their installed CLI is being replaced. Durable
    /// commands remain pending until the fence is lifted.
    maintenance: Mutex<HashSet<HarnessId>>,
    /// Chats whose previous turn completed cleanly (or whose paused queue was
    /// explicitly resumed) and are draining the whole pending queue batch.
    queued_turn_drains: Mutex<HashSet<String>>,
    /// chat_id → broadcast hub (retained across runs so subscribers survive turns).
    hubs: Mutex<HashMap<String, broadcast::Sender<JournaledEvent>>>,
    statuses: Mutex<HashMap<String, Session>>,
    sessions_tx: watch::Sender<Vec<Session>>,
    /// Last dispatched request per chat — the steer→new-turn fallback re-derives its
    /// run config from this (chat config rows land with the workspace doc in M4).
    last_requests: Mutex<HashMap<String, RunRequest>>,
    /// Harness-native conversations keyed by chat, harness, and cwd. The
    /// workspace row retains every key independently so switching away and
    /// back resumes the original native conversation.
    harness_sessions: Mutex<HashMap<HarnessSessionKey, jolt_proto::HarnessConversationRef>>,
    /// Auto-titler for untitled chats (wired at engine assembly; absent in bare tests).
    titles: OnceLock<crate::titles::TitleGenerator>,
    /// Immutable per-assistant-entry filesystem deltas (desktop-only viewer).
    turn_diffs: OnceLock<crate::TurnDiffStore>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn join_context(left: Option<String>, right: Option<String>) -> Option<String> {
    match (left, right) {
        (Some(left), Some(right)) => Some(format!("{left}\n\n{right}")),
        (left, right) => left.or(right),
    }
}

#[derive(Clone)]
pub struct SessionsEngine {
    inner: Arc<Inner>,
}

impl SessionsEngine {
    pub fn new(
        device_id: String,
        journal: Arc<RunJournal>,
        registry: Arc<HarnessRegistry>,
        usage: UsageStore,
    ) -> Self {
        let (sessions_tx, _) = watch::channel(Vec::new());
        Self {
            inner: Arc::new(Inner {
                device_id,
                journal,
                registry,
                mcp: McpHost::new(),
                usage,
                usage_contexts: Mutex::new(HashMap::new()),
                doc_host: OnceLock::new(),
                runs: Mutex::new(HashMap::new()),
                maintenance: Mutex::new(HashSet::new()),
                queued_turn_drains: Mutex::new(HashSet::new()),
                hubs: Mutex::new(HashMap::new()),
                statuses: Mutex::new(HashMap::new()),
                sessions_tx,
                last_requests: Mutex::new(HashMap::new()),
                harness_sessions: Mutex::new(HashMap::new()),
                titles: OnceLock::new(),
                turn_diffs: OnceLock::new(),
            }),
        }
    }

    pub fn set_turn_diffs(&self, store: crate::TurnDiffStore) {
        let _ = self.inner.turn_diffs.set(store);
    }

    async fn capture_turn_diff_baseline(
        &self,
        chat_id: &str,
        cwd: &str,
    ) -> Option<crate::TurnDiffBaseline> {
        capture_turn_diff_baseline(&self.inner, chat_id, cwd).await
    }

    pub async fn turn_diff_page(
        &self,
        chat_id: &str,
        assistant_message_id: &str,
        catalog_revision: &str,
        page_id: &str,
    ) -> Result<Option<jolt_proto::CheckoutDiffPage>, EngineError> {
        let store = self
            .inner
            .turn_diffs
            .get()
            .ok_or_else(|| EngineError::Other("turn diff store unavailable".into()))?;
        store
            .page(chat_id, assistant_message_id, catalog_revision, page_id)
            .await
    }

    /// Delete locally retained patch bodies after their owning chat is deleted.
    pub async fn remove_turn_diffs(&self, chat_id: &str) -> Result<(), EngineError> {
        let Some(store) = self.inner.turn_diffs.get() else {
            return Ok(());
        };
        store.remove_chat(chat_id).await
    }

    /// Wire the doc host (called once at engine assembly; the two services are mutually
    /// referential by design — sessions stream into docs, docs execute commands here).
    pub fn set_doc_host(&self, host: DocHost) {
        let _ = self.inner.doc_host.set(host);
    }

    /// Wire the chat auto-titler (called once at engine assembly). After each
    /// completed exchange the run task fires it for still-untitled chats.
    pub fn set_titles(&self, titles: crate::titles::TitleGenerator) {
        let _ = self.inner.titles.set(titles);
    }

    /// Regenerate an existing chat title from its first user prompt using the
    /// host device's configured harness and economy model.
    pub async fn regenerate_title(&self, chat_id: &str) -> Result<(), EngineError> {
        let host =
            self.inner.doc_host.get().ok_or_else(|| {
                EngineError::Other("doc host not wired into sessions engine".into())
            })?;
        let workspace = host
            .workspace()
            .ok_or_else(|| EngineError::Other("workspace not wired into doc host".into()))?;
        let chat = workspace
            .chat(chat_id)?
            .ok_or_else(|| EngineError::Other("chat has no workspace row".into()))?;
        if chat.device_id != self.inner.device_id {
            return Err(EngineError::Other(
                "chat title must be regenerated on its host device".into(),
            ));
        }
        let cwd = chat
            .cwd
            .as_deref()
            .ok_or_else(|| EngineError::Other("chat has no workspace folder".into()))?;
        let prompt = self
            .doc_handle(chat_id)?
            .doc()
            .read_entries()?
            .into_iter()
            .find_map(|entry| {
                if entry.role != MessageRole::User {
                    return None;
                }
                let text = entry
                    .parts
                    .into_iter()
                    .filter_map(|part| match part {
                        MessagePart::Text { text, .. } => Some(text),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                (!text.trim().is_empty()).then_some(text)
            })
            .ok_or_else(|| EngineError::Other("chat has no user prompt to title".into()))?;
        let titles = self
            .inner
            .titles
            .get()
            .ok_or_else(|| EngineError::Other("chat title generator unavailable".into()))?;
        titles
            .regenerate(chat_id, host.harness_for(chat_id), &prompt, cwd)
            .await
    }

    fn doc_handle(&self, chat_id: &str) -> Result<Arc<ChatDocHandle>, EngineError> {
        let host =
            self.inner.doc_host.get().ok_or_else(|| {
                EngineError::Other("doc host not wired into sessions engine".into())
            })?;
        host.open(chat_id)
    }

    /// Status watch: the full session list, re-sent on every transition.
    pub fn watch_sessions(&self) -> watch::Receiver<Vec<Session>> {
        self.inner.sessions_tx.subscribe()
    }

    pub fn session_status(&self, chat_id: &str) -> Option<Session> {
        lock(&self.inner.statuses).get(chat_id).cloned()
    }

    pub fn watch_usage(
        &self,
        chat_id: &str,
    ) -> rusqlite::Result<watch::Receiver<jolt_proto::UsageSummary>> {
        self.inner.usage.watch_chat(chat_id)
    }

    pub fn usage_breakdown(&self, days: u16) -> rusqlite::Result<jolt_proto::UsageBreakdown> {
        self.inner.usage.breakdown(days)
    }

    pub(crate) fn usage_store(&self) -> UsageStore {
        self.inner.usage.clone()
    }

    /// Any run currently working or blocked on input — the auto-updater's
    /// "don't restart from under a session" gate.
    pub fn any_active(&self) -> bool {
        lock(&self.inner.statuses).values().any(|s| {
            matches!(
                s.status,
                jolt_proto::SessionStatus::Working | jolt_proto::SessionStatus::AwaitingInput
            )
        })
    }

    pub(crate) fn set_harness_maintenance(&self, harness: HarnessId, enabled: bool) {
        let mut maintenance = lock(&self.inner.maintenance);
        if enabled {
            maintenance.insert(harness);
        } else {
            maintenance.remove(&harness);
        }
    }

    pub(crate) fn harness_in_maintenance(&self, harness: HarnessId) -> bool {
        lock(&self.inner.maintenance).contains(&harness)
    }

    /// `(busy, idle)` live process counts for one harness. An idle process has
    /// completed its visible turn and has no engine-owned continuation queued.
    pub(crate) fn harness_run_counts(&self, harness: HarnessId) -> (usize, usize) {
        lock(&self.inner.runs)
            .values()
            .filter(|run| run.harness == harness)
            .fold((0, 0), |(busy, idle), run| {
                if run.idle.load(Ordering::Acquire) {
                    (busy, idle + 1)
                } else {
                    (busy + 1, idle)
                }
            })
    }

    /// Ask every genuinely idle process for this harness to retire. Busy runs
    /// are left untouched and become eligible when they reach a clean boundary.
    pub(crate) fn retire_idle_harness(&self, harness: HarnessId) -> usize {
        let retire: Vec<_> = lock(&self.inner.runs)
            .values()
            .filter(|run| run.harness == harness && run.idle.load(Ordering::Acquire))
            .map(|run| run.retire.clone())
            .collect();
        let count = retire.len();
        for token in retire {
            token.cancel();
        }
        count
    }

    /// The last request dispatched for a chat (steer→new-turn fallback).
    pub fn last_request(&self, chat_id: &str) -> Option<RunRequest> {
        lock(&self.inner.last_requests).get(chat_id).cloned()
    }

    /// Whether a clean turn boundary has opened this chat's queue drain. The
    /// gate stays open while every item in that FIFO batch is routed into the
    /// harness mailbox, even after the first item marks the session Working.
    pub(crate) fn queued_turn_ready(&self, chat_id: &str) -> bool {
        lock(&self.inner.queued_turn_drains).contains(chat_id)
    }

    /// Close a queue drain once the command ledger has no more eligible items.
    pub(crate) fn finish_queued_turn_drain(&self, chat_id: &str) {
        lock(&self.inner.queued_turn_drains).remove(chat_id);
    }

    /// Explicitly resume a queue paused by an interruption or error. Ignore a
    /// stale resume that races another turn becoming active.
    pub(crate) fn resume_queued_turns(&self, chat_id: &str) {
        let busy = lock(&self.inner.statuses)
            .get(chat_id)
            .is_some_and(|session| {
                matches!(
                    session.status,
                    SessionStatus::Working | SessionStatus::AwaitingInput
                )
            });
        if !busy {
            lock(&self.inner.queued_turn_drains).insert(chat_id.to_string());
        }
    }

    /// Subscribe to a chat's live event stream: returns the journal replay after
    /// `after_seq` plus a live receiver. Subscribe-then-replay ordering means overlap
    /// (dedupe by seq) rather than gaps.
    pub fn subscribe(
        &self,
        chat_id: &str,
        after_seq: u64,
    ) -> Result<(Vec<JournaledEvent>, broadcast::Receiver<JournaledEvent>), EngineError> {
        let rx = {
            let mut hubs = lock(&self.inner.hubs);
            hubs.entry(chat_id.to_string())
                .or_insert_with(|| broadcast::channel(1024).0)
                .subscribe()
        };
        let replay = self
            .inner
            .journal
            .replay(chat_id, after_seq)?
            .into_iter()
            .map(|(seq, event)| JournaledEvent { seq, event })
            .collect();
        Ok((replay, rx))
    }
}

#[cfg(test)]
mod tests;
