//! Pi harness using the official bidirectional RPC mode.

mod normalize;
mod rpc;

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use base64::Engine as _;
use futures::StreamExt;
use futures::stream::BoxStream;
use serde_json::{Value, json};
use tokio::io::AsyncBufReadExt;
use tokio::process::{Child, Command};
use tokio::sync::{mpsc, oneshot};

use jolt_proto::{
    AgentCommand, AgentCommandSource, AgentEvent, CommandContext, DoneStatus, HarnessId, Model,
    ModelOption, ModelOptionChoice, ReasoningLevel, RunRequest, SandboxLevel, SteeringMode,
    UserInputAnswer, UserInputQuestion,
};

use crate::environment::HarnessEnvironment;
use crate::{BashMessage, BashRequest, BashResult, Harness, HarnessError, RunControls};
use rpc::{Incoming, RpcClient};

const REASONING_LEVELS: &[ReasoningLevel] = &[
    ReasoningLevel::Minimal,
    ReasoningLevel::Low,
    ReasoningLevel::Medium,
    ReasoningLevel::High,
    ReasoningLevel::XHigh,
    ReasoningLevel::Max,
];
const MAX_INLINE_IMAGE_BYTES: u64 = 5 * 1024 * 1024;
const PROJECT_TRUST_OPTION: &str = "projectTrust";
const TOOL_ACCESS_OPTION: &str = "toolAccess";

pub struct PiHarness {
    executable: Option<PathBuf>,
    environment: HarnessEnvironment,
    interrupt_grace: Duration,
    kill_grace: Duration,
}

impl Default for PiHarness {
    fn default() -> Self {
        Self {
            executable: None,
            environment: HarnessEnvironment::default(),
            interrupt_grace: Duration::from_secs(2),
            kill_grace: Duration::from_secs(3),
        }
    }
}

impl PiHarness {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_executable(mut self, path: impl Into<PathBuf>) -> Self {
        self.executable = Some(path.into());
        self
    }

    pub fn with_environment(mut self, environment: HarnessEnvironment) -> Self {
        self.environment = environment;
        self
    }

    pub fn with_graces(mut self, interrupt_grace: Duration, kill_grace: Duration) -> Self {
        self.interrupt_grace = interrupt_grace;
        self.kill_grace = kill_grace;
        self
    }

    fn resolve_executable(&self) -> Result<PathBuf, HarnessError> {
        if let Some(path) = &self.executable {
            return Ok(path.clone());
        }
        resolve_pi_executable().ok_or_else(|| {
            HarnessError::NotInstalled(
                "pi (searched PATH, the login shell's PATH, ~/.local/bin, ~/.npm-global/bin, \
                 Homebrew, and fnm/nvm/volta/pnpm/bun install dirs; set \
                 JOLT_PI_EXECUTABLE to override)"
                    .into(),
            )
        })
    }
}

#[async_trait]
impl Harness for PiHarness {
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
        REASONING_LEVELS
    }

    async fn models(&self) -> Result<Vec<Model>, HarnessError> {
        let executable = self.resolve_executable()?;
        let environment = self.environment.resolve(HarnessId::Pi).await?;
        let (mut child, client, mut incoming, _stderr) = spawn_rpc(
            &executable,
            None,
            &environment,
            &[
                "--no-session",
                "--no-extensions",
                "--no-skills",
                "--no-prompt-templates",
                "--no-context-files",
                "--no-approve",
            ],
        )?;
        let incoming_drain = tokio::spawn(async move { while incoming.recv().await.is_some() {} });
        let result = discover_models(&client).await;
        shutdown_child(&mut child, self.kill_grace).await;
        incoming_drain.abort();
        result
    }
    async fn commands(&self, context: CommandContext) -> Result<Vec<AgentCommand>, HarnessError> {
        let executable = self.resolve_executable()?;
        let environment = self.environment.resolve(HarnessId::Pi).await?;
        let mut owned_args = vec!["--no-session".to_string()];
        match command_discovery_trust(&context) {
            Some(true) => owned_args.push("--approve".into()),
            Some(false) => owned_args.push("--no-approve".into()),
            None => {}
        }
        let args: Vec<&str> = owned_args.iter().map(String::as_str).collect();
        let (mut child, client, mut incoming, _stderr) =
            spawn_rpc(&executable, Some(&context.cwd), &environment, &args)?;
        let incoming_drain = tokio::spawn(async move { while incoming.recv().await.is_some() {} });
        let result = discover_commands(&client).await;
        shutdown_child(&mut child, self.kill_grace).await;
        incoming_drain.abort();
        result
    }

    async fn bash(&self, request: BashRequest) -> Result<BashResult, HarnessError> {
        let executable = self.resolve_executable()?;
        let environment = self.environment.resolve(HarnessId::Pi).await?;
        run_bash_process(&executable, &environment, request, self.kill_grace).await
    }

    async fn run(
        &self,
        request: RunRequest,
        controls: RunControls,
    ) -> Result<BoxStream<'static, Result<AgentEvent, HarnessError>>, HarnessError> {
        let executable = self.resolve_executable()?;
        let environment = self.environment.resolve(HarnessId::Pi).await?;
        let (event_tx, event_rx) = mpsc::channel(256);
        tokio::spawn(run_session(Session {
            executable,
            environment,
            request,
            controls,
            event_tx,
            interrupt_grace: self.interrupt_grace,
            kill_grace: self.kill_grace,
        }));
        Ok(
            futures::stream::unfold(event_rx, |mut receiver| async move {
                receiver.recv().await.map(|event| (event, receiver))
            })
            .boxed(),
        )
    }
}

fn resolve_pi_executable() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("JOLT_PI_EXECUTABLE").filter(|path| !path.is_empty()) {
        return Some(PathBuf::from(path));
    }
    let executable = if cfg!(windows) { "pi.exe" } else { "pi" };
    let mut candidates: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|path| {
            std::env::split_paths(&path)
                .filter(|dir| !dir.as_os_str().is_empty())
                .map(|dir| dir.join(executable))
                .collect()
        })
        .unwrap_or_default();
    if let Some(path) = crate::shell_env::login_shell_path() {
        candidates.extend(
            std::env::split_paths(path)
                .filter(|dir| !dir.as_os_str().is_empty())
                .map(|dir| dir.join(executable)),
        );
    }
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        candidates.push(home.join(".local/bin/pi"));
        candidates.push(home.join(".npm-global/bin/pi"));
    }
    candidates.push(PathBuf::from("/opt/homebrew/bin/pi"));
    candidates.push(PathBuf::from("/usr/local/bin/pi"));
    candidates.extend(
        crate::node_version_manager_bins()
            .into_iter()
            .map(|dir| dir.join(executable)),
    );
    candidates.into_iter().find(|path| path.exists())
}

fn spawn_rpc(
    executable: &Path,
    cwd: Option<&str>,
    environment: &[(String, String)],
    extra_args: &[&str],
) -> Result<
    (
        Child,
        RpcClient,
        mpsc::Receiver<Incoming>,
        crate::StderrTail,
    ),
    HarnessError,
> {
    let mut command = Command::new(executable);
    command.args(["--mode", "rpc"]);
    command.args(extra_args);
    crate::compose_child_path(&mut command, executable);
    crate::environment::apply(&mut command, environment);
    if let Some(cwd) = cwd.filter(|cwd| !cwd.is_empty()) {
        command.current_dir(cwd);
    }
    for key in [
        "PI_CODING_AGENT",
        "PI_SESSION_ID",
        "PI_SESSION_FILE",
        "PI_PROVIDER",
        "PI_MODEL",
        "PI_REASONING_LEVEL",
    ] {
        command.env_remove(key);
    }
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = command.spawn().map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            HarnessError::NotInstalled(executable.display().to_string())
        } else {
            HarnessError::Io(error)
        }
    })?;
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| HarnessError::Protocol("Pi child has no stdin".into()))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| HarnessError::Protocol("Pi child has no stdout".into()))?;
    let stderr_tail = crate::StderrTail::default();
    if let Some(stderr) = child.stderr.take() {
        let tail = stderr_tail.clone();
        tokio::spawn(async move {
            let mut lines = tokio::io::BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                tracing::debug!(target: "jolt_harness::pi", "stderr: {line}");
                tail.push(&line);
            }
        });
    }
    let (client, incoming) = RpcClient::new(stdin, stdout);
    Ok((child, client, incoming, stderr_tail))
}

async fn discover_models(client: &RpcClient) -> Result<Vec<Model>, HarnessError> {
    let selected = client
        .request(json!({"type": "get_state"}))
        .await
        .ok()
        .and_then(|state| {
            let model = state.get("model")?;
            Some(format!(
                "{}/{}",
                model.get("provider")?.as_str()?,
                model.get("id")?.as_str()?
            ))
        });
    let response = client
        .request(json!({"type": "get_available_models"}))
        .await?;
    let available = response
        .get("models")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut models = Vec::with_capacity(available.len());
    for model in available {
        let provider = model
            .get("provider")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let id = model.get("id").and_then(Value::as_str).unwrap_or_default();
        if provider.is_empty() || id.is_empty() {
            continue;
        }
        let reasoning_levels = match client
            .request(json!({"type": "set_model", "provider": provider, "modelId": id}))
            .await
        {
            Ok(_) => client
                .request(json!({"type": "get_available_thinking_levels"}))
                .await
                .ok()
                .map(|levels| parse_thinking_levels(&levels))
                .unwrap_or_default(),
            Err(_) => Vec::new(),
        };
        models.push(Model {
            id: format!("{provider}/{id}"),
            label: model
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or(id)
                .to_owned(),
            description: Some(provider.to_owned()),
            reasoning_levels,
            options: vec![project_trust_option(), tool_access_option()],
        });
    }
    if models.is_empty() {
        return Err(HarnessError::Protocol(
            "Pi has no available models; run `pi` and use `/login` to configure a provider".into(),
        ));
    }
    if let Some(selected) = selected
        && let Some(index) = models.iter().position(|model| model.id == selected)
    {
        models[..=index].rotate_right(1);
    }
    Ok(models)
}

async fn discover_commands(client: &RpcClient) -> Result<Vec<AgentCommand>, HarnessError> {
    let response = client.request(json!({"type": "get_commands"})).await?;
    let commands = response
        .get("commands")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
        .iter()
        .filter_map(|command| {
            let name = command.get("name")?.as_str()?.trim_start_matches('/');
            if name.is_empty() {
                return None;
            }
            let source = match command.get("source").and_then(Value::as_str) {
                Some("extension") => AgentCommandSource::Extension,
                Some("skill") => AgentCommandSource::Skill,
                Some("prompt") | Some("prompt-template") => AgentCommandSource::Prompt,
                _ => AgentCommandSource::Harness,
            };
            Some(AgentCommand {
                name: name.to_owned(),
                description: command
                    .get("description")
                    .and_then(Value::as_str)
                    .filter(|value| !value.is_empty())
                    .map(str::to_owned),
                argument_hint: None,
                source,
            })
        })
        .collect();
    Ok(commands)
}

fn command_discovery_trust(context: &CommandContext) -> Option<bool> {
    match context
        .model_options
        .get(PROJECT_TRUST_OPTION)
        .and_then(Value::as_str)
    {
        Some("trust") => return Some(true),
        Some("ignore") => return Some(false),
        _ => {}
    }
    let cwd = Path::new(&context.cwd);
    if context.cwd.is_empty() || !has_trust_requiring_resources(cwd) {
        return None;
    }
    let agent_dir = pi_agent_dir();
    if saved_project_trust(cwd, agent_dir.as_deref()).is_some()
        || !matches!(
            default_project_trust(agent_dir.as_deref()).as_deref(),
            None | Some("ask")
        )
    {
        None
    } else {
        // Autocomplete must not open a trust prompt merely because `/` was typed.
        Some(false)
    }
}

fn parse_thinking_levels(response: &Value) -> Vec<ReasoningLevel> {
    response
        .get("levels")
        .and_then(Value::as_array)
        .map(|levels| levels.as_slice())
        .unwrap_or_default()
        .iter()
        .filter_map(|level| match level.as_str()? {
            "minimal" => Some(ReasoningLevel::Minimal),
            "low" => Some(ReasoningLevel::Low),
            "medium" => Some(ReasoningLevel::Medium),
            "high" => Some(ReasoningLevel::High),
            "xhigh" => Some(ReasoningLevel::XHigh),
            "max" => Some(ReasoningLevel::Max),
            _ => None,
        })
        .collect()
}

fn project_trust_option() -> ModelOption {
    ModelOption {
        id: PROJECT_TRUST_OPTION.into(),
        label: "Project resources".into(),
        choices: vec![
            ModelOptionChoice {
                id: "ask".into(),
                label: "Ask when needed".into(),
            },
            ModelOptionChoice {
                id: "trust".into(),
                label: "Trust for this chat".into(),
            },
            ModelOptionChoice {
                id: "ignore".into(),
                label: "Ignore for this chat".into(),
            },
        ],
        default_choice: "ask".into(),
    }
}

fn tool_access_option() -> ModelOption {
    ModelOption {
        id: TOOL_ACCESS_OPTION.into(),
        label: "Tool access".into(),
        choices: vec![
            ModelOptionChoice {
                id: "full".into(),
                label: "Full local access".into(),
            },
            ModelOptionChoice {
                id: "readOnly".into(),
                label: "Read only".into(),
            },
        ],
        default_choice: "full".into(),
    }
}

struct Session {
    executable: PathBuf,
    environment: Vec<(String, String)>,
    request: RunRequest,
    controls: RunControls,
    event_tx: mpsc::Sender<Result<AgentEvent, HarnessError>>,
    interrupt_grace: Duration,
    kill_grace: Duration,
}

async fn run_session(session: Session) {
    let Session {
        executable,
        environment,
        request,
        controls,
        event_tx,
        interrupt_grace,
        kill_grace,
    } = session;
    let RunControls {
        request_input,
        mut steering,
        mut bash,
        interrupt,
    } = controls;
    let request_input = Arc::new(request_input);

    let trust = tokio::select! {
        trust = project_trust_override(&request, &request_input) => match trust {
            Ok(trust) => trust,
            Err(error) => {
                let status = if interrupt.is_cancelled() {
                    DoneStatus::Interrupted
                } else {
                    DoneStatus::Errored
                };
                let _ = send(&event_tx, AgentEvent::Done {
                    status,
                    result: None,
                    error: (status == DoneStatus::Errored).then_some(error),
                    session_id: None,
                }).await;
                return;
            }
        },
        _ = interrupt.cancelled() => {
            let _ = send(&event_tx, AgentEvent::Done {
                status: DoneStatus::Interrupted,
                result: None,
                error: None,
                session_id: None,
            }).await;
            return;
        }
    };

    let mut owned_args = Vec::<String>::new();
    if let Some(resume) = &request.resume {
        owned_args.extend(["--session-id".into(), resume.clone()]);
    }
    if let Some(model) = &request.model {
        if let Some((provider, model_id)) = model.split_once('/') {
            owned_args.extend(["--provider".into(), provider.into()]);
            owned_args.extend(["--model".into(), model_id.into()]);
        } else {
            owned_args.extend(["--model".into(), model.clone()]);
        }
    }
    if let Some(reasoning) = request.reasoning {
        owned_args.extend(["--thinking".into(), reasoning_name(reasoning).into()]);
    }
    match trust {
        Some(true) => owned_args.push("--approve".into()),
        Some(false) => owned_args.push("--no-approve".into()),
        None => {}
    }
    let read_only = request.sandbox == SandboxLevel::ReadOnly
        || request
            .model_options
            .get(TOOL_ACCESS_OPTION)
            .and_then(Value::as_str)
            == Some("readOnly");
    if read_only {
        owned_args.extend(["--tools".into(), "read,grep,find,ls".into()]);
    } else {
        tracing::warn!(
            target: "jolt_harness::pi",
            requested_sandbox = ?request.sandbox,
            "Pi has no sandbox; running tools with the user's local permissions"
        );
    }
    let args: Vec<&str> = owned_args.iter().map(String::as_str).collect();
    let spawned = spawn_rpc(&executable, Some(&request.cwd), &environment, &args);
    let (mut child, client, mut incoming, stderr_tail) = match spawned {
        Ok(spawned) => spawned,
        Err(error) => {
            let _ = send(
                &event_tx,
                AgentEvent::Done {
                    status: DoneStatus::Errored,
                    result: None,
                    error: Some(error.to_string()),
                    session_id: None,
                },
            )
            .await;
            return;
        }
    };

    let state = tokio::select! {
        state = client.request(json!({"type": "get_state"})) => state,
        _ = interrupt.cancelled() => Err(HarnessError::Protocol("interrupted during Pi startup".into())),
    };
    let state = match state {
        Ok(state) => state,
        Err(error) => {
            let status = if interrupt.is_cancelled() {
                DoneStatus::Interrupted
            } else {
                DoneStatus::Errored
            };
            let _ = send(
                &event_tx,
                AgentEvent::Done {
                    status,
                    result: None,
                    error: (status == DoneStatus::Errored).then(|| error.to_string()),
                    session_id: None,
                },
            )
            .await;
            shutdown_child(&mut child, kill_grace).await;
            return;
        }
    };
    let session_id = state
        .get("sessionId")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let model = state
        .get("model")
        .and_then(|model| {
            Some(format!(
                "{}/{}",
                model.get("provider")?.as_str()?,
                model.get("id")?.as_str()?
            ))
        })
        .or_else(|| request.model.clone())
        .unwrap_or_default();
    let context_window = state
        .get("model")
        .and_then(|model| {
            model
                .get("contextWindow")
                .or_else(|| model.get("context_window"))
        })
        .and_then(Value::as_u64);
    let mut assistant_message_id = new_id();
    if !send(
        &event_tx,
        AgentEvent::SessionStarted {
            harness: HarnessId::Pi,
            model,
            tools: Vec::new(),
            cwd: request.cwd.clone(),
            session_id: session_id.clone(),
            assistant_message_id: assistant_message_id.clone(),
        },
    )
    .await
    {
        shutdown_child(&mut child, kill_grace).await;
        return;
    }

    let extension_commands: std::collections::HashSet<String> = discover_commands(&client)
        .await
        .unwrap_or_default()
        .into_iter()
        .filter(|command| command.source == AgentCommandSource::Extension)
        .map(|command| command.name)
        .collect();

    let images = load_images(&request.attachments).await;
    let initial_events = prompt_with_ui(
        &client,
        &request.prompt,
        &images,
        &mut incoming,
        &request_input,
    )
    .await;
    let initial_events = match initial_events {
        Ok(events) => events,
        Err(error) => {
            let _ = send(
                &event_tx,
                AgentEvent::Done {
                    status: DoneStatus::Errored,
                    result: None,
                    error: Some(error.to_string()),
                    session_id: Some(session_id.clone()),
                },
            )
            .await;
            shutdown_child(&mut child, kill_grace).await;
            return;
        }
    };
    // Extension commands may complete entirely inside the `prompt` RPC and do
    // not emit `agent_settled`. Surface any displayable custom messages and
    // synthesize the terminal event instead of leaving the Jolt run working.
    let initial_extension_command = request
        .prompt
        .strip_prefix('/')
        .and_then(|command| command.split_whitespace().next())
        .is_some_and(|name| extension_commands.contains(name));
    let command_completed_without_agent = initial_extension_command
        && client
            .request(json!({"type": "get_state"}))
            .await
            .ok()
            .and_then(|state| state.get("isStreaming").and_then(Value::as_bool))
            == Some(false);
    if command_completed_without_agent {
        tokio::task::yield_now().await;
        let mut queued = initial_events;
        while let Ok(Incoming::Event(event)) = incoming.try_recv() {
            queued.push(event);
        }
        for event in queued {
            if event.get("type").and_then(Value::as_str) == Some("message_end")
                && let Some(custom) = normalize::custom_message(&event)
                && !send(&event_tx, custom).await
            {
                break;
            }
        }
        let _ = send(
            &event_tx,
            AgentEvent::Done {
                status: DoneStatus::Completed,
                result: None,
                error: None,
                session_id: Some(session_id),
            },
        )
        .await;
        shutdown_child(&mut child, kill_grace).await;
        return;
    }

    for event in initial_events {
        if let Some(custom) = normalize::custom_message(&event)
            && !send(&event_tx, custom).await
        {
            shutdown_child(&mut child, kill_grace).await;
            return;
        }
    }

    type BashCompletion = (
        oneshot::Sender<Result<BashResult, HarnessError>>,
        Result<BashResult, HarnessError>,
    );
    let (bash_done_tx, mut bash_done_rx) = mpsc::channel::<BashCompletion>(1);
    let mut bash_running = false;
    let mut bash_open = true;

    let mut active = true;
    let mut steering_open = true;
    let mut interrupted = false;
    let mut interrupt_sent = false;
    let mut done_current = false;
    let mut pending_error: Option<String> = None;
    let mut escalation: Option<tokio::task::JoinHandle<()>> = None;

    'main: loop {
        tokio::select! {
            incoming_event = incoming.recv() => match incoming_event {
                Some(Incoming::Event(event)) => match event.get("type").and_then(Value::as_str) {
                    Some("agent_start") => {
                        active = true;
                        done_current = false;
                        pending_error = None;
                    }
                    Some("message_update") => {
                        if let Some(error) = normalize::message_error(&event) {
                            pending_error = Some(error);
                        }
                        if let Some(agent_event) = normalize::message_update(&event)
                            && !send(&event_tx, agent_event).await
                        {
                            break 'main;
                        }
                    }
                    Some("message_end") => {
                        if let Some(custom) = normalize::custom_message(&event)
                            && !send(&event_tx, custom).await
                        {
                            break 'main;
                        }
                        if event.get("message").and_then(|message| message.get("role"))
                            .and_then(Value::as_str) == Some("assistant")
                        {
                            if let Some(error) = normalize::message_end_error(&event) {
                                pending_error = Some(error);
                            }
                            if let Some(usage) = normalize::usage(&event, context_window)
                                && !send(&event_tx, usage).await
                            {
                                break 'main;
                            }
                            let previous = std::mem::replace(&mut assistant_message_id, new_id());
                            if !send(&event_tx, AgentEvent::AssistantMessageCompleted {
                                assistant_message_id: previous,
                            }).await {
                                break 'main;
                            }
                        }
                    }
                    Some("tool_execution_start") => {
                        if let Some(agent_event) = normalize::tool_start(&event)
                            && !send(&event_tx, agent_event).await
                        {
                            break 'main;
                        }
                    }
                    Some("tool_execution_end") => {
                        if let Some(agent_event) = normalize::tool_end(&event)
                            && !send(&event_tx, agent_event).await
                        {
                            break 'main;
                        }
                    }
                    Some("compaction_start") => {
                        if !send(&event_tx, AgentEvent::CompactionStarted).await {
                            break 'main;
                        }
                    }
                    Some("compaction_end") => {
                        if !send(&event_tx, AgentEvent::CompactionFinished).await {
                            break 'main;
                        }
                    }
                    Some("extension_ui_request") => {
                        handle_ui_request(&client, &event, &request_input);
                    }
                    Some("agent_settled") => {
                        active = false;
                        done_current = true;
                        let status = if interrupted {
                            DoneStatus::Interrupted
                        } else if pending_error.is_some() {
                            DoneStatus::Errored
                        } else {
                            DoneStatus::Completed
                        };
                        let error = (status == DoneStatus::Errored)
                            .then(|| pending_error.take())
                            .flatten();
                        if !send(&event_tx, AgentEvent::Done {
                            status,
                            result: None,
                            error,
                            session_id: Some(session_id.clone()),
                        }).await {
                            break 'main;
                        }
                        if interrupted || !steering_open {
                            break 'main;
                        }
                    }
                    _ => {}
                },
                Some(Incoming::Eof) | None => break 'main,
            },
            message = bash.recv(), if bash_open && !bash_running && !interrupted => match message {
                Some(BashMessage { request, response }) => {
                    bash_running = true;
                    let client = client.clone();
                    let session_id = session_id.clone();
                    let done = bash_done_tx.clone();
                    tokio::spawn(async move {
                        let result = execute_bash(&client, &request, Some(session_id)).await;
                        let _ = done.send((response, result)).await;
                    });
                }
                None => bash_open = false,
            },
            completion = bash_done_rx.recv(), if bash_running => {
                bash_running = false;
                if let Some((response, result)) = completion {
                    let _ = response.send(result);
                }
            },
            steer = steering.recv(), if steering_open && !interrupted => match steer {
                Some(message) => {
                    let was_active = active;
                    let text = message.prompt;
                    let slash_name = text
                        .strip_prefix('/')
                        .and_then(|command| command.split_whitespace().next());
                    let is_extension_command = slash_name
                        .is_some_and(|name| extension_commands.contains(name));
                    let command = if was_active && !is_extension_command {
                        json!({"type": "steer", "message": text})
                    } else {
                        json!({"type": "prompt", "message": text})
                    };
                    let mut response = client.request(command).await;
                    if let Err(error) = &response
                        && was_active
                        && !is_extension_command
                    {
                        tracing::debug!(target: "jolt_harness::pi", "steer raced with settlement; queuing prompt: {error}");
                        response = client.request(json!({"type": "prompt", "message": text})).await;
                    }
                    match response {
                        Ok(_) => {
                            let command_only_done = is_extension_command
                                && client
                                    .request(json!({"type": "get_state"}))
                                    .await
                                    .ok()
                                    .and_then(|state| {
                                        state.get("isStreaming").and_then(Value::as_bool)
                                    })
                                    == Some(false);
                            active = !command_only_done;
                            done_current = false;
                            let previous = std::mem::replace(&mut assistant_message_id, new_id());
                            if !send(&event_tx, AgentEvent::Steered {
                                assistant_message_id: Some(previous),
                                next_assistant_message_id: Some(assistant_message_id.clone()),
                            }).await {
                                break 'main;
                            }
                            if command_only_done {
                                while let Ok(Incoming::Event(event)) = incoming.try_recv() {
                                    if let Some(custom) = normalize::custom_message(&event)
                                        && !send(&event_tx, custom).await
                                    {
                                        break 'main;
                                    }
                                }
                                done_current = true;
                                if !send(&event_tx, AgentEvent::Done {
                                    status: DoneStatus::Completed,
                                    result: None,
                                    error: None,
                                    session_id: Some(session_id.clone()),
                                }).await {
                                    break 'main;
                                }
                            }
                        }
                        Err(error) => {
                            let _ = send(&event_tx, AgentEvent::Error {
                                message: format!("Steering Pi failed: {error}"),
                            }).await;
                            break 'main;
                        }
                    }
                }
                None => {
                    steering_open = false;
                    if !active {
                        break 'main;
                    }
                }
            },
            _ = interrupt.cancelled(), if !interrupt_sent => {
                interrupt_sent = true;
                interrupted = true;
                if active {
                    let abort_client = client.clone();
                    tokio::spawn(async move {
                        let _ = abort_client.request(json!({"type": "abort"})).await;
                    });
                    if let Some(pid) = child.id() {
                        escalation = Some(tokio::spawn(async move {
                            tokio::time::sleep(interrupt_grace).await;
                            send_signal(pid, Signal::Term);
                            tokio::time::sleep(kill_grace).await;
                            send_signal(pid, Signal::Kill);
                        }));
                    }
                } else {
                    break 'main;
                }
            },
            _ = event_tx.closed() => break 'main,
        }
    }

    if !event_tx.is_closed() && !done_current {
        if interrupted {
            let _ = send(
                &event_tx,
                AgentEvent::Done {
                    status: DoneStatus::Interrupted,
                    result: None,
                    error: None,
                    session_id: Some(session_id),
                },
            )
            .await;
        } else {
            let status = child.try_wait().ok().flatten();
            let _ = send(
                &event_tx,
                AgentEvent::Done {
                    status: DoneStatus::Errored,
                    result: None,
                    error: Some(crate::crash_message("pi --mode rpc", status, &stderr_tail)),
                    session_id: Some(session_id),
                },
            )
            .await;
        }
    }
    shutdown_child(&mut child, kill_grace).await;
    if let Some(escalation) = escalation {
        escalation.abort();
    }
}

async fn run_bash_process(
    executable: &Path,
    environment: &[(String, String)],
    request: BashRequest,
    kill_grace: Duration,
) -> Result<BashResult, HarnessError> {
    let mut owned_args = Vec::new();
    if let Some(resume) = &request.resume {
        owned_args.extend(["--session-id".to_string(), resume.clone()]);
    }
    let context = CommandContext {
        cwd: request.cwd.clone(),
        model_options: request.model_options.clone(),
    };
    match command_discovery_trust(&context) {
        Some(true) => owned_args.push("--approve".into()),
        Some(false) => owned_args.push("--no-approve".into()),
        None => {}
    }
    let args: Vec<&str> = owned_args.iter().map(String::as_str).collect();
    let (mut child, client, mut incoming, _stderr) =
        spawn_rpc(executable, Some(&request.cwd), environment, &args)?;
    let incoming_drain = tokio::spawn(async move { while incoming.recv().await.is_some() {} });
    let result = async {
        let state = client.request(json!({"type": "get_state"})).await?;
        let session_id = state
            .get("sessionId")
            .and_then(Value::as_str)
            .map(str::to_owned);
        execute_bash(&client, &request, session_id).await
    }
    .await;
    shutdown_child(&mut child, kill_grace).await;
    incoming_drain.abort();
    result
}

async fn execute_bash(
    client: &RpcClient,
    request: &BashRequest,
    session_id: Option<String>,
) -> Result<BashResult, HarnessError> {
    let data = client
        .request(json!({
            "type": "bash",
            "command": request.command,
            "excludeFromContext": request.exclude_from_context,
        }))
        .await?;
    let exit_code = data
        .get("exitCode")
        .and_then(Value::as_i64)
        .map(i32::try_from)
        .transpose()
        .map_err(|_| HarnessError::Protocol("bash: exitCode is out of range".into()))?;
    Ok(BashResult {
        output: data
            .get("output")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        exit_code,
        cancelled: data
            .get("cancelled")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        truncated: data
            .get("truncated")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        full_output_path: data
            .get("fullOutputPath")
            .and_then(Value::as_str)
            .map(str::to_owned),
        session_id,
    })
}

async fn prompt(client: &RpcClient, message: &str, images: &[Value]) -> Result<(), HarnessError> {
    let mut command = json!({"type": "prompt", "message": message});
    if !images.is_empty() {
        command
            .as_object_mut()
            .expect("prompt command is an object")
            .insert("images".into(), Value::Array(images.to_vec()));
    }
    client.request(command).await.map(|_| ())
}

async fn prompt_with_ui(
    client: &RpcClient,
    message: &str,
    images: &[Value],
    incoming: &mut mpsc::Receiver<Incoming>,
    request_input: &Arc<RequestInputFn>,
) -> Result<Vec<Value>, HarnessError> {
    let prompt = prompt(client, message, images);
    tokio::pin!(prompt);
    let mut queued = Vec::new();
    loop {
        tokio::select! {
            result = &mut prompt => return result.map(|_| queued),
            event = incoming.recv() => match event {
                Some(Incoming::Event(event))
                    if event.get("type").and_then(Value::as_str)
                        == Some("extension_ui_request") =>
                {
                    handle_ui_request(client, &event, request_input);
                }
                Some(Incoming::Event(event)) => queued.push(event),
                Some(Incoming::Eof) | None => {
                    return Err(HarnessError::Protocol(
                        "Pi exited while handling the initial prompt".into(),
                    ));
                }
            },
        }
    }
}

async fn send(sender: &mpsc::Sender<Result<AgentEvent, HarnessError>>, event: AgentEvent) -> bool {
    sender.send(Ok(event)).await.is_ok()
}

fn new_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

fn reasoning_name(level: ReasoningLevel) -> &'static str {
    match level {
        ReasoningLevel::Minimal => "minimal",
        ReasoningLevel::Low => "low",
        ReasoningLevel::Medium => "medium",
        ReasoningLevel::High => "high",
        ReasoningLevel::XHigh => "xhigh",
        ReasoningLevel::Max
        | ReasoningLevel::Ultra
        | ReasoningLevel::Ultracode
        | ReasoningLevel::Ultrathink => "max",
    }
}

type RequestInputFn = Box<
    dyn Fn(Vec<UserInputQuestion>) -> tokio::sync::oneshot::Receiver<Vec<UserInputAnswer>>
        + Send
        + Sync,
>;

async fn project_trust_override(
    request: &RunRequest,
    request_input: &Arc<RequestInputFn>,
) -> Result<Option<bool>, String> {
    let agent_dir = pi_agent_dir();
    project_trust_override_in(request, request_input, agent_dir.as_deref()).await
}

async fn project_trust_override_in(
    request: &RunRequest,
    request_input: &Arc<RequestInputFn>,
    agent_dir: Option<&Path>,
) -> Result<Option<bool>, String> {
    match request
        .model_options
        .get(PROJECT_TRUST_OPTION)
        .and_then(Value::as_str)
    {
        Some("trust") => return Ok(Some(true)),
        Some("ignore") => return Ok(Some(false)),
        _ => {}
    }
    if request.cwd.is_empty() || !has_trust_requiring_resources(Path::new(&request.cwd)) {
        return Ok(None);
    }
    if saved_project_trust(Path::new(&request.cwd), agent_dir).is_some()
        || !matches!(
            default_project_trust(agent_dir).as_deref(),
            None | Some("ask")
        )
    {
        return Ok(None);
    }
    let question = UserInputQuestion {
        id: new_id(),
        header: "Trust Pi project resources?".into(),
        question: format!(
            "Pi found project-local settings, extensions, skills, or prompts under `{}`. \
             Trusting them may execute local extension code with your user permissions.",
            request.cwd
        ),
        options: vec![
            "Trust this run".into(),
            "Always trust this folder".into(),
            "Continue without project resources".into(),
            "Always ignore this folder".into(),
            "Cancel".into(),
        ],
        multi_select: false,
    };
    let answers = (request_input)(vec![question.clone()])
        .await
        .unwrap_or_default();
    let selected = answers
        .iter()
        .find(|answer| answer.question_id == question.id)
        .and_then(|answer| answer.labels.first())
        .map(String::as_str);
    match selected {
        Some("Trust this run") => Ok(Some(true)),
        Some("Always trust this folder") => {
            if let Err(error) = save_project_trust(Path::new(&request.cwd), agent_dir, true) {
                tracing::warn!(target: "jolt_harness::pi", %error, "could not persist Pi project trust; applying it for this run only");
            }
            Ok(Some(true))
        }
        Some("Continue without project resources") => Ok(Some(false)),
        Some("Always ignore this folder") => {
            if let Err(error) = save_project_trust(Path::new(&request.cwd), agent_dir, false) {
                tracing::warn!(target: "jolt_harness::pi", %error, "could not persist Pi project distrust; applying it for this run only");
            }
            Ok(Some(false))
        }
        _ => Err("Pi run cancelled before project trust was granted".into()),
    }
}

fn pi_agent_dir() -> Option<PathBuf> {
    std::env::var_os("PI_CODING_AGENT_DIR")
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .map(|home| home.join(".pi/agent"))
        })
}

fn canonical(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn saved_project_trust(cwd: &Path, agent_dir: Option<&Path>) -> Option<bool> {
    let file = agent_dir?.join("trust.json");
    let data: serde_json::Map<String, Value> =
        serde_json::from_slice(&std::fs::read(file).ok()?).ok()?;
    let mut current = canonical(cwd);
    loop {
        if let Some(decision) = data.get(&current.to_string_lossy().to_string())
            && let Some(decision) = decision.as_bool()
        {
            return Some(decision);
        }
        if !current.pop() {
            return None;
        }
    }
}

fn default_project_trust(agent_dir: Option<&Path>) -> Option<String> {
    let settings: Value =
        serde_json::from_slice(&std::fs::read(agent_dir?.join("settings.json")).ok()?).ok()?;
    settings
        .get("defaultProjectTrust")
        .and_then(Value::as_str)
        .map(str::to_owned)
}

fn save_project_trust(cwd: &Path, agent_dir: Option<&Path>, trusted: bool) -> Result<(), String> {
    let agent_dir = agent_dir.ok_or_else(|| "Pi agent directory is unavailable".to_owned())?;
    std::fs::create_dir_all(agent_dir)
        .map_err(|error| format!("create {}: {error}", agent_dir.display()))?;
    let file = agent_dir.join("trust.json");
    let lock_path = agent_dir.join("trust.json.lock");
    let lock = (0..10)
        .find_map(|_| match std::fs::create_dir(&lock_path) {
            Ok(()) => Some(TrustFileLock(lock_path.clone())),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                std::thread::sleep(Duration::from_millis(20));
                None
            }
            Err(error) => {
                tracing::warn!(target: "jolt_harness::pi", %error, "Pi trust lock failed");
                None
            }
        })
        .ok_or_else(|| format!("could not acquire {}", lock_path.display()))?;
    let mut data: std::collections::BTreeMap<String, Value> = if file.exists() {
        serde_json::from_slice(&std::fs::read(&file).map_err(|error| error.to_string())?)
            .map_err(|error| format!("parse {}: {error}", file.display()))?
    } else {
        std::collections::BTreeMap::new()
    };
    if data
        .values()
        .any(|value| !value.is_boolean() && !value.is_null())
    {
        return Err(format!(
            "{} contains an invalid trust decision",
            file.display()
        ));
    }
    data.insert(
        canonical(cwd).to_string_lossy().to_string(),
        Value::Bool(trusted),
    );
    let mut json = serde_json::to_string_pretty(&data).map_err(|error| error.to_string())?;
    json.push('\n');
    std::fs::write(&file, json).map_err(|error| format!("write {}: {error}", file.display()))?;
    drop(lock);
    Ok(())
}

struct TrustFileLock(PathBuf);

impl Drop for TrustFileLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir(&self.0);
    }
}

fn has_trust_requiring_resources(cwd: &Path) -> bool {
    const PI_RESOURCES: &[&str] = &[
        "settings.json",
        "extensions",
        "skills",
        "prompts",
        "themes",
        "SYSTEM.md",
        "APPEND_SYSTEM.md",
    ];
    let cwd = canonical(cwd);
    if PI_RESOURCES
        .iter()
        .any(|resource| cwd.join(".pi").join(resource).exists())
    {
        return true;
    }
    let user_skills = std::env::var_os("HOME")
        .map(PathBuf::from)
        .map(|home| canonical(&home.join(".agents/skills")));
    let mut current = cwd;
    loop {
        let skills = canonical(&current.join(".agents/skills"));
        if skills.exists() && user_skills.as_ref() != Some(&skills) {
            return true;
        }
        if !current.pop() {
            return false;
        }
    }
}

fn handle_ui_request(client: &RpcClient, event: &Value, request_input: &Arc<RequestInputFn>) {
    let method = event
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if !matches!(method, "select" | "confirm" | "input" | "editor") {
        return;
    }
    let Some(id) = event.get("id").cloned() else {
        return;
    };
    let options = match method {
        "select" => event
            .get("options")
            .and_then(Value::as_array)
            .map(|options| {
                options
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default(),
        "confirm" => vec!["Yes".into(), "No".into()],
        _ => Vec::new(),
    };
    let question = UserInputQuestion {
        id: new_id(),
        header: event
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or("Pi extension")
            .to_owned(),
        question: event
            .get("message")
            .or_else(|| event.get("placeholder"))
            .and_then(Value::as_str)
            .unwrap_or("Enter a response")
            .to_owned(),
        options,
        multi_select: false,
    };
    let method = method.to_owned();
    let client = client.clone();
    let request_input = Arc::clone(request_input);
    tokio::spawn(async move {
        let answers = (request_input)(vec![question.clone()])
            .await
            .unwrap_or_default();
        let answer = answers
            .iter()
            .find(|answer| answer.question_id == question.id)
            .and_then(|answer| answer.labels.first())
            .cloned();
        let mut response = json!({"type": "extension_ui_response", "id": id});
        let object = response
            .as_object_mut()
            .expect("extension response is an object");
        match (method.as_str(), answer) {
            ("confirm", Some(answer)) => {
                object.insert(
                    "confirmed".into(),
                    Value::Bool(answer.eq_ignore_ascii_case("yes")),
                );
            }
            (_, Some(answer)) => {
                object.insert("value".into(), Value::String(answer));
            }
            _ => {
                object.insert("cancelled".into(), Value::Bool(true));
            }
        }
        client.send(response);
    });
}

fn image_media_type(path: &Path, bytes: &[u8]) -> Option<&'static str> {
    let by_extension = match path
        .extension()
        .and_then(|extension| extension.to_str())
        .map(|extension| extension.to_ascii_lowercase())
        .as_deref()
    {
        Some("png") => Some("image/png"),
        Some("jpg" | "jpeg") => Some("image/jpeg"),
        Some("gif") => Some("image/gif"),
        Some("webp") => Some("image/webp"),
        _ => None,
    };
    by_extension.or(match bytes {
        [0x89, b'P', b'N', b'G', ..] => Some("image/png"),
        [0xFF, 0xD8, 0xFF, ..] => Some("image/jpeg"),
        [b'G', b'I', b'F', b'8', ..] => Some("image/gif"),
        [
            b'R',
            b'I',
            b'F',
            b'F',
            _,
            _,
            _,
            _,
            b'W',
            b'E',
            b'B',
            b'P',
            ..,
        ] => Some("image/webp"),
        _ => None,
    })
}

async fn load_images(paths: &[String]) -> Vec<Value> {
    let mut images = Vec::new();
    for path in paths {
        let Ok(bytes) = tokio::fs::read(path).await else {
            tracing::warn!(target: "jolt_harness::pi", %path, "attachment unreadable; path ref only");
            continue;
        };
        if bytes.len() as u64 > MAX_INLINE_IMAGE_BYTES {
            continue;
        }
        let Some(mime_type) = image_media_type(Path::new(path), &bytes) else {
            continue;
        };
        images.push(json!({
            "data": base64::engine::general_purpose::STANDARD.encode(bytes),
            "mimeType": mime_type,
        }));
    }
    images
}

async fn shutdown_child(child: &mut Child, grace: Duration) {
    if matches!(child.try_wait(), Ok(Some(_))) {
        return;
    }
    if let Some(pid) = child.id() {
        send_signal(pid, Signal::Term);
        if tokio::time::timeout(grace, child.wait()).await.is_ok() {
            return;
        }
    }
    let _ = child.start_kill();
    let _ = child.wait().await;
}

#[derive(Clone, Copy)]
enum Signal {
    Term,
    Kill,
}

#[cfg(unix)]
fn send_signal(pid: u32, signal: Signal) {
    let signal = match signal {
        Signal::Term => libc::SIGTERM,
        Signal::Kill => libc::SIGKILL,
    };
    // SAFETY: `pid` belongs to a child process this harness spawned and has not reaped.
    unsafe {
        libc::kill(pid as libc::pid_t, signal);
    }
}

#[cfg(not(unix))]
fn send_signal(_pid: u32, _signal: Signal) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thinking_levels_drop_off_and_unknown_values() {
        assert_eq!(
            parse_thinking_levels(&json!({
                "levels": ["off", "minimal", "low", "medium", "high", "xhigh", "max"]
            })),
            REASONING_LEVELS
        );
    }

    #[tokio::test]
    async fn unresolved_project_trust_uses_the_input_bridge() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use tokio::sync::oneshot;

        let directory = tempfile::tempdir().unwrap();
        let agent_dir = directory.path().join("agent");
        let project = directory.path().join("project");
        std::fs::create_dir_all(project.join(".pi/extensions")).unwrap();
        let asked = Arc::new(AtomicBool::new(false));
        let saw_question = Arc::clone(&asked);
        let request_input: Arc<RequestInputFn> = Arc::new(Box::new(move |questions| {
            saw_question.store(true, Ordering::SeqCst);
            assert_eq!(questions[0].header, "Trust Pi project resources?");
            let (sender, receiver) = oneshot::channel();
            sender
                .send(vec![UserInputAnswer {
                    question_id: questions[0].id.clone(),
                    labels: vec!["Always trust this folder".into()],
                }])
                .unwrap();
            receiver
        }));
        let request = RunRequest {
            prompt: "test".into(),
            model: None,
            reasoning: None,
            model_options: serde_json::Map::new(),
            cwd: project.display().to_string(),
            sandbox: SandboxLevel::WorkspaceWrite,
            auto_approve: false,
            resume: None,
            attachments: Vec::new(),
        };
        assert_eq!(
            project_trust_override_in(&request, &request_input, Some(&agent_dir)).await,
            Ok(Some(true))
        );
        assert!(asked.load(Ordering::SeqCst));
        assert_eq!(saved_project_trust(&project, Some(&agent_dir)), Some(true));
    }

    #[test]
    fn project_resource_detection_matches_pi_boundaries() {
        let directory = tempfile::tempdir().unwrap();
        let project = directory.path().join("project");
        std::fs::create_dir_all(project.join(".pi/extensions")).unwrap();
        assert!(has_trust_requiring_resources(&project));
        std::fs::remove_dir_all(project.join(".pi")).unwrap();
        assert!(!has_trust_requiring_resources(&project));
        std::fs::create_dir_all(directory.path().join(".agents/skills/example")).unwrap();
        assert!(has_trust_requiring_resources(&project));
    }
}
