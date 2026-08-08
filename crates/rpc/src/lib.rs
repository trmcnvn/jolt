//! jolt-rpc — generic request/reply and streaming RPC over WebSocket and in-memory
//! transports. Device-to-device relay transport lives in `jolt-relay`.
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
use bytes::{BufMut, Bytes, BytesMut};
use futures::stream::BoxStream;
use serde::{Deserialize, Serialize};

mod client;
mod server;
pub mod terminal_wire;

pub use client::{RpcClient, connect_ws};
pub use server::{serve_connection, serve_ws_listener};

/// RPC method names — single source of truth for both ends.
/// Full surface: docs/rpc.md.
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
    Binary(Bytes),
}

/// Application-level frame limit, below the WebSocket library's broad default.
pub const MAX_WIRE_FRAME_BYTES: usize = 8 * 1024 * 1024;
/// Binary stream/request payload limit. Current terminal and attachment chunks are at most 64 KiB.
pub const MAX_BINARY_PAYLOAD_BYTES: usize = 1024 * 1024;

const MAX_METHOD_BYTES: usize = 256;
const MAX_BINARY_PARAMS_BYTES: usize = 64 * 1024;
const BINARY_MAGIC: &[u8; 4] = b"JRPB";
const BINARY_VERSION: u8 = 1;
const BINARY_STREAM_ITEM: u8 = 1;
const BINARY_UNARY_REQUEST: u8 = 2;
const BINARY_HEADER_LEN: usize = 14;
const BINARY_REQUEST_HEADER_LEN: usize = 20;

pub(crate) struct BinaryRequest {
    pub id: u64,
    pub method: String,
    pub params: serde_json::Value,
    pub payload: Bytes,
}

pub(crate) fn encode_binary_stream_item(id: u64, payload: Bytes) -> Result<Bytes, RpcError> {
    validate_binary_payload(payload.len())?;
    let mut frame = BytesMut::with_capacity(BINARY_HEADER_LEN + payload.len());
    frame.put_slice(BINARY_MAGIC);
    frame.put_slice(&[BINARY_VERSION, BINARY_STREAM_ITEM]);
    frame.put_u64_le(id);
    frame.put_slice(&payload);
    Ok(frame.freeze())
}

pub(crate) fn decode_binary_stream_item(bytes: Bytes) -> Result<(u64, Bytes), RpcError> {
    validate_binary_prefix(&bytes, BINARY_STREAM_ITEM)?;
    validate_binary_payload(bytes.len().saturating_sub(BINARY_HEADER_LEN))?;
    let id = u64::from_le_bytes(
        binary_payload(&bytes, 6, 8, "RPC stream id")?
            .try_into()
            .map_err(|_| RpcError::Transport("binary RPC frame: invalid stream id".into()))?,
    );
    Ok((id, bytes.slice(BINARY_HEADER_LEN..)))
}

pub(crate) fn encode_binary_request(
    id: u64,
    method: &str,
    params: &serde_json::Value,
    payload: Bytes,
) -> Result<Bytes, RpcError> {
    if method.is_empty() || method.len() > MAX_METHOD_BYTES {
        return Err(RpcError::BadParams(
            "invalid binary RPC method length".into(),
        ));
    }
    validate_binary_payload(payload.len())?;
    let params = serde_json::to_vec(params)
        .map_err(|error| RpcError::Transport(format!("serialize binary params: {error}")))?;
    if params.len() > MAX_BINARY_PARAMS_BYTES {
        return Err(RpcError::BadParams(
            "binary RPC params are too large".into(),
        ));
    }
    let method_len = u16::try_from(method.len())
        .map_err(|_| RpcError::BadParams("binary RPC method is too long".into()))?;
    let params_len = u32::try_from(params.len())
        .map_err(|_| RpcError::BadParams("binary RPC params are too large".into()))?;
    let mut frame = BytesMut::with_capacity(
        BINARY_REQUEST_HEADER_LEN + method.len() + params.len() + payload.len(),
    );
    frame.put_slice(BINARY_MAGIC);
    frame.put_slice(&[BINARY_VERSION, BINARY_UNARY_REQUEST]);
    frame.put_u64_le(id);
    frame.put_u16_le(method_len);
    frame.put_u32_le(params_len);
    frame.put_slice(method.as_bytes());
    frame.put_slice(&params);
    frame.put_slice(&payload);
    Ok(frame.freeze())
}

pub(crate) fn decode_binary_request(bytes: Bytes) -> Result<BinaryRequest, RpcError> {
    validate_binary_prefix(&bytes, BINARY_UNARY_REQUEST)?;
    let method_len = u16::from_le_bytes(
        binary_payload(&bytes, BINARY_HEADER_LEN, 2, "method length")?
            .try_into()
            .map_err(|_| RpcError::Transport("binary RPC frame: invalid method length".into()))?,
    ) as usize;
    let params_len = u32::from_le_bytes(
        binary_payload(&bytes, BINARY_HEADER_LEN + 2, 4, "params length")?
            .try_into()
            .map_err(|_| RpcError::Transport("binary RPC frame: invalid params length".into()))?,
    ) as usize;
    if method_len == 0 || method_len > MAX_METHOD_BYTES || params_len > MAX_BINARY_PARAMS_BYTES {
        return Err(RpcError::Transport(
            "binary RPC frame: invalid metadata length".into(),
        ));
    }
    let method_start = BINARY_REQUEST_HEADER_LEN;
    let params_start = method_start
        .checked_add(method_len)
        .ok_or_else(|| RpcError::Transport("binary RPC frame: length overflow".into()))?;
    let payload_start = params_start
        .checked_add(params_len)
        .ok_or_else(|| RpcError::Transport("binary RPC frame: length overflow".into()))?;
    let method = std::str::from_utf8(binary_payload(&bytes, method_start, method_len, "method")?)
        .map_err(|_| RpcError::Transport("binary RPC frame: method is not UTF-8".into()))?
        .to_owned();
    let params =
        serde_json::from_slice(binary_payload(&bytes, params_start, params_len, "params")?)
            .map_err(|error| {
                RpcError::Transport(format!("binary RPC frame: bad params JSON: {error}"))
            })?;
    validate_binary_payload(bytes.len().saturating_sub(payload_start))?;
    let id = u64::from_le_bytes(
        binary_payload(&bytes, 6, 8, "RPC request id")?
            .try_into()
            .map_err(|_| RpcError::Transport("binary RPC frame: invalid request id".into()))?,
    );
    Ok(BinaryRequest {
        id,
        method,
        params,
        payload: bytes.slice(payload_start..),
    })
}

fn validate_binary_prefix(bytes: &[u8], opcode: u8) -> Result<(), RpcError> {
    if bytes.get(..4) != Some(BINARY_MAGIC) {
        return Err(RpcError::Transport("binary RPC frame: bad magic".into()));
    }
    if bytes.get(4) != Some(&BINARY_VERSION) {
        return Err(RpcError::Transport(
            "binary RPC frame: unsupported version".into(),
        ));
    }
    if bytes.get(5) != Some(&opcode) {
        return Err(RpcError::Transport(
            "binary RPC frame: unexpected opcode".into(),
        ));
    }
    Ok(())
}

fn validate_binary_payload(len: usize) -> Result<(), RpcError> {
    if len > MAX_BINARY_PAYLOAD_BYTES {
        return Err(RpcError::Transport(
            "binary RPC payload is too large".into(),
        ));
    }
    Ok(())
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
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_present_json_value"
    )]
    pub ok: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub err: Option<String>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_present_json_value"
    )]
    pub item: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub done: bool,
}

fn deserialize_present_json_value<'de, D>(
    deserializer: D,
) -> Result<Option<serde_json::Value>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    serde_json::Value::deserialize(deserializer).map(Some)
}

impl ClientFrame {
    pub(crate) fn validate(&self) -> Result<(), RpcError> {
        let valid_method = self
            .method
            .as_deref()
            .is_some_and(|method| !method.is_empty() && method.len() <= MAX_METHOD_BYTES);
        if valid_method == self.cancel {
            return Err(RpcError::Transport(
                "client RPC frame must contain exactly one method or cancellation".into(),
            ));
        }
        Ok(())
    }
}

impl ServerFrame {
    pub(crate) fn validate(&self) -> Result<(), RpcError> {
        let variants = usize::from(self.ok.is_some())
            + usize::from(self.err.is_some())
            + usize::from(self.item.is_some())
            + usize::from(self.done);
        if variants != 1 {
            return Err(RpcError::Transport(
                "server RPC frame must contain exactly one result variant".into(),
            ));
        }
        Ok(())
    }
}

/// What a service returns for one invocation.
pub enum RpcReply {
    /// Unary response — sent as `{id, ok}`.
    Value(serde_json::Value),
    /// Stream — each item sent as `{id, item}`, then `{id, done: true}` when it ends.
    Stream(BoxStream<'static, serde_json::Value>),
    /// Binary stream — each item is one binary frame, followed by JSON `{id, done}`.
    BinaryStream(BoxStream<'static, Bytes>),
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

    /// Handle a unary request whose bulk body is carried outside JSON.
    async fn handle_binary(
        &self,
        method: &str,
        _params: serde_json::Value,
        _payload: Bytes,
    ) -> Result<RpcReply, RpcError> {
        Err(RpcError::UnknownMethod(method.to_owned()))
    }
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
                    futures::stream::iter([
                        Bytes::from_static(&[0, 1, 0x80, 0xff]),
                        Bytes::from_static(b"second"),
                    ])
                    .boxed(),
                )),
                "Boom" => Err(RpcError::Failed("boom".into())),
                other => Err(RpcError::UnknownMethod(other.into())),
            }
        }

        async fn handle_binary(
            &self,
            method: &str,
            params: serde_json::Value,
            payload: Bytes,
        ) -> Result<RpcReply, RpcError> {
            if method != "BinaryEcho" {
                return Err(RpcError::UnknownMethod(method.into()));
            }
            RpcReply::value(&serde_json::json!({
                "params": params,
                "bytes": payload.len(),
                "first": payload.first().copied(),
            }))
        }
    }

    #[test]
    fn server_frame_preserves_present_null_variants() {
        let unary: ServerFrame = serde_json::from_str(r#"{"id":1,"ok":null}"#).unwrap();
        assert_eq!(unary.ok, Some(serde_json::Value::Null));
        unary.validate().unwrap();

        let stream: ServerFrame = serde_json::from_str(r#"{"id":2,"item":null}"#).unwrap();
        assert_eq!(stream.item, Some(serde_json::Value::Null));
        stream.validate().unwrap();

        let missing: ServerFrame = serde_json::from_str(r#"{"id":3}"#).unwrap();
        assert!(missing.validate().is_err());

        let contradictory: ServerFrame =
            serde_json::from_str(r#"{"id":4,"ok":null,"done":true}"#).unwrap();
        assert!(contradictory.validate().is_err());
    }

    #[tokio::test]
    async fn memory_call_stream_and_error() {
        let client = memory_client(Arc::new(TestService));

        let echoed = client
            .call("Echo", serde_json::json!({"x": 1}))
            .await
            .unwrap();
        assert_eq!(echoed, serde_json::json!({"x": 1}));
        let binary = client
            .call_binary(
                "BinaryEcho",
                serde_json::json!({"name": "chunk"}),
                Bytes::from_static(&[9, 8, 7]),
            )
            .await
            .unwrap();
        assert_eq!(
            binary,
            serde_json::json!({"params": {"name": "chunk"}, "bytes": 3, "first": 9})
        );

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
        assert_eq!(
            binary.recv().await,
            Some(Bytes::from_static(&[0, 1, 0x80, 0xff]))
        );
        assert_eq!(binary.recv().await, Some(Bytes::from_static(b"second")));
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
        let binary = client
            .call_binary(
                "BinaryEcho",
                serde_json::json!({"name": "chunk"}),
                Bytes::from_static(&[9, 8, 7]),
            )
            .await
            .unwrap();
        assert_eq!(
            binary,
            serde_json::json!({"params": {"name": "chunk"}, "bytes": 3, "first": 9})
        );

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
        assert_eq!(
            binary.recv().await,
            Some(Bytes::from_static(&[0, 1, 0x80, 0xff]))
        );
        assert_eq!(binary.recv().await, Some(Bytes::from_static(b"second")));
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
