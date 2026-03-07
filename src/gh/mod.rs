pub mod filter;

use std::sync::Arc;

use anyhow::Result;
use base64::Engine;
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;
use tokio::net::TcpListener;

use crate::config::GhConfig;
use crate::proxy::log::{ProxyLogEntry, ProxyLogger};
use filter::GhCommandFilter;

struct GhState {
    filter: GhCommandFilter,
    logger: ProxyLogger,
}

pub async fn run(config: &GhConfig, logger: ProxyLogger) -> Result<()> {
    let state = Arc::new(GhState {
        filter: GhCommandFilter::new(&config.allow),
        logger,
    });

    let listener = TcpListener::bind(&config.listen).await?;
    eprintln!("[gh] listening on {}", config.listen);

    loop {
        let (stream, _addr) = listener.accept().await?;
        let state = state.clone();

        tokio::spawn(async move {
            let io = TokioIo::new(stream);
            let service = service_fn(move |req| {
                let state = state.clone();
                async move { handle_request(req, &state).await }
            });

            if let Err(e) = http1::Builder::new().serve_connection(io, service).await
                && !e.to_string().contains("error shutting down connection")
            {
                eprintln!("[gh] connection error: {e}");
            }
        });
    }
}

async fn handle_request(
    req: Request<Incoming>,
    state: &GhState,
) -> Result<Response<Full<Bytes>>, hyper::Error> {
    if req.method() != hyper::Method::POST {
        return Ok(error_response(405, "only POST is supported"));
    }

    let body = req.into_body().collect().await?.to_bytes();

    let parsed: serde_json::Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(_) => return Ok(error_response(400, "invalid JSON")),
    };

    let args: Vec<String> = match parsed["args"].as_array() {
        Some(arr) => arr
            .iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect(),
        None => return Ok(error_response(400, "missing \"args\" array")),
    };

    if args.is_empty() {
        return Ok(error_response(400, "empty args"));
    }

    let cmd_display = args.join(" ");

    if !state.filter.is_allowed(&args) {
        let entry = ProxyLogEntry::new("gh", "denied", &cmd_display);
        let _ = state.logger.log(&entry).await;
        eprintln!("[gh] DENIED: {cmd_display}");
        return Ok(error_response(
            403,
            &format!("command denied: {cmd_display}"),
        ));
    }

    let entry = ProxyLogEntry::new("gh", "allowed", &cmd_display);
    let _ = state.logger.log(&entry).await;

    // Spawn gh with GH_TOKEN from sidecar env
    let output = tokio::process::Command::new("gh")
        .args(&args)
        .env("GH_PROMPT_DISABLED", "1")
        .env("GH_NO_UPDATE_NOTIFIER", "1")
        .env("PAGER", "cat")
        .env("NO_COLOR", "1")
        .output()
        .await;

    match output {
        Ok(output) => {
            let stdout = output.stdout;
            let stderr = output.stderr;
            let exit_code = output.status.code().unwrap_or(1);

            let stderr_b64 = if stderr.is_empty() {
                String::new()
            } else {
                base64::engine::general_purpose::STANDARD.encode(&stderr)
            };

            let mut builder = Response::builder()
                .status(200)
                .header("X-Exit-Code", exit_code.to_string());

            if !stderr_b64.is_empty() {
                builder = builder.header("X-Stderr", stderr_b64);
            }

            Ok(builder.body(Full::new(Bytes::from(stdout))).unwrap())
        }
        Err(e) => {
            eprintln!("[gh] failed to spawn gh: {e}");
            Ok(error_response(500, &format!("failed to run gh: {e}")))
        }
    }
}

fn error_response(status: u16, message: &str) -> Response<Full<Bytes>> {
    let json = serde_json::json!({"error": message});
    Response::builder()
        .status(status)
        .header("Content-Type", "application/json")
        .header("X-Exit-Code", "1")
        .body(Full::new(Bytes::from(serde_json::to_vec(&json).unwrap())))
        .unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn start_gh_proxy(allow: &[&str]) -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);

        let state = Arc::new(GhState {
            filter: GhCommandFilter::new(&allow.iter().map(|s| s.to_string()).collect::<Vec<_>>()),
            logger: ProxyLogger::new("/dev/null"),
        });

        tokio::spawn(async move {
            let listener = TcpListener::bind(format!("127.0.0.1:{port}"))
                .await
                .unwrap();
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    continue;
                };
                let state = state.clone();
                tokio::spawn(async move {
                    let io = TokioIo::new(stream);
                    let service = service_fn(move |req| {
                        let state = state.clone();
                        async move { handle_request(req, &state).await }
                    });
                    let _ = http1::Builder::new().serve_connection(io, service).await;
                });
            }
        });

        for _ in 0..100 {
            if tokio::net::TcpStream::connect(format!("127.0.0.1:{port}"))
                .await
                .is_ok()
            {
                return port;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("gh proxy did not start");
    }

    async fn post(port: u16, args: &[&str]) -> (u16, String, String) {
        let client = reqwest::Client::new();
        let resp = client
            .post(format!("http://127.0.0.1:{port}/"))
            .json(&serde_json::json!({"args": args}))
            .send()
            .await
            .unwrap();

        let status = resp.status().as_u16();
        let exit_code: String = resp
            .headers()
            .get("x-exit-code")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("1")
            .to_string();
        let body = resp.text().await.unwrap();
        (status, exit_code, body)
    }

    #[tokio::test]
    async fn denied_command_returns_403() {
        let port = start_gh_proxy(&["pr *"]).await;
        let (status, _, body) = post(port, &["auth", "token"]).await;
        assert_eq!(status, 403);
        assert!(body.contains("denied"));
    }

    #[tokio::test]
    async fn api_always_denied() {
        let port = start_gh_proxy(&["*"]).await;
        let (status, _, _) = post(port, &["api", "/repos"]).await;
        assert_eq!(status, 403);
    }

    #[tokio::test]
    async fn empty_args_returns_400() {
        let port = start_gh_proxy(&["*"]).await;
        let client = reqwest::Client::new();
        let resp = client
            .post(format!("http://127.0.0.1:{port}/"))
            .json(&serde_json::json!({"args": []}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 400);
    }

    #[tokio::test]
    async fn get_returns_405() {
        let port = start_gh_proxy(&["*"]).await;
        let client = reqwest::Client::new();
        let resp = client
            .get(format!("http://127.0.0.1:{port}/"))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 405);
    }

    #[tokio::test]
    async fn allowed_command_executes() {
        let port = start_gh_proxy(&["version"]).await;
        let (status, exit_code, _body) = post(port, &["version"]).await;
        // gh may or may not be installed on the test host
        // If not installed, we get 500; if installed, we get 200
        assert!(status == 200 || status == 500);
        if status == 200 {
            assert_eq!(exit_code, "0");
        }
    }
}
