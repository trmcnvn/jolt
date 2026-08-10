use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use jolt_proto::{
    AgentAccountsSnapshot, AgentCommand, AgentLoginPoll, AgentLoginStart, Chat, ChatConfig,
    CheckoutDiffPage, CheckoutDiffWatchFrame, CheckoutReview, CheckoutVcsStatus, Device,
    ExtractQuestionsResult, FileSearchMatch, FolderListing, HarnessId, HarnessSecretsSnapshot,
    HarnessUpdateStatus, Model, RepoRef, ReviewDraft, Session, Space, TerminalSession,
    ThemeFileRecord, UsageBreakdown, UsageSummary, VcsAction, VcsActionEvent, VcsKind,
    VcsSettingsSnapshot, Worktree,
};
use jolt_session_doc::{
    QueuedPrompt, SessionCommandPayload, TranscriptPage, TranscriptSearchResult,
    TranscriptWatchFrame,
};

use crate::{
    Acknowledged, AttachmentChunk, AuthUrl, Cancellation, CommittedAttachment, HarnessDescriptor,
    LocalDevice, QueuedCommand, ScopeKind, ScopeStatus, SwitchedRef, methods,
};

pub trait UnaryRequest: Serialize {
    type Response: DeserializeOwned;
    const METHOD: &'static str;
}

pub async fn call<R: UnaryRequest>(
    client: &jolt_rpc::RpcClient,
    request: &R,
) -> Result<R::Response, jolt_rpc::RpcError> {
    let params = serialize_request(request)?;
    client.call_as(R::METHOD, params).await
}

pub trait StreamRequest: Serialize {
    type Item: DeserializeOwned;
    const METHOD: &'static str;
}

pub async fn subscribe<R: StreamRequest>(
    client: &jolt_rpc::RpcClient,
    request: &R,
) -> Result<tokio::sync::mpsc::Receiver<serde_json::Value>, jolt_rpc::RpcError> {
    client
        .subscribe(R::METHOD, serialize_request(request)?)
        .await
}

pub trait BinaryUnaryRequest: Serialize {
    type Response: DeserializeOwned;
    const METHOD: &'static str;
}

pub async fn call_binary<R: BinaryUnaryRequest>(
    client: &jolt_rpc::RpcClient,
    request: &R,
    payload: bytes::Bytes,
) -> Result<R::Response, jolt_rpc::RpcError> {
    let params = serialize_request(request)?;
    let value = client.call_binary(R::METHOD, params, payload).await?;
    serde_json::from_value(value).map_err(|error| jolt_rpc::RpcError::BadParams(error.to_string()))
}

pub trait BinaryStreamRequest: Serialize {
    const METHOD: &'static str;
}

pub async fn subscribe_binary<R: BinaryStreamRequest>(
    client: &jolt_rpc::RpcClient,
    request: &R,
) -> Result<tokio::sync::mpsc::Receiver<bytes::Bytes>, jolt_rpc::RpcError> {
    client
        .subscribe_binary(R::METHOD, serialize_request(request)?)
        .await
}

fn serialize_request(request: &impl Serialize) -> Result<serde_json::Value, jolt_rpc::RpcError> {
    serde_json::to_value(request).map_err(|error| jolt_rpc::RpcError::BadParams(error.to_string()))
}

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ListHarnesses {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_device_id: Option<String>,
}

impl UnaryRequest for ListHarnesses {
    type Response = Vec<HarnessDescriptor>;
    const METHOD: &'static str = methods::LIST_HARNESSES;
}

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WatchUpdateStatus {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_device_id: Option<String>,
}

impl StreamRequest for WatchUpdateStatus {
    type Item = jolt_update::UpdateStatus;
    const METHOD: &'static str = methods::UPDATE_STATUS;
}

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ApplyUpdate {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_device_id: Option<String>,
}

impl UnaryRequest for ApplyUpdate {
    type Response = Acknowledged;
    const METHOD: &'static str = methods::APPLY_UPDATE;
}

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WatchHarnessUpdates {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_device_id: Option<String>,
}

impl StreamRequest for WatchHarnessUpdates {
    type Item = Vec<HarnessUpdateStatus>;
    const METHOD: &'static str = methods::WATCH_HARNESS_UPDATES;
}

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CheckHarnessUpdates {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_device_id: Option<String>,
}

impl UnaryRequest for CheckHarnessUpdates {
    type Response = Acknowledged;
    const METHOD: &'static str = methods::CHECK_HARNESS_UPDATES;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ApplyHarnessUpdate {
    pub harness: HarnessId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_device_id: Option<String>,
}

impl UnaryRequest for ApplyHarnessUpdate {
    type Response = Acknowledged;
    const METHOD: &'static str = methods::APPLY_HARNESS_UPDATE;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListModels {
    pub harness: HarnessId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_device_id: Option<String>,
}

impl UnaryRequest for ListModels {
    type Response = Vec<Model>;
    const METHOD: &'static str = methods::LIST_MODELS;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListCommands {
    pub harness: HarnessId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default, skip_serializing_if = "serde_json::Map::is_empty")]
    pub model_options: serde_json::Map<String, serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_device_id: Option<String>,
}

impl UnaryRequest for ListCommands {
    type Response = Vec<AgentCommand>;
    const METHOD: &'static str = methods::LIST_COMMANDS;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QueueCommand {
    pub chat_id: String,
    pub command: SessionCommandPayload,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_device_id: Option<String>,
}

impl UnaryRequest for QueueCommand {
    type Response = QueuedCommand;
    const METHOD: &'static str = methods::QUEUE_COMMAND;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CancelQueuedPrompt {
    pub chat_id: String,
    pub command_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_device_id: Option<String>,
}

impl UnaryRequest for CancelQueuedPrompt {
    type Response = Cancellation;
    const METHOD: &'static str = methods::CANCEL_QUEUED_PROMPT;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GetTranscriptPage {
    pub chat_id: String,
    pub page_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_device_id: Option<String>,
}

impl UnaryRequest for GetTranscriptPage {
    type Response = TranscriptPage;
    const METHOD: &'static str = methods::GET_TRANSCRIPT_PAGE;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchTranscript {
    pub chat_id: String,
    pub query: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_device_id: Option<String>,
}

impl UnaryRequest for SearchTranscript {
    type Response = Vec<TranscriptSearchResult>;
    const METHOD: &'static str = methods::SEARCH_TRANSCRIPT;
}

/// Explicit permanent-host-loss recovery. The caller supplies a fresh chat id
/// and targets the engine that owns `space_id`; the source chat remains intact.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateRecoveryFork {
    pub source_chat_id: String,
    pub chat_id: String,
    pub space_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_device_id: Option<String>,
}

impl UnaryRequest for CreateRecoveryFork {
    type Response = Acknowledged;
    const METHOD: &'static str = methods::CREATE_RECOVERY_FORK;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExtractQuestions {
    pub chat_id: String,
    pub source_message_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_device_id: Option<String>,
}

impl UnaryRequest for ExtractQuestions {
    type Response = ExtractQuestionsResult;
    const METHOD: &'static str = methods::EXTRACT_QUESTIONS;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListRefs {
    #[serde(alias = "repo")]
    pub repo_path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_device_id: Option<String>,
}

impl UnaryRequest for ListRefs {
    type Response = Vec<RepoRef>;
    const METHOD: &'static str = methods::LIST_REFS;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GetCheckoutReview {
    pub chat_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_device_id: Option<String>,
}

impl UnaryRequest for GetCheckoutReview {
    type Response = Option<CheckoutReview>;
    const METHOD: &'static str = methods::GET_CHECKOUT_REVIEW;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GetCheckoutVcsStatus {
    pub chat_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_device_id: Option<String>,
}

impl UnaryRequest for GetCheckoutVcsStatus {
    type Response = CheckoutVcsStatus;
    const METHOD: &'static str = methods::GET_CHECKOUT_VCS_STATUS;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunVcsAction {
    pub action_id: String,
    pub chat_id: String,
    pub action: VcsAction,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_device_id: Option<String>,
}

impl StreamRequest for RunVcsAction {
    type Item = VcsActionEvent;
    const METHOD: &'static str = methods::RUN_VCS_ACTION;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SwitchRef {
    pub repo_path: String,
    pub ref_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_device_id: Option<String>,
}

impl UnaryRequest for SwitchRef {
    type Response = SwitchedRef;
    const METHOD: &'static str = methods::SWITCH_REF;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListFolders {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_device_id: Option<String>,
}

impl UnaryRequest for ListFolders {
    type Response = FolderListing;
    const METHOD: &'static str = methods::LIST_FOLDERS;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchFiles {
    pub query: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chat_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub space_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_device_id: Option<String>,
}

impl UnaryRequest for SearchFiles {
    type Response = Vec<FileSearchMatch>;
    const METHOD: &'static str = methods::SEARCH_FILES;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateWorktree {
    #[serde(alias = "repo")]
    pub repo_path: String,
    pub branch: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_device_id: Option<String>,
}

impl UnaryRequest for CreateWorktree {
    type Response = Worktree;
    const METHOD: &'static str = methods::CREATE_WORKTREE;
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VcsSettings {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_device_id: Option<String>,
}

impl UnaryRequest for VcsSettings {
    type Response = VcsSettingsSnapshot;
    const METHOD: &'static str = methods::VCS_SETTINGS;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetVcsBackend {
    pub backend: VcsKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_device_id: Option<String>,
}

impl UnaryRequest for SetVcsBackend {
    type Response = VcsSettingsSnapshot;
    const METHOD: &'static str = methods::SET_VCS_BACKEND;
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TerminalSettings {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_device_id: Option<String>,
}

impl UnaryRequest for TerminalSettings {
    type Response = jolt_proto::TerminalSettingsSnapshot;
    const METHOD: &'static str = methods::TERMINAL_SETTINGS;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetTerminalCommand {
    pub command: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_device_id: Option<String>,
}

impl UnaryRequest for SetTerminalCommand {
    type Response = jolt_proto::TerminalSettingsSnapshot;
    const METHOD: &'static str = methods::SET_TERMINAL_COMMAND;
}

/// A mutation against the synchronized workspace document.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "camelCase")]
pub enum Mutate {
    #[serde(rename_all = "camelCase")]
    CreateChat {
        chat_id: String,
        space_id: String,
        #[serde(default)]
        config: Option<ChatConfig>,
        #[serde(default)]
        branch: Option<String>,
        #[serde(default)]
        cwd: Option<String>,
    },
    #[serde(rename_all = "camelCase")]
    CreateSpace {
        space_id: String,
        device_id: String,
        path: String,
        #[serde(default)]
        name: Option<String>,
        #[serde(default)]
        git_detected: bool,
    },
    #[serde(rename_all = "camelCase")]
    RenameSpace {
        space_id: String,
        #[serde(default)]
        name: Option<String>,
    },
    #[serde(rename_all = "camelCase")]
    DeleteSpace { space_id: String },
    #[serde(rename_all = "camelCase")]
    RenameChat { chat_id: String, title: String },
    #[serde(rename_all = "camelCase")]
    SetChatBranch { chat_id: String, branch: String },
    #[serde(rename_all = "camelCase")]
    SetChatCwd { chat_id: String, cwd: String },
    #[serde(rename_all = "camelCase")]
    SetChatActivity {
        chat_id: String,
        #[serde(default)]
        last_message_at: Option<i64>,
        #[serde(default)]
        created_at: Option<i64>,
    },
    #[serde(rename_all = "camelCase")]
    SetChatHost { chat_id: String, device_id: String },
    #[serde(rename_all = "camelCase")]
    SetChatPinned { chat_id: String, pinned: bool },
    #[serde(rename_all = "camelCase")]
    SetChatArchived { chat_id: String, archived: bool },
    #[serde(rename_all = "camelCase")]
    SetChatConfig { chat_id: String, config: ChatConfig },
    #[serde(rename_all = "camelCase")]
    DeleteChat { chat_id: String },
    #[serde(rename_all = "camelCase")]
    RenameDevice { device_id: String, name: String },
    #[serde(rename_all = "camelCase")]
    DeleteDevice { device_id: String },
    #[serde(rename_all = "camelCase")]
    MarkChatSeen {
        chat_id: String,
        #[serde(default)]
        at: Option<i64>,
    },
}

impl UnaryRequest for Mutate {
    type Response = Acknowledged;
    const METHOD: &'static str = methods::MUTATE;
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListAgentAccounts {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub force_usage: Option<bool>,
    #[serde(default)]
    pub usage_only: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_device_id: Option<String>,
}

impl UnaryRequest for ListAgentAccounts {
    type Response = AgentAccountsSnapshot;
    const METHOD: &'static str = methods::LIST_AGENT_ACCOUNTS;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ActivateAgentAccount {
    pub harness: HarnessId,
    pub account_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_device_id: Option<String>,
}

impl UnaryRequest for ActivateAgentAccount {
    type Response = AgentAccountsSnapshot;
    const METHOD: &'static str = methods::ACTIVATE_AGENT_ACCOUNT;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ForgetAgentAccount {
    pub harness: HarnessId,
    pub account_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_device_id: Option<String>,
}

impl UnaryRequest for ForgetAgentAccount {
    type Response = AgentAccountsSnapshot;
    const METHOD: &'static str = methods::FORGET_AGENT_ACCOUNT;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StartAgentLogin {
    pub harness: HarnessId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_device_id: Option<String>,
}

impl UnaryRequest for StartAgentLogin {
    type Response = AgentLoginStart;
    const METHOD: &'static str = methods::START_AGENT_LOGIN;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompleteAgentLogin {
    pub login_id: String,
    pub code: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_device_id: Option<String>,
}

impl UnaryRequest for CompleteAgentLogin {
    type Response = AgentAccountsSnapshot;
    const METHOD: &'static str = methods::COMPLETE_AGENT_LOGIN;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PollAgentLogin {
    pub login_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_device_id: Option<String>,
}

impl UnaryRequest for PollAgentLogin {
    type Response = AgentLoginPoll;
    const METHOD: &'static str = methods::POLL_AGENT_LOGIN;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CancelAgentLogin {
    pub login_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_device_id: Option<String>,
}

impl UnaryRequest for CancelAgentLogin {
    type Response = Acknowledged;
    const METHOD: &'static str = methods::CANCEL_AGENT_LOGIN;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GetCheckoutDiffPage {
    pub chat_id: String,
    pub catalog_revision: String,
    pub page_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_device_id: Option<String>,
}

impl UnaryRequest for GetCheckoutDiffPage {
    type Response = CheckoutDiffPage;
    const METHOD: &'static str = methods::GET_CHECKOUT_DIFF_PAGE;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GetTurnDiffPage {
    pub chat_id: String,
    pub assistant_message_id: String,
    pub catalog_revision: String,
    pub page_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_device_id: Option<String>,
}

impl UnaryRequest for GetTurnDiffPage {
    type Response = CheckoutDiffPage;
    const METHOD: &'static str = methods::GET_TURN_DIFF_PAGE;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GetReviewDraft {
    pub review_key: String,
}

impl UnaryRequest for GetReviewDraft {
    type Response = Option<ReviewDraft>;
    const METHOD: &'static str = methods::GET_REVIEW_DRAFT;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PutReviewDraft {
    pub draft: ReviewDraft,
}

impl UnaryRequest for PutReviewDraft {
    type Response = Acknowledged;
    const METHOD: &'static str = methods::PUT_REVIEW_DRAFT;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeleteReviewDraft {
    pub review_key: String,
}

impl UnaryRequest for DeleteReviewDraft {
    type Response = Acknowledged;
    const METHOD: &'static str = methods::DELETE_REVIEW_DRAFT;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PinDiffDocument {
    pub chat_id: String,
    pub catalog_revision: String,
    pub review_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_device_id: Option<String>,
}

impl UnaryRequest for PinDiffDocument {
    type Response = Acknowledged;
    const METHOD: &'static str = methods::PIN_DIFF_DOCUMENT;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReleaseDiffDocument {
    pub catalog_revision: String,
    pub review_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_device_id: Option<String>,
}

impl UnaryRequest for ReleaseDiffDocument {
    type Response = Acknowledged;
    const METHOD: &'static str = methods::RELEASE_DIFF_DOCUMENT;
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ListHarnessSecrets {}

impl UnaryRequest for ListHarnessSecrets {
    type Response = HarnessSecretsSnapshot;
    const METHOD: &'static str = methods::LIST_HARNESS_SECRETS;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpsertHarnessSecret {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub label: String,
    pub environment_variable: String,
    pub harnesses: Vec<HarnessId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
}

impl UnaryRequest for UpsertHarnessSecret {
    type Response = HarnessSecretsSnapshot;
    const METHOD: &'static str = methods::UPSERT_HARNESS_SECRET;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeleteHarnessSecret {
    pub id: String,
}

impl UnaryRequest for DeleteHarnessSecret {
    type Response = HarnessSecretsSnapshot;
    const METHOD: &'static str = methods::DELETE_HARNESS_SECRET;
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SignIn {}

impl UnaryRequest for SignIn {
    type Response = AuthUrl;
    const METHOD: &'static str = methods::SIGN_IN;
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SignOut {}

impl UnaryRequest for SignOut {
    type Response = Acknowledged;
    const METHOD: &'static str = methods::SIGN_OUT;
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StopEngine {}

impl UnaryRequest for StopEngine {
    type Response = Acknowledged;
    const METHOD: &'static str = methods::STOP_ENGINE;
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EnsurePersonalOrg {}

impl UnaryRequest for EnsurePersonalOrg {
    type Response = Acknowledged;
    const METHOD: &'static str = methods::ENSURE_PERSONAL_ORG;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RegenerateChatTitle {
    pub chat_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_device_id: Option<String>,
}

impl UnaryRequest for RegenerateChatTitle {
    type Response = Acknowledged;
    const METHOD: &'static str = methods::REGENERATE_CHAT_TITLE;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageBreakdownRequest {
    pub days: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_device_id: Option<String>,
}

impl UnaryRequest for UsageBreakdownRequest {
    type Response = UsageBreakdown;
    const METHOD: &'static str = methods::USAGE_BREAKDOWN;
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ListThemes {}

impl UnaryRequest for ListThemes {
    type Response = Vec<ThemeFileRecord>;
    const METHOD: &'static str = methods::LIST_THEMES;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpsertThemes {
    pub themes: Vec<ThemeFileRecord>,
}

impl UnaryRequest for UpsertThemes {
    type Response = Acknowledged;
    const METHOD: &'static str = methods::UPSERT_THEMES;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeleteTheme {
    pub id: String,
}

impl UnaryRequest for DeleteTheme {
    type Response = Acknowledged;
    const METHOD: &'static str = methods::DELETE_THEME;
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProbeSync {}

impl UnaryRequest for ProbeSync {
    type Response = serde_json::Value;
    const METHOD: &'static str = methods::PROBE_SYNC;
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GetLocalDevice {}

impl UnaryRequest for GetLocalDevice {
    type Response = LocalDevice;
    const METHOD: &'static str = methods::LOCAL_DEVICE;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum ChatWatchFrame {
    Bootstrap {
        chats: Vec<Chat>,
    },
    Delta {
        upserts: Vec<Chat>,
        removed_ids: Vec<String>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum SessionWatchFrame {
    Bootstrap {
        sessions: Vec<Session>,
    },
    Delta {
        upserts: Vec<Session>,
        removed_chat_ids: Vec<String>,
    },
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ChatSection {
    Active,
    Archived,
    #[default]
    Any,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QueryChats {
    #[serde(default)]
    pub section: ChatSection,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub space_id: Option<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub query: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
    #[serde(default)]
    pub limit: u16,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChatPage {
    pub chats: Vec<Chat>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
    pub total: usize,
}

impl UnaryRequest for QueryChats {
    type Response = ChatPage;
    const METHOD: &'static str = methods::QUERY_CHATS;
}

macro_rules! empty_stream_request {
    ($name:ident, $item:ty, $method:ident) => {
        #[derive(Debug, Clone, Default, Serialize, Deserialize)]
        pub struct $name {}

        impl StreamRequest for $name {
            type Item = $item;
            const METHOD: &'static str = methods::$method;
        }
    };
}

empty_stream_request!(WatchChats, ChatWatchFrame, WATCH_CHATS);
empty_stream_request!(WatchDevices, Vec<Device>, WATCH_DEVICES);
empty_stream_request!(WatchSessions, SessionWatchFrame, WATCH_SESSIONS);
empty_stream_request!(WatchSpaces, Vec<Space>, WATCH_SPACES);
empty_stream_request!(WatchAuthStatus, serde_json::Value, AUTH_STATUS);
empty_stream_request!(WatchScopeStatus, ScopeStatus, SCOPE_STATUS);
empty_stream_request!(WatchThemes, Vec<ThemeFileRecord>, WATCH_THEMES);

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WatchChatUsage {
    pub chat_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_device_id: Option<String>,
}

impl StreamRequest for WatchChatUsage {
    type Item = UsageSummary;
    const METHOD: &'static str = methods::WATCH_CHAT_USAGE;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WatchQueuedPrompts {
    pub chat_id: String,
}

impl StreamRequest for WatchQueuedPrompts {
    type Item = Vec<QueuedPrompt>;
    const METHOD: &'static str = methods::WATCH_QUEUED_PROMPTS;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WatchTranscript {
    pub chat_id: String,
}

impl StreamRequest for WatchTranscript {
    type Item = TranscriptWatchFrame;
    const METHOD: &'static str = methods::WATCH_TRANSCRIPT_V2;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WatchCheckoutDiff {
    pub chat_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_device_id: Option<String>,
}

impl StreamRequest for WatchCheckoutDiff {
    type Item = CheckoutDiffWatchFrame;
    const METHOD: &'static str = methods::WATCH_CHECKOUT_DIFF_V2;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SubscribeTerminal {
    pub terminal_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after_seq: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_device_id: Option<String>,
}

impl BinaryStreamRequest for SubscribeTerminal {
    const METHOD: &'static str = methods::SUBSCRIBE_TERMINAL_V2;
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SwitchScope {
    pub scope: ScopeKind,
}

impl UnaryRequest for SwitchScope {
    type Response = ScopeStatus;
    const METHOD: &'static str = methods::SWITCH_SCOPE;
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ResolveAccountLink {
    pub merge: bool,
}

impl UnaryRequest for ResolveAccountLink {
    type Response = ScopeStatus;
    const METHOD: &'static str = methods::RESOLVE_ACCOUNT_LINK;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GetTransportCapabilities {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_device_id: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TransportCapabilities {
    pub binary_unary: bool,
}

impl UnaryRequest for GetTransportCapabilities {
    type Response = TransportCapabilities;
    const METHOD: &'static str = methods::GET_TRANSPORT_CAPABILITIES;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UploadChunk {
    pub upload_id: String,
    pub data: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seq: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_device_id: Option<String>,
}

impl UnaryRequest for UploadChunk {
    type Response = Acknowledged;
    const METHOD: &'static str = methods::UPLOAD_CHUNK;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UploadBinaryChunk {
    pub upload_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seq: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_device_id: Option<String>,
}

impl BinaryUnaryRequest for UploadBinaryChunk {
    type Response = Acknowledged;
    const METHOD: &'static str = methods::UPLOAD_BINARY_CHUNK;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UploadCommit {
    pub upload_id: String,
    pub file_name: String,
    pub chat_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_device_id: Option<String>,
}

impl UnaryRequest for UploadCommit {
    type Response = CommittedAttachment;
    const METHOD: &'static str = methods::UPLOAD_COMMIT;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReadAttachmentChunk {
    pub path: String,
    #[serde(default)]
    pub offset: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_device_id: Option<String>,
}

impl UnaryRequest for ReadAttachmentChunk {
    type Response = AttachmentChunk;
    const METHOD: &'static str = methods::READ_ATTACHMENT_CHUNK;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OpenTerminal {
    pub chat_id: String,
    pub cols: u16,
    pub rows: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_device_id: Option<String>,
}

impl UnaryRequest for OpenTerminal {
    type Response = TerminalSession;
    const METHOD: &'static str = methods::OPEN_TERMINAL;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WriteTerminal {
    pub terminal_id: String,
    /// Base64-encoded input bytes.
    pub data: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_device_id: Option<String>,
}

impl UnaryRequest for WriteTerminal {
    type Response = Acknowledged;
    const METHOD: &'static str = methods::WRITE_TERMINAL;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResizeTerminal {
    pub terminal_id: String,
    pub cols: u16,
    pub rows: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_device_id: Option<String>,
}

impl UnaryRequest for ResizeTerminal {
    type Response = Acknowledged;
    const METHOD: &'static str = methods::RESIZE_TERMINAL;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CloseTerminal {
    pub terminal_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_device_id: Option<String>,
}

impl UnaryRequest for CloseTerminal {
    type Response = Acknowledged;
    const METHOD: &'static str = methods::CLOSE_TERMINAL;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attachment_requests_use_wire_field_names() {
        let chunk = UploadChunk {
            upload_id: "upload-1".into(),
            data: "YWJj".into(),
            seq: Some(2),
            target_device_id: Some("device-1".into()),
        };
        assert_eq!(
            serde_json::to_value(chunk).unwrap(),
            serde_json::json!({
                "uploadId": "upload-1",
                "data": "YWJj",
                "seq": 2,
                "targetDeviceId": "device-1",
            })
        );

        let binary = UploadBinaryChunk {
            upload_id: "upload-2".into(),
            seq: Some(3),
            target_device_id: Some("device-1".into()),
        };
        assert_eq!(
            serde_json::to_value(binary).unwrap(),
            serde_json::json!({
                "uploadId": "upload-2",
                "seq": 3,
                "targetDeviceId": "device-1",
            })
        );

        let read = ReadAttachmentChunk {
            path: "/tmp/image.png".into(),
            offset: 45_000,
            target_device_id: None,
        };
        assert_eq!(
            serde_json::to_value(read).unwrap(),
            serde_json::json!({ "path": "/tmp/image.png", "offset": 45_000 })
        );
    }

    #[test]
    fn catalog_requests_omit_absent_routing_context() {
        let request = ListModels {
            harness: HarnessId::Pi,
            target_device_id: None,
        };
        assert_eq!(
            serde_json::to_value(request).unwrap(),
            serde_json::json!({ "harness": "pi" })
        );
    }

    #[test]
    fn recovery_fork_carries_fresh_identity_and_target_host() {
        let request = CreateRecoveryFork {
            source_chat_id: "source-chat".into(),
            chat_id: "recovered-chat".into(),
            space_id: "target-space".into(),
            target_device_id: Some("device-1".into()),
        };
        assert_eq!(
            serde_json::to_value(request).unwrap(),
            serde_json::json!({
                "sourceChatId": "source-chat",
                "chatId": "recovered-chat",
                "spaceId": "target-space",
                "targetDeviceId": "device-1",
            })
        );
    }

    #[test]
    fn harness_update_action_carries_only_a_typed_target() {
        let request = ApplyHarnessUpdate {
            harness: HarnessId::Codex,
            target_device_id: Some("device-1".into()),
        };
        assert_eq!(
            serde_json::to_value(request).unwrap(),
            serde_json::json!({ "harness": "codex", "targetDeviceId": "device-1" })
        );
    }

    #[test]
    fn jolt_update_requests_carry_the_target_device() {
        assert_eq!(
            serde_json::to_value(WatchUpdateStatus {
                target_device_id: Some("device-1".into()),
            })
            .unwrap(),
            serde_json::json!({ "targetDeviceId": "device-1" })
        );
        assert_eq!(
            serde_json::to_value(ApplyUpdate {
                target_device_id: Some("device-1".into()),
            })
            .unwrap(),
            serde_json::json!({ "targetDeviceId": "device-1" })
        );
    }
}
