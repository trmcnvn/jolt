//! jolt-engine — the machine-local backend: harness sessions, document hosting,
//! durable command execution, repositories, terminals, diffs, accounts, secrets,
//! usage, updates, and the IPC/device-relay RPC surface.
//!
//! See docs/architecture.md for process and data ownership.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};

use async_trait::async_trait;
use jolt_rpc::{RpcError, RpcReply, RpcService};

pub use jolt_proto::HarnessId;

use jolt_sync::DocsStore;

pub mod agent_accounts;
pub mod auth;
pub mod diff_projection;
pub mod diff_sync;
pub mod doc_host;
mod goals;
pub mod instance_lock;
mod mcp;
mod model_selection;
mod question_extraction;
pub mod registry;
pub mod repos;
pub mod rpc;
pub mod run_journal;
pub mod scopes;
pub mod secrets;
pub mod sessions;
mod simd_base64;
pub mod spaces;
pub mod terminals;
pub mod titles;
pub mod turn_diffs;
pub mod uploads;
pub mod usage;
pub mod vcs;
pub mod workspace_host;

pub use agent_accounts::{AgentAccounts, AgentAccountsConfig};
pub use auth::{Auth, AuthConfig, AuthState, AuthUser, OrgMembership};
pub use diff_projection::{DIFF_PAGE_MAX_BYTES, DIFF_PAGE_TARGET_BYTES, DiffProjection};
pub use diff_sync::{
    CheckoutDiffSync, DiffSidecar, DiffSnapshot, TurnDiffBaseline, capture_diff, capture_turn_diff,
    capture_turn_diff_baseline,
};
pub use doc_host::{ChatDocHandle, DocHost, DocHostConfig, EdgeConfig};
pub use instance_lock::InstanceLock;
pub use registry::{HarnessDescriptor, HarnessRegistry, default_registry};
pub use repos::{CheckoutIdentity, Repos, worktree_branch_from_title};
pub use rpc::EngineRpc;
pub use run_journal::{JournalError, RunJournal};
pub use scopes::{AccountScope, ScopeKind, ScopeLayout, ScopeStatus};
pub use secrets::{HarnessSecrets, SecretsError};
pub use sessions::{JournaledEvent, SessionsEngine, SteerOutcome};
pub use spaces::SpacesSync;
pub use terminals::Terminals;
pub use titles::TitleGenerator;
pub use turn_diffs::TurnDiffStore;
pub use uploads::{AttachmentChunk, CommittedAttachment, Uploads};
pub use usage::UsageStore;
pub use vcs::Vcs;
pub use workspace_host::{
    DEFAULT_ORG_ID, DEFAULT_USER_ID, WORKSPACE_DOC_ID, WorkspaceHost, WorkspaceHostConfig,
};

#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error("doc: {0}")]
    Doc(#[from] jolt_doc::DocError),
    #[error("journal: {0}")]
    Journal(#[from] run_journal::JournalError),
    #[error("store: {0}")]
    Store(#[from] jolt_sync::StoreError),
    #[error("harness: {0}")]
    Harness(#[from] jolt_harness::HarnessError),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("{0}")]
    Other(String),
}

/// Epoch millis now — the doc/journal timestamp base.
pub(crate) fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

pub(crate) fn new_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[derive(Debug, Clone)]
pub struct EngineConfig {
    /// Data directory (default `~/.jolt`, dev `~/.jolt-dev`).
    pub data_dir: PathBuf,
    /// Edge base URL.
    pub edge_url: String,
    /// Bearer for development edge room joins. `None` disables authenticated
    /// sync in Local, but the public release endpoint remains available.
    pub edge_token: Option<String>,
    /// Localhost IPC port for the UI.
    pub ipc_port: u16,
    /// Harness for doc-command runs on chats without a workspace `config` row.
    pub default_harness: HarnessId,
    /// Workspace-doc org (`ws/{orgId}` room). `None` = `$JOLT_ORG_ID` or the dev default.
    /// In WorkOS mode the signed-in session's org wins.
    pub org_id: Option<String>,
    /// WorkOS client id — enables real auth; `None` = dev mode (bearer = `edge_token`).
    pub workos_client_id: Option<String>,
}

#[derive(Clone)]
struct DeviceServices {
    secrets: HarnessSecrets,
    agent_accounts: AgentAccounts,
}

impl DeviceServices {
    fn open(data_dir: &Path) -> Result<Self, EngineError> {
        Ok(Self {
            secrets: HarnessSecrets::open(data_dir)
                .map_err(|error| EngineError::Other(format!("secrets: {error}")))?,
            agent_accounts: AgentAccounts::new(AgentAccountsConfig::detect(data_dir)),
        })
    }
}

/// The assembled engine core — also constructible without the IPC server for tests
/// and the in-process (headed) mode.
pub struct EngineCore {
    pub sessions: SessionsEngine,
    pub doc_host: DocHost,
    pub workspace: WorkspaceHost,
    pub registry: Arc<HarnessRegistry>,
    pub repos: Repos,
    pub terminals: Terminals,
    pub diff_sync: CheckoutDiffSync,
    pub spaces_sync: SpacesSync,
    pub uploads: Uploads,
    pub agent_accounts: AgentAccounts,
    pub secrets: HarnessSecrets,
    pub device_id: String,
    /// Auth service (attached by [`Engine::run`]; a lazy dev-mode instance otherwise).
    auth: std::sync::Mutex<Option<Auth>>,
    /// Peer link cache for `targetDeviceId` routing (attached when edge+auth are ready).
    links: std::sync::Mutex<Option<Arc<jolt_rpc::LinkCache>>>,
    /// Release checker (attached by [`Engine::assemble_runtime`]) — the
    /// UpdateStatus stream + ApplyUpdate.
    updater: std::sync::Mutex<Option<jolt_update::Updater>>,
    /// Exclusive data-dir lock — held for the engine's lifetime (single-instance).
    _instance_lock: InstanceLock,
}

impl EngineCore {
    /// Open stores under `data_dir`, wire sessions ⇄ doc host ⇄ workspace host, and
    /// recover stale journals from a previous crash. Identity comes from
    /// `$JOLT_ORG_ID` / `$JOLT_USER_ID` (dev defaults `dev-org` / `dev-user`);
    /// use [`Self::assemble_with_identity`] to pass one explicitly.
    pub fn assemble(
        data_dir: &Path,
        registry: Arc<HarnessRegistry>,
        default_harness: HarnessId,
        edge: Option<EdgeConfig>,
    ) -> Result<Self, EngineError> {
        let org_id = env_or("JOLT_ORG_ID", DEFAULT_ORG_ID);
        let user_id = env_or("JOLT_USER_ID", DEFAULT_USER_ID);
        Self::assemble_with_identity(data_dir, registry, default_harness, edge, &org_id, &user_id)
    }

    pub fn assemble_with_identity(
        data_dir: &Path,
        registry: Arc<HarnessRegistry>,
        default_harness: HarnessId,
        edge: Option<EdgeConfig>,
        org_id: &str,
        user_id: &str,
    ) -> Result<Self, EngineError> {
        let identity_dir = data_dir
            .join("orgs")
            .join(sanitize_path_id(org_id))
            .join(sanitize_path_id(user_id));
        Self::assemble_in_scope(
            data_dir,
            &identity_dir,
            data_dir,
            data_dir,
            registry,
            default_harness,
            edge,
            org_id,
            user_id,
            None,
        )
    }

    /// Assemble one Local or Account runtime. Device-wide configuration stays
    /// under `data_dir`; documents, usage, journals, uploads, and the logical
    /// device id are isolated under `scope_dir`.
    #[allow(clippy::too_many_arguments)]
    fn assemble_scoped(
        data_dir: &Path,
        scope_dir: &Path,
        lock_dir: &Path,
        registry: Arc<HarnessRegistry>,
        default_harness: HarnessId,
        edge: Option<EdgeConfig>,
        org_id: &str,
        user_id: &str,
        services: Option<DeviceServices>,
    ) -> Result<Self, EngineError> {
        Self::assemble_in_scope(
            data_dir,
            scope_dir,
            scope_dir,
            lock_dir,
            registry,
            default_harness,
            edge,
            org_id,
            user_id,
            services,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn assemble_in_scope(
        data_dir: &Path,
        identity_dir: &Path,
        device_dir: &Path,
        lock_dir: &Path,
        registry: Arc<HarnessRegistry>,
        default_harness: HarnessId,
        edge: Option<EdgeConfig>,
        org_id: &str,
        user_id: &str,
        services: Option<DeviceServices>,
    ) -> Result<Self, EngineError> {
        std::fs::create_dir_all(data_dir)?;
        std::fs::create_dir_all(identity_dir)?;
        std::fs::create_dir_all(device_dir)?;
        std::fs::create_dir_all(lock_dir)?;
        let lock = InstanceLock::acquire(lock_dir)?;
        let services = services.map_or_else(|| DeviceServices::open(data_dir), Ok)?;
        let secrets = services.secrets;
        registry.set_environment_provider(Arc::new(secrets.clone()));
        let device_id = load_or_create_device_id(device_dir)?;
        let store = Arc::new(DocsStore::open(identity_dir)?);
        let journal = Arc::new(RunJournal::open(identity_dir.join("journals"))?);
        let usage = UsageStore::open(&identity_dir.join("usage.sqlite"), device_id.clone())
            .map_err(|error| EngineError::Other(format!("usage store: {error}")))?;
        let repos = Repos::new(data_dir, &device_id);
        let sessions = SessionsEngine::new(device_id.clone(), journal, registry.clone(), usage);
        sessions.set_turn_diffs(TurnDiffStore::new(
            identity_dir.join("turn-diffs"),
            repos.clone(),
            device_id.clone(),
        ));
        let doc_host = DocHost::new(
            store.clone(),
            DocHostConfig {
                device_id: device_id.clone(),
                default_harness,
                edge: edge.clone(),
            },
        );
        let workspace = WorkspaceHost::open(
            store,
            WorkspaceHostConfig {
                device_id: device_id.clone(),
                device_name: local_device_name(),
                platform: std::env::consts::OS.to_string(),
                org_id: org_id.to_string(),
                user_id: user_id.to_string(),
                edge: edge.clone(),
            },
        )?;
        doc_host.set_workspace(workspace.clone());
        doc_host.set_sessions(sessions.clone());
        sessions.set_doc_host(doc_host.clone());
        match workspace.pause_active_goals_after_restart() {
            Ok(0) => {}
            Ok(paused) => tracing::info!(paused, "active goals paused on boot"),
            Err(err) => tracing::error!(error = %err, "active-goal recovery failed"),
        }
        match sessions.recover_stale() {
            Ok(0) => {}
            Ok(recovered) => tracing::info!(recovered, "stale sessions recovered on boot"),
            Err(err) => tracing::error!(error = %err, "stale-session recovery failed"),
        }
        let terminals = Terminals::new();
        let uploads = Uploads::new(identity_dir, edge.clone());
        let agent_accounts = services.agent_accounts;
        sessions.set_titles(TitleGenerator::new(
            workspace.clone(),
            registry.clone(),
            repos.clone(),
        ));
        let diff_sync = CheckoutDiffSync::start(repos.clone(), workspace.clone(), &device_id, edge);
        let spaces_sync = SpacesSync::start(repos.clone(), workspace.clone(), &device_id);
        Ok(Self {
            sessions,
            doc_host,
            workspace,
            registry,
            repos,
            terminals,
            diff_sync,
            spaces_sync,
            uploads,
            agent_accounts,
            secrets,
            device_id,
            auth: std::sync::Mutex::new(None),
            links: std::sync::Mutex::new(None),
            updater: std::sync::Mutex::new(None),
            _instance_lock: lock,
        })
    }

    /// Attach the auth service (before building the RPC service / relays).
    pub fn set_auth(&self, auth: Auth) {
        *self
            .auth
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(auth);
    }

    /// The attached auth service, or a lazily-created dev-mode one (in-process embeds
    /// that never wired WorkOS still answer AuthStatus honestly).
    pub fn auth(&self) -> Auth {
        let mut slot = self
            .auth
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        slot.get_or_insert_with(|| {
            let dev_user = std::env::var("JOLT_EDGE_TOKEN")
                .ok()
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_else(|| "dev-user".into());
            let mut config = AuthConfig::new("http://localhost:27640", std::env::temp_dir());
            config.dev_user_id = dev_user;
            Auth::new(config)
        })
        .clone()
    }

    /// Attach the peer link cache — enables `targetDeviceId` routing and [`Self::dial_device`].
    pub fn set_links(&self, links: Arc<jolt_rpc::LinkCache>) {
        *self
            .links
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(links);
    }

    pub fn links(&self) -> Option<Arc<jolt_rpc::LinkCache>> {
        self.links
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Attach the release checker (before building the RPC service).
    pub fn set_updater(&self, updater: jolt_update::Updater) {
        *self
            .updater
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(updater);
    }

    pub fn updater(&self) -> Option<jolt_update::Updater> {
        self.updater
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// A live RPC client to another device's engine through its relay DO (the router's
    /// dial seam). Cached per device; invalidated + re-dialed on failure.
    pub async fn dial_device(
        &self,
        device_id: &str,
    ) -> Result<Arc<jolt_rpc::RpcClient>, EngineError> {
        let links = self
            .links()
            .ok_or_else(|| EngineError::Other("peer links unavailable (offline)".into()))?;
        links
            .client(device_id)
            .await
            .map_err(|e| EngineError::Other(e.to_string()))
    }

    /// Start hosting our device room: serve the full RPC surface to relay clients and
    /// warm-open chat docs on nudges (§7 cold-chat command delivery). The token source
    /// re-reads auth on every (re)dial, so token refreshes take effect at reconnect.
    pub fn start_host_relay(&self, edge_url: &str) -> jolt_rpc::HostRelay {
        let auth = self.auth();
        let config =
            jolt_rpc::HostRelayConfig::new(edge_url, self.device_id.clone(), Arc::new(auth));
        let doc_host = self.doc_host.clone();
        let on_nudge: jolt_rpc::NudgeHandler = Arc::new(move |chat_id: String| {
            // Opening the doc joins its room + syncs; drain fires on the change
            // subscription — the command executes with no standing per-chat socket.
            match doc_host.open(&chat_id) {
                Ok(_) => tracing::info!(chat = %chat_id, "nudge: chat doc opened"),
                Err(err) => {
                    tracing::warn!(chat = %chat_id, error = %err, "nudge: open failed")
                }
            }
        });
        jolt_rpc::HostRelay::spawn(
            config,
            crate::rpc::relay_service(self.rpc_service()),
            on_nudge,
        )
    }

    pub fn rpc_service(&self) -> Arc<EngineRpc> {
        let mut rpc = EngineRpc::new(
            self.sessions.clone(),
            self.doc_host.clone(),
            self.workspace.clone(),
            self.registry.clone(),
            self.repos.clone(),
            self.terminals.clone(),
            self.diff_sync.clone(),
            self.spaces_sync.clone(),
            self.uploads.clone(),
            self.agent_accounts.clone(),
            self.secrets.clone(),
        )
        .with_auth(self.auth());
        if let Some(links) = self.links() {
            rpc = rpc.with_links(links);
        }
        if let Some(updater) = self.updater() {
            rpc = rpc.with_updater(updater);
        }
        Arc::new(rpc)
    }

    /// Graceful teardown: settle live runs (streaming entries stamped `aborted`),
    /// kill live PTYs, stamp our workspace `lastSeenAt`, and flush every open doc
    /// snapshot.
    pub async fn shutdown(&self) {
        self.sessions.shutdown().await;
        self.terminals.shutdown();
        self.agent_accounts.shutdown();
        self.doc_host.flush_all();
        self.workspace.shutdown();
    }
}

pub struct Engine {
    pub config: EngineConfig,
}

/// A fully assembled identity-scoped engine plus the relay handle whose lifetime
/// keeps this device reachable. Used by both the headless server and the headed
/// in-process engine so their production authentication paths cannot diverge.
pub struct EngineRuntime {
    core: EngineCore,
    _host_relay: Option<jolt_rpc::HostRelay>,
    owns_updater: bool,
}

#[derive(Clone)]
enum SupervisedEngineState {
    Waiting,
    Ready(Arc<EngineRpc>),
    Failed(String),
}

/// Owns the always-on Local runtime and, while authenticated, one Account
/// runtime. Switching changes only the viewport's routed service; both runtimes
/// continue executing independently.
pub struct EngineSupervisor {
    config: EngineConfig,
    auth: Auth,
    auth_rpc: rpc::AuthRpc,
    updater: jolt_update::Updater,
    state_tx: tokio::sync::watch::Sender<SupervisedEngineState>,
    scope_tx: tokio::sync::watch::Sender<ScopeStatus>,
    local: std::sync::Mutex<Option<EngineRuntime>>,
    account: std::sync::Mutex<Option<EngineRuntime>>,
    device_services: std::sync::Mutex<Option<DeviceServices>>,
    assembly_gate: tokio::sync::Mutex<()>,
}

impl EngineSupervisor {
    pub fn new(config: EngineConfig, auth: Auth) -> Arc<Self> {
        let (state_tx, _) = tokio::sync::watch::channel(SupervisedEngineState::Waiting);
        let (scope_tx, _) = tokio::sync::watch::channel(ScopeStatus::local());
        Arc::new_cyclic(|weak: &Weak<Self>| {
            let weak = weak.clone();
            let quiescent: jolt_update::QuiescentCheck = Arc::new(move || {
                weak.upgrade().is_none_or(|supervisor| {
                    let quiet = |slot: &Mutex<Option<EngineRuntime>>| {
                        lock(slot).as_ref().is_none_or(|runtime| {
                            !runtime.core().sessions.any_active()
                                && !runtime.core().terminals.any_open()
                        })
                    };
                    quiet(&supervisor.local) && quiet(&supervisor.account)
                })
            });
            let updater = jolt_update::Updater::spawn(config.edge_url.clone(), Some(quiescent));
            Self {
                config,
                auth: auth.clone(),
                auth_rpc: rpc::AuthRpc::new(auth),
                updater,
                state_tx,
                scope_tx,
                local: std::sync::Mutex::new(None),
                account: std::sync::Mutex::new(None),
                device_services: std::sync::Mutex::new(None),
                assembly_gate: tokio::sync::Mutex::new(()),
            }
        })
    }

    /// Resolve the preferred scope behind the splash, then continue watching
    /// authentication so a completed browser sign-in can assemble Account.
    pub fn spawn_when_ready(self: &Arc<Self>) -> tokio::task::JoinHandle<()> {
        let supervisor = self.clone();
        tokio::spawn(async move {
            if let Err(err) = supervisor.start().await {
                tracing::error!(error = %err, "engine assembly failed");
                supervisor
                    .state_tx
                    .send_replace(SupervisedEngineState::Failed(format!("{err:#}")));
                return;
            }
            let mut states = supervisor.auth.watch_state();
            loop {
                if states.changed().await.is_err() {
                    return;
                }
                let state = states.borrow().clone();
                if let Err(err) = supervisor.auth_changed(state).await {
                    tracing::warn!(error = %err, "account runtime transition failed");
                }
            }
        })
    }

    async fn start(&self) -> anyhow::Result<()> {
        self.ensure_local_runtime()?;
        if !self.auth.workos_enabled() && self.config.edge_token.is_none() {
            self.activate(ScopeKind::Local)?;
            return Ok(());
        }
        match self.auth.state() {
            AuthState::SignedIn { .. } => {
                if let Err(error) = self.prepare_signed_in(true).await {
                    tracing::warn!(error = %error, "account unavailable at startup; using Local");
                    self.activate(ScopeKind::Local)?;
                }
            }
            AuthState::NeedsOrganization { .. } => {
                if self.auth.ensure_personal_org().await.is_ok() {
                    self.prepare_signed_in(true).await?;
                } else {
                    self.activate(ScopeKind::Local)?;
                }
            }
            AuthState::SignedOut => self.activate(ScopeKind::Local)?,
        }
        Ok(())
    }

    async fn auth_changed(&self, state: AuthState) -> anyhow::Result<()> {
        match state {
            AuthState::SignedIn { .. } => self.prepare_signed_in(false).await,
            AuthState::NeedsOrganization { .. } => {
                self.auth.ensure_personal_org().await?;
                Ok(())
            }
            AuthState::SignedOut => {
                self.activate(ScopeKind::Local)?;
                let runtime = { lock(&self.account).take() };
                if let Some(runtime) = runtime {
                    runtime.shutdown().await;
                }
                self.publish_scope(ScopeKind::Local, false);
                Ok(())
            }
        }
    }

    async fn prepare_signed_in(&self, startup: bool) -> anyhow::Result<()> {
        let _gate = self.assembly_gate.lock().await;
        if lock(&self.account).is_some() {
            let active = if startup {
                ScopeKind::Account
            } else {
                self.scope_tx.borrow().active
            };
            self.publish_scope(active, false);
            if startup {
                self.activate(ScopeKind::Account)?;
            }
            return Ok(());
        }
        let local_has_data = self.local_has_data();
        if local_has_data {
            self.scope_tx.send_modify(|status| {
                status.account_available = false;
                status.account_email = self.auth.state().user().map(|user| user.email.clone());
                status.local_has_data = true;
                status.merge_pending = true;
            });
            self.activate(ScopeKind::Local)?;
            return Ok(());
        }
        self.ensure_account_runtime()?;
        self.activate(ScopeKind::Account)?;
        self.publish_scope(ScopeKind::Account, false);
        Ok(())
    }

    fn device_services(&self) -> Result<DeviceServices, EngineError> {
        let mut services = lock(&self.device_services);
        if services.is_none() {
            *services = Some(DeviceServices::open(&self.config.data_dir)?);
        }
        Ok(services
            .as_ref()
            .expect("device services initialized")
            .clone())
    }

    fn ensure_local_runtime(&self) -> anyhow::Result<()> {
        if lock(&self.local).is_some() {
            return Ok(());
        }
        let layout = ScopeLayout::new(&self.config.data_dir);
        let scope_dir = layout.ensure_local()?;
        let scope_id = layout.local_scope_id()?;
        let core = EngineCore::assemble_scoped(
            &self.config.data_dir,
            &scope_dir,
            &self.config.data_dir,
            Arc::new(default_registry()),
            self.config.default_harness,
            None,
            "local",
            &scope_id,
            Some(self.device_services()?),
        )?;
        core.set_auth(self.auth.clone());
        core.set_updater(self.updater.clone());
        *lock(&self.local) = Some(EngineRuntime {
            core,
            _host_relay: None,
            owns_updater: false,
        });
        Ok(())
    }

    fn ensure_account_runtime(&self) -> anyhow::Result<()> {
        if lock(&self.account).is_some() {
            return Ok(());
        }
        let (org_id, user_id) = self.account_identity()?;
        let scope = ScopeLayout::new(&self.config.data_dir).migrate_account(&org_id, &user_id)?;
        let device_id = load_or_create_device_id(&scope.dir)?;
        let edge = Some(
            EdgeConfig::new(self.config.edge_url.clone(), Arc::new(self.auth.clone()))
                .with_device(device_id),
        );
        let core = EngineCore::assemble_scoped(
            &self.config.data_dir,
            &scope.dir,
            &scope.dir,
            Arc::new(default_registry()),
            self.config.default_harness,
            edge.clone(),
            &org_id,
            &user_id,
            Some(self.device_services()?),
        )?;
        core.set_auth(self.auth.clone());
        core.set_updater(self.updater.clone());
        let host_relay = edge
            .as_ref()
            .map(|edge| configure_online_core(&core, edge, &self.auth));
        *lock(&self.account) = Some(EngineRuntime {
            core,
            _host_relay: host_relay,
            owns_updater: false,
        });
        Ok(())
    }

    fn account_identity(&self) -> anyhow::Result<(String, String)> {
        let dev_org = self
            .config
            .edge_token
            .as_deref()
            .and_then(|token| token.split_once('@'))
            .map(|(_, org)| org.to_string());
        let org_id = self
            .auth
            .state()
            .org_id()
            .map(str::to_string)
            .or(dev_org)
            .or_else(|| self.config.org_id.clone())
            .ok_or_else(|| anyhow::anyhow!("account has no organization"))?;
        let user_id = self
            .auth
            .user_id()
            .ok_or_else(|| anyhow::anyhow!("account has no user"))?;
        Ok((org_id, user_id))
    }

    fn local_has_data(&self) -> bool {
        lock(&self.local).as_ref().is_some_and(|runtime| {
            runtime
                .core()
                .workspace
                .read_chats()
                .is_ok_and(|chats| !chats.is_empty())
                || runtime
                    .core()
                    .workspace
                    .read_spaces()
                    .is_ok_and(|spaces| !spaces.is_empty())
        })
    }

    fn activate(&self, scope: ScopeKind) -> anyhow::Result<()> {
        let service = match scope {
            ScopeKind::Local => lock(&self.local)
                .as_ref()
                .map(|runtime| runtime.core().rpc_service()),
            ScopeKind::Account => lock(&self.account)
                .as_ref()
                .map(|runtime| runtime.core().rpc_service()),
        }
        .ok_or_else(|| anyhow::anyhow!("{scope:?} runtime unavailable"))?;
        self.state_tx
            .send_replace(SupervisedEngineState::Ready(service));
        let merge_pending = self.scope_tx.borrow().merge_pending;
        self.publish_scope(scope, merge_pending);
        Ok(())
    }

    fn publish_scope(&self, active: ScopeKind, merge_pending: bool) {
        let account_available = lock(&self.account).is_some();
        self.scope_tx.send_replace(ScopeStatus {
            active,
            account_available,
            account_email: self.auth.state().user().map(|user| user.email.clone()),
            local_has_data: self.local_has_data(),
            merge_pending,
        });
    }

    async fn resolve_account_link(&self, merge: bool) -> anyhow::Result<()> {
        let _gate = self.assembly_gate.lock().await;
        let (org_id, user_id) = self.account_identity()?;
        if merge {
            let layout = ScopeLayout::new(&self.config.data_dir);
            let account_existed = layout.has_account_data(&org_id, &user_id);
            if account_existed {
                self.ensure_account_runtime()?;
            }
            {
                let local = lock(&self.local);
                let runtime = local
                    .as_ref()
                    .ok_or_else(|| anyhow::anyhow!("Local runtime unavailable"))?;
                if runtime.core().sessions.any_active() || runtime.core().terminals.any_open() {
                    anyhow::bail!(
                        "finish or stop active Local runs and terminals before syncing Local"
                    );
                }
                let chat_ids: Vec<String> = runtime
                    .core()
                    .workspace
                    .read_chats()?
                    .into_iter()
                    .map(|chat| chat.id)
                    .collect();
                runtime.core().doc_host.rewrite_text_prefix(
                    &chat_ids,
                    &layout.local_dir().join("uploads").to_string_lossy(),
                    &layout
                        .account_dir(&org_id, &user_id)
                        .join("uploads")
                        .to_string_lossy(),
                )?;
            }
            // Stop routing new work into either store before merging files.
            self.state_tx.send_replace(SupervisedEngineState::Waiting);
            let local = { lock(&self.local).take() };
            if let Some(local) = local {
                local.shutdown().await;
            }
            let account = { lock(&self.account).take() };
            if let Some(account) = account {
                account.shutdown().await;
            }
            tokio::task::yield_now().await;
            let result = if account_existed {
                layout.merge_local_into_account(&org_id, &user_id)
            } else {
                layout.promote_local(&org_id, &user_id)
            };
            if let Err(error) = result {
                self.ensure_local_runtime()?;
                let _ = self.ensure_account_runtime();
                self.activate(ScopeKind::Local)?;
                return Err(error.into());
            }
            self.ensure_local_runtime()?;
        }
        if let Err(error) = self.ensure_account_runtime() {
            self.activate(ScopeKind::Local)?;
            return Err(error);
        }
        if merge && let Some(account) = lock(&self.account).as_ref() {
            for chat in account.core().workspace.read_chats()? {
                account.core().doc_host.open(&chat.id)?;
            }
        }
        self.activate(ScopeKind::Account)?;
        self.publish_scope(ScopeKind::Account, false);
        Ok(())
    }

    pub async fn assemble_current(&self) -> anyhow::Result<()> {
        self.start().await
    }

    /// Wait for splash-time Local/Account resolution without exposing the
    /// intermediate Local runtime to the headed viewport.
    pub async fn wait_ready(&self) -> anyhow::Result<()> {
        let mut state = self.state_tx.subscribe();
        loop {
            let current = state.borrow().clone();
            match current {
                SupervisedEngineState::Waiting => {}
                SupervisedEngineState::Ready(_) => return Ok(()),
                SupervisedEngineState::Failed(message) => anyhow::bail!(message),
            }
            state.changed().await?;
        }
    }

    pub async fn shutdown(&self) {
        self.state_tx.send_replace(SupervisedEngineState::Waiting);
        let account = { lock(&self.account).take() };
        if let Some(account) = account {
            account.shutdown().await;
        }
        let local = { lock(&self.local).take() };
        if let Some(local) = local {
            local.shutdown().await;
        }
        self.updater.shutdown();
    }
}

#[async_trait]
impl RpcService for EngineSupervisor {
    async fn handle(&self, method: &str, params: serde_json::Value) -> Result<RpcReply, RpcError> {
        match method {
            jolt_rpc::methods::SCOPE_STATUS => {
                return Ok(RpcReply::Stream(rpc::watch_stream(
                    self.scope_tx.subscribe(),
                )));
            }
            jolt_rpc::methods::SWITCH_SCOPE => {
                #[derive(serde::Deserialize)]
                struct Params {
                    scope: ScopeKind,
                }
                let params: Params = jolt_rpc::parse_params(params)?;
                self.activate(params.scope)
                    .map_err(|error| RpcError::Failed(error.to_string()))?;
                return RpcReply::value(&self.scope_tx.borrow().clone());
            }
            jolt_rpc::methods::RESOLVE_ACCOUNT_LINK => {
                #[derive(serde::Deserialize)]
                struct Params {
                    merge: bool,
                }
                let params: Params = jolt_rpc::parse_params(params)?;
                self.resolve_account_link(params.merge)
                    .await
                    .map_err(|error| RpcError::Failed(error.to_string()))?;
                return RpcReply::value(&self.scope_tx.borrow().clone());
            }
            jolt_rpc::methods::SIGN_OUT => {
                self.activate(ScopeKind::Local)
                    .map_err(|error| RpcError::Failed(error.to_string()))?;
                self.auth.sign_out();
                return RpcReply::value(&serde_json::json!({ "ok": true }));
            }
            _ if rpc::AuthRpc::handles(method) => {
                return self.auth_rpc.handle(method, params).await;
            }
            _ if rpc::theme_sync_method(method) => {
                let account_service = lock(&self.account)
                    .as_ref()
                    .map(|runtime| runtime.core().rpc_service());
                let Some(account_service) = account_service else {
                    return Err(RpcError::Failed(
                        "theme sync requires a signed-in account".into(),
                    ));
                };
                return account_service.handle(method, params).await;
            }
            _ => {}
        }

        let mut state = self.state_tx.subscribe();
        loop {
            let current = { state.borrow().clone() };
            match current {
                SupervisedEngineState::Waiting => {}
                SupervisedEngineState::Ready(service) => {
                    return service.handle(method, params).await;
                }
                SupervisedEngineState::Failed(message) => {
                    return Err(RpcError::Failed(message));
                }
            }
            state.changed().await.map_err(|_| RpcError::Closed)?;
        }
    }
}

impl EngineRuntime {
    pub fn core(&self) -> &EngineCore {
        &self.core
    }

    pub async fn shutdown(&self) {
        self.core.shutdown().await;
        if self.owns_updater
            && let Some(updater) = self.core.updater()
        {
            updater.shutdown();
        }
    }
}

fn configure_online_core(core: &EngineCore, edge: &EdgeConfig, auth: &Auth) -> jolt_rpc::HostRelay {
    let links = jolt_rpc::LinkCache::new(jolt_rpc::LinkCacheConfig::new(
        edge.url.clone(),
        Arc::new(auth.clone()),
    ));
    let links_for_presence = links.clone();
    core.workspace
        .set_peer_alive_hook(Arc::new(move |device_id: &str| {
            links_for_presence.reset_cooldown(device_id);
        }));
    core.set_links(links);
    core.start_host_relay(&edge.url)
}

impl Engine {
    pub fn new(config: EngineConfig) -> Self {
        Self { config }
    }

    /// Resolve the shared dev/WorkOS auth configuration for headed and headless
    /// modes. Production callers pass the baked WorkOS client id; explicit dev
    /// bearers still opt into the local dev identity.
    pub async fn build_auth(config: &EngineConfig) -> Auth {
        let mut auth_config = AuthConfig::new(config.edge_url.clone(), config.data_dir.clone());
        auth_config.workos_client_id = config.workos_client_id.clone();
        if let Ok(base) = std::env::var("JOLT_WORKOS_API_BASE")
            && !base.trim().is_empty()
        {
            auth_config.workos_api_base = base;
        }
        auth_config.callback_port = Some(
            std::env::var("JOLT_CALLBACK_PORT")
                .ok()
                .and_then(|p| p.parse().ok())
                .unwrap_or(27641),
        );
        if let Some(token) = &config.edge_token {
            auth_config.dev_user_id = token.clone();
        }
        Auth::detect(auth_config).await
    }

    /// Assemble the account-only runtime used by headless mode. Authentication
    /// is mandatory there; the headed app uses [`EngineSupervisor`] instead.
    pub async fn assemble_runtime(
        config: &EngineConfig,
        auth: Auth,
    ) -> anyhow::Result<EngineRuntime> {
        let dev_token_org = config
            .edge_token
            .as_deref()
            .and_then(|token| token.split_once('@'))
            .map(|(_, org)| org.to_string())
            .filter(|org| !org.is_empty());
        let org_id = auth
            .state()
            .org_id()
            .map(str::to_string)
            .or(dev_token_org)
            .or(config.org_id.clone())
            .unwrap_or_else(|| env_or("JOLT_ORG_ID", DEFAULT_ORG_ID));
        let user_id = auth
            .user_id()
            .ok_or_else(|| anyhow::anyhow!("headless mode requires an account"))?;
        let scope = ScopeLayout::new(&config.data_dir).migrate_account(&org_id, &user_id)?;
        let device_id = load_or_create_device_id(&scope.dir)?;
        let edge =
            EdgeConfig::new(config.edge_url.clone(), Arc::new(auth.clone())).with_device(device_id);
        let core = EngineCore::assemble_scoped(
            &config.data_dir,
            &scope.dir,
            &config.data_dir,
            Arc::new(default_registry()),
            config.default_harness,
            Some(edge.clone()),
            &org_id,
            &user_id,
            None,
        )?;
        core.set_auth(auth.clone());
        let quiescent: jolt_update::QuiescentCheck = {
            let sessions = core.sessions.clone();
            let terminals = core.terminals.clone();
            Arc::new(move || !sessions.any_active() && !terminals.any_open())
        };
        core.set_updater(jolt_update::Updater::spawn(
            config.edge_url.clone(),
            Some(quiescent),
        ));
        tracing::info!(device_id = %core.device_id, "account engine core assembled");
        let host_relay = Some(configure_online_core(&core, &edge, &auth));

        Ok(EngineRuntime {
            core,
            _host_relay: host_relay,
            owns_updater: true,
        })
    }

    /// Run until ctrl-c: auth (dev or WorkOS), sessions engine + doc host + command
    /// executor, IPC server, and — when edge+auth are ready — the device-room host
    /// relay + peer link cache (targetDeviceId routing).
    pub async fn run(self) -> anyhow::Result<()> {
        let config = self.config;
        tracing::info!(data_dir = %config.data_dir.display(), "engine starting");

        std::fs::create_dir_all(&config.data_dir)?;
        let auth = Self::build_auth(&config).await;
        let _refresh_loop = auth.spawn_refresh_loop();

        // WorkOS mode: gate edge features on a signed-in, org-scoped session. A TTY
        // gets the interactive paste-code flow; a service manager (systemd/launchd)
        // fails fast with a "run `jolt login`" error instead of hanging on a prompt.
        if auth.workos_enabled() {
            terminal_sign_in(&auth).await?;
        }

        let runtime = Self::assemble_runtime(&config, auth).await?;
        let service = runtime.core().rpc_service();

        // A daemon exists to serve this port, so a bind failure is fatal here —
        // unlike the headed app, which can still work over its in-process
        // transport (see `serve_ipc`).
        let server = serve_ipc(config.ipc_port, service).await?;

        shutdown_signal().await?;
        tracing::info!("shutting down");
        server.abort();
        runtime.shutdown().await;
        Ok(())
    }
}

/// Ctrl-C or SIGTERM. systemd/launchd stop (and the auto-updater's service
/// restart) deliver SIGTERM — without catching it the daemon dies mid-write
/// and every stop takes the crash-recovery path instead of the graceful drain.
async fn shutdown_signal() -> std::io::Result<()> {
    #[cfg(unix)]
    {
        let mut sigterm =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        tokio::select! {
            result = tokio::signal::ctrl_c() => result,
            _ = sigterm.recv() => Ok(()),
        }
    }
    #[cfg(not(unix))]
    {
        tokio::signal::ctrl_c().await
    }
}

/// Serve the typed RPC on the localhost IPC port.
///
/// Both engines call this: the headless daemon, and the headed app's embedded
/// engine. That second case is the point — an embedded engine that keeps the
/// port to itself forces anyone wanting a second viewport (the terminal app) to
/// stop the desktop app, start a daemon, and start it again in the right order.
/// Serving here means any viewport can just attach.
///
/// Localhost only, exactly as before: this widens *which process* can serve the
/// port, not who can reach it.
pub async fn serve_ipc(
    port: u16,
    service: std::sync::Arc<dyn jolt_rpc::RpcService>,
) -> std::io::Result<tokio::task::JoinHandle<()>> {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port)).await?;
    tracing::info!(port, "IPC server listening");
    Ok(tokio::spawn(jolt_rpc::serve_ws_listener(listener, service)))
}

/// Block until the WorkOS session is signed in and scoped to its hidden Personal
/// organization. On a TTY, print the headless sign-in URL and read the pasted
/// `state.code`; organization setup is automatic. Off a TTY, only a missing login
/// errors — a persisted org-less session can finish setup unattended.
pub async fn terminal_sign_in(auth: &Auth) -> Result<(), EngineError> {
    use std::io::IsTerminal;
    let interactive = std::io::stdin().is_terminal();
    let mut state_rx = auth.watch_state();
    let mut stdin_reader: Option<tokio::task::JoinHandle<()>> = None;
    loop {
        let state = state_rx.borrow().clone();
        match state {
            AuthState::SignedIn { user, org_id } => {
                tracing::info!(email = %user.email, org = org_id.as_deref().unwrap_or("<none>"),
                    "auth: session ready");
                break;
            }
            AuthState::NeedsOrganization { user } => {
                tracing::info!(email = %user.email, "auth: provisioning personal workspace");
                auth.ensure_personal_org().await?;
            }
            AuthState::SignedOut => {
                if !interactive {
                    return Err(EngineError::Other(
                        "not signed in — run `jolt login` on this machine first".into(),
                    ));
                }
                if stdin_reader.is_none() {
                    let url = auth.start_headless_sign_in();
                    println!("Sign in to Jolt:\n\n  {url}\n");
                    println!("Then paste the code shown in the browser here and press enter.");
                    let auth = auth.clone();
                    stdin_reader = Some(tokio::spawn(async move {
                        loop {
                            let Some(line) = read_stdin_line().await else {
                                return;
                            };
                            let pasted = line.trim();
                            if pasted.is_empty() {
                                continue;
                            }
                            match auth.complete_sign_in(pasted).await {
                                Ok(()) => return,
                                Err(err) => println!("Sign-in failed: {err}"),
                            }
                        }
                    }));
                }
            }
        }
        if state_rx.changed().await.is_err() {
            break;
        }
    }
    if let Some(reader) = stdin_reader {
        reader.abort();
    }
    Ok(())
}

/// One line from stdin (blocking read off the runtime). `None` = stdin closed.
async fn read_stdin_line() -> Option<String> {
    tokio::task::spawn_blocking(|| {
        let mut line = String::new();
        match std::io::stdin().read_line(&mut line) {
            Ok(0) | Err(_) => None, // EOF / error
            Ok(_) => Some(line),
        }
    })
    .await
    .ok()
    .flatten()
}

/// Best-effort human name for this device's registry row (hostname).
fn local_device_name() -> String {
    std::env::var("JOLT_DEVICE_NAME")
        .ok()
        .or_else(|| std::env::var("HOSTNAME").ok())
        .or_else(|| std::fs::read_to_string("/etc/hostname").ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown-device".to_string())
}

/// Trimmed env var or the given default.
fn env_or(key: &str, default: &str) -> String {
    std::env::var(key)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| default.to_string())
}

/// Filesystem-safe form of an org/user id (path segments for `orgs/{org}/{user}/`).
fn sanitize_path_id(id: &str) -> String {
    id.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_') {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Stable per-installation device id, persisted at `{data_dir}/device-id`.
fn load_or_create_device_id(data_dir: &Path) -> Result<String, EngineError> {
    let path = data_dir.join("device-id");
    match std::fs::read_to_string(&path) {
        Ok(id) if !id.trim().is_empty() => Ok(id.trim().to_string()),
        Ok(_) | Err(_) => {
            let id = new_id();
            std::fs::write(&path, &id)?;
            Ok(id)
        }
    }
}

#[cfg(test)]
mod supervisor_tests {
    use super::*;

    #[tokio::test]
    async fn existing_account_is_moved_and_coexists_with_local() {
        let dir = tempfile::tempdir().unwrap();
        let legacy = EngineCore::assemble_with_identity(
            dir.path(),
            Arc::new(default_registry()),
            HarnessId::Mock,
            None,
            "org-1",
            "user-1",
        )
        .unwrap();
        let account_device = legacy.device_id.clone();
        legacy.shutdown().await;
        drop(legacy);
        std::fs::write(
            dir.path().join("session.json"),
            serde_json::to_vec(&serde_json::json!({
                "refreshToken": "offline-refresh",
                "user": { "id": "user-1", "email": "user@example.com" },
                "orgId": "org-1"
            }))
            .unwrap(),
        )
        .unwrap();

        let mut auth_config = AuthConfig::new("http://127.0.0.1:1", dir.path());
        auth_config.workos_client_id = Some("client_test".into());
        let auth = Auth::new(auth_config);
        let supervisor = EngineSupervisor::new(
            EngineConfig {
                data_dir: dir.path().to_path_buf(),
                edge_url: "http://127.0.0.1:1".into(),
                edge_token: None,
                ipc_port: 0,
                default_harness: HarnessId::Mock,
                org_id: None,
                workos_client_id: Some("client_test".into()),
            },
            auth,
        );
        let task = supervisor.spawn_when_ready();
        let client = jolt_rpc::memory_client(supervisor.clone());
        let account_local_device = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            client.call(jolt_rpc::methods::LOCAL_DEVICE, serde_json::json!({})),
        )
        .await
        .expect("Account runtime booted")
        .unwrap();
        assert_eq!(account_local_device["deviceId"], account_device);
        assert!(!dir.path().join("orgs/org-1/user-1").exists());
        assert!(
            dir.path()
                .join("scopes/accounts/org-1/user-1/docs.sqlite3")
                .exists()
        );

        client
            .call(
                jolt_rpc::methods::SWITCH_SCOPE,
                serde_json::json!({ "scope": "local" }),
            )
            .await
            .unwrap();
        let local_device = client
            .call(jolt_rpc::methods::LOCAL_DEVICE, serde_json::json!({}))
            .await
            .unwrap();
        assert_ne!(local_device["deviceId"], account_device);
        assert!(lock(&supervisor.account).is_some(), "Account keeps running");

        task.abort();
        supervisor.shutdown().await;
    }

    #[tokio::test]
    async fn signed_out_supervisor_serves_local_and_updates() {
        let dir = tempfile::tempdir().unwrap();
        let mut auth_config = AuthConfig::new("http://127.0.0.1:1", dir.path());
        auth_config.workos_client_id = Some("client_test".into());
        let auth = Auth::new(auth_config);
        let supervisor = EngineSupervisor::new(
            EngineConfig {
                data_dir: dir.path().to_path_buf(),
                edge_url: "http://127.0.0.1:1".into(),
                edge_token: None,
                ipc_port: 0,
                default_harness: HarnessId::Mock,
                org_id: None,
                workos_client_id: Some("client_test".into()),
            },
            auth,
        );
        let task = supervisor.spawn_when_ready();
        let client = jolt_rpc::memory_client(supervisor.clone());
        let harnesses = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            client.call(jolt_rpc::methods::LIST_HARNESSES, serde_json::json!({})),
        )
        .await
        .expect("Local runtime booted")
        .expect("harness list");
        assert!(harnesses.as_array().is_some_and(|rows| !rows.is_empty()));
        let mut updates = client
            .subscribe(jolt_rpc::methods::UPDATE_STATUS, serde_json::json!({}))
            .await
            .unwrap();
        assert!(
            updates.recv().await.is_some(),
            "Local receives update status"
        );
        task.abort();
        supervisor.shutdown().await;
    }
}
