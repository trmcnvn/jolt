//! Restart round-trip + harness resume continuity (the "chats forget everything
//! after an app restart" regression): the EMBED assembly (`EngineCore::assemble`)
//! is run twice over one data dir, asserting
//! - chats + transcripts survive a graceful shutdown → relaunch;
//! - the next run in an existing chat carries the chat's stored harness-native
//!   session id as `RunRequest.resume`;
//! - a kill -9 style crash recovers the session id from the run journal and
//!   stamps streaming entries
//!   `aborted`;
//! - resume is cwd-scoped (harness session stores are keyed by cwd);
//! - a harness-rejected resume retries once as a fresh session;
//! - a steer with no live run after a restart dispatches as a new turn that
//!   still resumes the prior conversation.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use futures::stream::BoxStream;

use jolt_engine::{EngineCore, HarnessRegistry, RunJournal};
use jolt_harness::{Harness, HarnessError, RunControls};
use jolt_proto::{
    AgentEvent, ChatConfig, DoneStatus, HarnessId, Model, ReasoningLevel, RunRequest, SandboxLevel,
    SteeringMode,
};
use jolt_session_doc::{
    MessagePart, MessageRole, MessageStatus, SessionCommandPayload, SessionMessageEntry,
};
use jolt_store::DocsStore;

const CHAT: &str = "chat-restart";

type RequestLog = Arc<Mutex<Vec<RunRequest>>>;

fn run_request(prompt: &str, cwd: &str) -> RunRequest {
    RunRequest {
        prompt: prompt.into(),
        harness: None,
        model: None,
        reasoning: None,
        model_options: Default::default(),
        cwd: cwd.into(),
        sandbox: SandboxLevel::WorkspaceWrite,
        auto_approve: true,
        attachments: Vec::new(),
        resume: None,
    }
}

/// Records every `RunRequest` it receives (the resume-injection probe). A
/// successful run emits `SessionStarted{session_id}` … `Done{session_id}`;
/// with `fail_on_resume`, any request carrying `resume` dies the way claude
/// does on an unknown `--resume` id — an errored Done before any session
/// starts.
struct RecordingHarness {
    requests: RequestLog,
    session_id: String,
    fail_on_resume: bool,
}

#[async_trait]
impl Harness for RecordingHarness {
    fn id(&self) -> HarnessId {
        HarnessId::Mock
    }
    fn display_name(&self) -> &str {
        "Recording"
    }
    fn supports_steering(&self) -> bool {
        false
    }
    fn steering_mode(&self) -> SteeringMode {
        SteeringMode::TurnBoundary
    }
    fn reasoning_levels(&self) -> &[ReasoningLevel] {
        &[ReasoningLevel::Medium]
    }
    async fn models(&self) -> Result<Vec<Model>, HarnessError> {
        Ok(vec![])
    }
    async fn run(
        &self,
        request: RunRequest,
        _controls: RunControls,
    ) -> Result<BoxStream<'static, Result<AgentEvent, HarnessError>>, HarnessError> {
        self.requests
            .lock()
            .expect("request log")
            .push(request.clone());
        let events: Vec<Result<AgentEvent, HarnessError>> =
            if self.fail_on_resume && request.resume.is_some() {
                vec![Ok(AgentEvent::Done {
                    status: DoneStatus::Errored,
                    result: None,
                    error: Some("No conversation found with session ID".into()),
                    session_id: None,
                })]
            } else {
                vec![
                    Ok(AgentEvent::SessionStarted {
                        harness: HarnessId::Mock,
                        model: "mock-1".into(),
                        tools: vec![],
                        cwd: request.cwd.clone(),
                        session_id: self.session_id.clone(),
                        assistant_message_id: "a-1".into(),
                    }),
                    Ok(AgentEvent::TextDelta {
                        text: format!("ack: {}", request.prompt),
                    }),
                    Ok(AgentEvent::Done {
                        status: DoneStatus::Completed,
                        result: None,
                        error: None,
                        session_id: Some(self.session_id.clone()),
                    }),
                ]
            };
        Ok(futures::stream::iter(events).boxed())
    }
}

struct SwitchingHarness {
    id: HarnessId,
    requests: RequestLog,
    session_id: String,
}

struct DelayedInterruptHarness {
    release: Arc<tokio::sync::Notify>,
}

#[async_trait]
impl Harness for DelayedInterruptHarness {
    fn id(&self) -> HarnessId {
        HarnessId::Mock
    }

    fn display_name(&self) -> &str {
        "Delayed interrupt"
    }

    fn supports_steering(&self) -> bool {
        false
    }

    fn steering_mode(&self) -> SteeringMode {
        SteeringMode::TurnBoundary
    }

    fn reasoning_levels(&self) -> &[ReasoningLevel] {
        &[ReasoningLevel::Medium]
    }

    async fn models(&self) -> Result<Vec<Model>, HarnessError> {
        Ok(vec![])
    }

    async fn run(
        &self,
        request: RunRequest,
        _controls: RunControls,
    ) -> Result<BoxStream<'static, Result<AgentEvent, HarnessError>>, HarnessError> {
        let (tx, rx) = tokio::sync::mpsc::channel(8);
        let release = self.release.clone();
        tokio::spawn(async move {
            let _ = tx
                .send(Ok(AgentEvent::SessionStarted {
                    harness: HarnessId::Mock,
                    model: "mock-1".into(),
                    tools: vec![],
                    cwd: request.cwd,
                    session_id: "delayed-source".into(),
                    assistant_message_id: "delayed-source-a1".into(),
                }))
                .await;
            let _ = tx
                .send(Ok(AgentEvent::TextDelta {
                    text: "source still settling".into(),
                }))
                .await;
            release.notified().await;
            let _ = tx
                .send(Ok(AgentEvent::Done {
                    status: DoneStatus::Interrupted,
                    result: None,
                    error: None,
                    session_id: Some("delayed-source".into()),
                }))
                .await;
        });
        Ok(futures::stream::unfold(rx, |mut rx| async move {
            rx.recv().await.map(|event| (event, rx))
        })
        .boxed())
    }
}

#[async_trait]
impl Harness for SwitchingHarness {
    fn id(&self) -> HarnessId {
        self.id
    }

    fn display_name(&self) -> &str {
        "Switching"
    }

    fn supports_steering(&self) -> bool {
        false
    }

    fn steering_mode(&self) -> SteeringMode {
        SteeringMode::TurnBoundary
    }

    fn reasoning_levels(&self) -> &[ReasoningLevel] {
        &[ReasoningLevel::Medium]
    }

    async fn models(&self) -> Result<Vec<Model>, HarnessError> {
        Ok(vec![])
    }

    async fn run(
        &self,
        request: RunRequest,
        _controls: RunControls,
    ) -> Result<BoxStream<'static, Result<AgentEvent, HarnessError>>, HarnessError> {
        self.requests
            .lock()
            .expect("request log")
            .push(request.clone());
        Ok(futures::stream::iter(vec![
            Ok(AgentEvent::SessionStarted {
                harness: self.id,
                model: format!("{:?}-model", self.id),
                tools: vec![],
                cwd: request.cwd,
                session_id: self.session_id.clone(),
                assistant_message_id: uuid::Uuid::new_v4().to_string(),
            }),
            Ok(AgentEvent::TextDelta {
                text: "switching harness response".into(),
            }),
            Ok(AgentEvent::Done {
                status: DoneStatus::Completed,
                result: None,
                error: None,
                session_id: Some(self.session_id.clone()),
            }),
        ])
        .boxed())
    }
}

fn assemble(dir: &std::path::Path, harness: RecordingHarness) -> EngineCore {
    let registry = HarnessRegistry::new();
    registry.register(Arc::new(harness));
    EngineCore::assemble(dir, Arc::new(registry), HarnessId::Mock, None)
        .expect("engine core assembles")
}

fn queue_run(core: &EngineCore, prompt: &str, cwd: &str, message_id: &str) {
    core.doc_host
        .queue_command(
            CHAT,
            SessionCommandPayload::Run {
                request: run_request(prompt, cwd),
                message_id: message_id.into(),
            },
        )
        .expect("queue run command");
}

async fn wait_for<F>(predicate: F, what: &str)
where
    F: FnMut() -> bool,
{
    wait_for_within(predicate, what, Duration::from_secs(10)).await;
}

async fn wait_for_within<F>(mut predicate: F, what: &str, deadline: Duration)
where
    F: FnMut() -> bool,
{
    let deadline = tokio::time::Instant::now() + deadline;
    while !predicate() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for {what}"
        );
        tokio::time::sleep(Duration::from_millis(15)).await;
    }
}

/// Tolerant read for hot-polling predicates (mirrors e2e.rs `entries_now`).
fn entries_now(core: &EngineCore) -> Vec<SessionMessageEntry> {
    core.doc_host
        .open(CHAT)
        .ok()
        .and_then(|h| h.doc().read_entries().ok())
        .unwrap_or_default()
}

fn complete_assistant_count(core: &EngineCore) -> usize {
    entries_now(core)
        .iter()
        .filter(|e| e.role == MessageRole::Assistant && e.status == Some(MessageStatus::Complete))
        .count()
}

fn stored_harness_session(core: &EngineCore) -> Option<(String, Option<String>)> {
    let chat = core
        .workspace
        .chat(CHAT)
        .expect("read chat row")
        .expect("chat row exists");
    chat.harness_session_id
        .map(|id| (id, chat.harness_session_cwd))
}

/// Create + name the chat row up front so the auto-titler (which runs its own
/// harness request after a completed exchange on an UNTITLED chat) stays out
/// of the recorded request log.
fn pre_title(core: &EngineCore) {
    core.workspace
        .create_space("space-restart", &core.device_id, "/tmp", None, false)
        .expect("create space row");
    core.workspace
        .create_chat(CHAT, "space-restart", None, None)
        .expect("create chat row");
    core.workspace
        .rename_chat(CHAT, "Pre-titled")
        .expect("rename chat");
}

/// One full turn in a fresh engine over `dir`, then graceful shutdown — the
/// "before restart" phase shared by the tests below.
async fn run_one_turn_and_shutdown(dir: &std::path::Path, requests: &RequestLog, session: &str) {
    let core = assemble(
        dir,
        RecordingHarness {
            requests: requests.clone(),
            session_id: session.into(),
            fail_on_resume: false,
        },
    );
    pre_title(&core);
    queue_run(
        &core,
        "remember the codeword PINEAPPLE",
        "/tmp",
        "msg-user-1",
    );
    wait_for(
        || complete_assistant_count(&core) == 1,
        "first turn to complete",
    )
    .await;
    core.shutdown().await;
    drop(core);
}

#[tokio::test]
async fn restart_roundtrip_restores_chats_transcript_and_resume() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("data");
    let requests: RequestLog = Arc::new(Mutex::new(Vec::new()));

    run_one_turn_and_shutdown(&dir, &requests, "hs-restart-1").await;
    assert_eq!(
        requests.lock().unwrap()[0].resume,
        None,
        "a chat's first run must start a fresh harness session"
    );

    // Relaunch over the same data dir (the embedded-engine restart path).
    let core = assemble(
        &dir,
        RecordingHarness {
            requests: requests.clone(),
            session_id: "hs-restart-2".into(),
            fail_on_resume: false,
        },
    );

    // Sidebar state survived: the chat row is back with its cwd, preview, and
    // the stored harness session (cwd-scoped).
    let chats = core.workspace.read_chats().expect("read chats");
    assert_eq!(chats.len(), 1, "chat row survives restart: {chats:#?}");
    assert_eq!(chats[0].id, CHAT);
    assert_eq!(chats[0].cwd.as_deref(), Some("/tmp"));
    assert!(chats[0].last_message_preview.is_some());
    assert_eq!(
        stored_harness_session(&core),
        Some(("hs-restart-1".into(), Some("/tmp".into())))
    );

    // Transcript survived: user + completed assistant entry, texts intact.
    let entries = entries_now(&core);
    assert_eq!(
        entries.len(),
        2,
        "transcript survives restart: {entries:#?}"
    );
    assert_eq!(entries[0].id, "msg-user-1");
    assert_eq!(entries[0].role, MessageRole::User);
    assert_eq!(entries[1].role, MessageRole::Assistant);
    assert_eq!(entries[1].status, Some(MessageStatus::Complete));
    assert!(matches!(
        &entries[1].parts[0],
        MessagePart::Text { text, .. } if text.contains("PINEAPPLE")
    ));

    // The next run resumes the SAME harness conversation: the engine injects
    // the stored session id even though the caller sent `resume: None`.
    queue_run(&core, "what was the codeword?", "/tmp", "msg-user-2");
    wait_for(
        || complete_assistant_count(&core) == 2,
        "second turn to complete",
    )
    .await;
    {
        let log = requests.lock().unwrap();
        assert_eq!(log.len(), 2);
        assert_eq!(
            log[1].resume.as_deref(),
            Some("hs-restart-1"),
            "post-restart dispatch must resume the stored harness session"
        );
    }
    // The fresh turn's session id replaces the stored one.
    assert_eq!(
        stored_harness_session(&core),
        Some(("hs-restart-2".into(), Some("/tmp".into())))
    );
    core.shutdown().await;
}

#[tokio::test]
async fn kill_crash_recovers_resume_from_journal_and_stamps_aborted() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("data");
    let scope = dir.join("scopes/accounts/dev-org/dev-user");
    std::fs::create_dir_all(&scope).unwrap();
    // Pin the device id so the manufactured streaming entry counts as OURS.
    std::fs::write(scope.join("device-id"), "dev-crash").unwrap();

    // Manufacture the on-disk state a kill -9 mid-run leaves behind:
    // - a normalized assistant entry that is still streaming;
    // - a journal whose last event is NOT `Done` (run died mid-stream), holding
    //   the only copy of the harness session id (the debounced workspace-row
    //   write never landed).
    {
        let store =
            Arc::new(DocsStore::open(dir.join("scopes/accounts/dev-org/dev-user")).unwrap());
        let session = store.open_session(CHAT).unwrap();
        session
            .push_message(&SessionMessageEntry {
                id: "msg-user-1".into(),
                role: MessageRole::User,
                parts: vec![MessagePart::Text {
                    id: "t0".into(),
                    text: "long task".into(),
                }],
                created_at: 1,
                device_id: "dev-crash".into(),
                status: Some(MessageStatus::Complete),
                continuation_of: None,
            })
            .unwrap();
        session
            .push_message(&SessionMessageEntry {
                id: "msg-assistant-1".into(),
                role: MessageRole::Assistant,
                parts: vec![MessagePart::Text {
                    id: "t0".into(),
                    text: "partial…".into(),
                }],
                created_at: 2,
                device_id: "dev-crash".into(),
                status: Some(MessageStatus::Streaming),
                continuation_of: None,
            })
            .unwrap();

        let journal =
            RunJournal::open(dir.join("scopes/accounts/dev-org/dev-user/journals")).unwrap();
        journal
            .append(
                CHAT,
                &AgentEvent::SessionStarted {
                    harness: HarnessId::Mock,
                    model: "mock-1".into(),
                    tools: vec![],
                    cwd: "/tmp".into(),
                    session_id: "hs-crash".into(),
                    assistant_message_id: "msg-assistant-1".into(),
                },
            )
            .unwrap();
        journal
            .append(
                CHAT,
                &AgentEvent::TextDelta {
                    text: "partial…".into(),
                },
            )
            .unwrap();
    }

    let requests: RequestLog = Arc::new(Mutex::new(Vec::new()));
    let core = assemble(
        &dir,
        RecordingHarness {
            requests: requests.clone(),
            session_id: "hs-after-crash".into(),
            fail_on_resume: false,
        },
    );
    assert_eq!(core.device_id, "dev-crash");

    // Boot recovery stamped the abandoned streaming entry `aborted` …
    let entries = entries_now(&core);
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[1].status, Some(MessageStatus::Aborted));
    // … and closed the stale journal with a synthetic Done.
    let journal = RunJournal::open(dir.join("scopes/accounts/dev-org/dev-user/journals")).unwrap();
    assert!(matches!(
        journal.last_event(CHAT).unwrap(),
        Some((_, AgentEvent::Done { .. }))
    ));

    // The next run resumes the crashed conversation: the session id was
    // recovered from the journal (its only surviving home).
    pre_title(&core);
    queue_run(&core, "keep going", "/tmp", "msg-user-2");
    wait_for(
        || complete_assistant_count(&core) == 1,
        "post-crash turn to complete",
    )
    .await;
    assert_eq!(
        requests.lock().unwrap()[0].resume.as_deref(),
        Some("hs-crash"),
        "journal-recovered session id must ride the next dispatch"
    );
    core.shutdown().await;
}

/// A harness whose stream stays OPEN after each turn's Done, serving follow-up
/// turns from the steering mailbox — the persistent-session shape (codex; and
/// claude's stream-json stdin). Counts `run()` calls to prove the engine
/// reuses one child across turns instead of respawning.
struct PersistentHarness {
    runs_started: Arc<Mutex<usize>>,
    steers_received: Option<Arc<Mutex<Vec<String>>>>,
}

#[async_trait]
impl Harness for PersistentHarness {
    fn id(&self) -> HarnessId {
        HarnessId::Mock
    }
    fn display_name(&self) -> &str {
        "Persistent"
    }
    fn supports_steering(&self) -> bool {
        true
    }
    fn steering_mode(&self) -> SteeringMode {
        SteeringMode::StepBoundary
    }
    fn reasoning_levels(&self) -> &[ReasoningLevel] {
        &[ReasoningLevel::Medium]
    }
    async fn models(&self) -> Result<Vec<Model>, HarnessError> {
        Ok(vec![])
    }
    async fn run(
        &self,
        _request: RunRequest,
        controls: RunControls,
    ) -> Result<BoxStream<'static, Result<AgentEvent, HarnessError>>, HarnessError> {
        *self.runs_started.lock().unwrap() += 1;
        let (tx, rx) = tokio::sync::mpsc::channel::<Result<AgentEvent, HarnessError>>(32);
        let mut steering = controls.steering;
        let interrupt = controls.interrupt;
        let steers_received = self.steers_received.clone();
        tokio::spawn(async move {
            let turn = |n: usize, prompt: &str| {
                vec![
                    AgentEvent::TextDelta {
                        text: format!("turn {n} ack: {prompt}"),
                    },
                    AgentEvent::Done {
                        status: DoneStatus::Completed,
                        result: None,
                        error: None,
                        session_id: Some("hs-persist".into()),
                    },
                ]
            };
            let first = vec![AgentEvent::SessionStarted {
                harness: HarnessId::Mock,
                model: "mock-1".into(),
                tools: vec![],
                cwd: "/tmp".into(),
                session_id: "hs-persist".into(),
                assistant_message_id: "a-1".into(),
            }];
            for ev in first.into_iter().chain(turn(1, "first")) {
                if tx.send(Ok(ev)).await.is_err() {
                    return;
                }
            }
            // Parked: serve follow-up turns from the mailbox until the
            // engine hangs up (idle reap / interrupt / shutdown).
            let mut n = 1usize;
            loop {
                let steer = tokio::select! {
                    _ = interrupt.cancelled() => return,
                    steer = steering.recv() => match steer {
                        Some(steer) => steer,
                        None => return,
                    },
                };
                if let Some(log) = &steers_received {
                    log.lock().unwrap().push(steer.prompt.clone());
                }
                n += 1;
                let boundary = AgentEvent::Steered {
                    assistant_message_id: None,
                    next_assistant_message_id: steer.message_id.map(|id| format!("a-{id}")),
                };
                if tx.send(Ok(boundary)).await.is_err() {
                    return;
                }
                for ev in turn(n, &steer.prompt) {
                    if tx.send(Ok(ev)).await.is_err() {
                        return;
                    }
                }
            }
        });
        Ok(futures::stream::unfold(
            rx,
            |mut rx| async move { rx.recv().await.map(|ev| (ev, rx)) },
        )
        .boxed())
    }
}

#[tokio::test]
async fn persistent_session_serves_multiple_turns_on_one_child() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("data");
    std::fs::create_dir_all(&dir).unwrap();

    let runs_started = Arc::new(Mutex::new(0usize));
    let registry = HarnessRegistry::new();
    registry.register(Arc::new(PersistentHarness {
        runs_started: runs_started.clone(),
        steers_received: None,
    }));
    let core = EngineCore::assemble(&dir, Arc::new(registry), HarnessId::Mock, None)
        .expect("engine core assembles");
    pre_title(&core);

    queue_run(&core, "first", "/tmp", "msg-user-1");
    wait_for(
        || complete_assistant_count(&core) == 1,
        "first turn to complete",
    )
    .await;

    // The session PARKS (jolt runsBySession): the second message routes into
    // the live child instead of spawning a new one.
    queue_run(&core, "second", "/tmp", "msg-user-2");
    wait_for(
        || complete_assistant_count(&core) == 2,
        "second turn to complete on the same child",
    )
    .await;

    assert_eq!(
        *runs_started.lock().unwrap(),
        1,
        "one harness child must serve both turns"
    );
    let entries = entries_now(&core);
    assert_eq!(
        entries
            .iter()
            .filter(|e| e.role == MessageRole::User)
            .count(),
        2
    );
    core.shutdown().await;
}

#[tokio::test]
async fn fresh_crash_auto_resumes_and_notes_the_interruption() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("data");
    let scope = dir.join("scopes/accounts/dev-org/dev-user");
    std::fs::create_dir_all(&scope).unwrap();
    std::fs::write(scope.join("device-id"), "dev-crash").unwrap();

    // Same manufactured kill -9 state as above, but FRESH: the streaming entry
    // crashed moments ago, inside the 12h revival window.
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    {
        let store =
            Arc::new(DocsStore::open(dir.join("scopes/accounts/dev-org/dev-user")).unwrap());
        let session = store.open_session(CHAT).unwrap();
        session
            .push_message(&SessionMessageEntry {
                id: "msg-user-1".into(),
                role: MessageRole::User,
                parts: vec![MessagePart::Text {
                    id: "t0".into(),
                    text: "long task".into(),
                }],
                created_at: now - 60_000,
                device_id: "dev-crash".into(),
                status: Some(MessageStatus::Complete),
                continuation_of: None,
            })
            .unwrap();
        session
            .push_message(&SessionMessageEntry {
                id: "msg-assistant-1".into(),
                role: MessageRole::Assistant,
                parts: vec![MessagePart::Text {
                    id: "t0".into(),
                    text: "partial…".into(),
                }],
                created_at: now - 30_000,
                device_id: "dev-crash".into(),
                status: Some(MessageStatus::Streaming),
                continuation_of: None,
            })
            .unwrap();

        let journal =
            RunJournal::open(dir.join("scopes/accounts/dev-org/dev-user/journals")).unwrap();
        journal
            .append(
                CHAT,
                &AgentEvent::SessionStarted {
                    harness: HarnessId::Mock,
                    model: "mock-1".into(),
                    tools: vec![],
                    cwd: "/tmp".into(),
                    session_id: "hs-crash".into(),
                    assistant_message_id: "msg-assistant-1".into(),
                },
            )
            .unwrap();
        journal
            .append(
                CHAT,
                &AgentEvent::TextDelta {
                    text: "partial…".into(),
                },
            )
            .unwrap();
    }

    let requests: RequestLog = Arc::new(Mutex::new(Vec::new()));
    let core = assemble(
        &dir,
        RecordingHarness {
            requests: requests.clone(),
            session_id: "hs-after-crash".into(),
            fail_on_resume: false,
        },
    );

    // The run is PICKED BACK UP without any user action (jolt: "not just
    // eulogized"): recovery re-dispatches the crashed prompt itself.
    wait_for(
        || complete_assistant_count(&core) == 1,
        "auto-resumed turn to complete",
    )
    .await;

    let entries = entries_now(&core);
    // The aborted entry SAYS why it ended — and that the run is resuming.
    let aborted = entries
        .iter()
        .find(|e| e.status == Some(MessageStatus::Aborted))
        .expect("crashed entry stays, stamped aborted");
    assert!(
        aborted.parts.iter().any(|p| matches!(
            p,
            MessagePart::Error { message, .. }
                if message.contains("engine restart") && message.contains("resuming")
        )),
        "aborted entry carries the visible interruption note"
    );
    // Re-dispatch reuses the original user message id — never a duplicate.
    assert_eq!(
        entries
            .iter()
            .filter(|e| e.role == MessageRole::User)
            .count(),
        1
    );
    // The revived run continues the journal-recovered harness conversation.
    // (An auto-title request may precede it — titling fires at dispatch.)
    let recorded = requests.lock().unwrap().clone();
    let revived = recorded
        .iter()
        .find(|r| r.prompt == "long task")
        .expect("auto-resumed dispatch reached the harness");
    assert_eq!(
        revived.resume.as_deref(),
        Some("hs-crash"),
        "auto-resume must reattach the crashed harness session"
    );
    core.shutdown().await;
}

#[tokio::test]
async fn resume_is_cwd_scoped() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("data");
    let requests: RequestLog = Arc::new(Mutex::new(Vec::new()));

    run_one_turn_and_shutdown(&dir, &requests, "hs-cwd-1").await;

    let core = assemble(
        &dir,
        RecordingHarness {
            requests: requests.clone(),
            session_id: "hs-cwd-2".into(),
            fail_on_resume: false,
        },
    );
    // Same chat, different launch directory: claude session stores are keyed
    // by cwd, so the stored id must NOT be injected.
    queue_run(
        &core,
        "now from another project",
        "/elsewhere",
        "msg-user-2",
    );
    wait_for(
        || complete_assistant_count(&core) == 2,
        "cross-cwd turn to complete",
    )
    .await;
    assert_eq!(
        requests.lock().unwrap()[1].resume,
        None,
        "a session created under /tmp must not resume from /elsewhere"
    );
    core.shutdown().await;
}

#[tokio::test]
async fn steer_command_switches_instead_of_entering_the_previous_harness() {
    let tmp = tempfile::tempdir().unwrap();
    let source_runs = Arc::new(Mutex::new(0usize));
    let source_steers = Arc::new(Mutex::new(Vec::new()));
    let pi_requests: RequestLog = Arc::new(Mutex::new(Vec::new()));
    let registry = HarnessRegistry::new();
    registry.register(Arc::new(PersistentHarness {
        runs_started: source_runs.clone(),
        steers_received: Some(source_steers.clone()),
    }));
    registry.register(Arc::new(SwitchingHarness {
        id: HarnessId::Pi,
        requests: pi_requests.clone(),
        session_id: "pi-session".into(),
    }));
    let core = EngineCore::assemble(
        &tmp.path().join("data"),
        Arc::new(registry),
        HarnessId::Mock,
        None,
    )
    .expect("engine core assembles");
    pre_title(&core);

    queue_run(&core, "first mock turn", "/tmp", "switch-steer-u1");
    wait_for(
        || complete_assistant_count(&core) == 1,
        "source harness turn",
    )
    .await;
    assert_eq!(*source_runs.lock().unwrap(), 1);

    core.workspace
        .set_chat_config(
            CHAT,
            &ChatConfig {
                harness: HarnessId::Pi,
                model: None,
                reasoning: None,
                model_options: Default::default(),
                sandbox: SandboxLevel::WorkspaceWrite,
            },
        )
        .expect("select target harness");
    core.doc_host
        .queue_command(
            CHAT,
            SessionCommandPayload::Steer {
                prompt: "pi takes this turn".into(),
                message_id: Some("switch-steer-u2".into()),
            },
        )
        .expect("queue steer command");
    wait_for(
        || complete_assistant_count(&core) == 2,
        "cross-harness steer fallback",
    )
    .await;

    assert!(
        source_steers.lock().unwrap().is_empty(),
        "the target harness's prompt must never enter the source steering mailbox"
    );
    {
        let pi = pi_requests.lock().unwrap();
        assert_eq!(pi.len(), 1);
        assert!(pi[0].prompt.contains("(full)"));
        assert!(pi[0].prompt.contains("first mock turn"));
        assert!(pi[0].prompt.ends_with("pi takes this turn"));
    }

    core.shutdown().await;
}

#[tokio::test]
async fn harness_switch_marker_precedes_source_settlement() {
    let tmp = tempfile::tempdir().unwrap();
    let release = Arc::new(tokio::sync::Notify::new());
    let pi_requests: RequestLog = Arc::new(Mutex::new(Vec::new()));
    let registry = HarnessRegistry::new();
    registry.register(Arc::new(DelayedInterruptHarness {
        release: release.clone(),
    }));
    registry.register(Arc::new(SwitchingHarness {
        id: HarnessId::Pi,
        requests: pi_requests.clone(),
        session_id: "pi-session".into(),
    }));
    let core = EngineCore::assemble(
        &tmp.path().join("data"),
        Arc::new(registry),
        HarnessId::Mock,
        None,
    )
    .expect("engine core assembles");
    pre_title(&core);

    let queue = |prompt: &str, harness: HarnessId, message_id: &str| {
        let mut request = run_request(prompt, "/tmp");
        request.harness = Some(harness);
        core.doc_host
            .queue_command(
                CHAT,
                SessionCommandPayload::Run {
                    request,
                    message_id: message_id.into(),
                },
            )
            .unwrap();
    };

    queue("source turn", HarnessId::Mock, "instant-switch-u1");
    wait_for(
        || {
            entries_now(&core).iter().any(|entry| {
                entry.role == MessageRole::Assistant
                    && entry.status == Some(MessageStatus::Streaming)
            })
        },
        "source harness to stream",
    )
    .await;

    queue("pi takes over", HarnessId::Pi, "instant-switch-u2");
    wait_for_within(
        || {
            entries_now(&core).iter().any(|entry| {
                matches!(
                    entry.parts.as_slice(),
                    [MessagePart::HarnessSwitch {
                        from: HarnessId::Mock,
                        to: HarnessId::Pi,
                        ..
                    }]
                )
            })
        },
        "harness switch marker before source settlement",
        Duration::from_millis(500),
    )
    .await;
    assert!(
        pi_requests.lock().unwrap().is_empty(),
        "the target run must still be waiting for source settlement"
    );

    release.notify_one();
    wait_for(
        || !pi_requests.lock().unwrap().is_empty(),
        "target harness to start",
    )
    .await;
    core.shutdown().await;
}

#[tokio::test]
async fn switching_harnesses_preserves_native_sessions_and_uses_delta_handoffs() {
    let tmp = tempfile::tempdir().unwrap();
    let mock_requests: RequestLog = Arc::new(Mutex::new(Vec::new()));
    let pi_requests: RequestLog = Arc::new(Mutex::new(Vec::new()));
    let registry = HarnessRegistry::new();
    registry.register(Arc::new(SwitchingHarness {
        id: HarnessId::Mock,
        requests: mock_requests.clone(),
        session_id: "mock-session".into(),
    }));
    registry.register(Arc::new(SwitchingHarness {
        id: HarnessId::Pi,
        requests: pi_requests.clone(),
        session_id: "pi-session".into(),
    }));
    let core = EngineCore::assemble(
        &tmp.path().join("data"),
        Arc::new(registry),
        HarnessId::Mock,
        None,
    )
    .expect("engine core assembles");
    pre_title(&core);

    let queue = |prompt: &str, harness: HarnessId, message_id: &str| {
        let mut request = run_request(prompt, "/tmp");
        request.harness = Some(harness);
        core.doc_host
            .queue_command(
                CHAT,
                SessionCommandPayload::Run {
                    request,
                    message_id: message_id.into(),
                },
            )
            .unwrap();
    };

    queue("first mock turn", HarnessId::Mock, "switch-u1");
    wait_for(
        || complete_assistant_count(&core) == 1,
        "first harness turn",
    )
    .await;
    queue("pi takes over", HarnessId::Pi, "switch-u2");
    wait_for(
        || complete_assistant_count(&core) == 2,
        "second harness turn",
    )
    .await;
    queue("mock returns", HarnessId::Mock, "switch-u3");
    wait_for(
        || complete_assistant_count(&core) == 3,
        "return harness turn",
    )
    .await;

    {
        let pi = pi_requests.lock().unwrap();
        assert_eq!(pi.len(), 1);
        assert_eq!(pi[0].resume, None);
        assert!(pi[0].prompt.contains("(full)"));
        assert!(pi[0].prompt.contains("first mock turn"));
        assert!(pi[0].prompt.ends_with("pi takes over"));

        let mock = mock_requests.lock().unwrap();
        assert_eq!(mock.len(), 2);
        assert_eq!(mock[1].resume.as_deref(), Some("mock-session"));
        assert!(mock[1].prompt.contains("(delta)"));
        assert!(mock[1].prompt.contains("pi takes over"));
        assert!(!mock[1].prompt.contains("first mock turn"));
        assert!(mock[1].prompt.ends_with("mock returns"));
    }

    let entries = entries_now(&core);
    assert!(matches!(
        entries[2].parts.as_slice(),
        [MessagePart::HarnessSwitch {
            from: HarnessId::Mock,
            to: HarnessId::Pi,
            ..
        }]
    ));
    assert_eq!(entries[3].id, "switch-u2");
    assert!(matches!(
        entries[5].parts.as_slice(),
        [MessagePart::HarnessSwitch {
            from: HarnessId::Pi,
            to: HarnessId::Mock,
            ..
        }]
    ));
    assert_eq!(entries[6].id, "switch-u3");

    let chat = core.workspace.chat(CHAT).unwrap().unwrap();
    assert_eq!(chat.harness_conversations.len(), 2);
    core.shutdown().await;
}

#[tokio::test]
async fn rejected_resume_retries_as_fresh_session() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("data");
    let requests: RequestLog = Arc::new(Mutex::new(Vec::new()));

    run_one_turn_and_shutdown(&dir, &requests, "hs-dead").await;

    // Relaunch with a harness that rejects every resume (the stored session id
    // no longer exists on disk — claude's "No conversation found" exit).
    let core = assemble(
        &dir,
        RecordingHarness {
            requests: requests.clone(),
            session_id: "hs-fresh".into(),
            fail_on_resume: true,
        },
    );
    queue_run(&core, "second turn", "/tmp", "msg-user-2");
    wait_for(
        || complete_assistant_count(&core) == 2,
        "retried turn to complete",
    )
    .await;

    // Attempt with the dead id, then exactly one fresh retry.
    {
        let log = requests.lock().unwrap();
        assert_eq!(log.len(), 3, "one failed resume attempt + one fresh retry");
        assert_eq!(log[1].resume.as_deref(), Some("hs-dead"));
        assert_eq!(log[2].resume, None);
        assert!(
            log[2].prompt.contains("<jolt_harness_handoff")
                && log[2].prompt.ends_with("second turn"),
            "fresh fallback receives a full handoff before the original prompt"
        );
    }
    // The retry reused the same user entry — no duplicates, no error turn.
    let entries = entries_now(&core);
    let users: Vec<_> = entries
        .iter()
        .filter(|e| e.role == MessageRole::User)
        .collect();
    assert_eq!(users.len(), 2, "retry must not duplicate the user entry");
    assert_eq!(entries.len(), 4, "user+assistant per turn: {entries:#?}");
    // The fresh session id replaced the tombstoned one.
    assert_eq!(
        stored_harness_session(&core),
        Some(("hs-fresh".into(), Some("/tmp".into())))
    );
    core.shutdown().await;
}

/// Real-CLI proof of the whole regression fix: tell claude a codeword, restart
/// the engine (fresh `EngineCore::assemble` over the same data dir), ask for
/// the codeword back — the reply can only contain it if the second run resumed
/// the first run's harness session. Ignored by default: needs an installed,
/// authenticated `claude` CLI and spends real tokens (haiku, two tiny turns).
/// Run with: `cargo test -p jolt-engine --test restart_resume -- --ignored`
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires installed+authenticated claude CLI; spends tokens"]
async fn real_claude_remembers_codeword_across_engine_restart() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("data");
    let cwd = tmp.path().join("project");
    std::fs::create_dir_all(&cwd).unwrap();
    let cwd = cwd.to_string_lossy().to_string();

    let real_request = |prompt: &str| RunRequest {
        prompt: prompt.into(),
        harness: Some(HarnessId::ClaudeCode),
        model: Some("haiku".into()),
        reasoning: None,
        model_options: Default::default(),
        cwd: cwd.clone(),
        sandbox: SandboxLevel::WorkspaceWrite,
        auto_approve: false,
        attachments: Vec::new(),
        resume: None,
    };
    let assemble_real = || {
        EngineCore::assemble(
            &dir,
            Arc::new(jolt_engine::default_registry()),
            HarnessId::ClaudeCode,
            None,
        )
        .expect("engine core assembles")
    };

    let core = assemble_real();
    pre_title(&core); // keep the auto-titler from spending a second model call
    core.doc_host
        .queue_command(
            CHAT,
            SessionCommandPayload::Run {
                request: real_request(
                    "Remember the codeword: PINEAPPLE. Reply with exactly: stored",
                ),
                message_id: "msg-user-1".into(),
            },
        )
        .expect("queue first real run");
    wait_for_within(
        || complete_assistant_count(&core) == 1,
        "first real claude turn",
        Duration::from_secs(120),
    )
    .await;
    assert!(
        stored_harness_session(&core).is_some(),
        "claude session id must be stored on the chat row"
    );
    core.shutdown().await;
    drop(core);

    // "App restart": a brand-new engine over the same data dir.
    let core = assemble_real();
    core.doc_host
        .queue_command(
            CHAT,
            SessionCommandPayload::Run {
                request: real_request(
                    "What was the codeword I told you earlier? Reply with just the codeword.",
                ),
                message_id: "msg-user-2".into(),
            },
        )
        .expect("queue second real run");
    wait_for_within(
        || complete_assistant_count(&core) == 2,
        "post-restart real claude turn",
        Duration::from_secs(120),
    )
    .await;

    let entries = entries_now(&core);
    let last_assistant_text: String = entries
        .iter()
        .rev()
        .find(|e| e.role == MessageRole::Assistant)
        .into_iter()
        .flat_map(|e| {
            e.parts.iter().filter_map(|p| match p {
                MessagePart::Text { text, .. } => Some(text.clone()),
                _ => None,
            })
        })
        .collect();
    assert!(
        last_assistant_text.to_uppercase().contains("PINEAPPLE"),
        "post-restart reply must recall the codeword (got: {last_assistant_text:?})"
    );
    core.shutdown().await;
}

#[tokio::test]
async fn steer_after_restart_dispatches_new_turn_with_resume() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("data");
    let requests: RequestLog = Arc::new(Mutex::new(Vec::new()));

    run_one_turn_and_shutdown(&dir, &requests, "hs-steer").await;

    // Relaunch: no live run, no in-process `last_request`. A steer must fall
    // back to a new turn built from the chat's workspace row, resuming the
    // prior harness conversation.
    let core = assemble(
        &dir,
        RecordingHarness {
            requests: requests.clone(),
            session_id: "hs-steer-2".into(),
            fail_on_resume: false,
        },
    );
    core.doc_host
        .queue_command(
            CHAT,
            SessionCommandPayload::Steer {
                prompt: "actually, also add tests".into(),
                message_id: Some("msg-user-2".into()),
            },
        )
        .expect("queue steer command");
    wait_for(
        || complete_assistant_count(&core) == 2,
        "steer-as-new-turn to complete",
    )
    .await;

    {
        let log = requests.lock().unwrap();
        assert_eq!(log.len(), 2);
        assert_eq!(log[1].prompt, "actually, also add tests");
        assert_eq!(log[1].cwd, "/tmp", "run config rebuilt from the chat row");
        assert_eq!(
            log[1].resume.as_deref(),
            Some("hs-steer"),
            "steer-turned-run must resume the stored harness session"
        );
    }
    core.shutdown().await;
}
