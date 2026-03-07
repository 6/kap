pub mod agent;
pub mod api;
pub mod auth;
pub mod containers;
pub mod web;
pub mod ws;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;
use bytes::Bytes;
use http_body_util::Full;
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use tokio::net::TcpListener;

type Body = Full<Bytes>;

pub struct RemoteState {
    pub data_dir: PathBuf,
}

fn pid_file(data_dir: &Path) -> PathBuf {
    data_dir.join("pid")
}

fn read_pid(data_dir: &Path) -> Option<u32> {
    std::fs::read_to_string(pid_file(data_dir))
        .ok()
        .and_then(|s| s.trim().parse().ok())
}

fn is_process_running(pid: u32) -> bool {
    // signal 0 checks if process exists without sending a signal
    unsafe { libc::kill(pid as i32, 0) == 0 }
}

/// Start the remote access daemon. Idempotent — if already running, prints QR and exits.
pub async fn start(listen: &str, data_dir: PathBuf) -> Result<()> {
    // Check if already running
    if let Some(pid) = read_pid(&data_dir)
        && is_process_running(pid)
    {
        eprintln!("[remote] already running (pid {pid})");
        eprintln!();
        let port: u16 = listen
            .rsplit(':')
            .next()
            .and_then(|p| p.parse().ok())
            .unwrap_or(19420);
        print_pair(&data_dir, port)?;
        return Ok(());
    }

    // Write our PID
    let pid = std::process::id();
    std::fs::write(pid_file(&data_dir), pid.to_string())?;

    let result = run(listen, data_dir.clone()).await;

    // Clean up PID file on exit
    let _ = std::fs::remove_file(pid_file(&data_dir));
    result
}

/// Stop the remote access daemon.
pub fn stop() -> Result<()> {
    let data_dir = auth::data_dir();
    match read_pid(&data_dir) {
        Some(pid) if is_process_running(pid) => {
            eprintln!("[remote] stopping daemon (pid {pid})");
            unsafe {
                libc::kill(pid as i32, libc::SIGTERM);
            }
            let _ = std::fs::remove_file(pid_file(&data_dir));
            Ok(())
        }
        Some(_) => {
            // Stale PID file
            let _ = std::fs::remove_file(pid_file(&data_dir));
            eprintln!("[remote] not running (cleaned up stale pid file)");
            Ok(())
        }
        None => {
            eprintln!("[remote] not running");
            Ok(())
        }
    }
}

/// Start the remote access HTTP daemon.
async fn run(listen: &str, data_dir: PathBuf) -> Result<()> {
    let _pairing_token = auth::load_or_generate_pairing_token(&data_dir)?;

    let state = Arc::new(RemoteState {
        data_dir: data_dir.clone(),
    });

    let listener = TcpListener::bind(listen).await?;
    let local_addr = listener.local_addr()?;
    let port = local_addr.port();

    eprintln!("[remote] listening on http://{local_addr}");
    print_pair(&data_dir, port)?;

    loop {
        let (stream, addr) = listener.accept().await?;
        let state = state.clone();

        tokio::spawn(async move {
            let io = TokioIo::new(stream);
            let state = state.clone();
            let service = service_fn(move |req| {
                let state = state.clone();
                async move { handle_request(req, state).await }
            });

            #[allow(clippy::collapsible_if)]
            if let Err(e) = http1::Builder::new()
                .serve_connection(io, service)
                .with_upgrades()
                .await
            {
                if !e.to_string().contains("error shutting down connection") {
                    eprintln!("[remote] connection error from {addr}: {e}");
                }
            }
        });
    }
}

async fn handle_request(
    req: Request<Incoming>,
    state: Arc<RemoteState>,
) -> Result<Response<Body>, hyper::Error> {
    match route(req, state).await {
        Ok(resp) => Ok(resp),
        Err(e) => {
            eprintln!("[remote] handler error: {e}");
            Ok(Response::builder()
                .status(StatusCode::INTERNAL_SERVER_ERROR)
                .header("Content-Type", "application/json")
                .body(Full::new(Bytes::from(format!("{{\"error\":\"{e}\"}}"))))
                .unwrap())
        }
    }
}

async fn route(req: Request<Incoming>, state: Arc<RemoteState>) -> Result<Response<Body>> {
    let path = req.uri().path().to_string();

    // Serve the web UI at / (no auth required — the app handles auth client-side)
    if path == "/" || path == "/index.html" {
        return Ok(web::serve_app());
    }

    // Extract token from Authorization header or ?token= query param
    // (WebSocket API can't set headers, so we support query param for /ws/ routes)
    let token = extract_bearer_token(&req).map(String::from).or_else(|| {
        req.uri().query().and_then(|q| {
            q.split('&')
                .find_map(|p| p.strip_prefix("token=").map(String::from))
        })
    });
    let auth_result = match token.as_deref() {
        Some(t) => auth::validate_token(&state.data_dir, t),
        None => None,
    };

    // /api/pair only accepts the pairing token
    if path == "/api/pair" {
        match auth_result.as_deref() {
            Some("pairing") => return api::handle(req, &state).await,
            _ => {
                return Ok(Response::builder()
                    .status(StatusCode::UNAUTHORIZED)
                    .header("Content-Type", "application/json")
                    .body(Full::new(Bytes::from(
                        r#"{"error":"invalid or missing pairing token"}"#,
                    )))
                    .unwrap());
            }
        }
    }

    // All other API/WS routes require a valid token
    if auth_result.is_none() {
        return Ok(Response::builder()
            .status(StatusCode::UNAUTHORIZED)
            .header("Content-Type", "application/json")
            .body(Full::new(Bytes::from(
                r#"{"error":"invalid or missing authorization token"}"#,
            )))
            .unwrap());
    }

    if path.starts_with("/ws/") {
        ws::handle(req, state).await
    } else if path.starts_with("/api/") {
        api::handle(req, &state).await
    } else {
        Ok(Response::builder()
            .status(StatusCode::NOT_FOUND)
            .body(Full::new(Bytes::from("not found")))
            .unwrap())
    }
}

fn extract_bearer_token(req: &Request<Incoming>) -> Option<&str> {
    req.headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
}

/// Print the pairing QR code.
pub fn print_pair(data_dir: &Path, port: u16) -> Result<()> {
    let token = auth::load_or_generate_pairing_token(data_dir)?;
    let ip = auth::local_ip().unwrap_or_else(|| "localhost".to_string());
    let url = format!("http://{ip}:{port}/#{token}");

    auth::print_qr(&url);
    Ok(())
}

/// List paired devices.
pub fn list_devices(data_dir: &Path) {
    let devices = auth::load_devices(data_dir);
    if devices.is_empty() {
        println!("No paired devices.");
        println!("Run `devg remote pair` to get the pairing QR code.");
        return;
    }
    println!("{:<14} {:<20} {:<26}", "ID", "NAME", "PAIRED");
    for d in &devices {
        println!("{:<14} {:<20} {:<26}", d.id, d.name, d.paired_at);
    }
}

/// Revoke a paired device.
pub fn revoke(data_dir: &Path, device_id: &str) -> Result<()> {
    let removed = auth::revoke_device(data_dir, device_id)?;
    if removed {
        println!("Revoked device {device_id}");
    } else {
        println!("No device found with ID {device_id}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn extract_bearer(req: &Request<Full<Bytes>>) -> Option<String> {
        req.headers()
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .map(String::from)
    }

    #[test]
    fn extract_bearer_works() {
        let req = Request::builder()
            .header("Authorization", "Bearer my-token-123")
            .body(Full::new(Bytes::new()))
            .unwrap();
        assert_eq!(extract_bearer(&req).as_deref(), Some("my-token-123"));
    }

    #[test]
    fn extract_bearer_missing_header() {
        let req = Request::builder().body(Full::new(Bytes::new())).unwrap();
        assert!(extract_bearer(&req).is_none());
    }

    #[test]
    fn extract_bearer_wrong_scheme() {
        let req = Request::builder()
            .header("Authorization", "Basic abc123")
            .body(Full::new(Bytes::new()))
            .unwrap();
        assert!(extract_bearer(&req).is_none());
    }

    #[test]
    fn extract_bearer_empty_token() {
        let req = Request::builder()
            .header("Authorization", "Bearer ")
            .body(Full::new(Bytes::new()))
            .unwrap();
        assert_eq!(extract_bearer(&req).as_deref(), Some(""));
    }
}
