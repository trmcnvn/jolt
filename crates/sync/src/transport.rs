//! Shared transport contracts for active edge synchronization clients.

use futures::future::BoxFuture;

#[derive(Debug, Clone, thiserror::Error)]
pub enum SyncError {
    #[error("websocket: {0}")]
    WebSocket(String),
    #[error("protocol: {0}")]
    Protocol(String),
    #[error("auth: {0}")]
    Auth(String),
    #[error("client is shut down")]
    Closed,
}

/// Supplies a fresh WebSocket URL for every dial attempt.
pub trait UrlProvider: Send + Sync + 'static {
    fn url(&self) -> BoxFuture<'static, Result<String, SyncError>>;
}

/// Fixed URL used by development and tests.
pub struct StaticUrl(pub String);

impl UrlProvider for StaticUrl {
    fn url(&self) -> BoxFuture<'static, Result<String, SyncError>> {
        let url = self.0.clone();
        Box::pin(async move { Ok(url) })
    }
}

/// Live synchronization diagnostics rendered by `jolt sync`.
#[derive(Debug, Clone, Default)]
pub struct RoomStatsSnapshot {
    pub connected: bool,
    pub last_pushed_ms: i64,
    pub last_ack_ms: i64,
    pub rejoins: u64,
    pub probes: u64,
    pub full_resyncs: u64,
    pub disconnects: u64,
    pub rejected: u64,
}
