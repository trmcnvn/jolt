//! jolt-api — product RPC method contracts and typed client surface.

mod models;
mod requests;

pub use models::{
    Acknowledged, AttachmentChunk, AuthUrl, Cancellation, CommittedAttachment, HarnessDescriptor,
    LocalDevice, QueuedCommand, ScopeKind, ScopeStatus, SwitchedRef,
};
pub use requests::{
    ActivateAgentAccount, ApplyHarnessUpdate, ApplyUpdate, BinaryStreamRequest, BinaryUnaryRequest,
    CancelAgentLogin, CancelQueuedPrompt, ChatPage, ChatSection, ChatWatchFrame,
    CheckHarnessUpdates, CloseTerminal, CompleteAgentLogin, CreateRecoveryFork, CreateWorktree,
    DeleteHarnessSecret, DeleteReviewDraft, DeleteTheme, EnsurePersonalOrg, ExtractQuestions,
    ForgetAgentAccount, GetCheckoutDiffPage, GetCheckoutReview, GetCheckoutVcsStatus,
    GetLocalDevice, GetReviewDraft, GetTranscriptPage, GetTransportCapabilities, GetTurnDiffPage,
    ListAgentAccounts, ListCommands, ListFolders, ListHarnessSecrets, ListHarnesses, ListModels,
    ListRefs, ListThemes, Mutate, OpenTerminal, PinDiffDocument, PollAgentLogin, ProbeSync,
    PutReviewDraft, QueryChats, QueueCommand, ReadAttachmentChunk, RegenerateChatTitle,
    ReleaseDiffDocument, ResizeTerminal, ResolveAccountLink, RunVcsAction, SearchFiles,
    SearchTranscript, SessionWatchFrame, SetTerminalCommand, SetVcsBackend, SignIn, SignOut,
    StartAgentLogin, StopEngine, StreamRequest, SubscribeTerminal, SwitchRef, SwitchScope,
    TerminalSettings, TransportCapabilities, UnaryRequest, UploadBinaryChunk, UploadChunk,
    UploadCommit, UpsertHarnessSecret, UpsertThemes, UsageBreakdownRequest, VcsSettings,
    WatchAuthStatus, WatchChatUsage, WatchChats, WatchCheckoutDiff, WatchDevices,
    WatchHarnessUpdates, WatchQueuedPrompts, WatchScopeStatus, WatchSessions, WatchSpaces,
    WatchThemes, WatchTranscript, WatchUpdateStatus, WriteTerminal, call, call_binary, subscribe,
    subscribe_binary,
};

pub mod methods {
    pub const LIST_HARNESSES: &str = "ListHarnesses";
    /// Device-local harness release status, current frame then changes.
    pub const WATCH_HARNESS_UPDATES: &str = "WatchHarnessUpdates";
    /// Trigger an immediate background refresh of all installed harnesses.
    pub const CHECK_HARNESS_UPDATES: &str = "CheckHarnessUpdates";
    /// Start a user-approved update; progress is reported by the watch stream.
    pub const APPLY_HARNESS_UPDATE: &str = "ApplyHarnessUpdate";
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
    /// Materialize the last published projection of a permanently lost host
    /// under a fresh chat id assigned to this target device.
    pub const CREATE_RECOVERY_FORK: &str = "CreateRecoveryFork";
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
    /// Active-chat bootstrap followed by changed-row deltas. Archived history
    /// loads through `QueryChats` instead of riding every live frame.
    pub const WATCH_CHATS: &str = "WatchChats";
    /// Cursor-paged chat rows for archived history and server-side title search.
    pub const QUERY_CHATS: &str = "QueryChats";
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
    /// Gracefully stop a separately owned local engine (IPC-only).
    pub const STOP_ENGINE: &str = "StopEngine";
    // Repos / worktrees / folders (ControlRpc, relay-forwardable).
    pub const LIST_REPOS: &str = "ListRepos";
    pub const ADD_REPO: &str = "AddRepo";
    pub const CLONE_REPO: &str = "CloneRepo";
    pub const CREATE_REPO: &str = "CreateRepo";
    pub const LIST_BRANCHES: &str = "ListBranches";
    pub const LIST_REFS: &str = "ListRefs";
    /// Open provider-neutral PR/MR associated with a chat's concrete checkout.
    pub const GET_CHECKOUT_REVIEW: &str = "GetCheckoutReview";
    /// Current working-copy and publication status for a chat's concrete checkout.
    pub const GET_CHECKOUT_VCS_STATUS: &str = "GetCheckoutVcsStatus";
    /// Host-owned Commit/Push action progress stream for a concrete checkout.
    pub const RUN_VCS_ACTION: &str = "RunVcsAction";
    pub const SWITCH_REF: &str = "SwitchRef";
    pub const LIST_FOLDERS: &str = "ListFolders";
    /// Fuzzy relative-path search rooted in a known chat or space checkout.
    pub const SEARCH_FILES: &str = "SearchFiles";
    pub const CREATE_WORKTREE: &str = "CreateWorktree";
    pub const DELETE_WORKTREE: &str = "DeleteWorktree";
    /// Per-device active VCS backend and executable availability.
    pub const VCS_SETTINGS: &str = "VcsSettings";
    pub const SET_VCS_BACKEND: &str = "SetVcsBackend";
    /// Per-device command used when a new terminal opens.
    pub const TERMINAL_SETTINGS: &str = "TerminalSettings";
    pub const SET_TERMINAL_COMMAND: &str = "SetTerminalCommand";
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
    pub const GET_TRANSPORT_CAPABILITIES: &str = "GetTransportCapabilities";
    pub const UPLOAD_CHUNK: &str = "UploadChunk";
    pub const UPLOAD_BINARY_CHUNK: &str = "UploadBinaryChunk";
    pub const UPLOAD_COMMIT: &str = "UploadCommit";
    pub const READ_ATTACHMENT_CHUNK: &str = "ReadAttachmentChunk";
    // Updates (ControlRpc, relay-forwardable — a device reports/applies its own
    // binary's update). Stream: current UpdateStatus, then every change.
    pub const UPDATE_STATUS: &str = "UpdateStatus";
    /// Download + apply the newest release on the target device (symlink-managed
    /// installs; the service restart is scheduled after the reply flushes).
    pub const APPLY_UPDATE: &str = "ApplyUpdate";
}
