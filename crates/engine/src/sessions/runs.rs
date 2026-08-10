//! Agent run dispatch, steering, interruption, and shutdown.

use super::*;

impl SessionsEngine {
    /// Start or route one agent run for a chat.
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
    #[allow(clippy::too_many_arguments)] // internal retry seam keeps dispatch policy explicit
    pub(super) fn dispatch_with<'a>(
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

    #[allow(clippy::too_many_arguments)] // mirrors the retry seam above
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
        // Keep only caller-provided context for a failed native-resume retry;
        // goal and handoff context are rebuilt against the then-current state.
        let retry_context = context.clone();
        let goal = self
            .inner
            .workspace()
            .and_then(|workspace| workspace.chat_goal(chat_id))
            .filter(|goal| goal.status == jolt_proto::GoalStatus::Active);
        let goal_context = goal.as_ref().map(crate::goals::context);
        let mut context = join_context(context, goal_context);
        let harness = self.inner.registry.resolve(harness_id)?;
        let handle = self.doc_handle(chat_id)?;
        let user_id = message_id.clone().unwrap_or_else(new_id);
        let routed = lock(&self.inner.runs).get(chat_id).map(|h| {
            (
                h.run_id.clone(),
                h.harness,
                h.steerable,
                h.steer_tx.clone(),
                h.compaction_follow_up.clone(),
                h.pending_external_turns.clone(),
                h.turn_diff_tracker.clone(),
            )
        });
        if let Some((
            run_id,
            active_harness,
            steerable,
            steer_tx,
            compaction_follow_up,
            pending_external_turns,
            turn_diff_tracker,
        )) = routed
        {
            if write_user_entry {
                compaction_follow_up.cancel_for_user_message();
            }
            let message = SteerMessage {
                prompt: request.prompt.clone(),
                message_id: message_id.clone(),
            };
            let same_harness = active_harness == harness_id;
            if write_user_entry && !same_harness {
                // Make the handoff visible before waiting for the source run to
                // settle and finalize its turn diff. The later write is
                // idempotent and preserves the marker/message adjacency.
                handle.write_harness_switch(&user_id, active_harness, harness_id, now_ms())?;
            }
            let turn_diff_baseline = if steerable && same_harness {
                self.capture_turn_diff_baseline(chat_id, &request.cwd).await
            } else {
                None
            };
            if steerable && same_harness {
                pending_external_turns.fetch_add(1, Ordering::AcqRel);
                lock(&turn_diff_tracker).queue(turn_diff_baseline);
            }
            if steerable && same_harness && steer_tx.try_send(message).is_ok() {
                if write_user_entry {
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
            if steerable && same_harness {
                pending_external_turns.fetch_sub(1, Ordering::AcqRel);
                lock(&turn_diff_tracker).rollback_last_queue();
            }
            // A harness switch is a settled-boundary operation. Interrupt and
            // wait for the current segment/diff to finalize before summarizing.
            self.interrupt(chat_id).await?;
        }

        // Resolve the target continuation before choosing a handoff. Resuming
        // the currently active harness already carries its own native context;
        // returning to a different harness still needs the missed-turn delta.
        let mut resume_injected = false;
        if request.resume.is_none() && inject_resume {
            request.resume = self.inner.resume_for(chat_id, harness_id, &request.cwd);
            resume_injected = request.resume.is_some();
        }
        let source_harness = self.inner.last_harness(chat_id);
        let conversation = self
            .inner
            .conversation_for(chat_id, harness_id, &request.cwd);
        let handoff = if source_harness == Some(harness_id) && request.resume.is_some() {
            None
        } else {
            crate::handoff::build(
                handle.doc(),
                goal.as_ref(),
                source_harness,
                harness_id,
                conversation
                    .as_ref()
                    .and_then(|conversation| conversation.covered_through_message_id.as_deref()),
            )?
        };
        if let Some(handoff) = &handoff {
            context = join_context(context, Some(handoff.text.clone()));
        }
        if let Some(context) = &context {
            request.prompt = format!("{context}\n\n{turn_prompt}");
        }
        if write_user_entry {
            let created_at = now_ms();
            if let Some(source_harness) = source_harness.filter(|source| *source != harness_id) {
                handle.write_harness_switch(&user_id, source_harness, harness_id, created_at)?;
            }
            handle.write_user_message(&user_id, &turn_prompt, created_at)?;
        }

        let mut saved_request = request.clone();
        saved_request.prompt = turn_prompt.clone();
        lock(&self.inner.last_requests).insert(chat_id.to_string(), saved_request);
        lock(&self.inner.usage_contexts).insert(
            chat_id.to_string(),
            UsageContext {
                harness: harness_id,
                model: request.model.clone().unwrap_or_default(),
                cwd: request.cwd.clone(),
                service_tier: request
                    .model_options
                    .get("serviceTier")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned),
            },
        );

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
        let idle = Arc::new(AtomicBool::new(false));
        let retire = CancellationToken::new();
        let initial_turn_diff_baseline =
            self.capture_turn_diff_baseline(chat_id, &request.cwd).await;
        let turn_diff_tracker =
            Arc::new(Mutex::new(TurnDiffTracker::new(initial_turn_diff_baseline)));

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
                harness: harness_id,
                steerable: harness.supports_steering(),
                steer_tx,
                bash_tx: harness.supports_native_bash().then_some(bash_tx),
                interrupt_token,
                cancel: cancel_tx,
                engine_tx,
                pending_inputs,
                compaction_follow_up: compaction_follow_up.clone(),
                pending_external_turns: pending_external_turns.clone(),
                turn_diff_tracker: turn_diff_tracker.clone(),
                idle: idle.clone(),
                retire: retire.clone(),
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
            turn_diff_tracker,
            idle,
            retire,
            RunResumeState {
                user_message_id: user_id,
                resume_injected,
                turn_prompt,
                context: retry_context,
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
            request.resume = self.inner.resume_for(chat_id, harness_id, &request.cwd);
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
                .remember_harness_session(chat_id, harness_id, session_id, &request.cwd);
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
        self.steer_with_context(chat_id, prompt, message_id, None, None)
            .await
    }

    pub(crate) async fn steer_with_context(
        &self,
        chat_id: &str,
        prompt: &str,
        message_id: Option<String>,
        context: Option<String>,
        expected_harness: Option<HarnessId>,
    ) -> Result<SteerOutcome, EngineError> {
        let target = lock(&self.inner.runs)
            .get(chat_id)
            .filter(|handle| {
                handle.steerable && expected_harness.is_none_or(|harness| harness == handle.harness)
            })
            .map(|h| {
                (
                    h.steer_tx.clone(),
                    h.compaction_follow_up.clone(),
                    h.pending_external_turns.clone(),
                    h.turn_diff_tracker.clone(),
                )
            });
        let Some((steer_tx, compaction_follow_up, pending_external_turns, turn_diff_tracker)) =
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
        lock(&turn_diff_tracker).queue(turn_diff_baseline);
        if steer_tx.try_send(message).is_err() {
            pending_external_turns.fetch_sub(1, Ordering::AcqRel);
            lock(&turn_diff_tracker).rollback_last_queue();
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
        // Mark the run interrupted before harness teardown can close its event stream;
        // otherwise that close can race ahead and be classified as an error.
        let _ = cancel.send(true);
        // Harness-level interrupt (protocol + child teardown). The run task gives it a
        // grace period, then synthesizes Done{interrupted} if the harness ignores this.
        token.cancel();
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
            if let Some((harness, session_id, cwd)) = self.inner.journal_harness_session(&chat_id) {
                self.inner
                    .remember_harness_session(&chat_id, harness, &session_id, &cwd);
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
                        let (harness, _, cwd) = sessions.inner.journal_harness_session(&chat_id)?;
                        Some(RunRequest {
                            prompt: String::new(),
                            harness: Some(harness),
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
                let harness_id = host.harness_for_request(&chat_id, &request);
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
