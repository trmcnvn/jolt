//! Shared session state bookkeeping.

use super::*;

impl Inner {
    /// Journal + broadcast one event (the two unconditional legs of the pipeline).
    pub(super) fn publish(&self, chat_id: &str, event: &AgentEvent) -> u64 {
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
                let mut contexts = lock(&self.usage_contexts);
                let service_tier = contexts
                    .get(chat_id)
                    .and_then(|context| context.service_tier.clone());
                contexts.insert(
                    chat_id.to_string(),
                    UsageContext {
                        harness: *harness,
                        model: model.clone(),
                        cwd: cwd.clone(),
                        service_tier,
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
    pub(super) fn touch_session(&self, chat_id: &str) {
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

    pub(super) fn set_compacting(&self, chat_id: &str, compacting: bool) {
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

    pub(super) fn set_status(&self, chat_id: &str, status: SessionStatus, fresh_start: bool) {
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

    pub(super) fn workspace(&self) -> Option<&crate::workspace_host::WorkspaceHost> {
        self.doc_host.get().and_then(|host| host.workspace())
    }

    /// Sidebar freshness: push a message-persist preview into the chat's workspace row.
    pub(super) fn note_message(&self, chat_id: &str, text: &str) {
        if text.is_empty() {
            return;
        }
        if let Some(ws) = self.workspace() {
            ws.note_message(chat_id, text);
        }
    }

    pub(super) fn session_key(chat_id: &str, harness: HarnessId, cwd: &str) -> HarnessSessionKey {
        (chat_id.to_string(), harness, cwd.to_string())
    }

    /// Resolve one native continuation without ever falling through to a
    /// different harness or cwd.
    pub(super) fn conversation_for(
        &self,
        chat_id: &str,
        harness: HarnessId,
        cwd: &str,
    ) -> Option<jolt_proto::HarnessConversationRef> {
        let key = Self::session_key(chat_id, harness, cwd);
        if let Some(conversation) = lock(&self.harness_sessions).get(&key).cloned() {
            return Some(conversation);
        }
        if let Some(conversation) = self
            .workspace()
            .and_then(|workspace| workspace.chat_harness_conversation(chat_id, harness, cwd))
        {
            lock(&self.harness_sessions).insert(key, conversation.clone());
            return Some(conversation);
        }
        let session_id = self.journal_harness_session_for(chat_id, harness, cwd)?;
        let conversation = jolt_proto::HarnessConversationRef {
            id: new_id(),
            harness,
            device_id: self.device_id.clone(),
            cwd: cwd.to_string(),
            native_session_id: session_id,
            generation: 0,
            covered_through_message_id: None,
        };
        lock(&self.harness_sessions).insert(key, conversation.clone());
        if let Some(workspace) = self.workspace() {
            workspace.set_chat_harness_conversation(chat_id, &conversation);
        }
        Some(conversation)
    }

    pub(super) fn remember_harness_session(
        &self,
        chat_id: &str,
        harness: HarnessId,
        session_id: &str,
        cwd: &str,
    ) {
        if session_id.is_empty() {
            return;
        }
        let key = Self::session_key(chat_id, harness, cwd);
        let mut conversation = self
            .conversation_for(chat_id, harness, cwd)
            .unwrap_or_else(|| jolt_proto::HarnessConversationRef {
                id: new_id(),
                harness,
                device_id: self.device_id.clone(),
                cwd: cwd.to_string(),
                native_session_id: String::new(),
                generation: 0,
                covered_through_message_id: None,
            });
        conversation.native_session_id = session_id.to_string();
        lock(&self.harness_sessions).insert(key, conversation.clone());
        if let Some(workspace) = self.workspace() {
            workspace.set_chat_harness_conversation(chat_id, &conversation);
            // Keep the singular latest-session fields populated for existing
            // diagnostics; resume routing never reads them.
            workspace.set_chat_harness_session(chat_id, session_id, cwd);
        }
    }

    pub(super) fn remember_harness_coverage(
        &self,
        chat_id: &str,
        harness: HarnessId,
        cwd: &str,
        message_id: &str,
    ) {
        let Some(mut conversation) = self.conversation_for(chat_id, harness, cwd) else {
            return;
        };
        conversation.covered_through_message_id = Some(message_id.to_string());
        let key = Self::session_key(chat_id, harness, cwd);
        lock(&self.harness_sessions).insert(key, conversation.clone());
        if let Some(workspace) = self.workspace() {
            workspace.set_chat_harness_conversation(chat_id, &conversation);
        }
    }

    /// Invalidate only the rejected harness/cwd generation. Other native
    /// conversations attached to the chat remain resumable.
    pub(super) fn forget_harness_session(&self, chat_id: &str, harness: HarnessId, cwd: &str) {
        let key = Self::session_key(chat_id, harness, cwd);
        let mut conversation = self
            .conversation_for(chat_id, harness, cwd)
            .unwrap_or_else(|| jolt_proto::HarnessConversationRef {
                id: new_id(),
                harness,
                device_id: self.device_id.clone(),
                cwd: cwd.to_string(),
                native_session_id: String::new(),
                generation: 0,
                covered_through_message_id: None,
            });
        conversation.id = new_id();
        conversation.native_session_id.clear();
        conversation.generation = conversation.generation.saturating_add(1);
        conversation.covered_through_message_id = None;
        lock(&self.harness_sessions).insert(key, conversation.clone());
        if let Some(workspace) = self.workspace() {
            workspace.set_chat_harness_conversation(chat_id, &conversation);
            workspace.set_chat_harness_session(chat_id, "", "");
        }
    }

    pub(super) fn resume_for(
        &self,
        chat_id: &str,
        harness: HarnessId,
        cwd: &str,
    ) -> Option<String> {
        self.conversation_for(chat_id, harness, cwd)
            .filter(|conversation| !conversation.native_session_id.is_empty())
            .map(|conversation| conversation.native_session_id)
    }

    pub(super) fn journal_harness_session_for(
        &self,
        chat_id: &str,
        target_harness: HarnessId,
        target_cwd: &str,
    ) -> Option<String> {
        let events = self.journal.replay(chat_id, 0).ok()?;
        let mut current: Option<(HarnessId, String)> = None;
        let mut found = None;
        for (_, event) in events {
            match event {
                AgentEvent::SessionStarted {
                    harness,
                    session_id,
                    cwd,
                    ..
                } => {
                    current = Some((harness, cwd.clone()));
                    if harness == target_harness && cwd == target_cwd && !session_id.is_empty() {
                        found = Some(session_id);
                    }
                }
                AgentEvent::Done {
                    session_id: Some(session_id),
                    ..
                } if !session_id.is_empty()
                    && current.as_ref() == Some(&(target_harness, target_cwd.to_string())) =>
                {
                    found = Some(session_id);
                }
                _ => {}
            }
        }
        found
    }

    /// Latest native session in the journal, used only by crash recovery to
    /// reconstruct the abandoned run's own harness/cwd.
    pub(super) fn journal_harness_session(
        &self,
        chat_id: &str,
    ) -> Option<(HarnessId, String, String)> {
        let events = self.journal.replay(chat_id, 0).ok()?;
        let mut current: Option<(HarnessId, String)> = None;
        let mut found = None;
        for (_, event) in events {
            match event {
                AgentEvent::SessionStarted {
                    harness,
                    session_id,
                    cwd,
                    ..
                } => {
                    current = Some((harness, cwd.clone()));
                    if !session_id.is_empty() {
                        found = Some((harness, session_id, cwd));
                    }
                }
                AgentEvent::Done {
                    session_id: Some(session_id),
                    ..
                } if !session_id.is_empty() => {
                    if let Some((harness, cwd)) = &current {
                        found = Some((*harness, session_id, cwd.clone()));
                    }
                }
                _ => {}
            }
        }
        found
    }

    pub(super) fn last_harness(&self, chat_id: &str) -> Option<HarnessId> {
        lock(&self.usage_contexts)
            .get(chat_id)
            .map(|context| context.harness)
            .or_else(|| {
                self.journal_harness_session(chat_id)
                    .map(|(harness, _, _)| harness)
            })
    }

    pub(super) fn remove_run(&self, chat_id: &str, run_id: &str) {
        let mut runs = lock(&self.runs);
        if runs.get(chat_id).is_some_and(|h| h.run_id == run_id) {
            runs.remove(chat_id);
        }
    }
}

// ── run task ────────────────────────────────────────────────────────────────

/// Apply the render-parts privacy policy: strip heavy/sensitive tool inputs before doc
/// entry. Full inputs live only in the local run journal.
pub(super) fn render_parts(parts: &[MessagePart]) -> Vec<MessagePart> {
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
pub(super) fn folded_text(parts: &[MessagePart]) -> String {
    parts
        .iter()
        .filter_map(|part| match part {
            MessagePart::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

pub(super) fn sync_segment<'a>(
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

pub(super) fn finish_segment<'a>(
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

pub(super) const BASH_TRANSCRIPT_MAX_BYTES: u64 = 50 * 1024;

pub(super) fn bash_executable() -> PathBuf {
    let system_bash = Path::new("/bin/bash");
    if system_bash.exists() {
        system_bash.to_path_buf()
    } else {
        PathBuf::from("bash")
    }
}

pub(super) async fn run_local_bash(request: &BashRequest) -> Result<BashResult, EngineError> {
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
    if let Some(path) = jolt_platform::shell_env::login_shell_path() {
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

pub(super) async fn capture_turn_diff_baseline(
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
pub(super) enum TurnMutationScope {
    None,
    Paths(Vec<String>),
}

pub(super) fn successful_file_mutations(parts: &[MessagePart]) -> TurnMutationScope {
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

pub(super) async fn append_turn_diff(
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
pub(super) struct RunResumeState {
    pub(super) user_message_id: String,
    pub(super) resume_injected: bool,
    pub(super) turn_prompt: String,
    pub(super) context: Option<String>,
    pub(super) write_user_entry: bool,
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn drive_run(
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
    idle: Arc<AtomicBool>,
    retire: CancellationToken,
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
    // latency. `Some(when)` = idle since then; the 10-min reaper below ends
    // a session nobody comes back to (jolt SESSION_IDLE_MS).
    const SESSION_IDLE: std::time::Duration = std::time::Duration::from_secs(10 * 60);
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
            // Harness maintenance can retire a genuinely idle persistent
            // process immediately. The completed turn is already finalized,
            // so this is transcript-neutral and preserves native resume state.
            _ = retire.cancelled(), if idle_since.is_some() => {
                if let Some(token) = lock(&inner.runs)
                    .get(&chat_id)
                    .filter(|h| h.run_id == run_id)
                    .map(|h| h.interrupt_token.clone())
                {
                    token.cancel();
                }
                break SessionStatus::Idle;
            }
            // Idle reaper (jolt SESSION_IDLE_MS): a parked persistent session
            // nobody returned to in 10 minutes releases its child. The turn
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
            idle.store(false, Ordering::Release);
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
            inner.forget_harness_session(&chat_id, harness_id, &run_cwd);
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
            inner.remember_harness_coverage(&chat_id, harness_id, &run_cwd, &entry_id);
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
                inner.remember_harness_session(&chat_id, harness_id, session_id, cwd);
            }
            AgentEvent::Done {
                session_id: Some(session_id),
                ..
            } => {
                inner.remember_harness_session(&chat_id, harness_id, session_id, &run_cwd);
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
        let reveal_boundary = matches!(
            &event,
            AgentEvent::AssistantMessageCompleted { .. }
                | AgentEvent::ToolCall { .. }
                | AgentEvent::InputRequested { .. }
        ) || matches!(folded.last(), Some(MessagePart::TextReveal { .. }));

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
                inner.remember_harness_coverage(&chat_id, harness_id, &run_cwd, &entry_id);
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
                            command.status == jolt_session_doc::SessionCommandStatus::Pending
                                && matches!(
                                    command.payload,
                                    jolt_session_doc::SessionCommandPayload::Queue { .. }
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
                idle.store(!internal_follow_up_queued, Ordering::Release);
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

        if reveal_boundary && !folded.is_empty() {
            if let Err(err) = sync_segment(
                doc_ref,
                &mut writer,
                &entry_id,
                &device_id,
                segment_started,
                &folded,
            ) {
                tracing::warn!(chat = %chat_id, error = %err, "segment boundary sync failed");
            }
            dirty = false;
        } else if !folded.is_empty() && !dirty {
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
pub(super) fn finish_goal_turn(
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
pub(super) fn finish_goal_turn_in_workspace(
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

pub(super) fn apply_goal_blocker(
    goal: &mut jolt_proto::Goal,
    key: Option<String>,
    summary: String,
) {
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
