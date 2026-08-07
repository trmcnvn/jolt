//! jolt-rpc — the typed control plane (UiRpc / ControlRpc) over WebSocket + in-memory
//! transports, plus the device-room relay transport ({s,k,to,from} frames — [`device_room`]).
//!
//! Framing: control uses ndjson envelopes, one JSON object per WebSocket text
//! message or per line on byte transports. Binary stream items use versioned
//! WebSocket binary messages keyed by the same request id:
//!
//! - client → server: `{id, method, params}` to invoke, `{id, cancel: true}` to stop a stream;
//! - server → client: `{id, ok}` / `{id, err}` for unary calls,
//!   `{id, item}`* then `{id, done: true}` (or `{id, err}`) for streams.
//!
//! The server dispatches into an [`RpcService`]; the [`RpcClient`] offers `call`,
//! `subscribe`, and `subscribe_binary`. Both ends run over [`WireFrame`] channels,
//! so the in-memory transport ([`memory_client`]) exercises the exact same code
//! path as WebSocket text and binary messages.

use std::sync::Arc;

use async_trait::async_trait;
use futures::stream::BoxStream;
use serde::{Deserialize, Serialize};

mod client;
pub mod device_room;
mod server;
pub mod terminal_wire;

pub use client::{RpcClient, connect_ws};
pub use device_room::{
    DeviceFrameHeader, DeviceLink, HostRelay, HostRelayConfig, LinkCache, LinkCacheConfig,
    NudgeHandler, StaticToken, TokenSource, decode_device_frame, device_room_ws_url,
    encode_device_frame,
};
pub use server::{serve_connection, serve_ws_listener};

/// RPC method names — single source of truth for both ends.
/// Full surface: docs/rpc.md.
pub mod methods {
    pub const LIST_HARNESSES: &str = "ListHarnesses";
    pub const LIST_MODELS: &str = "ListModels";
    pub const LIST_COMMANDS: &str = "ListCommands";
    pub const QUEUE_COMMAND: &str = "QueueCommand";
    pub const CANCEL_QUEUED_PROMPT: &str = "CancelQueuedPrompt";
    pub const WATCH_QUEUED_PROMPTS: &str = "WatchQueuedPrompts";
    /// Tail-first transcript stream: compact manifest + trailing pages, then
    /// sequenced live-page deltas.
    pub const WATCH_TRANSCRIPT_V2: &str = "WatchTranscriptV2";
    /// Fetch one historical transcript page by its opaque catalog id.
    pub const GET_TRANSCRIPT_PAGE: &str = "GetTranscriptPage";
    /// Search all messages in one transcript and return page-backed anchors.
    pub const SEARCH_TRANSCRIPT: &str = "SearchTranscript";
    /// Extract prose questions from one completed assistant message.
    pub const EXTRACT_QUESTIONS: &str = "ExtractQuestions";
    /// Nudge every open room client to verify liveness NOW (window focus,
    /// app foregrounded). No params; IPC-only. Each room ignores the hint
    /// unless it has been broadcast-quiet ≥30s, so this is cheap to spam.
    pub const PROBE_SYNC: &str = "ProbeSync";
    /// Live sync introspection (`jolt sync` / debug surfaces): per-room
    /// connection state, last pushed-frame/ack ages, rejoin/probe/resync
    /// counters for the registry connection and every open chat doc. No params;
    /// IPC-only.
    pub const SYNC_STATUS: &str = "SyncStatus";
    pub const WATCH_CHATS: &str = "WatchChats";
    pub const WATCH_DEVICES: &str = "WatchDevices";
    pub const WATCH_SESSIONS: &str = "WatchSessions";
    /// Selected-chat usage on its host device (relay-forwardable stream).
    pub const WATCH_CHAT_USAGE: &str = "WatchChatUsage";
    /// Ranged device-local usage analytics (relay-forwardable unary call).
    pub const USAGE_BREAKDOWN: &str = "UsageBreakdown";
    /// Spaces registry (device+folder pairs) from the workspace doc.
    pub const WATCH_SPACES: &str = "WatchSpaces";
    /// Installation-level custom theme files, synchronized through the signed-in
    /// account registry but retained on every host after sign-out.
    pub const WATCH_THEMES: &str = "WatchThemes";
    pub const LIST_THEMES: &str = "ListThemes";
    pub const UPSERT_THEMES: &str = "UpsertThemes";
    pub const DELETE_THEME: &str = "DeleteTheme";
    /// Entity mutations against the workspace document.
    /// Params are tagged `{op: createChat|createSpace|renameSpace|deleteSpace|
    /// renameChat|setChatArchived|deleteChat|renameDevice|markChatSeen, …}`.
    pub const MUTATE: &str = "Mutate";
    /// Regenerate a session name from its first prompt with the host harness's
    /// economy model. Relay-forwardable to the session's host device.
    pub const REGENERATE_CHAT_TITLE: &str = "RegenerateChatTitle";
    /// This engine's identity → `{deviceId}` (IPC-only; never relay-forwarded —
    /// the answer is about whichever engine you are directly connected to).
    pub const LOCAL_DEVICE: &str = "LocalDevice";
    pub const AUTH_STATUS: &str = "AuthStatus";
    // Auth RPC mutations (IPC-only).
    pub const SIGN_IN: &str = "SignIn";
    pub const SIGN_IN_HEADLESS: &str = "SignInHeadless";
    pub const COMPLETE_SIGN_IN: &str = "CompleteSignIn";
    pub const SIGN_OUT: &str = "SignOut";
    pub const LIST_ORGS: &str = "ListOrgs";
    /// Provision or select the signed-in user's sole hidden organization.
    pub const ENSURE_PERSONAL_ORG: &str = "EnsurePersonalOrg";
    // Device-local Local/Account scope lifecycle (IPC-only).
    pub const SCOPE_STATUS: &str = "ScopeStatus";
    pub const SWITCH_SCOPE: &str = "SwitchScope";
    pub const RESOLVE_ACCOUNT_LINK: &str = "ResolveAccountLink";
    // Repos / worktrees / folders (ControlRpc, relay-forwardable).
    pub const LIST_REPOS: &str = "ListRepos";
    pub const ADD_REPO: &str = "AddRepo";
    pub const CLONE_REPO: &str = "CloneRepo";
    pub const CREATE_REPO: &str = "CreateRepo";
    pub const LIST_BRANCHES: &str = "ListBranches";
    pub const LIST_REFS: &str = "ListRefs";
    /// Open provider-neutral PR/MR associated with a chat's concrete checkout.
    pub const GET_CHECKOUT_REVIEW: &str = "GetCheckoutReview";
    pub const SWITCH_REF: &str = "SwitchRef";
    pub const LIST_FOLDERS: &str = "ListFolders";
    /// Fuzzy relative-path search rooted in a known chat or space checkout.
    pub const SEARCH_FILES: &str = "SearchFiles";
    pub const CREATE_WORKTREE: &str = "CreateWorktree";
    pub const DELETE_WORKTREE: &str = "DeleteWorktree";
    /// Per-device active VCS backend and executable availability.
    pub const VCS_SETTINGS: &str = "VcsSettings";
    pub const SET_VCS_BACKEND: &str = "SetVcsBackend";
    // Terminals (ControlRpc, relay-forwardable; V2 carries binary output).
    pub const OPEN_TERMINAL: &str = "OpenTerminal";
    /// Binary terminal output stream; control remains JSON RPC.
    pub const SUBSCRIBE_TERMINAL_V2: &str = "SubscribeTerminalV2";
    pub const WRITE_TERMINAL: &str = "WriteTerminal";
    pub const RESIZE_TERMINAL: &str = "ResizeTerminal";
    pub const CLOSE_TERMINAL: &str = "CloseTerminal";
    /// Checkout-specific paged diff projection, produced where the checkout lives.
    pub const WATCH_CHECKOUT_DIFF_V2: &str = "WatchCheckoutDiffV2";
    /// Fetch one immutable page from the current checkout diff catalog.
    pub const GET_CHECKOUT_DIFF_PAGE: &str = "GetCheckoutDiffPage";
    /// Fetch one immutable page captured for an assistant transcript entry.
    pub const GET_TURN_DIFF_PAGE: &str = "GetTurnDiffPage";
    /// Retain/release one working-copy revision while a local review draft exists.
    pub const PIN_DIFF_DOCUMENT: &str = "PinDiffDocument";
    pub const RELEASE_DIFF_DOCUMENT: &str = "ReleaseDiffDocument";
    // Pending review drafts are private to the directly connected viewing device.
    pub const GET_REVIEW_DRAFT: &str = "GetReviewDraft";
    pub const PUT_REVIEW_DRAFT: &str = "PutReviewDraft";
    pub const DELETE_REVIEW_DRAFT: &str = "DeleteReviewDraft";
    // Agent accounts (ControlRpc, relay-forwardable — CLI logins are per-device).
    pub const LIST_AGENT_ACCOUNTS: &str = "ListAgentAccounts";
    pub const ACTIVATE_AGENT_ACCOUNT: &str = "ActivateAgentAccount";
    pub const FORGET_AGENT_ACCOUNT: &str = "ForgetAgentAccount";
    pub const START_AGENT_LOGIN: &str = "StartAgentLogin";
    pub const COMPLETE_AGENT_LOGIN: &str = "CompleteAgentLogin";
    pub const POLL_AGENT_LOGIN: &str = "PollAgentLogin";
    pub const CANCEL_AGENT_LOGIN: &str = "CancelAgentLogin";
    // Device-local harness secrets. Values are accepted only by the local IPC
    // engine and are never relay-forwarded or returned by any method.
    pub const LIST_HARNESS_SECRETS: &str = "ListHarnessSecrets";
    pub const UPSERT_HARNESS_SECRET: &str = "UpsertHarnessSecret";
    pub const DELETE_HARNESS_SECRET: &str = "DeleteHarnessSecret";
    // Uploads / attachments (ControlRpc, relay-forwardable — target the chat's host device).
    pub const UPLOAD_CHUNK: &str = "UploadChunk";
    pub const UPLOAD_COMMIT: &str = "UploadCommit";
    pub const READ_ATTACHMENT_CHUNK: &str = "ReadAttachmentChunk";
    // Updates (ControlRpc, relay-forwardable — a device reports/applies its own
    // binary's update). Stream: current UpdateStatus, then every change.
    pub const UPDATE_STATUS: &str = "UpdateStatus";
    /// Download + apply the newest release on the target device (symlink-managed
    /// installs; the service restart is scheduled after the reply flushes).
    pub const APPLY_UPDATE: &str = "ApplyUpdate";
}

#[derive(Debug, thiserror::Error)]
pub enum RpcError {
    #[error("unknown method: {0}")]
    UnknownMethod(String),
    #[error("bad params: {0}")]
    BadParams(String),
    #[error("{0}")]
    Failed(String),
    #[error("transport: {0}")]
    Transport(String),
    #[error("connection closed")]
    Closed,
}

/// One transport message. JSON control frames remain text while high-volume
/// stream items use binary WebSocket messages end-to-end.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WireFrame {
    Text(String),
    Binary(Vec<u8>),
}

const BINARY_MAGIC: &[u8; 4] = b"JRPB";
const BINARY_VERSION: u8 = 1;
const BINARY_STREAM_ITEM: u8 = 1;
const BINARY_HEADER_LEN: usize = 14;

pub(crate) fn encode_binary_stream_item(id: u64, payload: &[u8]) -> Vec<u8> {
    let mut frame = Vec::with_capacity(BINARY_HEADER_LEN + payload.len());
    frame.extend_from_slice(BINARY_MAGIC);
    frame.extend_from_slice(&[BINARY_VERSION, BINARY_STREAM_ITEM]);
    frame.extend_from_slice(&id.to_le_bytes());
    frame.extend_from_slice(payload);
    frame
}

pub(crate) fn decode_binary_stream_item(bytes: &[u8]) -> Result<(u64, &[u8]), RpcError> {
    if bytes.get(..4) != Some(BINARY_MAGIC) {
        return Err(RpcError::Transport("binary RPC frame: bad magic".into()));
    }
    if bytes.get(4) != Some(&BINARY_VERSION) {
        return Err(RpcError::Transport(
            "binary RPC frame: unsupported version".into(),
        ));
    }
    if bytes.get(5) != Some(&BINARY_STREAM_ITEM) {
        return Err(RpcError::Transport(
            "binary RPC frame: unknown opcode".into(),
        ));
    }
    let id = u64::from_le_bytes(
        binary_payload(bytes, 6, 8, "RPC stream id")?
            .try_into()
            .map_err(|_| RpcError::Transport("binary RPC frame: invalid stream id".into()))?,
    );
    Ok((id, &bytes[BINARY_HEADER_LEN..]))
}

pub(crate) fn binary_payload<'a>(
    bytes: &'a [u8],
    offset: usize,
    len: usize,
    field: &str,
) -> Result<&'a [u8], RpcError> {
    let end = offset
        .checked_add(len)
        .ok_or_else(|| RpcError::Transport(format!("binary frame: {field} length overflow")))?;
    bytes
        .get(offset..end)
        .ok_or_else(|| RpcError::Transport(format!("binary frame: truncated {field}")))
}

/// A client-originated frame.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientFrame {
    pub id: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,
    #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
    pub params: serde_json::Value,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub cancel: bool,
}

/// A server-originated frame. Exactly one of `ok` / `err` / `item` / `done` is meaningful.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ServerFrame {
    pub id: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ok: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub err: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub item: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub done: bool,
}

/// What a service returns for one invocation.
pub enum RpcReply {
    /// Unary response — sent as `{id, ok}`.
    Value(serde_json::Value),
    /// Stream — each item sent as `{id, item}`, then `{id, done: true}` when it ends.
    Stream(BoxStream<'static, serde_json::Value>),
    /// Binary stream — each item is one binary frame, followed by JSON `{id, done}`.
    BinaryStream(BoxStream<'static, Vec<u8>>),
}

impl RpcReply {
    /// Serialize a value into a unary reply.
    pub fn value<T: Serialize>(value: &T) -> Result<Self, RpcError> {
        serde_json::to_value(value)
            .map(RpcReply::Value)
            .map_err(|e| RpcError::Failed(format!("serialize response: {e}")))
    }
}

/// Server-side dispatch: one implementation serves every transport.
#[async_trait]
pub trait RpcService: Send + Sync + 'static {
    async fn handle(&self, method: &str, params: serde_json::Value) -> Result<RpcReply, RpcError>;
}

/// Deserialize typed params out of the envelope's `params` value.
pub fn parse_params<T: serde::de::DeserializeOwned>(
    params: serde_json::Value,
) -> Result<T, RpcError> {
    serde_json::from_value(params).map_err(|e| RpcError::BadParams(e.to_string()))
}

/// Spawn an in-memory server for `service` and return a connected client.
/// Same envelopes, same dispatch loop as the WebSocket path — the in-process UI
/// transport deliberately keeps the serialization boundary (docs/rpc.md).
pub fn memory_client(service: Arc<dyn RpcService>) -> RpcClient {
    let (client_out, server_in) = tokio::sync::mpsc::channel::<WireFrame>(256);
    let (server_out, client_in) = tokio::sync::mpsc::channel::<WireFrame>(256);
    tokio::spawn(serve_connection(service, server_out, server_in));
    RpcClient::new(client_out, client_in)
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;

    struct TestService;

    #[async_trait]
    impl RpcService for TestService {
        async fn handle(
            &self,
            method: &str,
            params: serde_json::Value,
        ) -> Result<RpcReply, RpcError> {
            match method {
                "Echo" => Ok(RpcReply::Value(params)),
                "Count" => {
                    let n = params.get("n").and_then(|v| v.as_u64()).unwrap_or(0);
                    Ok(RpcReply::Stream(
                        futures::stream::iter((0..n).map(|i| serde_json::json!(i))).boxed(),
                    ))
                }
                "Never" => Ok(RpcReply::Stream(futures::stream::pending().boxed())),
                "Bytes" => Ok(RpcReply::BinaryStream(
                    futures::stream::iter([vec![0, 1, 0x80, 0xff], b"second".to_vec()]).boxed(),
                )),
                "Boom" => Err(RpcError::Failed("boom".into())),
                other => Err(RpcError::UnknownMethod(other.into())),
            }
        }
    }

    #[tokio::test]
    async fn memory_call_stream_and_error() {
        let client = memory_client(Arc::new(TestService));

        let echoed = client
            .call("Echo", serde_json::json!({"x": 1}))
            .await
            .unwrap();
        assert_eq!(echoed, serde_json::json!({"x": 1}));

        let mut items = client
            .subscribe("Count", serde_json::json!({"n": 3}))
            .await
            .unwrap();
        let mut seen = Vec::new();
        while let Some(v) = items.recv().await {
            seen.push(v);
        }
        assert_eq!(
            seen,
            vec![
                serde_json::json!(0),
                serde_json::json!(1),
                serde_json::json!(2)
            ]
        );

        let mut binary = client
            .subscribe_binary("Bytes", serde_json::Value::Null)
            .await
            .unwrap();
        assert_eq!(binary.recv().await, Some(vec![0, 1, 0x80, 0xff]));
        assert_eq!(binary.recv().await, Some(b"second".to_vec()));
        assert_eq!(binary.recv().await, None);

        let err = client
            .call("Boom", serde_json::Value::Null)
            .await
            .unwrap_err();
        assert!(matches!(err, RpcError::Failed(m) if m == "boom"));
    }

    #[tokio::test]
    async fn websocket_round_trip() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(serve_ws_listener(listener, Arc::new(TestService)));

        let client = connect_ws(&format!("ws://127.0.0.1:{port}")).await.unwrap();
        let echoed = client
            .call("Echo", serde_json::json!("hello"))
            .await
            .unwrap();
        assert_eq!(echoed, serde_json::json!("hello"));

        let mut items = client
            .subscribe("Count", serde_json::json!({"n": 2}))
            .await
            .unwrap();
        assert_eq!(items.recv().await, Some(serde_json::json!(0)));
        assert_eq!(items.recv().await, Some(serde_json::json!(1)));
        assert_eq!(items.recv().await, None);

        let mut binary = client
            .subscribe_binary("Bytes", serde_json::Value::Null)
            .await
            .unwrap();
        assert_eq!(binary.recv().await, Some(vec![0, 1, 0x80, 0xff]));
        assert_eq!(binary.recv().await, Some(b"second".to_vec()));
        assert_eq!(binary.recv().await, None);
    }

    #[tokio::test]
    async fn dropping_stream_receiver_cancels_server_side() {
        let client = memory_client(Arc::new(TestService));
        let items = client
            .subscribe("Never", serde_json::Value::Null)
            .await
            .unwrap();
        drop(items);
        // The next unary call still works — the dead stream didn't wedge the connection.
        let echoed = client.call("Echo", serde_json::json!(2)).await.unwrap();
        assert_eq!(echoed, serde_json::json!(2));
    }
}
