//! Module behavior tests.
use super::*;
use crate::workspace_host::WorkspaceHostConfig;

fn goal_workspace() -> (tempfile::TempDir, crate::workspace_host::WorkspaceHost) {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(jolt_store::DocsStore::open(dir.path()).unwrap());
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

fn bare_sessions() -> (tempfile::TempDir, SessionsEngine) {
    let dir = tempfile::tempdir().unwrap();
    let journal = Arc::new(RunJournal::open(dir.path().join("journals")).unwrap());
    let usage = UsageStore::open(&dir.path().join("usage.sqlite"), "device-1".into()).unwrap();
    (
        dir,
        SessionsEngine::new(
            "device-1".into(),
            journal,
            Arc::new(crate::registry::default_registry()),
            usage,
        ),
    )
}

fn insert_test_run(sessions: &SessionsEngine, chat_id: &str, idle: bool) -> CancellationToken {
    let (steer_tx, _) = mpsc::channel(1);
    let (cancel, _) = watch::channel(false);
    let (engine_tx, _) = mpsc::unbounded_channel();
    let retire = CancellationToken::new();
    lock(&sessions.inner.runs).insert(
        chat_id.into(),
        RunHandle {
            run_id: new_id(),
            harness: HarnessId::Pi,
            steerable: true,
            steer_tx,
            bash_tx: None,
            interrupt_token: CancellationToken::new(),
            cancel,
            engine_tx,
            pending_inputs: Arc::new(Mutex::new(HashMap::new())),
            compaction_follow_up: Arc::new(CompactionFollowUp::default()),
            pending_external_turns: Arc::new(AtomicUsize::new(0)),
            turn_diff_tracker: Arc::new(Mutex::new(TurnDiffTracker::default())),
            idle: Arc::new(AtomicBool::new(idle)),
            retire: retire.clone(),
        },
    );
    retire
}

#[test]
fn maintenance_retires_only_genuinely_idle_harness_processes() {
    let (_dir, sessions) = bare_sessions();
    let idle = insert_test_run(&sessions, "idle", true);
    let busy = insert_test_run(&sessions, "busy", false);

    assert_eq!(sessions.harness_run_counts(HarnessId::Pi), (1, 1));
    assert_eq!(sessions.retire_idle_harness(HarnessId::Pi), 1);
    assert!(idle.is_cancelled());
    assert!(!busy.is_cancelled());

    sessions.set_harness_maintenance(HarnessId::Pi, true);
    assert!(sessions.harness_in_maintenance(HarnessId::Pi));
    sessions.set_harness_maintenance(HarnessId::Pi, false);
    assert!(!sessions.harness_in_maintenance(HarnessId::Pi));
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
        TurnMutationScope::ExactPaths(vec!["src/session.rs".into()])
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
        TurnMutationScope::ExactPaths(vec!["src/one.rs".into(), "src/two.rs".into()])
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
        TurnMutationScope::Unknown
    );
}

#[test]
fn path_reporting_and_opaque_tools_produce_a_partial_scope() {
    assert_eq!(
        successful_file_mutations(&[
            tool_part(
                ToolCall::WriteFile {
                    path: "src/known.rs".into(),
                    content: None,
                },
                false,
            ),
            tool_part(
                ToolCall::Exec {
                    command: "generator".into(),
                },
                false,
            ),
        ]),
        TurnMutationScope::PartialPaths(vec!["src/known.rs".into()])
    );
}

#[test]
fn turn_diff_tracker_names_boundary_and_rollback_transitions() {
    let baseline = |name| Some(crate::TurnDiffBaseline::for_tracker_test(name));
    let mut tracker = TurnDiffTracker::new(baseline("initial"));
    assert!(tracker.active().is_some());

    tracker.queue(baseline("rejected"));
    tracker.rollback_last_queue();
    tracker.advance_after_done();
    assert!(tracker.active().is_none());

    tracker.queue(baseline("queued-too-early"));
    tracker.observe_boundary(baseline("observed-boundary"));
    assert!(tracker.active().is_some());
    tracker.advance_after_done();
    assert!(tracker.active().is_none());

    tracker.install_if_missing(baseline("internal-follow-up"));
    assert!(tracker.active().is_some());
}

fn active_goal(workspace: &crate::workspace_host::WorkspaceHost) -> jolt_proto::Goal {
    let goal = crate::goals::apply_operation(
        None,
        &jolt_session_doc::GoalOperation::Create {
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
