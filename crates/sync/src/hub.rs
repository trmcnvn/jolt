//! Host client for the wasm-free SessionHub command/projection protocol.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use futures::future::BoxFuture;
use futures::{SinkExt, StreamExt};
use jolt_session_doc::{
    SessionCommandEntry, SessionCommandPayload, SessionCommandStatus, TranscriptFrame,
    TranscriptManifest, TranscriptPage,
};
use serde::{Deserialize, Serialize};
use tokio::net::TcpStream;
use tokio::sync::{broadcast, mpsc, oneshot, watch};
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

use crate::{SyncError, UrlProvider};

const PING_INTERVAL: Duration = Duration::from_secs(15);
const SILENCE_LEASE: Duration = Duration::from_secs(45);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
const HOST_STATE_DEADLINE: Duration = Duration::from_secs(15);
const BACKOFF_BASE: Duration = Duration::from_millis(250);
const BACKOFF_CAP: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HubDeliveryState {
    Pending,
    Claimed,
    Terminal,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HubCommand {
    pub id: String,
    pub kind: String,
    pub payload: SessionCommandPayload,
    pub issued_by: String,
    pub issued_at: i64,
    pub expires_at: i64,
    #[serde(default)]
    pub based_on: Option<jolt_session_doc::CommandBasedOn>,
    pub seq: u64,
    pub update_revision: u64,
    pub delivery_state: HubDeliveryState,
    pub status: SessionCommandStatus,
    #[serde(default)]
    pub claimed_by: Option<String>,
    #[serde(default)]
    pub claim_token: Option<String>,
    #[serde(default)]
    pub resolution: Option<String>,
}

impl HubCommand {
    pub fn entry(&self) -> SessionCommandEntry {
        SessionCommandEntry {
            id: self.id.clone(),
            payload: self.payload.clone(),
            issued_by: self.issued_by.clone(),
            issued_at: self.issued_at,
            based_on: self.based_on.clone(),
            expires_at: Some(self.expires_at),
            status: self.status,
            resolution: self.resolution.clone(),
        }
    }
}

#[derive(Debug, Clone)]
pub enum SessionHubEvent {
    Connected {
        lease: u64,
        projection_sequence: u64,
        command_revision: u64,
        commands: Vec<HubCommand>,
    },
    Disconnected,
    Command(Box<HubCommand>),
}

#[derive(Debug, Clone, Copy, Default)]
pub struct SessionHubStats {
    pub connected: bool,
    pub lease: u64,
    pub projection_sequence: u64,
    pub command_revision: u64,
    pub reconnects: u64,
}

#[derive(Default)]
struct SharedStats {
    connected: std::sync::atomic::AtomicBool,
    lease: std::sync::atomic::AtomicU64,
    projection_sequence: std::sync::atomic::AtomicU64,
    command_revision: std::sync::atomic::AtomicU64,
    reconnects: std::sync::atomic::AtomicU64,
    commands: std::sync::Mutex<Vec<HubCommand>>,
}

impl SharedStats {
    fn snapshot(&self) -> SessionHubStats {
        use std::sync::atomic::Ordering::Relaxed;
        SessionHubStats {
            connected: self.connected.load(Relaxed),
            lease: self.lease.load(Relaxed),
            projection_sequence: self.projection_sequence.load(Relaxed),
            command_revision: self.command_revision.load(Relaxed),
            reconnects: self.reconnects.load(Relaxed),
        }
    }
}

#[derive(Debug)]
pub struct PublishResult {
    pub sequence: u64,
    pub duplicate: bool,
    pub need_base: bool,
}

#[derive(Debug)]
enum RequestKind {
    PublishBase {
        publish_id: String,
        manifest: TranscriptManifest,
        live_page: Option<TranscriptPage>,
    },
    PublishDelta {
        publish_id: String,
        page_id: String,
        base_page_revision: String,
        page_revision: String,
        frame: TranscriptFrame,
    },
    ClaimCommand {
        command_id: String,
    },
    ResolveCommand {
        command_id: String,
        claim_token: String,
        status: SessionCommandStatus,
        resolution: Option<String>,
    },
}

struct ActorRequest {
    kind: RequestKind,
    response: oneshot::Sender<Result<HubResponse, SyncError>>,
}

#[derive(Debug)]
struct HubResponse {
    sequence: u64,
    duplicate: bool,
    need_base: bool,
    command: Option<HubCommand>,
}

/// Connected host membership for one SessionHub.
pub struct SessionHubClient {
    requests: mpsc::Sender<ActorRequest>,
    events: broadcast::Sender<SessionHubEvent>,
    stats: Arc<SharedStats>,
    shutdown: watch::Sender<bool>,
    task: Option<tokio::task::JoinHandle<()>>,
}

impl SessionHubClient {
    pub async fn connect_via(provider: Arc<dyn UrlProvider>) -> Result<Self, SyncError> {
        let connector = Arc::new(WsHubConnector { provider });
        Self::connect_with(connector).await
    }

    async fn connect_with(connector: Arc<dyn HubConnector>) -> Result<Self, SyncError> {
        let (request_tx, request_rx) = mpsc::channel(128);
        let (events, _) = broadcast::channel(256);
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let (ready_tx, ready_rx) = oneshot::channel();
        let stats = Arc::new(SharedStats::default());
        let actor = HubActor {
            connector,
            requests: request_rx,
            events: events.clone(),
            shutdown: shutdown_rx,
            stats: stats.clone(),
        };
        let task = tokio::spawn(actor.run(ready_tx));
        match ready_rx.await {
            Ok(Ok(())) => Ok(Self {
                requests: request_tx,
                events,
                stats,
                shutdown: shutdown_tx,
                task: Some(task),
            }),
            Ok(Err(error)) => {
                task.abort();
                Err(error)
            }
            Err(_) => {
                task.abort();
                Err(SyncError::Closed)
            }
        }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<SessionHubEvent> {
        self.events.subscribe()
    }

    pub fn stats(&self) -> SessionHubStats {
        self.stats.snapshot()
    }

    /// Actionable commands included in the latest host-state frame.
    pub fn commands(&self) -> Vec<HubCommand> {
        self.stats
            .commands
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    pub async fn publish_base(
        &self,
        manifest: TranscriptManifest,
        live_page: Option<TranscriptPage>,
    ) -> Result<PublishResult, SyncError> {
        let response = self
            .request(RequestKind::PublishBase {
                publish_id: uuid::Uuid::new_v4().to_string(),
                manifest,
                live_page,
            })
            .await?;
        Ok(PublishResult {
            sequence: response.sequence,
            duplicate: response.duplicate,
            need_base: response.need_base,
        })
    }

    pub async fn publish_delta(
        &self,
        page_id: String,
        base_page_revision: String,
        page_revision: String,
        frame: TranscriptFrame,
    ) -> Result<PublishResult, SyncError> {
        let response = self
            .request(RequestKind::PublishDelta {
                publish_id: uuid::Uuid::new_v4().to_string(),
                page_id,
                base_page_revision,
                page_revision,
                frame,
            })
            .await?;
        Ok(PublishResult {
            sequence: response.sequence,
            duplicate: response.duplicate,
            need_base: response.need_base,
        })
    }

    pub async fn claim_command(&self, command_id: &str) -> Result<HubCommand, SyncError> {
        self.request(RequestKind::ClaimCommand {
            command_id: command_id.to_string(),
        })
        .await?
        .command
        .ok_or_else(|| SyncError::Protocol("claim response omitted command".into()))
    }

    pub async fn resolve_command(
        &self,
        command_id: &str,
        claim_token: &str,
        status: SessionCommandStatus,
        resolution: Option<&str>,
    ) -> Result<HubCommand, SyncError> {
        self.request(RequestKind::ResolveCommand {
            command_id: command_id.to_string(),
            claim_token: claim_token.to_string(),
            status,
            resolution: resolution.map(str::to_string),
        })
        .await?
        .command
        .ok_or_else(|| SyncError::Protocol("resolve response omitted command".into()))
    }

    async fn request(&self, kind: RequestKind) -> Result<HubResponse, SyncError> {
        let (response_tx, response_rx) = oneshot::channel();
        self.requests
            .send(ActorRequest {
                kind,
                response: response_tx,
            })
            .await
            .map_err(|_| SyncError::Closed)?;
        response_rx.await.map_err(|_| SyncError::Closed)?
    }

    pub async fn shutdown(&mut self) {
        let _ = self.shutdown.send(true);
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
    }
}

impl Drop for SessionHubClient {
    fn drop(&mut self) {
        let _ = self.shutdown.send(true);
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

struct HubPipe {
    tx: mpsc::Sender<String>,
    rx: mpsc::Receiver<String>,
}

trait HubConnector: Send + Sync + 'static {
    fn connect(&self) -> BoxFuture<'static, Result<HubPipe, SyncError>>;
}

struct WsHubConnector {
    provider: Arc<dyn UrlProvider>,
}

impl HubConnector for WsHubConnector {
    fn connect(&self) -> BoxFuture<'static, Result<HubPipe, SyncError>> {
        let provider = self.provider.clone();
        Box::pin(async move {
            let url = provider.url().await?;
            let (socket, _) = tokio_tungstenite::connect_async(url)
                .await
                .map_err(|error| SyncError::WebSocket(error.to_string()))?;
            let (out_tx, out_rx) = mpsc::channel(128);
            let (in_tx, in_rx) = mpsc::channel(128);
            tokio::spawn(pump(socket, out_rx, in_tx));
            Ok(HubPipe {
                tx: out_tx,
                rx: in_rx,
            })
        })
    }
}

async fn pump(
    socket: WebSocketStream<MaybeTlsStream<TcpStream>>,
    mut outbound: mpsc::Receiver<String>,
    inbound: mpsc::Sender<String>,
) {
    let (mut sink, mut stream) = socket.split();
    let mut ping = tokio::time::interval(PING_INTERVAL);
    ping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    ping.tick().await;
    let mut last_rx = tokio::time::Instant::now();
    loop {
        tokio::select! {
            frame = outbound.recv() => match frame {
                Some(text) => {
                    if sink.send(WsMessage::Text(text.into())).await.is_err() {
                        break;
                    }
                }
                None => {
                    let _ = sink.send(WsMessage::Close(None)).await;
                    break;
                }
            },
            frame = stream.next() => match frame {
                Some(Ok(WsMessage::Text(text))) => {
                    last_rx = tokio::time::Instant::now();
                    let text = text.to_string();
                    if text != "pong" && inbound.send(text).await.is_err() {
                        break;
                    }
                }
                Some(Ok(_)) => last_rx = tokio::time::Instant::now(),
                Some(Err(_)) | None => break,
            },
            _ = ping.tick() => {
                if sink.send(WsMessage::Text("ping".into())).await.is_err() {
                    break;
                }
            }
            _ = tokio::time::sleep_until(last_rx + SILENCE_LEASE) => break,
        }
    }
}

struct HubActor {
    connector: Arc<dyn HubConnector>,
    requests: mpsc::Receiver<ActorRequest>,
    events: broadcast::Sender<SessionHubEvent>,
    shutdown: watch::Receiver<bool>,
    stats: Arc<SharedStats>,
}

impl HubActor {
    async fn run(mut self, ready: oneshot::Sender<Result<(), SyncError>>) {
        let mut ready = Some(ready);
        let mut backoff = BACKOFF_BASE;
        loop {
            if *self.shutdown.borrow() {
                return;
            }
            let pipe = match tokio::time::timeout(CONNECT_TIMEOUT, self.connector.connect()).await {
                Ok(Ok(pipe)) => pipe,
                Ok(Err(error)) => {
                    if let Some(ready) = ready.take() {
                        let _ = ready.send(Err(error));
                        return;
                    }
                    if self.wait_backoff(backoff).await {
                        return;
                    }
                    backoff = (backoff * 2).min(BACKOFF_CAP);
                    continue;
                }
                Err(_) => {
                    let error = SyncError::WebSocket("SessionHub connect timed out".into());
                    if let Some(ready) = ready.take() {
                        let _ = ready.send(Err(error));
                        return;
                    }
                    if self.wait_backoff(backoff).await {
                        return;
                    }
                    backoff = (backoff * 2).min(BACKOFF_CAP);
                    continue;
                }
            };
            match self.run_session(pipe, &mut ready).await {
                Ok(SessionEnd::Shutdown) => return,
                Ok(SessionEnd::Lost) | Err(_) => {
                    use std::sync::atomic::Ordering::Relaxed;
                    if self.stats.connected.swap(false, Relaxed) {
                        backoff = BACKOFF_BASE;
                    }
                    self.stats.reconnects.fetch_add(1, Relaxed);
                    let _ = self.events.send(SessionHubEvent::Disconnected);
                    if self.wait_backoff(backoff).await {
                        return;
                    }
                    backoff = (backoff * 2).min(BACKOFF_CAP);
                }
            }
        }
    }

    async fn wait_backoff(&mut self, duration: Duration) -> bool {
        tokio::select! {
            _ = tokio::time::sleep(duration) => false,
            changed = self.shutdown.changed() => changed.is_err() || *self.shutdown.borrow(),
        }
    }

    async fn run_session(
        &mut self,
        mut pipe: HubPipe,
        ready: &mut Option<oneshot::Sender<Result<(), SyncError>>>,
    ) -> Result<SessionEnd, SyncError> {
        let mut lease = 0u64;
        let mut pending: HashMap<String, oneshot::Sender<Result<HubResponse, SyncError>>> =
            HashMap::new();
        let deadline = tokio::time::sleep(HOST_STATE_DEADLINE);
        tokio::pin!(deadline);
        loop {
            tokio::select! {
                changed = self.shutdown.changed() => {
                    if changed.is_err() || *self.shutdown.borrow() {
                        fail_pending(&mut pending, "SessionHub shutting down");
                        return Ok(SessionEnd::Shutdown);
                    }
                }
                _ = &mut deadline, if lease == 0 => {
                    fail_pending(&mut pending, "SessionHub host state timed out");
                    return Err(SyncError::Protocol("SessionHub host state timed out".into()));
                }
                inbound = pipe.rx.recv() => {
                    let Some(text) = inbound else {
                        fail_pending(&mut pending, "SessionHub connection lost");
                        return Ok(SessionEnd::Lost);
                    };
                    if let Some(next_lease) = self.handle_inbound(&text, &mut pending)? {
                        lease = next_lease;
                        if let Some(ready) = ready.take() {
                            let _ = ready.send(Ok(()));
                        }
                    }
                }
                request = self.requests.recv(), if lease != 0 => {
                    let Some(request) = request else {
                        return Ok(SessionEnd::Shutdown);
                    };
                    let request_id = uuid::Uuid::new_v4().to_string();
                    let encoded = encode_request(&request_id, lease, &request.kind)?;
                    pending.insert(request_id, request.response);
                    if pipe.tx.send(encoded).await.is_err() {
                        fail_pending(&mut pending, "SessionHub connection lost");
                        return Ok(SessionEnd::Lost);
                    }
                }
            }
        }
    }

    fn handle_inbound(
        &self,
        text: &str,
        pending: &mut HashMap<String, oneshot::Sender<Result<HubResponse, SyncError>>>,
    ) -> Result<Option<u64>, SyncError> {
        let frame: ServerFrame = serde_json::from_str(text)
            .map_err(|error| SyncError::Protocol(format!("SessionHub frame: {error}")))?;
        use std::sync::atomic::Ordering::Relaxed;
        match frame {
            ServerFrame::HostState {
                lease,
                projection_sequence,
                command_revision,
                commands,
            } => {
                self.stats.connected.store(true, Relaxed);
                self.stats.lease.store(lease, Relaxed);
                self.stats
                    .projection_sequence
                    .store(projection_sequence, Relaxed);
                self.stats.command_revision.store(command_revision, Relaxed);
                *self
                    .stats
                    .commands
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = commands.clone();
                let _ = self.events.send(SessionHubEvent::Connected {
                    lease,
                    projection_sequence,
                    command_revision,
                    commands,
                });
                Ok(Some(lease))
            }
            ServerFrame::Command { command } | ServerFrame::CommandUpdate { command } => {
                self.stats
                    .command_revision
                    .store(command.update_revision, Relaxed);
                let _ = self
                    .events
                    .send(SessionHubEvent::Command(Box::new(command)));
                Ok(None)
            }
            ServerFrame::Response {
                request_id,
                ok,
                sequence,
                duplicate,
                need_base,
                command,
                error,
            } => {
                if let Some(response) = pending.remove(&request_id) {
                    if ok {
                        if let Some(sequence) = sequence {
                            self.stats.projection_sequence.store(sequence, Relaxed);
                        }
                        let _ = response.send(Ok(HubResponse {
                            sequence: sequence.unwrap_or(0),
                            duplicate,
                            need_base,
                            command,
                        }));
                    } else {
                        let _ = response.send(Err(SyncError::Protocol(
                            error.unwrap_or_else(|| "SessionHub request rejected".into()),
                        )));
                    }
                }
                Ok(None)
            }
            ServerFrame::Error { code } => Err(SyncError::Protocol(code)),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
enum ServerFrame {
    HostState {
        lease: u64,
        projection_sequence: u64,
        command_revision: u64,
        commands: Vec<HubCommand>,
    },
    Command {
        command: HubCommand,
    },
    CommandUpdate {
        command: HubCommand,
    },
    Response {
        #[serde(rename = "requestId")]
        request_id: String,
        ok: bool,
        #[serde(default)]
        sequence: Option<u64>,
        #[serde(default)]
        duplicate: bool,
        #[serde(rename = "needBase", default)]
        need_base: bool,
        #[serde(default)]
        command: Option<HubCommand>,
        #[serde(default)]
        error: Option<String>,
    },
    Error {
        code: String,
    },
}

fn encode_request(
    request_id: &str,
    lease: u64,
    request: &RequestKind,
) -> Result<String, SyncError> {
    let value = match request {
        RequestKind::PublishBase {
            publish_id,
            manifest,
            live_page,
        } => {
            let mut value = serde_json::json!({
                "type": "publishBase",
                "requestId": request_id,
                "publishId": publish_id,
                "lease": lease,
                "manifest": manifest,
            });
            if let Some(live_page) = live_page {
                value["livePage"] = serde_json::to_value(live_page)
                    .map_err(|error| SyncError::Protocol(error.to_string()))?;
            }
            value
        }
        RequestKind::PublishDelta {
            publish_id,
            page_id,
            base_page_revision,
            page_revision,
            frame,
        } => serde_json::json!({
            "type": "publishDelta",
            "requestId": request_id,
            "publishId": publish_id,
            "lease": lease,
            "pageId": page_id,
            "basePageRevision": base_page_revision,
            "pageRevision": page_revision,
            "frame": frame,
        }),
        RequestKind::ClaimCommand { command_id } => serde_json::json!({
            "type": "claimCommand",
            "requestId": request_id,
            "lease": lease,
            "commandId": command_id,
        }),
        RequestKind::ResolveCommand {
            command_id,
            claim_token,
            status,
            resolution,
        } => {
            let mut value = serde_json::json!({
                "type": "resolveCommand",
                "requestId": request_id,
                "lease": lease,
                "commandId": command_id,
                "claimToken": claim_token,
                "status": status,
            });
            if let Some(resolution) = resolution {
                value["resolution"] = serde_json::Value::String(resolution.clone());
            }
            value
        }
    };
    serde_json::to_string(&value).map_err(|error| SyncError::Protocol(error.to_string()))
}

fn fail_pending(
    pending: &mut HashMap<String, oneshot::Sender<Result<HubResponse, SyncError>>>,
    message: &str,
) {
    for (_, response) in pending.drain() {
        let _ = response.send(Err(SyncError::WebSocket(message.to_string())));
    }
}

enum SessionEnd {
    Shutdown,
    Lost,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_state_uses_the_edge_camel_case_contract() {
        let frame: ServerFrame = serde_json::from_value(serde_json::json!({
            "type": "hostState",
            "lease": 7,
            "projectionSequence": 11,
            "commandRevision": 13,
            "commands": [{
                "id": "command-1",
                "kind": "interrupt",
                "payload": { "kind": "interrupt" },
                "issuedBy": "device-a",
                "issuedAt": 100,
                "expiresAt": 200,
                "seq": 1,
                "updateRevision": 2,
                "deliveryState": "pending",
                "status": "pending"
            }]
        }))
        .unwrap();
        match frame {
            ServerFrame::HostState {
                lease,
                projection_sequence,
                command_revision,
                commands,
            } => {
                assert_eq!(lease, 7);
                assert_eq!(projection_sequence, 11);
                assert_eq!(command_revision, 13);
                assert_eq!(commands[0].id, "command-1");
            }
            _ => panic!("expected host state"),
        }
    }

    #[test]
    fn optional_request_fields_are_omitted_not_null() {
        let manifest = TranscriptManifest {
            catalog_revision: "empty".into(),
            total_messages: 0,
            pages: Vec::new(),
            turns: Vec::new(),
        };
        let base: serde_json::Value = serde_json::from_str(
            &encode_request(
                "request-1",
                1,
                &RequestKind::PublishBase {
                    publish_id: "publish-1".into(),
                    manifest,
                    live_page: None,
                },
            )
            .unwrap(),
        )
        .unwrap();
        assert!(!base.as_object().unwrap().contains_key("livePage"));

        let resolved: serde_json::Value = serde_json::from_str(
            &encode_request(
                "request-2",
                1,
                &RequestKind::ResolveCommand {
                    command_id: "command-1".into(),
                    claim_token: "claim-1".into(),
                    status: SessionCommandStatus::Applied,
                    resolution: None,
                },
            )
            .unwrap(),
        )
        .unwrap();
        assert!(!resolved.as_object().unwrap().contains_key("resolution"));
    }
}
