//! WorkspaceHost — owns the per-user workspace **registry** (docs/
//! registry-sync.md; replaces the Loro workspace doc after the 2026-07/08
//! wedge incidents): local snapshot persistence, edge room sync
//! (`/registry/{orgId}/ws` → room `reg1/{orgId}/{userId}`, offline-tolerant —
//! spaces/sessions are private to their owner, never org-visible), the device
//! registry row for THIS device, and the typed watch channels the
//! WatchChats/WatchDevices/WatchSessions RPC streams are fed from.
//!
//! Writer discipline (kept from the doc schema): this host writes its own device row,
//! its own session-status rows, and rows for chats it hosts; renames/archives are LWW
//! sets accepted from any device (the Mutate surface).
//!
//! Liveness: `lastSeenAt` is a row write on boot/shutdown ONLY — the periodic 15s
//! heartbeat rides the room's presence frames (memory-only on the DO), so staying
//! online never grows server state.
//!
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};

use chrono::{DateTime, Utc};
use tokio::sync::watch;

use jolt_proto::{
    Chat, ChatConfig, Device, HarnessConversationRef, HarnessId, Session, Space, ThemeFileRecord,
};
use jolt_registry_model::{DeletedDevice, DeletedSpace, REGISTRY_DOC_ID, RegistryDoc};
use jolt_store::DocsStore;
use jolt_sync::{RegistryClient, RegistryTuning};

use crate::doc_host::EdgeConfig;
use crate::{EngineError, now_ms};

/// Org used when none is configured (matches the edge's dev-mode `user@org` bearers).
pub const DEFAULT_ORG_ID: &str = "dev-org";
/// User used when none is configured (dev mode without a bearer).
pub const DEFAULT_USER_ID: &str = "dev-user";
/// Presence beat cadence.
const PRESENCE_INTERVAL_MS: u64 = 15_000;
const MAX_THEME_FILES: usize = 256;
const MAX_THEME_FILE_BYTES: usize = 256 * 1024;
/// A presence heartbeat younger than this marks the device alive (3 missed
/// beats = offline). Also the "peer is reachable" signal that clears the
/// peer-dial cooldown.
const PRESENCE_FRESH_MS: i64 = 45_000;
/// Relay-status probe cadence. Presence heartbeats ride the registry room, so
/// any registry pathology (or our own room connection being down) silently
/// starves them — and every device looks offline while its relay works fine.
/// Before believing "offline", ask the device's DeviceRoom
/// (`GET /device/{id}/status` → `hostConnected`), which tracks the host socket
/// authoritatively and shares no machinery with the registry room. Probes only
/// run for devices whose heartbeat is stale, so the steady state (healthy
/// room, fresh beats) sends no extra traffic.
const RELAY_PROBE_INTERVAL_MS: u64 = 30_000;
/// Per-request timeout for a relay-status probe.
const RELAY_PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
/// Debounce window for local snapshot saves after a change.
const SNAPSHOT_DEBOUNCE_MS: u64 = 1_000;
/// Active-list retention: inactive threads move to Closed after three days.
const AUTO_ARCHIVE_AFTER_MS: i64 = 3 * 24 * 60 * 60 * 1_000;
/// Minute precision keeps the transition timely without waking for every row.
const AUTO_ARCHIVE_SWEEP_INTERVAL_MS: u64 = 60_000;
/// Initial-join retry backoff (base, cap). A first registry-room join that
/// fails must not strand the device offline until an app restart — retry until
/// it lands. Jittered so N devices restarting together don't resynchronize
/// their retries into a thundering herd on the cold DO.
pub(crate) const JOIN_RETRY_BASE: std::time::Duration = std::time::Duration::from_millis(500);
pub(crate) const JOIN_RETRY_CAP: std::time::Duration = std::time::Duration::from_secs(30);
/// Quiet-probe cadence for the registry room: fixed at 15 minutes. One room
/// per engine, so the fixed cadence costs ~100 DO wakes/day total, and the
/// probe is deadline-checked — a mute room is detected within
/// probe cadence + 10s instead of hours (2026-08-04 deaf-socket lesson).
const REGISTRY_PROBE_QUIET: std::time::Duration = std::time::Duration::from_secs(900);
/// Deaf-socket escalation: live peer presence dark this long after the
/// tripwire probe → redial on a fresh socket (see `check_presence_deafness`).
const PRESENCE_DEAF_REDIAL_MS: i64 = 60_000;

/// State for the presence deafness tripwire. Presence heartbeats ride the
/// SAME socket and the same DO broadcast fan-out as row updates, so "peers I
/// was seeing live via this room all went dark at once" is a *delivery*
/// signal, and it is bounded (~45-60s) at zero server cost — every device
/// already heartbeats each 15s. The monotonic seen-cache and the relay
/// status probe deliberately keep devices *fresh-looking* through other
/// paths; they must never feed this tripwire (they'd mask exactly the
/// failure it exists to catch — 2026-08-04 deaf-socket incident).
#[derive(Default)]
struct PresenceWatch {
    /// Armed once at least one OTHER device has been seen live via the
    /// room's presence map this session.
    armed: bool,
    /// Epoch ms when live peers first all went dark (0 = not dark).
    dark_since_ms: i64,
    /// The cheap first response (probe) already fired.
    probed: bool,
}

/// Cheap decorrelation jitter (0–500ms) without pulling in a rng — derived from
/// the sub-nanosecond wall clock. Mirrors the device relay's `jitter()`.
pub(crate) fn join_retry_jitter() -> std::time::Duration {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    std::time::Duration::from_millis(u64::from(nanos) % 500)
}

#[derive(Debug, Clone)]
pub struct WorkspaceHostConfig {
    pub device_id: String,
    /// Human name for this device's registry row (hostname by default).
    pub device_name: String,
    /// `std::env::consts::OS`-style platform string.
    pub platform: String,
    pub org_id: String,
    /// The signed-in user — registries are per-user (`reg1/{orgId}/{userId}`):
    /// spaces/sessions are private to their owner, never org-visible.
    pub user_id: String,
    /// When present, the host joins `/registry/{orgId}/ws`. `None` = fully offline
    /// (local snapshots only; the registry still drives everything device-side).
    pub edge: Option<EdgeConfig>,
}

struct WorkspaceHostInner {
    store: Arc<DocsStore>,
    config: WorkspaceHostConfig,
    reg: Arc<Mutex<RegistryDoc>>,
    chats_tx: watch::Sender<Vec<Chat>>,
    devices_tx: watch::Sender<Vec<Device>>,
    sessions_tx: watch::Sender<Vec<Session>>,
    spaces_tx: watch::Sender<Vec<Space>>,
    themes_tx: watch::Sender<Vec<ThemeFileRecord>>,
    room: Mutex<Option<Arc<RegistryClient>>>,
    /// Bumped on every registry change (local mutation or applied server
    /// frame) — drives republish + the snapshot debounce in `workspace_task`.
    changed_tx: watch::Sender<u64>,
    /// Freshest presence heartbeat (ms) we have EVER observed per device. The
    /// room's presence map forgets entries after its 30s TTL and starts empty
    /// on a (re)join, so without this cache a receive-side hiccup snaps a
    /// device's overlay back to its boot-time row `lastSeenAt` — an instant
    /// (and false) "offline" badge for a host that beat 20s ago.
    presence_seen: Mutex<std::collections::HashMap<String, i64>>,
    /// Called with a device id whenever its presence heartbeat proves it alive —
    /// wired to `LinkCache::reset_cooldown` so a peer that comes back is dialed
    /// immediately instead of waiting out the failure backoff.
    peer_alive: Mutex<Option<PeerAliveHook>>,
    /// Deaf-socket tripwire state — see `check_presence_deafness`.
    presence_watch: Mutex<PresenceWatch>,
}

/// "This peer is alive" callback (device id) — see `WorkspaceHost::set_peer_alive_hook`.
pub type PeerAliveHook = Arc<dyn Fn(&str) + Send + Sync>;

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[derive(Clone)]
pub struct WorkspaceHost {
    inner: Arc<WorkspaceHostInner>,
}

impl WorkspaceHost {
    /// Load or initialize the registry, upsert this device's row, start
    /// the change-driven task, and join the edge registry room when configured.
    pub fn open(store: Arc<DocsStore>, config: WorkspaceHostConfig) -> Result<Self, EngineError> {
        let mut doc = match store.load_snapshot(REGISTRY_DOC_ID)? {
            Some(bytes) => RegistryDoc::from_bytes(&bytes, &config.device_id)
                .map_err(|e| EngineError::Other(format!("registry snapshot load failed: {e}")))?,
            None => RegistryDoc::new(&config.device_id),
        };

        // Boot: upsert our own device row. A user-set name (RenameDevice is LWW from
        // any device) survives restarts — only a missing row gets the hostname.
        let now = Utc::now();
        let existing = doc
            .read_devices()?
            .into_iter()
            .find(|d| d.id == config.device_id);
        doc.upsert_device(&Device {
            id: config.device_id.clone(),
            name: existing
                .as_ref()
                .map(|d| d.name.clone())
                .filter(|n| !n.is_empty())
                .unwrap_or_else(|| config.device_name.clone()),
            platform: config.platform.clone(),
            last_seen_at: Some(now),
            // First registration stamps `createdAt`; restarts keep the original
            // (the Devices page "Added …" fragment).
            created_at: existing.and_then(|d| d.created_at).or(Some(now)),
            // Every boot restamps the running binary's version (fleet staleness
            // on the Devices page; workspace version — same for every crate).
            version: Some(env!("CARGO_PKG_VERSION").to_string()),
        })?;

        let auto_archived = archive_inactive_chats(&mut doc, &config.device_id, now)?;
        if !auto_archived.is_empty() {
            tracing::info!(
                count = auto_archived.len(),
                "archived inactive threads on startup"
            );
        }

        let state = doc.read_all()?;
        let (chats_tx, _) = watch::channel(state.chats);
        let (devices_tx, _) = watch::channel(state.devices);
        let (sessions_tx, _) = watch::channel(state.sessions);
        let (spaces_tx, _) = watch::channel(state.spaces);
        let (themes_tx, _) = watch::channel(doc.read_themes()?);
        let (changed_tx, changed_rx) = watch::channel(0u64);

        let host = Self {
            inner: Arc::new(WorkspaceHostInner {
                store,
                config,
                reg: Arc::new(Mutex::new(doc)),
                chats_tx,
                devices_tx,
                sessions_tx,
                spaces_tx,
                themes_tx,
                room: Mutex::new(None),
                changed_tx,
                presence_seen: Mutex::new(std::collections::HashMap::new()),
                peer_alive: Mutex::new(None),
                presence_watch: Mutex::new(PresenceWatch::default()),
            }),
        };
        // Persist immediately so a newly initialized registry survives a process
        // exit before the first debounced save.
        host.inner.save_snapshot();
        host.join_room();
        tokio::spawn(workspace_task(Arc::downgrade(&host.inner), changed_rx));
        if host.inner.config.edge.is_some() {
            tokio::spawn(relay_probe_task(Arc::downgrade(&host.inner)));
        }
        Ok(host)
    }

    /// Edge room join — offline-tolerant: a failed join logs and stays local-first.
    fn join_room(&self) {
        let Some(edge) = &self.inner.config.edge else {
            return;
        };
        let org_id = self.inner.config.org_id.clone();
        // Per-dial URL provider: the bearer is re-read on every (re)connect.
        let url = edge.room_url(format!("/registry/{org_id}/ws"));
        self.spawn_join(url);
    }

    /// Test seam: join a registry room at a fixed WebSocket URL without an
    /// `EdgeConfig` — integration tests wire hosts to an in-process mock
    /// server through this. Production always goes through [`Self::join_room`].
    #[doc(hidden)]
    pub fn connect_registry_url(&self, url: &str) {
        self.spawn_join(Arc::new(jolt_sync::StaticUrl(url.to_string())));
    }

    fn spawn_join(&self, url: Arc<dyn jolt_sync::UrlProvider>) {
        let org_id = self.inner.config.org_id.clone();
        let reg = self.inner.reg.clone();
        let device_id = self.inner.config.device_id.clone();
        let weak = Arc::downgrade(&self.inner);
        tokio::spawn(async move {
            // `RegistryClient` only self-reconnects AFTER a first successful
            // join; an INITIAL failure (a 500 from an overloaded DO, a token
            // racing a refresh, an edge deploy) must not end this task and
            // leave the device offline until an app restart. Retry the first
            // join on a capped, jittered backoff so a transient blip self-heals.
            let mut backoff = JOIN_RETRY_BASE;
            loop {
                if weak.upgrade().is_none() {
                    return; // host dropped
                }
                let tuning = RegistryTuning {
                    probe_quiet: REGISTRY_PROBE_QUIET,
                };
                match RegistryClient::connect_via_tuned(
                    url.clone(),
                    reg.clone(),
                    &device_id,
                    tuning,
                )
                .await
                {
                    Ok(client) => {
                        let client = Arc::new(client);
                        client.set_presence(now_ms());
                        let mut events = client.events();
                        let Some(inner) = weak.upgrade() else { return };
                        *lock(&inner.room) = Some(client);
                        inner.bump_changed();
                        tracing::info!(org = %org_id, "registry room joined");
                        drop(inner);
                        // The event pump lives for the client's whole life
                        // (across its self-reconnects); it ends only when the
                        // client is dropped at host teardown.
                        loop {
                            match events.recv().await {
                                Ok(jolt_sync::RegistryEvent::Applied)
                                | Ok(jolt_sync::RegistryEvent::Connected) => {
                                    let Some(inner) = weak.upgrade() else { return };
                                    inner.bump_changed();
                                }
                                Ok(jolt_sync::RegistryEvent::Presence) => {
                                    let Some(inner) = weak.upgrade() else { return };
                                    inner.publish();
                                }
                                Ok(jolt_sync::RegistryEvent::Disconnected) => {}
                                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                            }
                        }
                        if let Some(inner) = weak.upgrade() {
                            *lock(&inner.room) = None;
                        }
                        return;
                    }
                    Err(err) => {
                        tracing::warn!(org = %org_id, error = %err, backoff_ms = backoff.as_millis() as u64,
                            "registry room join failed; retrying");
                    }
                }
                tokio::time::sleep(backoff + join_retry_jitter()).await;
                backoff = (backoff * 2).min(JOIN_RETRY_CAP);
            }
        });
    }

    /// Wire the "peer is alive" signal (fresh presence heartbeat) to a callback —
    /// the engine points this at `LinkCache::reset_cooldown`.
    pub fn set_peer_alive_hook(&self, hook: PeerAliveHook) {
        *lock(&self.inner.peer_alive) = Some(hook);
    }

    pub fn device_id(&self) -> &str {
        &self.inner.config.device_id
    }

    pub fn connected(&self) -> bool {
        lock(&self.inner.room)
            .as_ref()
            .is_some_and(|room| room.stats().connected)
    }

    /// Probe the registry room's liveness NOW (window-focus sweep). Probes are
    /// deadline-checked in the client: an unanswered probe tears the session
    /// down for a fresh socket, so a deaf-receiving room (2026-08-04 incident)
    /// heals within seconds of the user looking at the app.
    pub fn probe(&self) {
        if let Some(room) = lock(&self.inner.room).as_ref() {
            room.probe();
        }
    }

    /// Registry room introspection for SyncStatus / `jolt sync`.
    /// `None` = no room yet (edge-less, or the initial join is still retrying).
    pub fn sync_status(&self) -> Option<jolt_sync::RoomStatsSnapshot> {
        lock(&self.inner.room).as_ref().map(|room| room.stats())
    }

    // ── registry access helpers ─────────────────────────────────────────────

    /// Run a mutation under the registry lock, then wake the publish/persist
    /// task and push the write to the room.
    fn mutate<R>(&self, f: impl FnOnce(&mut RegistryDoc) -> R) -> R {
        let result = f(&mut lock(&self.inner.reg));
        self.inner.bump_changed();
        if let Some(room) = lock(&self.inner.room).as_ref() {
            room.nudge();
        }
        result
    }

    fn read<R>(&self, f: impl FnOnce(&RegistryDoc) -> R) -> R {
        f(&lock(&self.inner.reg))
    }

    /// The chat row as currently known (overlay view).
    pub fn chat(&self, chat_id: &str) -> Result<Option<Chat>, EngineError> {
        Ok(self.read(|doc| doc.chat(chat_id))?)
    }

    /// The space row as currently known (overlay view).
    pub fn space(&self, space_id: &str) -> Result<Option<Space>, EngineError> {
        Ok(self.read(|doc| doc.space(space_id))?)
    }

    pub fn read_chats(&self) -> Result<Vec<Chat>, EngineError> {
        Ok(self.read(|doc| doc.read_chats())?)
    }

    pub fn read_devices(&self) -> Result<Vec<Device>, EngineError> {
        Ok(self.read(|doc| doc.read_devices())?)
    }

    pub fn read_sessions(&self) -> Result<Vec<Session>, EngineError> {
        Ok(self.read(|doc| doc.read_sessions())?)
    }

    // ── watches (WatchChats / WatchDevices / merged WatchSessions) ──────────

    pub fn watch_chats(&self) -> watch::Receiver<Vec<Chat>> {
        self.inner.chats_tx.subscribe()
    }

    pub fn watch_devices(&self) -> watch::Receiver<Vec<Device>> {
        self.inner.devices_tx.subscribe()
    }

    /// Raw workspace session-status rows (all devices').
    pub fn watch_session_rows(&self) -> watch::Receiver<Vec<Session>> {
        self.inner.sessions_tx.subscribe()
    }

    pub fn watch_spaces(&self) -> watch::Receiver<Vec<Space>> {
        self.inner.spaces_tx.subscribe()
    }

    pub fn watch_themes(&self) -> watch::Receiver<Vec<ThemeFileRecord>> {
        self.inner.themes_tx.subscribe()
    }

    fn ensure_theme_sync_ready(&self) -> Result<(), EngineError> {
        if self.inner.config.edge.is_some() && !self.connected() {
            return Err(EngineError::Other(
                "account registry is not connected".into(),
            ));
        }
        Ok(())
    }

    pub fn read_themes(&self) -> Result<Vec<ThemeFileRecord>, EngineError> {
        self.ensure_theme_sync_ready()?;
        Ok(self.read(|doc| doc.read_themes())?)
    }

    pub fn upsert_themes(&self, themes: &[ThemeFileRecord]) -> Result<(), EngineError> {
        self.ensure_theme_sync_ready()?;
        if themes.len() > MAX_THEME_FILES
            || themes.iter().any(|theme| {
                uuid::Uuid::parse_str(&theme.id).is_err()
                    || theme.contents.len() > MAX_THEME_FILE_BYTES
                    || (!theme.deleted && theme_revision(&theme.contents) != Some(theme.revision))
                    || (theme.deleted && !theme.contents.is_empty())
            })
        {
            return Err(EngineError::Other(
                "invalid custom theme sync payload".into(),
            ));
        }
        self.mutate(|doc| -> Result<(), EngineError> {
            let existing = doc.read_themes()?;
            let current: std::collections::HashMap<_, _> = existing
                .iter()
                .map(|theme| (theme.id.as_str(), theme.revision))
                .collect();
            let accepted: Vec<_> = themes
                .iter()
                .filter(|theme| {
                    current
                        .get(theme.id.as_str())
                        .is_none_or(|revision| theme.revision > *revision)
                })
                .collect();
            let mut live: std::collections::HashSet<_> = existing
                .iter()
                .filter(|theme| !theme.deleted)
                .map(|theme| theme.id.as_str())
                .collect();
            for theme in &accepted {
                if theme.deleted {
                    live.remove(theme.id.as_str());
                } else {
                    live.insert(theme.id.as_str());
                }
            }
            if live.len() > MAX_THEME_FILES {
                return Err(EngineError::Other("too many custom themes".into()));
            }
            for theme in accepted {
                doc.upsert_theme(theme);
            }
            Ok(())
        })?;
        Ok(())
    }

    pub fn delete_theme(&self, id: &str) -> Result<(), EngineError> {
        self.ensure_theme_sync_ready()?;
        if uuid::Uuid::parse_str(id).is_err() {
            return Err(EngineError::Other("invalid custom theme id".into()));
        }
        self.mutate(|doc| {
            let revision = doc
                .read_themes()
                .unwrap_or_default()
                .into_iter()
                .find(|theme| theme.id == id)
                .map_or(1, |theme| theme.revision.saturating_add(1));
            doc.upsert_theme(&ThemeFileRecord {
                id: id.to_string(),
                revision,
                deleted: true,
                contents: String::new(),
            });
        });
        Ok(())
    }

    /// WatchSessions source: remote devices' rows from the registry merged with
    /// this engine's live status watch (the local view is fresher for our own runs).
    pub fn merged_sessions_watch(
        &self,
        local: watch::Receiver<Vec<Session>>,
    ) -> watch::Receiver<Vec<Session>> {
        let mut rows = self.watch_session_rows();
        let mut local = local;
        let device_id = self.inner.config.device_id.clone();
        let (tx, rx) = watch::channel(merge_sessions(&device_id, &rows.borrow(), &local.borrow()));
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    changed = rows.changed() => if changed.is_err() { break },
                    changed = local.changed() => if changed.is_err() { break },
                }
                let merged = merge_sessions(
                    &device_id,
                    &rows.borrow_and_update(),
                    &local.borrow_and_update(),
                );
                if tx.send(merged).is_err() {
                    break; // no receivers left
                }
            }
        });
        rx
    }

    // ── chat ownership ──────────────────────────────────────────────────────

    /// Writer discipline: the chat's host is its row's `deviceId`. Unknown chats
    /// are claimable — the first run command claims them via [`Self::claim_chat`].
    pub fn is_host(&self, chat_id: &str) -> bool {
        match self.read(|doc| doc.chat(chat_id)) {
            Ok(Some(chat)) => chat.device_id == self.inner.config.device_id,
            Ok(None) => true,
            Err(err) => {
                tracing::warn!(chat = %chat_id, error = %err, "registry chat read failed");
                true
            }
        }
    }

    /// Claim-on-first-command: create the chat row under OUR device id when a run
    /// command arrives for a chat with no row yet. No-op when the row exists.
    ///
    /// The claim writes only identity, cwd, and space. The command plane can
    /// outrun the registry channel, so a full `createChat` with older clocks
    /// must still be able to supply config and title when it arrives.
    ///
    /// Spaces invariant: every chat belongs to a space, so the claim resolves an
    /// own-device space matching `cwd` — or auto-creates one (gitDetected false;
    /// SpacesSync corrects on its next pass). A cwd-less claim (e.g. note_message
    /// racing ahead of the run command) leaves `spaceId` unset; the row is
    /// invisible to the UI until a spaced claim/create lands.
    pub fn claim_chat(&self, chat_id: &str, cwd: Option<&str>) -> Result<(), EngineError> {
        if self.read(|doc| doc.chat(chat_id))?.is_some() {
            return Ok(());
        }
        let space_id = match cwd {
            Some(cwd) => Some(self.space_for_path(cwd)?),
            None => None,
        };
        self.mutate(|doc| doc.claim_chat(chat_id, cwd, space_id.as_deref(), Utc::now()));
        Ok(())
    }

    /// An own-device space whose path matches, else one at the linked
    /// worktree's parent checkout root, else a fresh space at that root.
    fn space_for_path(&self, path: &str) -> Result<String, EngineError> {
        let device_id = &self.inner.config.device_id;
        let spaces = self.read(|doc| doc.read_spaces())?;
        if let Some(space) = spaces
            .iter()
            .find(|s| s.device_id == *device_id && s.path == path)
        {
            return Ok(space.id.clone());
        }
        let root = linked_worktree_root(std::path::Path::new(path));
        if let Some(root) = root.as_deref()
            && let Some(space) = spaces
                .iter()
                .find(|s| s.device_id == *device_id && s.path == root)
        {
            return Ok(space.id.clone());
        }
        let space = Space {
            id: crate::new_id(),
            device_id: device_id.clone(),
            path: root.unwrap_or_else(|| path.to_string()),
            name: None,
            git_detected: false,
            git_checked_at: None,
            checkout_id: None,
            created_at: Utc::now(),
        };
        self.mutate(|doc| doc.upsert_space(&space))?;
        Ok(space.id)
    }

    /// The chat's configured harness/model row, when present (RunRequest harness
    /// selection; callers fall back to the engine default).
    pub fn chat_config(&self, chat_id: &str) -> Option<ChatConfig> {
        match self.read(|doc| doc.chat(chat_id)) {
            Ok(chat) => chat.and_then(|c| c.config),
            Err(err) => {
                tracing::warn!(chat = %chat_id, error = %err, "registry chat read failed");
                None
            }
        }
    }

    // ── host-side row writes ────────────────────────────────────────────────

    /// Sidebar freshness on message persist: preview = first 120 chars of the last
    /// message's text. Claims the row first so a pre-workspace chat gains one.
    pub fn note_message(&self, chat_id: &str, text: &str) {
        let preview: String = text.chars().take(120).collect();
        let result = self.claim_chat(chat_id, None).and_then(|_| {
            self.mutate(|doc| doc.set_chat_last_message(chat_id, &preview, Utc::now()))
                .map_err(EngineError::from)
        });
        if let Err(err) = result {
            tracing::warn!(chat = %chat_id, error = %err, "registry last-message write failed");
        }
    }

    /// The retained native conversation for one harness/cwd on this host.
    pub fn chat_harness_conversation(
        &self,
        chat_id: &str,
        harness: HarnessId,
        cwd: &str,
    ) -> Option<HarnessConversationRef> {
        let device_id = &self.inner.config.device_id;
        match self.read(|doc| doc.chat(chat_id)) {
            Ok(chat) => chat.and_then(|chat| {
                chat.harness_conversations.into_iter().find(|conversation| {
                    conversation.harness == harness
                        && conversation.device_id == *device_id
                        && conversation.cwd == cwd
                })
            }),
            Err(error) => {
                tracing::warn!(chat = %chat_id, %error, "registry harness-conversation read failed");
                None
            }
        }
    }

    /// Persist one native conversation without replacing continuations owned by
    /// other harnesses or working directories.
    pub fn set_chat_harness_conversation(
        &self,
        chat_id: &str,
        conversation: &HarnessConversationRef,
    ) {
        match self.mutate(|doc| doc.set_chat_harness_conversation(chat_id, conversation)) {
            Ok(_) => {}
            Err(error) => {
                tracing::warn!(chat = %chat_id, %error, "registry harness-conversation write failed");
            }
        }
    }

    /// Resume continuity: stamp the chat row with the harness-native session id
    /// of its latest run and the cwd it was created under. An empty `session_id`
    /// tombstones the row ("do not resume" after a rejected resume). Best-effort:
    /// a missing chat row (claim happens on first command) just returns.
    pub fn set_chat_harness_session(&self, chat_id: &str, session_id: &str, cwd: &str) {
        match self.mutate(|doc| doc.set_chat_harness_session(chat_id, session_id, cwd)) {
            Ok(_) => {}
            Err(err) => {
                tracing::warn!(chat = %chat_id, error = %err, "registry harness-session write failed");
            }
        }
    }

    /// The chat row's stored harness session `(session_id, cwd)`, if stamped.
    /// The empty-string tombstone passes through — callers must treat it as
    /// "explicitly no resume" (and must NOT fall back to older sources).
    pub fn chat_harness_session(&self, chat_id: &str) -> Option<(String, Option<String>)> {
        match self.read(|doc| doc.chat(chat_id)) {
            Ok(chat) => {
                let chat = chat?;
                let id = chat.harness_session_id?;
                Some((id, chat.harness_session_cwd))
            }
            Err(err) => {
                tracing::warn!(chat = %chat_id, error = %err, "registry chat read failed");
                None
            }
        }
    }

    pub fn set_chat_goal(
        &self,
        chat_id: &str,
        goal: Option<&jolt_proto::Goal>,
    ) -> Result<bool, EngineError> {
        Ok(self.mutate(|doc| doc.set_chat_goal(chat_id, goal))?)
    }

    pub(crate) fn mutate_chat_goal(
        &self,
        chat_id: &str,
        mutation: impl FnOnce(Option<jolt_proto::Goal>) -> Result<Option<jolt_proto::Goal>, EngineError>,
    ) -> Result<Option<jolt_proto::Goal>, EngineError> {
        self.mutate(|doc| {
            let current = doc.chat(chat_id)?.and_then(|chat| chat.goal);
            let next = mutation(current)?;
            if !doc.set_chat_goal(chat_id, next.as_ref())? {
                return Err(EngineError::Other("session has no chat row".into()));
            }
            Ok(next)
        })
    }

    pub fn chat_goal(&self, chat_id: &str) -> Option<jolt_proto::Goal> {
        self.read(|doc| doc.chat(chat_id))
            .ok()
            .flatten()
            .and_then(|chat| chat.goal)
    }

    /// A process restart must never silently resume an expensive autonomous loop.
    /// Only pause goals hosted by this device; another host may still be running.
    pub fn pause_active_goals_after_restart(&self) -> Result<usize, EngineError> {
        let goals: Vec<_> = self
            .read_chats()?
            .into_iter()
            .filter(|chat| self.is_host(&chat.id))
            .filter_map(|chat| {
                let goal = chat.goal?;
                (goal.status == jolt_proto::GoalStatus::Active).then_some((chat.id, goal))
            })
            .collect();
        let count = goals.len();
        for (chat_id, mut goal) in goals {
            goal.revision = goal.revision.saturating_add(1);
            goal.status = jolt_proto::GoalStatus::Paused;
            goal.pause_source = Some(jolt_proto::GoalPauseSource::System);
            goal.status_message = Some("Paused after Jolt restarted".into());
            goal.updated_at_ms = now_ms();
            self.set_chat_goal(&chat_id, Some(&goal))?;
        }
        Ok(count)
    }

    /// Session-status row upsert (sessions engine transitions land here too, in
    /// addition to the local watch channel).
    pub fn record_session(&self, session: &Session) {
        if let Err(err) = self.mutate(|doc| doc.upsert_session(session)) {
            tracing::warn!(chat = %session.chat_id, error = %err, "registry session write failed");
        }
    }

    // ── Mutate surface (LWW writes accepted from any device) ────────────────

    /// Create a chat *in a space*: the space fixes the host device and base cwd
    /// (`cwd` override = an isolated-worktree path). Fails when the space row is
    /// missing — the UI always creates chats from a picked space.
    pub fn create_chat(
        &self,
        chat_id: &str,
        space_id: &str,
        config: Option<ChatConfig>,
        cwd: Option<String>,
    ) -> Result<(), EngineError> {
        if self.read(|doc| doc.chat(chat_id))?.is_some() {
            return Ok(()); // idempotent: optimistic client retries never duplicate
        }
        let Some(space) = self.read(|doc| doc.space(space_id))? else {
            return Err(EngineError::Other(format!("no such space: {space_id}")));
        };
        self.mutate(|doc| {
            doc.upsert_chat(&Chat {
                id: chat_id.to_string(),
                device_id: space.device_id.clone(),
                title: None,
                archived: false,
                pinned: false,
                cwd: Some(cwd.unwrap_or_else(|| space.path.clone())),
                branch: None,
                checkout_id: None,
                config,
                last_message_preview: None,
                last_message_at: None,
                created_at: Utc::now(),
                harness_session_id: None,
                harness_session_cwd: None,
                harness_conversations: Vec::new(),
                space_id: Some(space.id.clone()),
                last_seen_at: None,
                goal: None,
            })
        })?;
        Ok(())
    }

    // ── spaces (Mutate surface + owner stamps) ──────────────────────────────

    /// Create a space (any device). Idempotent by id; a live duplicate of the
    /// same `(deviceId, path)` is a no-op backstop (the UI reuses via
    /// WatchSpaces). `git_detected` is seeded from the picker's FolderEntry;
    /// the owning device's SpacesSync re-verifies.
    pub fn create_space(
        &self,
        space_id: &str,
        device_id: &str,
        path: &str,
        name: Option<String>,
        git_detected: bool,
    ) -> Result<(), EngineError> {
        let spaces = self.read(|doc| doc.read_spaces())?;
        if spaces
            .iter()
            .any(|s| s.id == space_id || (s.device_id == device_id && s.path == path))
        {
            return Ok(());
        }
        self.mutate(|doc| {
            doc.upsert_space(&Space {
                id: space_id.to_string(),
                device_id: device_id.to_string(),
                path: path.to_string(),
                name,
                git_detected,
                git_checked_at: None,
                checkout_id: None,
                created_at: Utc::now(),
            })
        })?;
        Ok(())
    }

    pub fn rename_space(&self, space_id: &str, name: Option<&str>) -> Result<bool, EngineError> {
        Ok(self.mutate(|doc| doc.rename_space(space_id, name))?)
    }

    /// Hard-delete a space and its chats (registry cascade — one atomic batch).
    /// The caller (rpc layer) tears down live runs / doc-host handles for the
    /// returned chat ids.
    pub fn delete_space(&self, space_id: &str) -> Result<DeletedSpace, EngineError> {
        Ok(self.mutate(|doc| doc.delete_space(space_id))?)
    }

    /// Synced seen marker (any device; LWW + monotonic guard in the doc layer).
    pub fn mark_chat_seen(
        &self,
        chat_id: &str,
        at: chrono::DateTime<Utc>,
    ) -> Result<bool, EngineError> {
        Ok(self.mutate(|doc| doc.set_chat_seen(chat_id, at))?)
    }

    /// Owner-only git stamp (SpacesSync). Refuses rows owned by another device.
    pub fn set_space_git(
        &self,
        space_id: &str,
        detected: bool,
        checkout_id: Option<&str>,
    ) -> Result<bool, EngineError> {
        match self.read(|doc| doc.space(space_id))? {
            Some(space) if space.device_id == self.inner.config.device_id => {
                Ok(self
                    .mutate(|doc| doc.set_space_git(space_id, detected, checkout_id, Utc::now()))?)
            }
            Some(space) => {
                tracing::warn!(
                    space = %space_id, owner = %space.device_id,
                    "refusing git stamp on space owned by another device"
                );
                Ok(false)
            }
            None => Ok(false),
        }
    }

    pub fn read_spaces(&self) -> Result<Vec<Space>, EngineError> {
        Ok(self.read(|doc| doc.read_spaces())?)
    }

    pub fn rename_chat(&self, chat_id: &str, title: &str) -> Result<bool, EngineError> {
        Ok(self.mutate(|doc| doc.rename_chat(chat_id, title))?)
    }

    pub fn set_chat_pinned(&self, chat_id: &str, pinned: bool) -> Result<bool, EngineError> {
        Ok(self.mutate(|doc| doc.set_chat_pinned(chat_id, pinned))?)
    }

    /// Backdate a chat's activity timestamps (epoch ms). Returns false when
    /// the chat doesn't exist.
    pub fn set_chat_activity(
        &self,
        chat_id: &str,
        last_message_at: Option<i64>,
        created_at: Option<i64>,
    ) -> Result<bool, EngineError> {
        let Some(mut chat) = self.read(|doc| doc.chat(chat_id))? else {
            return Ok(false);
        };
        if let Some(ms) = last_message_at {
            chat.last_message_at = chrono::DateTime::<Utc>::from_timestamp_millis(ms);
        }
        if let Some(ms) = created_at
            && let Some(at) = chrono::DateTime::<Utc>::from_timestamp_millis(ms)
        {
            chat.created_at = at;
        }
        self.mutate(|doc| doc.upsert_chat(&chat))?;
        Ok(true)
    }

    /// Host assignment is immutable because cwd, working-copy, and harness
    /// state are machine-local. Permanent host loss must create a recovery fork.
    pub fn set_chat_host(&self, chat_id: &str, device_id: &str) -> Result<bool, EngineError> {
        let Some(chat) = self.read(|doc| doc.chat(chat_id))? else {
            return Ok(false);
        };
        if chat.device_id != device_id {
            return Err(EngineError::Other(
                "chat host assignment is immutable; create a recovery-fork chat".into(),
            ));
        }
        Ok(true)
    }

    pub fn set_chat_archived(&self, chat_id: &str, archived: bool) -> Result<bool, EngineError> {
        Ok(self.mutate(|doc| doc.set_chat_archived(chat_id, archived))?)
    }

    /// LWW full-config replace on the chat row (jolt `SetChatConfig` — the
    /// composer's mid-session model/reasoning/options changes). Returns false
    /// when the chat doesn't exist.
    pub fn set_chat_config(&self, chat_id: &str, config: &ChatConfig) -> Result<bool, EngineError> {
        Ok(self.mutate(|doc| doc.set_chat_config(chat_id, config))?)
    }

    /// Tombstone: removes the chats (and session-status) row; the per-chat session
    /// doc remains untouched.
    pub fn delete_chat(&self, chat_id: &str) -> Result<bool, EngineError> {
        Ok(self.mutate(|doc| doc.delete_chat(chat_id))?)
    }

    pub fn rename_device(&self, device_id: &str, name: &str) -> Result<bool, EngineError> {
        Ok(self.mutate(|doc| doc.rename_device(device_id, name))?)
    }

    pub fn delete_device(&self, device_id: &str) -> Result<DeletedDevice, EngineError> {
        Ok(self.mutate(|doc| doc.delete_device(device_id))?)
    }

    // ── git metadata (diff-sync host writes) ────────────────────────────────

    /// HEAD-watcher reconciliation: the branch checked out at the chat's cwd.
    pub fn set_chat_branch(&self, chat_id: &str, branch: &str) -> Result<bool, EngineError> {
        Ok(self.mutate(|doc| doc.set_chat_branch(chat_id, branch))?)
    }

    /// Retarget a chat onto another folder (mid-session switch to an existing
    /// worktree). Resume is cwd-scoped — the next run there starts fresh.
    pub fn set_chat_cwd(&self, chat_id: &str, cwd: &str) -> Result<bool, EngineError> {
        Ok(self.mutate(|doc| doc.set_chat_cwd(chat_id, cwd))?)
    }

    /// Canonical checkout identity for the chat's cwd (diff grouping key).
    pub fn set_chat_checkout(&self, chat_id: &str, checkout_id: &str) -> Result<bool, EngineError> {
        Ok(self.mutate(|doc| doc.set_chat_checkout(chat_id, checkout_id))?)
    }

    // ── persistence / teardown ──────────────────────────────────────────────

    /// Persist the snapshot now (shutdown path; bypasses the debounce).
    pub fn flush(&self) {
        self.inner.save_snapshot();
    }

    /// Shutdown: stamp our `lastSeenAt` (the only periodic-ish row write besides
    /// boot) and flush the snapshot.
    pub fn shutdown(&self) {
        let now = Utc::now();
        let device_id = self.inner.config.device_id.clone();
        if let Err(err) = self.mutate(|doc| doc.set_device_last_seen(&device_id, now)) {
            tracing::warn!(error = %err, "device lastSeenAt stamp failed");
        }
        self.inner.save_snapshot();
    }
}

impl WorkspaceHostInner {
    fn bump_changed(&self) {
        self.changed_tx.send_modify(|v| *v = v.wrapping_add(1));
    }

    fn archive_inactive_chats(&self, now: DateTime<Utc>) {
        let result = archive_inactive_chats(&mut lock(&self.reg), &self.config.device_id, now);
        match result {
            Ok(chat_ids) if !chat_ids.is_empty() => {
                self.bump_changed();
                if let Some(room) = lock(&self.room).as_ref() {
                    room.nudge();
                }
                tracing::info!(count = chat_ids.len(), chats = ?chat_ids, "archived inactive threads");
            }
            Ok(_) => {}
            Err(err) => tracing::warn!(error = %err, "inactive thread archive sweep failed"),
        }
    }

    fn publish(&self) {
        let state = { lock(&self.reg).read_all() };
        match state {
            Ok(mut state) => {
                self.overlay_presence(&mut state.devices);
                // send_replace, NOT send: `watch::Sender::send` drops the value when
                // no receiver exists yet, so a stream subscribed later would start
                // from a stale snapshot (found the hard way by the e2e smoke).
                self.chats_tx.send_replace(state.chats);
                self.devices_tx.send_replace(state.devices);
                self.sessions_tx.send_replace(state.sessions);
                self.spaces_tx.send_replace(state.spaces);
                match lock(&self.reg).read_themes() {
                    Ok(themes) => {
                        self.themes_tx.send_replace(themes);
                    }
                    Err(err) => tracing::warn!(error = %err, "theme registry read failed"),
                }
            }
            Err(err) => {
                tracing::warn!(error = %err, "registry read failed");
            }
        }
    }

    /// Fold the 15s presence heartbeats into the device rows' `lastSeenAt`
    /// before publishing. The row is written on boot/shutdown ONLY (server-
    /// state hygiene), so without this overlay every device looks offline
    /// ~70s after its boot — and a genuinely dead host is indistinguishable
    /// from slow sync. Fresh remote heartbeats also fire the peer-alive hook
    /// (dial-cooldown reset).
    fn overlay_presence(&self, devices: &mut [Device]) {
        let mut alive_peers: Vec<String> = Vec::new();
        {
            // No live room handle is NOT "everyone is offline": the cache (fed
            // by past heartbeats and the relay-status probe) still overlays —
            // a dead registry room must never fake an offline badge for
            // devices whose relay connection is fine.
            let room = lock(&self.room);
            let live_map = room
                .as_ref()
                .map(|room| room.presence())
                .unwrap_or_default();
            let mut seen = lock(&self.presence_seen);
            let now = now_ms();
            let mut live_fresh_peers = 0usize;
            for device in devices.iter_mut() {
                // Freshest of the live presence entry and the cache: the room
                // map's 30s TTL (and its empty state right after a rejoin)
                // must not erase freshness this engine already witnessed — the
                // device is offline only once heartbeats genuinely stop
                // arriving for the UI's whole online window.
                let live = live_map.get(&device.id).copied();
                if device.id != self.config.device_id
                    && live.is_some_and(|ms| now.saturating_sub(ms) < PRESENCE_FRESH_MS)
                {
                    live_fresh_peers += 1;
                }
                let cached = seen.get(&device.id).copied();
                let Some(ms) = live.into_iter().chain(cached).max() else {
                    continue;
                };
                seen.insert(device.id.clone(), ms);
                if let Some(at) = chrono::DateTime::<Utc>::from_timestamp_millis(ms)
                    && device.last_seen_at.is_none_or(|prev| prev < at)
                {
                    device.last_seen_at = Some(at);
                }
                if device.id != self.config.device_id && now.saturating_sub(ms) < PRESENCE_FRESH_MS
                {
                    alive_peers.push(device.id.clone());
                }
            }
            if let Some(room) = room.as_ref() {
                self.check_presence_deafness(room, live_fresh_peers, now);
            }
        }
        if alive_peers.is_empty() {
            return;
        }
        let hook = lock(&self.peer_alive).clone();
        if let Some(hook) = hook {
            for id in &alive_peers {
                hook(id);
            }
        }
    }

    /// The deaf-socket tripwire (see [`PresenceWatch`]). LIVE presence
    /// freshness only — never the seen-cache or relay probe. Escalation
    /// ladder: first all-dark observation → deadline-checked probe (free on a
    /// healthy room); still dark [`PRESENCE_DEAF_REDIAL_MS`] later → fresh-
    /// socket redial (the only cure when the server→client path drops even
    /// probe answers). Disarms after the redial and re-arms when a peer is
    /// next seen live, so a genuinely-offline fleet costs one probe + one
    /// redial, ever.
    fn check_presence_deafness(&self, room: &RegistryClient, live_fresh_peers: usize, now: i64) {
        let mut watch = lock(&self.presence_watch);
        if live_fresh_peers > 0 {
            watch.armed = true;
            watch.dark_since_ms = 0;
            watch.probed = false;
            return;
        }
        if !watch.armed {
            return;
        }
        if watch.dark_since_ms == 0 {
            watch.dark_since_ms = now;
        }
        if !watch.probed {
            tracing::info!(
                "all live peer presence went dark; probing registry room (deaf-socket tripwire)"
            );
            room.probe();
            watch.probed = true;
        } else if now.saturating_sub(watch.dark_since_ms) > PRESENCE_DEAF_REDIAL_MS {
            tracing::warn!(
                dark_ms = now.saturating_sub(watch.dark_since_ms),
                "peer presence still dark after probe; requesting registry room redial"
            );
            room.redial();
            watch.armed = false;
            watch.dark_since_ms = 0;
            watch.probed = false;
        }
    }

    fn save_snapshot(&self) {
        let bytes = lock(&self.reg).to_bytes();
        match bytes {
            Ok(bytes) => {
                if let Err(err) = self.store.save_snapshot(REGISTRY_DOC_ID, &bytes) {
                    tracing::warn!(error = %err, "registry snapshot save failed");
                }
            }
            Err(err) => {
                tracing::warn!(error = %err, "registry snapshot export failed");
            }
        }
    }

    /// Presence heartbeat — a memory-only frame on the room, never a row write.
    fn presence_tick(&self) {
        if let Some(room) = lock(&self.room).as_ref() {
            room.set_presence(now_ms());
        }
    }
}

/// Return the parent checkout root for a standard linked Git worktree.
/// Linked worktrees contain a `.git` file pointing at
/// `<root>/.git/worktrees/<name>`; primary checkouts and other layouts return
/// `None`. This stays filesystem-only because claims run synchronously.
fn linked_worktree_root(path: &std::path::Path) -> Option<String> {
    let gitfile = path.join(".git");
    if !std::fs::metadata(&gitfile).ok()?.is_file() {
        return None;
    }
    let content = std::fs::read_to_string(&gitfile).ok()?;
    let target = content
        .lines()
        .find_map(|line| line.strip_prefix("gitdir:"))?
        .trim();
    let mut target = std::path::PathBuf::from(target);
    if target.is_relative() {
        target = std::fs::canonicalize(path.join(target)).ok()?;
    }
    let worktrees = target.parent()?;
    let dot_git = worktrees.parent()?;
    if worktrees.file_name()? != "worktrees" || dot_git.file_name()? != ".git" {
        return None;
    }
    Some(dot_git.parent()?.to_string_lossy().into_owned())
}

fn theme_revision(contents: &str) -> Option<u64> {
    serde_json::from_str::<serde_json::Value>(contents)
        .ok()?
        .get("revision")?
        .as_u64()
}

/// Local live statuses win for this device's chats; every other device's rows come
/// from the registry. Sorted by chat id (stable stream output).
fn merge_sessions(device_id: &str, rows: &[Session], local: &[Session]) -> Vec<Session> {
    let mut merged: std::collections::HashMap<String, Session> = rows
        .iter()
        .filter(|s| s.device_id != device_id)
        .map(|s| (s.chat_id.clone(), s.clone()))
        .collect();
    for session in local {
        merged.insert(session.chat_id.clone(), session.clone());
    }
    let mut list: Vec<Session> = merged.into_values().collect();
    list.sort_by(|a, b| a.chat_id.cmp(&b.chat_id));
    list
}

/// Background task: relay-verified presence. Every [`RELAY_PROBE_INTERVAL_MS`],
/// for each known device whose merged heartbeat freshness has gone stale, ask
/// its DeviceRoom whether the host socket is live (`/device/{id}/status`); a
/// positive answer refreshes the presence cache so the overlay keeps the badge
/// online. The DeviceRoom shares no machinery with the registry room, so a
/// false "offline" now requires BOTH independent paths to be down — at which
/// point the device is, for every purpose the app has, genuinely offline.
/// Steady state (healthy room, fresh heartbeats) probes nothing.
async fn relay_probe_task(weak: Weak<WorkspaceHostInner>) {
    let mut tick = tokio::time::interval(std::time::Duration::from_millis(RELAY_PROBE_INTERVAL_MS));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    tick.tick().await; // consume the immediate first tick
    let client = reqwest::Client::new();
    loop {
        tick.tick().await;
        let Some(inner) = weak.upgrade() else { return };
        let Some(edge) = inner.config.edge.clone() else {
            return;
        };
        let self_id = inner.config.device_id.clone();
        let now = now_ms();
        let stale: Vec<String> = {
            let Ok(devices) = lock(&inner.reg).read_devices() else {
                continue;
            };
            let seen = lock(&inner.presence_seen);
            devices
                .into_iter()
                .filter(Device::is_engine_host)
                .filter(|d| d.id != self_id)
                .filter(|d| {
                    seen.get(&d.id)
                        .is_none_or(|ms| now.saturating_sub(*ms) >= PRESENCE_FRESH_MS)
                })
                .map(|d| d.id)
                .collect()
        };
        drop(inner);
        if stale.is_empty() {
            continue;
        }
        let Some(bearer) = edge.bearer().await else {
            continue; // signed out
        };
        let mut refreshed = false;
        for device_id in stale {
            let url = format!(
                "{}/device/{}/status",
                edge.url.trim_end_matches('/'),
                device_id
            );
            let response = client
                .get(&url)
                .bearer_auth(&bearer)
                .timeout(RELAY_PROBE_TIMEOUT)
                .send()
                .await;
            let Ok(response) = response else { continue };
            if !response.status().is_success() {
                continue;
            }
            let Ok(body) = response.json::<serde_json::Value>().await else {
                continue;
            };
            if body
                .get("hostConnected")
                .and_then(serde_json::Value::as_bool)
                == Some(true)
            {
                let Some(inner) = weak.upgrade() else { return };
                lock(&inner.presence_seen).insert(device_id.clone(), now_ms());
                tracing::debug!(device = %device_id, "presence: relay-verified alive");
                refreshed = true;
            }
        }
        if refreshed && let Some(inner) = weak.upgrade() {
            inner.publish();
        }
    }
}

/// Background task: reacts to registry changes (local mutations and applied
/// server frames) by re-publishing the watch channels and debouncing snapshots,
/// and refreshes presence every [`PRESENCE_INTERVAL_MS`]. Holds only a weak
/// handle so a dropped host tears the task down.
async fn workspace_task(weak: Weak<WorkspaceHostInner>, mut changed_rx: watch::Receiver<u64>) {
    let mut presence =
        tokio::time::interval(std::time::Duration::from_millis(PRESENCE_INTERVAL_MS));
    presence.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    presence.tick().await; // consume the immediate first tick
    let mut auto_archive = tokio::time::interval(std::time::Duration::from_millis(
        AUTO_ARCHIVE_SWEEP_INTERVAL_MS,
    ));
    auto_archive.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    auto_archive.tick().await; // startup already swept before the initial publish
    let mut save_deadline: Option<tokio::time::Instant> = None;
    loop {
        let sleep_until = save_deadline.unwrap_or_else(tokio::time::Instant::now);
        tokio::select! {
            changed = changed_rx.changed() => {
                if changed.is_err() {
                    break; // host (and its change sender) is gone
                }
                let Some(inner) = weak.upgrade() else { break };
                inner.publish();
                if save_deadline.is_none() {
                    save_deadline = Some(
                        tokio::time::Instant::now()
                            + std::time::Duration::from_millis(SNAPSHOT_DEBOUNCE_MS),
                    );
                }
            }
            _ = tokio::time::sleep_until(sleep_until), if save_deadline.is_some() => {
                save_deadline = None;
                let Some(inner) = weak.upgrade() else { break };
                inner.save_snapshot();
            }
            _ = presence.tick() => {
                let Some(inner) = weak.upgrade() else { break };
                inner.presence_tick();
                // Re-publish on the same cadence: remote heartbeats decay when a
                // device goes silent, and watchers (the UI online dot, "host
                // offline" hints) need a tick to observe that staleness.
                inner.publish();
            }
            _ = auto_archive.tick() => {
                let Some(inner) = weak.upgrade() else { break };
                inner.archive_inactive_chats(Utc::now());
            }
        }
    }
}

fn should_auto_archive_chat(
    chat: &Chat,
    archive_changed_at: Option<DateTime<Utc>>,
    session: Option<&Session>,
    owner_device_id: &str,
    now: DateTime<Utc>,
) -> bool {
    if chat.archived || chat.device_id != owner_device_id {
        return false;
    }
    if chat
        .goal
        .as_ref()
        .is_some_and(|goal| goal.status == jolt_proto::GoalStatus::Active)
    {
        return false;
    }
    if matches!(
        jolt_proto::view::effective_indicator(session, now),
        jolt_proto::view::Indicator::Working | jolt_proto::view::Indicator::AwaitingInput
    ) {
        return false;
    }

    let activity_at = chat
        .last_message_at
        .into_iter()
        .chain(archive_changed_at)
        .fold(chat.created_at, std::cmp::max);
    now.signed_duration_since(activity_at).num_milliseconds() >= AUTO_ARCHIVE_AFTER_MS
}

fn archive_inactive_chats(
    doc: &mut RegistryDoc,
    owner_device_id: &str,
    now: DateTime<Utc>,
) -> Result<Vec<String>, jolt_registry_model::RegistryError> {
    let sessions = doc.read_sessions()?;
    let sessions_by_chat: std::collections::HashMap<&str, &Session> = sessions
        .iter()
        .map(|session| (session.chat_id.as_str(), session))
        .collect();
    let chat_ids: Vec<String> = doc
        .read_chats()?
        .into_iter()
        .filter(|chat| {
            should_auto_archive_chat(
                chat,
                doc.chat_archived_changed_at(&chat.id),
                sessions_by_chat.get(chat.id.as_str()).copied(),
                owner_device_id,
                now,
            )
        })
        .map(|chat| chat.id)
        .collect();

    for chat_id in &chat_ids {
        doc.set_chat_archived(chat_id, true)?;
    }
    Ok(chat_ids)
}

#[cfg(test)]
mod tests {
    use super::{AUTO_ARCHIVE_AFTER_MS, linked_worktree_root, should_auto_archive_chat};
    use chrono::{DateTime, TimeDelta, Utc};
    use jolt_proto::{Chat, Goal, GoalStatus, Session, SessionStatus};

    fn chat(now: DateTime<Utc>) -> Chat {
        Chat {
            id: "chat-1".into(),
            device_id: "dev-a".into(),
            title: None,
            archived: false,
            pinned: false,
            cwd: None,
            branch: None,
            checkout_id: None,
            config: None,
            last_message_preview: None,
            last_message_at: Some(now - TimeDelta::milliseconds(AUTO_ARCHIVE_AFTER_MS)),
            created_at: now - TimeDelta::days(4),
            harness_session_id: None,
            harness_session_cwd: None,
            harness_conversations: Vec::new(),
            space_id: Some("space-1".into()),
            last_seen_at: None,
            goal: None,
        }
    }

    fn session(now: DateTime<Utc>, updated_at: DateTime<Utc>) -> Session {
        Session {
            chat_id: "chat-1".into(),
            device_id: "dev-a".into(),
            status: SessionStatus::Working,
            compacting: false,
            started_at: Some(now - TimeDelta::minutes(1)),
            updated_at,
        }
    }

    fn active_goal(now: DateTime<Utc>) -> Goal {
        Goal {
            id: "goal-1".into(),
            revision: 1,
            objective: "Ship it".into(),
            status: GoalStatus::Active,
            pause_source: None,
            status_message: None,
            token_budget: None,
            tokens_used: 0,
            elapsed_active_ms: 0,
            turns: 0,
            blocker_key: None,
            blocker_streak: 0,
            created_at_ms: now.timestamp_millis(),
            updated_at_ms: now.timestamp_millis(),
        }
    }

    #[test]
    fn inactive_chat_archives_at_three_days() {
        let now = DateTime::from_timestamp_millis(2_000_000_000_000).unwrap();
        assert!(should_auto_archive_chat(
            &chat(now),
            None,
            None,
            "dev-a",
            now
        ));
    }

    #[test]
    fn recent_reopen_and_live_work_delay_auto_archive() {
        let now = DateTime::from_timestamp_millis(2_000_000_000_000).unwrap();
        let candidate = chat(now);
        assert!(!should_auto_archive_chat(
            &candidate,
            Some(now - TimeDelta::days(1)),
            None,
            "dev-a",
            now,
        ));
        assert!(!should_auto_archive_chat(
            &candidate,
            None,
            Some(&session(now, now)),
            "dev-a",
            now,
        ));
        assert!(should_auto_archive_chat(
            &candidate,
            None,
            Some(&session(now, now - TimeDelta::minutes(1))),
            "dev-a",
            now,
        ));
    }

    #[test]
    fn auto_archive_only_touches_owned_inactive_non_goal_threads() {
        let now = DateTime::from_timestamp_millis(2_000_000_000_000).unwrap();
        let mut candidate = chat(now);
        candidate.device_id = "dev-b".into();
        assert!(!should_auto_archive_chat(
            &candidate, None, None, "dev-a", now
        ));

        candidate.device_id = "dev-a".into();
        candidate.goal = Some(active_goal(now));
        assert!(!should_auto_archive_chat(
            &candidate, None, None, "dev-a", now
        ));

        candidate.goal = None;
        candidate.archived = true;
        assert!(!should_auto_archive_chat(
            &candidate, None, None, "dev-a", now
        ));
    }

    #[test]
    fn linked_worktree_resolves_to_checkout_root() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("project");
        let worktree = dir.path().join("clever-ember");
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

        assert_eq!(
            linked_worktree_root(&worktree).as_deref(),
            Some(root.to_str().unwrap())
        );
    }

    #[test]
    fn primary_checkout_and_other_layouts_have_no_parent_root() {
        let dir = tempfile::tempdir().unwrap();
        let primary = dir.path().join("primary");
        std::fs::create_dir_all(primary.join(".git")).unwrap();
        assert_eq!(linked_worktree_root(&primary), None);

        let plain = dir.path().join("plain");
        std::fs::create_dir_all(&plain).unwrap();
        assert_eq!(linked_worktree_root(&plain), None);

        let odd = dir.path().join("odd");
        std::fs::create_dir_all(&odd).unwrap();
        std::fs::write(odd.join(".git"), "gitdir: /somewhere/else\n").unwrap();
        assert_eq!(linked_worktree_root(&odd), None);
    }
}
