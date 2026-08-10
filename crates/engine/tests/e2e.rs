//! M2 end-to-end tests: doc-queued commands → host executor → harness stream →
//! journal + broadcast + folded doc entries, plus interrupt/recovery/idempotence
//! and the RPC surface over the in-memory transport.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use futures::stream::BoxStream;

use jolt_engine::{EngineCore, HarnessRegistry, RunJournal, SteerOutcome};
use jolt_harness::mock::MockHarness;
use jolt_harness::{BashRequest, BashResult, Harness, HarnessError, McpServerConfig, RunControls};
use jolt_proto::{
    AgentEvent, DoneStatus, GoalStatus, HarnessId, Model, ReasoningLevel, RunRequest, SandboxLevel,
    SessionStatus, SteeringMode, ToolCall,
};
use jolt_session_doc::{
    GoalOperation, MessagePart, MessageRole, MessageStatus, SegmentWriter, SessionCommandEntry,
    SessionCommandPayload, SessionCommandStatus, SessionDoc, SessionMessageEntry,
};
use jolt_store::DocsStore;

const CHAT: &str = "chat-e2e";
const VIEWER: &str = "viewer-device";

fn run_request(prompt: &str) -> RunRequest {
    RunRequest {
        prompt: prompt.into(),
        harness: None,
        model: None,
        reasoning: None,
        model_options: Default::default(),
        cwd: "/tmp".into(),
        sandbox: SandboxLevel::WorkspaceWrite,
        auto_approve: true,
        attachments: Vec::new(),
        resume: None,
    }
}

fn done(status: DoneStatus) -> AgentEvent {
    AgentEvent::Done {
        status,
        result: None,
        error: None,
        session_id: Some("hs-1".into()),
    }
}

fn mock_script() -> Vec<AgentEvent> {
    vec![
        AgentEvent::SessionStarted {
            harness: HarnessId::Mock,
            model: "mock-1".into(),
            tools: vec![],
            cwd: "/tmp".into(),
            session_id: "hs-1".into(),
            assistant_message_id: "a-1".into(),
        },
        AgentEvent::TextDelta { text: "Hel".into() },
        AgentEvent::TextDelta { text: "lo".into() },
        AgentEvent::ToolCall {
            id: "tool-1".into(),
            call: ToolCall::WriteFile {
                path: "/tmp/x".into(),
                content: Some("SECRET".into()),
            },
        },
        AgentEvent::ToolResult {
            id: "tool-1".into(),
            is_error: false,
        },
        done(DoneStatus::Completed),
    ]
}

struct McpCompletingHarness;

async fn call_mcp_tool(
    config: &McpServerConfig,
    id: u64,
    name: &str,
    arguments: serde_json::Value,
) -> Result<serde_json::Value, HarnessError> {
    let response = reqwest::Client::new()
        .post(&config.url)
        .bearer_auth(&config.bearer_token)
        .header("Accept", "application/json, text/event-stream")
        .header("Content-Type", "application/json")
        .header("MCP-Protocol-Version", "2025-03-26")
        .json(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "tools/call",
            "params": { "name": name, "arguments": arguments }
        }))
        .send()
        .await
        .map_err(|error| HarnessError::Protocol(format!("MCP request failed: {error}")))?;
    if !response.status().is_success() {
        return Err(HarnessError::Protocol(format!(
            "MCP request returned {}",
            response.status()
        )));
    }
    response
        .json()
        .await
        .map_err(|error| HarnessError::Protocol(format!("invalid MCP response: {error}")))
}

#[async_trait]
impl Harness for McpCompletingHarness {
    fn id(&self) -> HarnessId {
        HarnessId::Mock
    }

    fn display_name(&self) -> &str {
        "MCP completing"
    }

    fn supports_steering(&self) -> bool {
        false
    }

    fn supports_mcp(&self) -> bool {
        true
    }

    fn steering_mode(&self) -> SteeringMode {
        SteeringMode::TurnBoundary
    }

    fn reasoning_levels(&self) -> &[ReasoningLevel] {
        &[]
    }

    async fn models(&self) -> Result<Vec<Model>, HarnessError> {
        Ok(Vec::new())
    }

    async fn run(
        &self,
        request: RunRequest,
        controls: RunControls,
    ) -> Result<BoxStream<'static, Result<AgentEvent, HarnessError>>, HarnessError> {
        let config = controls
            .mcp
            .as_ref()
            .ok_or_else(|| HarnessError::Protocol("missing Jolt MCP configuration".into()))?;
        let current = call_mcp_tool(config, 1, "goal_get", serde_json::json!({})).await?;
        let goal = &current["result"]["structuredContent"];
        let goal_id = goal["id"]
            .as_str()
            .ok_or_else(|| HarnessError::Protocol("goal_get omitted goal id".into()))?;
        let revision = goal["revision"]
            .as_u64()
            .ok_or_else(|| HarnessError::Protocol("goal_get omitted revision".into()))?;
        let completed = call_mcp_tool(
            config,
            2,
            "goal_complete",
            serde_json::json!({
                "goalId": goal_id,
                "expectedRevision": revision,
                "summary": "Completed through Jolt MCP"
            }),
        )
        .await?;
        if completed["result"]["isError"] == true {
            return Err(HarnessError::Protocol(format!(
                "goal_complete failed: {}",
                completed["result"]
            )));
        }

        let events = vec![
            AgentEvent::SessionStarted {
                harness: HarnessId::Mock,
                model: "mock".into(),
                tools: vec!["goal_complete".into()],
                cwd: request.cwd,
                session_id: "mcp-session".into(),
                assistant_message_id: "mcp-assistant".into(),
            },
            AgentEvent::TextDelta {
                text: "Goal complete.".into(),
            },
            AgentEvent::Usage {
                input_tokens: 11,
                output_tokens: 7,
                cache_read_input_tokens: 0,
                cache_write_input_tokens: 0,
                cost_usd: None,
                cost_provenance: None,
                context_tokens: None,
                context_window: None,
            },
            done(DoneStatus::Completed),
        ];
        Ok(futures::stream::iter(events.into_iter().map(Ok)).boxed())
    }
}

struct McpAskingHarness;

#[async_trait]
impl Harness for McpAskingHarness {
    fn id(&self) -> HarnessId {
        HarnessId::Mock
    }

    fn display_name(&self) -> &str {
        "MCP asking"
    }

    fn supports_steering(&self) -> bool {
        false
    }

    fn supports_mcp(&self) -> bool {
        true
    }

    fn steering_mode(&self) -> SteeringMode {
        SteeringMode::TurnBoundary
    }

    fn reasoning_levels(&self) -> &[ReasoningLevel] {
        &[]
    }

    async fn models(&self) -> Result<Vec<Model>, HarnessError> {
        Ok(Vec::new())
    }

    async fn run(
        &self,
        request: RunRequest,
        controls: RunControls,
    ) -> Result<BoxStream<'static, Result<AgentEvent, HarnessError>>, HarnessError> {
        let config = controls
            .mcp
            .ok_or_else(|| HarnessError::Protocol("missing Jolt MCP configuration".into()))?;
        let started = AgentEvent::SessionStarted {
            harness: HarnessId::Mock,
            model: "mock".into(),
            tools: vec!["request_answers".into()],
            cwd: request.cwd,
            session_id: "mcp-answer-session".into(),
            assistant_message_id: "mcp-answer-assistant".into(),
        };
        let answer = async move {
            let response = call_mcp_tool(
                &config,
                1,
                "request_answers",
                serde_json::json!({
                    "questions": [{
                        "header": "Decision",
                        "question": "What should happen next?",
                        "options": ["Ship", "Document", "Wait"],
                        "multiSelect": true
                    }]
                }),
            )
            .await?;
            if response["result"]["isError"] == true {
                return Err(HarnessError::Protocol(format!(
                    "request_answers failed: {}",
                    response["result"]
                )));
            }
            let labels = response["result"]["structuredContent"]["answers"][0]["labels"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(serde_json::Value::as_str)
                .collect::<Vec<_>>()
                .join(", ");
            Ok(AgentEvent::TextDelta {
                text: format!("selected {labels}"),
            })
        };
        Ok(futures::stream::once(async move { Ok(started) })
            .chain(futures::stream::once(answer))
            .chain(futures::stream::once(async {
                Ok(done(DoneStatus::Completed))
            }))
            .boxed())
    }
}

struct McpStartupFailureHarness;

#[async_trait]
impl Harness for McpStartupFailureHarness {
    fn id(&self) -> HarnessId {
        HarnessId::Mock
    }

    fn display_name(&self) -> &str {
        "MCP startup failure"
    }

    fn supports_steering(&self) -> bool {
        false
    }

    fn supports_mcp(&self) -> bool {
        true
    }

    fn steering_mode(&self) -> SteeringMode {
        SteeringMode::TurnBoundary
    }

    fn reasoning_levels(&self) -> &[ReasoningLevel] {
        &[]
    }

    async fn models(&self) -> Result<Vec<Model>, HarnessError> {
        Ok(Vec::new())
    }

    async fn run(
        &self,
        _request: RunRequest,
        controls: RunControls,
    ) -> Result<BoxStream<'static, Result<AgentEvent, HarnessError>>, HarnessError> {
        if controls.mcp.is_none() {
            return Err(HarnessError::Protocol(
                "missing Jolt MCP configuration".into(),
            ));
        }
        Err(HarnessError::Protocol("startup failed".into()))
    }
}

struct EditingHarness {
    report_file_tool: bool,
}

#[async_trait]
impl Harness for EditingHarness {
    fn id(&self) -> HarnessId {
        HarnessId::Mock
    }

    fn display_name(&self) -> &str {
        "Editing"
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
        std::fs::write(
            std::path::Path::new(&request.cwd).join("changed.txt"),
            "after\n",
        )
        .map_err(HarnessError::Io)?;
        let mut events = vec![AgentEvent::SessionStarted {
            harness: HarnessId::Mock,
            model: "mock".into(),
            tools: vec![],
            cwd: request.cwd,
            session_id: "editing-session".into(),
            assistant_message_id: "assistant".into(),
        }];
        if self.report_file_tool {
            events.extend([
                AgentEvent::ToolCall {
                    id: "edit".into(),
                    call: ToolCall::EditFile {
                        path: "changed.txt".into(),
                        old_string: Some("before\n".into()),
                        new_string: Some("after\n".into()),
                    },
                },
                AgentEvent::ToolResult {
                    id: "edit".into(),
                    is_error: false,
                },
            ]);
        } else {
            events.push(AgentEvent::TextDelta {
                text: "No files changed.".into(),
            });
        }
        events.push(done(DoneStatus::Completed));
        Ok(futures::stream::iter(events.into_iter().map(Ok)).boxed())
    }
}

/// Scripted harness with a per-event delay; optionally hangs after the script until its
/// interrupt token cancels, then ends with `Done{interrupted}`.
struct ScriptedHarness {
    script: Vec<AgentEvent>,
    step_delay: Duration,
    hang_until_interrupt: bool,
}

#[async_trait]
impl Harness for ScriptedHarness {
    fn id(&self) -> HarnessId {
        HarnessId::Mock
    }
    fn display_name(&self) -> &str {
        "Scripted"
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
        let (tx, rx) = tokio::sync::mpsc::channel::<Result<AgentEvent, HarnessError>>(16);
        let script = self.script.clone();
        let delay = self.step_delay;
        let hang = self.hang_until_interrupt;
        let token = controls.interrupt.clone();
        tokio::spawn(async move {
            for event in script {
                if tx.send(Ok(event)).await.is_err() {
                    return;
                }
                tokio::time::sleep(delay).await;
            }
            if hang {
                token.cancelled().await;
                let _ = tx.send(Ok(done(DoneStatus::Interrupted))).await;
            }
        });
        Ok(futures::stream::unfold(rx, |mut rx| async move {
            rx.recv().await.map(|event| (event, rx))
        })
        .boxed())
    }
}

struct CompactionContinuationHarness {
    continuation: Arc<Mutex<Option<String>>>,
    resumes_without_prompt: bool,
}

#[async_trait]
impl Harness for CompactionContinuationHarness {
    fn id(&self) -> HarnessId {
        HarnessId::Mock
    }

    fn display_name(&self) -> &str {
        "Compaction continuation"
    }

    fn supports_steering(&self) -> bool {
        true
    }

    fn steering_mode(&self) -> SteeringMode {
        SteeringMode::StepBoundary
    }

    fn reasoning_levels(&self) -> &[ReasoningLevel] {
        &[]
    }

    async fn models(&self) -> Result<Vec<Model>, HarnessError> {
        Ok(Vec::new())
    }

    async fn run(
        &self,
        _request: RunRequest,
        controls: RunControls,
    ) -> Result<BoxStream<'static, Result<AgentEvent, HarnessError>>, HarnessError> {
        let continuation = self.continuation.clone();
        let resumes_without_prompt = self.resumes_without_prompt;
        let mut steering = controls.steering;
        let interrupt = controls.interrupt;
        let (tx, rx) = tokio::sync::mpsc::channel(16);
        tokio::spawn(async move {
            for event in [
                AgentEvent::SessionStarted {
                    harness: HarnessId::Mock,
                    model: "mock-1".into(),
                    tools: Vec::new(),
                    cwd: "/tmp".into(),
                    session_id: "hs-compaction".into(),
                    assistant_message_id: "a-before-compaction".into(),
                },
                AgentEvent::CompactionStarted,
                AgentEvent::CompactionFinished,
            ] {
                if tx.send(Ok(event)).await.is_err() {
                    return;
                }
            }
            if resumes_without_prompt
                && tx
                    .send(Ok(AgentEvent::TextDelta {
                        text: "Resumed naturally".into(),
                    }))
                    .await
                    .is_err()
            {
                return;
            }
            if tx.send(Ok(done(DoneStatus::Completed))).await.is_err() {
                return;
            }

            tokio::select! {
                message = steering.recv() => {
                    let Some(message) = message else { return };
                    *continuation.lock().unwrap() = Some(message.prompt);
                    for event in [
                        AgentEvent::Steered {
                            assistant_message_id: Some("a-before-compaction".into()),
                            next_assistant_message_id: Some("a-after-compaction".into()),
                        },
                        AgentEvent::TextDelta { text: "Resumed after compaction".into() },
                        done(DoneStatus::Completed),
                    ] {
                        if tx.send(Ok(event)).await.is_err() {
                            return;
                        }
                    }
                    interrupt.cancelled().await;
                }
                _ = interrupt.cancelled() => {}
            }
        });
        Ok(futures::stream::unfold(rx, |mut rx| async move {
            rx.recv().await.map(|event| (event, rx))
        })
        .boxed())
    }
}

struct CompactionUserCancellationHarness {
    messages: Arc<Mutex<Vec<String>>>,
}

#[async_trait]
impl Harness for CompactionUserCancellationHarness {
    fn id(&self) -> HarnessId {
        HarnessId::Mock
    }

    fn display_name(&self) -> &str {
        "Compaction user cancellation"
    }

    fn supports_steering(&self) -> bool {
        true
    }

    fn steering_mode(&self) -> SteeringMode {
        SteeringMode::StepBoundary
    }

    fn reasoning_levels(&self) -> &[ReasoningLevel] {
        &[]
    }

    async fn models(&self) -> Result<Vec<Model>, HarnessError> {
        Ok(Vec::new())
    }

    async fn run(
        &self,
        _request: RunRequest,
        controls: RunControls,
    ) -> Result<BoxStream<'static, Result<AgentEvent, HarnessError>>, HarnessError> {
        let messages = self.messages.clone();
        let mut steering = controls.steering;
        let interrupt = controls.interrupt;
        let (tx, rx) = tokio::sync::mpsc::channel(16);
        tokio::spawn(async move {
            for event in [
                AgentEvent::SessionStarted {
                    harness: HarnessId::Mock,
                    model: "mock-1".into(),
                    tools: Vec::new(),
                    cwd: "/tmp".into(),
                    session_id: "hs-user-cancel".into(),
                    assistant_message_id: "a-user-cancel".into(),
                },
                AgentEvent::CompactionStarted,
                AgentEvent::CompactionFinished,
            ] {
                if tx.send(Ok(event)).await.is_err() {
                    return;
                }
            }

            let first = tokio::select! {
                message = steering.recv() => message,
                _ = interrupt.cancelled() => return,
            };
            let Some(first) = first else { return };
            messages.lock().unwrap().push(first.prompt);
            if tx.send(Ok(done(DoneStatus::Completed))).await.is_err() {
                return;
            }

            tokio::select! {
                message = steering.recv() => {
                    if let Some(message) = message {
                        messages.lock().unwrap().push(message.prompt);
                    }
                }
                _ = interrupt.cancelled() => return,
            }
            interrupt.cancelled().await;
        });
        Ok(futures::stream::unfold(rx, |mut rx| async move {
            rx.recv().await.map(|event| (event, rx))
        })
        .boxed())
    }
}

struct PromptCapturingHarness {
    seen: Arc<Mutex<Vec<RunRequest>>>,
}

#[async_trait]
impl Harness for PromptCapturingHarness {
    fn id(&self) -> HarnessId {
        HarnessId::Mock
    }

    fn display_name(&self) -> &str {
        "Prompt capture"
    }

    fn supports_steering(&self) -> bool {
        false
    }

    fn steering_mode(&self) -> SteeringMode {
        SteeringMode::TurnBoundary
    }

    fn reasoning_levels(&self) -> &[ReasoningLevel] {
        &[]
    }

    async fn models(&self) -> Result<Vec<Model>, HarnessError> {
        Ok(Vec::new())
    }

    async fn run(
        &self,
        request: RunRequest,
        _controls: RunControls,
    ) -> Result<BoxStream<'static, Result<AgentEvent, HarnessError>>, HarnessError> {
        self.seen.lock().unwrap().push(request);
        Ok(futures::stream::iter(vec![
            Ok(AgentEvent::SessionStarted {
                harness: HarnessId::Mock,
                model: "mock".into(),
                tools: Vec::new(),
                cwd: "/tmp".into(),
                session_id: "capture-session".into(),
                assistant_message_id: "capture-assistant".into(),
            }),
            Ok(done(DoneStatus::Completed)),
        ])
        .boxed())
    }
}

struct QueuedTurnHarness {
    prompts: Arc<Mutex<Vec<String>>>,
    release_first: Arc<tokio::sync::Notify>,
    release_queued: Arc<tokio::sync::Notify>,
}

#[async_trait]
impl Harness for QueuedTurnHarness {
    fn id(&self) -> HarnessId {
        HarnessId::Mock
    }

    fn display_name(&self) -> &str {
        "Queued turns"
    }

    fn supports_steering(&self) -> bool {
        true
    }

    fn steering_mode(&self) -> SteeringMode {
        SteeringMode::StepBoundary
    }

    fn reasoning_levels(&self) -> &[ReasoningLevel] {
        &[]
    }

    async fn models(&self) -> Result<Vec<Model>, HarnessError> {
        Ok(Vec::new())
    }

    async fn run(
        &self,
        request: RunRequest,
        controls: RunControls,
    ) -> Result<BoxStream<'static, Result<AgentEvent, HarnessError>>, HarnessError> {
        if request.prompt.starts_with("Reply with ONLY a concise") {
            return Ok(futures::stream::iter(vec![
                Ok(AgentEvent::TextDelta {
                    text: "Queue test".into(),
                }),
                Ok(done(DoneStatus::Completed)),
            ])
            .boxed());
        }
        self.prompts.lock().unwrap().push(request.prompt);
        let prompts = self.prompts.clone();
        let release_first = self.release_first.clone();
        let release_queued = self.release_queued.clone();
        let mut steering = controls.steering;
        let interrupt = controls.interrupt;
        let (tx, rx) = tokio::sync::mpsc::channel(16);
        tokio::spawn(async move {
            if tx
                .send(Ok(AgentEvent::SessionStarted {
                    harness: HarnessId::Mock,
                    model: "mock".into(),
                    tools: Vec::new(),
                    cwd: "/tmp".into(),
                    session_id: "queued-session".into(),
                    assistant_message_id: "queued-a-0".into(),
                }))
                .await
                .is_err()
            {
                return;
            }
            release_first.notified().await;
            if tx.send(Ok(done(DoneStatus::Completed))).await.is_err() {
                return;
            }
            let mut turn = 1usize;
            loop {
                tokio::select! {
                    message = steering.recv() => {
                        let Some(message) = message else { return };
                        prompts.lock().unwrap().push(message.prompt);
                        if turn == 1 {
                            release_queued.notified().await;
                        }
                        for event in [
                            AgentEvent::Steered {
                                assistant_message_id: Some(format!("queued-a-{}", turn - 1)),
                                next_assistant_message_id: Some(format!("queued-a-{turn}")),
                            },
                            done(DoneStatus::Completed),
                        ] {
                            if tx.send(Ok(event)).await.is_err() {
                                return;
                            }
                        }
                        turn += 1;
                    }
                    _ = interrupt.cancelled() => return,
                }
            }
        });
        Ok(futures::stream::unfold(rx, |mut rx| async move {
            rx.recv().await.map(|event| (event, rx))
        })
        .boxed())
    }
}

struct BashHarness {
    seen: Arc<Mutex<Vec<BashRequest>>>,
    release: Option<Arc<tokio::sync::Notify>>,
}

#[async_trait]
impl Harness for BashHarness {
    fn id(&self) -> HarnessId {
        HarnessId::Pi
    }

    fn display_name(&self) -> &str {
        "Pi"
    }

    fn supports_steering(&self) -> bool {
        true
    }

    fn supports_native_bash(&self) -> bool {
        true
    }

    fn steering_mode(&self) -> SteeringMode {
        SteeringMode::StepBoundary
    }

    fn reasoning_levels(&self) -> &[ReasoningLevel] {
        &[]
    }

    async fn models(&self) -> Result<Vec<Model>, HarnessError> {
        Ok(Vec::new())
    }

    async fn bash(&self, request: BashRequest) -> Result<BashResult, HarnessError> {
        self.seen.lock().unwrap().push(request);
        if let Some(release) = &self.release {
            release.notified().await;
        }
        Ok(BashResult {
            output: "shell-output\n".into(),
            exit_code: Some(0),
            cancelled: false,
            truncated: false,
            full_output_path: None,
            session_id: Some("pi-shell-session".into()),
        })
    }

    async fn run(
        &self,
        _request: RunRequest,
        _controls: RunControls,
    ) -> Result<BoxStream<'static, Result<AgentEvent, HarnessError>>, HarnessError> {
        Err(HarnessError::Protocol("unexpected agent run".into()))
    }
}

fn registry_with(harness: Arc<dyn Harness>) -> Arc<HarnessRegistry> {
    let registry = HarnessRegistry::new();
    registry.register(harness);
    Arc::new(registry)
}

fn assemble(dir: &std::path::Path, harness: Arc<dyn Harness>) -> EngineCore {
    EngineCore::assemble(dir, registry_with(harness), HarnessId::Mock, None)
        .expect("engine core assembles")
}

/// Queue a command into the chat doc the way a REMOTE viewer device would: an immutable
/// pending entry appended under the viewer's device id (ledger rule 1).
fn queue_as_viewer(doc: &SessionDoc, id: &str, payload: SessionCommandPayload) {
    let now = chrono::Utc::now().timestamp_millis();
    let based_on = doc.read_entries().expect("read entries").last().map(|m| {
        jolt_session_doc::CommandBasedOn {
            turn_id: Some(m.id.clone()),
            frontier: None,
        }
    });
    doc.queue_command(&SessionCommandEntry {
        id: id.into(),
        payload,
        issued_by: VIEWER.into(),
        issued_at: now,
        based_on,
        expires_at: None,
        status: SessionCommandStatus::Pending,
        resolution: None,
    })
    .expect("queue command");
}

async fn wait_for<F>(mut predicate: F, what: &str)
where
    F: FnMut() -> bool,
{
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while !predicate() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for {what}"
        );
        tokio::time::sleep(Duration::from_millis(15)).await;
    }
}

fn entries(core: &EngineCore) -> Vec<SessionMessageEntry> {
    core.doc_host
        .open(CHAT)
        .expect("open chat")
        .doc()
        .read_entries()
        .expect("read entries")
}

/// Tolerant read for hot-polling predicates: a snapshot taken between a
/// segment writer's `push_container` and its field writes deserializes with
/// fields missing — treat that instant as "not yet" instead of panicking.
fn entries_now(core: &EngineCore) -> Vec<SessionMessageEntry> {
    core.doc_host
        .open(CHAT)
        .ok()
        .and_then(|h| h.doc().read_entries().ok())
        .unwrap_or_default()
}

fn command_status(core: &EngineCore, id: &str) -> Option<(SessionCommandStatus, Option<String>)> {
    core.doc_host
        .open(CHAT)
        .expect("open chat")
        .doc()
        .read_commands()
        .expect("read commands")
        .into_iter()
        .find(|c| c.id == id)
        .map(|c| (c.status, c.resolution))
}

#[tokio::test]
async fn mcp_completion_during_harness_startup_is_scoped_and_accounted() {
    let dir = tempfile::tempdir().unwrap();
    let core = assemble(dir.path(), Arc::new(McpCompletingHarness));
    core.workspace.claim_chat(CHAT, Some("/tmp")).unwrap();
    let handle = core.doc_host.open(CHAT).unwrap();
    queue_as_viewer(
        handle.doc(),
        "goal-create",
        SessionCommandPayload::Goal {
            operation: GoalOperation::Create {
                objective: "Complete through MCP".into(),
                token_budget: Some(100),
            },
        },
    );

    wait_for(
        || {
            core.workspace
                .chat_goal(CHAT)
                .is_some_and(|goal| goal.status == GoalStatus::Complete && goal.turns == 1)
        },
        "MCP goal completion",
    )
    .await;

    let goal = core.workspace.chat_goal(CHAT).unwrap();
    assert_eq!(goal.status, GoalStatus::Complete);
    assert_eq!(
        goal.status_message.as_deref(),
        Some("Completed through Jolt MCP")
    );
    assert_eq!(goal.tokens_used, 18);
    assert_eq!(goal.turns, 1);
    assert!(goal.elapsed_active_ms > 0);
    assert_eq!(
        command_status(&core, "goal-create").map(|status| status.0),
        Some(SessionCommandStatus::Applied)
    );
}

#[tokio::test]
async fn harness_startup_failure_pauses_and_accounts_the_active_goal() {
    let dir = tempfile::tempdir().unwrap();
    let core = assemble(dir.path(), Arc::new(McpStartupFailureHarness));
    core.workspace.claim_chat(CHAT, Some("/tmp")).unwrap();
    let handle = core.doc_host.open(CHAT).unwrap();
    queue_as_viewer(
        handle.doc(),
        "failing-goal-create",
        SessionCommandPayload::Goal {
            operation: GoalOperation::Create {
                objective: "Encounter startup failure".into(),
                token_budget: None,
            },
        },
    );

    wait_for(
        || {
            core.workspace.chat_goal(CHAT).is_some_and(|goal| {
                goal.status == GoalStatus::Paused
                    && goal.pause_source == Some(jolt_proto::GoalPauseSource::System)
                    && goal.turns == 1
            })
        },
        "failed startup goal accounting",
    )
    .await;

    let goal = core.workspace.chat_goal(CHAT).unwrap();
    assert_eq!(
        goal.status_message.as_deref(),
        Some("harness protocol error: startup failed")
    );
    assert_eq!(goal.tokens_used, 0);
    assert!(goal.elapsed_active_ms > 0);
}

#[tokio::test]
async fn mcp_request_answers_round_trips_through_the_composer_ui() {
    let dir = tempfile::tempdir().unwrap();
    let core = assemble(dir.path(), Arc::new(McpAskingHarness));
    let handle = core.doc_host.open(CHAT).unwrap();
    queue_as_viewer(
        handle.doc(),
        "mcp-answer-run",
        SessionCommandPayload::Run {
            request: run_request("ask through Jolt"),
            message_id: "mcp-answer-user".into(),
        },
    );

    wait_for(
        || {
            core.sessions
                .session_status(CHAT)
                .map(|status| status.status)
                == Some(SessionStatus::AwaitingInput)
        },
        "MCP answer UI",
    )
    .await;
    wait_for(
        || {
            entries_now(&core).iter().any(|entry| {
                entry.parts.iter().any(|part| {
                    matches!(
                        part,
                        MessagePart::Input {
                            resolved: false,
                            ..
                        }
                    )
                })
            })
        },
        "MCP input transcript part",
    )
    .await;
    let (request_id, question) = entries(&core)
        .iter()
        .find_map(|entry| {
            entry.parts.iter().find_map(|part| match part {
                MessagePart::Input {
                    request_id,
                    questions,
                    resolved: false,
                    ..
                } => questions
                    .first()
                    .map(|question| (request_id.clone(), question.clone())),
                _ => None,
            })
        })
        .unwrap();
    assert_eq!(question.header, "Decision");
    assert_eq!(question.question, "What should happen next?");
    assert_eq!(question.options, ["Ship", "Document", "Wait"]);
    assert!(question.multi_select);
    let question_id = question.id;
    queue_as_viewer(
        handle.doc(),
        "mcp-answer-response",
        SessionCommandPayload::RespondInput {
            request_id,
            answers: vec![jolt_proto::UserInputAnswer {
                question_id,
                labels: vec!["Ship".into(), "Document".into()],
            }],
        },
    );

    wait_for(
        || {
            entries_now(&core).iter().any(|entry| {
                entry.status == Some(MessageStatus::Complete)
                    && entry.parts.iter().any(
                        |part| matches!(part, MessagePart::Text { text, .. } if text == "selected Ship, Document"),
                    )
            })
        },
        "MCP answer tool result",
    )
    .await;
    assert_eq!(
        command_status(&core, "mcp-answer-response"),
        Some((SessionCommandStatus::Applied, None))
    );
    assert!(entries(&core).iter().any(|entry| {
        entry
            .parts
            .iter()
            .any(|part| matches!(part, MessagePart::Input { resolved: true, .. }))
    }));
}

#[tokio::test]
async fn queued_messages_drain_fifo_as_one_batch_at_the_next_boundary() {
    let dir = tempfile::tempdir().unwrap();
    let prompts = Arc::new(Mutex::new(Vec::new()));
    let release_first = Arc::new(tokio::sync::Notify::new());
    let release_queued = Arc::new(tokio::sync::Notify::new());
    let core = assemble(
        dir.path(),
        Arc::new(QueuedTurnHarness {
            prompts: prompts.clone(),
            release_first: release_first.clone(),
            release_queued: release_queued.clone(),
        }),
    );

    let handle = core.doc_host.open(CHAT).unwrap();
    queue_as_viewer(
        handle.doc(),
        "run-first",
        SessionCommandPayload::Run {
            request: run_request("first"),
            message_id: "m-first".into(),
        },
    );
    wait_for(|| !prompts.lock().unwrap().is_empty(), "first turn").await;
    assert_eq!(prompts.lock().unwrap().as_slice(), ["first"]);

    let first_queue = "queue-second";
    queue_as_viewer(
        handle.doc(),
        first_queue,
        SessionCommandPayload::Queue {
            request: run_request("second"),
            message_id: "m-second".into(),
        },
    );
    let third_queue = "queue-third";
    queue_as_viewer(
        handle.doc(),
        third_queue,
        SessionCommandPayload::Queue {
            request: run_request("third"),
            message_id: "m-third".into(),
        },
    );
    wait_for(
        || {
            command_status(&core, first_queue)
                .is_some_and(|value| value.0 == SessionCommandStatus::Pending)
        },
        "queued command to remain pending",
    )
    .await;
    assert!(!entries(&core).iter().any(|entry| entry.id == "m-second"));

    release_first.notify_one();
    wait_for(
        || prompts.lock().unwrap().as_slice() == ["first", "second"],
        "first queued prompt",
    )
    .await;
    wait_for(
        || {
            [first_queue, third_queue].iter().all(|id| {
                command_status(&core, id)
                    .is_some_and(|value| value.0 == SessionCommandStatus::Applied)
            })
        },
        "whole queue to drain before the queued turn settles",
    )
    .await;
    release_queued.notify_one();
    wait_for(
        || prompts.lock().unwrap().as_slice() == ["first", "second", "third"],
        "queued prompts in FIFO order",
    )
    .await;
    let user_ids: Vec<_> = entries(&core)
        .into_iter()
        .filter(|entry| entry.role == MessageRole::User)
        .map(|entry| entry.id)
        .collect();
    assert_eq!(user_ids, ["m-first", "m-second", "m-third"]);
}

#[tokio::test]
async fn pending_queued_message_can_be_cancelled() {
    let dir = tempfile::tempdir().unwrap();
    let prompts = Arc::new(Mutex::new(Vec::new()));
    let release_first = Arc::new(tokio::sync::Notify::new());
    let core = assemble(
        dir.path(),
        Arc::new(QueuedTurnHarness {
            prompts: prompts.clone(),
            release_first: release_first.clone(),
            release_queued: Arc::new(tokio::sync::Notify::new()),
        }),
    );
    let handle = core.doc_host.open(CHAT).unwrap();
    queue_as_viewer(
        handle.doc(),
        "run-first",
        SessionCommandPayload::Run {
            request: run_request("first"),
            message_id: "m-first".into(),
        },
    );
    wait_for(|| !prompts.lock().unwrap().is_empty(), "first turn").await;
    assert_eq!(prompts.lock().unwrap().as_slice(), ["first"]);
    let queued = core
        .doc_host
        .queue_command(
            CHAT,
            SessionCommandPayload::Queue {
                request: run_request("cancel me"),
                message_id: "m-cancelled".into(),
            },
        )
        .unwrap();
    wait_for(
        || {
            command_status(&core, &queued)
                .is_some_and(|value| value.0 == SessionCommandStatus::Pending)
        },
        "pending queue item",
    )
    .await;
    assert!(core.doc_host.cancel_queued_prompt(CHAT, &queued).unwrap());
    release_first.notify_one();
    wait_for(
        || {
            command_status(&core, &queued)
                .is_some_and(|value| value.0 == SessionCommandStatus::Cancelled)
        },
        "queue cancellation",
    )
    .await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(prompts.lock().unwrap().as_slice(), ["first"]);
    assert!(!entries(&core).iter().any(|entry| entry.id == "m-cancelled"));
}

#[tokio::test]
async fn completed_turn_persists_an_authoritative_filesystem_diff() {
    let data = tempfile::tempdir().unwrap();
    let checkout = tempfile::tempdir().unwrap();
    let git = |args: &[&str]| {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(checkout.path())
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    };
    git(&["init", "-q"]);
    git(&["config", "user.email", "jolt@example.invalid"]);
    git(&["config", "user.name", "Jolt Test"]);
    std::fs::write(checkout.path().join("changed.txt"), "before\n").unwrap();
    std::fs::write(checkout.path().join("preexisting.txt"), "committed\n").unwrap();
    git(&["add", "."]);
    git(&["commit", "-qm", "initial"]);
    std::fs::write(
        checkout.path().join("preexisting.txt"),
        "dirty before turn\n",
    )
    .unwrap();

    let core = assemble(
        data.path(),
        Arc::new(EditingHarness {
            report_file_tool: true,
        }),
    );
    core.repos.set_vcs(jolt_proto::VcsKind::Git).unwrap();
    core.doc_host.open(CHAT).unwrap();
    let mut request = run_request("edit it");
    request.cwd = checkout.path().to_string_lossy().into_owned();
    core.sessions
        .dispatch(CHAT, HarnessId::Mock, request, Some("user".into()))
        .await
        .unwrap();
    wait_for(
        || {
            entries_now(&core).iter().any(|entry| {
                entry.role == MessageRole::Assistant
                    && entry.status == Some(MessageStatus::Complete)
                    && entry
                        .parts
                        .iter()
                        .any(|part| matches!(part, MessagePart::Changes { .. }))
            })
        },
        "turn diff",
    )
    .await;

    let assistant = entries(&core)
        .into_iter()
        .find(|entry| entry.role == MessageRole::Assistant)
        .unwrap();
    let diff = assistant
        .parts
        .iter()
        .find_map(|part| match part {
            MessagePart::Changes { diff, .. } => Some(diff),
            _ => None,
        })
        .unwrap();
    assert_eq!(diff.files.len(), 1);
    assert_eq!(diff.files[0].path, "changed.txt");
    assert_eq!(diff.attribution, jolt_proto::TurnDiffAttribution::Exact);
    let page = core
        .sessions
        .turn_diff_page(
            CHAT,
            &assistant.id,
            &diff.catalog_revision,
            &diff.pages[0].id,
        )
        .await
        .unwrap()
        .unwrap();
    assert!(page.patch.contains("-before"));
    assert!(page.patch.contains("+after"));
    assert!(!page.patch.contains("preexisting.txt"));
}

#[tokio::test]
async fn turn_without_a_successful_file_tool_ignores_checkout_changes() {
    let data = tempfile::tempdir().unwrap();
    let checkout = tempfile::tempdir().unwrap();
    let git = |args: &[&str]| {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(checkout.path())
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    };
    git(&["init", "-q"]);
    git(&["config", "user.email", "jolt@example.invalid"]);
    git(&["config", "user.name", "Jolt Test"]);
    std::fs::write(checkout.path().join("changed.txt"), "before\n").unwrap();
    git(&["add", "."]);
    git(&["commit", "-qm", "initial"]);

    let core = assemble(
        data.path(),
        Arc::new(EditingHarness {
            report_file_tool: false,
        }),
    );
    core.repos.set_vcs(jolt_proto::VcsKind::Git).unwrap();
    core.doc_host.open(CHAT).unwrap();
    let mut request = run_request("inspect it");
    request.cwd = checkout.path().to_string_lossy().into_owned();
    core.sessions
        .dispatch(CHAT, HarnessId::Mock, request, Some("user".into()))
        .await
        .unwrap();
    wait_for(
        || {
            entries_now(&core).iter().any(|entry| {
                entry.role == MessageRole::Assistant
                    && entry.status == Some(MessageStatus::Complete)
            })
        },
        "completed assistant turn",
    )
    .await;

    let assistant = entries(&core)
        .into_iter()
        .find(|entry| entry.role == MessageRole::Assistant)
        .unwrap();
    assert!(
        !assistant
            .parts
            .iter()
            .any(|part| matches!(part, MessagePart::Changes { .. }))
    );
}

#[tokio::test]
async fn hidden_prompt_produces_an_assistant_turn_without_a_user_entry() {
    let dir = tempfile::tempdir().unwrap();
    let core = assemble(
        dir.path(),
        Arc::new(MockHarness {
            script: mock_script(),
        }),
    );
    let handle = core.doc_host.open(CHAT).unwrap();

    queue_as_viewer(
        handle.doc(),
        "cmd-hidden-prompt",
        SessionCommandPayload::HiddenPrompt {
            request: run_request("Restate your last message simply."),
        },
    );
    wait_for(
        || {
            command_status(&core, "cmd-hidden-prompt")
                == Some((SessionCommandStatus::Applied, None))
        },
        "hidden prompt to complete",
    )
    .await;
    wait_for(
        || {
            entries_now(&core)
                .iter()
                .any(|entry| entry.role == MessageRole::Assistant)
        },
        "hidden prompt assistant response",
    )
    .await;

    let transcript = entries(&core);
    assert!(
        !transcript
            .iter()
            .any(|entry| entry.role == MessageRole::User)
    );
    let assistant = transcript
        .iter()
        .find(|entry| entry.role == MessageRole::Assistant)
        .expect("assistant response");
    let Some(MessagePart::Text { text, .. }) = assistant.parts.first() else {
        panic!("assistant text")
    };
    assert_eq!(text, "Hello");
}

#[tokio::test]
async fn queued_bash_command_executes_without_an_agent_turn() {
    let dir = tempfile::tempdir().unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let release = Arc::new(tokio::sync::Notify::new());
    let registry = registry_with(Arc::new(BashHarness {
        seen: seen.clone(),
        release: Some(release.clone()),
    }));
    let core = EngineCore::assemble(dir.path(), registry, HarnessId::Pi, None).unwrap();
    let handle = core.doc_host.open(CHAT).unwrap();

    queue_as_viewer(
        handle.doc(),
        "cmd-bash-1",
        SessionCommandPayload::Bash {
            command: "pwd".into(),
            exclude_from_context: true,
            cwd: "/tmp".into(),
            message_id: "msg-bash-1".into(),
        },
    );

    wait_for(
        || {
            entries_now(&core).iter().any(|entry| {
                entry.id == "msg-bash-1" && entry.status == Some(MessageStatus::Streaming)
            })
        },
        "pending bash transcript",
    )
    .await;
    let pending = entries(&core);
    let MessagePart::Text { text, .. } = &pending[0].parts[0] else {
        panic!("expected pending shell transcript")
    };
    assert!(text.contains("Output pending…"));
    assert_eq!(
        command_status(&core, "cmd-bash-1"),
        Some((SessionCommandStatus::Pending, None))
    );

    release.notify_one();
    wait_for(
        || {
            command_status(&core, "cmd-bash-1")
                .is_some_and(|(status, _)| status != SessionCommandStatus::Pending)
        },
        "bash command to complete",
    )
    .await;

    let requests = seen.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].command, "pwd");
    assert!(requests[0].exclude_from_context);
    drop(requests);

    let transcript = entries(&core);
    assert_eq!(transcript.len(), 1);
    assert_eq!(transcript[0].id, "msg-bash-1");
    assert_eq!(transcript[0].role, MessageRole::System);
    let MessagePart::Text { text, .. } = &transcript[0].parts[0] else {
        panic!("expected rendered shell output")
    };
    assert!(text.contains("$ pwd"));
    assert!(text.contains("shell-output"));
    assert!(text.contains("excluded from agent context"));
    assert_eq!(
        command_status(&core, "cmd-bash-1"),
        Some((SessionCommandStatus::Applied, None))
    );
}

#[cfg(unix)]
#[tokio::test]
async fn local_bash_fallback_includes_only_single_bang_output_in_next_prompt() {
    let dir = tempfile::tempdir().unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let core = assemble(
        dir.path(),
        Arc::new(PromptCapturingHarness { seen: seen.clone() }),
    );
    let handle = core.doc_host.open(CHAT).unwrap();
    let cwd = dir.path().to_string_lossy().into_owned();

    queue_as_viewer(
        handle.doc(),
        "cmd-bash-included",
        SessionCommandPayload::Bash {
            command: "printf included-shell-output".into(),
            exclude_from_context: false,
            cwd: cwd.clone(),
            message_id: "msg-bash-included".into(),
        },
    );
    queue_as_viewer(
        handle.doc(),
        "cmd-bash-excluded",
        SessionCommandPayload::Bash {
            command: "printf excluded-shell-output".into(),
            exclude_from_context: true,
            cwd: cwd.clone(),
            message_id: "msg-bash-excluded".into(),
        },
    );
    wait_for(
        || {
            command_status(&core, "cmd-bash-excluded")
                == Some((SessionCommandStatus::Applied, None))
        },
        "local bash commands to complete",
    )
    .await;

    let mut request = run_request("use the shell result");
    request.cwd = cwd;
    queue_as_viewer(
        handle.doc(),
        "cmd-run-after-bash",
        SessionCommandPayload::Run {
            request,
            message_id: "msg-run-after-bash".into(),
        },
    );
    wait_for(
        || {
            seen.lock()
                .unwrap()
                .iter()
                .any(|request| request.prompt.ends_with("use the shell result"))
        },
        "agent prompt after local bash",
    )
    .await;

    {
        let requests = seen.lock().unwrap();
        let request = requests
            .iter()
            .find(|request| {
                request.prompt.contains("included-shell-output")
                    && request.prompt.ends_with("use the shell result")
            })
            .expect("agent request");
        assert!(request.prompt.contains("included-shell-output"));
        assert!(!request.prompt.contains("excluded-shell-output"));
    }

    let user = entries(&core)
        .into_iter()
        .find(|entry| entry.id == "msg-run-after-bash")
        .expect("visible user entry");
    assert_eq!(
        user.parts,
        vec![MessagePart::Text {
            id: "t0".into(),
            text: "use the shell result".into(),
        }]
    );
    wait_for(
        || {
            command_status(&core, "cmd-run-after-bash")
                == Some((SessionCommandStatus::Applied, None))
        },
        "first agent turn to be applied",
    )
    .await;
    let mut second_request = run_request("second turn");
    second_request.cwd = dir.path().to_string_lossy().into_owned();
    queue_as_viewer(
        handle.doc(),
        "cmd-second-run",
        SessionCommandPayload::Run {
            request: second_request,
            message_id: "msg-second-run".into(),
        },
    );
    wait_for(
        || {
            seen.lock()
                .unwrap()
                .iter()
                .any(|request| request.prompt == "second turn")
        },
        "second agent turn",
    )
    .await;
    let requests = seen.lock().unwrap();
    let second = requests
        .iter()
        .find(|request| request.prompt == "second turn")
        .expect("second agent request");
    assert!(!second.prompt.contains("included-shell-output"));
}

#[tokio::test]
async fn queued_run_command_executes_end_to_end() {
    let dir = tempfile::tempdir().unwrap();
    let core = assemble(
        dir.path(),
        Arc::new(MockHarness {
            script: mock_script(),
        }),
    );
    let handle = core.doc_host.open(CHAT).unwrap();

    // Live event subscription (journal replay + broadcast) before anything runs.
    let (replayed, mut live) = core.sessions.subscribe(CHAT, 0).unwrap();
    assert!(replayed.is_empty());

    // A viewer device queues the run command into the doc.
    queue_as_viewer(
        handle.doc(),
        "cmd-run-1",
        SessionCommandPayload::Run {
            request: run_request("do the thing"),
            message_id: "msg-user-1".into(),
        },
    );

    // The host executor picks it up, runs the harness, and the doc settles.
    wait_for(
        || {
            entries(&core).iter().any(|e| {
                e.role == MessageRole::Assistant && e.status == Some(MessageStatus::Complete)
            })
        },
        "assistant entry to complete",
    )
    .await;

    let all = entries(&core);
    assert_eq!(all.len(), 2, "user + assistant entries, got {all:#?}");
    // User entry carries the command's client-minted message id.
    assert_eq!(all[0].id, "msg-user-1");
    assert_eq!(all[0].role, MessageRole::User);
    assert_eq!(
        all[0].parts,
        vec![MessagePart::Text {
            id: "t0".into(),
            text: "do the thing".into()
        }]
    );
    // Assistant entry: folded text, its pre-tool reveal boundary, then the resolved
    // sanitized tool call.
    let assistant = &all[1];
    assert_eq!(assistant.status, Some(MessageStatus::Complete));
    assert_eq!(assistant.parts.len(), 3);
    match &assistant.parts[0] {
        MessagePart::Text { text, .. } => assert_eq!(text, "Hello"),
        other => panic!("unexpected first part {other:?}"),
    }
    assert!(matches!(assistant.parts[1], MessagePart::TextReveal { .. }));
    match &assistant.parts[2] {
        MessagePart::Tool {
            call,
            resolved,
            is_error,
            ..
        } => {
            assert!(*resolved);
            assert!(!*is_error);
            assert_eq!(
                call,
                &ToolCall::WriteFile {
                    path: "/tmp/x".into(),
                    content: None
                }
            );
        }
        other => panic!("unexpected third part {other:?}"),
    }

    // Command outcome written by the host (sole outcome writer).
    assert_eq!(
        command_status(&core, "cmd-run-1"),
        Some((SessionCommandStatus::Applied, None))
    );

    // Journal replay: the full script in order, terminal Done last.
    let replay = core.sessions.subscribe(CHAT, 0).unwrap().0;
    assert_eq!(replay.len(), mock_script().len());
    assert!(matches!(
        replay.last().map(|j| &j.event),
        Some(AgentEvent::Done {
            status: DoneStatus::Completed,
            ..
        })
    ));
    let seqs: Vec<u64> = replay.iter().map(|j| j.seq).collect();
    assert_eq!(seqs, (1..=mock_script().len() as u64).collect::<Vec<_>>());

    // The live broadcast delivered the same events.
    let mut broadcast_count = 0usize;
    while let Ok(event) = live.try_recv() {
        assert!(event.seq >= 1);
        broadcast_count += 1;
    }
    assert_eq!(broadcast_count, mock_script().len());

    // Final session status: Idle.
    assert_eq!(
        core.sessions.session_status(CHAT).map(|s| s.status),
        Some(SessionStatus::Idle)
    );
}

#[tokio::test]
async fn session_status_transitions_idle_working_idle() {
    let dir = tempfile::tempdir().unwrap();
    let core = assemble(
        dir.path(),
        Arc::new(ScriptedHarness {
            script: mock_script(),
            step_delay: Duration::from_millis(40),
            hang_until_interrupt: false,
        }),
    );
    let mut watch = core.sessions.watch_sessions();
    assert!(watch.borrow().is_empty(), "no sessions before dispatch");

    let handle = core.doc_host.open(CHAT).unwrap();
    queue_as_viewer(
        handle.doc(),
        "cmd-run-status",
        SessionCommandPayload::Run {
            request: run_request("go"),
            message_id: "m-1".into(),
        },
    );

    let mut seen = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let status = tokio::time::timeout_at(deadline, watch.changed())
            .await
            .expect("status change before timeout")
            .map(|_| watch.borrow().first().map(|s| s.status))
            .expect("watch alive");
        if let Some(status) = status {
            if seen.last() != Some(&status) {
                seen.push(status);
            }
            if status == SessionStatus::Idle {
                break;
            }
        }
    }
    assert_eq!(seen, vec![SessionStatus::Working, SessionStatus::Idle]);
}

#[tokio::test]
async fn compaction_events_toggle_live_session_feedback() {
    let dir = tempfile::tempdir().unwrap();
    let core = assemble(
        dir.path(),
        Arc::new(ScriptedHarness {
            script: vec![
                AgentEvent::SessionStarted {
                    harness: HarnessId::Mock,
                    model: "mock-1".into(),
                    tools: vec![],
                    cwd: "/tmp".into(),
                    session_id: "hs-1".into(),
                    assistant_message_id: "a-1".into(),
                },
                AgentEvent::CompactionStarted,
                AgentEvent::CompactionFinished,
                done(DoneStatus::Completed),
            ],
            step_delay: Duration::from_millis(40),
            hang_until_interrupt: false,
        }),
    );
    let mut watch = core.sessions.watch_sessions();
    let handle = core.doc_host.open(CHAT).unwrap();
    queue_as_viewer(
        handle.doc(),
        "cmd-run-compaction",
        SessionCommandPayload::Run {
            request: run_request("go"),
            message_id: "m-1".into(),
        },
    );

    let mut seen = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        tokio::time::timeout_at(deadline, watch.changed())
            .await
            .expect("session change before timeout")
            .expect("watch alive");
        if let Some(session) = watch.borrow().first() {
            let state = (session.status, session.compacting);
            if seen.last() != Some(&state) {
                seen.push(state);
            }
            if session.status == SessionStatus::Idle {
                break;
            }
        }
    }
    assert_eq!(
        seen,
        vec![
            (SessionStatus::Working, false),
            (SessionStatus::Working, true),
            (SessionStatus::Working, false),
            (SessionStatus::Idle, false),
        ]
    );
}

#[tokio::test]
async fn compaction_shutdown_queues_a_hidden_continuation() {
    let dir = tempfile::tempdir().unwrap();
    let continuation = Arc::new(Mutex::new(None));
    let core = assemble(
        dir.path(),
        Arc::new(CompactionContinuationHarness {
            continuation: continuation.clone(),
            resumes_without_prompt: false,
        }),
    );
    let handle = core.doc_host.open(CHAT).unwrap();
    queue_as_viewer(
        handle.doc(),
        "cmd-run-compaction-continuation",
        SessionCommandPayload::Run {
            request: run_request("go"),
            message_id: "m-1".into(),
        },
    );

    wait_for(
        || continuation.lock().unwrap().is_some(),
        "compaction continuation",
    )
    .await;
    let prompt = continuation.lock().unwrap().clone().unwrap();
    assert!(prompt.contains("Resume the existing task"));
    wait_for(
        || {
            entries_now(&core).iter().any(|entry| {
                entry.role == MessageRole::Assistant
                    && entry.parts.iter().any(|part| {
                        matches!(part, MessagePart::Text { text, .. } if text == "Resumed after compaction")
                    })
            })
        },
        "resumed assistant turn",
    )
    .await;
    assert_eq!(
        entries(&core)
            .iter()
            .filter(|entry| entry.role == MessageRole::User)
            .count(),
        1,
        "the continuation prompt must stay hidden"
    );
    core.shutdown().await;
}

#[tokio::test]
async fn agent_message_after_compaction_cancels_the_continuation() {
    let dir = tempfile::tempdir().unwrap();
    let continuation = Arc::new(Mutex::new(None));
    let core = assemble(
        dir.path(),
        Arc::new(CompactionContinuationHarness {
            continuation: continuation.clone(),
            resumes_without_prompt: true,
        }),
    );
    let handle = core.doc_host.open(CHAT).unwrap();
    queue_as_viewer(
        handle.doc(),
        "cmd-run-compaction-natural-resume",
        SessionCommandPayload::Run {
            request: run_request("go"),
            message_id: "m-1".into(),
        },
    );

    wait_for(
        || {
            entries_now(&core).iter().any(|entry| {
                entry.role == MessageRole::Assistant
                    && entry.parts.iter().any(|part| {
                        matches!(part, MessagePart::Text { text, .. } if text == "Resumed naturally")
                    })
            })
        },
        "natural post-compaction output",
    )
    .await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(continuation.lock().unwrap().is_none());
    core.shutdown().await;
}

#[tokio::test]
async fn user_message_after_compaction_cancels_the_continuation() {
    let dir = tempfile::tempdir().unwrap();
    let messages = Arc::new(Mutex::new(Vec::new()));
    let core = assemble(
        dir.path(),
        Arc::new(CompactionUserCancellationHarness {
            messages: messages.clone(),
        }),
    );
    let handle = core.doc_host.open(CHAT).unwrap();
    queue_as_viewer(
        handle.doc(),
        "cmd-run-compaction-user-cancel",
        SessionCommandPayload::Run {
            request: run_request("go"),
            message_id: "m-1".into(),
        },
    );
    wait_for(
        || {
            core.sessions.subscribe(CHAT, 0).is_ok_and(|(replay, _)| {
                replay
                    .iter()
                    .any(|entry| entry.event == AgentEvent::CompactionFinished)
            })
        },
        "compaction finish",
    )
    .await;

    assert_eq!(
        core.sessions
            .steer(CHAT, "Keep going", Some("m-2".into()))
            .await
            .unwrap(),
        SteerOutcome::Accepted
    );
    wait_for(|| messages.lock().unwrap().len() == 1, "user steer").await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(messages.lock().unwrap().as_slice(), ["Keep going"]);
    core.shutdown().await;
}

#[tokio::test]
async fn interrupt_stamps_streaming_entry_aborted() {
    let dir = tempfile::tempdir().unwrap();
    let core = assemble(
        dir.path(),
        Arc::new(ScriptedHarness {
            script: vec![AgentEvent::TextDelta {
                text: "partial output".into(),
            }],
            step_delay: Duration::from_millis(5),
            hang_until_interrupt: true,
        }),
    );
    let handle = core.doc_host.open(CHAT).unwrap();
    queue_as_viewer(
        handle.doc(),
        "cmd-run-hang",
        SessionCommandPayload::Run {
            request: run_request("hang"),
            message_id: "m-1".into(),
        },
    );

    // Wait until the streaming entry is visibly in the doc, then interrupt via a
    // viewer-queued durable command (based_on = the streaming entry = current turn).
    wait_for(
        || {
            entries(&core)
                .iter()
                .any(|e| e.status == Some(MessageStatus::Streaming))
        },
        "streaming entry",
    )
    .await;
    queue_as_viewer(
        handle.doc(),
        "cmd-int-1",
        SessionCommandPayload::Interrupt {},
    );

    wait_for(
        || {
            entries(&core)
                .iter()
                .any(|e| e.status == Some(MessageStatus::Aborted))
        },
        "aborted stamp",
    )
    .await;

    let all = entries(&core);
    let assistant = all
        .iter()
        .find(|e| e.role == MessageRole::Assistant)
        .unwrap();
    assert_eq!(assistant.status, Some(MessageStatus::Aborted));
    match &assistant.parts[0] {
        MessagePart::Text { text, .. } => assert_eq!(text, "partial output"),
        other => panic!("unexpected part {other:?}"),
    }
    assert_eq!(
        command_status(&core, "cmd-int-1"),
        Some((SessionCommandStatus::Applied, None))
    );
    // Journal closed with a Done — nothing left to recover.
    let journal =
        RunJournal::open(dir.path().join("scopes/accounts/dev-org/dev-user/journals")).unwrap();
    assert!(journal.stale_sessions().unwrap().is_empty());
    assert_eq!(
        core.sessions.session_status(CHAT).map(|s| s.status),
        Some(SessionStatus::Idle)
    );
}

#[tokio::test]
async fn steer_with_no_live_run_falls_back_to_new_turn() {
    let dir = tempfile::tempdir().unwrap();
    let core = assemble(
        dir.path(),
        Arc::new(MockHarness {
            script: mock_script(),
        }),
    );
    let handle = core.doc_host.open(CHAT).unwrap();

    queue_as_viewer(
        handle.doc(),
        "cmd-run-1",
        SessionCommandPayload::Run {
            request: run_request("first"),
            message_id: "m-1".into(),
        },
    );
    wait_for(
        || {
            matches!(
                command_status(&core, "cmd-run-1"),
                Some((SessionCommandStatus::Applied, _))
            )
        },
        "first run applied",
    )
    .await;
    wait_for(
        || core.sessions.session_status(CHAT).map(|s| s.status) == Some(SessionStatus::Idle),
        "first run settled",
    )
    .await;

    // No live run anymore (mock finishes instantly): a steer command must fall
    // back to dispatch-as-next-turn.
    queue_as_viewer(
        handle.doc(),
        "cmd-steer-1",
        SessionCommandPayload::Steer {
            prompt: "also do this".into(),
            message_id: Some("m-2".into()),
        },
    );
    wait_for(
        || {
            matches!(
                command_status(&core, "cmd-steer-1"),
                Some((SessionCommandStatus::Applied, Some(_)))
            )
        },
        "steer fallback applied",
    )
    .await;
    let (status, resolution) = command_status(&core, "cmd-steer-1").unwrap();
    assert_eq!(status, SessionCommandStatus::Applied);
    assert_eq!(resolution.as_deref(), Some("queued as new turn"));

    wait_for(
        || {
            entries(&core)
                .iter()
                .filter(|e| {
                    e.role == MessageRole::Assistant && e.status == Some(MessageStatus::Complete)
                })
                .count()
                == 2
        },
        "second assistant entry",
    )
    .await;
    // The steer prompt became a user entry with its client-minted id.
    assert!(
        entries(&core)
            .iter()
            .any(|e| e.id == "m-2" && e.role == MessageRole::User)
    );
}

#[tokio::test]
async fn processed_commands_are_skipped_on_redelivery() {
    let dir = tempfile::tempdir().unwrap();

    // Simulate a crash AFTER mark-processed but BEFORE execute/outcome: the ledger has
    // the id, the doc still says pending.
    {
        let store = DocsStore::open(dir.path().join("scopes/accounts/dev-org/dev-user")).unwrap();
        assert!(store.mark_processed("cmd-crashed").unwrap());
    }

    let core = assemble(
        dir.path(),
        Arc::new(MockHarness {
            script: mock_script(),
        }),
    );
    let handle = core.doc_host.open(CHAT).unwrap();
    queue_as_viewer(
        handle.doc(),
        "cmd-crashed",
        SessionCommandPayload::Run {
            request: run_request("never again"),
            message_id: "m-x".into(),
        },
    );

    // Give the drain a moment: the command must be SKIPPED — no user entry, no run.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        entries(&core).is_empty(),
        "skipped command must not execute"
    );
    assert_eq!(
        command_status(&core, "cmd-crashed"),
        Some((SessionCommandStatus::Pending, None)),
        "skip leaves the entry pending without an outcome"
    );
    assert!(core.sessions.session_status(CHAT).is_none());

    // Direct ledger-evaluation check: re-evaluating a processed command = Skip.
    let store = DocsStore::open(dir.path().join("scopes/accounts/dev-org/dev-user")).unwrap();
    let commands = handle.doc().read_commands().unwrap();
    let entry = commands.iter().find(|c| c.id == "cmd-crashed").unwrap();
    let is_processed = |id: &str| store.is_processed(id).unwrap_or(false);
    let never_past = |_: &str| false;
    let verdict = jolt_session_doc::evaluate_command(
        entry,
        &jolt_session_doc::EvaluationContext {
            is_processed: &is_processed,
            now_ms: chrono::Utc::now().timestamp_millis(),
            entries: &commands,
            current_turn_id: None,
            turn_is_past: &never_past,
        },
    );
    assert_eq!(verdict, jolt_session_doc::CommandDisposition::Skip);
}

#[tokio::test]
async fn recover_stale_journal_stamps_aborted_on_boot() {
    let dir = tempfile::tempdir().unwrap();
    let device_id = "dev-host-fixed";
    let scope = dir.path().join("scopes/accounts/dev-org/dev-user");
    std::fs::create_dir_all(&scope).unwrap();
    std::fs::write(scope.join("device-id"), device_id).unwrap();

    // Craft the crash state: a journal without a terminal Done + a doc snapshot whose
    // assistant entry is still `streaming`.
    {
        let journal =
            RunJournal::open(dir.path().join("scopes/accounts/dev-org/dev-user/journals")).unwrap();
        journal
            .append(
                CHAT,
                &AgentEvent::TextDelta {
                    text: "doomed".into(),
                },
            )
            .unwrap();

        let doc = SessionDoc::init(CHAT).unwrap();
        doc.push_message(&SessionMessageEntry {
            id: "m-user".into(),
            role: MessageRole::User,
            parts: vec![MessagePart::Text {
                id: "t0".into(),
                text: "hi".into(),
            }],
            created_at: 1,
            device_id: device_id.into(),
            status: Some(MessageStatus::Complete),
            continuation_of: None,
        })
        .unwrap();
        let mut writer = SegmentWriter::begin(&doc, "m-assist", device_id, 2).unwrap();
        writer
            .sync(&[MessagePart::Text {
                id: "t0".into(),
                text: "doomed".into(),
            }])
            .unwrap();
        // No finish — the "process" dies here with the entry still streaming.
        let store = DocsStore::open(dir.path().join("scopes/accounts/dev-org/dev-user")).unwrap();
        store
            .save_snapshot(CHAT, &doc.export_snapshot().unwrap())
            .unwrap();
    }

    // Boot: EngineCore::assemble runs recover_stale.
    let core = assemble(
        dir.path(),
        Arc::new(MockHarness {
            script: mock_script(),
        }),
    );
    assert_eq!(core.device_id, device_id);

    let all = entries(&core);
    let assistant = all.iter().find(|e| e.id == "m-assist").unwrap();
    assert_eq!(assistant.status, Some(MessageStatus::Aborted));
    match &assistant.parts[0] {
        MessagePart::Text { text, .. } => assert_eq!(text, "doomed"),
        other => panic!("unexpected part {other:?}"),
    }

    // Journal closed with a synthetic Done{interrupted}; no longer stale.
    let journal =
        RunJournal::open(dir.path().join("scopes/accounts/dev-org/dev-user/journals")).unwrap();
    assert!(journal.stale_sessions().unwrap().is_empty());
    let (_, last) = journal.last_event(CHAT).unwrap().unwrap();
    assert!(matches!(
        last,
        AgentEvent::Done {
            status: DoneStatus::Interrupted,
            ..
        }
    ));
    assert_eq!(
        core.sessions.session_status(CHAT).map(|s| s.status),
        Some(SessionStatus::Idle)
    );
}

#[tokio::test]
async fn rpc_surface_over_in_memory_transport() {
    let dir = tempfile::tempdir().unwrap();
    let core = assemble(
        dir.path(),
        Arc::new(MockHarness {
            script: mock_script(),
        }),
    );
    let client = jolt_rpc::memory_client(core.rpc_service());

    // ListHarnesses + ListModels.
    let harnesses = client
        .call(jolt_api::methods::LIST_HARNESSES, serde_json::Value::Null)
        .await
        .unwrap();
    assert_eq!(harnesses[0]["id"], "mock");
    let models = client
        .call(
            jolt_api::methods::LIST_MODELS,
            serde_json::json!({"harness": "mock"}),
        )
        .await
        .unwrap();
    assert_eq!(models[0]["id"], "mock-1");
    let commands = client
        .call(
            jolt_api::methods::LIST_COMMANDS,
            serde_json::json!({"harness": "mock", "cwd": "/tmp"}),
        )
        .await
        .unwrap();
    assert_eq!(
        commands,
        serde_json::json!([
            {
                "name": "answer",
                "description": "Answer questions from the latest assistant response",
                "source": "jolt"
            },
            {
                "name": "bro",
                "description": "Restate the latest assistant response in plain language",
                "source": "jolt"
            },
            {
                "name": "goal",
                "description": "Open the long-running goal manager",
                "argumentHint": "<objective>|pause|resume|clear",
                "source": "jolt"
            }
        ])
    );

    // Session and paged transcript streams.
    let mut sessions_stream = client
        .subscribe(jolt_api::methods::WATCH_SESSIONS, serde_json::Value::Null)
        .await
        .unwrap();
    let first_sessions = tokio::time::timeout(Duration::from_secs(5), sessions_stream.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(
        serde_json::from_value(first_sessions).unwrap(),
        jolt_api::SessionWatchFrame::Bootstrap { sessions } if sessions.is_empty()
    ));

    let mut paged_stream = client
        .subscribe(
            jolt_api::methods::WATCH_TRANSCRIPT_V2,
            serde_json::json!({"chatId": CHAT}),
        )
        .await
        .unwrap();
    let paged_initial = tokio::time::timeout(Duration::from_secs(5), paged_stream.recv())
        .await
        .unwrap()
        .unwrap();
    let paged_initial: jolt_session_doc::TranscriptWatchFrame =
        serde_json::from_value(paged_initial).unwrap();
    let jolt_session_doc::TranscriptWatchFrame::Bootstrap { bootstrap } = paged_initial else {
        panic!("paged stream must open with a bootstrap");
    };
    assert_eq!(bootstrap.manifest.total_messages, 0);
    assert!(bootstrap.pages.is_empty());

    // QueueCommand (as this device's composer would over IPC).
    let command = serde_json::to_value(SessionCommandPayload::Run {
        request: run_request("via rpc"),
        message_id: "m-rpc-1".into(),
    })
    .unwrap();
    let queued = client
        .call(
            jolt_api::methods::QUEUE_COMMAND,
            serde_json::json!({"chatId": CHAT, "command": command}),
        )
        .await
        .unwrap();
    assert!(queued["commandId"].is_string());

    // The paged stream carries structural bootstraps only when message count
    // changes, then bounded live-page deltas while the assistant streams.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let (page_id, page_revision) = loop {
        let item = tokio::time::timeout_at(deadline, paged_stream.recv())
            .await
            .expect("paged transcript before timeout")
            .expect("paged stream alive");
        match serde_json::from_value::<jolt_session_doc::TranscriptWatchFrame>(item).unwrap() {
            jolt_session_doc::TranscriptWatchFrame::Bootstrap { bootstrap } => {
                if let Some(page) = bootstrap.pages.last()
                    && page.messages.len() == 2
                    && page.messages[1].status == Some(MessageStatus::Complete)
                {
                    break (page.id.clone(), page.revision.clone());
                }
            }
            jolt_session_doc::TranscriptWatchFrame::Delta {
                page_id,
                page_revision,
                frame,
                ..
            } => {
                if let jolt_session_doc::TranscriptFrame::Delta { upsert, .. } = &frame
                    && upsert
                        .iter()
                        .any(|change| change.entry.status == Some(MessageStatus::Complete))
                {
                    break (page_id, page_revision);
                }
            }
        }
    };
    let fetched: jolt_session_doc::TranscriptPage = client
        .call_as(
            jolt_api::methods::GET_TRANSCRIPT_PAGE,
            serde_json::json!({"chatId": CHAT, "pageId": page_id}),
        )
        .await
        .unwrap();
    assert_eq!(fetched.revision, page_revision);
    assert_eq!(fetched.messages.len(), 2);
    assert_eq!(fetched.messages[0].id, "m-rpc-1");
    assert_eq!(fetched.messages[0].role, MessageRole::User);
    match &fetched.messages[1].parts[0] {
        MessagePart::Text { text, .. } => assert_eq!(text, "Hello"),
        other => panic!("unexpected part {other:?}"),
    }

    let search: Vec<jolt_session_doc::TranscriptSearchResult> = client
        .call_as(
            jolt_api::methods::SEARCH_TRANSCRIPT,
            serde_json::json!({"chatId": CHAT, "query": "via RPC"}),
        )
        .await
        .unwrap();
    assert_eq!(search.len(), 1);
    assert_eq!(search[0].message_id, "m-rpc-1");
    assert_eq!(search[0].page_id, fetched.id);

    // WatchSessions eventually reports the settled Idle session.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let item = tokio::time::timeout_at(deadline, sessions_stream.recv())
            .await
            .expect("session update before timeout")
            .expect("stream alive");
        let frame: jolt_api::SessionWatchFrame = serde_json::from_value(item).unwrap();
        let sessions = match frame {
            jolt_api::SessionWatchFrame::Bootstrap { sessions } => sessions,
            jolt_api::SessionWatchFrame::Delta { upserts, .. } => upserts,
        };
        if sessions
            .iter()
            .any(|session| session.status == SessionStatus::Idle)
        {
            break;
        }
    }
}

#[tokio::test]
async fn respond_input_resolves_pending_question() {
    // Harness that asks a question through RunControls and echoes the answer.
    struct AskingHarness;
    #[async_trait]
    impl Harness for AskingHarness {
        fn id(&self) -> HarnessId {
            HarnessId::Mock
        }
        fn display_name(&self) -> &str {
            "Asking"
        }
        fn supports_steering(&self) -> bool {
            false
        }
        fn steering_mode(&self) -> SteeringMode {
            SteeringMode::TurnBoundary
        }
        fn reasoning_levels(&self) -> &[ReasoningLevel] {
            &[]
        }
        async fn models(&self) -> Result<Vec<Model>, HarnessError> {
            Ok(vec![])
        }
        async fn run(
            &self,
            _request: RunRequest,
            controls: RunControls,
        ) -> Result<BoxStream<'static, Result<AgentEvent, HarnessError>>, HarnessError> {
            let (tx, rx) = tokio::sync::mpsc::channel::<Result<AgentEvent, HarnessError>>(16);
            tokio::spawn(async move {
                let answers = (controls.request_input)(vec![jolt_proto::UserInputQuestion {
                    id: "q1".into(),
                    header: "Pick".into(),
                    question: "Which one?".into(),
                    options: vec!["a".into(), "b".into()],
                    multi_select: false,
                }])
                .await
                .unwrap_or_default();
                let picked = answers
                    .first()
                    .and_then(|a| a.labels.first().cloned())
                    .unwrap_or_else(|| "none".into());
                let _ = tx
                    .send(Ok(AgentEvent::TextDelta {
                        text: format!("picked {picked}"),
                    }))
                    .await;
                let _ = tx.send(Ok(done(DoneStatus::Completed))).await;
            });
            Ok(futures::stream::unfold(rx, |mut rx| async move {
                rx.recv().await.map(|event| (event, rx))
            })
            .boxed())
        }
    }

    let dir = tempfile::tempdir().unwrap();
    let core = assemble(dir.path(), Arc::new(AskingHarness));
    let handle = core.doc_host.open(CHAT).unwrap();
    queue_as_viewer(
        handle.doc(),
        "cmd-run-ask",
        SessionCommandPayload::Run {
            request: run_request("ask me"),
            message_id: "m-1".into(),
        },
    );

    // The input request surfaces: status AwaitingInput + an unresolved input part.
    wait_for(
        || {
            core.sessions.session_status(CHAT).map(|s| s.status)
                == Some(SessionStatus::AwaitingInput)
        },
        "awaiting input",
    )
    .await;
    wait_for(
        || {
            entries(&core).iter().any(|e| {
                e.parts.iter().any(|p| {
                    matches!(
                        p,
                        MessagePart::Input {
                            resolved: false,
                            ..
                        }
                    )
                })
            })
        },
        "input part in doc",
    )
    .await;

    // A viewer answers through the durable command queue.
    let request_id = entries(&core)
        .iter()
        .find_map(|e| {
            e.parts.iter().find_map(|p| match p {
                MessagePart::Input { request_id, .. } => Some(request_id.clone()),
                _ => None,
            })
        })
        .unwrap();
    queue_as_viewer(
        handle.doc(),
        "cmd-answer-1",
        SessionCommandPayload::RespondInput {
            request_id,
            answers: vec![jolt_proto::UserInputAnswer {
                question_id: "q1".into(),
                labels: vec!["b".into()],
            }],
        },
    );

    wait_for(
        || {
            entries_now(&core).iter().any(|e| {
                e.status == Some(MessageStatus::Complete)
                    && e.parts
                        .iter()
                        .any(|p| matches!(p, MessagePart::Text { text, .. } if text == "picked b"))
            })
        },
        "answered turn to complete",
    )
    .await;
    assert_eq!(
        command_status(&core, "cmd-answer-1"),
        Some((SessionCommandStatus::Applied, None))
    );
    // The input part is marked resolved in the doc.
    assert!(entries(&core).iter().any(|e| {
        e.parts
            .iter()
            .any(|p| matches!(p, MessagePart::Input { resolved: true, .. }))
    }));
    // The run task writes the Complete entry BEFORE settling the status row —
    // wait for the transition instead of asserting the instant in between.
    wait_for(
        || core.sessions.session_status(CHAT).map(|s| s.status) == Some(SessionStatus::Idle),
        "session to settle idle",
    )
    .await;
}

/// Resilience: a RespondInput whose id matches no pending request is REJECTED
/// with a resolution (never silently dropped), the question stays live (the
/// panel persists), and a subsequent correct answer still resumes the run —
/// a wrong answer can never brick the session.
#[tokio::test(flavor = "multi_thread")]
async fn wrong_id_respond_is_rejected_and_correct_answer_still_resumes() {
    struct AskingHarness;
    #[async_trait]
    impl Harness for AskingHarness {
        fn id(&self) -> HarnessId {
            HarnessId::Mock
        }
        fn display_name(&self) -> &str {
            "Asking"
        }
        fn supports_steering(&self) -> bool {
            false
        }
        fn steering_mode(&self) -> SteeringMode {
            SteeringMode::TurnBoundary
        }
        fn reasoning_levels(&self) -> &[ReasoningLevel] {
            &[]
        }
        async fn models(&self) -> Result<Vec<Model>, HarnessError> {
            Ok(vec![])
        }
        async fn run(
            &self,
            _request: RunRequest,
            controls: RunControls,
        ) -> Result<BoxStream<'static, Result<AgentEvent, HarnessError>>, HarnessError> {
            let (tx, rx) = tokio::sync::mpsc::channel::<Result<AgentEvent, HarnessError>>(16);
            tokio::spawn(async move {
                let answers = (controls.request_input)(vec![jolt_proto::UserInputQuestion {
                    id: "q1".into(),
                    header: "Pick".into(),
                    question: "Which one?".into(),
                    options: vec!["a".into(), "b".into()],
                    multi_select: false,
                }])
                .await
                .unwrap_or_default();
                let picked = answers
                    .first()
                    .and_then(|a| a.labels.first().cloned())
                    .unwrap_or_else(|| "none".into());
                let _ = tx
                    .send(Ok(AgentEvent::TextDelta {
                        text: format!("picked {picked}"),
                    }))
                    .await;
                let _ = tx.send(Ok(done(DoneStatus::Completed))).await;
            });
            Ok(futures::stream::unfold(rx, |mut rx| async move {
                rx.recv().await.map(|event| (event, rx))
            })
            .boxed())
        }
    }

    let dir = tempfile::tempdir().unwrap();
    let core = assemble(dir.path(), Arc::new(AskingHarness));
    let handle = core.doc_host.open(CHAT).unwrap();
    queue_as_viewer(
        handle.doc(),
        "cmd-run-wrong",
        SessionCommandPayload::Run {
            request: run_request("ask me"),
            message_id: "m-1".into(),
        },
    );
    wait_for(
        || {
            core.sessions.session_status(CHAT).map(|s| s.status)
                == Some(SessionStatus::AwaitingInput)
        },
        "awaiting input",
    )
    .await;
    wait_for(
        || {
            entries_now(&core).iter().any(|e| {
                e.parts.iter().any(|p| {
                    matches!(
                        p,
                        MessagePart::Input {
                            resolved: false,
                            ..
                        }
                    )
                })
            })
        },
        "input part in doc",
    )
    .await;

    // A wrong-id answer: rejected with a resolution, question still live.
    queue_as_viewer(
        handle.doc(),
        "cmd-answer-bogus",
        SessionCommandPayload::RespondInput {
            request_id: "bogus-id".into(),
            answers: vec![jolt_proto::UserInputAnswer {
                question_id: "q1".into(),
                labels: vec!["a".into()],
            }],
        },
    );
    wait_for(
        || {
            command_status(&core, "cmd-answer-bogus")
                .is_some_and(|(s, _)| s != SessionCommandStatus::Pending)
        },
        "bogus answer processed",
    )
    .await;
    assert_eq!(
        command_status(&core, "cmd-answer-bogus"),
        Some((
            SessionCommandStatus::Rejected,
            Some("no pending input request".into())
        ))
    );
    // The run is still waiting and the part is still unresolved — the
    // QuestionPanel keeps presenting the real request.
    assert_eq!(
        core.sessions.session_status(CHAT).map(|s| s.status),
        Some(SessionStatus::AwaitingInput)
    );
    let request_id = entries(&core)
        .iter()
        .find_map(|e| {
            e.parts.iter().find_map(|p| match p {
                MessagePart::Input {
                    request_id,
                    resolved: false,
                    ..
                } => Some(request_id.clone()),
                _ => None,
            })
        })
        .expect("question still live after rejected answer");

    // The correct answer still resumes and completes the run.
    queue_as_viewer(
        handle.doc(),
        "cmd-answer-right",
        SessionCommandPayload::RespondInput {
            request_id,
            answers: vec![jolt_proto::UserInputAnswer {
                question_id: "q1".into(),
                labels: vec!["b".into()],
            }],
        },
    );
    wait_for(
        || {
            entries_now(&core).iter().any(|e| {
                e.status == Some(MessageStatus::Complete)
                    && e.parts
                        .iter()
                        .any(|p| matches!(p, MessagePart::Text { text, .. } if text == "picked b"))
            })
        },
        "answered turn to complete",
    )
    .await;
    assert_eq!(
        core.sessions.session_status(CHAT).map(|s| s.status),
        Some(SessionStatus::Idle)
    );
}

/// Resilience: interrupting a run that is BLOCKED on a question unparks the
/// harness immediately (the pending resolver is failed with empty answers),
/// the entry settles `aborted`, the chip flips terminal (never dangles
/// unresolved), and the next run works — a blocked question can never brick
/// the session.
#[tokio::test(flavor = "multi_thread")]
async fn interrupt_unblocks_a_run_awaiting_input() {
    struct BlockingHarness;
    #[async_trait]
    impl Harness for BlockingHarness {
        fn id(&self) -> HarnessId {
            HarnessId::Mock
        }
        fn display_name(&self) -> &str {
            "Blocking"
        }
        fn supports_steering(&self) -> bool {
            false
        }
        fn steering_mode(&self) -> SteeringMode {
            SteeringMode::TurnBoundary
        }
        fn reasoning_levels(&self) -> &[ReasoningLevel] {
            &[]
        }
        async fn models(&self) -> Result<Vec<Model>, HarnessError> {
            Ok(vec![])
        }
        async fn run(
            &self,
            request: RunRequest,
            controls: RunControls,
        ) -> Result<BoxStream<'static, Result<AgentEvent, HarnessError>>, HarnessError> {
            let (tx, rx) = tokio::sync::mpsc::channel::<Result<AgentEvent, HarnessError>>(16);
            if request.prompt.ends_with("second run") {
                // The post-interrupt turn: completes immediately.
                tokio::spawn(async move {
                    let _ = tx
                        .send(Ok(AgentEvent::TextDelta {
                            text: "second done".into(),
                        }))
                        .await;
                    let _ = tx.send(Ok(done(DoneStatus::Completed))).await;
                });
            } else {
                let interrupt = controls.interrupt.clone();
                tokio::spawn(async move {
                    // Blocks on the question; an interrupt fails the resolver
                    // (empty answers) and cancels the token — like a real CLI
                    // being torn down, the stream then ends WITHOUT a Done.
                    let _ = (controls.request_input)(vec![jolt_proto::UserInputQuestion {
                        id: "q1".into(),
                        header: "Pick".into(),
                        question: "Which one?".into(),
                        options: vec!["a".into(), "b".into()],
                        multi_select: false,
                    }])
                    .await;
                    interrupt.cancelled().await;
                    drop(tx);
                });
            }
            Ok(futures::stream::unfold(rx, |mut rx| async move {
                rx.recv().await.map(|event| (event, rx))
            })
            .boxed())
        }
    }

    let dir = tempfile::tempdir().unwrap();
    let core = assemble(dir.path(), Arc::new(BlockingHarness));
    let handle = core.doc_host.open(CHAT).unwrap();
    queue_as_viewer(
        handle.doc(),
        "cmd-run-block",
        SessionCommandPayload::Run {
            request: run_request("ask and block"),
            message_id: "m-1".into(),
        },
    );
    wait_for(
        || {
            core.sessions.session_status(CHAT).map(|s| s.status)
                == Some(SessionStatus::AwaitingInput)
        },
        "awaiting input",
    )
    .await;
    wait_for(
        || {
            entries_now(&core).iter().any(|e| {
                e.parts.iter().any(|p| {
                    matches!(
                        p,
                        MessagePart::Input {
                            resolved: false,
                            ..
                        }
                    )
                })
            })
        },
        "input part in doc",
    )
    .await;

    // Interrupt while blocked: settles promptly (well under the 3s grace —
    // the unparked resolver lets the harness wind down on its own).
    let start = std::time::Instant::now();
    core.sessions.interrupt(CHAT).await.unwrap();
    assert!(
        start.elapsed() < std::time::Duration::from_secs(3),
        "interrupt settled via the unparked resolver, not the grace timeout"
    );
    wait_for(
        || {
            entries_now(&core)
                .iter()
                .any(|e| e.status == Some(MessageStatus::Aborted))
        },
        "entry stamped aborted",
    )
    .await;
    // The chip is terminal — no dangling unresolved question survives the run.
    assert!(entries(&core).iter().all(|e| {
        e.parts.iter().all(|p| {
            !matches!(
                p,
                MessagePart::Input {
                    resolved: false,
                    ..
                }
            )
        })
    }));

    // And the session is usable: the next run completes.
    queue_as_viewer(
        handle.doc(),
        "cmd-run-second",
        SessionCommandPayload::Run {
            request: run_request("second run"),
            message_id: "m-2".into(),
        },
    );
    wait_for(
        || {
            entries_now(&core).iter().any(|e| {
                e.status == Some(MessageStatus::Complete)
                    && e.parts.iter().any(
                        |p| matches!(p, MessagePart::Text { text, .. } if text == "second done"),
                    )
            })
        },
        "second run to complete",
    )
    .await;
}

/// Regression (the "nothing happened after I answered" bug): a harness that
/// emits its OWN `InputRequested` (keyed by its internal id — Claude's
/// control-request id) *and* asks through `RunControls::request_input` used to
/// fold TWO input parts into the doc. The UI answers the LAST unresolved part;
/// the harness-emitted twin's id was unknown to `respond_input`'s pending map,
/// so the RespondInput doc command was rejected and the run never resumed.
/// The engine now drops harness-emitted `InputRequested` events (the input
/// bridge is the sole authority), so exactly one — answerable — part folds.
#[tokio::test(flavor = "multi_thread")]
async fn harness_emitted_input_twin_is_dropped_and_answer_resumes() {
    struct DoubleEmitHarness;
    #[async_trait]
    impl Harness for DoubleEmitHarness {
        fn id(&self) -> HarnessId {
            HarnessId::Mock
        }
        fn display_name(&self) -> &str {
            "DoubleEmit"
        }
        fn supports_steering(&self) -> bool {
            false
        }
        fn steering_mode(&self) -> SteeringMode {
            SteeringMode::TurnBoundary
        }
        fn reasoning_levels(&self) -> &[ReasoningLevel] {
            &[]
        }
        async fn models(&self) -> Result<Vec<Model>, HarnessError> {
            Ok(vec![])
        }
        async fn run(
            &self,
            _request: RunRequest,
            controls: RunControls,
        ) -> Result<BoxStream<'static, Result<AgentEvent, HarnessError>>, HarnessError> {
            let (tx, rx) = tokio::sync::mpsc::channel::<Result<AgentEvent, HarnessError>>(16);
            tokio::spawn(async move {
                let question = jolt_proto::UserInputQuestion {
                    id: "q1".into(),
                    header: "Pick".into(),
                    question: "Which one?".into(),
                    options: vec!["a".into(), "b".into()],
                    multi_select: false,
                };
                // The pre-fix Claude/Codex shape: surface the question under
                // the harness's own id BEFORE asking through the bridge.
                let _ = tx
                    .send(Ok(AgentEvent::InputRequested {
                        request_id: "claude-ctrl-1".into(),
                        questions: vec![question.clone()],
                    }))
                    .await;
                let answers = (controls.request_input)(vec![question])
                    .await
                    .unwrap_or_default();
                let picked = answers
                    .first()
                    .and_then(|a| a.labels.first().cloned())
                    .unwrap_or_else(|| "none".into());
                let _ = tx
                    .send(Ok(AgentEvent::TextDelta {
                        text: format!("picked {picked}"),
                    }))
                    .await;
                let _ = tx.send(Ok(done(DoneStatus::Completed))).await;
            });
            Ok(futures::stream::unfold(rx, |mut rx| async move {
                rx.recv().await.map(|event| (event, rx))
            })
            .boxed())
        }
    }

    let dir = tempfile::tempdir().unwrap();
    let core = assemble(dir.path(), Arc::new(DoubleEmitHarness));
    let handle = core.doc_host.open(CHAT).unwrap();
    queue_as_viewer(
        handle.doc(),
        "cmd-run-twin",
        SessionCommandPayload::Run {
            request: run_request("ask me twice"),
            message_id: "m-1".into(),
        },
    );

    wait_for(
        || {
            core.sessions.session_status(CHAT).map(|s| s.status)
                == Some(SessionStatus::AwaitingInput)
        },
        "awaiting input",
    )
    .await;
    wait_for(
        || {
            entries_now(&core).iter().any(|e| {
                e.parts.iter().any(|p| {
                    matches!(
                        p,
                        MessagePart::Input {
                            resolved: false,
                            ..
                        }
                    )
                })
            })
        },
        "input part in doc",
    )
    .await;

    // Exactly ONE input part folded, and not under the harness's own id.
    let input_ids: Vec<String> = entries(&core)
        .iter()
        .flat_map(|e| {
            e.parts.iter().filter_map(|p| match p {
                MessagePart::Input { request_id, .. } => Some(request_id.clone()),
                _ => None,
            })
        })
        .collect();
    assert_eq!(input_ids.len(), 1, "one chip, not a twin: {input_ids:?}");
    assert_ne!(input_ids[0], "claude-ctrl-1");

    // Answer the LAST unresolved part — exactly what the QuestionPanel does.
    let request_id = entries(&core)
        .iter()
        .rev()
        .find_map(|e| {
            e.parts.iter().rev().find_map(|p| match p {
                MessagePart::Input {
                    request_id,
                    resolved: false,
                    ..
                } => Some(request_id.clone()),
                _ => None,
            })
        })
        .unwrap();
    queue_as_viewer(
        handle.doc(),
        "cmd-answer-twin",
        SessionCommandPayload::RespondInput {
            request_id,
            answers: vec![jolt_proto::UserInputAnswer {
                question_id: "q1".into(),
                labels: vec!["a".into()],
            }],
        },
    );

    // The run resumes and completes; the chip flips to resolved.
    wait_for(
        || {
            entries_now(&core).iter().any(|e| {
                e.status == Some(MessageStatus::Complete)
                    && e.parts
                        .iter()
                        .any(|p| matches!(p, MessagePart::Text { text, .. } if text == "picked a"))
            })
        },
        "answered turn to complete",
    )
    .await;
    assert_eq!(
        command_status(&core, "cmd-answer-twin"),
        Some((SessionCommandStatus::Applied, None))
    );
    assert!(entries(&core).iter().any(|e| {
        e.parts
            .iter()
            .any(|p| matches!(p, MessagePart::Input { resolved: true, .. }))
    }));
    // The run task writes the Complete entry BEFORE settling the status row —
    // wait for the transition instead of asserting the instant in between.
    wait_for(
        || core.sessions.session_status(CHAT).map(|s| s.status) == Some(SessionStatus::Idle),
        "session to settle idle",
    )
    .await;
}

// ---------------------------------------------------------------------------
// Attachments (round 17): chunked upload → durable path → Run carrying both
// the prompt-embedded refs (the persisted transport) and the staged paths.
// ---------------------------------------------------------------------------

/// Delegates to a scripted mock but records every RunRequest the engine hands
/// over (the chat run AND the auto-title run share the harness) — proves
/// `attachments` survives doc-queue → executor → harness.
struct CapturingHarness {
    script: Vec<AgentEvent>,
    seen: Arc<std::sync::Mutex<Vec<RunRequest>>>,
}

#[async_trait]
impl Harness for CapturingHarness {
    fn id(&self) -> HarnessId {
        HarnessId::Mock
    }
    fn display_name(&self) -> &str {
        "Capturing"
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
        request: RunRequest,
        controls: RunControls,
    ) -> Result<BoxStream<'static, Result<AgentEvent, HarnessError>>, HarnessError> {
        self.seen.lock().unwrap().push(request.clone());
        MockHarness {
            script: self.script.clone(),
        }
        .run(request, controls)
        .await
    }
}

#[tokio::test]
async fn attachment_upload_then_run_threads_refs_and_paths() {
    use base64::Engine as _;
    let b64 = base64::engine::general_purpose::STANDARD;
    let dir = tempfile::tempdir().unwrap();
    let seen: Arc<std::sync::Mutex<Vec<RunRequest>>> = Default::default();
    let core = assemble(
        dir.path(),
        Arc::new(CapturingHarness {
            script: mock_script(),
            seen: seen.clone(),
        }),
    );
    let client = jolt_rpc::memory_client(core.rpc_service());

    // Chunked upload exactly as the composer sends it: base64 split across
    // positional UploadChunk slots, then UploadCommit → the durable path.
    let payload: Vec<u8> = (0..=255u8).cycle().take(9_001).collect();
    let encoded = b64.encode(&payload);
    let (first, second) = encoded.split_at(encoded.len() / 2);
    for (seq, data) in [(0, first), (1, second)] {
        client
            .call(
                jolt_api::methods::UPLOAD_CHUNK,
                serde_json::json!({ "uploadId": "e2e-att", "seq": seq, "data": data }),
            )
            .await
            .expect("UploadChunk");
    }
    let committed = client
        .call(
            jolt_api::methods::UPLOAD_COMMIT,
            serde_json::json!({ "uploadId": "e2e-att", "fileName": "red.png", "chatId": CHAT }),
        )
        .await
        .expect("UploadCommit");
    let path = committed["path"].as_str().expect("path").to_string();
    assert_eq!(committed["sha256"].as_str().expect("sha256").len(), 64);
    assert_eq!(
        std::fs::read(&path).expect("durable upload file"),
        payload,
        "committed file holds the exact reassembled bytes"
    );

    // Run with attachment refs embedded in the persistent prompt text and
    // paths on the additive field.
    let prompt = format!(
        "what color is this?\n\nAttached images (local files — open them to view):\n- {path}"
    );
    let mut request = run_request(&prompt);
    request.attachments = vec![path.clone()];
    let handle = core.doc_host.open(CHAT).unwrap();
    queue_as_viewer(
        handle.doc(),
        "cmd-att-1",
        SessionCommandPayload::Run {
            request,
            message_id: "msg-att-1".into(),
        },
    );
    wait_for(
        || {
            entries_now(&core).iter().any(|e| {
                e.role == MessageRole::Assistant && e.status == Some(MessageStatus::Complete)
            })
        },
        "assistant entry to complete",
    )
    .await;

    // Doc user entry: the message text carries the refs verbatim (render-back
    // parses them into thumbnails).
    let all = entries(&core);
    assert_eq!(all[0].id, "msg-att-1");
    assert_eq!(all[0].role, MessageRole::User);
    match &all[0].parts[0] {
        MessagePart::Text { text, .. } => {
            assert!(text.contains("Attached images (local files"));
            assert!(text.contains(&path));
        }
        other => panic!("unexpected user part {other:?}"),
    }

    // The harness saw the staged paths on the request itself (the chat run —
    // NOT the auto-title run, which fires at dispatch now, embeds the user
    // prompt in its wrapper, and legitimately carries no attachments).
    let requests = seen.lock().unwrap().clone();
    let chat_run = requests
        .iter()
        .find(|r| r.prompt.contains("what color is this?") && !r.prompt.contains("word title"))
        .expect("chat run reached the harness");
    assert_eq!(chat_run.attachments, vec![path.clone()]);
    assert!(chat_run.prompt.contains(&path));

    // Read-back over the same RPC surface the transcript uses.
    let chunk = client
        .call(
            jolt_api::methods::READ_ATTACHMENT_CHUNK,
            serde_json::json!({ "path": path, "offset": 0 }),
        )
        .await
        .expect("ReadAttachmentChunk");
    assert_eq!(chunk["mimeType"], "image/png");
    assert_eq!(chunk["name"], "e2e-att-red.png");
}

/// Real-CLI proof of the image pipeline: upload a tiny solid-red PNG through
/// the chunked RPC path, run claude (haiku) with the staged path on
/// `attachments` + the refs in the prompt, and check the reply names the
/// color — it can only know it by SEEING the inline image block (the sandbox
/// prompt forbids opening the file). Ignored by default: needs an installed,
/// authenticated `claude` CLI and spends real tokens.
/// Run with: `cargo test -p jolt-engine --test e2e -- --ignored`
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires installed+authenticated claude CLI; spends tokens"]
async fn real_claude_sees_uploaded_image_inline() {
    use base64::Engine as _;
    let b64 = base64::engine::general_purpose::STANDARD;
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("data");
    let cwd = tmp.path().join("project");
    std::fs::create_dir_all(&cwd).unwrap();

    let core = EngineCore::assemble(
        &dir,
        Arc::new(jolt_engine::default_registry()),
        HarnessId::ClaudeCode,
        None,
    )
    .expect("engine core assembles");
    // Pre-title the chat so the auto-titler doesn't spend a second model call.
    core.workspace
        .create_chat(CHAT, &core.device_id, None, Some("/tmp".into()))
        .expect("create chat row");
    core.workspace
        .rename_chat(CHAT, "Pre-titled")
        .expect("rename chat");

    // 8×8 solid-red PNG, uploaded exactly as the composer does.
    const RED_PNG_B64: &str = "iVBORw0KGgoAAAANSUhEUgAAAAgAAAAICAIAAABLbSncAAAAEklEQVR4nGP4z8CAB+GTG2wAAJP0GeGuMDBnAAAAAElFTkSuQmCC";
    let client = jolt_rpc::memory_client(core.rpc_service());
    client
        .call(
            jolt_api::methods::UPLOAD_CHUNK,
            serde_json::json!({ "uploadId": "real-img", "seq": 0, "data": RED_PNG_B64 }),
        )
        .await
        .expect("UploadChunk");
    let committed = client
        .call(
            jolt_api::methods::UPLOAD_COMMIT,
            serde_json::json!({ "uploadId": "real-img", "fileName": "swatch.png", "chatId": CHAT }),
        )
        .await
        .expect("UploadCommit");
    let path = committed["path"].as_str().expect("path").to_string();
    assert_eq!(
        std::fs::read(&path).expect("committed file"),
        b64.decode(RED_PNG_B64).unwrap()
    );

    let prompt = format!(
        "Without running any tools or opening any files, answer from the attached image alone: \
         what solid color is this image? Reply with exactly one lowercase word.\n\n\
         Attached images (local files — open them to view):\n- {path}"
    );
    let request = RunRequest {
        prompt,
        harness: None,
        model: Some("haiku".into()),
        reasoning: None,
        model_options: Default::default(),
        cwd: cwd.to_string_lossy().to_string(),
        sandbox: SandboxLevel::WorkspaceWrite,
        auto_approve: false,
        attachments: vec![path],
        resume: None,
    };
    core.doc_host
        .queue_command(
            CHAT,
            SessionCommandPayload::Run {
                request,
                message_id: "msg-img-1".into(),
            },
        )
        .expect("queue real image run");
    wait_for_within_secs(
        || {
            entries_now(&core).iter().any(|e| {
                e.role == MessageRole::Assistant && e.status == Some(MessageStatus::Complete)
            })
        },
        "real claude image turn",
        120,
    )
    .await;

    let reply: String = entries(&core)
        .iter()
        .filter(|e| e.role == MessageRole::Assistant)
        .flat_map(|e| e.parts.iter())
        .filter_map(|p| match p {
            MessagePart::Text { text, .. } => Some(text.clone()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase();
    assert!(
        reply.contains("red"),
        "claude should name the image's color; got: {reply:?}"
    );
    core.shutdown().await;
}

async fn wait_for_within_secs<F>(mut predicate: F, what: &str, secs: u64)
where
    F: FnMut() -> bool,
{
    let deadline = tokio::time::Instant::now() + Duration::from_secs(secs);
    while !predicate() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for {what}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

// ---------------------------------------------------------------------------
// Liveness heartbeats: empty reasoning deltas keep the session fresh but
// never reach the journal (redacted thinking + tool-input-generation noise).
// ---------------------------------------------------------------------------

#[tokio::test]
async fn empty_reasoning_deltas_are_heartbeats_not_journal_noise() {
    let mut script = vec![AgentEvent::SessionStarted {
        harness: HarnessId::Mock,
        model: "mock-1".into(),
        tools: vec![],
        cwd: "/tmp".into(),
        session_id: "hs-hb".into(),
        assistant_message_id: "a-hb".into(),
    }];
    // A long "silent" stretch: redacted thinking / input_json_delta windows
    // stream as empty reasoning deltas.
    for _ in 0..40 {
        script.push(AgentEvent::ReasoningDelta {
            text: String::new(),
        });
    }
    script.push(AgentEvent::ReasoningDelta {
        text: "planning".into(),
    });
    script.push(AgentEvent::TextDelta {
        text: "done".into(),
    });
    script.push(AgentEvent::Done {
        status: DoneStatus::Completed,
        result: Some("done".into()),
        error: None,
        session_id: None,
    });
    let dir = tempfile::tempdir().unwrap();
    let core = assemble(dir.path(), Arc::new(MockHarness { script }));
    let handle = core.doc_host.open(CHAT).unwrap();
    queue_as_viewer(
        handle.doc(),
        "cmd-hb-1",
        SessionCommandPayload::Run {
            request: run_request("hb"),
            message_id: "msg-hb-1".into(),
        },
    );
    wait_for(
        || {
            entries(&core).iter().any(|entry| {
                entry.role == MessageRole::Assistant
                    && entry.status == Some(MessageStatus::Complete)
            })
        },
        "run completes",
    )
    .await;
    // Journal replay: the 40 empties were filtered; real content survived.
    let replay = core.sessions.subscribe(CHAT, 0).unwrap().0;
    let empties = replay
        .iter()
        .filter(|j| matches!(&j.event, AgentEvent::ReasoningDelta { text } if text.is_empty()))
        .count();
    let nonempty = replay
        .iter()
        .filter(|j| matches!(&j.event, AgentEvent::ReasoningDelta { text } if !text.is_empty()))
        .count();
    assert_eq!(empties, 0, "empty reasoning deltas never reach the journal");
    assert_eq!(nonempty, 1, "real reasoning text is preserved");
    assert!(
        replay
            .iter()
            .any(|j| matches!(&j.event, AgentEvent::TextDelta { text } if text == "done")),
        "text deltas unaffected"
    );
}

#[tokio::test]
async fn parked_session_ignores_trailing_frames_and_stays_idle() {
    let mut script = mock_script();
    script.push(AgentEvent::ToolCall {
        id: "tool-1".into(),
        call: ToolCall::Exec {
            command: "echo late-echo".into(),
        },
    });
    script.push(AgentEvent::TextDelta {
        text: "trailing flush".into(),
    });
    let dir = tempfile::tempdir().unwrap();
    let core = assemble(dir.path(), Arc::new(MockHarness { script }));
    let handle = core.doc_host.open(CHAT).unwrap();
    queue_as_viewer(
        handle.doc(),
        "cmd-run-parked",
        SessionCommandPayload::Run {
            request: run_request("go"),
            message_id: "m-parked".into(),
        },
    );

    wait_for(
        || core.sessions.session_status(CHAT).map(|s| s.status) == Some(SessionStatus::Idle),
        "session to complete",
    )
    .await;
    let deadline = tokio::time::Instant::now() + Duration::from_millis(600);
    while tokio::time::Instant::now() < deadline {
        assert_eq!(
            core.sessions.session_status(CHAT).map(|s| s.status),
            Some(SessionStatus::Idle),
            "trailing frames must not re-arm Working"
        );
        assert!(
            entries_now(&core).len() <= 2,
            "trailing frames must not open a phantom entry"
        );
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    let all = entries(&core);
    assert_eq!(all.len(), 2, "user + one assistant entry");
    assert_eq!(all[1].status, Some(MessageStatus::Complete));
}

#[tokio::test]
async fn stale_tool_echo_after_steer_boundary_does_not_split_text() {
    let script = vec![
        AgentEvent::SessionStarted {
            harness: HarnessId::Mock,
            model: "mock-1".into(),
            tools: vec![],
            cwd: "/tmp".into(),
            session_id: "hs-steer".into(),
            assistant_message_id: "a-1".into(),
        },
        AgentEvent::TextDelta {
            text: "part one".into(),
        },
        AgentEvent::ToolCall {
            id: "tool-long".into(),
            call: ToolCall::Exec {
                command: "sleep 60".into(),
            },
        },
        AgentEvent::Steered {
            assistant_message_id: Some("a-1".into()),
            next_assistant_message_id: Some("a-2".into()),
        },
        AgentEvent::TextDelta {
            text: "part ".into(),
        },
        AgentEvent::ToolCall {
            id: "tool-long".into(),
            call: ToolCall::Exec {
                command: "sleep 60".into(),
            },
        },
        AgentEvent::ToolResult {
            id: "tool-long".into(),
            is_error: false,
        },
        AgentEvent::TextDelta { text: "two".into() },
        done(DoneStatus::Completed),
    ];
    let dir = tempfile::tempdir().unwrap();
    let core = assemble(dir.path(), Arc::new(MockHarness { script }));
    let handle = core.doc_host.open(CHAT).unwrap();
    queue_as_viewer(
        handle.doc(),
        "cmd-run-echo",
        SessionCommandPayload::Run {
            request: run_request("go"),
            message_id: "m-echo".into(),
        },
    );

    wait_for(
        || {
            entries_now(&core).len() == 3
                && core.sessions.session_status(CHAT).map(|s| s.status) == Some(SessionStatus::Idle)
        },
        "both segments to land",
    )
    .await;
    let all = entries(&core);
    assert!(
        all[1]
            .parts
            .iter()
            .any(|part| matches!(part, MessagePart::Tool { id, .. } if id == "tool-long")),
        "first segment keeps its tool: {:#?}",
        all[1].parts
    );
    let text_parts: Vec<_> = all[2]
        .parts
        .iter()
        .filter_map(|part| match part {
            MessagePart::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        text_parts,
        vec!["part two"],
        "stale echo must not split the streaming text"
    );
    assert!(
        !all[2]
            .parts
            .iter()
            .any(|part| matches!(part, MessagePart::Tool { .. })),
        "stale echo must not create a tool part: {:#?}",
        all[2].parts
    );
}
