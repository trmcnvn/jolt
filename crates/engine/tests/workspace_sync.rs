//! Workspace-registry integration across two `EngineCore`s with distinct data
//! directories and device identities. The in-memory bridge speaks the same JSON
//! protocol as RegistryRoom; an ignored variant exercises a live edge.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use futures::stream::BoxStream;

use jolt_api::methods;
use jolt_engine::{EngineCore, HarnessRegistry};
use jolt_harness::{Harness, HarnessError, RunControls};
use jolt_proto::{
    AgentEvent, ChatConfig, DoneStatus, HarnessId, Model, ReasoningLevel, RunRequest, SandboxLevel,
    SessionStatus, SteeringMode,
};
use jolt_session_doc::{
    CommandBasedOn, SessionCommandEntry, SessionCommandPayload, SessionCommandStatus,
};

const VIEWER: &str = "viewer-device";

/// Scripted harness: emits SessionStarted + text + Done with a per-event delay (so
/// `Working` is observable across the bridge).
struct ScriptedHarness {
    id: HarnessId,
    text: &'static str,
    step_delay: Duration,
}

#[async_trait]
impl Harness for ScriptedHarness {
    fn id(&self) -> HarnessId {
        self.id
    }
    fn display_name(&self) -> &str {
        "Scripted"
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
        _controls: RunControls,
    ) -> Result<BoxStream<'static, Result<AgentEvent, HarnessError>>, HarnessError> {
        let (tx, rx) = tokio::sync::mpsc::channel::<Result<AgentEvent, HarnessError>>(16);
        let harness = self.id;
        let text = self.text;
        let delay = self.step_delay;
        tokio::spawn(async move {
            let script = vec![
                AgentEvent::SessionStarted {
                    harness,
                    model: "scripted-1".into(),
                    tools: vec![],
                    cwd: "/tmp".into(),
                    session_id: "hs-1".into(),
                    assistant_message_id: "a-1".into(),
                },
                AgentEvent::TextDelta { text: text.into() },
                AgentEvent::Done {
                    status: DoneStatus::Completed,
                    result: None,
                    error: None,
                    session_id: Some("hs-1".into()),
                },
            ];
            for event in script {
                if tx.send(Ok(event)).await.is_err() {
                    return;
                }
                tokio::time::sleep(delay).await;
            }
        });
        Ok(futures::stream::unfold(rx, |mut rx| async move {
            rx.recv().await.map(|event| (event, rx))
        })
        .boxed())
    }
}

fn registry() -> Arc<HarnessRegistry> {
    let registry = HarnessRegistry::new();
    registry.register(Arc::new(ScriptedHarness {
        id: HarnessId::Mock,
        text: "Hello",
        step_delay: Duration::from_millis(60),
    }));
    registry.register(Arc::new(ScriptedHarness {
        id: HarnessId::Pi,
        text: "From Pi",
        step_delay: Duration::from_millis(10),
    }));
    Arc::new(registry)
}

/// Assemble an engine with a fixed device id under its own data dir (offline).
fn assemble(dir: &std::path::Path, device_id: &str) -> EngineCore {
    let scope = dir.join("scopes/accounts/dev-org/dev-user");
    std::fs::create_dir_all(&scope).expect("create data dir");
    std::fs::write(scope.join("device-id"), device_id).expect("write device id");
    EngineCore::assemble(dir, registry(), HarnessId::Mock, None).expect("engine core assembles")
}

/// The in-process room: an in-memory registry server speaking the DO's JSON
/// WS protocol (what the RegistryRoom DO does over the wire), with both
/// engines' hosts wired to it via the test seam.
async fn bridge(
    a: &EngineCore,
    b: &EngineCore,
) -> jolt_sync::registry::mock_server::MockRegistryServer {
    let server = jolt_sync::registry::mock_server::MockRegistryServer::start().await;
    a.workspace.connect_registry_url(&server.url());
    b.workspace.connect_registry_url(&server.url());
    server
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
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

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

/// Queue a run command into a chat doc the way a remote viewer would (ledger rule 1).
fn queue_run(core: &EngineCore, chat_id: &str, command_id: &str, message_id: &str) {
    queue_run_with(
        core,
        chat_id,
        command_id,
        message_id,
        run_request("go do it"),
    );
}

fn queue_run_with(
    core: &EngineCore,
    chat_id: &str,
    command_id: &str,
    message_id: &str,
    request: RunRequest,
) {
    let handle = core.doc_host.open(chat_id).expect("open chat");
    let now = chrono::Utc::now().timestamp_millis();
    handle
        .doc()
        .queue_command(&SessionCommandEntry {
            id: command_id.into(),
            payload: SessionCommandPayload::Run {
                request,
                message_id: message_id.into(),
            },
            issued_by: VIEWER.into(),
            issued_at: now,
            based_on: None::<CommandBasedOn>,
            expires_at: None,
            status: SessionCommandStatus::Pending,
            resolution: None,
        })
        .expect("queue command");
}

#[tokio::test]
async fn custom_theme_files_converge_between_hosts() {
    let dir_a = tempfile::tempdir().unwrap();
    let dir_b = tempfile::tempdir().unwrap();
    let a = assemble(dir_a.path(), "dev-a");
    let b = assemble(dir_b.path(), "dev-b");
    let _link = bridge(&a, &b).await;

    let theme_id = "00000000-0000-4000-8000-000000000001";
    a.workspace
        .upsert_themes(&[jolt_proto::ThemeFileRecord {
            id: theme_id.into(),
            revision: 1,
            deleted: false,
            contents: r#"{"revision":1}"#.into(),
        }])
        .unwrap();
    wait_for(
        || {
            b.workspace
                .read_themes()
                .is_ok_and(|themes| themes.len() == 1)
        },
        "theme file on peer",
    )
    .await;
    assert_eq!(b.workspace.read_themes().unwrap()[0].id, theme_id);

    b.workspace.delete_theme(theme_id).unwrap();
    wait_for(
        || {
            a.workspace
                .read_themes()
                .is_ok_and(|themes| themes.first().is_some_and(|theme| theme.deleted))
        },
        "theme deletion on peer",
    )
    .await;
}

#[tokio::test]
async fn two_engines_share_a_workspace() {
    let dir_a = tempfile::tempdir().unwrap();
    let dir_b = tempfile::tempdir().unwrap();
    let a = assemble(dir_a.path(), "dev-a");
    let b = assemble(dir_b.path(), "dev-b");
    let link = bridge(&a, &b).await;

    // Device rows from BOTH engines appear on both sides.
    for core in [&a, &b] {
        wait_for(
            || {
                let ids: Vec<String> = core
                    .workspace
                    .read_devices()
                    .unwrap_or_default()
                    .into_iter()
                    .map(|d| d.id)
                    .collect();
                ids == ["dev-a", "dev-b"]
            },
            "both device rows",
        )
        .await;
    }

    // CreateSpace + CreateChat on A (Mutate over the real RPC surface), hosted
    // by dev-a via the space.
    let client_a = jolt_rpc::memory_client(a.rpc_service());
    let client_b = jolt_rpc::memory_client(b.rpc_service());
    client_a
        .call(
            methods::MUTATE,
            serde_json::json!({
                "op": "createSpace", "spaceId": "space-1", "deviceId": "dev-a", "path": "/tmp"
            }),
        )
        .await
        .expect("create space");
    client_a
        .call(
            methods::MUTATE,
            serde_json::json!({
                "op": "createChat", "chatId": "chat-1", "spaceId": "space-1"
            }),
        )
        .await
        .expect("create chat");
    // The space row crosses to B alongside the chat row.
    wait_for(
        || {
            b.workspace
                .read_spaces()
                .unwrap_or_default()
                .iter()
                .any(|s| s.id == "space-1" && s.device_id == "dev-a" && s.path == "/tmp")
        },
        "space row on B",
    )
    .await;
    wait_for(
        || b.workspace.chat("chat-1").ok().flatten().is_some(),
        "chat row on B",
    )
    .await;

    // Run on A: B's workspace view shows the session Working, then Idle.
    queue_run(&a, "chat-1", "cmd-run-1", "m-1");
    let b_status = |wanted: SessionStatus| {
        let ws = b.workspace.clone();
        move || {
            ws.read_sessions()
                .unwrap_or_default()
                .iter()
                .any(|s| s.chat_id == "chat-1" && s.device_id == "dev-a" && s.status == wanted)
        }
    };
    wait_for(b_status(SessionStatus::Working), "Working on B").await;
    wait_for(b_status(SessionStatus::Idle), "Idle on B").await;

    // Sidebar freshness crossed too: the chat row's preview settles on the
    // assistant's final text (first-120-chars policy).
    wait_for(
        || {
            b.workspace
                .chat("chat-1")
                .ok()
                .flatten()
                .and_then(|c| c.last_message_preview)
                .as_deref()
                == Some("Hello")
        },
        "assistant preview on B",
    )
    .await;

    // Rename + pin + archive from B (LWW from any device) become visible on A.
    client_b
        .call(
            methods::MUTATE,
            serde_json::json!({ "op": "renameChat", "chatId": "chat-1", "title": "Renamed from B" }),
        )
        .await
        .expect("rename chat");
    client_b
        .call(
            methods::MUTATE,
            serde_json::json!({ "op": "setChatPinned", "chatId": "chat-1", "pinned": true }),
        )
        .await
        .expect("pin chat");
    client_b
        .call(
            methods::MUTATE,
            serde_json::json!({ "op": "setChatArchived", "chatId": "chat-1", "archived": true }),
        )
        .await
        .expect("archive chat");
    wait_for(
        || {
            a.workspace.chat("chat-1").ok().flatten().is_some_and(|c| {
                c.title.as_deref() == Some("Renamed from B") && c.pinned && c.archived
            })
        },
        "rename + pin + archive on A",
    )
    .await;

    // Device rename from B visible on A.
    client_b
        .call(
            methods::MUTATE,
            serde_json::json!({ "op": "renameDevice", "deviceId": "dev-b", "name": "B's VPS" }),
        )
        .await
        .expect("rename device");
    wait_for(
        || {
            a.workspace
                .read_devices()
                .unwrap_or_default()
                .iter()
                .any(|d| d.id == "dev-b" && d.name == "B's VPS")
        },
        "device rename on A",
    )
    .await;

    drop(link);
    a.shutdown().await;
    b.shutdown().await;
}

#[tokio::test]
async fn claim_on_first_command_creates_the_chat_row() {
    let dir_a = tempfile::tempdir().unwrap();
    let dir_b = tempfile::tempdir().unwrap();
    let a = assemble(dir_a.path(), "dev-a");
    let b = assemble(dir_b.path(), "dev-b");
    let link = bridge(&a, &b).await;

    // No CreateChat: the first run command claims the chat under A's device id.
    queue_run(&a, "chat-claimed", "cmd-claim-1", "m-1");
    wait_for(
        || {
            b.workspace
                .chat("chat-claimed")
                .ok()
                .flatten()
                .is_some_and(|c| c.device_id == "dev-a" && c.cwd.as_deref() == Some("/tmp"))
        },
        "claimed chat row on B",
    )
    .await;

    drop(link);
    a.shutdown().await;
    b.shutdown().await;
}

#[tokio::test]
async fn claim_resolves_worktree_cwd_to_repo_space() {
    let dir = tempfile::tempdir().unwrap();
    let core = assemble(dir.path(), "dev-a");
    let client = jolt_rpc::memory_client(core.rpc_service());

    let repo = tempfile::tempdir().unwrap();
    let root = repo.path().join("project");
    let worktree = repo.path().join("clever-ember");
    std::fs::create_dir_all(root.join(".git/worktrees/clever-ember")).unwrap();
    std::fs::create_dir_all(&worktree).unwrap();
    std::fs::write(
        worktree.join(".git"),
        format!(
            "gitdir: {}\n",
            root.join(".git/worktrees/clever-ember").display()
        ),
    )
    .unwrap();

    client
        .call(
            methods::MUTATE,
            serde_json::json!({
                "op": "createSpace", "spaceId": "space-project", "deviceId": "dev-a",
                "path": root.to_string_lossy(),
            }),
        )
        .await
        .expect("create project space");

    let request = RunRequest {
        cwd: worktree.to_string_lossy().into_owned(),
        ..run_request("go do it")
    };
    queue_run_with(&core, "chat-worktree", "cmd-worktree", "m-1", request);
    wait_for(
        || {
            core.workspace
                .chat("chat-worktree")
                .ok()
                .flatten()
                .is_some_and(|chat| chat.space_id.as_deref() == Some("space-project"))
        },
        "worktree chat attributed to project space",
    )
    .await;
    assert_eq!(core.workspace.read_spaces().unwrap().len(), 1);
    core.shutdown().await;
}

#[tokio::test]
async fn claimed_chat_records_request_harness() {
    let dir = tempfile::tempdir().unwrap();
    let core = assemble(dir.path(), "dev-a");
    let request = RunRequest {
        harness: Some(HarnessId::Pi),
        ..run_request("go do it")
    };

    queue_run_with(&core, "chat-harness", "cmd-harness", "m-1", request);
    wait_for(
        || {
            core.workspace
                .chat("chat-harness")
                .ok()
                .flatten()
                .and_then(|chat| chat.config)
                .is_some_and(|config| config.harness == HarnessId::Pi)
        },
        "claimed chat request harness",
    )
    .await;
    core.shutdown().await;
}

#[tokio::test]
async fn non_host_engine_leaves_remote_chats_commands_alone() {
    let dir_a = tempfile::tempdir().unwrap();
    let a = assemble(dir_a.path(), "dev-a");

    // The workspace says dev-b hosts this chat (via its dev-b space); a run
    // command in A's local copy of the session doc must NOT execute on A
    // (is_host gating).
    a.workspace
        .create_space("space-remote", "dev-b", "/tmp/remote", None, false)
        .expect("create remote space row");
    a.workspace
        .create_chat("chat-remote", "space-remote", None, None)
        .expect("create remote-hosted chat row");
    queue_run(&a, "chat-remote", "cmd-remote-1", "m-1");

    tokio::time::sleep(Duration::from_millis(400)).await;
    let handle = a.doc_host.open("chat-remote").expect("open chat");
    let commands = handle.doc().read_commands().expect("read commands");
    assert_eq!(commands.len(), 1);
    assert_eq!(
        commands[0].status,
        SessionCommandStatus::Pending,
        "command must stay pending"
    );
    let entries = handle.doc().read_entries().expect("read entries");
    assert!(
        entries.is_empty(),
        "non-host must not write entries: {entries:#?}"
    );
    assert!(a.sessions.session_status("chat-remote").is_none());

    a.shutdown().await;
}

#[tokio::test]
async fn chat_config_selects_the_run_harness() {
    let dir_a = tempfile::tempdir().unwrap();
    let a = assemble(dir_a.path(), "dev-a"); // default harness = Mock ("Hello")

    a.workspace
        .create_space("space-cfg", "dev-a", "/tmp/cfg", None, false)
        .expect("create space");
    a.workspace
        .create_chat(
            "chat-cfg",
            "space-cfg",
            Some(ChatConfig {
                harness: HarnessId::Pi,
                model: None,
                reasoning: None,
                model_options: Default::default(),
                sandbox: SandboxLevel::WorkspaceWrite,
            }),
            None,
        )
        .expect("create configured chat");
    queue_run(&a, "chat-cfg", "cmd-cfg-1", "m-1");

    // The configured harness (Pi, "From Pi") ran — not the default Mock.
    let handle = a.doc_host.open("chat-cfg").expect("open chat");
    wait_for(
        || {
            handle
                .doc()
                .read_entries()
                .unwrap_or_default()
                .iter()
                .any(|e| {
                    e.parts.iter().any(
                    |p| matches!(p, jolt_session_doc::MessagePart::Text { text, .. } if text == "From Pi"),
                )
                })
        },
        "configured-harness output",
    )
    .await;

    a.shutdown().await;
}

/// Live-edge variant: the same convergence through a real registry room. Requires
/// the TS edge (`wrangler dev` in `edge/` with AUTH_MODE=dev):
///
/// ```sh
/// JOLT_EDGE_WS=ws://127.0.0.1:8787 cargo test -p jolt-engine -- --ignored
/// ```
#[tokio::test]
#[ignore = "requires a live edge: set JOLT_EDGE_WS (e.g. ws://127.0.0.1:8787)"]
async fn two_engines_converge_through_a_real_registry_room() {
    use jolt_engine::doc_host::EdgeConfig;

    let base = std::env::var("JOLT_EDGE_WS")
        .expect("set JOLT_EDGE_WS to the edge origin, e.g. ws://127.0.0.1:8787");
    let org = format!("org-{}", uuid::Uuid::new_v4().simple());

    let assemble_live = |dir: &std::path::Path, device_id: &str, user: &str| {
        let scope = dir.join("scopes/accounts").join(&org).join(user);
        std::fs::create_dir_all(&scope).expect("create data dir");
        std::fs::write(scope.join("device-id"), device_id).expect("write device id");
        // Dev-mode bearer `user@org` carries the org claim the registry route checks.
        let edge = Some(EdgeConfig::with_static_token(
            base.clone(),
            format!("{user}@{org}"),
        ));
        EngineCore::assemble_with_identity(dir, registry(), HarnessId::Mock, edge, &org, user)
            .expect("engine core assembles")
    };

    // Registries are per-user: convergence is across
    // ONE user's devices — two engines, same user, different device ids.
    let dir_a = tempfile::tempdir().unwrap();
    let dir_b = tempfile::tempdir().unwrap();
    let a = assemble_live(dir_a.path(), "dev-live-a", "alice");
    let b = assemble_live(dir_b.path(), "dev-live-b", "alice");

    // Both device rows converge through the real room.
    for core in [&a, &b] {
        wait_for(
            || {
                let ids: Vec<String> = core
                    .workspace
                    .read_devices()
                    .unwrap_or_default()
                    .into_iter()
                    .map(|d| d.id)
                    .collect();
                ids == ["dev-live-a", "dev-live-b"]
            },
            "both device rows through the edge",
        )
        .await;
    }

    // A rename from B lands on A.
    b.workspace
        .rename_device("dev-live-a", "renamed by b")
        .expect("rename");
    wait_for(
        || {
            a.workspace
                .read_devices()
                .unwrap_or_default()
                .iter()
                .any(|d| d.id == "dev-live-a" && d.name == "renamed by b")
        },
        "device rename through the edge",
    )
    .await;

    a.shutdown().await;
    b.shutdown().await;
}

#[tokio::test]
#[ignore = "requires a live edge: set JOLT_EDGE_WS (e.g. ws://127.0.0.1:27640)"]
async fn remote_command_executes_and_publishes_through_session_hub() {
    use jolt_engine::doc_host::EdgeConfig;

    let ws_base = std::env::var("JOLT_EDGE_WS").expect("set JOLT_EDGE_WS");
    let http_base = ws_base
        .replacen("ws://", "http://", 1)
        .replacen("wss://", "https://", 1);
    let org = format!("org-{}", uuid::Uuid::new_v4().simple());
    let user = "alice";
    let token = format!("{user}@{org}");
    let chat = format!("hub-{}", uuid::Uuid::new_v4().simple());
    let dir = tempfile::tempdir().unwrap();
    let scope = dir.path().join("scopes/accounts").join(&org).join(user);
    std::fs::create_dir_all(&scope).unwrap();
    std::fs::write(scope.join("device-id"), "hub-host").unwrap();
    let imported_command_id = format!("imported-{}", uuid::Uuid::new_v4().simple());
    let imported_at = chrono::Utc::now().timestamp_millis();
    let store = Arc::new(jolt_store::DocsStore::open(&scope).unwrap());
    store
        .import_session_state(
            &chat,
            &[],
            &[SessionCommandEntry {
                id: imported_command_id.clone(),
                payload: SessionCommandPayload::Interrupt {},
                issued_by: "viewer-device".into(),
                issued_at: imported_at,
                based_on: None,
                expires_at: Some(imported_at + 60_000),
                status: SessionCommandStatus::Pending,
                resolution: None,
            }],
        )
        .unwrap();
    drop(store);
    let core = EngineCore::assemble_with_identity(
        dir.path(),
        registry(),
        HarnessId::Mock,
        Some(EdgeConfig::with_static_token(
            http_base.clone(),
            token.clone(),
        )),
        &org,
        user,
    )
    .unwrap();
    core.workspace
        .create_space("hub-space", "hub-host", "/tmp", None, false)
        .unwrap();
    core.workspace
        .create_chat(&chat, "hub-space", None, None)
        .unwrap();
    let handle = core.doc_host.open(&chat).unwrap();
    handle
        .write_user_message(
            "initial-race-message",
            "written while SessionHub connects",
            imported_at,
        )
        .unwrap();
    wait_for(|| handle.connected(), "SessionHub host connection").await;
    wait_for(
        || {
            handle
                .doc()
                .read_command(&imported_command_id)
                .unwrap_or_default()
                .is_some_and(|command| command.status != SessionCommandStatus::Pending)
        },
        "imported remote command reconciliation",
    )
    .await;

    let message_id = format!("message-{}", uuid::Uuid::new_v4().simple());
    let command_id = format!("command-{}", uuid::Uuid::new_v4().simple());
    let now = chrono::Utc::now().timestamp_millis();
    let command = serde_json::json!({
        "id": command_id,
        "kind": "run",
        "payload": {
            "kind": "run",
            "request": run_request("from remote"),
            "messageId": message_id
        },
        "issuedBy": "viewer-device",
        "issuedAt": now,
        "expiresAt": now + 60_000
    });
    reqwest::Client::new()
        .post(format!("{http_base}/hub/{chat}/command"))
        .bearer_auth(&token)
        .json(&command)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();

    wait_for(
        || {
            let entries = handle.doc().read_entries().unwrap_or_default();
            entries.iter().any(|entry| entry.id == message_id)
                && entries
                    .iter()
                    .any(|entry| entry.role == jolt_session_doc::MessageRole::Assistant)
        },
        "remote command transcript",
    )
    .await;
    wait_for(
        || {
            handle
                .doc()
                .read_commands()
                .unwrap_or_default()
                .iter()
                .any(|command| {
                    command.id == command_id && command.status == SessionCommandStatus::Applied
                })
        },
        "terminal remote command",
    )
    .await;

    let local_command_id = core
        .doc_host
        .queue_command(&chat, SessionCommandPayload::Interrupt {})
        .unwrap();
    wait_for(
        || {
            handle
                .doc()
                .read_command(&local_command_id)
                .unwrap_or_default()
                .is_some_and(|command| command.status != SessionCommandStatus::Pending)
        },
        "terminal local command",
    )
    .await;
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let page: serde_json::Value = reqwest::Client::new()
                .get(format!("{http_base}/hub/{chat}/commands?after=0"))
                .bearer_auth(&token)
                .send()
                .await
                .unwrap()
                .error_for_status()
                .unwrap()
                .json()
                .await
                .unwrap();
            if page["commands"].as_array().is_some_and(|commands| {
                commands.iter().any(|command| {
                    command["id"] == local_command_id && command["deliveryState"] == "terminal"
                })
            }) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("local command SessionHub reconciliation");

    let bootstrap: serde_json::Value = reqwest::Client::new()
        .get(format!("{http_base}/hub/{chat}/bootstrap"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(bootstrap["manifest"]["totalMessages"].as_u64().unwrap_or(0) >= 2);

    for index in 0..=jolt_session_doc::TRANSCRIPT_PAGE_MESSAGE_COUNT {
        handle
            .write_user_message(
                &format!("sealed-{index}"),
                "page body",
                now + index as i64 + 1,
            )
            .unwrap();
    }
    let client = reqwest::Client::new();
    let (page_id, content_hash) = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let bootstrap: serde_json::Value = client
                .get(format!("{http_base}/hub/{chat}/bootstrap"))
                .bearer_auth(&token)
                .send()
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
            if let Some(page) = bootstrap["manifest"]["pages"]
                .as_array()
                .and_then(|pages| pages.iter().find(|page| page["live"] == false))
                && let (Some(id), Some(hash)) = (page["id"].as_str(), page["contentHash"].as_str())
            {
                break (id.to_string(), hash.to_string());
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("sealed page publication");
    let page_response = client
        .get(format!("{http_base}/transcript/{chat}/page?id={page_id}"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert!(page_response.status().is_success());
    assert!(
        handle
            .doc()
            .page_is_published(&page_id, &content_hash)
            .unwrap()
    );
    wait_for(
        || !handle.doc().hub_projection_dirty().unwrap_or(true),
        "SessionHub projection acknowledgement",
    )
    .await;

    let recovery_chat = format!("recovery-{}", uuid::Uuid::new_v4().simple());
    let recovered = core
        .doc_host
        .import_recovery_fork(&chat, &recovery_chat)
        .await
        .expect("materialize recovery fork");
    let recovery_handle = core.doc_host.open(&recovery_chat).unwrap();
    let recovery_entries = recovery_handle.doc().read_entries().unwrap();
    assert_eq!(recovery_entries.len(), recovered);
    assert!(matches!(
        recovery_entries.last(),
        Some(entry) if entry.role == jolt_session_doc::MessageRole::System
    ));
    drop(recovery_handle);
    core.doc_host.purge_chat(&recovery_chat);

    core.shutdown().await;
    drop(handle);
    drop(core);

    let store = Arc::new(jolt_store::DocsStore::open(&scope).unwrap());
    store
        .open_session(&chat)
        .unwrap()
        .set_command_status(&command_id, SessionCommandStatus::Pending, None)
        .unwrap();
    drop(store);
    let restarted = EngineCore::assemble_with_identity(
        dir.path(),
        registry(),
        HarnessId::Mock,
        Some(EdgeConfig::with_static_token(http_base, token)),
        &org,
        user,
    )
    .unwrap();
    let restarted_handle = restarted.doc_host.open(&chat).unwrap();
    wait_for(
        || {
            restarted_handle
                .doc()
                .read_commands()
                .unwrap_or_default()
                .iter()
                .any(|command| {
                    command.id == command_id && command.status == SessionCommandStatus::Applied
                })
        },
        "terminal command reconciliation after restart",
    )
    .await;
    restarted.shutdown().await;
}
