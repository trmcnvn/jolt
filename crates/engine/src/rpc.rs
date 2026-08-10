//! EngineRpc — the engine-side `RpcService`: sessions + docs + the workspace-doc
//! entity surface.
//!
//! Methods:
//! - `ListHarnesses` → `[HarnessDescriptor]`
//! - `ListModels {harness}` → `[Model]`
//! - `ListCommands {harness}` → Jolt's built-in `[AgentCommand]` catalog
//! - `QueueCommand {chatId, command}` → `{commandId}` (durable doc command)
//! - `ExtractQuestions {chatId, sourceMessageId}` → extracted prose questions
//!   re-emitted on every doc change
//! - `WatchChats` → active-row bootstrap plus changed-row deltas; `QueryChats` pages/searches history
//! - `WatchDevices` → stream of the workspace doc's device rows
//! - `WatchSessions` → merged live-status bootstrap plus changed-row deltas
//! - `Mutate {op, …}` → `{ok}` — workspace entity mutations (createChat, renameChat,
//!   setChatPinned, setChatArchived, deleteChat, renameDevice, markChatSeen)
//! - `RegenerateChatTitle {chatId}` → `{ok}` — replace a session name using the
//!   host harness's economy model
//! - `LocalDevice` → `{deviceId}` — this engine's identity (never forwarded)
//! - AuthRpc: `AuthStatus` (stream), `SignIn`/`SignInHeadless` → `{url}`,
//!   `CompleteSignIn {code}`, `SignOut`, `ListOrgs`, and automatic provisioning
//!   of the user's sole hidden "Personal" organization
//! - Repos: `ListRepos`, `AddRepo {path}`, `CloneRepo {url}`,
//!   `CreateRepo {name}`, `ListBranches {repoPath}` (default branch first),
//!   `ListFolders {path?}`, `CreateWorktree {repoPath, branch}`, `DeleteWorktree
//!   {repoPath, worktreePath}`; `WatchCheckoutDiffV2` → checkout manifest stream;
//!   `GetCheckoutDiffPage` → immutable patch page; `GetCheckoutVcsStatus` →
//!   checkout-scoped Git/JJ status; `RunVcsAction` → streamed Commit/Push progress
//! - Terminals: `OpenTerminal {chatId, cols, rows}` → `TerminalSession`,
//!   (replay then live tail), `WriteTerminal {terminalId, data}`, `ResizeTerminal`,
//!   `CloseTerminal`. M5 is single-user local: per-user owner checks land with
//!   real multi-account auth in M6.
//! - Agent accounts: `ListAgentAccounts {forceUsage?}` →
//!   `AgentAccountsSnapshot`, `ActivateAgentAccount`/`ForgetAgentAccount`
//!   `{harness, accountId}` → snapshot, `StartAgentLogin {harness}` →
//!   `{loginId, url, mode}`, `CompleteAgentLogin {loginId, code}` → snapshot,
//!   `PollAgentLogin {loginId}`, `CancelAgentLogin {loginId}`.
//! - Uploads: binary `UploadBinaryChunk {uploadId, seq?}` (legacy base64
//!   `UploadChunk` remains for rolling upgrades), `UploadCommit {uploadId,
//!   fileName, chatId}` → `{path, sha256}`,
//!   `ReadAttachmentChunk {path, offset}` → `{name, mimeType, data, nextOffset,
//!   done}` (path-jailed to the uploads dir + workspace-known chat cwds).
//!
//! ## Device-addressed routing (`targetDeviceId`)
//!
//! ControlRpc methods are relay-forwardable: params may carry `targetDeviceId`. When it
//! names another device, the call is forwarded verbatim over that device's relay DO via
//! the [`LinkCache`] — the remote engine sees its own id and handles locally, so the
//! forward can never loop. Streaming methods are proxied by re-subscribing remotely and
//! piping items. To make another method device-addressable, nothing per-method is needed
//! beyond listing it in [`forwardable`] (and [`is_stream_method`] if it streams);
//! handlers stay transport-agnostic. `ListCommands` is routed as well so clients see the
//! built-in catalog supported by the device that will host the chat.

use async_trait::async_trait;
use bytes::Bytes;
use futures::StreamExt;
use futures::stream::BoxStream;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::time::Duration;
use tokio::sync::watch;

use jolt_api::{
    ActivateAgentAccount, ApplyHarnessUpdate, CancelAgentLogin, CancelQueuedPrompt, ChatPage,
    ChatSection, ChatWatchFrame, CloseTerminal, CompleteAgentLogin, CreateWorktree,
    DeleteHarnessSecret, DeleteReviewDraft, DeleteTheme, ExtractQuestions, ForgetAgentAccount,
    GetCheckoutDiffPage, GetCheckoutReview, GetCheckoutVcsStatus, GetReviewDraft,
    GetTranscriptPage, GetTransportCapabilities, GetTurnDiffPage, ListAgentAccounts, ListCommands,
    ListFolders, ListHarnessSecrets, ListModels, ListRefs, Mutate, OpenTerminal, PinDiffDocument,
    PollAgentLogin, PutReviewDraft, QueryChats, QueueCommand, ReadAttachmentChunk,
    RegenerateChatTitle, ReleaseDiffDocument, ResizeTerminal, RunVcsAction, SearchFiles,
    SearchTranscript, SessionWatchFrame, SetTerminalCommand, SetVcsBackend, StartAgentLogin,
    SubscribeTerminal, SwitchRef, UploadBinaryChunk, UploadChunk, UploadCommit,
    UpsertHarnessSecret, UpsertThemes, UsageBreakdownRequest, WatchChatUsage, WatchCheckoutDiff,
    WatchQueuedPrompts, WatchTranscript, WriteTerminal, methods,
};
#[cfg(test)]
use jolt_proto::HarnessId;
use jolt_proto::{
    AgentCommand, AgentCommandSource, Chat, CheckoutVcsStatus, ExtractQuestionsResult, Session,
    SessionStatus, ToolCall, VcsAction, VcsActionEvent, VcsActionPhase, VcsActionResult,
    VcsCommitMessage, VcsCommitResult, VcsPublicationState,
};
use jolt_relay::LinkCache;
use jolt_rpc::{RpcError, RpcReply, RpcService, parse_params};
use jolt_session_doc::{MessagePart, MessageRole, MessageStatus, join_continuation_entries};

use crate::agent_accounts::AgentAccounts;
use crate::auth::Auth;
use crate::diff_sync::CheckoutDiffSync;
use crate::doc_host::DocHost;
use crate::harness_updates::HarnessUpdater;
use crate::registry::HarnessRegistry;
use crate::review_store::ReviewStore;
use crate::secrets::HarnessSecrets;
use crate::sessions::SessionsEngine;
use crate::uploads::Uploads;
use crate::workspace_host::WorkspaceHost;
use jolt_terminal::{TerminalOutput, Terminals};
use jolt_vcs::{Repos, home_dir};

mod routing;

#[cfg(test)]
use routing::local_only;
use routing::{forwardable, is_binary_stream_method, is_stream_method};
pub(crate) use routing::{relay_service, theme_sync_method};

const FILE_SEARCH_RPC_TIMEOUT: Duration = Duration::from_secs(6);
const FILE_SEARCH_FEATURED_PATHS: usize = 32;

const ANSWER_QUESTIONS_COMMAND: &str = "answer";
const BRO_COMMAND: &str = "bro";
const GOAL_COMMAND: &str = "goal";

fn jolt_commands() -> Vec<AgentCommand> {
    vec![
        AgentCommand {
            name: ANSWER_QUESTIONS_COMMAND.into(),
            description: Some("Answer questions from the latest assistant response".into()),
            argument_hint: None,
            source: AgentCommandSource::Jolt,
        },
        AgentCommand {
            name: BRO_COMMAND.into(),
            description: Some("Restate the latest assistant response in plain language".into()),
            argument_hint: None,
            source: AgentCommandSource::Jolt,
        },
        AgentCommand {
            name: GOAL_COMMAND.into(),
            description: Some("Open the long-running goal manager".into()),
            argument_hint: Some("<objective>|pause|resume|clear".into()),
            source: AgentCommandSource::Jolt,
        },
    ]
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RepoPathParams {
    /// `repoPath` per §3.5 (the §2.1 shorthand `repo` is accepted as an alias).
    #[serde(alias = "repo")]
    repo_path: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DeleteWorktreeParams {
    #[serde(alias = "repo")]
    repo_path: String,
    #[serde(alias = "path")]
    worktree_path: String,
}

fn tool_file_path(call: &ToolCall) -> Option<&str> {
    match call {
        ToolCall::ReadFile { path, .. }
        | ToolCall::WriteFile { path, .. }
        | ToolCall::EditFile { path, .. } => Some(path),
        ToolCall::ApplyPatch { path, paths } => path
            .as_deref()
            .or_else(|| paths.first().map(String::as_str)),
        ToolCall::Search { path, .. } => path.as_deref(),
        ToolCall::Exec { .. }
        | ToolCall::Glob { .. }
        | ToolCall::WebFetch { .. }
        | ToolCall::WebSearch { .. }
        | ToolCall::Todo { .. }
        | ToolCall::SpawnAgent { .. }
        | ToolCall::Mcp { .. }
        | ToolCall::Unknown { .. } => None,
    }
}

struct VcsActionTaskContext {
    sessions: SessionsEngine,
    workspace: WorkspaceHost,
    repos: Repos,
    diff_sync: CheckoutDiffSync,
    device_id: String,
}

fn publication_revision(state: &VcsPublicationState) -> Option<&str> {
    match state {
        VcsPublicationState::Ready { target, .. }
        | VcsPublicationState::Behind { target, .. }
        | VcsPublicationState::Diverged { target, .. }
        | VcsPublicationState::NoCompletedChanges { target, .. } => Some(&target.revision),
        VcsPublicationState::Ambiguous { candidates } => {
            candidates.first().map(|target| target.revision.as_str())
        }
        VcsPublicationState::NoRemote | VcsPublicationState::Unavailable { .. } => None,
    }
}

async fn validate_publication_revision(
    repos: &Repos,
    path: &std::path::Path,
    title: &str,
    publish_ref: Option<&str>,
    expected: &str,
) -> Result<(), String> {
    let (_, publication) = repos
        .publication_status_for_ref(path, title, publish_ref)
        .await
        .map_err(|error| error.to_string())?;
    if publication_revision(&publication) != Some(expected) {
        return Err("Publication state changed; refresh before pushing".into());
    }
    Ok(())
}

async fn run_vcs_action_task(
    context: VcsActionTaskContext,
    request: RunVcsAction,
    events: tokio::sync::mpsc::Sender<VcsActionEvent>,
) {
    let action_id = request.action_id.clone();
    let mut phases = Vec::new();
    if request.action.includes_commit() {
        let generates = matches!(
            &request.action,
            VcsAction::Commit {
                message: VcsCommitMessage::Generate,
                ..
            } | VcsAction::CommitAndPush {
                message: VcsCommitMessage::Generate,
                ..
            }
        );
        if generates {
            phases.push(VcsActionPhase::GeneratingMessage);
        }
        phases.push(VcsActionPhase::Committing);
    }
    if request.action.includes_push() {
        phases.push(VcsActionPhase::Pushing);
    }
    let _ = events
        .send(VcsActionEvent::Started {
            action_id: action_id.clone(),
            phases,
        })
        .await;

    let mut phase = None;
    let mut completed_commit: Option<VcsCommitResult> = None;
    let result: Result<VcsActionResult, String> = async {
        let chat = context
            .workspace
            .chat(&request.chat_id)
            .map_err(|error| error.to_string())?
            .ok_or_else(|| "chat not found".to_string())?;
        if chat.device_id != context.device_id {
            return Err("chat belongs to another device".into());
        }
        let cwd = chat
            .cwd
            .clone()
            .ok_or_else(|| "chat has no workspace folder".to_string())?;
        let path = std::path::Path::new(&cwd);
        let identity = context
            .repos
            .checkout_identity(path)
            .await
            .map_err(|error| error.to_string())?;
        let action_lock = context.repos.action_lock(&identity.id);
        let _guard = action_lock.lock().await;

        let chats = context.workspace.watch_chats().borrow().clone();
        let active_shared_run = chats.iter().any(|candidate| {
            let same_checkout = candidate.checkout_id.as_deref() == Some(identity.id.as_str())
                || candidate.cwd.as_deref() == Some(cwd.as_str());
            same_checkout
                && context.sessions.session_status(&candidate.id).is_some_and(|session| {
                    matches!(session.status, SessionStatus::Working | SessionStatus::AwaitingInput)
                })
        });
        if active_shared_run {
            return Err("Wait for active agent work in this checkout to finish before committing or pushing".into());
        }

        let title = chat.title.as_deref().unwrap_or("update");
        let publication_expectation = match &request.action {
            VcsAction::Push {
                expected_publication,
                publish_ref,
                ..
            }
            | VcsAction::CommitAndPush {
                expected_publication,
                publish_ref,
                ..
            } => Some((expected_publication.as_str(), publish_ref.as_deref())),
            VcsAction::Commit { .. } => None,
        };
        if let Some((expected, publish_ref)) = publication_expectation {
            validate_publication_revision(
                &context.repos,
                path,
                title,
                publish_ref,
                expected,
            )
            .await?;
        }

        let commit_spec = match &request.action {
            VcsAction::Commit {
                expected_working_copy,
                selection,
                message,
            }
            | VcsAction::CommitAndPush {
                expected_working_copy,
                selection,
                message,
                ..
            } => Some((expected_working_copy, selection, message)),
            VcsAction::Push { .. } => None,
        };
        if let Some((expected, selection, requested_message)) = commit_spec {
            let (manifest, paths, patch) = context
                .diff_sync
                .commit_context(&request.chat_id, expected, selection)
                .await
                .map_err(|error| error.to_string())?;
            let generator_paths = paths.clone().unwrap_or_else(|| {
                manifest
                    .files
                    .iter()
                    .map(|file| file.path.clone())
                    .collect()
            });
            let message = match requested_message {
                VcsCommitMessage::Generate => {
                    phase = Some(VcsActionPhase::GeneratingMessage);
                    let _ = events
                        .send(VcsActionEvent::PhaseStarted {
                            action_id: action_id.clone(),
                            phase: VcsActionPhase::GeneratingMessage,
                            label: "Generating commit message…".into(),
                        })
                        .await;
                    context
                        .sessions
                        .generate_commit_message(
                            &request.chat_id,
                            &cwd,
                            &generator_paths,
                            &patch,
                        )
                        .await
                        .map_err(|error| error.to_string())?
                }
                VcsCommitMessage::Provided { value } => {
                    let message = value.trim();
                    if message.is_empty() {
                        return Err("Commit message cannot be empty".into());
                    }
                    if message.chars().count() > 10_000 {
                        return Err("Commit message cannot exceed 10,000 characters".into());
                    }
                    message.to_string()
                }
            };
            // Model generation can take long enough for an editor to change the
            // checkout. Revalidate the exact diff immediately before mutation.
            context
                .diff_sync
                .commit_context(&request.chat_id, expected, selection)
                .await
                .map_err(|error| error.to_string())?;
            if let Some((expected, publish_ref)) = publication_expectation {
                validate_publication_revision(
                    &context.repos,
                    path,
                    title,
                    publish_ref,
                    expected,
                )
                .await?;
            }
            phase = Some(VcsActionPhase::Committing);
            let _ = events
                .send(VcsActionEvent::PhaseStarted {
                    action_id: action_id.clone(),
                    phase: VcsActionPhase::Committing,
                    label: "Committing…".into(),
                })
                .await;
            let commit = context
                .repos
                .commit_changes(path, paths.as_deref(), &message)
                .await
                .map_err(|error| error.to_string())?;
            completed_commit = Some(commit);
        }

        let push = if request.action.includes_push() {
            let (publish_ref, allow_default_ref) = match &request.action {
                VcsAction::Push {
                    publish_ref,
                    allow_default_ref,
                    ..
                }
                | VcsAction::CommitAndPush {
                    publish_ref,
                    allow_default_ref,
                    ..
                } => (publish_ref.as_deref(), *allow_default_ref),
                VcsAction::Commit { .. } => (None, false),
            };
            phase = Some(VcsActionPhase::Pushing);
            let _ = events
                .send(VcsActionEvent::PhaseStarted {
                    action_id: action_id.clone(),
                    phase: VcsActionPhase::Pushing,
                    label: "Pushing…".into(),
                })
                .await;
            Some(
                context
                    .repos
                    .push_completed(path, title, publish_ref, allow_default_ref)
                    .await
                    .map_err(|error| error.to_string())?,
            )
        } else {
            None
        };

        let _ = context.diff_sync.refresh_manifest(&request.chat_id).await;
        match (completed_commit.clone(), push) {
            (Some(commit), Some(push)) => Ok(VcsActionResult::CommitAndPush { commit, push }),
            (Some(commit), None) => Ok(VcsActionResult::Commit { commit }),
            (None, Some(push)) => Ok(VcsActionResult::Push { push }),
            (None, None) => Err("VCS action contained no operation".into()),
        }
    }
    .await;

    match result {
        Ok(result) => {
            let _ = events
                .send(VcsActionEvent::Finished { action_id, result })
                .await;
        }
        Err(message) => {
            let _ = events
                .send(VcsActionEvent::Failed {
                    action_id,
                    phase,
                    completed_commit,
                    message,
                })
                .await;
        }
    }
}

pub struct EngineRpc {
    sessions: SessionsEngine,
    doc_host: DocHost,
    workspace: WorkspaceHost,
    registry: std::sync::Arc<HarnessRegistry>,
    repos: Repos,
    terminals: Terminals,
    diff_sync: CheckoutDiffSync,
    review_store: ReviewStore,
    spaces_sync: crate::SpacesSync,
    uploads: Uploads,
    agent_accounts: AgentAccounts,
    secrets: HarnessSecrets,
    auth: Option<Auth>,
    links: Option<std::sync::Arc<LinkCache>>,
    updater: Option<jolt_update::Updater>,
    harness_updater: Option<HarnessUpdater>,
}

impl EngineRpc {
    #[allow(clippy::too_many_arguments)] // engine assembly seam, not a public API
    pub fn new(
        sessions: SessionsEngine,
        doc_host: DocHost,
        workspace: WorkspaceHost,
        registry: std::sync::Arc<HarnessRegistry>,
        repos: Repos,
        terminals: Terminals,
        diff_sync: CheckoutDiffSync,
        review_store: ReviewStore,
        spaces_sync: crate::SpacesSync,
        uploads: Uploads,
        agent_accounts: AgentAccounts,
        secrets: HarnessSecrets,
    ) -> Self {
        Self {
            sessions,
            doc_host,
            workspace,
            registry,
            repos,
            terminals,
            diff_sync,
            review_store,
            spaces_sync,
            uploads,
            agent_accounts,
            secrets,
            auth: None,
            links: None,
            updater: None,
            harness_updater: None,
        }
    }

    /// Attach the auth service (AuthStatus + AuthRpc mutations).
    pub fn with_auth(mut self, auth: Auth) -> Self {
        self.auth = Some(auth);
        self
    }

    /// Attach the peer link cache — enables `targetDeviceId` relay forwarding.
    pub fn with_links(mut self, links: std::sync::Arc<LinkCache>) -> Self {
        self.links = Some(links);
        self
    }

    /// Attach the release checker (UpdateStatus stream + ApplyUpdate).
    pub fn with_updater(mut self, updater: jolt_update::Updater) -> Self {
        self.updater = Some(updater);
        self
    }

    pub fn with_harness_updater(mut self, updater: HarnessUpdater) -> Self {
        self.harness_updater = Some(updater);
        self
    }

    fn auth(&self) -> Result<&Auth, RpcError> {
        self.auth
            .as_ref()
            .ok_or_else(|| RpcError::Failed("auth unavailable".into()))
    }

    async fn checkout_vcs_status(&self, chat_id: &str) -> Result<CheckoutVcsStatus, RpcError> {
        let chat = self
            .workspace
            .chat(chat_id)
            .map_err(|error| RpcError::Failed(error.to_string()))?
            .ok_or_else(|| RpcError::Failed("chat not found".into()))?;
        if chat.device_id != self.doc_host.device_id() {
            return Err(RpcError::Failed("chat belongs to another device".into()));
        }
        let cwd = chat
            .cwd
            .ok_or_else(|| RpcError::Failed("chat has no workspace folder".into()))?;
        let identity = self
            .repos
            .checkout_identity(std::path::Path::new(&cwd))
            .await
            .map_err(|error| RpcError::Failed(error.to_string()))?;
        let action_lock = self.repos.action_lock(&identity.id);
        let _guard = action_lock.lock().await;
        let manifest = self
            .diff_sync
            .refresh_manifest(chat_id)
            .await
            .map_err(|error| RpcError::Failed(error.to_string()))?;
        let title = chat.title.as_deref().unwrap_or("update");
        let (reference, publication) = self
            .repos
            .publication_status(std::path::Path::new(&cwd), title)
            .await
            .map_err(|error| RpcError::Failed(error.to_string()))?;
        Ok(CheckoutVcsStatus {
            checkout_id: identity.id,
            backend: manifest.vcs,
            reference,
            working_copy: manifest,
            publication,
        })
    }

    fn updater(&self) -> Result<&jolt_update::Updater, RpcError> {
        self.updater
            .as_ref()
            .ok_or_else(|| RpcError::Failed("updates unavailable".into()))
    }

    fn harness_updater(&self) -> Result<&HarnessUpdater, RpcError> {
        self.harness_updater
            .as_ref()
            .ok_or_else(|| RpcError::Failed("harness updates unavailable".into()))
    }

    /// Resolve a mention-search root from synced workspace rows. A client may
    /// name an existing linked worktree for a new chat, but it is verified
    /// against the space repository before any filesystem walk begins.
    async fn file_search_root(&self, p: &SearchFiles) -> Result<std::path::PathBuf, RpcError> {
        let local_device = self.doc_host.device_id();
        match (&p.chat_id, &p.space_id) {
            (Some(_), Some(_)) | (None, None) => Err(RpcError::BadParams(
                "SearchFiles needs exactly one of chatId or spaceId".into(),
            )),
            (Some(chat_id), None) => {
                if p.path.is_some() {
                    return Err(RpcError::BadParams(
                        "SearchFiles path applies only to a space".into(),
                    ));
                }
                let chat = self
                    .workspace
                    .chat(chat_id)
                    .map_err(|e| RpcError::Failed(e.to_string()))?
                    .ok_or_else(|| RpcError::Failed("chat not found".into()))?;
                if chat.device_id != local_device {
                    return Err(RpcError::Failed("chat belongs to another device".into()));
                }
                let cwd = chat
                    .cwd
                    .map(std::path::PathBuf::from)
                    .ok_or_else(|| RpcError::Failed("chat has no workspace folder".into()))?;
                let space_id = chat
                    .space_id
                    .ok_or_else(|| RpcError::Failed("chat has no workspace space".into()))?;
                let space = self
                    .workspace
                    .space(&space_id)
                    .map_err(|e| RpcError::Failed(e.to_string()))?
                    .ok_or_else(|| RpcError::Failed("chat workspace space not found".into()))?;
                if space.device_id != local_device {
                    return Err(RpcError::Failed(
                        "chat space belongs to another device".into(),
                    ));
                }
                if let Some(cwd) = self
                    .repos
                    .workspace_checkout(std::path::Path::new(&space.path), &cwd)
                    .await
                {
                    Ok(cwd)
                } else {
                    Err(RpcError::Failed(
                        "chat folder is not a workspace checkout".into(),
                    ))
                }
            }
            (None, Some(space_id)) => {
                let space = self
                    .workspace
                    .space(space_id)
                    .map_err(|e| RpcError::Failed(e.to_string()))?
                    .ok_or_else(|| RpcError::Failed("space not found".into()))?;
                if space.device_id != local_device {
                    return Err(RpcError::Failed("space belongs to another device".into()));
                }
                let space_path = std::path::PathBuf::from(&space.path);
                let requested = p
                    .path
                    .as_deref()
                    .map_or_else(|| space_path.clone(), std::path::PathBuf::from);
                if let Some(requested) =
                    self.repos.workspace_checkout(&space_path, &requested).await
                {
                    Ok(requested)
                } else {
                    Err(RpcError::BadParams(
                        "SearchFiles path is not a workspace checkout".into(),
                    ))
                }
            }
        }
    }

    /// Most-recent-first paths the current chat actually touched, followed by
    /// files still changed in its checkout. The search worker validates and
    /// normalizes them against the resolved root before using them as ranking
    /// hints, so stale or out-of-workspace tool paths simply disappear.
    fn featured_file_paths(&self, chat_id: &str) -> Vec<String> {
        let mut paths = Vec::new();
        let mut seen = HashSet::new();
        if let Ok(handle) = self.doc_host.open(chat_id)
            && let Ok(entries) = handle.doc().read_entries()
        {
            for entry in entries.into_iter().rev() {
                for part in entry.parts.into_iter().rev() {
                    if let MessagePart::Tool { call, .. } = part
                        && let Some(path) = tool_file_path(&call)
                        && !path.trim().is_empty()
                        && seen.insert(path.to_string())
                    {
                        paths.push(path.to_string());
                        if paths.len() == FILE_SEARCH_FEATURED_PATHS {
                            break;
                        }
                    }
                }
                if paths.len() == FILE_SEARCH_FEATURED_PATHS {
                    break;
                }
            }
        }

        if let Some(diff) = self.diff_sync.current_manifest(chat_id) {
            for file in &diff.files {
                if paths.len() == FILE_SEARCH_FEATURED_PATHS {
                    break;
                }
                if seen.insert(file.path.clone()) {
                    paths.push(file.path.clone());
                }
            }
        }
        paths
    }

    /// Forward a device-addressed call over the target device's relay. On transport
    /// failure the cached link is invalidated so the next call re-dials.
    async fn forward(
        &self,
        target: &str,
        method: &str,
        params: serde_json::Value,
    ) -> Result<RpcReply, RpcError> {
        let Some(links) = &self.links else {
            return Err(RpcError::Failed(format!(
                "cannot reach device {target}: remote routing unavailable (offline)"
            )));
        };
        let client = links.client(target).await?;
        if is_binary_stream_method(method) {
            let rx = match client.subscribe_binary(method, params).await {
                Ok(rx) => rx,
                Err(err) => {
                    links.invalidate(target);
                    return Err(err);
                }
            };
            let stream = futures::stream::unfold((rx, client), |(mut rx, client)| async move {
                rx.recv().await.map(|item| (item, (rx, client)))
            });
            return Ok(RpcReply::BinaryStream(stream.boxed()));
        }
        if is_stream_method(method) {
            let rx = match client.subscribe(method, params).await {
                Ok(rx) => rx,
                Err(err) => {
                    links.invalidate(target);
                    return Err(err);
                }
            };
            // Pipe remote items; the held client keeps the link's RpcClient alive for
            // the stream's lifetime. A remote error just ends the stream (the relay
            // link-down path fails pending calls; stream receivers close).
            let stream = futures::stream::unfold((rx, client), |(mut rx, client)| async move {
                rx.recv().await.map(|item| (item, (rx, client)))
            });
            return Ok(RpcReply::Stream(stream.boxed()));
        }
        match client.call(method, params).await {
            Ok(value) => Ok(RpcReply::Value(value)),
            Err(err) => {
                if matches!(err, RpcError::Closed | RpcError::Transport(_)) {
                    links.invalidate(target);
                }
                Err(err)
            }
        }
    }

    async fn forward_binary(
        &self,
        target: &str,
        method: &str,
        params: serde_json::Value,
        payload: Bytes,
    ) -> Result<RpcReply, RpcError> {
        let Some(links) = &self.links else {
            return Err(RpcError::Failed(format!(
                "cannot reach device {target}: remote routing unavailable (offline)"
            )));
        };
        let client = links.client(target).await?;
        match client.call_binary(method, params, payload).await {
            Ok(value) => Ok(RpcReply::Value(value)),
            Err(err) => {
                if matches!(err, RpcError::Closed | RpcError::Transport(_)) {
                    links.invalidate(target);
                }
                Err(err)
            }
        }
    }

    fn mutate(&self, params: Mutate) -> Result<(), RpcError> {
        let failed = |e: crate::EngineError| RpcError::Failed(e.to_string());
        match params {
            Mutate::CreateChat {
                chat_id,
                space_id,
                config,
                branch,
                cwd,
            } => {
                self.workspace
                    .create_chat(&chat_id, &space_id, config, cwd)
                    .map_err(failed)?;
                if let Some(branch) = branch.as_deref().filter(|b| !b.is_empty()) {
                    self.workspace
                        .set_chat_branch(&chat_id, branch)
                        .map_err(failed)?;
                }
                Ok(())
            }
            Mutate::CreateSpace {
                space_id,
                device_id,
                path,
                name,
                git_detected,
            } => self
                .workspace
                .create_space(&space_id, &device_id, &path, name, git_detected)
                .map_err(failed),
            Mutate::RenameSpace { space_id, name } => self
                .workspace
                .rename_space(&space_id, name.as_deref())
                .map_err(failed)
                .map(drop),
            Mutate::DeleteSpace { space_id } => {
                let deleted = self.workspace.delete_space(&space_id).map_err(failed)?;
                // Best-effort teardown of live runs we host for the deleted chats
                // (the doc rows are already tombstoned; a straggler run would only
                // write into an orphaned session doc).
                let sessions = self.sessions.clone();
                let doc_host = self.doc_host.clone();
                let chat_ids = deleted.chat_ids;
                tokio::spawn(async move {
                    for chat_id in chat_ids {
                        if let Err(err) = sessions.interrupt(&chat_id).await {
                            tracing::debug!(chat = %chat_id, error = %err, "deleteSpace interrupt skipped");
                        }
                        doc_host.purge_chat(&chat_id);
                        if let Err(err) = sessions.remove_turn_diffs(&chat_id).await {
                            tracing::warn!(chat = %chat_id, error = %err, "turn diff cleanup failed");
                        }
                    }
                });
                Ok(())
            }
            Mutate::RenameChat { chat_id, title } => self
                .workspace
                .rename_chat(&chat_id, &title)
                .map_err(failed)
                .map(drop),
            Mutate::SetChatBranch { chat_id, branch } => self
                .workspace
                .set_chat_branch(&chat_id, &branch)
                .map_err(failed)
                .map(drop),
            Mutate::SetChatCwd { chat_id, cwd } => self
                .workspace
                .set_chat_cwd(&chat_id, &cwd)
                .map_err(failed)
                .map(drop),
            Mutate::SetChatActivity {
                chat_id,
                last_message_at,
                created_at,
            } => self
                .workspace
                .set_chat_activity(&chat_id, last_message_at, created_at)
                .map_err(failed)
                .map(drop),
            Mutate::SetChatHost { chat_id, device_id } => self
                .workspace
                .set_chat_host(&chat_id, &device_id)
                .map_err(failed)
                .map(drop),
            Mutate::SetChatPinned { chat_id, pinned } => self
                .workspace
                .set_chat_pinned(&chat_id, pinned)
                .map_err(failed)
                .map(drop),
            Mutate::SetChatArchived { chat_id, archived } => self
                .workspace
                .set_chat_archived(&chat_id, archived)
                .map_err(failed)
                .map(drop),
            Mutate::SetChatConfig { chat_id, config } => self
                .workspace
                .set_chat_config(&chat_id, &config)
                .map_err(failed)
                .map(drop),
            Mutate::DeleteChat { chat_id } => {
                self.workspace.delete_chat(&chat_id).map_err(failed)?;
                let sessions = self.sessions.clone();
                let doc_host = self.doc_host.clone();
                tokio::spawn(async move {
                    if let Err(err) = sessions.interrupt(&chat_id).await {
                        tracing::debug!(chat = %chat_id, error = %err, "deleteChat interrupt skipped");
                    }
                    doc_host.purge_chat(&chat_id);
                    if let Err(err) = sessions.remove_turn_diffs(&chat_id).await {
                        tracing::warn!(chat = %chat_id, error = %err, "turn diff cleanup failed");
                    }
                });
                Ok(())
            }
            Mutate::RenameDevice { device_id, name } => self
                .workspace
                .rename_device(&device_id, &name)
                .map_err(failed)
                .map(drop),
            Mutate::DeleteDevice { device_id } => {
                let deleted = self.workspace.delete_device(&device_id).map_err(failed)?;
                let sessions = self.sessions.clone();
                let doc_host = self.doc_host.clone();
                tokio::spawn(async move {
                    for chat_id in deleted.chat_ids {
                        if let Err(err) = sessions.interrupt(&chat_id).await {
                            tracing::debug!(chat = %chat_id, error = %err, "deleteDevice interrupt skipped");
                        }
                        doc_host.purge_chat(&chat_id);
                        if let Err(err) = sessions.remove_turn_diffs(&chat_id).await {
                            tracing::warn!(chat = %chat_id, error = %err, "turn diff cleanup failed");
                        }
                    }
                });
                Ok(())
            }
            Mutate::MarkChatSeen { chat_id, at } => {
                let at = at
                    .and_then(chrono::DateTime::<chrono::Utc>::from_timestamp_millis)
                    .unwrap_or_else(chrono::Utc::now);
                self.workspace
                    .mark_chat_seen(&chat_id, at)
                    .map_err(failed)
                    .map(drop)
            }
        }
    }
}

/// A watch receiver as a stream: current value first, then every change.
pub(crate) fn watch_stream<T>(rx: watch::Receiver<T>) -> BoxStream<'static, serde_json::Value>
where
    T: serde::Serialize + Clone + Send + Sync + 'static,
{
    futures::stream::unfold((rx, false), |(mut rx, emitted)| async move {
        if emitted {
            rx.changed().await.ok()?;
        }
        let value = {
            let borrowed = rx.borrow_and_update();
            serde_json::to_value(&*borrowed).ok()?
        };
        Some((value, (rx, true)))
    })
    .boxed()
}

/// Active rows bootstrap once; subsequent frames contain only changed rows.
/// The previous full registry view stays server-side so a slow subscriber may
/// safely coalesce workspace watch updates without missing the resulting diff.
fn chat_stream(rx: watch::Receiver<Vec<Chat>>) -> BoxStream<'static, serde_json::Value> {
    futures::stream::unfold(
        (rx, None::<HashMap<String, Chat>>),
        |(mut rx, previous)| async move {
            let mut previous = previous;
            loop {
                if previous.is_some() {
                    rx.changed().await.ok()?;
                }
                let current: Vec<Chat> = rx.borrow_and_update().clone();
                let current_by_id: HashMap<_, _> = current
                    .iter()
                    .cloned()
                    .map(|chat| (chat.id.clone(), chat))
                    .collect();
                let frame = if let Some(old) = previous.replace(current_by_id.clone()) {
                    let upserts: Vec<_> = current
                        .into_iter()
                        .filter(|chat| old.get(&chat.id) != Some(chat))
                        .collect();
                    let removed_ids: Vec<_> = old
                        .keys()
                        .filter(|id| !current_by_id.contains_key(*id))
                        .cloned()
                        .collect();
                    if upserts.is_empty() && removed_ids.is_empty() {
                        continue;
                    }
                    ChatWatchFrame::Delta {
                        upserts,
                        removed_ids,
                    }
                } else {
                    ChatWatchFrame::Bootstrap {
                        chats: current.into_iter().filter(|chat| !chat.archived).collect(),
                    }
                };
                let value = serde_json::to_value(frame).ok()?;
                return Some((value, (rx, previous)));
            }
        },
    )
    .boxed()
}

fn session_stream(rx: watch::Receiver<Vec<Session>>) -> BoxStream<'static, serde_json::Value> {
    futures::stream::unfold(
        (rx, None::<HashMap<String, Session>>),
        |(mut rx, previous)| async move {
            let mut previous = previous;
            loop {
                if previous.is_some() {
                    rx.changed().await.ok()?;
                }
                let current: Vec<Session> = rx.borrow_and_update().clone();
                let current_by_chat: HashMap<_, _> = current
                    .iter()
                    .cloned()
                    .map(|session| (session.chat_id.clone(), session))
                    .collect();
                let frame = if let Some(old) = previous.replace(current_by_chat.clone()) {
                    let upserts: Vec<_> = current
                        .into_iter()
                        .filter(|session| old.get(&session.chat_id) != Some(session))
                        .collect();
                    let removed_chat_ids: Vec<_> = old
                        .keys()
                        .filter(|id| !current_by_chat.contains_key(*id))
                        .cloned()
                        .collect();
                    if upserts.is_empty() && removed_chat_ids.is_empty() {
                        continue;
                    }
                    SessionWatchFrame::Delta {
                        upserts,
                        removed_chat_ids,
                    }
                } else {
                    SessionWatchFrame::Bootstrap { sessions: current }
                };
                let value = serde_json::to_value(frame).ok()?;
                return Some((value, (rx, previous)));
            }
        },
    )
    .boxed()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ChatCursor {
    archived: bool,
    pinned: bool,
    activity_ms: i64,
    id: String,
}

fn chat_activity_ms(chat: &Chat) -> i64 {
    chat.last_message_at
        .unwrap_or(chat.created_at)
        .timestamp_millis()
}

fn chat_cursor(chat: &Chat) -> ChatCursor {
    ChatCursor {
        archived: chat.archived,
        pinned: !chat.archived && chat.pinned,
        activity_ms: chat_activity_ms(chat),
        id: chat.id.clone(),
    }
}

fn compare_chat_cursor(left: &ChatCursor, right: &ChatCursor) -> std::cmp::Ordering {
    left.archived
        .cmp(&right.archived)
        .then_with(|| right.pinned.cmp(&left.pinned))
        .then_with(|| right.activity_ms.cmp(&left.activity_ms))
        .then_with(|| left.id.cmp(&right.id))
}

fn query_chats(chats: Vec<Chat>, request: &QueryChats) -> Result<ChatPage, RpcError> {
    let query = request.query.trim().to_lowercase();
    let mut chats: Vec<_> = chats
        .into_iter()
        .filter(|chat| match request.section {
            ChatSection::Active => !chat.archived,
            ChatSection::Archived => chat.archived,
            ChatSection::Any => true,
        })
        .filter(|chat| {
            request
                .space_id
                .as_deref()
                .is_none_or(|space_id| chat.space_id.as_deref() == Some(space_id))
        })
        .filter(|chat| {
            query.is_empty()
                || chat
                    .title
                    .as_deref()
                    .unwrap_or("New session")
                    .to_lowercase()
                    .contains(&query)
        })
        .collect();
    chats.sort_by(|left, right| compare_chat_cursor(&chat_cursor(left), &chat_cursor(right)));
    let total = chats.len();
    let start = match request.cursor.as_deref() {
        Some(cursor) => {
            let cursor: ChatCursor = serde_json::from_str(cursor)
                .map_err(|_| RpcError::BadParams("invalid chat cursor".into()))?;
            chats
                .iter()
                .position(|chat| compare_chat_cursor(&chat_cursor(chat), &cursor).is_gt())
                .unwrap_or(chats.len())
        }
        None => 0,
    };
    let limit = usize::from(if request.limit == 0 {
        50
    } else {
        request.limit
    })
    .min(100);
    let end = (start + limit).min(chats.len());
    let page = chats[start..end].to_vec();
    let next_cursor = (end < chats.len())
        .then(|| page.last())
        .flatten()
        .and_then(|chat| serde_json::to_string(&chat_cursor(chat)).ok());
    Ok(ChatPage {
        chats: page,
        next_cursor,
        total,
    })
}

/// The transcript watch as delta frames (`jolt_session_doc::transcript_delta`): a
/// full `reset` first, then only changed entries per commit — the whole-Vec
/// serialization here was the per-tick cost that scaled with transcript size.
fn transcript_stream(
    bootstrap: jolt_session_doc::TranscriptBootstrap,
    rx: tokio::sync::broadcast::Receiver<jolt_session_doc::TranscriptWatchFrame>,
) -> BoxStream<'static, serde_json::Value> {
    futures::stream::unfold((Some(bootstrap), rx), |(opening, mut rx)| async move {
        if let Some(bootstrap) = opening {
            let frame = jolt_session_doc::TranscriptWatchFrame::Bootstrap { bootstrap };
            return serde_json::to_value(frame)
                .ok()
                .map(|value| (value, (None, rx)));
        }
        match rx.recv().await {
            Ok(frame) => serde_json::to_value(frame)
                .ok()
                .map(|value| (value, (None, rx))),
            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                // Ending forces the client to resubscribe for an atomic
                // bootstrap instead of applying deltas across a gap.
                None
            }
            Err(tokio::sync::broadcast::error::RecvError::Closed) => None,
        }
    })
    .boxed()
}

fn diff_stream(
    bootstrap: jolt_proto::CheckoutDiffBootstrap,
    rx: tokio::sync::broadcast::Receiver<jolt_proto::CheckoutDiffWatchFrame>,
) -> BoxStream<'static, serde_json::Value> {
    futures::stream::unfold((Some(bootstrap), rx), |(opening, mut rx)| async move {
        if let Some(bootstrap) = opening {
            let frame = jolt_proto::CheckoutDiffWatchFrame::Bootstrap { bootstrap };
            return serde_json::to_value(frame)
                .ok()
                .map(|value| (value, (None, rx)));
        }
        match rx.recv().await {
            Ok(frame) => serde_json::to_value(frame)
                .ok()
                .map(|value| (value, (None, rx))),
            Err(
                tokio::sync::broadcast::error::RecvError::Lagged(_)
                | tokio::sync::broadcast::error::RecvError::Closed,
            ) => None,
        }
    })
    .boxed()
}

/// Authentication-only RPC surface used while the headed app is waiting for a
/// production WorkOS session. Keeping this independent from [`EngineRpc`] lets
/// the UI sign in and provision the hidden Personal organization before
/// identity-scoped stores are opened.
#[derive(Clone)]
pub struct AuthRpc {
    auth: Auth,
}

impl AuthRpc {
    pub fn new(auth: Auth) -> Self {
        Self { auth }
    }

    pub fn handles(method: &str) -> bool {
        matches!(
            method,
            methods::AUTH_STATUS
                | methods::SIGN_IN
                | methods::SIGN_IN_HEADLESS
                | methods::COMPLETE_SIGN_IN
                | methods::SIGN_OUT
                | methods::LIST_ORGS
                | methods::ENSURE_PERSONAL_ORG
        )
    }
}

#[async_trait]
impl RpcService for AuthRpc {
    async fn handle(&self, method: &str, params: serde_json::Value) -> Result<RpcReply, RpcError> {
        match method {
            methods::AUTH_STATUS => Ok(RpcReply::Stream(watch_stream(self.auth.watch_state()))),
            methods::SIGN_IN => {
                let url = self
                    .auth
                    .start_sign_in()
                    .await
                    .map_err(|e| RpcError::Failed(e.to_string()))?;
                RpcReply::value(&serde_json::json!({ "url": url }))
            }
            methods::SIGN_IN_HEADLESS => {
                let url = self.auth.start_headless_sign_in();
                RpcReply::value(&serde_json::json!({ "url": url }))
            }
            methods::COMPLETE_SIGN_IN => {
                #[derive(Deserialize)]
                struct P {
                    code: String,
                }
                let p: P = parse_params(params)?;
                self.auth
                    .complete_sign_in(&p.code)
                    .await
                    .map_err(|e| RpcError::Failed(e.to_string()))?;
                RpcReply::value(&serde_json::json!({ "ok": true }))
            }
            methods::SIGN_OUT => {
                self.auth.sign_out();
                RpcReply::value(&serde_json::json!({ "ok": true }))
            }
            methods::LIST_ORGS => {
                let orgs = self
                    .auth
                    .list_orgs()
                    .await
                    .map_err(|e| RpcError::Failed(e.to_string()))?;
                RpcReply::value(&serde_json::json!({ "orgs": orgs }))
            }
            methods::ENSURE_PERSONAL_ORG => {
                self.auth
                    .ensure_personal_org()
                    .await
                    .map_err(|e| RpcError::Failed(e.to_string()))?;
                RpcReply::value(&serde_json::json!({ "ok": true }))
            }
            _ => Err(RpcError::UnknownMethod(method.to_string())),
        }
    }
}

#[async_trait]
impl RpcService for EngineRpc {
    async fn handle(&self, method: &str, params: serde_json::Value) -> Result<RpcReply, RpcError> {
        // Device-addressed routing: forward calls that target another device over its
        // relay. The target compares the id to its own, so forwards cannot loop.
        if forwardable(method)
            && let Some(target) = params.get("targetDeviceId").and_then(|v| v.as_str())
            && target != self.doc_host.device_id()
        {
            let target = target.to_string();
            return self.forward(&target, method, params).await;
        }
        if AuthRpc::handles(method) {
            return AuthRpc::new(self.auth()?.clone())
                .handle(method, params)
                .await;
        }
        match method {
            methods::LIST_HARNESSES => RpcReply::value(&self.registry.descriptors()),
            methods::WATCH_HARNESS_UPDATES => Ok(RpcReply::Stream(watch_stream(
                self.harness_updater()?.watch(),
            ))),
            methods::CHECK_HARNESS_UPDATES => {
                self.harness_updater()?.check_now();
                RpcReply::value(&serde_json::json!({ "ok": true }))
            }
            methods::APPLY_HARNESS_UPDATE => {
                let p: ApplyHarnessUpdate = parse_params(params)?;
                self.harness_updater()?
                    .apply(p.harness)
                    .map_err(|error| RpcError::Failed(error.to_string()))?;
                RpcReply::value(&serde_json::json!({ "ok": true }))
            }
            methods::LIST_MODELS => {
                let p: ListModels = parse_params(params)?;
                let harness = self
                    .registry
                    .resolve(p.harness)
                    .map_err(|e| RpcError::Failed(e.to_string()))?;
                let models = harness
                    .models()
                    .await
                    .map_err(|e| RpcError::Failed(e.to_string()))?;
                RpcReply::value(&models)
            }
            methods::LIST_COMMANDS => {
                let p: ListCommands = parse_params(params)?;
                self.registry
                    .resolve(p.harness)
                    .map_err(|e| RpcError::Failed(e.to_string()))?;
                RpcReply::value(&jolt_commands())
            }
            methods::QUEUE_COMMAND => {
                let p: QueueCommand = parse_params(params)?;
                let restores_chat = matches!(
                    &p.command,
                    jolt_session_doc::SessionCommandPayload::Run { .. }
                        | jolt_session_doc::SessionCommandPayload::Queue { .. }
                        | jolt_session_doc::SessionCommandPayload::Steer { .. }
                );
                let command_id = self
                    .doc_host
                    .queue_command(&p.chat_id, p.command)
                    .map_err(|e| RpcError::Failed(e.to_string()))?;
                if restores_chat
                    && let Err(error) = self.workspace.set_chat_archived(&p.chat_id, false)
                {
                    tracing::warn!(chat = %p.chat_id, %error, "sent archived chat could not be restored");
                }
                RpcReply::value(&serde_json::json!({ "commandId": command_id }))
            }
            methods::CANCEL_QUEUED_PROMPT => {
                let p: CancelQueuedPrompt = parse_params(params)?;
                let cancelled = self
                    .doc_host
                    .cancel_queued_prompt(&p.chat_id, &p.command_id)
                    .map_err(|e| RpcError::Failed(e.to_string()))?;
                RpcReply::value(&serde_json::json!({ "cancelled": cancelled }))
            }
            methods::WATCH_QUEUED_PROMPTS => {
                let p: WatchQueuedPrompts = parse_params(params)?;
                let handle = self
                    .doc_host
                    .open(&p.chat_id)
                    .map_err(|e| RpcError::Failed(e.to_string()))?;
                Ok(RpcReply::Stream(watch_stream(handle.watch_queue())))
            }
            methods::WATCH_TRANSCRIPT_V2 => {
                let p: WatchTranscript = parse_params(params)?;
                let handle = self
                    .doc_host
                    .open(&p.chat_id)
                    .map_err(|e| RpcError::Failed(e.to_string()))?;
                let (bootstrap, rx) = handle
                    .watch_transcript()
                    .map_err(|e| RpcError::Failed(e.to_string()))?;
                Ok(RpcReply::Stream(transcript_stream(bootstrap, rx)))
            }
            methods::GET_TRANSCRIPT_PAGE => {
                let p: GetTranscriptPage = parse_params(params)?;
                let handle = self
                    .doc_host
                    .open(&p.chat_id)
                    .map_err(|e| RpcError::Failed(e.to_string()))?;
                match handle
                    .transcript_page(&p.page_id)
                    .map_err(|e| RpcError::Failed(e.to_string()))?
                {
                    Some(page) => RpcReply::value(&page),
                    None => Err(RpcError::BadParams(format!(
                        "unknown transcript page {}",
                        p.page_id
                    ))),
                }
            }
            methods::SEARCH_TRANSCRIPT => {
                const RESULT_LIMIT: usize = 100;
                let p: SearchTranscript = parse_params(params)?;
                let handle = self
                    .doc_host
                    .open(&p.chat_id)
                    .map_err(|e| RpcError::Failed(e.to_string()))?;
                let results = handle
                    .search_transcript(&p.query, RESULT_LIMIT)
                    .map_err(|e| RpcError::Failed(e.to_string()))?;
                RpcReply::value(&results)
            }
            methods::WATCH_CHAT_USAGE => {
                let p: WatchChatUsage = parse_params(params)?;
                let usage = self
                    .sessions
                    .watch_usage(&p.chat_id)
                    .map_err(|error| RpcError::Failed(format!("usage store: {error}")))?;
                Ok(RpcReply::Stream(watch_stream(usage)))
            }
            methods::USAGE_BREAKDOWN => {
                let p: UsageBreakdownRequest = parse_params(params)?;
                if !matches!(p.days, 7 | 30 | 90) {
                    return Err(RpcError::BadParams("days must be 7, 30, or 90".into()));
                }
                let breakdown = self
                    .sessions
                    .usage_breakdown(p.days)
                    .map_err(|error| RpcError::Failed(format!("usage store: {error}")))?;
                RpcReply::value(&breakdown)
            }
            methods::EXTRACT_QUESTIONS => {
                let p: ExtractQuestions = parse_params(params)?;
                let status = self.sessions.session_status(&p.chat_id);
                if status.is_some_and(|session| {
                    matches!(
                        session.status,
                        SessionStatus::Working | SessionStatus::AwaitingInput
                    )
                }) {
                    return Err(RpcError::Failed(
                        "wait for the current run to finish before extracting questions".into(),
                    ));
                }
                let handle = self
                    .doc_host
                    .open(&p.chat_id)
                    .map_err(|e| RpcError::Failed(e.to_string()))?;
                let entries = handle
                    .doc()
                    .read_entries()
                    .map(join_continuation_entries)
                    .map_err(|e| RpcError::Failed(e.to_string()))?;
                let entry = entries
                    .iter()
                    .find(|entry| entry.id == p.source_message_id)
                    .ok_or_else(|| RpcError::Failed("assistant message no longer exists".into()))?;
                if entry.role != MessageRole::Assistant
                    || entry.status != Some(MessageStatus::Complete)
                {
                    return Err(RpcError::Failed(
                        "questions can only be extracted from a completed assistant message".into(),
                    ));
                }
                let assistant_text = entry
                    .parts
                    .iter()
                    .filter_map(|part| match part {
                        MessagePart::Text { text, .. } if !text.trim().is_empty() => {
                            Some(text.as_str())
                        }
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                if assistant_text.is_empty() {
                    return Err(RpcError::Failed(
                        "the assistant message has no text to inspect".into(),
                    ));
                }
                let chat = self
                    .workspace
                    .chat(&p.chat_id)
                    .map_err(|e| RpcError::Failed(e.to_string()))?
                    .ok_or_else(|| RpcError::Failed("chat no longer exists".into()))?;
                let prior = self.sessions.last_request(&p.chat_id);
                let harness = chat
                    .config
                    .as_ref()
                    .map(|config| config.harness)
                    .ok_or_else(|| RpcError::Failed("chat harness is unavailable".into()))?;
                let model = chat
                    .config
                    .as_ref()
                    .and_then(|config| config.model.as_deref())
                    .or_else(|| prior.as_ref().and_then(|request| request.model.as_deref()));
                let cwd = chat
                    .cwd
                    .as_deref()
                    .or_else(|| prior.as_ref().map(|request| request.cwd.as_str()))
                    .unwrap_or(".");
                let questions = crate::question_extraction::extract_questions(
                    &self.registry,
                    self.sessions.usage_store(),
                    &p.chat_id,
                    harness,
                    model,
                    cwd,
                    &assistant_text,
                )
                .await
                .map_err(|e| RpcError::Failed(e.to_string()))?;
                RpcReply::value(&ExtractQuestionsResult {
                    source_message_id: p.source_message_id,
                    questions,
                })
            }
            methods::PROBE_SYNC => {
                self.workspace.probe();
                self.doc_host.probe_open_chats();
                RpcReply::value(&serde_json::json!({}))
            }
            methods::SYNC_STATUS => {
                fn room_json(s: &jolt_sync::RoomStatsSnapshot) -> serde_json::Value {
                    serde_json::json!({
                        "connected": s.connected,
                        "lastPushedMs": s.last_pushed_ms,
                        "lastAckMs": s.last_ack_ms,
                        "rejoins": s.rejoins,
                        "probes": s.probes,
                        "fullResyncs": s.full_resyncs,
                        "disconnects": s.disconnects,
                        "rejected": s.rejected,
                    })
                }
                let workspace = self.workspace.sync_status();
                let chats: Vec<serde_json::Value> = self
                    .doc_host
                    .sync_statuses()
                    .iter()
                    .map(|(chat_id, room)| {
                        serde_json::json!({
                            "chatId": chat_id,
                            "room": room.as_ref().map(room_json),
                        })
                    })
                    .collect();
                RpcReply::value(&serde_json::json!({
                    "deviceId": self.doc_host.device_id(),
                    "nowMs": crate::now_ms(),
                    "workspace": workspace.as_ref().map(room_json),
                    "chats": chats,
                }))
            }
            methods::WATCH_CHATS => Ok(RpcReply::Stream(chat_stream(self.workspace.watch_chats()))),
            methods::QUERY_CHATS => {
                let request: QueryChats = parse_params(params)?;
                let chats = self
                    .workspace
                    .read_chats()
                    .map_err(|error| RpcError::Failed(error.to_string()))?;
                RpcReply::value(&query_chats(chats, &request)?)
            }
            methods::WATCH_DEVICES => Ok(RpcReply::Stream(watch_stream(
                self.workspace.watch_devices(),
            ))),
            methods::WATCH_SPACES => Ok(RpcReply::Stream(watch_stream(
                self.workspace.watch_spaces(),
            ))),
            methods::WATCH_THEMES => Ok(RpcReply::Stream(watch_stream(
                self.workspace.watch_themes(),
            ))),
            methods::LIST_THEMES => RpcReply::value(
                &self
                    .workspace
                    .read_themes()
                    .map_err(|err| RpcError::Failed(err.to_string()))?,
            ),
            methods::UPSERT_THEMES => {
                let params: UpsertThemes = parse_params(params)?;
                self.workspace
                    .upsert_themes(&params.themes)
                    .map_err(|err| RpcError::Failed(err.to_string()))?;
                RpcReply::value(&serde_json::json!({ "ok": true }))
            }
            methods::DELETE_THEME => {
                let params: DeleteTheme = parse_params(params)?;
                self.workspace
                    .delete_theme(&params.id)
                    .map_err(|err| RpcError::Failed(err.to_string()))?;
                RpcReply::value(&serde_json::json!({ "ok": true }))
            }
            methods::WATCH_SESSIONS => {
                // Local live statuses merged with remote devices' workspace rows.
                let merged = self
                    .workspace
                    .merged_sessions_watch(self.sessions.watch_sessions());
                Ok(RpcReply::Stream(session_stream(merged)))
            }
            methods::LOCAL_DEVICE => {
                RpcReply::value(&serde_json::json!({ "deviceId": self.doc_host.device_id() }))
            }
            methods::UPDATE_STATUS => Ok(RpcReply::Stream(watch_stream(self.updater()?.watch()))),
            methods::APPLY_UPDATE => {
                let version = self
                    .updater()?
                    .apply()
                    .await
                    .map_err(|e| RpcError::Failed(format!("{e:#}")))?;
                RpcReply::value(&serde_json::json!({ "ok": true, "version": version }))
            }
            methods::MUTATE => {
                let p: Mutate = parse_params(params)?;
                self.mutate(p)?;
                RpcReply::value(&serde_json::json!({ "ok": true }))
            }
            methods::REGENERATE_CHAT_TITLE => {
                let p: RegenerateChatTitle = parse_params(params)?;
                self.sessions
                    .regenerate_title(&p.chat_id)
                    .await
                    .map_err(|error| RpcError::Failed(error.to_string()))?;
                RpcReply::value(&serde_json::json!({ "ok": true }))
            }
            methods::PIN_DIFF_DOCUMENT => {
                let p: PinDiffDocument = parse_params(params)?;
                self.diff_sync
                    .pin_diff(&p.chat_id, &p.catalog_revision, &p.review_id)
                    .await
                    .map_err(|error| RpcError::Failed(error.to_string()))?;
                RpcReply::value(&serde_json::json!({ "ok": true }))
            }
            methods::RELEASE_DIFF_DOCUMENT => {
                let p: ReleaseDiffDocument = parse_params(params)?;
                self.diff_sync
                    .release_diff(&p.catalog_revision, &p.review_id)
                    .await
                    .map_err(|error| RpcError::Failed(error.to_string()))?;
                RpcReply::value(&serde_json::json!({ "ok": true }))
            }
            methods::GET_REVIEW_DRAFT => {
                let p: GetReviewDraft = parse_params(params)?;
                let draft = self
                    .review_store
                    .get(&p.review_key)
                    .map_err(|error| RpcError::Failed(format!("review draft read: {error}")))?;
                RpcReply::value(&draft)
            }
            methods::PUT_REVIEW_DRAFT => {
                let p: PutReviewDraft = parse_params(params)?;
                self.review_store
                    .put(&p.draft)
                    .map_err(|error| RpcError::Failed(format!("review draft write: {error}")))?;
                RpcReply::value(&serde_json::json!({ "ok": true }))
            }
            methods::DELETE_REVIEW_DRAFT => {
                let p: DeleteReviewDraft = parse_params(params)?;
                self.review_store
                    .delete(&p.review_key)
                    .map_err(|error| RpcError::Failed(format!("review draft delete: {error}")))?;
                RpcReply::value(&serde_json::json!({ "ok": true }))
            }
            methods::WATCH_CHECKOUT_DIFF_V2 => {
                let p: WatchCheckoutDiff = parse_params(params)?;
                let (bootstrap, receiver) = self
                    .diff_sync
                    .watch_diff(&p.chat_id)
                    .await
                    .map_err(|error| RpcError::Failed(error.to_string()))?;
                Ok(RpcReply::Stream(diff_stream(bootstrap, receiver)))
            }
            methods::GET_CHECKOUT_DIFF_PAGE => {
                let p: GetCheckoutDiffPage = parse_params(params)?;
                match self
                    .diff_sync
                    .diff_page(&p.chat_id, &p.catalog_revision, &p.page_id)
                    .map_err(|error| RpcError::Failed(error.to_string()))?
                {
                    Some(page) => RpcReply::value(&page),
                    None => Err(RpcError::BadParams(format!(
                        "unknown checkout diff page {}",
                        p.page_id
                    ))),
                }
            }
            methods::GET_TURN_DIFF_PAGE => {
                let p: GetTurnDiffPage = parse_params(params)?;
                match self
                    .sessions
                    .turn_diff_page(
                        &p.chat_id,
                        &p.assistant_message_id,
                        &p.catalog_revision,
                        &p.page_id,
                    )
                    .await
                    .map_err(|error| RpcError::Failed(error.to_string()))?
                {
                    Some(page) => RpcReply::value(&page),
                    None => Err(RpcError::BadParams(format!(
                        "unknown turn diff page {}",
                        p.page_id
                    ))),
                }
            }
            methods::VCS_SETTINGS => RpcReply::value(&self.repos.vcs_settings()),
            methods::SET_VCS_BACKEND => {
                let p: SetVcsBackend = parse_params(params)?;
                let snapshot = self
                    .repos
                    .set_vcs(p.backend)
                    .map_err(|e| RpcError::Failed(e.to_string()))?;
                self.spaces_sync.reconcile_now().await;
                self.diff_sync.reconcile_now().await;
                self.diff_sync.sync_all();
                RpcReply::value(&snapshot)
            }
            methods::TERMINAL_SETTINGS => RpcReply::value(&self.terminals.settings()),
            methods::SET_TERMINAL_COMMAND => {
                let p: SetTerminalCommand = parse_params(params)?;
                let snapshot = self
                    .terminals
                    .set_launch_command(p.command)
                    .map_err(|error| RpcError::Failed(error.to_string()))?;
                RpcReply::value(&snapshot)
            }

            methods::LIST_REPOS => RpcReply::value(&self.repos.list().await),
            methods::ADD_REPO => {
                #[derive(Deserialize)]
                struct P {
                    path: String,
                }
                let p: P = parse_params(params)?;
                let repo = self
                    .repos
                    .add(&p.path)
                    .await
                    .map_err(|e| RpcError::Failed(e.to_string()))?;
                RpcReply::value(&repo)
            }
            methods::CLONE_REPO => {
                #[derive(Deserialize)]
                struct P {
                    url: String,
                }
                let p: P = parse_params(params)?;
                let repo = self
                    .repos
                    .clone_repo(&p.url)
                    .await
                    .map_err(|e| RpcError::Failed(e.to_string()))?;
                RpcReply::value(&repo)
            }
            methods::CREATE_REPO => {
                #[derive(Deserialize)]
                struct P {
                    name: String,
                }
                let p: P = parse_params(params)?;
                let repo = self
                    .repos
                    .create(&p.name)
                    .await
                    .map_err(|e| RpcError::Failed(e.to_string()))?;
                RpcReply::value(&repo)
            }
            methods::LIST_BRANCHES => {
                let p: RepoPathParams = parse_params(params)?;
                let branches = self
                    .repos
                    .branches(std::path::Path::new(&p.repo_path))
                    .await
                    .map_err(|e| RpcError::Failed(e.to_string()))?;
                RpcReply::value(&branches)
            }
            methods::LIST_REFS => {
                let p: ListRefs = parse_params(params)?;
                let refs = self
                    .repos
                    .refs(std::path::Path::new(&p.repo_path))
                    .await
                    .map_err(|e| RpcError::Failed(e.to_string()))?;
                RpcReply::value(&refs)
            }
            methods::GET_CHECKOUT_VCS_STATUS => {
                let p: GetCheckoutVcsStatus = parse_params(params)?;
                RpcReply::value(&self.checkout_vcs_status(&p.chat_id).await?)
            }
            methods::RUN_VCS_ACTION => {
                let p: RunVcsAction = parse_params(params)?;
                let (tx, rx) = tokio::sync::mpsc::channel(32);
                let context = VcsActionTaskContext {
                    sessions: self.sessions.clone(),
                    workspace: self.workspace.clone(),
                    repos: self.repos.clone(),
                    diff_sync: self.diff_sync.clone(),
                    device_id: self.doc_host.device_id().to_string(),
                };
                tokio::spawn(run_vcs_action_task(context, p, tx));
                let stream = futures::stream::unfold(rx, |mut receiver| async move {
                    let event = receiver.recv().await?;
                    Some((serde_json::to_value(event).ok()?, receiver))
                });
                Ok(RpcReply::Stream(Box::pin(stream)))
            }
            methods::GET_CHECKOUT_REVIEW => {
                let p: GetCheckoutReview = parse_params(params)?;
                let chat = self
                    .workspace
                    .chat(&p.chat_id)
                    .map_err(|e| RpcError::Failed(e.to_string()))?
                    .ok_or_else(|| RpcError::Failed("chat not found".into()))?;
                if chat.device_id != self.doc_host.device_id() {
                    return Err(RpcError::Failed("chat belongs to another device".into()));
                }
                let cwd = chat
                    .cwd
                    .ok_or_else(|| RpcError::Failed("chat has no workspace folder".into()))?;
                let review = jolt_vcs::detect_review(&self.repos, std::path::Path::new(&cwd)).await;
                RpcReply::value(&review)
            }
            methods::SWITCH_REF => {
                let p: SwitchRef = parse_params(params)?;
                let branch = self
                    .repos
                    .switch_ref(std::path::Path::new(&p.repo_path), &p.ref_name)
                    .await
                    .map_err(|e| RpcError::Failed(e.to_string()))?;
                RpcReply::value(&serde_json::json!({ "branch": branch }))
            }
            methods::LIST_FOLDERS => {
                let p: ListFolders = parse_params(params)?;
                let listing = self
                    .repos
                    .list_folders(p.path)
                    .await
                    .map_err(|e| RpcError::Failed(e.to_string()))?;
                RpcReply::value(&listing)
            }
            methods::SEARCH_FILES => {
                let p: SearchFiles = parse_params(params)?;
                if p.query.chars().count() > 256 {
                    return Err(RpcError::BadParams(
                        "SearchFiles query must not exceed 256 characters".into(),
                    ));
                }
                let matches = tokio::time::timeout(FILE_SEARCH_RPC_TIMEOUT, async {
                    let root = self.file_search_root(&p).await?;
                    let featured_paths = p
                        .chat_id
                        .as_deref()
                        .filter(|_| p.query.is_empty())
                        .map(|chat_id| self.featured_file_paths(chat_id))
                        .unwrap_or_default();
                    self.repos
                        .search_files(root, p.query, featured_paths)
                        .await
                        .map_err(|e| RpcError::Failed(e.to_string()))
                })
                .await
                .map_err(|_| RpcError::Failed("file search timed out".into()))??;
                RpcReply::value(&matches)
            }
            methods::CREATE_WORKTREE => {
                let p: CreateWorktree = parse_params(params)?;
                let worktree = self
                    .repos
                    .create_worktree(std::path::Path::new(&p.repo_path), &p.branch)
                    .await
                    .map_err(|e| RpcError::Failed(e.to_string()))?;
                RpcReply::value(&worktree)
            }
            methods::DELETE_WORKTREE => {
                let p: DeleteWorktreeParams = parse_params(params)?;
                self.repos
                    .delete_worktree(
                        std::path::Path::new(&p.repo_path),
                        std::path::Path::new(&p.worktree_path),
                    )
                    .await
                    .map_err(|e| RpcError::Failed(e.to_string()))?;
                RpcReply::value(&serde_json::json!({ "ok": true }))
            }
            methods::OPEN_TERMINAL => {
                let p: OpenTerminal = parse_params(params)?;
                // The terminal runs in the chat's checkout; a chat with no cwd (or
                // no row yet) gets the home directory.
                let cwd = self
                    .workspace
                    .chat(&p.chat_id)
                    .ok()
                    .flatten()
                    .and_then(|chat| chat.cwd)
                    .unwrap_or_else(|| home_dir().to_string_lossy().to_string());
                let session = self
                    .terminals
                    .open_with_command(&cwd, p.cols, p.rows, p.command.as_deref())
                    .map_err(|e| RpcError::Failed(e.to_string()))?;
                RpcReply::value(&session)
            }
            methods::SUBSCRIBE_TERMINAL_V2 => {
                let p: SubscribeTerminal = parse_params(params)?;
                let rx = self
                    .terminals
                    .subscribe_output(&p.terminal_id, p.after_seq)
                    .map_err(|e| RpcError::Failed(e.to_string()))?;
                let stream = futures::stream::unfold(rx, |mut rx| async move {
                    let event = rx.recv().await?;
                    let encoded = match event {
                        TerminalOutput::Data { seq, data } => {
                            jolt_rpc::terminal_wire::encode_data(seq, &data)
                        }
                        TerminalOutput::Exit {
                            seq,
                            exit_code,
                            signal,
                        } => {
                            jolt_rpc::terminal_wire::encode_exit(seq, exit_code, signal.as_deref())
                        }
                        TerminalOutput::ReplayGap {
                            requested_after,
                            oldest_available,
                        } => jolt_rpc::terminal_wire::encode_replay_gap(
                            requested_after,
                            oldest_available,
                        ),
                    };
                    Some((encoded, rx))
                });
                Ok(RpcReply::BinaryStream(stream.boxed()))
            }
            methods::WRITE_TERMINAL => {
                let p: WriteTerminal = parse_params(params)?;
                self.terminals
                    .write(&p.terminal_id, &p.data)
                    .map_err(|e| RpcError::Failed(e.to_string()))?;
                RpcReply::value(&serde_json::json!({ "ok": true }))
            }
            methods::RESIZE_TERMINAL => {
                let p: ResizeTerminal = parse_params(params)?;
                self.terminals
                    .resize(&p.terminal_id, p.cols, p.rows)
                    .map_err(|e| RpcError::Failed(e.to_string()))?;
                RpcReply::value(&serde_json::json!({ "ok": true }))
            }
            methods::CLOSE_TERMINAL => {
                let p: CloseTerminal = parse_params(params)?;
                self.terminals
                    .close(&p.terminal_id)
                    .map_err(|e| RpcError::Failed(e.to_string()))?;
                RpcReply::value(&serde_json::json!({ "ok": true }))
            }
            methods::LIST_AGENT_ACCOUNTS => {
                let p: ListAgentAccounts = parse_params(params)?;
                let snapshot = if p.usage_only {
                    self.agent_accounts
                        .usage_snapshot(p.force_usage.unwrap_or(false))
                        .await
                } else {
                    self.agent_accounts
                        .list(p.force_usage.unwrap_or(false))
                        .await
                }
                .map_err(|e| RpcError::Failed(e.to_string()))?;
                RpcReply::value(&snapshot)
            }
            methods::ACTIVATE_AGENT_ACCOUNT => {
                let p: ActivateAgentAccount = parse_params(params)?;
                let snapshot = self
                    .agent_accounts
                    .activate(p.harness, &p.account_id)
                    .await
                    .map_err(|e| RpcError::Failed(e.to_string()))?;
                RpcReply::value(&snapshot)
            }
            methods::FORGET_AGENT_ACCOUNT => {
                let p: ForgetAgentAccount = parse_params(params)?;
                let snapshot = self
                    .agent_accounts
                    .forget(p.harness, &p.account_id)
                    .await
                    .map_err(|e| RpcError::Failed(e.to_string()))?;
                RpcReply::value(&snapshot)
            }
            methods::START_AGENT_LOGIN => {
                let p: StartAgentLogin = parse_params(params)?;
                let start = self
                    .agent_accounts
                    .start_login(p.harness)
                    .await
                    .map_err(|e| RpcError::Failed(e.to_string()))?;
                RpcReply::value(&start)
            }
            methods::COMPLETE_AGENT_LOGIN => {
                let p: CompleteAgentLogin = parse_params(params)?;
                let snapshot = self
                    .agent_accounts
                    .complete_login(&p.login_id, &p.code)
                    .await
                    .map_err(|e| RpcError::Failed(e.to_string()))?;
                RpcReply::value(&snapshot)
            }
            methods::POLL_AGENT_LOGIN => {
                let p: PollAgentLogin = parse_params(params)?;
                let poll = self
                    .agent_accounts
                    .poll_login(&p.login_id)
                    .await
                    .map_err(|e| RpcError::Failed(e.to_string()))?;
                RpcReply::value(&poll)
            }
            methods::CANCEL_AGENT_LOGIN => {
                let p: CancelAgentLogin = parse_params(params)?;
                self.agent_accounts.cancel_login(&p.login_id);
                RpcReply::value(&serde_json::json!({ "ok": true }))
            }
            methods::LIST_HARNESS_SECRETS => {
                let _: ListHarnessSecrets = parse_params(params)?;
                RpcReply::value(&self.secrets.snapshot().await)
            }
            methods::UPSERT_HARNESS_SECRET => {
                let p: UpsertHarnessSecret = parse_params(params)?;
                let snapshot = self
                    .secrets
                    .upsert(
                        p.id.as_deref(),
                        &p.label,
                        &p.environment_variable,
                        p.harnesses,
                        p.value.as_deref(),
                    )
                    .await
                    .map_err(|error| RpcError::Failed(error.to_string()))?;
                RpcReply::value(&snapshot)
            }
            methods::DELETE_HARNESS_SECRET => {
                let p: DeleteHarnessSecret = parse_params(params)?;
                let snapshot = self
                    .secrets
                    .delete(&p.id)
                    .await
                    .map_err(|error| RpcError::Failed(error.to_string()))?;
                RpcReply::value(&snapshot)
            }
            methods::GET_TRANSPORT_CAPABILITIES => {
                let _: GetTransportCapabilities = parse_params(params)?;
                RpcReply::value(&jolt_api::TransportCapabilities { binary_unary: true })
            }
            methods::UPLOAD_CHUNK => {
                let p: UploadChunk = parse_params(params)?;
                self.uploads
                    .append(&p.upload_id, &p.data, p.seq)
                    .map_err(|e| RpcError::Failed(e.to_string()))?;
                RpcReply::value(&serde_json::json!({ "ok": true }))
            }
            methods::UPLOAD_COMMIT => {
                let p: UploadCommit = parse_params(params)?;
                let committed = self
                    .uploads
                    .commit(&p.upload_id, &p.file_name, &p.chat_id)
                    .await
                    .map_err(|e| RpcError::Failed(e.to_string()))?;
                RpcReply::value(&committed)
            }
            methods::READ_ATTACHMENT_CHUNK => {
                let p: ReadAttachmentChunk = parse_params(params)?;
                // Path jail: the uploads dir plus every workspace-known chat cwd.
                let roots: Vec<std::path::PathBuf> = self
                    .workspace
                    .read_chats()
                    .unwrap_or_default()
                    .into_iter()
                    .filter_map(|chat| chat.cwd)
                    .map(std::path::PathBuf::from)
                    .collect();
                let chunk = self
                    .uploads
                    .read_chunk(&p.path, p.offset, &roots)
                    .map_err(|e| RpcError::Failed(e.to_string()))?;
                RpcReply::value(&chunk)
            }
            other => Err(RpcError::UnknownMethod(other.to_string())),
        }
    }

    async fn handle_binary(
        &self,
        method: &str,
        params: serde_json::Value,
        payload: Bytes,
    ) -> Result<RpcReply, RpcError> {
        if forwardable(method)
            && let Some(target) = params
                .get("targetDeviceId")
                .and_then(|value| value.as_str())
            && target != self.doc_host.device_id()
        {
            let target = target.to_owned();
            return self.forward_binary(&target, method, params, payload).await;
        }
        match method {
            methods::UPLOAD_BINARY_CHUNK => {
                let request: UploadBinaryChunk = parse_params(params)?;
                self.uploads
                    .append_bytes(&request.upload_id, &payload, request.seq)
                    .map_err(|error| RpcError::Failed(error.to_string()))?;
                RpcReply::value(&serde_json::json!({ "ok": true }))
            }
            other => Err(RpcError::UnknownMethod(other.to_owned())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chat(id: &str, title: &str, archived: bool, minutes_ago: i64) -> Chat {
        let created_at = chrono::Utc::now() - chrono::TimeDelta::minutes(minutes_ago);
        Chat {
            id: id.into(),
            device_id: "device".into(),
            title: Some(title.into()),
            archived,
            pinned: false,
            cwd: None,
            branch: None,
            checkout_id: None,
            config: None,
            last_message_preview: None,
            last_message_at: Some(created_at),
            created_at,
            harness_session_id: None,
            harness_session_cwd: None,
            harness_conversations: Vec::new(),
            space_id: Some("space".into()),
            last_seen_at: None,
            goal: None,
        }
    }

    #[tokio::test]
    async fn chat_watch_bootstraps_active_rows_then_sends_only_changes() {
        let active = chat("active", "Active", false, 1);
        let archived = chat("archived", "Archived", true, 2);
        let (tx, rx) = watch::channel(vec![active.clone(), archived.clone()]);
        let mut stream = chat_stream(rx);
        let opening: ChatWatchFrame = serde_json::from_value(stream.next().await.unwrap()).unwrap();
        match opening {
            ChatWatchFrame::Bootstrap { chats } => {
                assert_eq!(
                    chats
                        .iter()
                        .map(|chat| chat.id.as_str())
                        .collect::<Vec<_>>(),
                    ["active"]
                );
            }
            ChatWatchFrame::Delta { .. } => panic!("expected bootstrap"),
        }

        let mut changed = archived;
        changed.title = Some("Renamed".into());
        tx.send_replace(vec![active, changed]);
        let delta: ChatWatchFrame = serde_json::from_value(stream.next().await.unwrap()).unwrap();
        match delta {
            ChatWatchFrame::Delta {
                upserts,
                removed_ids,
            } => {
                assert_eq!(
                    upserts
                        .iter()
                        .map(|chat| chat.id.as_str())
                        .collect::<Vec<_>>(),
                    ["archived"]
                );
                assert!(removed_ids.is_empty());
            }
            ChatWatchFrame::Bootstrap { .. } => panic!("expected delta"),
        }
    }

    #[tokio::test]
    async fn session_watch_sends_only_changed_status_rows() {
        let now = chrono::Utc::now();
        let idle = Session {
            chat_id: "chat".into(),
            device_id: "device".into(),
            status: SessionStatus::Idle,
            compacting: false,
            started_at: None,
            updated_at: now,
        };
        let (tx, rx) = watch::channel(vec![idle.clone()]);
        let mut stream = session_stream(rx);
        let opening: SessionWatchFrame =
            serde_json::from_value(stream.next().await.unwrap()).unwrap();
        assert!(matches!(opening, SessionWatchFrame::Bootstrap { .. }));

        let mut working = idle;
        working.status = SessionStatus::Working;
        tx.send_replace(vec![working]);
        let delta: SessionWatchFrame =
            serde_json::from_value(stream.next().await.unwrap()).unwrap();
        match delta {
            SessionWatchFrame::Delta {
                upserts,
                removed_chat_ids,
            } => {
                assert_eq!(upserts.len(), 1);
                assert_eq!(upserts[0].status, SessionStatus::Working);
                assert!(removed_chat_ids.is_empty());
            }
            SessionWatchFrame::Bootstrap { .. } => panic!("expected delta"),
        }
    }

    #[test]
    fn chat_query_searches_active_and_archived_with_stable_pages() {
        let chats = vec![
            chat("active", "Navigation work", false, 1),
            chat("new", "Navigation polish", true, 2),
            chat("old", "Navigation history", true, 3),
            chat("other", "Composer", true, 0),
        ];
        let first = query_chats(
            chats.clone(),
            &QueryChats {
                section: ChatSection::Any,
                query: "NAVIGATION".into(),
                limit: 2,
                ..QueryChats::default()
            },
        )
        .expect("first page");
        assert_eq!(first.total, 3);
        assert_eq!(
            first
                .chats
                .iter()
                .map(|chat| chat.id.as_str())
                .collect::<Vec<_>>(),
            ["active", "new"]
        );
        let second = query_chats(
            chats,
            &QueryChats {
                section: ChatSection::Any,
                query: "navigation".into(),
                cursor: first.next_cursor,
                limit: 2,
                ..QueryChats::default()
            },
        )
        .expect("second page");
        assert_eq!(
            second
                .chats
                .iter()
                .map(|chat| chat.id.as_str())
                .collect::<Vec<_>>(),
            ["old"]
        );
        assert!(second.next_cursor.is_none());
    }

    /// The UI's Switch/Forget calls send `{id, accountId, harness}` (+ optional
    /// `targetDeviceId`); the extra fields must be tolerated, `accountId` wins.
    #[test]
    fn agent_account_params_accept_ui_shape() {
        let p: ActivateAgentAccount = parse_params(serde_json::json!({
            "id": "acct-1",
            "accountId": "acct-1",
            "harness": "claude-code",
            "targetDeviceId": "dev-2",
        }))
        .expect("ui param shape");
        assert_eq!(p.account_id, "acct-1");
        assert_eq!(p.harness, HarnessId::ClaudeCode);
    }

    #[test]
    fn local_device_is_not_forwardable() {
        assert!(!forwardable(methods::LOCAL_DEVICE));
        assert!(forwardable(methods::QUEUE_COMMAND));
        assert!(forwardable(methods::LIST_COMMANDS));
        assert!(forwardable(methods::SEARCH_TRANSCRIPT));
        assert!(forwardable(methods::SEARCH_FILES));
        assert!(forwardable(methods::GET_CHECKOUT_REVIEW));
        assert!(forwardable(methods::REGENERATE_CHAT_TITLE));
        assert!(forwardable(methods::GET_TURN_DIFF_PAGE));
        assert!(forwardable(methods::PIN_DIFF_DOCUMENT));
        assert!(forwardable(methods::SUBSCRIBE_TERMINAL_V2));
        assert!(forwardable(methods::GET_TRANSPORT_CAPABILITIES));
        assert!(forwardable(methods::UPLOAD_BINARY_CHUNK));
        assert!(is_binary_stream_method(methods::SUBSCRIBE_TERMINAL_V2));
        assert!(!forwardable(methods::GET_REVIEW_DRAFT));
        assert!(local_only(methods::GET_REVIEW_DRAFT));
        assert!(!forwardable(methods::UPSERT_HARNESS_SECRET));
        assert!(local_only(methods::UPSERT_HARNESS_SECRET));
    }

    #[test]
    fn command_catalog_contains_only_native_jolt_commands() {
        let commands = jolt_commands();
        assert_eq!(commands.len(), 3);
        assert_eq!(commands[0].name, "answer");
        assert_eq!(commands[1].name, "bro");
        assert_eq!(commands[2].name, "goal");
        assert!(
            commands
                .iter()
                .all(|command| command.source == AgentCommandSource::Jolt)
        );
    }

    #[test]
    fn tool_file_paths_keep_workspace_activity_only() {
        assert_eq!(
            tool_file_path(&ToolCall::EditFile {
                path: "src/main.rs".into(),
                old_string: None,
                new_string: None,
            }),
            Some("src/main.rs")
        );
        assert_eq!(
            tool_file_path(&ToolCall::Exec {
                command: "cargo test".into(),
            }),
            None
        );
    }
}
