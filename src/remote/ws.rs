use std::sync::Arc;

use anyhow::Result;
use bytes::Bytes;
use futures_util::SinkExt;
use http_body_util::Full;
use hyper::body::Incoming;
use hyper::{Request, Response, StatusCode};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::handshake::derive_accept_key;
use tokio_tungstenite::tungstenite::protocol::Role;

use super::RemoteState;
use crate::remote::containers;

type Body = Full<Bytes>;

/// Handle WebSocket upgrade requests.
pub async fn handle(req: Request<Incoming>, state: Arc<RemoteState>) -> Result<Response<Body>> {
    let path = req.uri().path().to_string();

    match path.as_str() {
        "/ws/logs" => handle_ws_upgrade(req, state).await,
        _ => Ok(Response::builder()
            .status(StatusCode::NOT_FOUND)
            .body(Full::new(Bytes::from("not found")))
            .unwrap()),
    }
}

async fn handle_ws_upgrade(
    req: Request<Incoming>,
    _state: Arc<RemoteState>,
) -> Result<Response<Body>> {
    // Extract the Sec-WebSocket-Key for the accept response
    let ws_key = match req.headers().get("sec-websocket-key") {
        Some(key) => key.clone(),
        None => {
            return Ok(Response::builder()
                .status(StatusCode::BAD_REQUEST)
                .body(Full::new(Bytes::from("missing Sec-WebSocket-Key")))
                .unwrap());
        }
    };

    let accept_key = derive_accept_key(ws_key.as_bytes());

    // Spawn the WebSocket handler after upgrade completes
    tokio::task::spawn(async move {
        match hyper::upgrade::on(req).await {
            Ok(upgraded) => {
                let io = hyper_util::rt::TokioIo::new(upgraded);
                let ws = WebSocketStream::from_raw_socket(io, Role::Server, None).await;

                if let Err(e) = stream_logs(ws).await {
                    eprintln!("[remote] ws/logs error: {e}");
                }
            }
            Err(e) => {
                eprintln!("[remote] ws upgrade failed: {e}");
            }
        }
    });

    // Return the 101 Switching Protocols response
    Ok(Response::builder()
        .status(StatusCode::SWITCHING_PROTOCOLS)
        .header("Upgrade", "websocket")
        .header("Connection", "Upgrade")
        .header("Sec-WebSocket-Accept", accept_key)
        .body(Full::new(Bytes::new()))
        .unwrap())
}

async fn stream_logs(
    mut ws: WebSocketStream<hyper_util::rt::TokioIo<hyper::upgrade::Upgraded>>,
) -> Result<()> {
    let (_app, sidecar) = containers::find_containers()?;

    let mut child = containers::exec_stream(
        &sidecar,
        &[
            "tail",
            "-f",
            "-n",
            "20", // send last 20 lines as catch-up
            "/var/log/devg/proxy.jsonl",
        ],
    )
    .await?;

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow::anyhow!("no stdout from tail"))?;

    let reader = BufReader::new(stdout);
    let mut lines = reader.lines();

    while let Ok(Some(line)) = lines.next_line().await {
        if ws.send(Message::Text(line.into())).await.is_err() {
            break; // client disconnected
        }
    }

    let _ = child.kill().await;
    Ok(())
}
