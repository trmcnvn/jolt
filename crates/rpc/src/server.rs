//! Server-side JSON control and binary stream dispatch plus WebSocket acceptor.

use std::collections::HashMap;
use std::sync::Arc;

use futures::{SinkExt, StreamExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message as WsMessage;

use crate::{
    ClientFrame, MAX_WIRE_FRAME_BYTES, RpcError, RpcReply, RpcService, ServerFrame, WireFrame,
    decode_binary_request, encode_binary_stream_item,
};

/// Serve one connection: read client frames from `inbound`, write ordered text or binary frames to `out`.
/// Returns when `inbound` closes; all in-flight request tasks are aborted on exit.
pub async fn serve_connection(
    service: Arc<dyn RpcService>,
    out: mpsc::Sender<WireFrame>,
    mut inbound: mpsc::Receiver<WireFrame>,
) {
    let mut running: HashMap<u64, tokio::task::AbortHandle> = HashMap::new();
    while let Some(frame) = inbound.recv().await {
        match frame {
            WireFrame::Text(payload) => {
                if payload.len() > MAX_WIRE_FRAME_BYTES {
                    tracing::warn!(
                        bytes = payload.len(),
                        "rpc: dropping oversized client frame"
                    );
                    continue;
                }
                // ndjson: a transport may batch several frames per message.
                for line in payload.lines() {
                    let line = line.trim();
                    if line.is_empty() {
                        continue;
                    }
                    let frame: ClientFrame = match serde_json::from_str(line) {
                        Ok(frame) => frame,
                        Err(err) => {
                            tracing::warn!(error = %err, "rpc: dropping malformed client frame");
                            continue;
                        }
                    };
                    if let Err(err) = frame.validate() {
                        tracing::warn!(error = %err, "rpc: dropping invalid client frame");
                        continue;
                    }
                    running.retain(|_, task| !task.is_finished());
                    if frame.cancel {
                        if let Some(task) = running.remove(&frame.id) {
                            task.abort();
                        }
                        continue;
                    }
                    let method = frame.method.expect("validated method frame has a method");
                    let task = tokio::spawn(handle_request(
                        service.clone(),
                        out.clone(),
                        frame.id,
                        method,
                        frame.params,
                        None,
                    ));
                    running.insert(frame.id, task.abort_handle());
                }
            }
            WireFrame::Binary(bytes) => {
                if bytes.len() > MAX_WIRE_FRAME_BYTES {
                    tracing::warn!(
                        bytes = bytes.len(),
                        "rpc: dropping oversized binary request"
                    );
                    continue;
                }
                let request = match decode_binary_request(bytes) {
                    Ok(request) => request,
                    Err(err) => {
                        tracing::warn!(error = %err, "rpc: dropping malformed binary request");
                        continue;
                    }
                };
                running.retain(|_, task| !task.is_finished());
                let task = tokio::spawn(handle_request(
                    service.clone(),
                    out.clone(),
                    request.id,
                    request.method,
                    request.params,
                    Some(request.payload),
                ));
                running.insert(request.id, task.abort_handle());
            }
        }
    }
    for (_, task) in running {
        task.abort();
    }
}

async fn handle_request(
    service: Arc<dyn RpcService>,
    out: mpsc::Sender<WireFrame>,
    id: u64,
    method: String,
    params: serde_json::Value,
    binary: Option<bytes::Bytes>,
) {
    let send = |frame: ServerFrame| {
        let out = out.clone();
        async move {
            frame.validate()?;
            match serde_json::to_string(&frame) {
                Ok(json) => out
                    .send(WireFrame::Text(json))
                    .await
                    .map_err(|_| RpcError::Closed),
                Err(err) => {
                    tracing::error!(error = %err, "rpc: failed to serialize server frame");
                    Err(RpcError::Closed)
                }
            }
        }
    };
    let reply = match binary {
        Some(payload) => service.handle_binary(&method, params, payload).await,
        None => service.handle(&method, params).await,
    };
    match reply {
        Ok(RpcReply::Value(value)) => {
            let _ = send(ServerFrame {
                id,
                ok: Some(value),
                ..Default::default()
            })
            .await;
        }
        Ok(RpcReply::Stream(mut stream)) => {
            while let Some(item) = stream.next().await {
                if send(ServerFrame {
                    id,
                    item: Some(item),
                    ..Default::default()
                })
                .await
                .is_err()
                {
                    return; // connection gone
                }
            }
            let _ = send(ServerFrame {
                id,
                done: true,
                ..Default::default()
            })
            .await;
        }
        Ok(RpcReply::BinaryStream(mut stream)) => {
            while let Some(item) = stream.next().await {
                let encoded = match encode_binary_stream_item(id, item) {
                    Ok(encoded) => encoded,
                    Err(err) => {
                        tracing::warn!(id, error = %err, "rpc: dropping invalid binary stream item");
                        return;
                    }
                };
                if out.send(WireFrame::Binary(encoded)).await.is_err() {
                    return;
                }
            }
            let _ = send(ServerFrame {
                id,
                done: true,
                ..Default::default()
            })
            .await;
        }
        Err(err) => {
            let _ = send(ServerFrame {
                id,
                err: Some(err.to_string()),
                ..Default::default()
            })
            .await;
        }
    }
}

/// Accept WebSocket connections forever, serving each with `service`.
pub async fn serve_ws_listener(listener: TcpListener, service: Arc<dyn RpcService>) {
    loop {
        match listener.accept().await {
            Ok((stream, peer)) => {
                tracing::debug!(%peer, "rpc: connection accepted");
                tokio::spawn(serve_ws_socket(stream, service.clone()));
            }
            Err(err) => {
                tracing::warn!(error = %err, "rpc: accept failed");
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
        }
    }
}

async fn serve_ws_socket(stream: TcpStream, service: Arc<dyn RpcService>) {
    let config = tokio_tungstenite::tungstenite::protocol::WebSocketConfig::default()
        .max_message_size(Some(MAX_WIRE_FRAME_BYTES))
        .max_frame_size(Some(MAX_WIRE_FRAME_BYTES));
    let ws = match tokio_tungstenite::accept_async_with_config(stream, Some(config)).await {
        Ok(ws) => ws,
        Err(err) => {
            tracing::warn!(error = %err, "rpc: websocket handshake failed");
            return;
        }
    };
    let (mut sink, mut ws_stream) = ws.split();
    let (out_tx, mut out_rx) = mpsc::channel::<WireFrame>(256);
    let (in_tx, in_rx) = mpsc::channel::<WireFrame>(256);

    // Pump: socket <-> string channels. Ends when either side closes.
    let pump = tokio::spawn(async move {
        loop {
            tokio::select! {
                frame = out_rx.recv() => match frame {
                    Some(WireFrame::Text(text)) => {
                        if sink.send(WsMessage::Text(text.into())).await.is_err() {
                            break;
                        }
                    }
                    Some(WireFrame::Binary(bytes)) => {
                        if sink.send(WsMessage::Binary(bytes)).await.is_err() {
                            break;
                        }
                    }
                    None => {
                        let _ = sink.send(WsMessage::Close(None)).await;
                        break;
                    }
                },
                message = ws_stream.next() => match message {
                    Some(Ok(WsMessage::Text(text))) => {
                        if in_tx.send(WireFrame::Text(text.to_string())).await.is_err() {
                            break;
                        }
                    }
                    Some(Ok(WsMessage::Binary(bytes))) => {
                        if in_tx.send(WireFrame::Binary(bytes)).await.is_err() {
                            break;
                        }
                    }
                    Some(Ok(WsMessage::Close(_))) | Some(Err(_)) | None => break,
                    Some(Ok(_)) => {} // ping/pong
                },
            }
        }
    });

    serve_connection(service, out_tx, in_rx).await;
    pump.abort();
}
