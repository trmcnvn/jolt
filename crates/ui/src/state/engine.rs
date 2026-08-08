use std::path::PathBuf;
use std::sync::Arc;

use jolt_proto::HarnessId;
use jolt_rpc::RpcClient;

/// Everything needed to reach (or start) an engine.
#[derive(Debug, Clone)]
pub struct EngineBootConfig {
    /// Data directory for the embedded engine (`~/.jolt`).
    pub data_dir: PathBuf,
    /// Localhost IPC port to dial / serve.
    pub ipc_port: u16,
    /// Edge base URL for the embedded engine.
    pub edge_url: String,
    /// Development bearer for authenticated edge room joins. Update checks use
    /// the public edge release endpoint even when this is `None`.
    pub edge_token: Option<String>,
    /// Workspace org override for explicit dev-mode runs.
    pub org_id: Option<String>,
    /// WorkOS client id for production authentication.
    pub workos_client_id: Option<String>,
    /// Harness for doc-command runs until per-chat config lands (M4).
    pub default_harness: HarnessId,
}

/// How this UI reached its engine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EngineMode {
    /// Engine embedded in this process (in-memory RPC transport).
    InProcess,
    /// Connected to a separate daemon over localhost WebSocket.
    Remote { url: String },
}

/// Backend ownership supplied by the application composition root. The UI sees
/// only the product RPC client and lifecycle; it does not depend on the engine.
pub trait EngineBackend: Send + Sync {
    fn client(&self) -> &RpcClient;
    fn mode(&self) -> EngineMode;
    fn shutdown(&self) -> futures::future::BoxFuture<'static, ()>;
}

/// Application-provided connection/bootstrap function.
pub type EngineConnector = Arc<
    dyn Fn(EngineBootConfig) -> futures::future::BoxFuture<'static, anyhow::Result<EngineHandle>>
        + Send
        + Sync,
>;

/// Cheaply clonable handle to whichever backend won bootstrap.
#[derive(Clone)]
pub struct EngineHandle {
    inner: Arc<dyn EngineBackend>,
}

impl EngineHandle {
    pub fn new(backend: Arc<dyn EngineBackend>) -> Self {
        Self { inner: backend }
    }

    pub fn client(&self) -> &RpcClient {
        self.inner.client()
    }

    pub fn mode(&self) -> EngineMode {
        self.inner.mode()
    }

    pub async fn shutdown(&self) {
        self.inner.shutdown().await;
    }

    #[cfg(test)]
    pub async fn bootstrap(config: EngineBootConfig) -> anyhow::Result<Self> {
        use jolt_engine::{Engine, EngineConfig, EngineSupervisor};

        struct TestBackend {
            supervisor: Arc<EngineSupervisor>,
            boot_task: tokio::task::JoinHandle<()>,
            refresh_task: tokio::task::JoinHandle<()>,
            ipc_task: Option<tokio::task::JoinHandle<()>>,
            client: RpcClient,
        }

        impl EngineBackend for TestBackend {
            fn client(&self) -> &RpcClient {
                &self.client
            }

            fn mode(&self) -> EngineMode {
                EngineMode::InProcess
            }

            fn shutdown(&self) -> futures::future::BoxFuture<'static, ()> {
                self.boot_task.abort();
                self.refresh_task.abort();
                if let Some(task) = &self.ipc_task {
                    task.abort();
                }
                let supervisor = self.supervisor.clone();
                Box::pin(async move { supervisor.shutdown().await })
            }
        }

        struct TestRemote {
            client: RpcClient,
            url: String,
        }

        impl EngineBackend for TestRemote {
            fn client(&self) -> &RpcClient {
                &self.client
            }

            fn mode(&self) -> EngineMode {
                EngineMode::Remote {
                    url: self.url.clone(),
                }
            }

            fn shutdown(&self) -> futures::future::BoxFuture<'static, ()> {
                Box::pin(std::future::ready(()))
            }
        }

        let url = format!("ws://127.0.0.1:{}", config.ipc_port);
        if let Ok(client) = jolt_rpc::connect_ws(&url).await {
            return Ok(Self::new(Arc::new(TestRemote { client, url })));
        }

        let engine_config = EngineConfig {
            data_dir: config.data_dir,
            edge_url: config.edge_url,
            edge_token: config.edge_token,
            ipc_port: config.ipc_port,
            default_harness: config.default_harness,
            org_id: config.org_id,
            workos_client_id: config.workos_client_id,
        };
        let auth = Engine::build_auth(&engine_config).await;
        let refresh_task = auth.spawn_refresh_loop();
        let supervisor = EngineSupervisor::new(engine_config.clone(), auth);
        let client = jolt_rpc::memory_client(supervisor.clone());
        let ipc_task = jolt_engine::serve_ipc(engine_config.ipc_port, supervisor.clone())
            .await
            .ok();
        let boot_task = supervisor.spawn_when_ready();
        if let Err(error) = supervisor.wait_ready().await {
            boot_task.abort();
            refresh_task.abort();
            return Err(error);
        }
        Ok(Self::new(Arc::new(TestBackend {
            supervisor,
            boot_task,
            refresh_task,
            ipc_task,
            client,
        })))
    }
}
