//! Product-owned MCP host injected into supported harness subprocesses.
//!
//! The listener is loopback-only and starts lazily. Each live harness process
//! receives an in-memory, chat-scoped bearer lease; neither endpoint
//! configuration nor credentials enter the synchronized run protocol.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};
use std::time::Duration;

use axum::extract::{Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, ErrorCode,
    Implementation, InitializeResult, JsonObject, ListToolsResult, PaginatedRequestParams,
    ServerCapabilities, ServerInfo, Tool, ToolAnnotations,
};
use rmcp::schemars::{self, JsonSchema};
use rmcp::service::{RequestContext, RoleServer};
use rmcp::transport::StreamableHttpService;
use rmcp::transport::streamable_http_server::StreamableHttpServerConfig;
use rmcp::transport::streamable_http_server::session::never::NeverSessionManager;
use rmcp::{ErrorData, ServerHandler};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::task::JoinHandle;

use jolt_harness::{CancellationToken, McpServerConfig};
use jolt_proto::{Goal, GoalStatus, UserInputAnswer, UserInputQuestion};

use crate::goals::{self, AgentGoalAction};
use crate::workspace_host::WorkspaceHost;
use crate::{EngineError, new_id};

const SERVER_NAME: &str = "jolt";
const GOAL_GET: &str = "goal_get";
const GOAL_UPDATE: &str = "goal_update";
const GOAL_COMPLETE: &str = "goal_complete";
const GOAL_REPORT_BLOCKED: &str = "goal_report_blocked";
const GOAL_PAUSE: &str = "goal_pause";
const GOAL_RESUME: &str = "goal_resume";
const REQUEST_ANSWERS: &str = "request_answers";

const MAX_ANSWER_QUESTIONS: usize = 24;
const MAX_ANSWER_HEADER_CHARS: usize = 120;
const MAX_ANSWER_QUESTION_CHARS: usize = 4_000;
const MAX_ANSWER_OPTIONS: usize = 20;
const MAX_ANSWER_OPTION_CHARS: usize = 500;

const TOOL_NAMES: &[&str] = &[
    GOAL_GET,
    GOAL_UPDATE,
    GOAL_COMPLETE,
    GOAL_REPORT_BLOCKED,
    GOAL_PAUSE,
    GOAL_RESUME,
    REQUEST_ANSWERS,
];

type TokenDigest = [u8; 32];
pub(crate) type McpAnswerRequester = Arc<
    dyn Fn(
            Vec<UserInputQuestion>,
            CancellationToken,
        ) -> futures::future::BoxFuture<'static, Option<Vec<UserInputAnswer>>>
        + Send
        + Sync,
>;

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn token_digest(token: &str) -> TokenDigest {
    Sha256::digest(token.as_bytes()).into()
}

#[derive(Clone)]
struct McpRunTarget {
    chat_id: Arc<str>,
    workspace: Option<WorkspaceHost>,
    goal_signal: Arc<Mutex<Option<McpGoalSignal>>>,
    answer_requester: Option<McpAnswerRequester>,
    active: Arc<AtomicBool>,
}

impl McpRunTarget {
    fn ensure_active(&self) -> Result<(), EngineError> {
        if !self.active.load(Ordering::Acquire) {
            return Err(EngineError::Other("the MCP run lease has ended".into()));
        }
        Ok(())
    }

    fn workspace(&self) -> Result<&WorkspaceHost, EngineError> {
        self.ensure_active()?;
        self.workspace
            .as_ref()
            .ok_or_else(|| EngineError::Other("workspace registry is unavailable".into()))
    }

    fn answer_requester(&self) -> Result<McpAnswerRequester, EngineError> {
        self.ensure_active()?;
        self.answer_requester
            .clone()
            .ok_or_else(|| EngineError::Other("the Jolt answer UI is unavailable".into()))
    }

    fn clear_signal(&self) {
        *lock(&self.goal_signal) = None;
    }
}

#[derive(Clone, Debug)]
pub(crate) enum McpGoalSignal {
    Blocked {
        goal_id: String,
        expected_revision: u64,
        blocker_key: String,
        summary: String,
    },
}

#[derive(Default)]
struct TokenRegistry {
    active: Mutex<HashMap<TokenDigest, McpRunTarget>>,
}

impl TokenRegistry {
    fn insert(&self, token: &str, target: McpRunTarget) -> TokenDigest {
        let digest = token_digest(token);
        lock(&self.active).insert(digest, target);
        digest
    }

    fn target(&self, token: &str) -> Option<McpRunTarget> {
        lock(&self.active).get(&token_digest(token)).cloned()
    }

    fn remove(&self, digest: &TokenDigest) {
        if let Some(target) = lock(&self.active).remove(digest) {
            target.active.store(false, Ordering::Release);
        }
    }

    fn clear(&self) {
        for target in lock(&self.active).drain().map(|(_, target)| target) {
            target.active.store(false, Ordering::Release);
        }
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct GoalGetParams {}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct GoalRefParams {
    goal_id: String,
    expected_revision: u64,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct GoalSummaryParams {
    goal_id: String,
    expected_revision: u64,
    summary: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct GoalPauseParams {
    goal_id: String,
    expected_revision: u64,
    reason: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct GoalBlockedParams {
    goal_id: String,
    expected_revision: u64,
    blocker_key: String,
    summary: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RequestAnswersParams {
    questions: Vec<AnswerQuestionParams>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct AnswerQuestionParams {
    #[serde(default)]
    header: Option<String>,
    question: String,
    #[serde(default)]
    options: Vec<String>,
    #[serde(default)]
    multi_select: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AnswerToolResult {
    answers: Vec<AnswerToolEntry>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AnswerToolEntry {
    question: String,
    labels: Vec<String>,
}

#[derive(Clone, Default)]
struct JoltMcpServer;

impl JoltMcpServer {
    fn tool(name: &str) -> Option<Tool> {
        let mut annotations = ToolAnnotations::new();
        annotations.read_only_hint = Some(name == GOAL_GET);
        annotations.destructive_hint = Some(false);
        annotations.idempotent_hint = Some(name == GOAL_GET);
        annotations.open_world_hint = Some(false);
        let tool = match name {
            GOAL_GET => Tool::new(
                GOAL_GET,
                "Read the current Jolt goal and its revision, status, budget, and usage. Call this before mutating a goal when its revision may have changed.",
                JsonObject::new(),
            )
            .with_input_schema::<GoalGetParams>(),
            GOAL_UPDATE => Tool::new(
                GOAL_UPDATE,
                "Record concise concrete progress on the active Jolt goal without changing its objective. Use the returned revision for any later goal mutation.",
                JsonObject::new(),
            )
            .with_input_schema::<GoalSummaryParams>(),
            GOAL_COMPLETE => Tool::new(
                GOAL_COMPLETE,
                "Complete the active Jolt goal only after every requirement is achieved and verified against authoritative current state.",
                JsonObject::new(),
            )
            .with_input_schema::<GoalSummaryParams>(),
            GOAL_REPORT_BLOCKED => Tool::new(
                GOAL_REPORT_BLOCKED,
                "Report that this goal turn cannot make meaningful progress without user input or an external-state change. Reuse the same stable blockerKey across turns; Jolt blocks after three consecutive matching reports.",
                JsonObject::new(),
            )
            .with_input_schema::<GoalBlockedParams>(),
            GOAL_PAUSE => Tool::new(
                GOAL_PAUSE,
                "Pause the active Jolt goal only when autonomous work should intentionally stop. A user-paused goal cannot be resumed by an agent.",
                JsonObject::new(),
            )
            .with_input_schema::<GoalPauseParams>(),
            GOAL_RESUME => Tool::new(
                GOAL_RESUME,
                "Resume only an agent-paused or blocked Jolt goal. User-, system-, usage-, and budget-paused goals require user action.",
                JsonObject::new(),
            )
            .with_input_schema::<GoalRefParams>(),
            REQUEST_ANSWERS => Tool::new(
                REQUEST_ANSWERS,
                "Ask one or more questions through Jolt's answer UI and wait for the user's responses. Use options for choices or omit them for typed answers. Prefer this over asking answerable questions only in prose.",
                JsonObject::new(),
            )
            .with_input_schema::<RequestAnswersParams>(),
            _ => return None,
        };
        Some(tool.with_annotations(annotations))
    }

    fn target(context: &RequestContext<RoleServer>) -> Result<McpRunTarget, EngineError> {
        let parts = context
            .extensions
            .get::<axum::http::request::Parts>()
            .ok_or_else(|| EngineError::Other("MCP request context is unavailable".into()))?;
        parts
            .extensions
            .get::<McpRunTarget>()
            .cloned()
            .ok_or_else(|| EngineError::Other("MCP run context is unavailable".into()))
    }

    fn parameters<T: DeserializeOwned>(arguments: Option<JsonObject>) -> Result<T, EngineError> {
        serde_json::from_value(serde_json::Value::Object(arguments.unwrap_or_default()))
            .map_err(|error| EngineError::Other(format!("invalid Jolt tool arguments: {error}")))
    }

    fn mutate(
        target: &McpRunTarget,
        params: GoalRefParams,
        action: AgentGoalAction,
    ) -> Result<Goal, EngineError> {
        let workspace = target.workspace()?;
        let next = workspace.mutate_chat_goal(&target.chat_id, |current| {
            goals::apply_agent_action(current, &params.goal_id, params.expected_revision, action)
                .map(Some)
        })?;
        next.ok_or_else(|| EngineError::Other("this session has no goal".into()))
    }

    fn success(goal: &Goal) -> CallToolResponse {
        CallToolResult::structured(
            serde_json::to_value(goal).expect("Goal serialization is infallible"),
        )
        .into()
    }

    fn failure(error: EngineError) -> CallToolResponse {
        CallToolResult::error(vec![ContentBlock::text(error.to_string())]).into()
    }

    fn answer_questions(
        params: RequestAnswersParams,
    ) -> Result<Vec<UserInputQuestion>, EngineError> {
        if params.questions.is_empty() {
            return Err(EngineError::Other(
                "request_answers requires at least one question".into(),
            ));
        }
        if params.questions.len() > MAX_ANSWER_QUESTIONS {
            return Err(EngineError::Other(format!(
                "request_answers accepts at most {MAX_ANSWER_QUESTIONS} questions"
            )));
        }
        params
            .questions
            .into_iter()
            .map(|question| {
                let text = Self::answer_text(
                    &question.question,
                    "answer question",
                    MAX_ANSWER_QUESTION_CHARS,
                )?;
                let header = question
                    .header
                    .as_deref()
                    .map(|header| {
                        Self::answer_text(header, "answer header", MAX_ANSWER_HEADER_CHARS)
                    })
                    .transpose()?
                    .unwrap_or_else(|| "Question".into());
                if question.options.len() > MAX_ANSWER_OPTIONS {
                    return Err(EngineError::Other(format!(
                        "an answer question accepts at most {MAX_ANSWER_OPTIONS} options"
                    )));
                }
                let options: Vec<String> = question
                    .options
                    .iter()
                    .map(|option| {
                        Self::answer_text(option, "answer option", MAX_ANSWER_OPTION_CHARS)
                    })
                    .collect::<Result<_, _>>()?;
                if question.multi_select && options.is_empty() {
                    return Err(EngineError::Other(
                        "multiSelect requires at least one option".into(),
                    ));
                }
                let mut unique = std::collections::HashSet::new();
                if !options.iter().all(|option| unique.insert(option)) {
                    return Err(EngineError::Other(
                        "answer options must be unique within a question".into(),
                    ));
                }
                Ok(UserInputQuestion {
                    id: new_id(),
                    header,
                    question: text,
                    options,
                    multi_select: question.multi_select,
                })
            })
            .collect()
    }

    fn answer_text(value: &str, label: &str, max_chars: usize) -> Result<String, EngineError> {
        let value = value.trim();
        if value.is_empty() {
            return Err(EngineError::Other(format!("{label} must not be empty")));
        }
        if value.chars().count() > max_chars {
            return Err(EngineError::Other(format!(
                "{label} exceeds the {max_chars} character limit"
            )));
        }
        Ok(value.to_string())
    }

    async fn call_answer_tool(
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let result = async {
            let target = Self::target(&context)?;
            let requester = target.answer_requester()?;
            let params = Self::parameters::<RequestAnswersParams>(request.arguments)?;
            let questions = Self::answer_questions(params)?;
            let answers = requester(questions.clone(), context.ct.clone())
                .await
                .ok_or_else(|| EngineError::Other("the answer request was cancelled".into()))?;
            let answers = questions
                .into_iter()
                .map(|question| AnswerToolEntry {
                    labels: answers
                        .iter()
                        .find(|answer| answer.question_id == question.id)
                        .map(|answer| answer.labels.clone())
                        .unwrap_or_default(),
                    question: question.question,
                })
                .collect();
            Ok::<_, EngineError>(
                CallToolResult::structured(
                    serde_json::to_value(AnswerToolResult { answers })
                        .expect("answer result serialization is infallible"),
                )
                .into(),
            )
        }
        .await;
        Ok(result.unwrap_or_else(Self::failure))
    }

    fn call_goal_tool(
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let target = match Self::target(&context) {
            Ok(target) => target,
            Err(error) => return Ok(Self::failure(error)),
        };
        let result = match request.name.as_ref() {
            GOAL_GET => Self::parameters::<GoalGetParams>(request.arguments).and_then(|_| {
                target
                    .workspace()
                    .and_then(|workspace| {
                        workspace
                            .chat_goal(&target.chat_id)
                            .ok_or_else(|| EngineError::Other("this session has no goal".into()))
                    })
                    .map(|goal| Self::success(&goal))
            }),
            GOAL_UPDATE => {
                Self::parameters::<GoalSummaryParams>(request.arguments).and_then(|params| {
                    target.clear_signal();
                    Self::mutate(
                        &target,
                        GoalRefParams {
                            goal_id: params.goal_id,
                            expected_revision: params.expected_revision,
                        },
                        AgentGoalAction::Update {
                            summary: params.summary,
                        },
                    )
                    .map(|goal| Self::success(&goal))
                })
            }
            GOAL_COMPLETE => {
                Self::parameters::<GoalSummaryParams>(request.arguments).and_then(|params| {
                    target.clear_signal();
                    Self::mutate(
                        &target,
                        GoalRefParams {
                            goal_id: params.goal_id,
                            expected_revision: params.expected_revision,
                        },
                        AgentGoalAction::Complete {
                            summary: params.summary,
                        },
                    )
                    .map(|goal| Self::success(&goal))
                })
            }
            GOAL_PAUSE => {
                Self::parameters::<GoalPauseParams>(request.arguments).and_then(|params| {
                    target.clear_signal();
                    Self::mutate(
                        &target,
                        GoalRefParams {
                            goal_id: params.goal_id,
                            expected_revision: params.expected_revision,
                        },
                        AgentGoalAction::Pause {
                            reason: params.reason,
                        },
                    )
                    .map(|goal| Self::success(&goal))
                })
            }
            GOAL_RESUME => {
                Self::parameters::<GoalRefParams>(request.arguments).and_then(|params| {
                    target.clear_signal();
                    Self::mutate(&target, params, AgentGoalAction::Resume)
                        .map(|goal| Self::success(&goal))
                })
            }
            GOAL_REPORT_BLOCKED => Self::parameters::<GoalBlockedParams>(request.arguments)
                .and_then(|params| {
                    let key = goals::validate_blocker_key(&params.blocker_key)?;
                    let summary = goals::validate_blocker_summary(&params.summary)?;
                    let goal = target
                        .workspace()?
                        .chat_goal(&target.chat_id)
                        .ok_or_else(|| EngineError::Other("this session has no goal".into()))?;
                    if goal.id != params.goal_id || goal.revision != params.expected_revision {
                        return Err(EngineError::Other(
                            "the goal changed before this command applied".into(),
                        ));
                    }
                    if goal.status != GoalStatus::Active {
                        return Err(EngineError::Other("the goal is not active".into()));
                    }
                    *lock(&target.goal_signal) = Some(McpGoalSignal::Blocked {
                        goal_id: goal.id.clone(),
                        expected_revision: goal.revision,
                        blocker_key: key,
                        summary,
                    });
                    Ok(CallToolResult::structured(serde_json::json!({
                        "accepted": true,
                        "goal": goal,
                        "message": "The blocker report will be applied when this goal turn ends."
                    }))
                    .into())
                }),
            _ => {
                return Err(ErrorData::new(
                    ErrorCode::METHOD_NOT_FOUND,
                    format!("unknown Jolt tool: {}", request.name),
                    None,
                ));
            }
        };
        Ok(result.unwrap_or_else(Self::failure))
    }
}

impl ServerHandler for JoltMcpServer {
    fn get_info(&self) -> ServerInfo {
        InitializeResult::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(
                Implementation::new(SERVER_NAME, env!("CARGO_PKG_VERSION")).with_title("Jolt"),
            )
    }

    fn get_tool(&self, name: &str) -> Option<Tool> {
        Self::tool(name)
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        Ok(ListToolsResult {
            tools: TOOL_NAMES
                .iter()
                .filter_map(|name| Self::tool(name))
                .collect(),
            ..Default::default()
        })
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        if request.name.as_ref() == REQUEST_ANSWERS {
            Self::call_answer_tool(request, context).await
        } else {
            Self::call_goal_tool(request, context)
        }
    }
}

struct RunningServer {
    endpoint: String,
    task: JoinHandle<()>,
}

struct McpHostInner {
    tokens: Arc<TokenRegistry>,
    running: tokio::sync::Mutex<Option<RunningServer>>,
    cancellation: CancellationToken,
}

impl Drop for McpHostInner {
    fn drop(&mut self) {
        self.cancellation.cancel();
    }
}

/// Identity-scoped owner of Jolt's loopback MCP listener.
pub(crate) struct McpHost {
    inner: Arc<McpHostInner>,
}

impl McpHost {
    pub(crate) fn new() -> Self {
        Self {
            inner: Arc::new(McpHostInner {
                tokens: Arc::new(TokenRegistry::default()),
                running: tokio::sync::Mutex::new(None),
                cancellation: CancellationToken::new(),
            }),
        }
    }

    /// Start the listener when necessary and issue one process-lifetime lease.
    pub(crate) async fn lease(
        &self,
        chat_id: String,
        workspace: Option<WorkspaceHost>,
        answer_requester: Option<McpAnswerRequester>,
    ) -> Result<McpLease, std::io::Error> {
        let endpoint = self.endpoint().await?;
        let token = format!(
            "{}{}",
            uuid::Uuid::new_v4().simple(),
            uuid::Uuid::new_v4().simple()
        );
        let goal_signal = Arc::new(Mutex::new(None));
        let active = Arc::new(AtomicBool::new(true));
        let digest = self.inner.tokens.insert(
            &token,
            McpRunTarget {
                chat_id: chat_id.into(),
                workspace,
                goal_signal: goal_signal.clone(),
                answer_requester,
                active: active.clone(),
            },
        );
        Ok(McpLease {
            config: McpServerConfig {
                name: SERVER_NAME.into(),
                url: endpoint,
                bearer_token: token,
            },
            digest,
            registry: Arc::downgrade(&self.inner.tokens),
            goal_signal,
            active,
        })
    }

    async fn endpoint(&self) -> Result<String, std::io::Error> {
        let mut running = self.inner.running.lock().await;
        if let Some(server) = running.as_ref() {
            return Ok(server.endpoint.clone());
        }

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let endpoint = format!("http://{address}/mcp");
        let cancellation = self.inner.cancellation.child_token();
        let config = StreamableHttpServerConfig::default()
            .with_legacy_session_mode(false)
            .with_json_response(true)
            .with_cancellation_token(cancellation.clone())
            .with_allowed_hosts([format!("127.0.0.1:{}", address.port())]);
        let service = StreamableHttpService::new(
            || Ok(JoltMcpServer),
            Arc::new(NeverSessionManager::default()),
            config,
        );
        let router = axum::Router::new().nest_service("/mcp", service).layer(
            middleware::from_fn_with_state(self.inner.tokens.clone(), authenticate),
        );
        let task = tokio::spawn(async move {
            if let Err(error) = axum::serve(listener, router)
                .with_graceful_shutdown(cancellation.cancelled_owned())
                .await
            {
                tracing::warn!(%error, "MCP listener stopped unexpectedly");
            }
        });
        *running = Some(RunningServer {
            endpoint: endpoint.clone(),
            task,
        });
        Ok(endpoint)
    }

    pub(crate) async fn shutdown(&self) {
        self.inner.tokens.clear();
        self.inner.cancellation.cancel();
        let Some(mut running) = self.inner.running.lock().await.take() else {
            return;
        };
        if tokio::time::timeout(Duration::from_secs(2), &mut running.task)
            .await
            .is_err()
        {
            running.task.abort();
            let _ = running.task.await;
        }
    }
}

/// Keeps one bearer credential authorized for the lifetime of a live run.
pub(crate) struct McpLease {
    config: McpServerConfig,
    digest: TokenDigest,
    registry: Weak<TokenRegistry>,
    goal_signal: Arc<Mutex<Option<McpGoalSignal>>>,
    active: Arc<AtomicBool>,
}

impl McpLease {
    pub(crate) fn config(&self) -> McpServerConfig {
        self.config.clone()
    }

    pub(crate) fn take_goal_signal(&self) -> Option<McpGoalSignal> {
        lock(&self.goal_signal).take()
    }
}

impl Drop for McpLease {
    fn drop(&mut self) {
        self.active.store(false, Ordering::Release);
        if let Some(registry) = self.registry.upgrade() {
            registry.remove(&self.digest);
        }
    }
}

async fn authenticate(
    State(tokens): State<Arc<TokenRegistry>>,
    mut request: Request,
    next: Next,
) -> Response {
    if request.headers().contains_key(header::ORIGIN) {
        return StatusCode::FORBIDDEN.into_response();
    }
    let target = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .filter(|token| !token.is_empty())
        .and_then(|token| tokens.target(token));
    let Some(target) = target else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    if !target.active.load(Ordering::Acquire) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    request.extensions_mut().insert(target);
    next.run(request).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace_host::WorkspaceHostConfig;

    async fn request(url: &str, token: Option<&str>, body: serde_json::Value) -> reqwest::Response {
        let client = reqwest::Client::new();
        let mut request = client
            .post(url)
            .header(
                header::ACCEPT.as_str(),
                "application/json, text/event-stream",
            )
            .header(header::CONTENT_TYPE.as_str(), "application/json")
            .header("MCP-Protocol-Version", "2025-03-26")
            .json(&body);
        if let Some(token) = token {
            request = request.bearer_auth(token);
        }
        request.send().await.unwrap()
    }

    fn test_workspace() -> (tempfile::TempDir, WorkspaceHost) {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(jolt_sync::DocsStore::open(dir.path()).unwrap());
        let workspace = WorkspaceHost::open(
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
        workspace
            .create_chat("chat-2", "space-1", None, None)
            .unwrap();
        (dir, workspace)
    }

    fn initialize_body() -> serde_json::Value {
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-03-26",
                "capabilities": {},
                "clientInfo": { "name": "jolt-test", "version": "1" }
            }
        })
    }

    #[tokio::test]
    async fn lease_authenticates_tool_server_and_revokes_on_drop() {
        let host = McpHost::new();
        let lease = host.lease("chat-1".into(), None, None).await.unwrap();

        let missing = request(&lease.config.url, None, initialize_body()).await;
        assert_eq!(missing.status(), StatusCode::UNAUTHORIZED);

        let response = request(
            &lease.config.url,
            Some(&lease.config.bearer_token),
            initialize_body(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let body: serde_json::Value = response.json().await.unwrap();
        assert_eq!(body["result"]["serverInfo"]["name"], SERVER_NAME);
        assert!(body["result"]["capabilities"]["tools"].is_object());

        let listed = request(
            &lease.config.url,
            Some(&lease.config.bearer_token),
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "tools/list",
                "params": {}
            }),
        )
        .await;
        assert_eq!(listed.status(), StatusCode::OK);
        let body: serde_json::Value = listed.json().await.unwrap();
        let names: Vec<_> = body["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|tool| tool["name"].as_str())
            .collect();
        assert_eq!(names, TOOL_NAMES);

        let unscoped = request(
            &lease.config.url,
            Some(&lease.config.bearer_token),
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 3,
                "method": "tools/call",
                "params": { "name": GOAL_GET, "arguments": {} }
            }),
        )
        .await;
        assert_eq!(unscoped.status(), StatusCode::OK);
        let body: serde_json::Value = unscoped.json().await.unwrap();
        assert_eq!(body["result"]["isError"], true);
        assert!(
            body["result"]["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("workspace registry is unavailable")
        );

        let url = lease.config.url.clone();
        let token = lease.config.bearer_token.clone();
        drop(lease);
        let revoked = request(&url, Some(&token), initialize_body()).await;
        assert_eq!(revoked.status(), StatusCode::UNAUTHORIZED);

        host.shutdown().await;
    }

    #[tokio::test]
    async fn goal_tools_mutate_only_the_leased_chat_and_reject_stale_revisions() {
        let (_dir, workspace) = test_workspace();
        let first = goals::apply_operation(
            None,
            &jolt_doc::GoalOperation::Create {
                objective: "Ship chat one".into(),
                token_budget: None,
            },
        )
        .unwrap()
        .unwrap();
        let second = goals::apply_operation(
            None,
            &jolt_doc::GoalOperation::Create {
                objective: "Ship chat two".into(),
                token_budget: None,
            },
        )
        .unwrap()
        .unwrap();
        workspace.set_chat_goal("chat-1", Some(&first)).unwrap();
        workspace.set_chat_goal("chat-2", Some(&second)).unwrap();

        let host = McpHost::new();
        let lease = host
            .lease("chat-1".into(), Some(workspace.clone()), None)
            .await
            .unwrap();

        let updated = request(
            &lease.config.url,
            Some(&lease.config.bearer_token),
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "tools/call",
                "params": {
                    "name": GOAL_UPDATE,
                    "arguments": {
                        "goalId": first.id,
                        "expectedRevision": first.revision,
                        "summary": "Implemented the goal tools"
                    }
                }
            }),
        )
        .await;
        let body: serde_json::Value = updated.json().await.unwrap();
        assert_eq!(body["result"]["isError"], false);
        let current = workspace.chat_goal("chat-1").unwrap();
        assert_eq!(current.revision, first.revision + 1);
        assert_eq!(
            current.status_message.as_deref(),
            Some("Implemented the goal tools")
        );

        let stale = request(
            &lease.config.url,
            Some(&lease.config.bearer_token),
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "tools/call",
                "params": {
                    "name": GOAL_COMPLETE,
                    "arguments": {
                        "goalId": first.id,
                        "expectedRevision": first.revision,
                        "summary": "Too early"
                    }
                }
            }),
        )
        .await;
        let body: serde_json::Value = stale.json().await.unwrap();
        assert_eq!(body["result"]["isError"], true);

        let other_chat = request(
            &lease.config.url,
            Some(&lease.config.bearer_token),
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 3,
                "method": "tools/call",
                "params": {
                    "name": GOAL_COMPLETE,
                    "arguments": {
                        "goalId": second.id,
                        "expectedRevision": second.revision,
                        "summary": "Wrong chat"
                    }
                }
            }),
        )
        .await;
        let body: serde_json::Value = other_chat.json().await.unwrap();
        assert_eq!(body["result"]["isError"], true);
        assert_eq!(
            workspace.chat_goal("chat-2").unwrap().status,
            GoalStatus::Active
        );

        host.shutdown().await;
    }

    #[tokio::test]
    async fn request_answers_invokes_the_leased_answer_ui() {
        let requester: McpAnswerRequester = Arc::new(|questions, _cancellation| {
            Box::pin(async move {
                assert_eq!(questions.len(), 2);
                assert_eq!(questions[0].header, "Deploy");
                assert_eq!(questions[0].options, ["Now", "Later"]);
                assert!(!questions[0].multi_select);
                assert_eq!(questions[1].header, "Question");
                assert!(questions[1].options.is_empty());
                Some(vec![
                    UserInputAnswer {
                        question_id: questions[0].id.clone(),
                        labels: vec!["Now".into()],
                    },
                    UserInputAnswer {
                        question_id: questions[1].id.clone(),
                        labels: vec!["After tests pass".into()],
                    },
                ])
            })
        });
        let host = McpHost::new();
        let lease = host
            .lease("chat-1".into(), None, Some(requester))
            .await
            .unwrap();
        let response = request(
            &lease.config.url,
            Some(&lease.config.bearer_token),
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "tools/call",
                "params": {
                    "name": REQUEST_ANSWERS,
                    "arguments": {
                        "questions": [
                            {
                                "header": " Deploy ",
                                "question": "When should I deploy?",
                                "options": ["Now", "Later"]
                            },
                            { "question": "Any final guidance?" }
                        ]
                    }
                }
            }),
        )
        .await;
        let body: serde_json::Value = response.json().await.unwrap();
        assert_eq!(body["result"]["isError"], false);
        assert_eq!(
            body["result"]["structuredContent"],
            serde_json::json!({
                "answers": [
                    { "question": "When should I deploy?", "labels": ["Now"] },
                    { "question": "Any final guidance?", "labels": ["After tests pass"] }
                ]
            })
        );
        host.shutdown().await;
    }

    #[tokio::test]
    async fn browser_origin_is_rejected() {
        let host = McpHost::new();
        let lease = host.lease("chat-1".into(), None, None).await.unwrap();
        let response = reqwest::Client::new()
            .post(&lease.config.url)
            .bearer_auth(&lease.config.bearer_token)
            .header(header::ORIGIN.as_str(), "http://127.0.0.1")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        host.shutdown().await;
    }
}
