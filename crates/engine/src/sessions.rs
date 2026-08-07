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

use jolt_doc::{
    DocError, MessagePart, MessageRole, MessageStatus, STREAM_COMMIT_MS, SegmentWriter, SessionDoc,
    fold_event_into_parts, sanitize_tool_call,
};
use jolt_harness::{
    BashMessage, BashRequest, BashResult, CancellationToken, Harness, RunControls, SteerMessage,
};
use jolt_proto::{
    AgentEvent, DoneStatus, HarnessId, RunRequest, Session, SessionStatus, ToolCall,
    UserInputAnswer, UserInputQuestion,
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

/// A harness-native session id plus the cwd it was created under. Harness
/// session stores are cwd-scoped, so resume is only injected for runs launched
/// from the same cwd.
#[derive(Debug, Clone)]
struct HarnessSessionRef {
    session_id: String,
    cwd: String,
}

struct RunHandle {
    run_id: String,
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
    turn_diff_baselines: Arc<Mutex<VecDeque<Option<crate::TurnDiffBaseline>>>>,
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
    /// Harness-native session ids per chat (resume continuity across turns) —
    /// the live-process cache over the durable copy on the workspace chat row
    /// (jolt kept the same pair on `chats.harness_session_id`). An empty
    /// session id is the "do not resume" tombstone after a rejected resume.
    harness_sessions: Mutex<HashMap<String, HarnessSessionRef>>,
    /// Auto-titler for untitled chats (wired at engine assembly; absent in bare tests).
    titles: OnceLock<crate::titles::TitleGenerator>,
    /// Immutable per-assistant-entry filesystem deltas (desktop-only viewer).
    turn_diffs: OnceLock<crate::TurnDiffStore>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
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

    /// Start (or route) a run for `chat_id`.
    ///
    /// - The user message entry is written to the doc immediately (id = `message_id`).
    /// - A live steerable run receives the prompt as its next turn via the mailbox;
    ///   otherwise any live run is interrupted first — never two runtimes driving
    ///   one chat.
    pub async fn dispatch(
        &self,
        chat_id: &str,
        harness_id: HarnessId,
        request: RunRequest,
        message_id: Option<String>,
    ) -> Result<String, EngineError> {
        self.dispatch_with(chat_id, harness_id, request, message_id, None, true, true)
            .await
    }

    /// Dispatch while prepending host-generated context to the harness prompt
    /// without exposing it in the user's transcript entry.
    pub(crate) async fn dispatch_with_context(
        &self,
        chat_id: &str,
        harness_id: HarnessId,
        request: RunRequest,
        message_id: Option<String>,
        context: Option<String>,
    ) -> Result<String, EngineError> {
        self.dispatch_with(
            chat_id, harness_id, request, message_id, context, true, true,
        )
        .await
    }

    /// Dispatch a control prompt to the harness without writing a user entry.
    pub(crate) async fn dispatch_hidden_with_context(
        &self,
        chat_id: &str,
        harness_id: HarnessId,
        request: RunRequest,
        context: Option<String>,
    ) -> Result<String, EngineError> {
        self.dispatch_with(chat_id, harness_id, request, None, context, true, false)
            .await
    }

    /// [`Self::dispatch`] with resume injection controllable: the failed-resume
    /// retry re-dispatches with `inject_resume = false` so a session id the
    /// harness just rejected can never be re-injected from the journal.
    /// Boxed future: `drive_run` re-enters this for that retry, and the
    /// erasure breaks the opaque-type cycle the recursion would otherwise form.
    fn dispatch_with<'a>(
        &'a self,
        chat_id: &'a str,
        harness_id: HarnessId,
        request: RunRequest,
        message_id: Option<String>,
        context: Option<String>,
        inject_resume: bool,
        write_user_entry: bool,
    ) -> futures::future::BoxFuture<'a, Result<String, EngineError>> {
        Box::pin(self.dispatch_inner(
            chat_id,
            harness_id,
            request,
            message_id,
            context,
            inject_resume,
            write_user_entry,
        ))
    }

    async fn dispatch_inner(
        &self,
        chat_id: &str,
        harness_id: HarnessId,
        mut request: RunRequest,
        message_id: Option<String>,
        context: Option<String>,
        inject_resume: bool,
        write_user_entry: bool,
    ) -> Result<String, EngineError> {
        let turn_prompt = request.prompt.clone();
        let goal_context = self
            .inner
            .workspace()
            .and_then(|workspace| workspace.chat_goal(chat_id))
            .filter(|goal| goal.status == jolt_proto::GoalStatus::Active)
            .map(|goal| crate::goals::context(&goal));
        let context = match (context, goal_context) {
            (Some(existing), Some(goal)) => Some(format!("{existing}\n\n{goal}")),
            (existing, goal) => existing.or(goal),
        };
        if let Some(context) = &context {
            request.prompt = format!("{context}\n\n{turn_prompt}");
        }
        let routed = lock(&self.inner.runs).get(chat_id).map(|h| {
            (
                h.run_id.clone(),
                h.steerable,
                h.steer_tx.clone(),
                h.compaction_follow_up.clone(),
                h.pending_external_turns.clone(),
                h.turn_diff_baselines.clone(),
            )
        });
        if let Some((
            run_id,
            steerable,
            steer_tx,
            compaction_follow_up,
            pending_external_turns,
            turn_diff_baselines,
        )) = routed
        {
            if write_user_entry {
                compaction_follow_up.cancel_for_user_message();
            }
            let message = SteerMessage {
                prompt: request.prompt.clone(),
                message_id: message_id.clone(),
            };
            let turn_diff_baseline = if steerable {
                self.capture_turn_diff_baseline(chat_id, &request.cwd).await
            } else {
                None
            };
            if steerable {
                pending_external_turns.fetch_add(1, Ordering::AcqRel);
                lock(&turn_diff_baselines).push_back(turn_diff_baseline);
            }
            if steerable && steer_tx.try_send(message).is_ok() {
                if write_user_entry {
                    let user_id = message_id.unwrap_or_else(new_id);
                    let handle = self.doc_handle(chat_id)?;
                    handle.write_user_message(&user_id, &turn_prompt, now_ms())?;
                }
                // Working BEFORE the lastMessageAt bump: both ride the
                // workspace doc from this one peer, so causal order makes it
                // impossible for an observer to hold [new message, old status]
                // — that gap read as unseen-with-no-live-run = a phantom
                // "completed" flash on every remote send (2026-07-31).
                self.set_status(chat_id, SessionStatus::Working, false);
                if write_user_entry {
                    self.inner.note_message(chat_id, &turn_prompt);
                }
                return Ok(run_id);
            }
            if steerable {
                pending_external_turns.fetch_sub(1, Ordering::AcqRel);
                lock(&turn_diff_baselines).pop_back();
            }
            // Mailbox closed (runtime mid-teardown / non-steering harness): replace it.
            self.interrupt(chat_id).await?;
        }

        let harness = self.inner.registry.resolve(harness_id)?;
        let handle = self.doc_handle(chat_id)?;
        let user_id = message_id.unwrap_or_else(new_id);
        if write_user_entry {
            handle.write_user_message(&user_id, &turn_prompt, now_ms())?;
        }

        // Engine-owned resume: callers always send `resume: None`; every
        // dispatch reads the chat's stored harness session and threads it back
        // in so a new
        // process (app restart) continues the same harness conversation.
        let mut resume_injected = false;
        if request.resume.is_none() && inject_resume {
            request.resume = self.inner.resume_for(chat_id, &request.cwd);
            resume_injected = request.resume.is_some();
        }
        let mut saved_request = request.clone();
        saved_request.prompt = turn_prompt.clone();
        lock(&self.inner.last_requests).insert(chat_id.to_string(), saved_request);

        let run_id = new_id();
        let (steer_tx, steer_rx) = mpsc::channel::<SteerMessage>(32);
        let (bash_tx, bash_rx) = mpsc::channel::<BashMessage>(32);
        let (cancel_tx, cancel_rx) = watch::channel(false);
        let (engine_tx, engine_rx) = mpsc::unbounded_channel::<AgentEvent>();
        let pending_inputs: PendingInputs = Arc::new(Mutex::new(HashMap::new()));
        let answer_requester: crate::mcp::McpAnswerRequester = {
            let pending = pending_inputs.clone();
            let engine_tx = engine_tx.clone();
            Arc::new(move |questions, cancellation| {
                let (request_id, mut answers) =
                    begin_input_request(&pending, &engine_tx, questions);
                let guard = PendingInputGuard::new(pending.clone(), engine_tx.clone(), request_id);
                Box::pin(async move {
                    let result = tokio::select! {
                        result = &mut answers => result.ok(),
                        () = cancellation.cancelled() => None,
                    };
                    drop(guard);
                    result
                })
            })
        };
        let mcp_lease = if harness.supports_mcp() {
            match self
                .inner
                .mcp
                .lease(
                    chat_id.to_string(),
                    self.inner.workspace().cloned(),
                    Some(answer_requester),
                )
                .await
            {
                Ok(lease) => Some(lease),
                Err(error) => {
                    tracing::warn!(
                        harness = ?harness_id,
                        %error,
                        "MCP listener unavailable; starting run without Jolt MCP"
                    );
                    None
                }
            }
        } else {
            None
        };
        let compaction_follow_up = Arc::new(CompactionFollowUp::default());
        let pending_external_turns = Arc::new(AtomicUsize::new(0));
        let turn_diff_baselines = Arc::new(Mutex::new(VecDeque::new()));
        let initial_turn_diff_baseline =
            self.capture_turn_diff_baseline(chat_id, &request.cwd).await;

        // Native harness questions and Jolt's MCP answer tool share one pending-input
        // registry, event stream, durable response command, and composer UI.
        let request_input = {
            let pending = pending_inputs.clone();
            let engine_tx = engine_tx.clone();
            Box::new(move |questions: Vec<UserInputQuestion>| {
                begin_input_request(&pending, &engine_tx, questions).1
            })
        };
        let interrupt_token = CancellationToken::new();
        let controls = RunControls {
            persist_session: true,
            mcp: mcp_lease.as_ref().map(McpLease::config),
            request_input,
            steering: steer_rx,
            bash: bash_rx,
            interrupt: interrupt_token.clone(),
        };

        lock(&self.inner.runs).insert(
            chat_id.to_string(),
            RunHandle {
                run_id: run_id.clone(),
                steerable: harness.supports_steering(),
                steer_tx,
                bash_tx: harness.supports_native_bash().then_some(bash_tx),
                interrupt_token,
                cancel: cancel_tx,
                engine_tx,
                pending_inputs,
                compaction_follow_up: compaction_follow_up.clone(),
                pending_external_turns: pending_external_turns.clone(),
                turn_diff_baselines: turn_diff_baselines.clone(),
            },
        );
        self.set_status(chat_id, SessionStatus::Working, true);
        // AFTER Working (same causal-order guarantee as the steer path): the
        // lastMessageAt bump must never be observable ahead of the live run.
        if write_user_entry {
            self.inner.note_message(chat_id, &turn_prompt);

            // Name the chat NOW, off the first prompt — not after the first
            // exchange completes ("called New session for a long time for no
            // reason"; the titler only needs the prompt and skips titled chats;
            // the Done-time call below stays as the retry for a failed
            // generation).
            if let Some(titles) = self.inner.titles.get() {
                titles.maybe_generate(chat_id, harness_id, &turn_prompt, &request.cwd);
            }
        }

        tokio::spawn(drive_run(
            self.inner.clone(),
            chat_id.to_string(),
            run_id.clone(),
            harness,
            request,
            handle.doc_arc(),
            controls,
            mcp_lease,
            engine_rx,
            cancel_rx,
            compaction_follow_up,
            pending_external_turns,
            turn_diff_baselines,
            initial_turn_diff_baseline,
            RunResumeState {
                user_message_id: user_id,
                resume_injected,
                turn_prompt,
                context,
                write_user_entry,
            },
        ));
        Ok(run_id)
    }

    /// Whether this harness records included shell output in its own session.
    pub(crate) fn bash_context_is_native(
        &self,
        harness_id: HarnessId,
    ) -> Result<bool, EngineError> {
        Ok(self
            .inner
            .registry
            .resolve(harness_id)?
            .supports_native_bash())
    }

    /// Execute a user shell command without starting an agent turn. Pi uses
    /// its native session-aware RPC; other harnesses use Jolt's local Bash.
    pub async fn bash(
        &self,
        chat_id: &str,
        harness_id: HarnessId,
        mut request: BashRequest,
    ) -> Result<BashResult, EngineError> {
        if request.resume.is_none() {
            request.resume = self.inner.resume_for(chat_id, &request.cwd);
        }
        let target = lock(&self.inner.runs)
            .get(chat_id)
            .and_then(|handle| handle.bash_tx.clone());
        let result = if let Some(target) = target {
            let (response, result) = oneshot::channel();
            target
                .send(BashMessage {
                    request: request.clone(),
                    response,
                })
                .await
                .map_err(|_| EngineError::Other("Pi shell command channel closed".into()))?;
            result
                .await
                .map_err(|_| EngineError::Other("Pi shell command was cancelled".into()))??
        } else {
            let harness = self.inner.registry.resolve(harness_id)?;
            if harness.supports_native_bash() {
                harness.bash(request.clone()).await?
            } else {
                run_local_bash(&request).await?
            }
        };
        if let Some(session_id) = &result.session_id {
            self.inner
                .remember_harness_session(chat_id, session_id, &request.cwd);
        }
        Ok(result)
    }

    /// Push a steer prompt into the live run's mailbox. `NotSteerable` when no live
    /// steerable run exists — the caller (command executor) dispatches a new turn.
    pub async fn steer(
        &self,
        chat_id: &str,
        prompt: &str,
        message_id: Option<String>,
    ) -> Result<SteerOutcome, EngineError> {
        self.steer_with_context(chat_id, prompt, message_id, None)
            .await
    }

    pub(crate) async fn steer_with_context(
        &self,
        chat_id: &str,
        prompt: &str,
        message_id: Option<String>,
        context: Option<String>,
    ) -> Result<SteerOutcome, EngineError> {
        let target = lock(&self.inner.runs)
            .get(chat_id)
            .filter(|h| h.steerable)
            .map(|h| {
                (
                    h.steer_tx.clone(),
                    h.compaction_follow_up.clone(),
                    h.pending_external_turns.clone(),
                    h.turn_diff_baselines.clone(),
                )
            });
        let Some((steer_tx, compaction_follow_up, pending_external_turns, turn_diff_baselines)) =
            target
        else {
            return Ok(SteerOutcome::NotSteerable);
        };
        compaction_follow_up.cancel_for_user_message();
        let goal_context = self
            .inner
            .workspace()
            .and_then(|workspace| workspace.chat_goal(chat_id))
            .filter(|goal| goal.status == jolt_proto::GoalStatus::Active)
            .map(|goal| crate::goals::context(&goal));
        let context = match (context, goal_context) {
            (Some(existing), Some(goal)) => Some(format!("{existing}\n\n{goal}")),
            (existing, goal) => existing.or(goal),
        };
        let harness_prompt = context
            .map(|context| format!("{context}\n\n{prompt}"))
            .unwrap_or_else(|| prompt.to_string());
        let message = SteerMessage {
            prompt: harness_prompt,
            message_id: message_id.clone(),
        };
        let cwd = lock(&self.inner.last_requests)
            .get(chat_id)
            .map(|request| request.cwd.clone());
        let turn_diff_baseline = match cwd {
            Some(cwd) => self.capture_turn_diff_baseline(chat_id, &cwd).await,
            None => None,
        };
        pending_external_turns.fetch_add(1, Ordering::AcqRel);
        lock(&turn_diff_baselines).push_back(turn_diff_baseline);
        if steer_tx.try_send(message).is_err() {
            pending_external_turns.fetch_sub(1, Ordering::AcqRel);
            lock(&turn_diff_baselines).pop_back();
            return Ok(SteerOutcome::NotSteerable);
        }
        // Accepted: the steer prompt becomes a user entry immediately (client-minted id).
        let user_id = message_id.unwrap_or_else(new_id);
        let handle = self.doc_handle(chat_id)?;
        handle.write_user_message(&user_id, prompt, now_ms())?;
        self.inner.note_message(chat_id, prompt);
        Ok(SteerOutcome::Accepted)
    }

    /// Interrupt the live run, if any. The run settles with a synthetic
    /// `Done{interrupted}` and its streaming entry stamped `aborted`; this waits
    /// (bounded) for that settlement so callers observe a consistent doc.
    pub async fn interrupt(&self, chat_id: &str) -> Result<bool, EngineError> {
        let target = lock(&self.inner.runs).get(chat_id).map(|h| {
            (
                h.run_id.clone(),
                h.interrupt_token.clone(),
                h.cancel.clone(),
                h.pending_inputs.clone(),
            )
        });
        let Some((run_id, token, cancel, pending)) = target else {
            return Ok(false);
        };
        // Unpark any blocked question FIRST (mirrors jolt: harness teardown can await a
        // parked question callback — a run stuck on a question would deadlock the stop).
        let parked: Vec<_> = lock(&pending).drain().map(|(_, tx)| tx).collect();
        for tx in parked {
            let _ = tx.send(Vec::new());
        }
        // Harness-level interrupt (protocol + child teardown) …
        token.cancel();
        // … plus the engine-side grace deadline in the run task, so a harness that
        // ignores its token still settles with a synthesized Done{interrupted}.
        let _ = cancel.send(true);
        // Bounded settle wait (the run task appends Done + stamps `aborted`).
        for _ in 0..500 {
            if !self.is_live(chat_id, &run_id) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        Ok(true)
    }

    /// Resolve a pending `request_input` question set. Returns `false` when no such
    /// request is pending (unknown id, or the run already settled).
    pub fn respond_input(
        &self,
        chat_id: &str,
        request_id: &str,
        answers: Vec<UserInputAnswer>,
    ) -> Result<bool, EngineError> {
        let target = lock(&self.inner.runs)
            .get(chat_id)
            .map(|h| (h.pending_inputs.clone(), h.engine_tx.clone()));
        let Some((pending, engine_tx)) = target else {
            return Ok(false);
        };
        let Some(resolver) = lock(&pending).remove(request_id) else {
            return Ok(false);
        };
        let _ = resolver.send(answers);
        let _ = engine_tx.send(AgentEvent::InputResolved {
            request_id: request_id.to_string(),
        });
        Ok(true)
    }

    /// Boot recovery: for every journal whose last event is not `Done` (a run died
    /// mid-stream), stamp this device's abandoned `streaming` doc entries `aborted`
    /// with a VISIBLE "Run interrupted by engine restart" error part, close the
    /// journal with a synthetic `Done{interrupted}` — and then PICK THE RUN BACK
    /// UP: a fresh crashed turn with revival budget left is re-dispatched against
    /// the remembered harness session (jolt: "not just eulogized";
    /// `MAX_AUTO_RESUME` = 3 consecutive revivals, fresh = crashed < 12h ago).
    pub fn recover_stale(&self) -> Result<usize, EngineError> {
        const MAX_AUTO_RESUME: u32 = 3;
        const RESUME_FRESH_MS: i64 = 12 * 60 * 60 * 1000;

        let stale = self.inner.journal.stale_sessions()?;
        let mut recovered = 0usize;
        for chat_id in stale {
            if lock(&self.inner.runs).contains_key(&chat_id) {
                continue; // a live run owns this journal
            }
            let handle = self.doc_handle(&chat_id)?;
            // Harness continuity first: the crashed run's session id may only
            // exist in the journal (the debounced workspace-row write may
            // never have landed) — remember it so the revived run resumes the
            // same harness conversation.
            if let Some((session_id, cwd)) = self.inner.journal_harness_session(&chat_id) {
                self.inner
                    .remember_harness_session(&chat_id, &session_id, &cwd);
            }
            // The revival prompt: the last user message (idempotent re-dispatch
            // under the SAME id — `write_user_message` dedupes by id, so the
            // transcript never shows a duplicate).
            let prompt = handle.doc().read_entries().ok().and_then(|entries| {
                entries
                    .iter()
                    .rev()
                    .find(|e| e.role == MessageRole::User)
                    .and_then(|e| {
                        e.parts.iter().find_map(|p| match p {
                            MessagePart::Text { text, .. } => Some((e.id.clone(), text.clone())),
                            _ => None,
                        })
                    })
            });
            let attempts = self.inner.journal.resume_attempts(&chat_id);
            let fresh = handle
                .doc()
                .read_entries()
                .ok()
                .and_then(|entries| {
                    entries
                        .iter()
                        .rev()
                        .find(|e| {
                            e.role == MessageRole::Assistant
                                && e.status == Some(MessageStatus::Streaming)
                        })
                        .map(|e| now_ms() - e.created_at < RESUME_FRESH_MS)
                })
                .unwrap_or(false);
            let goal_paused_after_restart = self
                .inner
                .workspace()
                .and_then(|workspace| workspace.chat_goal(&chat_id))
                .is_some_and(|goal| {
                    goal.status == jolt_proto::GoalStatus::Paused
                        && goal.status_message.as_deref() == Some("Paused after Jolt restarted")
                });
            let will_resume = fresh
                && prompt.is_some()
                && attempts < MAX_AUTO_RESUME
                && !goal_paused_after_restart;

            let note = if will_resume {
                "Run interrupted by engine restart — resuming"
            } else {
                "Run interrupted by engine restart"
            };
            let done = AgentEvent::Done {
                status: DoneStatus::Interrupted,
                result: None,
                error: Some(note.into()),
                session_id: None,
            };
            self.inner.publish(&chat_id, &done);
            let stamped = handle.mark_abandoned_streams(note)?.len();
            self.set_status(&chat_id, SessionStatus::Idle, false);
            tracing::info!(chat = %chat_id, stamped, will_resume, attempts, "recovered stale session journal");
            recovered += 1;

            if !will_resume {
                continue;
            }
            let attempt = self.inner.journal.note_resume_attempt(&chat_id);
            let (user_id, prompt_text) = prompt.expect("gated by will_resume");
            let sessions = self.clone();
            tokio::spawn(async move {
                let Some(host) = sessions.inner.doc_host.get().cloned() else {
                    return;
                };
                let request = sessions
                    .last_request(&chat_id)
                    .or_else(|| host.request_from_chat_row(&chat_id, &prompt_text))
                    // Last resort: use the journal's own cwd because a crash can
                    // predate the debounced workspace-row write.
                    .or_else(|| {
                        let (_, cwd) = sessions.inner.journal_harness_session(&chat_id)?;
                        Some(RunRequest {
                            prompt: String::new(),
                            model: None,
                            reasoning: None,
                            model_options: Default::default(),
                            cwd,
                            sandbox: jolt_proto::SandboxLevel::WorkspaceWrite,
                            auto_approve: false,
                            attachments: Vec::new(),
                            resume: None,
                        })
                    });
                let Some(mut request) = request else {
                    tracing::warn!(chat = %chat_id, "auto-resume skipped: no run config");
                    return;
                };
                request.prompt = prompt_text;
                request.resume = None; // dispatch re-injects the remembered session
                request.attachments = Vec::new();
                let harness_id = host.harness_for(&chat_id);
                match sessions
                    .dispatch(&chat_id, harness_id, request, Some(user_id))
                    .await
                {
                    Ok(_) => {
                        tracing::info!(chat = %chat_id, attempt, "auto-resumed crashed run")
                    }
                    Err(err) => {
                        tracing::warn!(chat = %chat_id, error = %err, "auto-resume dispatch failed")
                    }
                }
            });
        }
        Ok(recovered)
    }

    /// Graceful shutdown: interrupt every live run so streaming entries settle.
    pub async fn shutdown(&self) {
        let chats: Vec<String> = lock(&self.inner.runs).keys().cloned().collect();
        for chat_id in chats {
            if let Err(err) = self.interrupt(&chat_id).await {
                tracing::warn!(chat = %chat_id, error = %err, "shutdown interrupt failed");
            }
        }
        self.inner.mcp.shutdown().await;
    }

    fn is_live(&self, chat_id: &str, run_id: &str) -> bool {
        lock(&self.inner.runs)
            .get(chat_id)
            .is_some_and(|h| h.run_id == run_id)
    }

    fn set_status(&self, chat_id: &str, status: SessionStatus, fresh_start: bool) {
        self.inner.set_status(chat_id, status, fresh_start);
    }
}

impl Inner {
    /// Journal + broadcast one event (the two unconditional legs of the pipeline).
    fn publish(&self, chat_id: &str, event: &AgentEvent) -> u64 {
        let seq = match self.journal.append(chat_id, event) {
            Ok(seq) => seq,
            Err(err) => {
                tracing::error!(chat = %chat_id, error = %err, "journal append failed");
                0
            }
        };
        match event {
            AgentEvent::SessionStarted {
                harness,
                model,
                cwd,
                ..
            } => {
                lock(&self.usage_contexts).insert(
                    chat_id.to_string(),
                    UsageContext {
                        harness: *harness,
                        model: model.clone(),
                        cwd: cwd.clone(),
                    },
                );
            }
            AgentEvent::Usage { .. } if seq != 0 => {
                let context = lock(&self.usage_contexts).get(chat_id).cloned();
                if let Some(context) = context
                    && let Err(error) = self.usage.record(chat_id, seq, &context, event)
                {
                    tracing::error!(chat = %chat_id, %error, "usage ledger write failed");
                }
            }
            _ => {}
        }
        if let Some(hub) = lock(&self.hubs).get(chat_id) {
            let _ = hub.send(JournaledEvent {
                seq,
                event: event.clone(),
            });
        }
        seq
    }

    /// Bump the session's freshness on stream activity WITHOUT a status
    /// transition. Long silent-LOOKING stretches (thinking heartbeats, a big
    /// tool input being generated) still carry events — the UI's 45s
    /// staleness gate must not flip "Working" off mid-run. Throttled: a
    /// workspace-doc mirror per delta would be far too chatty.
    fn touch_session(&self, chat_id: &str) {
        const TOUCH_THROTTLE_MS: i64 = 10_000;
        let now = Utc::now();
        let session = {
            let mut statuses = lock(&self.statuses);
            let Some(entry) = statuses.get_mut(chat_id) else {
                return;
            };
            let age = now
                .signed_duration_since(entry.updated_at)
                .num_milliseconds();
            if age < TOUCH_THROTTLE_MS {
                return;
            }
            entry.updated_at = now;
            let session = entry.clone();
            let mut list: Vec<Session> = statuses.values().cloned().collect();
            list.sort_by(|a, b| a.chat_id.cmp(&b.chat_id));
            self.sessions_tx.send_replace(list);
            session
        };
        if let Some(ws) = self.workspace() {
            ws.record_session(&session);
        }
    }

    fn set_compacting(&self, chat_id: &str, compacting: bool) {
        let now = Utc::now();
        let session = {
            let mut statuses = lock(&self.statuses);
            let Some(entry) = statuses.get_mut(chat_id) else {
                return;
            };
            if entry.compacting == compacting {
                return;
            }
            entry.compacting = compacting;
            entry.updated_at = now;
            let session = entry.clone();
            let mut list: Vec<Session> = statuses.values().cloned().collect();
            list.sort_by(|a, b| a.chat_id.cmp(&b.chat_id));
            self.sessions_tx.send_replace(list);
            session
        };
        if let Some(ws) = self.workspace() {
            ws.record_session(&session);
        }
    }

    fn set_status(&self, chat_id: &str, status: SessionStatus, fresh_start: bool) {
        let now = Utc::now();
        let session = {
            let mut statuses = lock(&self.statuses);
            let entry = statuses
                .entry(chat_id.to_string())
                .or_insert_with(|| Session {
                    chat_id: chat_id.to_string(),
                    device_id: self.device_id.clone(),
                    status,
                    compacting: false,
                    started_at: None,
                    updated_at: now,
                });
            entry.status = status;
            entry.updated_at = now;
            if fresh_start {
                entry.started_at = Some(now);
            }
            if status != SessionStatus::Working || fresh_start {
                entry.compacting = false;
            }
            let session = entry.clone();
            let mut list: Vec<Session> = statuses.values().cloned().collect();
            list.sort_by(|a, b| a.chat_id.cmp(&b.chat_id));
            // send_replace: keep the current value fresh even with no receivers,
            // so late WatchSessions subscribers see the last transition.
            self.sessions_tx.send_replace(list);
            session
        };
        // Mirror the transition into the workspace doc's session-status row so
        // remote devices' sidebars show this run (staleness-checked client-side).
        if let Some(ws) = self.workspace() {
            ws.record_session(&session);
        }
    }

    fn workspace(&self) -> Option<&crate::workspace_host::WorkspaceHost> {
        self.doc_host.get().and_then(|host| host.workspace())
    }

    /// Sidebar freshness: push a message-persist preview into the chat's workspace row.
    fn note_message(&self, chat_id: &str, text: &str) {
        if text.is_empty() {
            return;
        }
        if let Some(ws) = self.workspace() {
            ws.note_message(chat_id, text);
        }
    }

    /// Record the chat's harness-native session id (and its cwd): live-process
    /// cache plus the durable workspace chat row, which survives an engine
    /// restart.
    fn remember_harness_session(&self, chat_id: &str, session_id: &str, cwd: &str) {
        if session_id.is_empty() {
            return;
        }
        lock(&self.harness_sessions).insert(
            chat_id.to_string(),
            HarnessSessionRef {
                session_id: session_id.to_string(),
                cwd: cwd.to_string(),
            },
        );
        if let Some(ws) = self.workspace() {
            ws.set_chat_harness_session(chat_id, session_id, cwd);
        }
    }

    /// A harness rejected the stored session id: tombstone it (empty string on
    /// the row, cleared cache) so no lookup source — including the journal,
    /// which still names the dead id — can re-inject it.
    fn forget_harness_session(&self, chat_id: &str) {
        lock(&self.harness_sessions).insert(
            chat_id.to_string(),
            HarnessSessionRef {
                session_id: String::new(),
                cwd: String::new(),
            },
        );
        if let Some(ws) = self.workspace() {
            ws.set_chat_harness_session(chat_id, "", "");
        }
    }

    /// The session id to resume for a run in `chat_id` launching from `cwd`:
    /// live-process cache → workspace chat row → journal scan (the crash path
    /// where the debounced row write never landed — SessionStarted/Done events
    /// are journaled per event, flushed immediately). Cwd-gated throughout:
    /// harness session stores are keyed by cwd, so a session created elsewhere
    /// never rides `--resume`. An empty stored id is the explicit tombstone —
    /// no resume, no falling through to staler sources.
    fn resume_for(&self, chat_id: &str, cwd: &str) -> Option<String> {
        let cwd_ok = |session_cwd: &str| session_cwd.is_empty() || session_cwd == cwd;
        if let Some(known) = lock(&self.harness_sessions).get(chat_id).cloned() {
            return (!known.session_id.is_empty() && cwd_ok(&known.cwd))
                .then_some(known.session_id);
        }
        if let Some(ws) = self.workspace()
            && let Some((session_id, session_cwd)) = ws.chat_harness_session(chat_id)
        {
            return (!session_id.is_empty() && cwd_ok(session_cwd.as_deref().unwrap_or("")))
                .then_some(session_id);
        }
        let (session_id, session_cwd) = self.journal_harness_session(chat_id)?;
        // Cache the journal hit (memory + row) so later dispatches skip the scan.
        self.remember_harness_session(chat_id, &session_id, &session_cwd);
        cwd_ok(&session_cwd).then_some(session_id)
    }

    /// The last harness session id named anywhere in the chat's journal, with
    /// the cwd of the `SessionStarted` that governs it. `Done.session_id`
    /// inherits the cwd of the most recent `SessionStarted` (same run).
    fn journal_harness_session(&self, chat_id: &str) -> Option<(String, String)> {
        let events = match self.journal.replay(chat_id, 0) {
            Ok(events) => events,
            Err(err) => {
                tracing::warn!(chat = %chat_id, error = %err, "journal scan for harness session failed");
                return None;
            }
        };
        let mut current_cwd = String::new();
        let mut found: Option<(String, String)> = None;
        for (_, event) in events {
            match event {
                AgentEvent::SessionStarted {
                    session_id, cwd, ..
                } => {
                    current_cwd = cwd;
                    if !session_id.is_empty() {
                        found = Some((session_id, current_cwd.clone()));
                    }
                }
                AgentEvent::Done {
                    session_id: Some(session_id),
                    ..
                } if !session_id.is_empty() => {
                    found = Some((session_id, current_cwd.clone()));
                }
                _ => {}
            }
        }
        found
    }

    fn remove_run(&self, chat_id: &str, run_id: &str) {
        let mut runs = lock(&self.runs);
        if runs.get(chat_id).is_some_and(|h| h.run_id == run_id) {
            runs.remove(chat_id);
        }
    }
}

// ── run task ────────────────────────────────────────────────────────────────

/// Apply the render-parts privacy policy: strip heavy/sensitive tool inputs before doc
/// entry. Full inputs live only in the local run journal.
fn render_parts(parts: &[MessagePart]) -> Vec<MessagePart> {
    parts
        .iter()
        .map(|part| match part {
            MessagePart::Tool {
                id,
                call,
                is_error,
                resolved,
            } => MessagePart::Tool {
                id: id.clone(),
                call: sanitize_tool_call(call),
                is_error: *is_error,
                resolved: *resolved,
            },
            other => other.clone(),
        })
        .collect()
}

/// The persisted assistant text of a folded segment (workspace preview source).
fn folded_text(parts: &[MessagePart]) -> String {
    parts
        .iter()
        .filter_map(|part| match part {
            MessagePart::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn sync_segment<'a>(
    doc: &'a SessionDoc,
    writer: &mut Option<SegmentWriter<'a>>,
    entry_id: &str,
    device_id: &str,
    started_at: i64,
    folded: &[MessagePart],
) -> Result<(), DocError> {
    if folded.is_empty() {
        return Ok(());
    }
    let rendered = render_parts(folded);
    if writer.is_none() {
        *writer = Some(SegmentWriter::begin(doc, entry_id, device_id, started_at)?);
    }
    if let Some(w) = writer.as_mut() {
        w.sync(&rendered)?;
    }
    Ok(())
}

fn finish_segment<'a>(
    doc: &'a SessionDoc,
    writer: Option<SegmentWriter<'a>>,
    entry_id: &str,
    device_id: &str,
    started_at: i64,
    folded: &[MessagePart],
    status: MessageStatus,
) -> Result<(), DocError> {
    let rendered = render_parts(folded);
    match writer {
        Some(w) => w.finish(&rendered, status),
        None if !folded.is_empty() => {
            SegmentWriter::begin(doc, entry_id, device_id, started_at)?.finish(&rendered, status)
        }
        None => Ok(()),
    }
}

const BASH_TRANSCRIPT_MAX_BYTES: u64 = 50 * 1024;

fn bash_executable() -> PathBuf {
    let system_bash = Path::new("/bin/bash");
    if system_bash.exists() {
        system_bash.to_path_buf()
    } else {
        PathBuf::from("bash")
    }
}

async fn run_local_bash(request: &BashRequest) -> Result<BashResult, EngineError> {
    let output_path = std::env::temp_dir().join(format!("jolt-bash-{}.log", new_id()));
    let stdout = std::fs::File::create(&output_path)?;
    let stderr = stdout.try_clone()?;
    let executable = bash_executable();
    let mut command = Command::new(&executable);
    command
        .args(["-lc", &request.command])
        .current_dir(&request.cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr));
    if let Some(path) = jolt_harness::shell_env::login_shell_path() {
        command.env("PATH", path);
    }

    let status = match command.status().await {
        Ok(status) => status,
        Err(error) => {
            let _ = tokio::fs::remove_file(&output_path).await;
            return Err(error.into());
        }
    };
    let output_result: Result<(Vec<u8>, bool), std::io::Error> = async {
        let output_len = tokio::fs::metadata(&output_path).await?.len();
        let truncated = output_len > BASH_TRANSCRIPT_MAX_BYTES;
        let mut output_file = tokio::fs::File::open(&output_path).await?;
        if truncated {
            output_file
                .seek(std::io::SeekFrom::Start(
                    output_len - BASH_TRANSCRIPT_MAX_BYTES,
                ))
                .await?;
        }
        let mut output = Vec::with_capacity(output_len.min(BASH_TRANSCRIPT_MAX_BYTES) as usize);
        output_file.read_to_end(&mut output).await?;
        Ok((output, truncated))
    }
    .await;
    let (output, truncated) = match output_result {
        Ok(output) => output,
        Err(error) => {
            let _ = tokio::fs::remove_file(&output_path).await;
            return Err(error.into());
        }
    };

    let full_output_path = if truncated {
        Some(output_path.to_string_lossy().into_owned())
    } else {
        let _ = tokio::fs::remove_file(&output_path).await;
        None
    };
    Ok(BashResult {
        output: String::from_utf8_lossy(&output).into_owned(),
        exit_code: status.code(),
        cancelled: false,
        truncated,
        full_output_path,
        session_id: None,
    })
}

async fn capture_turn_diff_baseline(
    inner: &Inner,
    chat_id: &str,
    cwd: &str,
) -> Option<crate::TurnDiffBaseline> {
    let store = inner.turn_diffs.get()?;
    match store.capture_baseline(Path::new(cwd)).await {
        Ok(baseline) => Some(baseline),
        Err(error) => {
            tracing::warn!(chat = %chat_id, %error, "turn diff baseline capture failed");
            None
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum TurnMutationScope {
    None,
    Paths(Vec<String>),
}

fn successful_file_mutations(parts: &[MessagePart]) -> TurnMutationScope {
    let mut paths = Vec::new();
    let mut seen = HashSet::new();
    for part in parts {
        let MessagePart::Tool {
            call,
            is_error: false,
            resolved: true,
            ..
        } = part
        else {
            continue;
        };
        let mutation_paths = match call {
            ToolCall::WriteFile { path, .. } | ToolCall::EditFile { path, .. } => {
                std::slice::from_ref(path)
            }
            ToolCall::ApplyPatch {
                path: Some(path), ..
            } => std::slice::from_ref(path),
            ToolCall::ApplyPatch { path: None, paths } => paths,
            _ => continue,
        };
        for path in mutation_paths {
            if !path.trim().is_empty() && seen.insert(path.as_str()) {
                paths.push(path.clone());
            }
        }
    }
    if paths.is_empty() {
        TurnMutationScope::None
    } else {
        TurnMutationScope::Paths(paths)
    }
}

async fn append_turn_diff(
    inner: &Inner,
    chat_id: &str,
    assistant_message_id: &str,
    cwd: &str,
    baseline: Option<&crate::TurnDiffBaseline>,
    folded: &mut Vec<MessagePart>,
) {
    let (Some(store), Some(baseline)) = (inner.turn_diffs.get(), baseline) else {
        return;
    };
    // A checkout can host several concurrent sessions. Restrict the baseline
    // delta to paths this turn explicitly mutated so edits from other sessions
    // are not attributed here. A pathless mutation report cannot safely claim
    // any checkout-wide change and therefore produces no diff card by itself.
    let TurnMutationScope::Paths(paths) = successful_file_mutations(folded) else {
        return;
    };
    match store
        .finalize(
            chat_id,
            assistant_message_id,
            Path::new(cwd),
            baseline,
            &paths,
        )
        .await
    {
        Ok(Some(diff)) => folded.push(MessagePart::Changes {
            id: "changes".into(),
            diff,
        }),
        Ok(None) => {}
        Err(error) => {
            tracing::warn!(chat = %chat_id, message = %assistant_message_id, %error, "turn diff finalization failed");
        }
    }
}

/// Resume bookkeeping for one run task: the turn prompt stays separate from
/// hidden shell context for titling and a failed-resume retry; only
/// engine-injected resume ids are retried fresh.
struct RunResumeState {
    user_message_id: String,
    resume_injected: bool,
    turn_prompt: String,
    context: Option<String>,
    write_user_entry: bool,
}

#[allow(clippy::too_many_arguments)]
async fn drive_run(
    inner: Arc<Inner>,
    chat_id: String,
    run_id: String,
    harness: Arc<dyn Harness>,
    request: RunRequest,
    doc: Arc<SessionDoc>,
    controls: RunControls,
    mcp_lease: Option<McpLease>,
    mut engine_rx: mpsc::UnboundedReceiver<AgentEvent>,
    mut cancel_rx: watch::Receiver<bool>,
    compaction_follow_up: Arc<CompactionFollowUp>,
    pending_external_turns: Arc<AtomicUsize>,
    turn_diff_baselines: Arc<Mutex<VecDeque<Option<crate::TurnDiffBaseline>>>>,
    initial_turn_diff_baseline: Option<crate::TurnDiffBaseline>,
    resume_state: RunResumeState,
) {
    let device_id = inner.device_id.clone();
    // Captured for post-run auto-titling (the request moves into the harness).
    let harness_id = harness.id();
    let user_prompt = resume_state.turn_prompt.clone();
    let run_cwd = request.cwd.clone();
    // Capture the goal before starting the harness. A fast client can call a
    // Jolt MCP goal tool during harness startup, before `run` returns its event
    // stream; that turn must still be charged to the goal active at dispatch.
    let mut goal_turn_started = tokio::time::Instant::now();
    let mut goal_turn_id = inner
        .workspace()
        .and_then(|workspace| workspace.chat_goal(&chat_id))
        .filter(|goal| goal.status == jolt_proto::GoalStatus::Active)
        .map(|goal| goal.id);
    // Kept whole for the failed-resume retry (fresh session, same user entry).
    // Option so the retry branch (inside the event loop) can take ownership.
    let mut retry_request = Some(RunRequest {
        resume: None,
        ..request.clone()
    });
    let mut stream = match harness.run(request, controls).await {
        Ok(stream) => stream,
        Err(err) => {
            let message = err.to_string();
            let goal_signal = mcp_lease.as_ref().and_then(McpLease::take_goal_signal);
            finish_goal_turn(
                &inner,
                &chat_id,
                goal_turn_id.as_deref(),
                DoneStatus::Errored,
                Some(&message),
                goal_signal,
                0,
                goal_turn_started
                    .elapsed()
                    .as_millis()
                    .min(u128::from(u64::MAX)) as u64,
            );
            inner.publish(
                &chat_id,
                &AgentEvent::Error {
                    message: message.clone(),
                },
            );
            inner.publish(
                &chat_id,
                &AgentEvent::Done {
                    status: DoneStatus::Errored,
                    result: None,
                    error: Some(message),
                    session_id: None,
                },
            );
            inner.remove_run(&chat_id, &run_id);
            inner.set_status(&chat_id, SessionStatus::Errored, false);
            return;
        }
    };

    let doc_ref: &SessionDoc = &doc;
    let mut folded: Vec<MessagePart> = Vec::new();
    let mut entry_id = new_id();
    let mut active_turn_diff_baseline = initial_turn_diff_baseline;
    let mut segment_started = now_ms();
    let mut writer: Option<SegmentWriter<'_>> = None;
    let mut dirty = false;
    let mut flush_at = tokio::time::Instant::now();
    // Set when the engine interrupts the run: the harness gets this long to end its own
    // stream (its token was cancelled); past it, a terminal Done is synthesized.
    let mut interrupt_deadline: Option<tokio::time::Instant> = None;
    let mut interrupted = false;
    let mut saw_session_started = false;
    // Liveness heartbeat: this loop RUNNING is proof the harness stream is
    // open, so freshness must not depend on events arriving. Silent stretches
    // are normal and UNBOUNDED — a long tool call, redacted thinking, an
    // agent waiting on an external process, a question parked for an hour —
    // and each starved the UI's 45s staleness gate in turn (working strip /
    // AwaitingInput dot vanishing mid-run, both user-reported). No stall
    // timeout here by design (a first port was rejected — agents may
    // legitimately be quiet for >10min): a live child means Working, dying
    // paths each carry their own error, and engine death stops these ticks
    // so the gate still catches real crashes. touch_session throttles at 10s.
    let mut live_heartbeat = tokio::time::interval(std::time::Duration::from_secs(15));
    live_heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // PERSISTENT SESSION (jolt runsBySession): a completed turn on a
    // steerable harness parks here instead of ending the run — the child and
    // its steering mailbox stay warm, and the next user message (dispatch
    // routes into a live run) starts the next turn with zero respawn/resume
    // latency. `Some(when)` = idle since then; the 30-min reaper below ends
    // a session nobody comes back to (jolt SESSION_IDLE_MS).
    const SESSION_IDLE: std::time::Duration = std::time::Duration::from_secs(30 * 60);
    let mut idle_since: Option<tokio::time::Instant> = None;
    let steerable = harness.supports_steering();
    let mut goal_turn_tokens = 0u64;
    let mut goal_turn_error: Option<String> = None;

    let final_status = loop {
        let event: AgentEvent = tokio::select! {
            biased;
            changed = cancel_rx.changed(), if !interrupted => {
                let _ = changed;
                interrupted = true;
                interrupt_deadline = Some(
                    tokio::time::Instant::now() + std::time::Duration::from_secs(3),
                );
                continue;
            }
            _ = tokio::time::sleep_until(
                interrupt_deadline.unwrap_or_else(tokio::time::Instant::now)
            ), if interrupt_deadline.is_some() => AgentEvent::Done {
                status: DoneStatus::Interrupted,
                result: None,
                error: None,
                session_id: None,
            },
            _ = live_heartbeat.tick() => {
                inner.touch_session(&chat_id);
                continue;
            }
            // Idle reaper (jolt SESSION_IDLE_MS): a parked persistent session
            // nobody returned to in 30 minutes releases its child. The turn
            // was finalized at Done, so this end is clean — no aborted stamp.
            _ = tokio::time::sleep_until(
                idle_since.map(|at| at + SESSION_IDLE).unwrap_or_else(tokio::time::Instant::now)
            ), if idle_since.is_some() => {
                tracing::info!(chat = %chat_id, "reaping idle persistent session");
                if let Some(token) = lock(&inner.runs)
                    .get(&chat_id)
                    .filter(|h| h.run_id == run_id)
                    .map(|h| h.interrupt_token.clone())
                {
                    token.cancel();
                }
                break SessionStatus::Idle;
            }
            Some(event) = engine_rx.recv() => event,
            next = stream.next() => match next {
                Some(Ok(event)) => event,
                Some(Err(err)) => AgentEvent::Done {
                    status: DoneStatus::Errored,
                    result: None,
                    error: Some(err.to_string()),
                    session_id: None,
                },
                None if interrupted => AgentEvent::Done {
                    status: DoneStatus::Interrupted,
                    result: None,
                    error: None,
                    session_id: None,
                },
                // Stream end while PARKED idle: a per-turn adapter closing
                // after its final Done — a clean end, not a crash (the turn
                // was already finalized). Persistent adapters keep the
                // stream open and never hit this.
                None if idle_since.is_some() => break SessionStatus::Idle,
                None => AgentEvent::Done {
                    status: DoneStatus::Errored,
                    result: None,
                    error: Some("harness stream ended without Done".into()),
                    session_id: None,
                },
            },
            _ = tokio::time::sleep_until(flush_at), if dirty => {
                // Coalesced STREAM_COMMIT_MS tick: one doc commit per window.
                if let Err(err) = sync_segment(
                    doc_ref, &mut writer, &entry_id, &device_id, segment_started, &folded,
                ) {
                    tracing::warn!(chat = %chat_id, error = %err, "segment sync failed");
                }
                dirty = false;
                continue;
            }
        };

        // Any stream activity proves the run is alive — keep the session's
        // freshness inside the UI's 45s staleness window (throttled).
        inner.touch_session(&chat_id);
        // First event after parking idle = the next turn beginning (a routed
        // dispatch steered in): the session is Working again.
        if idle_since.take().is_some() {
            goal_turn_started = tokio::time::Instant::now();
            goal_turn_id = inner
                .workspace()
                .and_then(|workspace| workspace.chat_goal(&chat_id))
                .filter(|goal| goal.status == jolt_proto::GoalStatus::Active)
                .map(|goal| goal.id);
            pending_external_turns
                .fetch_update(Ordering::AcqRel, Ordering::Acquire, |pending| {
                    pending.checked_sub(1)
                })
                .ok();
            if active_turn_diff_baseline.is_none() {
                active_turn_diff_baseline = lock(&turn_diff_baselines).pop_front().flatten();
            }
            inner.set_status(&chat_id, SessionStatus::Working, true);
        }
        // Empty reasoning deltas are PURE heartbeats: redacted thinking and
        // tool-input-generation windows stream them with no text. They fold
        // to nothing, so journaling/publishing them is only noise (hundreds
        // per long turn observed) — the touch above already did their job.
        if matches!(&event, AgentEvent::ReasoningDelta { text } if text.is_empty()) {
            continue;
        }
        compaction_follow_up.observe_agent_event(&event);

        // Failed-resume fallback: an engine-injected `--resume` naming a session
        // the harness no longer knows dies before ever starting (claude exits
        // without an init frame; codex falls back internally via thread/start).
        // Signature: errored Done, no SessionStarted, nothing streamed. Retry
        // ONCE as a fresh session against the same user entry — tombstone the
        // dead id first so no lookup source (journal included) re-injects it.
        if resume_state.resume_injected
            && !saw_session_started
            && folded.is_empty()
            && !interrupted
            && matches!(
                &event,
                AgentEvent::Done {
                    status: DoneStatus::Errored,
                    ..
                }
            )
            && let Some(mut retry) = retry_request.take()
        {
            tracing::warn!(
                chat = %chat_id,
                "harness rejected injected resume id; retrying as a fresh session"
            );
            inner.forget_harness_session(&chat_id);
            inner.remove_run(&chat_id, &run_id);
            let engine = SessionsEngine {
                inner: inner.clone(),
            };
            let chat = chat_id.clone();
            let message_id = resume_state.user_message_id.clone();
            retry.prompt = resume_state.turn_prompt.clone();
            let context = resume_state.context.clone();
            let write_user_entry = resume_state.write_user_entry;
            tokio::spawn(async move {
                // `inject_resume = false`: the retry must start fresh. The user
                // entry write inside dispatch is idempotent by message id.
                if let Err(err) = engine
                    .dispatch_with(
                        &chat,
                        harness_id,
                        retry,
                        Some(message_id),
                        context,
                        false,
                        write_user_entry,
                    )
                    .await
                {
                    tracing::error!(chat = %chat, error = %err, "fresh-session retry dispatch failed");
                }
            });
            return;
        }

        // A steer boundary splits the assistant entry exactly where the fold resets.
        if let AgentEvent::Steered {
            next_assistant_message_id,
            ..
        } = &event
        {
            inner.publish(&chat_id, &event);
            append_turn_diff(
                &inner,
                &chat_id,
                &entry_id,
                &run_cwd,
                active_turn_diff_baseline.as_ref(),
                &mut folded,
            )
            .await;
            if let Err(err) = finish_segment(
                doc_ref,
                writer.take(),
                &entry_id,
                &device_id,
                segment_started,
                &folded,
                MessageStatus::Complete,
            ) {
                tracing::warn!(chat = %chat_id, error = %err, "segment finish failed");
            }
            inner.note_message(&chat_id, &folded_text(&folded));
            folded.clear();
            dirty = false;
            entry_id = next_assistant_message_id.clone().unwrap_or_else(new_id);
            segment_started = now_ms();
            // A queued baseline was captured before the steer entered the
            // harness. Recapture at the observed boundary so late work from
            // the previous segment cannot leak into this one.
            lock(&turn_diff_baselines).pop_front();
            active_turn_diff_baseline =
                capture_turn_diff_baseline(&inner, &chat_id, &run_cwd).await;
            continue;
        }

        match &event {
            AgentEvent::SessionStarted {
                session_id, cwd, ..
            } => {
                saw_session_started = true;
                // The event's own cwd (where the harness actually created the
                // session) scopes the stored id, not the request's.
                inner.remember_harness_session(&chat_id, session_id, cwd);
            }
            AgentEvent::Done {
                session_id: Some(session_id),
                ..
            } => {
                inner.remember_harness_session(&chat_id, session_id, &run_cwd);
            }
            AgentEvent::InputRequested { request_id, .. } => {
                // The engine's input bridge is the sole authority on input
                // requests: it mints the id and parks the resolver BEFORE
                // emitting the event, so a legitimate id is always pending
                // here. A harness emitting its own copy (a different id no
                // resolver knows) would fold an unanswerable twin chip into
                // the doc — and answering the twin would never resume the
                // run. Drop such events.
                let pending = lock(&inner.runs)
                    .get(&chat_id)
                    .map(|h| h.pending_inputs.clone());
                let known = pending.is_some_and(|p| lock(&p).contains_key(request_id));
                if !known {
                    tracing::warn!(
                        chat = %chat_id,
                        request = %request_id,
                        "dropping harness-emitted InputRequested (unknown id; \
                         the engine input bridge owns this lifecycle)"
                    );
                    continue;
                }
                inner.set_status(&chat_id, SessionStatus::AwaitingInput, false);
            }
            AgentEvent::InputResolved { .. } => {
                inner.set_status(&chat_id, SessionStatus::Working, false);
            }
            AgentEvent::CompactionStarted => {
                inner.set_compacting(&chat_id, true);
            }
            AgentEvent::CompactionFinished => {
                inner.set_compacting(&chat_id, false);
            }
            AgentEvent::Usage {
                input_tokens,
                output_tokens,
                ..
            } => {
                goal_turn_tokens = goal_turn_tokens
                    .saturating_add(*input_tokens)
                    .saturating_add(*output_tokens);
            }
            AgentEvent::Error { message } => goal_turn_error = Some(message.clone()),
            _ => {}
        }

        inner.publish(&chat_id, &event);

        // A mid-run SessionStarted re-emission from a Claude SDK background
        // invocation must not wipe the segment being written.
        let skip_fold = matches!(&event, AgentEvent::SessionStarted { .. }) && !folded.is_empty();
        if !skip_fold {
            fold_event_into_parts(&mut folded, &event);
        }

        if let AgentEvent::Done { status, error, .. } = &event {
            let goal_signal = mcp_lease.as_ref().and_then(McpLease::take_goal_signal);
            let goal_after_turn = finish_goal_turn(
                &inner,
                &chat_id,
                goal_turn_id.as_deref(),
                *status,
                error.as_deref().or(goal_turn_error.as_deref()),
                goal_signal,
                goal_turn_tokens,
                goal_turn_started
                    .elapsed()
                    .as_millis()
                    .min(u128::from(u64::MAX)) as u64,
            );
            goal_turn_tokens = 0;
            goal_turn_started = tokio::time::Instant::now();
            goal_turn_error = None;
            goal_turn_id = None;
            let message_status = match status {
                DoneStatus::Interrupted => MessageStatus::Aborted,
                DoneStatus::Completed | DoneStatus::Errored => MessageStatus::Complete,
            };
            append_turn_diff(
                &inner,
                &chat_id,
                &entry_id,
                &run_cwd,
                active_turn_diff_baseline.as_ref(),
                &mut folded,
            )
            .await;
            // No dangling chips: a run that ends for ANY reason (completed,
            // errored, interrupted) terminally resolves its input parts — an
            // unresolved question must not outlive the run that asked it
            // (its resolver died with the run; an answer could never land).
            for part in folded.iter_mut() {
                if let MessagePart::Input { resolved, .. } = part {
                    *resolved = true;
                }
            }
            // A Done landing on a PARKED session with nothing streamed (the
            // idle reaper's or an interrupt's own teardown) has no entry to
            // finalize — writing one would leave an empty aborted stub.
            let nothing_streamed = writer.is_none() && folded.is_empty();
            if !nothing_streamed {
                if let Err(err) = finish_segment(
                    doc_ref,
                    writer.take(),
                    &entry_id,
                    &device_id,
                    segment_started,
                    &folded,
                    message_status,
                ) {
                    tracing::warn!(chat = %chat_id, error = %err, "final segment finish failed");
                }
                inner.note_message(&chat_id, &folded_text(&folded));
            }
            if *status == DoneStatus::Completed {
                // A cleanly completed turn resets the auto-resume revival
                // budget: only consecutive crash-revive-crash cycles spend it.
                inner.journal.clear_resume_attempts(&chat_id);
            }
            let continue_after_compaction =
                compaction_follow_up.take_on_shutdown() && *status == DoneStatus::Completed;
            let mut internal_follow_up_queued = false;
            if continue_after_compaction {
                let steer_tx = lock(&inner.runs)
                    .get(&chat_id)
                    .filter(|handle| handle.run_id == run_id)
                    .map(|handle| handle.steer_tx.clone());
                match steer_tx {
                    Some(steer_tx)
                        if steer_tx
                            .try_send(SteerMessage {
                                prompt: CONTINUE_AFTER_COMPACTION_PROMPT.into(),
                                message_id: None,
                            })
                            .is_ok() =>
                    {
                        internal_follow_up_queued = true;
                        tracing::info!(chat = %chat_id, "queued continuation after compaction shutdown");
                    }
                    _ => {
                        tracing::warn!(chat = %chat_id, "could not queue continuation after compaction shutdown");
                    }
                }
            }
            if !internal_follow_up_queued
                && *status == DoneStatus::Completed
                && let Some(goal) =
                    goal_after_turn.filter(|goal| goal.status == jolt_proto::GoalStatus::Active)
            {
                let user_queue_pending =
                    doc_ref
                        .read_commands()
                        .unwrap_or_default()
                        .iter()
                        .any(|command| {
                            command.status == jolt_doc::SessionCommandStatus::Pending
                                && matches!(
                                    command.payload,
                                    jolt_doc::SessionCommandPayload::Queue { .. }
                                )
                        });
                if !user_queue_pending && pending_external_turns.load(Ordering::Acquire) == 0 {
                    let steer_tx = lock(&inner.runs)
                        .get(&chat_id)
                        .filter(|handle| handle.run_id == run_id)
                        .map(|handle| handle.steer_tx.clone());
                    if steer_tx.is_some_and(|steer_tx| {
                        steer_tx
                            .try_send(SteerMessage {
                                prompt: crate::goals::continuation(&goal),
                                message_id: None,
                            })
                            .is_ok()
                    }) {
                        internal_follow_up_queued = true;
                    }
                }
            }
            // Exchange completed on an untitled chat → name it (fire-and-forget;
            // interrupted/errored turns never trigger naming).
            if *status == DoneStatus::Completed
                && resume_state.write_user_entry
                && let Some(titles) = inner.titles.get()
            {
                titles.maybe_generate(&chat_id, harness_id, &user_prompt, &run_cwd);
            }
            // PERSISTENT SESSION: a cleanly completed turn on a steerable
            // harness PARKS instead of ending — child + mailbox stay warm for
            // the next routed dispatch; per-turn state resets for it.
            if *status == DoneStatus::Completed && steerable && !interrupted {
                folded.clear();
                dirty = false;
                entry_id = new_id();
                segment_started = now_ms();
                active_turn_diff_baseline = lock(&turn_diff_baselines).pop_front().flatten();
                if internal_follow_up_queued && active_turn_diff_baseline.is_none() {
                    active_turn_diff_baseline =
                        capture_turn_diff_baseline(&inner, &chat_id, &run_cwd).await;
                }
                // Resume-retry is strictly a first-turn concern.
                saw_session_started = true;
                idle_since = Some(tokio::time::Instant::now());
                inner.set_status(&chat_id, SessionStatus::Idle, false);
                // A hidden compaction continuation already owns the next turn;
                // its eventual clean Done will release the next user queue item.
                if !internal_follow_up_queued {
                    lock(&inner.queued_turn_drains).insert(chat_id.clone());
                    if let Some(host) = inner.doc_host.get() {
                        host.kick_commands(&chat_id);
                    }
                }
                continue;
            }
            break match status {
                DoneStatus::Errored => SessionStatus::Errored,
                _ => SessionStatus::Idle,
            };
        }

        if !folded.is_empty() && !dirty {
            dirty = true;
            flush_at =
                tokio::time::Instant::now() + std::time::Duration::from_millis(STREAM_COMMIT_MS);
        }
    };

    inner.remove_run(&chat_id, &run_id);
    inner.set_status(&chat_id, final_status, false);
}

#[allow(
    clippy::too_many_arguments,
    reason = "goal finalization combines one turn's terminal event, MCP signal, and usage"
)]
fn finish_goal_turn(
    inner: &Inner,
    chat_id: &str,
    started_goal_id: Option<&str>,
    done: DoneStatus,
    error: Option<&str>,
    signal: Option<crate::mcp::McpGoalSignal>,
    tokens: u64,
    elapsed_ms: u64,
) -> Option<jolt_proto::Goal> {
    finish_goal_turn_in_workspace(
        inner.workspace()?,
        chat_id,
        started_goal_id,
        done,
        error,
        signal,
        tokens,
        elapsed_ms,
    )
}

#[allow(
    clippy::too_many_arguments,
    reason = "workspace seam preserves the complete goal-turn finalization input for tests"
)]
fn finish_goal_turn_in_workspace(
    workspace: &crate::workspace_host::WorkspaceHost,
    chat_id: &str,
    started_goal_id: Option<&str>,
    done: DoneStatus,
    error: Option<&str>,
    signal: Option<crate::mcp::McpGoalSignal>,
    tokens: u64,
    elapsed_ms: u64,
) -> Option<jolt_proto::Goal> {
    match workspace.mutate_chat_goal(chat_id, |current| {
        let Some(mut goal) = current else {
            return Ok(None);
        };
        if started_goal_id != Some(goal.id.as_str()) {
            return Ok(Some(goal));
        }

        goal.tokens_used = goal.tokens_used.saturating_add(tokens);
        goal.elapsed_active_ms = goal.elapsed_active_ms.saturating_add(elapsed_ms.max(1));
        goal.turns = goal.turns.saturating_add(1);
        goal.updated_at_ms = now_ms();

        if goal.status == jolt_proto::GoalStatus::Active {
            if done == DoneStatus::Interrupted {
                goal.status = jolt_proto::GoalStatus::Paused;
                goal.pause_source = Some(jolt_proto::GoalPauseSource::System);
                goal.status_message = Some("Goal turn interrupted".into());
            } else if done == DoneStatus::Errored || error.is_some() {
                let message = error.unwrap_or("Harness turn failed");
                goal.status = if message.to_ascii_lowercase().contains("rate")
                    || message.to_ascii_lowercase().contains("quota")
                    || message.to_ascii_lowercase().contains("usage")
                {
                    jolt_proto::GoalStatus::UsageLimited
                } else {
                    jolt_proto::GoalStatus::Paused
                };
                goal.pause_source = (goal.status == jolt_proto::GoalStatus::Paused)
                    .then_some(jolt_proto::GoalPauseSource::System);
                goal.status_message = Some(message.to_string());
            } else if let Some(crate::mcp::McpGoalSignal::Blocked {
                goal_id,
                expected_revision,
                blocker_key,
                summary,
            }) = signal.filter(|signal| match signal {
                crate::mcp::McpGoalSignal::Blocked {
                    goal_id,
                    expected_revision,
                    ..
                } => goal_id == &goal.id && *expected_revision == goal.revision,
            }) {
                debug_assert_eq!(goal_id, goal.id);
                debug_assert_eq!(expected_revision, goal.revision);
                apply_goal_blocker(&mut goal, Some(blocker_key), summary);
            } else {
                goal.blocker_key = None;
                goal.blocker_streak = 0;
            }
        }

        if goal.status == jolt_proto::GoalStatus::Active
            && goal
                .token_budget
                .is_some_and(|budget| goal.tokens_used >= budget)
        {
            goal.status = jolt_proto::GoalStatus::BudgetLimited;
            goal.pause_source = None;
            goal.status_message = Some("Token budget reached".into());
        }
        goal.revision = goal.revision.saturating_add(1);
        Ok(Some(goal))
    }) {
        Ok(goal) => goal,
        Err(error) => {
            tracing::warn!(chat = %chat_id, %error, "goal state write failed");
            workspace.chat_goal(chat_id)
        }
    }
}

fn apply_goal_blocker(goal: &mut jolt_proto::Goal, key: Option<String>, summary: String) {
    if key.is_some() && key == goal.blocker_key {
        goal.blocker_streak = goal.blocker_streak.saturating_add(1);
    } else {
        goal.blocker_key = key;
        goal.blocker_streak = 1;
    }
    if !summary.trim().is_empty() {
        goal.status_message = Some(summary);
    }
    if goal.blocker_key.is_some() && goal.blocker_streak >= 3 {
        goal.status = jolt_proto::GoalStatus::Blocked;
        goal.pause_source = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace_host::WorkspaceHostConfig;

    fn goal_workspace() -> (tempfile::TempDir, crate::workspace_host::WorkspaceHost) {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(jolt_sync::DocsStore::open(dir.path()).unwrap());
        let workspace = crate::workspace_host::WorkspaceHost::open(
            store,
            WorkspaceHostConfig {
                device_id: "device-1".into(),
                device_name: "Test".into(),
                platform: "test".into(),
                org_id: "org-1".into(),
                user_id: "user-1".into(),
                edge: None,
            },
        )
        .unwrap();
        workspace
            .create_space("space-1", "device-1", "/tmp", None, false)
            .unwrap();
        workspace
            .create_chat("chat-1", "space-1", None, None)
            .unwrap();
        (dir, workspace)
    }

    fn tool_part(call: ToolCall, is_error: bool) -> MessagePart {
        MessagePart::Tool {
            id: "tool".into(),
            call,
            is_error,
            resolved: true,
        }
    }

    #[test]
    fn turn_mutation_scope_contains_only_successful_explicit_paths() {
        let parts = vec![
            tool_part(
                ToolCall::EditFile {
                    path: "src/session.rs".into(),
                    old_string: None,
                    new_string: None,
                },
                false,
            ),
            tool_part(
                ToolCall::WriteFile {
                    path: "src/session.rs".into(),
                    content: None,
                },
                false,
            ),
            tool_part(
                ToolCall::EditFile {
                    path: "src/failed.rs".into(),
                    old_string: None,
                    new_string: None,
                },
                true,
            ),
        ];

        assert_eq!(
            successful_file_mutations(&parts),
            TurnMutationScope::Paths(vec!["src/session.rs".into()])
        );
    }

    #[test]
    fn multi_file_patch_preserves_every_reported_path() {
        assert_eq!(
            successful_file_mutations(&[tool_part(
                ToolCall::ApplyPatch {
                    path: None,
                    paths: vec!["src/one.rs".into(), "src/two.rs".into()],
                },
                false,
            )]),
            TurnMutationScope::Paths(vec!["src/one.rs".into(), "src/two.rs".into()])
        );
    }

    #[test]
    fn pathless_patch_does_not_claim_checkout_changes() {
        assert_eq!(
            successful_file_mutations(&[tool_part(
                ToolCall::ApplyPatch {
                    path: None,
                    paths: Vec::new(),
                },
                false,
            )]),
            TurnMutationScope::None
        );
    }

    fn active_goal(workspace: &crate::workspace_host::WorkspaceHost) -> jolt_proto::Goal {
        let goal = crate::goals::apply_operation(
            None,
            &jolt_doc::GoalOperation::Create {
                objective: "Finish the work".into(),
                token_budget: None,
            },
        )
        .unwrap()
        .unwrap();
        workspace.set_chat_goal("chat-1", Some(&goal)).unwrap();
        goal
    }

    #[tokio::test]
    async fn dropped_mcp_answer_request_resolves_the_input_ui() {
        let pending: PendingInputs = Arc::new(Mutex::new(HashMap::new()));
        let (engine_tx, mut engine_rx) = mpsc::unbounded_channel();
        let (request_id, answers) = begin_input_request(
            &pending,
            &engine_tx,
            vec![UserInputQuestion {
                id: "question-1".into(),
                header: "Question".into(),
                question: "Continue?".into(),
                options: vec!["Yes".into(), "No".into()],
                multi_select: false,
            }],
        );
        assert!(matches!(
            engine_rx.recv().await,
            Some(AgentEvent::InputRequested { .. })
        ));

        let guard = PendingInputGuard::new(pending.clone(), engine_tx, request_id.clone());
        drop(guard);
        assert!(lock(&pending).is_empty());
        assert!(answers.await.is_err());
        assert!(matches!(
            engine_rx.recv().await,
            Some(AgentEvent::InputResolved { request_id: resolved }) if resolved == request_id
        ));
    }

    #[test]
    fn compaction_follow_up_survives_only_until_user_or_agent_activity() {
        let follow_up = CompactionFollowUp::default();

        follow_up.observe_agent_event(&AgentEvent::CompactionStarted);
        assert!(!follow_up.take_on_shutdown());
        follow_up.observe_agent_event(&AgentEvent::CompactionFinished);
        assert!(follow_up.take_on_shutdown());
        assert!(!follow_up.take_on_shutdown());

        follow_up.observe_agent_event(&AgentEvent::CompactionFinished);
        follow_up.observe_agent_event(&AgentEvent::TextDelta {
            text: "Continuing normally".into(),
        });
        assert!(!follow_up.take_on_shutdown());

        follow_up.observe_agent_event(&AgentEvent::CompactionFinished);
        follow_up.cancel_for_user_message();
        assert!(!follow_up.take_on_shutdown());
    }

    #[tokio::test]
    async fn direct_completion_keeps_terminal_status_and_accounts_the_turn() {
        let (_dir, workspace) = goal_workspace();
        let goal = active_goal(&workspace);
        let completed = workspace
            .mutate_chat_goal("chat-1", |current| {
                crate::goals::apply_agent_action(
                    current,
                    &goal.id,
                    goal.revision,
                    crate::goals::AgentGoalAction::Complete {
                        summary: "Verified every requirement".into(),
                    },
                )
                .map(Some)
            })
            .unwrap()
            .unwrap();

        let finished = finish_goal_turn_in_workspace(
            &workspace,
            "chat-1",
            Some(&goal.id),
            DoneStatus::Completed,
            None,
            None,
            42,
            100,
        )
        .unwrap();
        assert_eq!(finished.status, jolt_proto::GoalStatus::Complete);
        assert_eq!(finished.status_message, completed.status_message);
        assert_eq!(finished.tokens_used, 42);
        assert_eq!(finished.elapsed_active_ms, 100);
        assert_eq!(finished.turns, 1);
    }

    #[tokio::test]
    async fn blocker_reports_require_three_distinct_goal_turns() {
        let (_dir, workspace) = goal_workspace();
        let goal = active_goal(&workspace);
        for expected_streak in 1..=3 {
            let current = workspace.chat_goal("chat-1").unwrap();
            let finished = finish_goal_turn_in_workspace(
                &workspace,
                "chat-1",
                Some(&goal.id),
                DoneStatus::Completed,
                None,
                Some(crate::mcp::McpGoalSignal::Blocked {
                    goal_id: current.id.clone(),
                    expected_revision: current.revision,
                    blocker_key: "waiting-for-review".into(),
                    summary: "Waiting for review".into(),
                }),
                1,
                1,
            )
            .unwrap();
            assert_eq!(finished.blocker_streak, expected_streak);
            assert_eq!(
                finished.status,
                if expected_streak == 3 {
                    jolt_proto::GoalStatus::Blocked
                } else {
                    jolt_proto::GoalStatus::Active
                }
            );
        }
    }
}
