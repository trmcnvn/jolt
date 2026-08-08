//! App state: the engine connection, entity lists, and the selected chat's
//! transcript — one gpui [`Entity`] the whole shell renders from.
//!
//! ## EngineHandle
//! The application composition root supplies an [`EngineConnector`] that either
//! dials the localhost daemon or embeds an engine. The UI owns only the resulting
//! [`EngineHandle`] and product RPC client.
//!
//! ## Async bridging
//! Connector startup runs through `gpui_tokio::Tokio::spawn`. RPC futures are
//! runtime-agnostic, so subscription pumps run on gpui's executor and fold each
//! frame into the entity with `this.update(...)` + `cx.notify()`.
//!
//! Pure logic (sort order, staleness, gate phase) lives in free functions with
//! unit tests; rendering reads them.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
#[cfg(test)]
use std::sync::Arc;

use chrono::{DateTime, Utc};
use gpui::{App, Context, Entity, Task};
use gpui_tokio::Tokio;
use jolt_api::{
    ApplyUpdate, ChatWatchFrame, DeleteTheme, GetLocalDevice, GetTranscriptPage, ListThemes,
    Mutate, ProbeSync, ScopeKind, ScopeStatus, SessionWatchFrame, StreamRequest, UpsertThemes,
    WatchAuthStatus, WatchChatUsage, WatchChats, WatchDevices, WatchHarnessUpdates,
    WatchQueuedPrompts, WatchScopeStatus, WatchSessions, WatchSpaces, WatchTranscript,
    WatchUpdateStatus, call as call_api, subscribe as subscribe_api,
};
#[cfg(test)]
use jolt_api::{ListHarnesses, SwitchScope};
#[cfg(test)]
use jolt_proto::HarnessId;
use jolt_proto::{
    AuthState, Chat, ChatIndicator, Device, HarnessUpdateStatus, Session, Space, ThemeFileRecord,
    UsageSummary,
};
use jolt_session_doc::{
    QueuedPrompt, SessionMessageEntry, TranscriptDesync, TranscriptManifest, TranscriptPage,
    TranscriptWatchFrame,
};

mod engine;

pub use engine::{EngineBackend, EngineBootConfig, EngineConnector, EngineHandle, EngineMode};

// ---------------------------------------------------------------------------
// Pure state + reducers
// ---------------------------------------------------------------------------

// The frontend-agnostic derivations (sort orders, staleness gating, sidebar
// grouping, the boot gate, relative times) live in `jolt_proto::view`, pure
// and with their own test suite. Re-exported here because every call site in
// this crate reads them as `state::…`.
pub use jolt_proto::view::{
    ChatGroup, ConnectionStatus, GatePhase, Indicator, SESSION_STALE_MS, attention_rank,
    chat_location, display_status, effective_indicator, format_time_ago, gate_phase, group_chats,
    parse_auth_state, project_label, sort_active, sort_chats, sort_spaces,
};

// ---------------------------------------------------------------------------
// AppState entity
// ---------------------------------------------------------------------------

/// How long a queued send may override the synced session status. An offline
/// host must not leave the chat looking permanently active.
pub const PENDING_SEND_TTL_MS: i64 = 30_000;

#[derive(Debug, Clone)]
struct PendingSend {
    message_id: String,
    started_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub enum RemoteJoltUpdateAction {
    Applying {
        target_version: String,
    },
    Verifying {
        target_version: String,
    },
    Failed {
        target_version: String,
        message: String,
    },
}

impl RemoteJoltUpdateAction {
    pub fn target_version(&self) -> &str {
        match self {
            Self::Applying { target_version }
            | Self::Verifying { target_version }
            | Self::Failed { target_version, .. } => target_version,
        }
    }
}

/// Root application state. Reducer methods (`apply_*`, [`Self::session_for`], …)
/// are plain `&mut self` functions so tests construct the struct directly; gpui
/// glue ([`Self::bootstrap`], [`Self::select_chat`]) layers subscriptions on top.
pub struct AppState {
    pub connection: ConnectionStatus,
    /// Auth stream value; `None` until the engine reports one (M4).
    pub auth: Option<AuthState>,
    /// Active Local/Account data scope.
    pub scope: Option<ScopeStatus>,
    pub devices: Vec<Device>,
    /// Sorted (see [`sort_spaces`]).
    pub spaces: Vec<Space>,
    /// Sorted (see [`sort_chats`]); includes archived rows — views filter.
    pub chats: Vec<Chat>,
    pub sessions: Vec<Session>,
    /// Live cumulative usage for the selected chat, streamed from its host.
    pub selected_usage: Option<UsageSummary>,
    /// The active space for session context and new-session defaults. Healed by
    /// [`Self::apply_spaces`] when the row vanishes; selecting a chat implies
    /// its space.
    pub selected_space: Option<String>,
    pub selected_chat: Option<String>,
    /// The initial spaces frame has landed. Device-local filter state must not
    /// reconcile against the empty pre-sync collection.
    pub spaces_synced: bool,
    /// Loaded transcript window, flattened for composer derivations. Historical
    /// unloaded pages live as compact descriptors in `transcript_manifest`.
    pub transcript: Vec<SessionMessageEntry>,
    pub transcript_manifest: Option<TranscriptManifest>,
    pub transcript_pages: Vec<TranscriptPage>,
    pub transcript_loading_pages: HashSet<String>,
    pub transcript_page_errors: HashSet<String>,
    transcript_sequence: u64,
    /// Optimistic user echoes per chat id, shown until the doc frame carrying
    /// the same message id arrives (client-minted ids make dedup exact).
    echoes: HashMap<String, Vec<SessionMessageEntry>>,
    /// Latest queued send per chat, overlaid as Working until the host writes
    /// the matching message into the transcript.
    pending_sends: HashMap<String, PendingSend>,
    /// Engine-owned turns waiting behind the selected chat's current run.
    pub queued_prompts: Vec<QueuedPrompt>,
    /// This engine's device id (best-effort `LocalDevice` probe; `None` until
    /// the engine serves it — views degrade gracefully).
    pub local_device_id: Option<String>,
    /// Latest `UpdateStatus` frame — drives the sidebar update strip.
    pub update: Option<jolt_update::UpdateStatus>,
    /// Jolt release status reported by remote engine-host devices.
    pub remote_updates: HashMap<String, jolt_update::UpdateStatus>,
    /// Explicit remote Jolt update actions and their reconnect/error state.
    pub remote_update_actions: HashMap<String, RemoteJoltUpdateAction>,
    /// Coding-harness release and apply states for this device.
    pub harness_updates: Vec<HarnessUpdateStatus>,
    /// Coding-harness states streamed from reachable engine-host devices.
    pub remote_harness_updates: HashMap<String, Vec<HarnessUpdateStatus>>,
    pub remote_harness_update_device_names: HashMap<String, String>,
    /// Data directory (`ui-settings.json`, `composer-defaults.json`); set at
    /// bootstrap so child views can persist small preference files.
    pub data_dir: Option<PathBuf>,
    engine: Option<EngineHandle>,
    /// Watches bound to the currently selected Local/Account runtime.
    watch_tasks: Vec<Task<()>>,
    /// Auth, update, and scope watches survive runtime switches.
    global_tasks: Vec<Task<()>>,
    /// Device-targeted harness watches; dropping one cancels its retry loop.
    remote_harness_update_tasks: HashMap<String, Task<()>>,
    remote_update_tasks: HashMap<String, Task<()>>,
    remote_update_action_tasks: HashMap<String, Task<()>>,
    transcript_task: Option<Task<()>>,
    queue_task: Option<Task<()>>,
    usage_task: Option<Task<()>>,
    theme_sync_task: Option<Task<()>>,
}

impl Default for AppState {
    fn default() -> Self {
        Self::new()
    }
}

impl AppState {
    pub fn new() -> Self {
        Self {
            connection: ConnectionStatus::Connecting,
            auth: None,
            scope: None,
            devices: Vec::new(),
            spaces: Vec::new(),
            chats: Vec::new(),
            sessions: Vec::new(),
            selected_usage: None,
            selected_space: None,
            selected_chat: None,
            transcript: Vec::new(),
            transcript_manifest: None,
            transcript_pages: Vec::new(),
            transcript_loading_pages: HashSet::new(),
            transcript_page_errors: HashSet::new(),
            transcript_sequence: 0,
            echoes: HashMap::new(),
            pending_sends: HashMap::new(),
            queued_prompts: Vec::new(),
            local_device_id: None,
            update: None,
            remote_updates: HashMap::new(),
            remote_update_actions: HashMap::new(),
            harness_updates: Vec::new(),
            remote_harness_updates: HashMap::new(),
            remote_harness_update_device_names: HashMap::new(),
            data_dir: None,
            engine: None,
            watch_tasks: Vec::new(),
            global_tasks: Vec::new(),
            remote_harness_update_tasks: HashMap::new(),
            remote_update_tasks: HashMap::new(),
            remote_update_action_tasks: HashMap::new(),
            transcript_task: None,
            queue_task: None,
            usage_task: None,
            theme_sync_task: None,
            spaces_synced: false,
        }
    }

    // ---- reducers (pure) ----

    pub fn apply_chats(&mut self, mut chats: Vec<Chat>) {
        sort_chats(&mut chats);
        self.chats = chats;
        if let Some(selected) = &self.selected_chat
            && !self.chats.iter().any(|c| &c.id == selected)
        {
            // Selected chat vanished (deleted elsewhere): drop selection + transcript.
            self.selected_chat = None;
            self.clear_transcript_projection();
            self.transcript_task = None;
            self.queued_prompts.clear();
            self.queue_task = None;
            self.selected_usage = None;
            self.usage_task = None;
        }
    }

    pub fn apply_chat_watch_frame(&mut self, frame: ChatWatchFrame) {
        match frame {
            ChatWatchFrame::Bootstrap { chats } => {
                let mut merged: Vec<_> = self
                    .chats
                    .iter()
                    .filter(|chat| chat.archived)
                    .cloned()
                    .collect();
                merged.extend(chats);
                self.apply_chats(merged);
            }
            ChatWatchFrame::Delta {
                upserts,
                removed_ids,
            } => {
                let removed: HashSet<_> = removed_ids.into_iter().collect();
                self.chats.retain(|chat| !removed.contains(&chat.id));
                for chat in upserts {
                    if let Some(existing) = self.chats.iter_mut().find(|row| row.id == chat.id) {
                        *existing = chat;
                    } else {
                        self.chats.push(chat);
                    }
                }
                let chats = std::mem::take(&mut self.chats);
                self.apply_chats(chats);
            }
        }
    }

    pub fn merge_chat_page(&mut self, chats: Vec<Chat>) {
        for chat in chats {
            if let Some(existing) = self.chats.iter_mut().find(|row| row.id == chat.id) {
                *existing = chat;
            } else {
                self.chats.push(chat);
            }
        }
        sort_chats(&mut self.chats);
    }

    pub fn apply_sessions(&mut self, sessions: Vec<Session>) {
        self.sessions = sessions;
    }

    pub fn apply_session_watch_frame(&mut self, frame: SessionWatchFrame) {
        match frame {
            SessionWatchFrame::Bootstrap { sessions } => self.apply_sessions(sessions),
            SessionWatchFrame::Delta {
                upserts,
                removed_chat_ids,
            } => {
                let removed: HashSet<_> = removed_chat_ids.into_iter().collect();
                self.sessions
                    .retain(|session| !removed.contains(&session.chat_id));
                for session in upserts {
                    if let Some(existing) = self
                        .sessions
                        .iter_mut()
                        .find(|row| row.chat_id == session.chat_id)
                    {
                        *existing = session;
                    } else {
                        self.sessions.push(session);
                    }
                }
            }
        }
    }

    pub fn apply_spaces(&mut self, mut spaces: Vec<Space>) {
        sort_spaces(&mut spaces);
        self.spaces = spaces;
        self.spaces_synced = true;
        // Heal a vanished selection (space deleted elsewhere): fall back to the
        // first space; its chats died with it, so a matching chat selection is
        // healed by the accompanying chats frame (`apply_chats`).
        if let Some(selected) = &self.selected_space
            && !self.spaces.iter().any(|s| &s.id == selected)
        {
            self.selected_space = self.spaces.first().map(|s| s.id.clone());
        }
        // First frame with no selection yet: pick the first space so the shell
        // never renders an empty main area while spaces exist.
        if self.selected_space.is_none() {
            self.selected_space = self.spaces.first().map(|s| s.id.clone());
        }
    }

    /// Optimistic local echo of a `setChatConfig` mutate: stamp the row now so
    /// the chips update on click; the next chats watch frame carries the same
    /// value once the engine applies the LWW write.
    pub fn apply_chat_config(&mut self, chat_id: &str, config: jolt_proto::ChatConfig) {
        if let Some(chat) = self.chats.iter_mut().find(|c| c.id == chat_id) {
            chat.config = Some(config);
        }
    }

    pub fn apply_devices(&mut self, devices: Vec<Device>) {
        self.devices = devices;
    }

    fn reconcile_remote_harness_update_watches(&mut self, cx: &mut Context<Self>) {
        if self.scope.as_ref().map(|scope| scope.active) != Some(ScopeKind::Account) {
            return;
        }
        let (Some(handle), Some(local_device_id)) =
            (self.engine.clone(), self.local_device_id.as_deref())
        else {
            return;
        };
        let desired: HashMap<String, String> = self
            .devices
            .iter()
            .filter(|device| device.is_engine_host() && device.id != local_device_id)
            .map(|device| (device.id.clone(), device.name.clone()))
            .collect();
        self.remote_harness_update_tasks
            .retain(|device_id, _| desired.contains_key(device_id));
        self.remote_update_tasks
            .retain(|device_id, _| desired.contains_key(device_id));
        self.remote_update_action_tasks
            .retain(|device_id, _| desired.contains_key(device_id));
        self.remote_harness_updates
            .retain(|device_id, _| desired.contains_key(device_id));
        self.remote_updates
            .retain(|device_id, _| desired.contains_key(device_id));
        self.remote_update_actions
            .retain(|device_id, _| desired.contains_key(device_id));
        self.remote_harness_update_device_names = desired.clone();
        for device_id in desired.into_keys() {
            self.remote_harness_update_tasks
                .entry(device_id.clone())
                .or_insert_with(|| {
                    spawn_remote_harness_update_watch(cx, handle.clone(), device_id.clone())
                });
            self.remote_update_tasks
                .entry(device_id.clone())
                .or_insert_with(|| {
                    spawn_remote_update_watch(cx, handle.clone(), device_id.clone())
                });
        }
    }

    pub fn apply_update(&mut self, status: jolt_update::UpdateStatus) {
        self.update = Some(status);
    }

    fn apply_remote_update(&mut self, device_id: String, status: jolt_update::UpdateStatus) {
        if let Some(action) = self.remote_update_actions.get(&device_id)
            && !jolt_update::version_newer(action.target_version(), &status.current_version)
        {
            self.remote_update_actions.remove(&device_id);
            self.remote_update_action_tasks.remove(&device_id);
        }
        self.remote_updates.insert(device_id, status);
    }

    pub fn begin_remote_jolt_update(
        &mut self,
        device_id: String,
        target_version: String,
        cx: &mut Context<Self>,
    ) {
        if self
            .remote_update_actions
            .get(&device_id)
            .is_some_and(|action| !matches!(action, RemoteJoltUpdateAction::Failed { .. }))
        {
            return;
        }
        let Some(handle) = self.engine.clone() else {
            return;
        };
        self.remote_update_actions.insert(
            device_id.clone(),
            RemoteJoltUpdateAction::Applying {
                target_version: target_version.clone(),
            },
        );
        let request = ApplyUpdate {
            target_device_id: Some(device_id.clone()),
        };
        let task_device_id = device_id.clone();
        let task = cx.spawn(async move |this, cx| {
            let result = call_api(handle.client(), &request).await;
            this.update(cx, |state, cx| {
                let already_updated = state.remote_updates.get(&device_id).is_some_and(|status| {
                    !jolt_update::version_newer(&target_version, &status.current_version)
                });
                if already_updated {
                    state.remote_update_actions.remove(&device_id);
                } else {
                    let action = match result {
                        Ok(_)
                        | Err(jolt_rpc::RpcError::Closed | jolt_rpc::RpcError::Transport(_)) => {
                            RemoteJoltUpdateAction::Verifying { target_version }
                        }
                        Err(error) => RemoteJoltUpdateAction::Failed {
                            target_version,
                            message: error.to_string(),
                        },
                    };
                    state.remote_update_actions.insert(device_id, action);
                }
                cx.notify();
            })
            .ok();
        });
        self.remote_update_action_tasks.insert(task_device_id, task);
        cx.notify();
    }

    pub fn apply_harness_updates(&mut self, statuses: Vec<HarnessUpdateStatus>) {
        self.harness_updates = statuses;
    }

    pub fn apply_auth(&mut self, auth: AuthState) {
        if !matches!(&auth, AuthState::SignedIn { .. }) {
            self.remote_harness_updates.clear();
            self.remote_harness_update_device_names.clear();
            self.remote_harness_update_tasks.clear();
            self.remote_updates.clear();
            self.remote_update_actions.clear();
            self.remote_update_tasks.clear();
            self.remote_update_action_tasks.clear();
        }
        self.auth = Some(auth);
    }

    pub fn active_scope(&self) -> ScopeKind {
        self.scope
            .as_ref()
            .map_or(ScopeKind::Account, |status| status.active)
    }

    pub fn account_available(&self) -> bool {
        self.scope
            .as_ref()
            .is_some_and(|status| status.account_available)
    }

    /// Tolerant AuthStatus frame reducer (see [`parse_auth_state`]).
    pub fn apply_auth_value(&mut self, value: serde_json::Value) {
        match parse_auth_state(&value) {
            Some(auth) => self.apply_auth(auth),
            None => tracing::warn!("dropping unrecognized AuthStatus frame"),
        }
    }

    /// The signed-in user, if the engine reports one.
    pub fn auth_user(&self) -> Option<&jolt_proto::UserProfile> {
        match self.auth.as_ref()? {
            AuthState::SignedIn { user, .. } | AuthState::NeedsOrganization { user } => Some(user),
            AuthState::SignedOut => None,
        }
    }

    fn clear_transcript_projection(&mut self) {
        self.transcript.clear();
        self.transcript_manifest = None;
        self.transcript_pages.clear();
        self.transcript_loading_pages.clear();
        self.transcript_page_errors.clear();
        self.transcript_sequence = 0;
    }

    fn trim_transcript_pages_around(&mut self, page_id: &str) {
        const PAGE_RADIUS: usize = 4;
        let Some(manifest) = self.transcript_manifest.as_ref() else {
            return;
        };
        let Some(center) = manifest.pages.iter().position(|page| page.id == page_id) else {
            return;
        };
        let live = manifest.pages.last().map(|page| page.id.as_str());
        self.transcript_pages.retain(|page| {
            live == Some(page.id.as_str())
                || manifest
                    .pages
                    .iter()
                    .position(|descriptor| descriptor.id == page.id)
                    .is_some_and(|index| index.abs_diff(center) <= PAGE_RADIUS)
        });
    }

    fn rebuild_loaded_transcript(&mut self) {
        self.transcript_pages.sort_by_key(|page| page.first_ordinal);
        self.transcript = self
            .transcript_pages
            .iter()
            .flat_map(|page| page.messages.iter().cloned())
            .collect();
        self.ack_pending_send_from_transcript();
    }

    pub fn apply_transcript_watch_frame(
        &mut self,
        frame: TranscriptWatchFrame,
    ) -> Result<(), TranscriptDesync> {
        match frame {
            TranscriptWatchFrame::Bootstrap { bootstrap } => {
                self.transcript_sequence = bootstrap.sequence;
                self.transcript_manifest = Some(bootstrap.manifest);
                self.transcript_pages = bootstrap.pages;
                self.transcript_loading_pages.clear();
                self.transcript_page_errors.clear();
                self.rebuild_loaded_transcript();
            }
            TranscriptWatchFrame::Delta {
                sequence,
                page_id,
                page_revision,
                frame,
            } => {
                if sequence != self.transcript_sequence.wrapping_add(1) {
                    return Err(TranscriptDesync(format!(
                        "sequence mismatch: have {}, received {sequence}",
                        self.transcript_sequence
                    )));
                }
                let Some(page) = self
                    .transcript_pages
                    .iter_mut()
                    .find(|page| page.id == page_id)
                else {
                    return Err(TranscriptDesync(format!(
                        "live page {page_id} is not loaded"
                    )));
                };
                jolt_session_doc::apply_transcript_frame(&mut page.messages, frame)?;
                page.revision = page_revision.clone();
                self.transcript_sequence = sequence;
                if let Some(manifest) = self.transcript_manifest.as_mut()
                    && let Some(descriptor) =
                        manifest.pages.iter_mut().find(|page| page.id == page_id)
                {
                    descriptor.revision = page_revision;
                    descriptor.message_count = page.messages.len();
                }
                self.rebuild_loaded_transcript();
            }
        }
        // Projection frames supersede optimistic echoes carrying the same id.
        if let Some(chat_id) = self.selected_chat.as_deref()
            && let Some(echoes) = self.echoes.get_mut(chat_id)
        {
            let transcript = &self.transcript;
            echoes.retain(|echo| !transcript.iter().any(|entry| entry.id == echo.id));
        }
        Ok(())
    }

    pub fn load_transcript_page(&mut self, page_id: String, cx: &mut Context<Self>) {
        if self.transcript_pages.iter().any(|page| page.id == page_id)
            || !self.transcript_loading_pages.insert(page_id.clone())
        {
            return;
        }
        self.transcript_page_errors.remove(&page_id);
        let Some(handle) = self.engine.clone() else {
            self.transcript_loading_pages.remove(&page_id);
            return;
        };
        let Some(chat_id) = self.selected_chat.clone() else {
            self.transcript_loading_pages.remove(&page_id);
            return;
        };
        cx.spawn(async move |this, cx| {
            let request = GetTranscriptPage {
                chat_id: chat_id.clone(),
                page_id: page_id.clone(),
                target_device_id: None,
            };
            let mut result = call_api(handle.client(), &request).await;
            for delay in [250u64, 1_000] {
                if result.is_ok() {
                    break;
                }
                cx.background_executor()
                    .timer(std::time::Duration::from_millis(delay))
                    .await;
                result = call_api(handle.client(), &request).await;
            }
            this.update(cx, |state, cx| {
                state.transcript_loading_pages.remove(&page_id);
                if state.selected_chat.as_deref() != Some(chat_id.as_str()) {
                    return;
                }
                match result {
                    Ok(page) => {
                        state.transcript_page_errors.remove(&page_id);
                        if !state
                            .transcript_pages
                            .iter()
                            .any(|loaded| loaded.id == page.id)
                        {
                            let loaded_id = page.id.clone();
                            state.transcript_pages.push(page);
                            state.trim_transcript_pages_around(&loaded_id);
                            state.rebuild_loaded_transcript();
                        }
                    }
                    Err(error) => {
                        tracing::warn!(%chat_id, %page_id, %error, "transcript page load failed");
                        state.transcript_page_errors.insert(page_id);
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    pub fn apply_transcript(&mut self, entries: Vec<SessionMessageEntry>) {
        // Doc frames supersede optimistic echoes carrying the same id.
        if let Some(chat_id) = self.selected_chat.as_deref()
            && let Some(echoes) = self.echoes.get_mut(chat_id)
        {
            echoes.retain(|echo| !entries.iter().any(|e| e.id == echo.id));
        }
        self.transcript = entries;
        self.ack_pending_send_from_transcript();
    }

    /// Add an optimistic transcript echo (prompt or pending shell command).
    pub fn push_echo(&mut self, chat_id: &str, entry: SessionMessageEntry) {
        let echoes = self.echoes.entry(chat_id.to_string()).or_default();
        if !echoes.iter().any(|e| e.id == entry.id) {
            echoes.push(entry);
        }
    }

    /// Drop an echo (send failed — the prompt returns to the draft).
    pub fn remove_echo(&mut self, chat_id: &str, message_id: &str) {
        if let Some(echoes) = self.echoes.get_mut(chat_id) {
            echoes.retain(|e| e.id != message_id);
        }
    }

    /// Overlay a queued send as Working until its host acknowledges the
    /// client-minted message id, the send fails, or the TTL expires.
    pub fn begin_pending_send(
        &mut self,
        chat_id: &str,
        message_id: &str,
        started_at: DateTime<Utc>,
    ) {
        self.pending_sends.insert(
            chat_id.to_string(),
            PendingSend {
                message_id: message_id.to_string(),
                started_at,
            },
        );
    }

    /// Clear only the overlay started by this message. A stale failure or TTL
    /// task must not clear a newer send for the same chat.
    pub fn end_pending_send(&mut self, chat_id: &str, message_id: &str) {
        if self
            .pending_sends
            .get(chat_id)
            .is_some_and(|pending| pending.message_id == message_id)
        {
            self.pending_sends.remove(chat_id);
        }
    }

    pub fn send_pending(&self, chat_id: &str, now: DateTime<Utc>) -> bool {
        self.pending_sends.get(chat_id).is_some_and(|pending| {
            now.signed_duration_since(pending.started_at)
                .num_milliseconds()
                <= PENDING_SEND_TTL_MS
        })
    }

    fn pending_send_host_device_id(&self, chat_id: &str) -> Option<&str> {
        if let Some(chat) = self.chats.iter().find(|chat| chat.id == chat_id) {
            return Some(&chat.device_id);
        }
        if self.selected_chat.as_deref() != Some(chat_id) {
            return None;
        }
        self.selected_space_row()
            .map(|space| space.device_id.as_str())
    }

    /// Host name while an unacknowledged send is waiting on an offline device.
    /// Unlike the Working overlay, this does not expire: the durable command is
    /// still queued until the host returns or the matching transcript entry lands.
    pub fn queued_send_offline_host_name(&self, chat_id: &str, now: DateTime<Utc>) -> Option<&str> {
        self.pending_sends.get(chat_id)?;
        let device_id = self.pending_send_host_device_id(chat_id)?;
        (!self.device_online(device_id, now))
            .then(|| self.device_name(device_id).unwrap_or("Unknown device"))
    }

    fn ack_pending_send_from_transcript(&mut self) {
        if let Some(chat_id) = self.selected_chat.as_deref()
            && let Some(pending) = self.pending_sends.get(chat_id)
            && self
                .transcript
                .iter()
                .any(|entry| entry.id == pending.message_id)
        {
            self.pending_sends.remove(chat_id);
        }
    }

    /// Unconfirmed echoes for the selected chat, in send order.
    pub fn pending_echoes(&self) -> &[SessionMessageEntry] {
        self.selected_chat
            .as_deref()
            .and_then(|id| self.echoes.get(id))
            .map(|v| v.as_slice())
            .unwrap_or(&[])
    }

    // ---- queries ----

    /// Non-archived chats in sidebar order.
    pub fn visible_chats(&self) -> impl Iterator<Item = &Chat> {
        self.chats.iter().filter(|c| !c.archived)
    }

    pub fn selected_space_row(&self) -> Option<&Space> {
        let id = self.selected_space.as_deref()?;
        self.spaces.iter().find(|s| s.id == id)
    }

    pub fn space_row(&self, space_id: &str) -> Option<&Space> {
        self.spaces.iter().find(|s| s.id == space_id)
    }

    /// Spaces in the stable alphabetical order used by both space pickers.
    pub fn spaces_sorted(&self) -> Vec<&Space> {
        let mut spaces: Vec<&Space> = self.spaces.iter().collect();
        spaces.sort_by_key(|space| (space.display_name().to_lowercase(), space.id.clone()));
        spaces
    }

    pub fn space_for_chat(&self, chat: &Chat) -> Option<&Space> {
        self.space_row(chat.space_id.as_deref()?)
    }

    /// Non-archived chats of a space. Chats with a dangling/missing `space_id`
    /// are invisible by construction.
    pub fn chats_in_space(&self, space_id: &str) -> Vec<&Chat> {
        self.visible_chats()
            .filter(|chat| chat.space_id.as_deref() == Some(space_id))
            .collect()
    }

    pub fn device_name(&self, device_id: &str) -> Option<&str> {
        self.devices
            .iter()
            .find(|d| d.id == device_id)
            .map(|d| d.name.as_str())
    }

    pub fn device_display_name(&self, device_id: &str) -> Option<&str> {
        if self.active_scope() == ScopeKind::Local {
            Some("local")
        } else {
            self.device_name(device_id)
        }
    }

    /// Host-presence check: is this device's 15s presence heartbeat fresh?
    /// Distinguishes "host offline" (its queued work syncs when it returns)
    /// from slow sync. The local device is trivially online; unknown devices
    /// get the benefit of the doubt (no evidence — don't cry wolf).
    pub fn device_online(&self, device_id: &str, now: DateTime<Utc>) -> bool {
        if self.local_device_id.as_deref() == Some(device_id) {
            return true;
        }
        match self.devices.iter().find(|d| d.id == device_id) {
            Some(d) => crate::settings::devices::device_online(d.last_seen_at, now),
            None => true,
        }
    }

    /// Space provenance shared by filter rows, session headers, and
    /// new-session chips. Returns the rendered tag and whether the host is
    /// offline.
    pub fn space_device_tag(&self, space: &Space, now: DateTime<Utc>) -> (String, bool) {
        let offline =
            self.active_scope() != ScopeKind::Local && !self.device_online(&space.device_id, now);
        let device = self
            .device_display_name(&space.device_id)
            .unwrap_or("Unknown device");
        let tag = if offline {
            format!("@ {device} · offline")
        } else {
            format!("@ {device}")
        };
        (tag, offline)
    }

    /// Does the selected space's folder have git? Drives the branch picker and
    /// the diff sidebar (owner-stamped, synced — no RPC).
    pub fn selected_space_git(&self) -> bool {
        self.selected_space_row().is_some_and(|s| s.git_detected)
    }

    /// Full display status for a chat (session header and Active list). A
    /// pending send reads as Working only while its host is reachable; offline sends remain
    /// queued without impersonating active work.
    pub fn display_status_for(&self, chat: &Chat, now: DateTime<Utc>) -> ChatIndicator {
        if self.queued_send_offline_host_name(&chat.id, now).is_none()
            && self.send_pending(&chat.id, now)
        {
            return ChatIndicator::Working;
        }
        display_status(chat, self.session_for(&chat.id), now)
    }

    /// The sidebar's Threads list: every non-archived chat of a LIVE space,
    /// on any device — idle included — pinned first, then in pure recency
    /// order (status drives the dot, never the position; see [`sort_active`]).
    pub fn overview_chats(&self, now: DateTime<Utc>) -> Vec<(ChatIndicator, &Chat)> {
        let mut rows: Vec<(ChatIndicator, &Chat)> = self
            .visible_chats()
            .filter(|c| {
                c.space_id
                    .as_deref()
                    .is_some_and(|id| self.space_row(id).is_some())
            })
            .map(|c| (self.display_status_for(c, now), c))
            .collect();
        sort_active(&mut rows);
        rows
    }

    pub fn session_for(&self, chat_id: &str) -> Option<&Session> {
        self.sessions.iter().find(|s| s.chat_id == chat_id)
    }

    /// Staleness-checked status dot for a chat row.
    pub fn indicator_for(&self, chat_id: &str, now: DateTime<Utc>) -> Indicator {
        if self.queued_send_offline_host_name(chat_id, now).is_none()
            && self.send_pending(chat_id, now)
        {
            return Indicator::Working;
        }
        effective_indicator(self.session_for(chat_id), now)
    }

    pub fn selected_chat_row(&self) -> Option<&Chat> {
        let id = self.selected_chat.as_deref()?;
        self.chats.iter().find(|c| c.id == id)
    }

    pub fn gate(&self) -> GatePhase {
        if self.connection == ConnectionStatus::Ready
            && self
                .scope
                .as_ref()
                .is_some_and(|status| status.active == ScopeKind::Local)
        {
            GatePhase::Ready
        } else {
            gate_phase(&self.connection, self.auth.as_ref())
        }
    }

    pub fn engine(&self) -> Option<&EngineHandle> {
        self.engine.as_ref()
    }

    // ---- gpui glue ----

    /// Kick off (or retry) the engine bootstrap: dial → connect-or-embed on
    /// tokio, then attach subscriptions. Safe to call again after `Failed`.
    pub fn bootstrap(
        state: Entity<AppState>,
        config: EngineBootConfig,
        connector: EngineConnector,
        cx: &mut App,
    ) {
        let data_dir = config.data_dir.clone();
        state.update(cx, |s, cx| {
            s.connection = ConnectionStatus::Connecting;
            s.data_dir = Some(data_dir);
            cx.notify();
        });
        let boot = Tokio::spawn(cx, connector(config));
        cx.spawn(async move |cx| {
            let outcome = match boot.await {
                Ok(Ok(handle)) => Ok(handle),
                Ok(Err(err)) => Err(format!("{err:#}")),
                Err(join_err) => Err(join_err.to_string()),
            };
            // NB: at the pinned rev `Entity::update(&mut AsyncApp)` returns the
            // closure's value directly (no Result) — AsyncApp implements
            // AppContext like App does.
            state.update(cx, |s, cx| match outcome {
                Ok(handle) => s.attach_engine(handle, cx),
                Err(message) => {
                    tracing::error!(%message, "engine bootstrap failed");
                    s.connection = ConnectionStatus::Failed(message);
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// Wire the connected engine and start standing watches. The splash stays
    /// up until `ScopeStatus` chooses Local or Account; engines without scope
    /// support mark Ready when that subscription is rejected.
    fn attach_engine(&mut self, handle: EngineHandle, cx: &mut Context<Self>) {
        self.engine = Some(handle.clone());
        self.restart_scope_watches(handle.clone(), cx);
        self.restart_theme_sync(handle.clone(), cx);
        self.global_tasks = vec![
            spawn_auth_watch(cx, handle.clone()),
            spawn_scope_watch(cx, handle.clone()),
            spawn_watch(
                cx,
                handle.clone(),
                WatchUpdateStatus::default(),
                AppState::apply_update,
            ),
            spawn_watch(
                cx,
                handle.clone(),
                WatchHarnessUpdates::default(),
                AppState::apply_harness_updates,
            ),
        ];
        // Re-subscribe selected-chat projections after reconnect.
        if let Some(chat_id) = self.selected_chat.clone() {
            let target_device_id = self
                .chats
                .iter()
                .find(|chat| chat.id == chat_id)
                .map(|chat| chat.device_id.clone());
            self.transcript_task =
                Some(spawn_transcript_watch(cx, handle.clone(), chat_id.clone()));
            self.queue_task = Some(spawn_queue_watch(cx, handle.clone(), chat_id.clone()));
            self.usage_task = Some(spawn_usage_watch(cx, handle, chat_id, target_device_id));
        }
        cx.notify();
    }

    fn restart_theme_sync(&mut self, handle: EngineHandle, cx: &mut Context<Self>) {
        let Some(data_dir) = self.data_dir.clone() else {
            return;
        };
        self.theme_sync_task = Some(spawn_theme_file_sync(cx, handle, data_dir));
    }

    fn restart_scope_watches(&mut self, handle: EngineHandle, cx: &mut Context<Self>) {
        self.devices.clear();
        self.spaces.clear();
        self.chats.clear();
        self.sessions.clear();
        self.selected_space = None;
        self.selected_chat = None;
        self.selected_usage = None;
        self.clear_transcript_projection();
        self.transcript_task = None;
        self.queued_prompts.clear();
        self.queue_task = None;
        self.usage_task = None;
        self.local_device_id = None;
        self.spaces_synced = false;
        self.watch_tasks = vec![
            spawn_sessions_watch(cx, handle.clone()),
            spawn_chats_watch(cx, handle.clone()),
            spawn_devices_watch(cx, handle.clone()),
            spawn_watch(
                cx,
                handle.clone(),
                WatchSpaces::default(),
                AppState::apply_spaces,
            ),
            spawn_local_device_probe(cx, handle),
        ];
    }

    /// Select a chat (or clear). Swaps the per-chat doc-transcript subscription:
    /// dropping the old task drops its stream receiver, which cancels the doc
    /// watch server-side. Selecting a chat also lands in its space and marks it
    /// seen.
    pub fn select_chat(&mut self, chat_id: Option<String>, cx: &mut Context<Self>) {
        if self.selected_chat == chat_id {
            // Re-selecting still clears a fresh "completed" badge.
            if let Some(id) = chat_id {
                self.mark_chat_seen(&id, cx);
            }
            return;
        }
        self.selected_chat = chat_id.clone();
        self.clear_transcript_projection();
        self.transcript_task = None;
        self.queued_prompts.clear();
        self.queue_task = None;
        self.selected_usage = None;
        self.usage_task = None;
        if let Some(id) = chat_id.as_deref() {
            // A chat implies its space; `select_chat(None)` (the new-session
            // canvas) stays within the current space.
            if let Some(space_id) = self
                .chats
                .iter()
                .find(|c| c.id == id)
                .and_then(|c| c.space_id.clone())
            {
                self.selected_space = Some(space_id);
            }
            self.mark_chat_seen(id, cx);
        }
        if let (Some(chat_id), Some(handle)) = (chat_id, self.engine.clone()) {
            let target_device_id = self
                .chats
                .iter()
                .find(|chat| chat.id == chat_id)
                .map(|chat| chat.device_id.clone());
            self.transcript_task =
                Some(spawn_transcript_watch(cx, handle.clone(), chat_id.clone()));
            self.queue_task = Some(spawn_queue_watch(cx, handle.clone(), chat_id.clone()));
            self.usage_task = Some(spawn_usage_watch(cx, handle, chat_id, target_device_id));
        }
        cx.notify();
    }

    /// Select a space; the caller (shell) decides which chat to land on.
    pub fn select_space(&mut self, space_id: Option<String>, cx: &mut Context<Self>) {
        if self.selected_space == space_id {
            return;
        }
        self.selected_space = space_id;
        cx.notify();
    }

    /// Synced seen marker: only fires when the chat is currently unseen
    /// (idempotence — no mutate spam), stamps the local row optimistically so
    /// the LWW round-trip is invisible, and fire-and-forgets the mutate.
    /// Window-focus liveness sweep: ask the engine to probe every open room
    /// (workspace + chat docs). Fire-and-forget; each room ignores the hint
    /// unless it has been broadcast-quiet ≥30s, so spamming is harmless.
    pub fn probe_sync(&mut self, cx: &mut Context<Self>) {
        let Some(handle) = self.engine.clone() else {
            return;
        };
        cx.spawn(async move |_, _| {
            if let Err(err) = call_api(handle.client(), &ProbeSync::default()).await {
                tracing::debug!(error = %err, "probe sync failed");
            }
        })
        .detach();
    }

    pub fn mark_chat_seen(&mut self, chat_id: &str, cx: &mut Context<Self>) {
        let Some(chat) = self.chats.iter_mut().find(|c| c.id == chat_id) else {
            return;
        };
        if !chat.unseen() {
            return;
        }
        chat.last_seen_at = Some(Utc::now());
        cx.notify();
        let Some(handle) = self.engine.clone() else {
            return;
        };
        let chat_id = chat_id.to_string();
        cx.spawn(async move |_, _| {
            let request = Mutate::MarkChatSeen {
                chat_id: chat_id.clone(),
                at: None,
            };
            if let Err(err) = call_api(handle.client(), &request).await {
                tracing::warn!(chat = %chat_id, error = %err, "markChatSeen failed");
            }
        })
        .detach();
    }
}

/// Reconcile installation-level theme files with the signed-in account
/// registry. The task intentionally survives Local/Account viewport switches;
/// the supervisor routes these methods to the account runtime directly.
fn spawn_theme_file_sync(
    cx: &mut Context<AppState>,
    handle: EngineHandle,
    data_dir: PathBuf,
) -> Task<()> {
    const INTERVAL: std::time::Duration = std::time::Duration::from_secs(5);
    cx.spawn(async move |this, cx| {
        let mut known = Vec::<ThemeFileRecord>::new();
        loop {
            let local = match crate::themes::local_theme_records(&data_dir) {
                Ok(records) => records,
                Err(err) => {
                    tracing::warn!(error = %err, "could not read local theme files for sync");
                    cx.background_executor().timer(INTERVAL).await;
                    continue;
                }
            };
            let local_map: std::collections::BTreeMap<_, _> = local
                .iter()
                .map(|record| (record.id.clone(), record.contents.clone()))
                .collect();
            let initial_remote = match call_api(handle.client(), &ListThemes::default()).await {
                Ok(records) => records,
                Err(err) => {
                    tracing::debug!(error = %err, "theme sync unavailable; retrying");
                    cx.background_executor().timer(INTERVAL).await;
                    continue;
                }
            };

            let plan = match crate::themes::plan_theme_file_sync(&local, &initial_remote, &known) {
                Ok(plan) => plan,
                Err(err) => {
                    tracing::warn!(error = %err, "could not reconcile conflicting theme files");
                    cx.background_executor().timer(INTERVAL).await;
                    continue;
                }
            };
            let mut mutated = false;
            if !plan.upserts.is_empty() {
                if let Err(err) = call_api(
                    handle.client(),
                    &UpsertThemes {
                        themes: plan.upserts.clone(),
                    },
                )
                .await
                {
                    tracing::debug!(error = %err, "theme upload interrupted; retrying");
                    cx.background_executor().timer(INTERVAL).await;
                    continue;
                }
                known = plan.project_upserts_onto(&initial_remote);
                mutated = true;
            }
            let mut deletion_failed = false;
            for id in &plan.deletes {
                if let Err(err) = call_api(handle.client(), &DeleteTheme { id: id.clone() }).await {
                    tracing::debug!(error = %err, "theme deletion interrupted; retrying");
                    deletion_failed = true;
                    break;
                }
                mutated = true;
            }
            if deletion_failed {
                cx.background_executor().timer(INTERVAL).await;
                continue;
            }
            if mutated {
                known = plan.project_onto(&initial_remote);
            }
            let remote = if mutated {
                let Ok(records) = call_api(handle.client(), &ListThemes::default()).await else {
                    cx.background_executor().timer(INTERVAL).await;
                    continue;
                };
                records
            } else {
                initial_remote
            };
            let remote_map: std::collections::BTreeMap<_, _> = remote
                .iter()
                .filter(|record| !record.deleted)
                .map(|record| (record.id.clone(), record.contents.clone()))
                .collect();
            if remote_map != local_map {
                match crate::themes::install_synced_theme_files(&remote, &data_dir) {
                    Ok(()) => {
                        this.update(cx, |_, cx| crate::appearance::reload_theme_files(cx))
                            .ok();
                    }
                    Err(err) => {
                        tracing::warn!(error = %err, "dropping invalid synced theme frame");
                    }
                }
            }
            known = remote;
            cx.background_executor().timer(INTERVAL).await;
        }
    })
}

fn spawn_sessions_watch(cx: &mut Context<AppState>, handle: EngineHandle) -> Task<()> {
    cx.spawn(async move |this, cx| {
        let mut rx = match subscribe_api(handle.client(), &WatchSessions::default()).await {
            Ok(rx) => rx,
            Err(err) => {
                tracing::debug!(error = %err, "sessions watch unavailable");
                return;
            }
        };
        while let Some(value) = rx.recv().await {
            let frame: SessionWatchFrame = match serde_json::from_value(value) {
                Ok(frame) => frame,
                Err(err) => {
                    tracing::warn!(error = %err, "dropping malformed sessions frame");
                    continue;
                }
            };
            if this
                .update(cx, |state, cx| {
                    state.apply_session_watch_frame(frame);
                    cx.notify();
                })
                .is_err()
            {
                break;
            }
        }
    })
}

/// Chats watch. Session selection remains viewport-local in the shell.
fn spawn_chats_watch(cx: &mut Context<AppState>, handle: EngineHandle) -> Task<()> {
    cx.spawn(async move |this, cx| {
        let mut rx = match subscribe_api(handle.client(), &WatchChats::default()).await {
            Ok(rx) => rx,
            Err(err) => {
                tracing::debug!(error = %err, "chats watch unavailable");
                return;
            }
        };
        while let Some(value) = rx.recv().await {
            let frame: ChatWatchFrame = match serde_json::from_value(value) {
                Ok(frame) => frame,
                Err(err) => {
                    tracing::warn!(error = %err, "dropping malformed chats frame");
                    continue;
                }
            };
            let alive = this.update(cx, |state, cx| {
                state.apply_chat_watch_frame(frame);
                cx.notify();
            });
            if alive.is_err() {
                break;
            }
        }
    })
}

/// Pump authentication state frames.
fn spawn_auth_watch(cx: &mut Context<AppState>, handle: EngineHandle) -> Task<()> {
    cx.spawn(async move |this, cx| {
        let mut rx = match subscribe_api(handle.client(), &WatchAuthStatus::default()).await {
            Ok(rx) => rx,
            Err(err) => {
                tracing::debug!(error = %err, "auth watch unavailable");
                return;
            }
        };
        while let Some(value) = rx.recv().await {
            let Some(auth) = parse_auth_state(&value) else {
                tracing::warn!("dropping unrecognized AuthStatus frame");
                continue;
            };
            if this
                .update(cx, |state, cx| {
                    state.apply_auth(auth);
                    cx.notify();
                })
                .is_err()
            {
                break;
            }
        }
    })
}

fn spawn_scope_watch(cx: &mut Context<AppState>, handle: EngineHandle) -> Task<()> {
    cx.spawn(async move |this, cx| {
        let mut rx = match subscribe_api(handle.client(), &WatchScopeStatus::default()).await {
            Ok(rx) => rx,
            Err(err) => {
                tracing::warn!(error = %err, "scope watch unavailable");
                this.update(cx, |state, cx| {
                    state.connection =
                        ConnectionStatus::Failed(format!("ScopeStatus unavailable: {err}"));
                    cx.notify();
                })
                .ok();
                return;
            }
        };
        let mut received_status = false;
        while let Some(value) = rx.recv().await {
            let status: ScopeStatus = match serde_json::from_value(value) {
                Ok(status) => status,
                Err(err) => {
                    tracing::warn!(error = %err, "dropping malformed scope frame");
                    continue;
                }
            };
            received_status = true;
            if this
                .update(cx, |state, cx| {
                    let changed = state.scope.as_ref().map(|old| old.active) != Some(status.active);
                    state.scope = Some(status);
                    state.connection = ConnectionStatus::Ready;
                    if changed {
                        state.restart_scope_watches(handle.clone(), cx);
                    }
                    cx.notify();
                })
                .is_err()
            {
                break;
            }
        }
        // RPC stream errors arrive as a closed receiver after subscribe succeeds.
        if !received_status {
            tracing::warn!("scope watch closed before its initial status");
            this.update(cx, |state, cx| {
                state.connection = ConnectionStatus::Failed(
                    "ScopeStatus stream closed before initialization".into(),
                );
                cx.notify();
            })
            .ok();
        }
    })
}

fn spawn_watch<R>(
    cx: &mut Context<AppState>,
    handle: EngineHandle,
    request: R,
    apply: fn(&mut AppState, R::Item),
) -> Task<()>
where
    R: StreamRequest + Send + 'static,
{
    cx.spawn(async move |this, cx| {
        let mut rx = match subscribe_api(handle.client(), &request).await {
            Ok(rx) => rx,
            Err(err) => {
                tracing::debug!(method = R::METHOD, error = %err, "watch unavailable");
                return;
            }
        };
        while let Some(value) = rx.recv().await {
            let parsed: R::Item = match serde_json::from_value(value) {
                Ok(parsed) => parsed,
                Err(err) => {
                    tracing::warn!(method = R::METHOD, error = %err, "dropping malformed watch frame");
                    continue;
                }
            };
            let alive = this.update(cx, |state, cx| {
                apply(state, parsed);
                cx.notify();
            });
            if alive.is_err() {
                break;
            }
        }
    })
}

fn spawn_devices_watch(cx: &mut Context<AppState>, handle: EngineHandle) -> Task<()> {
    cx.spawn(async move |this, cx| {
        let request = WatchDevices::default();
        let mut rx = match subscribe_api(handle.client(), &request).await {
            Ok(rx) => rx,
            Err(error) => {
                tracing::debug!(%error, "devices watch unavailable");
                return;
            }
        };
        while let Some(value) = rx.recv().await {
            let devices: Vec<Device> = match serde_json::from_value(value) {
                Ok(devices) => devices,
                Err(error) => {
                    tracing::warn!(%error, "dropping malformed devices watch frame");
                    continue;
                }
            };
            if this
                .update(cx, |state, cx| {
                    state.apply_devices(devices);
                    state.reconcile_remote_harness_update_watches(cx);
                    cx.notify();
                })
                .is_err()
            {
                break;
            }
        }
    })
}

fn spawn_remote_update_watch(
    cx: &mut Context<AppState>,
    handle: EngineHandle,
    device_id: String,
) -> Task<()> {
    cx.spawn(async move |this, cx| {
        const RETRY_DELAY: std::time::Duration = std::time::Duration::from_secs(10);
        loop {
            let request = WatchUpdateStatus {
                target_device_id: Some(device_id.clone()),
            };
            let mut rx = match subscribe_api(handle.client(), &request).await {
                Ok(rx) => rx,
                Err(error) => {
                    tracing::debug!(%device_id, %error, "remote Jolt update watch unavailable; retrying");
                    if this.update(cx, |_, _| {}).is_err() {
                        return;
                    }
                    cx.background_executor().timer(RETRY_DELAY).await;
                    continue;
                }
            };
            while let Some(value) = rx.recv().await {
                let status: jolt_update::UpdateStatus = match serde_json::from_value(value) {
                    Ok(status) => status,
                    Err(error) => {
                        tracing::warn!(%device_id, %error, "dropping malformed remote Jolt update frame");
                        continue;
                    }
                };
                if this
                    .update(cx, |state, cx| {
                        state.apply_remote_update(device_id.clone(), status);
                        cx.notify();
                    })
                    .is_err()
                {
                    return;
                }
            }
            if this.update(cx, |_, _| {}).is_err() {
                return;
            }
            cx.background_executor().timer(RETRY_DELAY).await;
        }
    })
}

fn spawn_remote_harness_update_watch(
    cx: &mut Context<AppState>,
    handle: EngineHandle,
    device_id: String,
) -> Task<()> {
    cx.spawn(async move |this, cx| {
        const RETRY_DELAY: std::time::Duration = std::time::Duration::from_secs(10);
        loop {
            let request = WatchHarnessUpdates {
                target_device_id: Some(device_id.clone()),
            };
            let mut rx = match subscribe_api(handle.client(), &request).await {
                Ok(rx) => rx,
                Err(error) => {
                    tracing::debug!(%device_id, %error, "remote harness update watch unavailable; retrying");
                    if this.update(cx, |_, _| {}).is_err() {
                        return;
                    }
                    cx.background_executor().timer(RETRY_DELAY).await;
                    continue;
                }
            };
            while let Some(value) = rx.recv().await {
                let statuses: Vec<HarnessUpdateStatus> = match serde_json::from_value(value) {
                    Ok(statuses) => statuses,
                    Err(error) => {
                        tracing::warn!(%device_id, %error, "dropping malformed remote harness update frame");
                        continue;
                    }
                };
                if this
                    .update(cx, |state, cx| {
                        state
                            .remote_harness_updates
                            .insert(device_id.clone(), statuses);
                        cx.notify();
                    })
                    .is_err()
                {
                    return;
                }
            }
            if this.update(cx, |_, _| {}).is_err() {
                return;
            }
            cx.background_executor().timer(RETRY_DELAY).await;
        }
    })
}

/// Best-effort `LocalDevice` probe: fills `local_device_id` for the "This
/// device" badge. Engines that don't serve the method leave it `None`.
fn spawn_local_device_probe(cx: &mut Context<AppState>, handle: EngineHandle) -> Task<()> {
    cx.spawn(async move |this, cx| {
        let Ok(device) = call_api(handle.client(), &GetLocalDevice::default()).await else {
            tracing::debug!("LocalDevice unavailable; skipping this-device badge");
            return;
        };
        this.update(cx, |state, cx| {
            state.local_device_id = Some(device.device_id);
            state.reconcile_remote_harness_update_watches(cx);
            cx.notify();
        })
        .ok();
    })
}

fn spawn_usage_watch(
    cx: &mut Context<AppState>,
    handle: EngineHandle,
    chat_id: String,
    target_device_id: Option<String>,
) -> Task<()> {
    cx.spawn(async move |this, cx| {
        const RETRY_DELAY: std::time::Duration = std::time::Duration::from_secs(2);
        loop {
            let request = WatchChatUsage {
                chat_id: chat_id.clone(),
                target_device_id: target_device_id.clone(),
            };
            let mut rx = match subscribe_api(handle.client(), &request).await {
                Ok(rx) => rx,
                Err(error) => {
                    tracing::debug!(%chat_id, %error, "usage watch unavailable; retrying");
                    if this.update(cx, |_, _| {}).is_err() {
                        return;
                    }
                    cx.background_executor().timer(RETRY_DELAY).await;
                    continue;
                }
            };
            while let Some(value) = rx.recv().await {
                let Ok(usage) = serde_json::from_value::<UsageSummary>(value) else {
                    tracing::warn!(%chat_id, "dropping malformed usage summary");
                    continue;
                };
                if this
                    .update(cx, |state, cx| {
                        if state.selected_chat.as_deref() == Some(chat_id.as_str()) {
                            state.selected_usage = Some(usage);
                            cx.notify();
                        }
                    })
                    .is_err()
                {
                    return;
                }
            }
            if this.update(cx, |_, _| {}).is_err() {
                return;
            }
            cx.background_executor().timer(RETRY_DELAY).await;
        }
    })
}

fn spawn_queue_watch(
    cx: &mut Context<AppState>,
    handle: EngineHandle,
    chat_id: String,
) -> Task<()> {
    cx.spawn(async move |this, cx| {
        const RETRY_DELAY: std::time::Duration = std::time::Duration::from_secs(2);
        loop {
            let request = WatchQueuedPrompts {
                chat_id: chat_id.clone(),
            };
            let mut rx = match subscribe_api(handle.client(), &request).await {
                Ok(rx) => rx,
                Err(error) => {
                    tracing::warn!(%chat_id, %error, "queue watch failed; retrying");
                    cx.background_executor().timer(RETRY_DELAY).await;
                    continue;
                }
            };
            while let Some(value) = rx.recv().await {
                let prompts: Vec<QueuedPrompt> = match serde_json::from_value(value) {
                    Ok(prompts) => prompts,
                    Err(error) => {
                        tracing::warn!(%chat_id, %error, "malformed queue frame");
                        break;
                    }
                };
                if this
                    .update(cx, |state, cx| {
                        if state.selected_chat.as_deref() == Some(chat_id.as_str()) {
                            state.queued_prompts = prompts;
                            cx.notify();
                        }
                    })
                    .is_err()
                {
                    return;
                }
            }
            if this.update(cx, |_, _| {}).is_err() {
                return;
            }
            cx.background_executor().timer(RETRY_DELAY).await;
        }
    })
}

fn spawn_transcript_watch(
    cx: &mut Context<AppState>,
    handle: EngineHandle,
    chat_id: String,
) -> Task<()> {
    cx.spawn(async move |this, cx| {
        // Outer loop: a delta desync (missed frame) resubscribes immediately
        // and the fresh stream's opening reset heals the copy; a subscribe
        // failure, malformed frame, or stream end retries on a delay. Every
        // path re-enters the loop — a return here freezes the transcript
        // with no banner and no heal short of an app restart (this watch and
        // its engine-side room are the ONLY transcript delivery path). The
        // task itself is dropped by select_chat/apply_chats when the chat is
        // deselected or deleted, so retrying can't outlive relevance.
        const RETRY_DELAY: std::time::Duration = std::time::Duration::from_secs(2);
        'resubscribe: loop {
            let request = WatchTranscript {
                chat_id: chat_id.clone(),
            };
            let mut rx = match subscribe_api(handle.client(), &request).await {
                Ok(rx) => rx,
                Err(err) => {
                    tracing::warn!(%chat_id, error = %err, "transcript watch failed; retrying");
                    if this.update(cx, |_, _| {}).is_err() {
                        return;
                    }
                    cx.background_executor().timer(RETRY_DELAY).await;
                    continue 'resubscribe;
                }
            };
            while let Some(value) = rx.recv().await {
                let frame: TranscriptWatchFrame = match serde_json::from_value(value) {
                    Ok(frame) => frame,
                    Err(err) => {
                        // Schema skew (a newer peer's entry shape arriving
                        // through sync): a skipped frame is a silently stale
                        // copy, so resubscribe for a fresh reset — delayed,
                        // in case the reset itself is what can't parse.
                        tracing::warn!(error = %err, "malformed transcript frame; resubscribing");
                        cx.background_executor().timer(RETRY_DELAY).await;
                        continue 'resubscribe;
                    }
                };
                let mut desync = false;
                let alive = this.update(cx, |state, cx| {
                    // Guard against a stale pump racing a newer selection.
                    if state.selected_chat.as_deref() == Some(chat_id.as_str()) {
                        if let Err(err) = state.apply_transcript_watch_frame(frame) {
                            tracing::warn!(%chat_id, error = %err, "resubscribing transcript");
                            desync = true;
                        }
                        cx.notify();
                    }
                });
                if alive.is_err() {
                    return;
                }
                if desync {
                    continue 'resubscribe;
                }
            }
            // Stream ended: engine restart, RPC drop, or chat purge. Retry;
            // the purge case is cleaned up by apply_chats dropping this task.
            tracing::debug!(%chat_id, "transcript stream ended; resubscribing");
            if this.update(cx, |_, _| {}).is_err() {
                return;
            }
            cx.background_executor().timer(RETRY_DELAY).await;
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeDelta;
    use jolt_engine::{EngineCore, default_registry};
    // `SessionStatus` is only needed to build the fixtures below — the module
    // itself derives everything through `jolt_proto::view`.
    use jolt_proto::{SessionStatus, UserProfile};

    /// A localhost port that was just free (bind :0, read, drop).
    async fn free_port() -> u16 {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        listener.local_addr().unwrap().port()
    }

    #[tokio::test]
    async fn bootstrap_embeds_engine_when_port_is_free() {
        let dir = tempfile::tempdir().unwrap();
        let handle = EngineHandle::bootstrap(EngineBootConfig {
            data_dir: dir.path().to_path_buf(),
            ipc_port: free_port().await,
            edge_url: "http://127.0.0.1:1".into(),
            edge_token: None, // offline
            org_id: None,
            workos_client_id: None,
            default_harness: HarnessId::Mock,
        })
        .await
        .unwrap();
        assert_eq!(handle.mode(), EngineMode::InProcess);
        // Same protocol over the in-memory transport: a real engine answers.
        let harnesses = call_api(handle.client(), &ListHarnesses::default())
            .await
            .unwrap();
        assert!(!harnesses.is_empty());
        handle.shutdown().await;
    }

    #[tokio::test]
    async fn an_embedded_engine_serves_the_ipc_port_for_other_viewports() {
        // The whole point of embedding-and-serving: a second viewport (the
        // terminal app) can attach to this window's engine with no setup, no
        // separate daemon, and no launch ordering.
        let dir = tempfile::tempdir().unwrap();
        let port = free_port().await;
        let handle = EngineHandle::bootstrap(EngineBootConfig {
            data_dir: dir.path().to_path_buf(),
            ipc_port: port,
            edge_url: "http://127.0.0.1:1".into(),
            edge_token: None, // offline
            org_id: None,
            workos_client_id: None,
            default_harness: HarnessId::Mock,
        })
        .await
        .unwrap();
        assert_eq!(handle.mode(), EngineMode::InProcess);

        // Attach the way an external viewport would, and speak the same protocol.
        let attached = jolt_rpc::connect_ws(&format!("ws://127.0.0.1:{port}"))
            .await
            .expect("a second viewport must be able to attach");
        let harnesses = call_api(&attached, &ListHarnesses::default())
            .await
            .unwrap();
        assert!(!harnesses.is_empty());

        // Shutting the window down stops accepting, so the next viewport
        // starts its own engine rather than talking to closing stores.
        handle.shutdown().await;
        assert!(
            tokio::net::TcpStream::connect(("127.0.0.1", port))
                .await
                .is_err(),
            "the port must be released on shutdown"
        );
    }

    #[tokio::test]
    async fn a_stranger_on_the_ipc_port_does_not_wedge_the_window() {
        // A process that accepts TCP and never speaks WebSocket used to hang
        // the dial forever; now it times out and we embed instead, losing only
        // the ability to serve other viewports.
        let squatter = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let port = squatter.local_addr().unwrap().port();
        let dir = tempfile::tempdir().unwrap();
        let handle = EngineHandle::bootstrap(EngineBootConfig {
            data_dir: dir.path().to_path_buf(),
            ipc_port: port,
            edge_url: "http://127.0.0.1:1".into(),
            edge_token: None,
            org_id: None,
            workos_client_id: None,
            default_harness: HarnessId::Mock,
        })
        .await
        .expect("a taken port must not fail the boot");
        assert_eq!(handle.mode(), EngineMode::InProcess);
        assert!(
            call_api(handle.client(), &ListHarnesses::default())
                .await
                .is_ok(),
            "the window still works over its own transport"
        );
        handle.shutdown().await;
        drop(squatter);
    }

    #[tokio::test]
    async fn production_bootstrap_opens_local_without_sign_in() {
        let dir = tempfile::tempdir().unwrap();
        let handle = EngineHandle::bootstrap(EngineBootConfig {
            data_dir: dir.path().to_path_buf(),
            ipc_port: free_port().await,
            edge_url: "http://127.0.0.1:1".into(),
            edge_token: None,
            org_id: None,
            workos_client_id: Some("client_test".into()),
            default_harness: HarnessId::Mock,
        })
        .await
        .unwrap();

        let mut auth = subscribe_api(handle.client(), &WatchAuthStatus::default())
            .await
            .unwrap();
        assert_eq!(
            parse_auth_state(&auth.recv().await.unwrap()),
            Some(AuthState::SignedOut)
        );
        let harnesses = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            call_api(handle.client(), &ListHarnesses::default()),
        )
        .await
        .expect("Local runtime assembled")
        .expect("Local runtime is available while signed out");
        assert!(!harnesses.is_empty());
        let scope = call_api(
            handle.client(),
            &SwitchScope {
                scope: ScopeKind::Local,
            },
        )
        .await
        .unwrap();
        assert_eq!(scope.active, ScopeKind::Local);
        assert!(dir.path().join("scopes/local/current").exists());
        assert!(
            !dir.path().join("orgs/dev-org/dev-user").exists(),
            "production Local boot must not create dev-user data"
        );
        handle.shutdown().await;
    }

    #[tokio::test]
    async fn bootstrap_connects_when_daemon_is_listening() {
        // Stand in for `jolt headless`: an engine served over the WS IPC port.
        let daemon_dir = tempfile::tempdir().unwrap();
        let core = EngineCore::assemble(
            daemon_dir.path(),
            Arc::new(default_registry()),
            HarnessId::Mock,
            None,
        )
        .unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(jolt_rpc::serve_ws_listener(listener, core.rpc_service()));

        let ui_dir = tempfile::tempdir().unwrap();
        let handle = EngineHandle::bootstrap(EngineBootConfig {
            data_dir: ui_dir.path().to_path_buf(),
            ipc_port: port,
            edge_url: "http://127.0.0.1:1".into(),
            edge_token: None,
            org_id: None,
            workos_client_id: None,
            default_harness: HarnessId::Mock,
        })
        .await
        .unwrap();
        assert_eq!(
            handle.mode(),
            EngineMode::Remote {
                url: format!("ws://127.0.0.1:{port}")
            }
        );
        let harnesses = call_api(handle.client(), &ListHarnesses::default())
            .await
            .unwrap();
        assert!(!harnesses.is_empty());
    }

    fn chat(id: &str, created_min: i64, last_msg_min: Option<i64>) -> Chat {
        let base = DateTime::parse_from_rfc3339("2026-07-19T12:00:00Z")
            .unwrap()
            .to_utc();
        Chat {
            id: id.into(),
            device_id: "dev".into(),
            title: None,
            archived: false,
            pinned: false,
            cwd: None,
            branch: None,
            checkout_id: None,
            config: None,
            last_message_preview: None,
            last_message_at: last_msg_min.map(|m| base + TimeDelta::minutes(m)),
            created_at: base + TimeDelta::minutes(created_min),
            harness_session_id: None,
            harness_session_cwd: None,
            harness_conversations: Vec::new(),
            space_id: None,
            last_seen_at: None,
            goal: None,
        }
    }

    fn user_entry(id: &str) -> SessionMessageEntry {
        SessionMessageEntry {
            id: id.into(),
            role: jolt_session_doc::MessageRole::User,
            parts: Vec::new(),
            created_at: 0,
            device_id: "dev".into(),
            status: None,
            continuation_of: None,
        }
    }

    fn space(id: &str, device_id: &str, path: &str, created_min: i64) -> Space {
        let base = DateTime::parse_from_rfc3339("2026-07-19T12:00:00Z")
            .unwrap()
            .to_utc();
        Space {
            id: id.into(),
            device_id: device_id.into(),
            path: path.into(),
            name: None,
            git_detected: false,
            git_checked_at: None,
            checkout_id: None,
            created_at: base + TimeDelta::minutes(created_min),
        }
    }

    fn session(
        chat_id: &str,
        status: SessionStatus,
        updated_secs_ago: i64,
        now: DateTime<Utc>,
    ) -> Session {
        Session {
            chat_id: chat_id.into(),
            device_id: "dev".into(),
            status,
            compacting: false,
            started_at: None,
            updated_at: now - TimeDelta::seconds(updated_secs_ago),
        }
    }

    #[test]
    fn chats_sort_by_last_message_desc_with_created_fallback() {
        let mut chats = vec![
            chat("a", 0, Some(10)),
            chat("b", 5, None), // no messages → keys on created_at (+5min)
            chat("c", 1, Some(30)),
            chat("d", 40, None), // created after every message
        ];
        sort_chats(&mut chats);
        let order: Vec<&str> = chats.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(order, ["d", "c", "a", "b"]);
    }

    #[test]
    fn chat_watch_merges_active_bootstrap_and_changed_rows() {
        let mut state = AppState::new();
        let mut archived = chat("archived", 0, Some(1));
        archived.archived = true;
        state.merge_chat_page(vec![archived]);
        state.apply_chat_watch_frame(ChatWatchFrame::Bootstrap {
            chats: vec![chat("active", 0, Some(2))],
        });
        assert_eq!(state.chats.len(), 2);

        let mut restored = state
            .chats
            .iter()
            .find(|chat| chat.id == "archived")
            .unwrap()
            .clone();
        restored.archived = false;
        state.apply_chat_watch_frame(ChatWatchFrame::Delta {
            upserts: vec![restored],
            removed_ids: vec!["active".into()],
        });
        assert_eq!(state.chats.len(), 1);
        assert!(!state.chats[0].archived);
    }

    #[test]
    fn chat_sort_ties_are_deterministic() {
        let mut chats = vec![chat("z", 0, Some(10)), chat("a", 0, Some(10))];
        sort_chats(&mut chats);
        assert_eq!(chats[0].id, "a");
    }

    #[test]
    fn working_indicator_staleness() {
        let now = Utc::now();
        // Fresh working session shows.
        let fresh = session("c", SessionStatus::Working, 10, now);
        assert_eq!(effective_indicator(Some(&fresh), now), Indicator::Working);
        // Stale working session is suppressed — crashed backend, not eternal spinner.
        let stale = session("c", SessionStatus::Working, 46, now);
        assert_eq!(effective_indicator(Some(&stale), now), Indicator::None);
        // Exactly at the boundary still shows (strictly-older-than semantics).
        let edge = session("c", SessionStatus::Working, 45, now);
        assert_eq!(effective_indicator(Some(&edge), now), Indicator::Working);
        // Future timestamps (clock skew) count as fresh.
        let skewed = session("c", SessionStatus::Working, -30, now);
        assert_eq!(effective_indicator(Some(&skewed), now), Indicator::Working);
    }

    #[test]
    fn indicator_kinds() {
        let now = Utc::now();
        assert_eq!(effective_indicator(None, now), Indicator::None);
        let idle = session("c", SessionStatus::Idle, 0, now);
        assert_eq!(effective_indicator(Some(&idle), now), Indicator::None);
        // Errored is not staleness-gated: the error stays visible.
        let errored = session("c", SessionStatus::Errored, 600, now);
        assert_eq!(effective_indicator(Some(&errored), now), Indicator::Errored);
        let awaiting = session("c", SessionStatus::AwaitingInput, 5, now);
        assert_eq!(
            effective_indicator(Some(&awaiting), now),
            Indicator::AwaitingInput
        );
        let awaiting_stale = session("c", SessionStatus::AwaitingInput, 300, now);
        assert_eq!(
            effective_indicator(Some(&awaiting_stale), now),
            Indicator::None
        );
    }

    #[test]
    fn display_status_derivation() {
        let now = Utc::now();
        let mut c = chat("c", 0, Some(10));
        // Live states win regardless of seen.
        let working = session("c", SessionStatus::Working, 5, now);
        assert_eq!(
            display_status(&c, Some(&working), now),
            ChatIndicator::Working
        );
        let awaiting = session("c", SessionStatus::AwaitingInput, 5, now);
        assert_eq!(
            display_status(&c, Some(&awaiting), now),
            ChatIndicator::AwaitingInput
        );
        // Finished + unseen = Completed (no session row at all).
        assert_eq!(display_status(&c, None, now), ChatIndicator::Completed);
        // Idle session + unseen = Completed.
        let idle = session("c", SessionStatus::Idle, 5, now);
        assert_eq!(
            display_status(&c, Some(&idle), now),
            ChatIndicator::Completed
        );
        // Stale working session falls back to the seen check.
        let stale = session("c", SessionStatus::Working, 300, now);
        assert_eq!(
            display_status(&c, Some(&stale), now),
            ChatIndicator::Completed
        );
        // Seen after the last message = Idle.
        c.last_seen_at = c.last_message_at.map(|t| t + TimeDelta::minutes(1));
        assert_eq!(display_status(&c, Some(&idle), now), ChatIndicator::Idle);
        // Errored + unseen = Errored; seen clears it to Idle.
        let errored = session("c", SessionStatus::Errored, 600, now);
        assert_eq!(display_status(&c, Some(&errored), now), ChatIndicator::Idle);
        c.last_seen_at = None;
        assert_eq!(
            display_status(&c, Some(&errored), now),
            ChatIndicator::Errored
        );
        // No messages at all: nothing to see — Idle.
        let fresh = chat("f", 0, None);
        assert_eq!(display_status(&fresh, None, now), ChatIndicator::Idle);
    }

    #[test]
    fn active_list_sorts_pins_first_then_recency_status_never_moves_rows() {
        let a = chat("a", 0, Some(10)); // Completed (older)
        let b = chat("b", 0, Some(20)); // Completed (newer)
        let mut c = chat("c", 0, Some(5)); // AwaitingInput, pinned
        c.pinned = true;
        let mut d = chat("d", 0, Some(1)); // Working, pinned
        d.pinned = true;
        let mut rows = vec![
            (ChatIndicator::Completed, &a),
            (ChatIndicator::Completed, &b),
            (ChatIndicator::AwaitingInput, &c),
            (ChatIndicator::Working, &d),
        ];
        sort_active(&mut rows);
        let order: Vec<&str> = rows.iter().map(|(_, c)| c.id.as_str()).collect();
        assert_eq!(
            order,
            ["c", "d", "b", "a"],
            "pins first, then recency desc; status ignored"
        );

        // Opening a completed session (completed → seen → idle) must NOT
        // change its position (user report: rows jumped under the pointer).
        let mut seen = vec![
            (ChatIndicator::Idle, &a),
            (ChatIndicator::Completed, &b),
            (ChatIndicator::AwaitingInput, &c),
            (ChatIndicator::Working, &d),
        ];
        sort_active(&mut seen);
        let order_after: Vec<&str> = seen.iter().map(|(_, c)| c.id.as_str()).collect();
        assert_eq!(order, order_after);
    }

    #[test]
    fn apply_spaces_sorts_and_heals_selection() {
        let mut state = AppState::new();
        state.apply_spaces(vec![
            space("s2", "dev", "/b", 2),
            space("s1", "dev", "/a", 1),
        ]);
        let ids: Vec<&str> = state.spaces.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, ["s1", "s2"]);
        // First frame auto-selects the first space.
        assert_eq!(state.selected_space.as_deref(), Some("s1"));
        state.selected_space = Some("s2".into());
        // Vanished selection heals to the first space.
        state.apply_spaces(vec![space("s1", "dev", "/a", 1)]);
        assert_eq!(state.selected_space.as_deref(), Some("s1"));
        // No spaces at all: selection clears.
        state.apply_spaces(vec![]);
        assert_eq!(state.selected_space, None);
    }

    #[test]
    fn local_space_device_tag_describes_the_scope() {
        let now = Utc::now();
        let mut state = AppState::new();
        state.scope = Some(ScopeStatus::local());

        assert_eq!(state.device_display_name("dev"), Some("local"));
        assert_eq!(
            state.space_device_tag(&space("s1", "dev", "/a", 1), now),
            ("@ local".to_string(), false)
        );
    }

    #[test]
    fn chats_in_space_filters_visible_sessions() {
        let mut state = AppState::new();
        state.apply_spaces(vec![space("s1", "dev", "/a", 1)]);
        let mut in_space_new = chat("new", 5, None);
        in_space_new.space_id = Some("s1".into());
        let mut in_space_old = chat("old", 1, Some(50)); // active but created first
        in_space_old.space_id = Some("s1".into());
        let mut other = chat("other", 2, None);
        other.space_id = Some("s2".into());
        let mut archived = chat("gone", 0, None);
        archived.space_id = Some("s1".into());
        archived.archived = true;
        let dangling = chat("dangling", 3, None); // no space id
        state.apply_chats(vec![in_space_new, in_space_old, other, archived, dangling]);
        let ids: Vec<&str> = state
            .chats_in_space("s1")
            .iter()
            .map(|c| c.id.as_str())
            .collect();
        assert_eq!(ids, ["old", "new"]);
        // The overview shows every live-space chat (idle included) — chats of
        // unknown spaces stay hidden. Completed ("old") outranks idle ("new").
        let now = Utc::now();
        let overview: Vec<&str> = state
            .overview_chats(now)
            .iter()
            .map(|(_, c)| c.id.as_str())
            .collect();
        assert_eq!(overview, ["old", "new"]);
    }

    #[test]
    fn apply_chats_drops_vanished_selection() {
        let mut state = AppState::new();
        state.apply_chats(vec![chat("a", 0, None), chat("b", 1, None)]);
        state.selected_chat = Some("a".into());
        state.transcript = vec![];
        state.apply_chats(vec![chat("b", 1, None)]);
        assert_eq!(state.selected_chat, None);
        // Still-present selection survives.
        state.selected_chat = Some("b".into());
        state.apply_chats(vec![chat("b", 1, None), chat("c", 2, None)]);
        assert_eq!(state.selected_chat.as_deref(), Some("b"));
    }

    #[test]
    fn remote_jolt_update_clears_after_the_target_version_reconnects() {
        let mut state = AppState::new();
        state.remote_update_actions.insert(
            "device-2".into(),
            RemoteJoltUpdateAction::Verifying {
                target_version: "0.2.0".into(),
            },
        );
        state.apply_remote_update(
            "device-2".into(),
            jolt_update::UpdateStatus {
                current_version: "0.2.0".into(),
                latest_version: Some("0.2.0".into()),
                update_available: false,
                can_apply: true,
                checked_at: Some(1),
                error: None,
            },
        );

        assert!(!state.remote_update_actions.contains_key("device-2"));
        assert_eq!(state.remote_updates["device-2"].current_version, "0.2.0");
    }

    #[test]
    fn apply_chat_config_stamps_the_row() {
        let mut state = AppState::new();
        state.apply_chats(vec![chat("a", 0, None), chat("b", 1, None)]);
        let config = jolt_proto::ChatConfig {
            harness: HarnessId::ClaudeCode,
            model: Some("claude-fable-5".into()),
            reasoning: Some(jolt_proto::ReasoningLevel::XHigh),
            model_options: serde_json::Map::new(),
            sandbox: jolt_proto::SandboxLevel::WorkspaceWrite,
        };
        state.apply_chat_config("a", config.clone());
        assert_eq!(
            state.chats.iter().find(|c| c.id == "a").unwrap().config,
            Some(config)
        );
        assert!(
            state
                .chats
                .iter()
                .find(|c| c.id == "b")
                .unwrap()
                .config
                .is_none()
        );
        // Unknown chat: no-op, no panic.
        state.apply_chat_config(
            "missing",
            jolt_proto::ChatConfig {
                harness: HarnessId::ClaudeCode,
                model: None,
                reasoning: None,
                model_options: serde_json::Map::new(),
                sandbox: jolt_proto::SandboxLevel::WorkspaceWrite,
            },
        );
    }

    #[test]
    fn visible_chats_filters_archived() {
        let mut state = AppState::new();
        let mut archived = chat("a", 0, Some(99));
        archived.archived = true;
        state.apply_chats(vec![archived, chat("b", 1, None)]);
        let visible: Vec<&str> = state.visible_chats().map(|c| c.id.as_str()).collect();
        assert_eq!(visible, ["b"]);
    }

    #[test]
    fn pending_send_overlays_working_until_ttl() {
        let now = Utc::now();
        let unseen = chat("c", 0, Some(10));
        let mut state = AppState::new();

        assert_eq!(
            state.display_status_for(&unseen, now),
            ChatIndicator::Completed
        );
        state.begin_pending_send("c", "m1", now);
        assert_eq!(
            state.display_status_for(&unseen, now),
            ChatIndicator::Working
        );
        assert_eq!(state.indicator_for("c", now), Indicator::Working);

        let expired = now + TimeDelta::milliseconds(PENDING_SEND_TTL_MS + 1);
        assert_eq!(
            state.display_status_for(&unseen, expired),
            ChatIndicator::Completed
        );
        assert_eq!(state.indicator_for("c", expired), Indicator::None);
    }

    #[test]
    fn pending_send_to_offline_host_stays_queued_instead_of_working() {
        let now = Utc::now();
        let unseen = chat("c", 0, Some(10));
        let mut state = AppState::new();
        state.apply_chats(vec![unseen.clone()]);
        state.devices = vec![Device {
            id: "dev".into(),
            name: "MacBook".into(),
            platform: "macos".into(),
            last_seen_at: Some(now - TimeDelta::minutes(2)),
            created_at: None,
            version: None,
        }];
        state.begin_pending_send("c", "m1", now);

        assert_eq!(
            state.queued_send_offline_host_name("c", now),
            Some("MacBook")
        );
        assert_eq!(
            state.display_status_for(&unseen, now),
            ChatIndicator::Completed
        );
        assert_eq!(state.indicator_for("c", now), Indicator::None);
        let expired = now + TimeDelta::milliseconds(PENDING_SEND_TTL_MS + 1);
        assert_eq!(
            state.queued_send_offline_host_name("c", expired),
            Some("MacBook")
        );
        assert_eq!(state.indicator_for("c", expired), Indicator::None);

        state.devices[0].last_seen_at = Some(now);
        assert_eq!(state.queued_send_offline_host_name("c", now), None);
        assert_eq!(
            state.display_status_for(&unseen, now),
            ChatIndicator::Working
        );
        assert_eq!(state.indicator_for("c", now), Indicator::Working);
    }

    #[test]
    fn pending_send_clears_on_matching_ack_or_cleanup() {
        let now = Utc::now();
        let mut state = AppState::new();
        state.selected_chat = Some("c".into());
        state.begin_pending_send("c", "m1", now);
        state.apply_transcript(vec![user_entry("other")]);
        assert!(state.send_pending("c", now));

        state.apply_transcript(vec![user_entry("other"), user_entry("m1")]);
        assert!(!state.send_pending("c", now));

        state.begin_pending_send("c", "m2", now);
        state.begin_pending_send("c", "m3", now);
        state.end_pending_send("c", "m2");
        assert!(state.send_pending("c", now));
        state.end_pending_send("c", "m3");
        assert!(!state.send_pending("c", now));
    }

    #[test]
    fn echoes_show_until_doc_frame_confirms() {
        let mut state = AppState::new();
        state.selected_chat = Some("c1".into());
        let echo = SessionMessageEntry {
            id: "m1".into(),
            role: jolt_session_doc::MessageRole::User,
            parts: vec![],
            created_at: 0,
            device_id: "local".into(),
            status: None,
            continuation_of: None,
        };
        state.push_echo("c1", echo.clone());
        // Duplicate pushes dedupe.
        state.push_echo("c1", echo.clone());
        assert_eq!(state.pending_echoes().len(), 1);
        // Frames without the id keep the echo.
        state.apply_transcript(vec![]);
        assert_eq!(state.pending_echoes().len(), 1);
        // The confirming frame prunes it.
        state.apply_transcript(vec![SessionMessageEntry {
            id: "m1".into(),
            ..echo.clone()
        }]);
        assert!(state.pending_echoes().is_empty());
        // Failure path: explicit removal.
        state.push_echo(
            "c1",
            SessionMessageEntry {
                id: "m2".into(),
                ..echo.clone()
            },
        );
        state.remove_echo("c1", "m2");
        assert!(state.pending_echoes().is_empty());
        // Echoes are per chat.
        state.push_echo(
            "other",
            SessionMessageEntry {
                id: "m3".into(),
                ..echo
            },
        );
        assert!(state.pending_echoes().is_empty());
    }

    #[test]
    fn gate_phases() {
        let user = UserProfile {
            id: "u".into(),
            email: "w@example.com".into(),
            name: None,
        };
        assert_eq!(
            gate_phase(&ConnectionStatus::Connecting, None),
            GatePhase::Loading
        );
        assert_eq!(
            gate_phase(&ConnectionStatus::Failed("boom".into()), None),
            GatePhase::Failed("boom".into())
        );
        // Unknown auth (pre-M4) gates nothing.
        assert_eq!(gate_phase(&ConnectionStatus::Ready, None), GatePhase::Ready);
        assert_eq!(
            gate_phase(&ConnectionStatus::Ready, Some(&AuthState::SignedOut)),
            GatePhase::Ready
        );
        assert_eq!(
            gate_phase(
                &ConnectionStatus::Ready,
                Some(&AuthState::SignedIn {
                    user: user.clone(),
                    org_id: None
                })
            ),
            GatePhase::Ready
        );
        // No hidden org yet → automatic account setup.
        assert_eq!(
            gate_phase(
                &ConnectionStatus::Ready,
                Some(&AuthState::NeedsOrganization { user })
            ),
            GatePhase::OrgGate
        );
    }

    #[test]
    fn auth_frames_parse_both_wire_shapes() {
        // Proto shape.
        let proto = serde_json::json!({ "state": "signedOut" });
        assert_eq!(parse_auth_state(&proto), Some(AuthState::SignedOut));
        // Engine shape (`_tag`, PascalCase, orgId).
        let engine = serde_json::json!({
            "_tag": "SignedIn",
            "user": { "id": "u1", "email": "w@example.com" },
            "orgId": "org-1",
        });
        let Some(AuthState::SignedIn { user, org_id }) = parse_auth_state(&engine) else {
            panic!("expected SignedIn");
        };
        assert_eq!(user.email, "w@example.com");
        assert_eq!(org_id.as_deref(), Some("org-1"));
        let needs = serde_json::json!({
            "_tag": "NeedsOrganization",
            "user": { "id": "u1", "email": "w@example.com", "name": "W" },
        });
        assert!(matches!(
            parse_auth_state(&needs),
            Some(AuthState::NeedsOrganization { .. })
        ));
        // Garbage → None (frame dropped, not a crash).
        assert_eq!(
            parse_auth_state(&serde_json::json!({ "_tag": "Wat" })),
            None
        );
        assert_eq!(parse_auth_state(&serde_json::json!(42)), None);
    }

    fn chat_with_cwd(id: &str, created_min: i64, cwd: Option<&str>) -> Chat {
        let mut c = chat(id, created_min, None);
        c.cwd = cwd.map(str::to_string);
        c
    }

    #[test]
    fn project_labels_from_cwd() {
        assert_eq!(project_label(Some("/home/w/dev/jolt")), "jolt");
        assert_eq!(project_label(Some("/home/w/dev/jolt/")), "jolt");
        assert_eq!(project_label(None), "No project");
        assert_eq!(project_label(Some("   ")), "No project");
        assert_eq!(project_label(Some("/")), "/");
    }

    #[test]
    fn grouped_sidebar_preserves_recency_order() {
        // Input is sidebar-sorted (most recent first).
        let chats = [
            chat_with_cwd("a", 9, Some("/dev/jolt")),
            chat_with_cwd("b", 8, Some("/dev/zed")),
            chat_with_cwd("c", 7, Some("/dev/jolt")),
            chat_with_cwd("d", 6, None),
        ];
        let groups = group_chats(chats.iter());
        let labels: Vec<&str> = groups.iter().map(|g| g.label.as_str()).collect();
        // Groups ordered by their most recent chat; rows keep order.
        assert_eq!(labels, ["jolt", "zed", "No project"]);
        let jolt_ids: Vec<&str> = groups[0].chats.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(jolt_ids, ["a", "c"]);
        assert!(group_chats(std::iter::empty()).is_empty());
    }

    #[test]
    fn relative_times_match_jolt_format() {
        let now = Utc::now();
        let ago = |secs: i64| now - chrono::Duration::seconds(secs);
        assert_eq!(format_time_ago(ago(0), now), "now");
        assert_eq!(format_time_ago(ago(59), now), "now");
        assert_eq!(format_time_ago(ago(60), now), "1m");
        assert_eq!(format_time_ago(ago(59 * 60), now), "59m");
        assert_eq!(format_time_ago(ago(60 * 60), now), "1h");
        assert_eq!(format_time_ago(ago(23 * 3600 + 3599), now), "23h");
        assert_eq!(format_time_ago(ago(24 * 3600), now), "1d");
        assert_eq!(format_time_ago(ago(6 * 86400), now), "6d");
        assert_eq!(format_time_ago(ago(7 * 86400), now), "1w");
        assert_eq!(format_time_ago(ago(30 * 86400), now), "4w");
        assert_eq!(format_time_ago(ago(35 * 86400), now), "1mo");
        assert_eq!(format_time_ago(ago(400 * 86400), now), "1y");
        // Clock skew (future timestamps) clamps to "now".
        assert_eq!(
            format_time_ago(now + chrono::Duration::hours(2), now),
            "now"
        );
    }

    #[test]
    fn chat_location_joins_project_and_branch() {
        let mut c = chat_with_cwd("x", 1, Some("/home/w/dev/soccertcg"));
        c.branch = Some("jolt/rebalance".into());
        assert_eq!(
            chat_location(&c).as_deref(),
            Some("soccertcg · jolt/rebalance")
        );
        c.branch = None;
        assert_eq!(chat_location(&c).as_deref(), Some("soccertcg"));
        c.cwd = None;
        c.branch = Some("main".into());
        assert_eq!(chat_location(&c).as_deref(), Some("main"));
        c.branch = Some("   ".into());
        assert_eq!(chat_location(&c), None);
        c.branch = None;
        assert_eq!(chat_location(&c), None);
    }
}
