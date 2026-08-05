//! jolt-engine — the headless backend: sessions engine, doc host + command executor,
//! run journal + crash recovery, and the IPC RPC server.
//!
//! Spec: ARCHITECTURE.md §5 and docs/research/feature-inventory.md §3. M2 surface:
//! sessions + docs + commands + minimal IPC. Terminals, repos/diffs, uploads, auth,
//! agent accounts, and the device-room host land in later milestones.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use jolt_rpc::{RpcError, RpcReply, RpcService};

pub use jolt_proto::HarnessId;

use jolt_sync::DocsStore;

pub mod agent_accounts;
pub mod auth;
pub mod diff_sync;
pub mod doc_host;
pub mod instance_lock;
mod question_extraction;
pub mod registry;
pub mod repos;
pub mod rpc;
pub mod run_journal;
pub mod sessions;
pub mod spaces;
pub mod terminals;
pub mod titles;
pub mod uploads;
pub mod vcs;
pub mod workspace_host;

pub use agent_accounts::{AgentAccounts, AgentAccountsConfig};
pub use auth::{Auth, AuthConfig, AuthState, AuthUser, OrgMembership};
pub use diff_sync::{CheckoutDiffSync, DiffSidecar, DiffSnapshot, capture_diff};
pub use doc_host::{ChatDocHandle, DocHost, DocHostConfig, EdgeConfig};
pub use instance_lock::InstanceLock;
pub use registry::{HarnessDescriptor, HarnessRegistry, default_registry};
pub use repos::{CheckoutIdentity, Repos, worktree_branch_from_title};
pub use rpc::EngineRpc;
pub use run_journal::{JournalError, RunJournal};
pub use sessions::{JournaledEvent, SessionsEngine, SteerOutcome};
pub use spaces::SpacesSync;
pub use terminals::Terminals;
pub use titles::TitleGenerator;
pub use uploads::{AttachmentChunk, Uploads};
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

#[derive(Debug, Clone)]
pub struct EngineConfig {
    /// Data directory (default `~/.jolt`, dev `~/.jolt-dev`).
    pub data_dir: PathBuf,
    /// Edge base URL.
    pub edge_url: String,
    /// Bearer for edge room joins; `None` runs fully offline (sync disabled).
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
        std::fs::create_dir_all(data_dir)?;
        // Single-instance guard: two engines on one data dir would race the
        // SQLite snapshots + journals. Taken before any store opens or the IPC
        // port binds; held (and kernel-released on crash) for the engine's life.
        let lock = InstanceLock::acquire(data_dir)?;
        let device_id = load_or_create_device_id(data_dir)?;
        // Identity-scoped storage: snapshots, the command ledger, and run
        // journals live under `orgs/{orgId}/{userId}/` so switching accounts or
        // orgs on one machine never reuses another identity's cached docs.
        let org_dir = data_dir
            .join("orgs")
            .join(sanitize_path_id(org_id))
            .join(sanitize_path_id(user_id));
        let store = Arc::new(DocsStore::open(&org_dir)?);
        let journal = Arc::new(RunJournal::open(org_dir.join("journals"))?);
        let sessions = SessionsEngine::new(device_id.clone(), journal, registry.clone());
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
        match sessions.recover_stale() {
            Ok(0) => {}
            Ok(recovered) => tracing::info!(recovered, "stale sessions recovered on boot"),
            Err(err) => tracing::error!(error = %err, "stale-session recovery failed"),
        }
        let repos = Repos::new(data_dir, &device_id);
        let terminals = Terminals::new();
        let uploads = Uploads::new(data_dir, edge.clone());
        let agent_accounts = AgentAccounts::new(AgentAccountsConfig::detect(data_dir));
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
        jolt_rpc::HostRelay::spawn(config, self.rpc_service(), on_nudge)
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
        if let Some(updater) = self.updater() {
            updater.shutdown();
        }
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
}

#[derive(Clone)]
enum SupervisedEngineState {
    Waiting,
    Ready(Arc<EngineRpc>),
    Failed(String),
}

/// Process-level RPC owner that keeps authentication available while the sole
/// identity-scoped runtime waits for sign-in and automatic organization setup.
pub struct EngineSupervisor {
    config: EngineConfig,
    auth: Auth,
    auth_rpc: rpc::AuthRpc,
    state_tx: tokio::sync::watch::Sender<SupervisedEngineState>,
    runtime: tokio::sync::Mutex<Option<EngineRuntime>>,
    assembly_gate: tokio::sync::Mutex<()>,
}

impl EngineSupervisor {
    pub fn new(config: EngineConfig, auth: Auth) -> Arc<Self> {
        let (state_tx, _) = tokio::sync::watch::channel(SupervisedEngineState::Waiting);
        Arc::new(Self {
            config,
            auth: auth.clone(),
            auth_rpc: rpc::AuthRpc::new(auth),
            state_tx,
            runtime: tokio::sync::Mutex::new(None),
            assembly_gate: tokio::sync::Mutex::new(()),
        })
    }

    /// Wait for the automatically-provisioned org-scoped session and assemble
    /// the runtime. Auth RPCs remain usable while this task waits.
    pub fn spawn_when_ready(self: &Arc<Self>) -> tokio::task::JoinHandle<()> {
        let supervisor = self.clone();
        tokio::spawn(async move {
            let mut auth_state = supervisor.auth.watch_state();
            while !auth_state.borrow().is_signed_in() {
                if auth_state.changed().await.is_err() {
                    supervisor
                        .state_tx
                        .send_replace(SupervisedEngineState::Failed(
                            "authentication state closed before sign-in".into(),
                        ));
                    return;
                }
            }
            if let Err(err) = supervisor.assemble_current().await {
                tracing::error!(error = %err, "engine assembly failed");
                supervisor
                    .state_tx
                    .send_replace(SupervisedEngineState::Failed(format!("{err:#}")));
            }
        })
    }

    /// Assemble immediately for callers that already completed authentication
    /// and automatic organization provisioning.
    pub async fn assemble_current(&self) -> anyhow::Result<()> {
        let _gate = self.assembly_gate.lock().await;
        if self.runtime.lock().await.is_some() {
            return Ok(());
        }
        let runtime = Engine::assemble_runtime(&self.config, self.auth.clone()).await?;
        let service = runtime.core().rpc_service();
        *self.runtime.lock().await = Some(runtime);
        self.state_tx
            .send_replace(SupervisedEngineState::Ready(service));
        Ok(())
    }

    pub async fn shutdown(&self) {
        self.state_tx.send_replace(SupervisedEngineState::Waiting);
        if let Some(runtime) = self.runtime.lock().await.take() {
            runtime.shutdown().await;
        }
    }
}

#[async_trait]
impl RpcService for EngineSupervisor {
    async fn handle(&self, method: &str, params: serde_json::Value) -> Result<RpcReply, RpcError> {
        if rpc::AuthRpc::handles(method) {
            return self.auth_rpc.handle(method, params).await;
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
    }
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

    /// Open the identity-scoped stores and online transports for an auth session
    /// that is already ready. The headed UI waits behind its sign-in gate before
    /// calling this; headless mode waits on the terminal flow.
    pub async fn assemble_runtime(
        config: &EngineConfig,
        auth: Auth,
    ) -> anyhow::Result<EngineRuntime> {
        let online = (auth.workos_enabled() || config.edge_token.is_some())
            && auth.access_token().await.is_some();
        let device_id = load_or_create_device_id(&config.data_dir)?;
        let edge = online.then(|| {
            EdgeConfig::new(config.edge_url.clone(), Arc::new(auth.clone())).with_device(device_id)
        });

        let dev_token_org = config
            .edge_token
            .as_deref()
            .and_then(|t| t.split_once('@'))
            .map(|(_, org)| org.to_string())
            .filter(|s| !s.is_empty());
        let org_id = auth
            .state()
            .org_id()
            .map(str::to_string)
            .or(dev_token_org)
            .or(config.org_id.clone())
            .unwrap_or_else(|| env_or("JOLT_ORG_ID", DEFAULT_ORG_ID));
        let user_id = auth
            .user_id()
            .unwrap_or_else(|| env_or("JOLT_USER_ID", DEFAULT_USER_ID));
        let core = EngineCore::assemble_with_identity(
            &config.data_dir,
            Arc::new(default_registry()),
            config.default_harness,
            edge.clone(),
            &org_id,
            &user_id,
        )?;
        core.set_auth(auth.clone());
        // Release checker: polls {edge}/releases on a 6h cadence; headless
        // installs with JOLT_AUTO_UPDATE=1 apply + restart themselves — gated
        // on quiescence so a restart never lands under a live run or open PTY.
        let quiescent: jolt_update::QuiescentCheck = {
            let sessions = core.sessions.clone();
            let terminals = core.terminals.clone();
            Arc::new(move || !sessions.any_active() && !terminals.any_open())
        };
        core.set_updater(jolt_update::Updater::spawn(
            config.edge_url.clone(),
            Some(quiescent),
        ));
        tracing::info!(device_id = %core.device_id, "engine core assembled");

        let host_relay = edge.as_ref().map(|edge| {
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
        });

        Ok(EngineRuntime {
            core,
            _host_relay: host_relay,
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

        let supervisor = EngineSupervisor::new(config.clone(), auth);
        supervisor.assemble_current().await?;

        // A daemon exists to serve this port, so a bind failure is fatal here —
        // unlike the headed app, which can still work over its in-process
        // transport (see `serve_ipc`).
        let server = serve_ipc(config.ipc_port, supervisor.clone()).await?;

        shutdown_signal().await?;
        tracing::info!("shutting down");
        server.abort();
        supervisor.shutdown().await;
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
