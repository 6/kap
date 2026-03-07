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
type WsStream = WebSocketStream<hyper_util::rt::TokioIo<hyper::upgrade::Upgraded>>;

/// Handle WebSocket upgrade requests.
pub async fn handle(req: Request<Incoming>, state: Arc<RemoteState>) -> Result<Response<Body>> {
    let path = req.uri().path().to_string();

    match path.as_str() {
        "/ws/logs" => ws_upgrade(req, stream_logs),
        p if p.starts_with("/ws/agent/") => {
            let session_id = p["/ws/agent/".len()..].to_string();
            ws_upgrade(req, move |ws| stream_agent(ws, session_id))
        }
        _ => {
            let _ = state;
            Ok(Response::builder()
                .status(StatusCode::NOT_FOUND)
                .body(Full::new(Bytes::from("not found")))
                .unwrap())
        }
    }
}

/// Generic WebSocket upgrade that spawns a handler future.
fn ws_upgrade<F, Fut>(req: Request<Incoming>, handler: F) -> Result<Response<Body>>
where
    F: FnOnce(WsStream) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = Result<()>> + Send,
{
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

    tokio::task::spawn(async move {
        match hyper::upgrade::on(req).await {
            Ok(upgraded) => {
                let io = hyper_util::rt::TokioIo::new(upgraded);
                let ws = WebSocketStream::from_raw_socket(io, Role::Server, None).await;

                if let Err(e) = handler(ws).await {
                    eprintln!("[remote] ws handler error: {e}");
                }
            }
            Err(e) => {
                eprintln!("[remote] ws upgrade failed: {e}");
            }
        }
    });

    Ok(Response::builder()
        .status(StatusCode::SWITCHING_PROTOCOLS)
        .header("Upgrade", "websocket")
        .header("Connection", "Upgrade")
        .header("Sec-WebSocket-Accept", accept_key)
        .body(Full::new(Bytes::new()))
        .unwrap())
}

async fn stream_logs(mut ws: WsStream) -> Result<()> {
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
            break;
        }
    }

    let _ = child.kill().await;
    Ok(())
}

async fn stream_agent(mut ws: WsStream, session_id: String) -> Result<()> {
    let (app, _sidecar) = containers::find_containers()?;

    // Find the session file path
    let path_output = containers::exec_in(
        &app,
        &[
            "sh",
            "-c",
            &format!(
                "find /home /root -name '{session_id}.jsonl' -path '*/.claude/projects/*' 2>/dev/null | head -1"
            ),
        ],
    )
    .ok_or_else(|| anyhow::anyhow!("session {session_id} not found"))?;

    let session_path = path_output.trim().to_string();
    if session_path.is_empty() {
        anyhow::bail!("session {session_id} not found");
    }

    let mut child = containers::exec_stream(
        &app,
        &[
            "tail",
            "-f",
            "-n",
            "50", // send last 50 lines as catch-up
            &session_path,
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
        // Parse and filter like the agent module does, then send as JSON
        let events = crate::remote::agent::parse_session_events(&line);
        for event in events {
            if let Ok(json) = serde_json::to_string(&event)
                && ws.send(Message::Text(json.into())).await.is_err()
            {
                let _ = child.kill().await;
                return Ok(());
            }
        }
    }

    let _ = child.kill().await;
    Ok(())
}
