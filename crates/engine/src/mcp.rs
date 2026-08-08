//! Engine adapter for the product-owned MCP host.

use std::sync::Arc;

use jolt_harness::McpServerConfig;
use jolt_mcp::{McpBackend, McpError, McpGoalAction};
use jolt_proto::Goal;

use crate::goals::{self, AgentGoalAction};
use crate::workspace_host::WorkspaceHost;

pub(crate) use jolt_mcp::{McpAnswerRequester, McpGoalSignal};

struct WorkspaceBackend(WorkspaceHost);

impl McpBackend for WorkspaceBackend {
    fn goal(&self, chat_id: &str) -> Result<Option<Goal>, McpError> {
        Ok(self.0.chat_goal(chat_id))
    }

    fn mutate_goal(
        &self,
        chat_id: &str,
        goal_id: &str,
        expected_revision: u64,
        action: McpGoalAction,
    ) -> Result<Goal, McpError> {
        let action = match action {
            McpGoalAction::Update { summary } => AgentGoalAction::Update { summary },
            McpGoalAction::Complete { summary } => AgentGoalAction::Complete { summary },
            McpGoalAction::Pause { reason } => AgentGoalAction::Pause { reason },
            McpGoalAction::Resume => AgentGoalAction::Resume,
        };
        self.0
            .mutate_chat_goal(chat_id, |current| {
                goals::apply_agent_action(current, goal_id, expected_revision, action).map(Some)
            })
            .map_err(|error| McpError::new(error.to_string()))?
            .ok_or_else(|| McpError::new("this session has no goal"))
    }
}

pub(crate) struct McpHost(jolt_mcp::McpHost);

impl McpHost {
    pub(crate) fn new() -> Self {
        Self(jolt_mcp::McpHost::new())
    }

    pub(crate) async fn lease(
        &self,
        chat_id: String,
        workspace: Option<WorkspaceHost>,
        answer_requester: Option<McpAnswerRequester>,
    ) -> Result<McpLease, std::io::Error> {
        let backend =
            workspace.map(|workspace| Arc::new(WorkspaceBackend(workspace)) as Arc<dyn McpBackend>);
        let inner = self.0.lease(chat_id, backend, answer_requester).await?;
        Ok(McpLease {
            config: inner.config(),
            inner,
        })
    }

    pub(crate) async fn shutdown(&self) {
        self.0.shutdown().await;
    }
}

pub(crate) struct McpLease {
    pub(crate) config: McpServerConfig,
    inner: jolt_mcp::McpLease,
}

impl McpLease {
    pub(crate) fn config(&self) -> McpServerConfig {
        self.config.clone()
    }

    pub(crate) fn take_goal_signal(&self) -> Option<McpGoalSignal> {
        self.inner.take_goal_signal()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace_host::WorkspaceHostConfig;
    use jolt_mcp::{
        GOAL_COMPLETE, GOAL_GET, GOAL_UPDATE, REQUEST_ANSWERS, SERVER_NAME, TOOL_NAMES,
    };
    use jolt_proto::{GoalStatus, UserInputAnswer};
    use reqwest::{StatusCode, header};

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
        let store = Arc::new(jolt_store::DocsStore::open(dir.path()).unwrap());
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
            &jolt_session_doc::GoalOperation::Create {
                objective: "Ship chat one".into(),
                token_budget: None,
            },
        )
        .unwrap()
        .unwrap();
        let second = goals::apply_operation(
            None,
            &jolt_session_doc::GoalOperation::Create {
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
