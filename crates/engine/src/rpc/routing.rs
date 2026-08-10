use async_trait::async_trait;
use jolt_api::methods;
use jolt_rpc::{RpcError, RpcReply, RpcService};

use super::EngineRpc;

pub(super) fn local_only(method: &str) -> bool {
    matches!(
        method,
        methods::GET_REVIEW_DRAFT
            | methods::PUT_REVIEW_DRAFT
            | methods::DELETE_REVIEW_DRAFT
            | methods::LIST_HARNESS_SECRETS
            | methods::UPSERT_HARNESS_SECRET
            | methods::DELETE_HARNESS_SECRET
            | methods::WATCH_THEMES
            | methods::LIST_THEMES
            | methods::UPSERT_THEMES
            | methods::DELETE_THEME
    )
}

struct RelayRpc {
    inner: std::sync::Arc<EngineRpc>,
}

#[async_trait]
impl RpcService for RelayRpc {
    async fn handle(&self, method: &str, params: serde_json::Value) -> Result<RpcReply, RpcError> {
        if local_only(method) {
            return Err(RpcError::UnknownMethod(method.to_owned()));
        }
        self.inner.handle(method, params).await
    }

    async fn handle_binary(
        &self,
        method: &str,
        params: serde_json::Value,
        payload: bytes::Bytes,
    ) -> Result<RpcReply, RpcError> {
        if local_only(method) {
            return Err(RpcError::UnknownMethod(method.to_owned()));
        }
        self.inner.handle_binary(method, params, payload).await
    }
}

pub(crate) fn relay_service(inner: std::sync::Arc<EngineRpc>) -> std::sync::Arc<dyn RpcService> {
    std::sync::Arc::new(RelayRpc { inner })
}

pub(crate) fn theme_sync_method(method: &str) -> bool {
    matches!(
        method,
        methods::WATCH_THEMES
            | methods::LIST_THEMES
            | methods::UPSERT_THEMES
            | methods::DELETE_THEME
    )
}

pub(super) fn forwardable(method: &str) -> bool {
    matches!(
        method,
        methods::LIST_HARNESSES
            | methods::WATCH_HARNESS_UPDATES
            | methods::CHECK_HARNESS_UPDATES
            | methods::APPLY_HARNESS_UPDATE
            | methods::LIST_MODELS
            | methods::LIST_COMMANDS
            | methods::QUEUE_COMMAND
            | methods::WATCH_TRANSCRIPT_V2
            | methods::GET_TRANSCRIPT_PAGE
            | methods::SEARCH_TRANSCRIPT
            | methods::EXTRACT_QUESTIONS
            | methods::WATCH_CHAT_USAGE
            | methods::USAGE_BREAKDOWN
            | methods::REGENERATE_CHAT_TITLE
            // Repos/worktrees/folders are device-local filesystem state.
            | methods::LIST_REPOS
            | methods::ADD_REPO
            | methods::CLONE_REPO
            | methods::CREATE_REPO
            | methods::LIST_BRANCHES
            | methods::LIST_REFS
            | methods::GET_CHECKOUT_REVIEW
            | methods::SWITCH_REF
            | methods::LIST_FOLDERS
            | methods::SEARCH_FILES
            | methods::CREATE_WORKTREE
            | methods::DELETE_WORKTREE
            | methods::VCS_SETTINGS
            | methods::SET_VCS_BACKEND
            | methods::TERMINAL_SETTINGS
            | methods::SET_TERMINAL_COMMAND
            // Checkout diffs are produced on the device holding the checkout.
            | methods::WATCH_CHECKOUT_DIFF_V2
            | methods::GET_CHECKOUT_DIFF_PAGE
            | methods::GET_TURN_DIFF_PAGE
            | methods::PIN_DIFF_DOCUMENT
            | methods::RELEASE_DIFF_DOCUMENT
            // Terminals live on the chat's host device.
            | methods::OPEN_TERMINAL
            | methods::SUBSCRIBE_TERMINAL_V2
            | methods::WRITE_TERMINAL
            | methods::RESIZE_TERMINAL
            | methods::CLOSE_TERMINAL
            // Agent accounts are per-device CLI logins (the device switcher
            // retargets which device's logins are shown).
            | methods::LIST_AGENT_ACCOUNTS
            | methods::ACTIVATE_AGENT_ACCOUNT
            | methods::FORGET_AGENT_ACCOUNT
            | methods::START_AGENT_LOGIN
            | methods::COMPLETE_AGENT_LOGIN
            | methods::POLL_AGENT_LOGIN
            | methods::CANCEL_AGENT_LOGIN
            // Uploads/attachments target the chat's host device (the agent reads
            // the committed file from that device's disk).
            | methods::GET_TRANSPORT_CAPABILITIES
            | methods::UPLOAD_CHUNK
            | methods::UPLOAD_BINARY_CHUNK
            | methods::UPLOAD_COMMIT
            | methods::READ_ATTACHMENT_CHUNK
            // Updates report/apply on the device whose binary they concern.
            | methods::UPDATE_STATUS
            | methods::APPLY_UPDATE
    )
}

/// Forwardable methods whose reply is a stream (proxied item-by-item).
pub(super) fn is_stream_method(method: &str) -> bool {
    matches!(
        method,
        methods::WATCH_TRANSCRIPT_V2
            | methods::WATCH_HARNESS_UPDATES
            | methods::WATCH_CHAT_USAGE
            | methods::WATCH_CHECKOUT_DIFF_V2
            | methods::UPDATE_STATUS
    )
}

pub(super) fn is_binary_stream_method(method: &str) -> bool {
    method == methods::SUBSCRIBE_TERMINAL_V2
}
