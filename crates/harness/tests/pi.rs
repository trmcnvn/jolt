//! PiHarness integration tests against the fake RPC CLI.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures::{StreamExt, stream::BoxStream};
use tokio::sync::{mpsc, oneshot};

use jolt_harness::environment::{HarnessEnvironment, HarnessEnvironmentProvider};
use jolt_harness::{
    BashMessage, BashRequest, CancellationToken, Harness, HarnessError, PiHarness, RunControls,
    SteerMessage,
};
use jolt_proto::{
    AgentCommandSource, AgentEvent, CommandContext, DoneStatus, HarnessId, ReasoningLevel,
    RunRequest, SandboxLevel, ToolCall, UserInputAnswer, UserInputQuestion,
};

fn fixture_path() -> PathBuf {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake-pi.sh");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755));
    }
    path
}

fn harness() -> PiHarness {
    PiHarness::new().with_executable(fixture_path())
}

struct TestEnvironment;

#[async_trait]
impl HarnessEnvironmentProvider for TestEnvironment {
    async fn environment(&self, harness: HarnessId) -> Result<Vec<(String, String)>, HarnessError> {
        assert_eq!(harness, HarnessId::Pi);
        Ok(vec![("JOLT_TEST_SECRET".into(), "available".into())])
    }
}

fn request(prompt: &str) -> RunRequest {
    RunRequest {
        prompt: prompt.into(),
        model: Some("test-provider/alpha".into()),
        reasoning: Some(ReasoningLevel::High),
        model_options: serde_json::Map::new(),
        cwd: "/tmp".into(),
        sandbox: SandboxLevel::DangerFullAccess,
        auto_approve: false,
        attachments: Vec::new(),
        resume: None,
    }
}

fn controls(answer: &'static str) -> (RunControls, mpsc::Sender<SteerMessage>, CancellationToken) {
    let (steer_tx, steer_rx) = mpsc::channel(8);
    let interrupt = CancellationToken::new();
    let controls = RunControls {
        request_input: Box::new(move |questions: Vec<UserInputQuestion>| {
            let (tx, rx) = oneshot::channel();
            let answers = questions
                .iter()
                .map(|question| UserInputAnswer {
                    question_id: question.id.clone(),
                    labels: vec![answer.into()],
                })
                .collect();
            let _ = tx.send(answers);
            rx
        }),
        steering: steer_rx,
        bash: mpsc::channel(1).1,
        interrupt: interrupt.clone(),
    };
    (controls, steer_tx, interrupt)
}

async fn until_done(
    stream: &mut BoxStream<'static, Result<AgentEvent, HarnessError>>,
) -> Vec<AgentEvent> {
    tokio::time::timeout(Duration::from_secs(10), async {
        let mut events = Vec::new();
        while let Some(event) = stream.next().await {
            let event = event.expect("valid event");
            let done = matches!(event, AgentEvent::Done { .. });
            events.push(event);
            if done {
                break;
            }
        }
        events
    })
    .await
    .expect("Pi run settled")
}

#[tokio::test]
async fn commands_are_discovered_from_pi_rpc() {
    let commands = harness()
        .commands(CommandContext {
            cwd: "/tmp".into(),
            model_options: serde_json::Map::new(),
        })
        .await
        .unwrap();
    assert_eq!(commands.len(), 2);
    assert_eq!(commands[0].name, "review");
    assert_eq!(commands[0].source, AgentCommandSource::Extension);
    assert_eq!(commands[1].source, AgentCommandSource::Skill);
}

#[tokio::test]
async fn models_are_live_provider_qualified_and_selected_first() {
    let models = harness().models().await.unwrap();
    assert_eq!(models.len(), 2);
    assert_eq!(models[0].id, "openai-codex/beta");
    assert_eq!(models[0].label, "Beta");
    assert_eq!(
        models[0].reasoning_levels.last(),
        Some(&ReasoningLevel::Max)
    );
    assert_eq!(models[1].id, "test-provider/alpha");
    assert_eq!(
        models[1].reasoning_levels,
        vec![
            ReasoningLevel::Low,
            ReasoningLevel::Medium,
            ReasoningLevel::High
        ]
    );
    let trust = &models[0].options[0];
    assert_eq!(trust.id, "projectTrust");
    assert_eq!(trust.default_choice, "ask");
    let access = &models[0].options[1];
    assert_eq!(access.id, "toolAccess");
    assert_eq!(access.default_choice, "full");
}

#[tokio::test]
async fn warm_pi_session_executes_bash_through_its_control_mailbox() {
    let (steer_tx, steer_rx) = mpsc::channel(1);
    let (bash_tx, bash_rx) = mpsc::channel(1);
    let interrupt = CancellationToken::new();
    let controls = RunControls {
        request_input: Box::new(|_| oneshot::channel().1),
        steering: steer_rx,
        bash: bash_rx,
        interrupt: interrupt.clone(),
    };
    let mut stream = harness()
        .run(request("scenario:shell-base"), controls)
        .await
        .unwrap();
    let events = until_done(&mut stream).await;
    assert!(matches!(
        events.last(),
        Some(AgentEvent::Done {
            status: DoneStatus::Completed,
            ..
        })
    ));

    let (response, result) = oneshot::channel();
    bash_tx
        .send(BashMessage {
            request: BashRequest {
                command: "printf hidden".into(),
                cwd: "/tmp".into(),
                resume: None,
                model_options: serde_json::Map::new(),
                exclude_from_context: true,
            },
            response,
        })
        .await
        .unwrap();
    let result = tokio::time::timeout(Duration::from_secs(2), result)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(result.output, "shell-output");
    assert_eq!(result.session_id.as_deref(), Some("pi-session-1"));

    drop(steer_tx);
    interrupt.cancel();
}

#[tokio::test]
async fn direct_bash_records_context_visibility_in_pi() {
    let request = |command: &str, exclude_from_context| BashRequest {
        command: command.into(),
        cwd: "/tmp".into(),
        resume: Some("resume-123".into()),
        model_options: serde_json::Map::new(),
        exclude_from_context,
    };
    let included = harness()
        .bash(request("printf shell-output", false))
        .await
        .unwrap();
    assert_eq!(included.output, "shell-output");
    assert_eq!(included.exit_code, Some(0));
    assert_eq!(included.session_id.as_deref(), Some("resume-123"));

    harness()
        .bash(request("printf hidden", true))
        .await
        .unwrap();
}

#[tokio::test]
async fn command_only_extension_surfaces_custom_output_and_completes() {
    let (controls, _steer, _interrupt) = controls("Blue");
    let mut stream = harness().run(request("/review"), controls).await.unwrap();
    let events = until_done(&mut stream).await;
    assert!(events.iter().any(|event| matches!(
        event,
        AgentEvent::TextDelta { text } if text == "Review ready"
    )));
    assert!(matches!(
        events.last(),
        Some(AgentEvent::Done {
            status: DoneStatus::Completed,
            ..
        })
    ));
}

#[tokio::test]
async fn happy_path_maps_streaming_tools_usage_images_and_done() {
    let temp = tempfile::tempdir().unwrap();
    let image = temp.path().join("tiny.png");
    std::fs::write(&image, [0x89, b'P', b'N', b'G', 0, 0]).unwrap();
    let mut req = request("scenario:happy");
    req.attachments = vec![image.display().to_string()];
    let (controls, _steer, _interrupt) = controls("Yes");
    let mut stream = harness().run(req, controls).await.unwrap();
    let events = until_done(&mut stream).await;

    assert!(events.iter().any(|event| matches!(
        event,
        AgentEvent::SessionStarted {
            harness: HarnessId::Pi,
            model,
            session_id,
            ..
        } if model == "test-provider/alpha" && session_id == "pi-session-1"
    )));
    assert!(events.contains(&AgentEvent::CompactionStarted));
    assert!(events.contains(&AgentEvent::CompactionFinished));
    assert!(events.contains(&AgentEvent::ReasoningDelta {
        text: "considering".into()
    }));
    assert!(events.contains(&AgentEvent::TextDelta {
        text: "Hello from Pi".into()
    }));
    assert!(events.contains(&AgentEvent::ToolCall {
        id: "b1".into(),
        call: ToolCall::Exec {
            command: "cargo test".into()
        }
    }));
    assert!(events.contains(&AgentEvent::ToolResult {
        id: "e1".into(),
        is_error: true
    }));
    assert!(events.contains(&AgentEvent::Usage {
        input_tokens: 3,
        output_tokens: 4,
        cache_read_input_tokens: 5,
        cache_write_input_tokens: 6,
        cost_usd: None,
        context_tokens: Some(14),
        context_window: None,
    }));
    assert!(events.iter().any(|event| matches!(
        event,
        AgentEvent::Done {
            status: DoneStatus::Completed,
            session_id: Some(id),
            ..
        } if id == "pi-session-1"
    )));
}

#[tokio::test]
async fn steering_uses_native_rpc_command() {
    let (controls, steer, _interrupt) = controls("Yes");
    let mut stream = harness()
        .run(request("scenario:steer"), controls)
        .await
        .unwrap();
    while let Some(event) = stream.next().await {
        if event.unwrap()
            == (AgentEvent::TextDelta {
                text: "first".into(),
            })
        {
            break;
        }
    }
    steer
        .send(SteerMessage {
            prompt: "redirect please".into(),
            message_id: None,
        })
        .await
        .unwrap();
    let events = until_done(&mut stream).await;
    assert!(
        events
            .iter()
            .any(|event| matches!(event, AgentEvent::Steered { .. }))
    );
    assert!(events.contains(&AgentEvent::TextDelta {
        text: "steered".into()
    }));
}

#[tokio::test]
async fn extension_command_steers_use_prompt_and_complete_without_agent_settled() {
    let (controls, steer, _interrupt) = controls("Yes");
    let mut stream = harness()
        .run(request("scenario:steer"), controls)
        .await
        .unwrap();
    while let Some(event) = stream.next().await {
        if event.unwrap()
            == (AgentEvent::TextDelta {
                text: "first".into(),
            })
        {
            break;
        }
    }
    steer
        .send(SteerMessage {
            prompt: "/review".into(),
            message_id: None,
        })
        .await
        .unwrap();
    let events = until_done(&mut stream).await;
    assert!(events.contains(&AgentEvent::TextDelta {
        text: "Review ready".into()
    }));
    assert!(matches!(
        events.last(),
        Some(AgentEvent::Done {
            status: DoneStatus::Completed,
            ..
        })
    ));
}

#[tokio::test]
async fn rejected_steer_falls_back_to_queued_prompt() {
    let (controls, steer, _interrupt) = controls("Yes");
    let mut stream = harness()
        .run(request("scenario:steer-race"), controls)
        .await
        .unwrap();
    while let Some(event) = stream.next().await {
        if event.unwrap()
            == (AgentEvent::TextDelta {
                text: "first".into(),
            })
        {
            break;
        }
    }
    steer
        .send(SteerMessage {
            prompt: "redirect please".into(),
            message_id: None,
        })
        .await
        .unwrap();
    let events = until_done(&mut stream).await;
    assert!(events.contains(&AgentEvent::TextDelta {
        text: "fallback".into()
    }));
}

#[tokio::test]
async fn provider_error_settles_errored_with_message() {
    let (controls, _steer, _interrupt) = controls("Yes");
    let mut stream = harness()
        .run(request("scenario:fail"), controls)
        .await
        .unwrap();
    let events = until_done(&mut stream).await;
    assert!(events.iter().any(|event| matches!(
        event,
        AgentEvent::Done {
            status: DoneStatus::Errored,
            error: Some(error),
            ..
        } if error == "provider exploded"
    )));
}

#[tokio::test]
async fn extension_select_round_trips_through_jolt_input() {
    let (controls, _steer, _interrupt) = controls("Blue");
    let mut stream = harness()
        .run(request("scenario:input"), controls)
        .await
        .unwrap();
    let events = until_done(&mut stream).await;
    assert!(events.contains(&AgentEvent::TextDelta {
        text: "picked Blue".into()
    }));
}

#[tokio::test]
async fn interrupt_sends_abort_and_settles_interrupted() {
    let (controls, _steer, interrupt) = controls("Yes");
    let mut stream = harness()
        .run(request("scenario:interrupt"), controls)
        .await
        .unwrap();
    while let Some(event) = stream.next().await {
        if event.unwrap()
            == (AgentEvent::TextDelta {
                text: "working".into(),
            })
        {
            break;
        }
    }
    interrupt.cancel();
    let events = until_done(&mut stream).await;
    assert!(events.iter().any(|event| matches!(
        event,
        AgentEvent::Done {
            status: DoneStatus::Interrupted,
            error: None,
            ..
        }
    )));
}

#[tokio::test]
async fn scoped_environment_reaches_pi_child() {
    let environment = HarnessEnvironment::default();
    environment.set_provider(Arc::new(TestEnvironment));
    let harness = harness().with_environment(environment);
    let (controls, _steer, _interrupt) = controls("Yes");
    let mut stream = harness
        .run(request("scenario:environment"), controls)
        .await
        .unwrap();
    let events = until_done(&mut stream).await;
    assert!(events.iter().any(|event| matches!(
        event,
        AgentEvent::Done {
            status: DoneStatus::Completed,
            ..
        }
    )));
}

#[tokio::test]
async fn resume_trust_and_read_only_flags_reach_pi() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(temp.path().join(".pi/extensions")).unwrap();
    let mut req = request("scenario:args");
    req.cwd = temp.path().display().to_string();
    req.resume = Some("resume-123".into());
    req.sandbox = SandboxLevel::WorkspaceWrite;
    req.model_options
        .insert("projectTrust".into(), serde_json::json!("trust"));
    req.model_options
        .insert("toolAccess".into(), serde_json::json!("readOnly"));
    let (controls, _steer, _interrupt) = controls("Yes");
    let mut stream = harness().run(req, controls).await.unwrap();
    let events = until_done(&mut stream).await;
    assert!(events.iter().any(|event| matches!(
        event,
        AgentEvent::Done {
            status: DoneStatus::Completed,
            ..
        }
    )));
    assert!(events.iter().any(|event| matches!(
        event,
        AgentEvent::SessionStarted { session_id, .. } if session_id == "resume-123"
    )));
}

#[tokio::test]
async fn unresponsive_pi_is_reaped_after_interrupt() {
    let harness = harness().with_graces(Duration::from_millis(40), Duration::from_millis(40));
    let (controls, _steer, interrupt) = controls("Yes");
    let mut stream = harness
        .run(request("scenario:wedge"), controls)
        .await
        .unwrap();
    while let Some(event) = stream.next().await {
        if event.unwrap()
            == (AgentEvent::TextDelta {
                text: "working".into(),
            })
        {
            break;
        }
    }
    interrupt.cancel();
    let events = until_done(&mut stream).await;
    assert!(events.iter().any(|event| matches!(
        event,
        AgentEvent::Done {
            status: DoneStatus::Interrupted,
            ..
        }
    )));
}

/// Manual compatibility probe against the user's installed Pi CLI. It never
/// sends a model prompt or makes a provider request.
#[tokio::test]
#[ignore = "requires an installed, authenticated Pi CLI"]
async fn installed_pi_models_smoke() {
    let models = PiHarness::new().models().await.unwrap();
    assert!(!models.is_empty());
    assert!(models.iter().all(|model| model.id.contains('/')));
}

#[tokio::test]
async fn missing_binary_is_not_installed() {
    let missing = PiHarness::new().with_executable("/definitely/not/a/pi-binary");
    assert!(matches!(
        missing.models().await,
        Err(HarnessError::NotInstalled(_))
    ));
}
