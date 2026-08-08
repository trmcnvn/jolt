use std::sync::Arc;

use jolt_engine::{Engine, EngineConfig, EngineSupervisor};
use jolt_rpc::{RpcClient, connect_ws, memory_client};
use jolt_ui::{EngineBackend, EngineBootConfig, EngineConnector, EngineHandle, EngineMode};

struct InProcessEngine {
    supervisor: Arc<EngineSupervisor>,
    boot_task: tokio::task::JoinHandle<()>,
    refresh_task: tokio::task::JoinHandle<()>,
    ipc_task: Option<tokio::task::JoinHandle<()>>,
    client: RpcClient,
}

impl EngineBackend for InProcessEngine {
    fn client(&self) -> &RpcClient {
        &self.client
    }

    fn mode(&self) -> EngineMode {
        EngineMode::InProcess
    }

    fn shutdown(&self) -> futures::future::BoxFuture<'static, ()> {
        self.boot_task.abort();
        self.refresh_task.abort();
        if let Some(ipc) = &self.ipc_task {
            ipc.abort();
        }
        let supervisor = self.supervisor.clone();
        Box::pin(async move { supervisor.shutdown().await })
    }
}

struct RemoteEngine {
    client: RpcClient,
    url: String,
}

impl EngineBackend for RemoteEngine {
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

pub fn connector() -> EngineConnector {
    Arc::new(|config| Box::pin(connect(config)))
}

async fn connect(config: EngineBootConfig) -> anyhow::Result<EngineHandle> {
    let url = format!("ws://127.0.0.1:{}", config.ipc_port);
    match connect_ws(&url).await {
        Ok(client) => {
            tracing::info!(%url, "connected to engine daemon");
            return Ok(EngineHandle::new(Arc::new(RemoteEngine { client, url })));
        }
        Err(error) => tracing::debug!(%url, %error, "engine daemon unavailable"),
    }

    tracing::info!(data_dir = %config.data_dir.display(), "embedding engine");
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
    let client = memory_client(supervisor.clone());
    let ipc_task = match jolt_engine::serve_ipc(engine_config.ipc_port, supervisor.clone()).await {
        Ok(task) => Some(task),
        Err(error) => {
            tracing::warn!(
                port = engine_config.ipc_port,
                %error,
                "IPC port unavailable; other viewports cannot attach to this window"
            );
            None
        }
    };
    let boot_task = supervisor.spawn_when_ready();
    if let Err(error) = supervisor.wait_ready().await {
        boot_task.abort();
        if let Some(task) = &ipc_task {
            task.abort();
        }
        refresh_task.abort();
        return Err(error);
    }
    Ok(EngineHandle::new(Arc::new(InProcessEngine {
        supervisor,
        boot_task,
        refresh_task,
        ipc_task,
        client,
    })))
}
