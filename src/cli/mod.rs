pub mod filter;
pub mod shim;

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

use crate::config::CliToolMode;
use crate::proxy::log::{ProxyLogEntry, ProxyLogger};
use crate::reload::{self, CliTools, Shared};

struct CliState {
    tools: Shared<CliTools>,
    logger: ProxyLogger,
}

const DEFAULT_CLI_LISTEN: &str = "0.0.0.0:3130";

pub async fn run(tools: Shared<CliTools>, logger: ProxyLogger) -> Result<()> {
    // Log initial tools
    {
        let t = reload::load(&tools);
        for (name, tool) in &t.tools {
            let mode = match tool.mode {
                CliToolMode::Proxy => "proxy",
                CliToolMode::Direct => "direct",
            };
            eprintln!("[cli] {name} ({mode})");
        }
    }

    let state = Arc::new(CliState { tools, logger });
    let listener = TcpListener::bind(DEFAULT_CLI_LISTEN).await?;
    eprintln!("[cli] listening on {DEFAULT_CLI_LISTEN}");

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
                eprintln!("[cli] connection error: {e}");
            }
        });
    }
}

async fn handle_request(
    req: Request<Incoming>,
    state: &CliState,
) -> Result<Response<Full<Bytes>>, hyper::Error> {
    if req.method() != hyper::Method::POST {
        return Ok(error_response(405, "only POST is supported"));
    }

    // Extract tool name from path: /gh -> "gh"
    let tool_name = req
        .uri()
        .path()
        .trim_start_matches('/')
        .split('/')
        .next()
        .unwrap_or("")
        .to_string();

    // Load current tools (hot-reloadable)
    let tools = reload::load(&state.tools);

    let Some(tool) = tools.tools.get(&tool_name) else {
        return Ok(error_response(404, &format!("unknown tool: {tool_name}")));
    };

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

    let cmd_display = args.join(" ");
    let log_target = format!("cli/{tool_name}");

    // Direct mode: skip allow/deny filtering (the sidecar isn't executing anything,
    // just returning credentials so the shim can exec the real binary locally)
    if tool.mode == CliToolMode::Direct {
        let entry = ProxyLogEntry::new(&log_target, "direct", &cmd_display);
        let _ = state.logger.log(&entry).await;
        let env_pairs: Vec<String> = tool
            .env_vars
            .iter()
            .filter_map(|var| std::env::var(var).ok().map(|val| format!("{var}={val}")))
            .collect();
        let env_b64 = base64::engine::general_purpose::STANDARD.encode(env_pairs.join("\n"));

        let mut builder = Response::builder().status(200).header("X-Mode", "direct");
        if !env_b64.is_empty() {
            builder = builder.header("X-Env", env_b64);
        }
        return Ok(builder.body(Full::new(Bytes::new())).unwrap());
    }

    // Proxy mode: check allow/deny filters before executing
    if !tool.filter.is_allowed(&args) {
        let entry = ProxyLogEntry::new(&log_target, "denied", &cmd_display);
        let _ = state.logger.log(&entry).await;
        eprintln!("[cli] {tool_name} DENIED: {cmd_display}");
        return Ok(error_response(
            403,
            &format!("command denied: {tool_name} {cmd_display}"),
        ));
    }

    let entry = ProxyLogEntry::new(&log_target, "allowed", &cmd_display);
    let _ = state.logger.log(&entry).await;

    // Map the app container's workspace path to /workspace on the sidecar
    let cwd = parsed["cwd"]
        .as_str()
        .map(map_workspace_path)
        .unwrap_or_else(|| "/workspace".to_string());

    // Spawn the tool with only the configured env vars from the sidecar env
    let mut cmd = tokio::process::Command::new(&tool_name);
    cmd.args(&args);
    cmd.current_dir(&cwd);
    cmd.env_clear();
    // Pass through PATH so the binary can be found
    if let Ok(path) = std::env::var("PATH") {
        cmd.env("PATH", path);
    }
    // Pass through HOME for tools that need config dirs
    if let Ok(home) = std::env::var("HOME") {
        cmd.env("HOME", home);
    }
    // Pass only the configured env vars
    for var in &tool.env_vars {
        if let Ok(val) = std::env::var(var) {
            cmd.env(var, val);
        }
    }
    // Disable interactive prompts and pagers
    cmd.env("NO_COLOR", "1");
    cmd.env("PAGER", "cat");
    cmd.env("GH_PROMPT_DISABLED", "1");
    cmd.env("GH_NO_UPDATE_NOTIFIER", "1");

    match cmd.output().await {
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
            eprintln!("[cli] failed to spawn {tool_name}: {e}");
            Ok(error_response(
                500,
                &format!("failed to run {tool_name}: {e}"),
            ))
        }
    }
}

/// Map app container paths like /workspaces/project/subdir to /workspace/subdir.
/// The sidecar mounts the project root at /workspace.
fn map_workspace_path(path: &str) -> String {
    // /workspaces/<project>/subdir -> /workspace/subdir
    if let Some(rest) = path.strip_prefix("/workspaces/") {
        if let Some(pos) = rest.find('/') {
            return format!("/workspace{}", &rest[pos..]);
        }
        return "/workspace".to_string();
    }
    "/workspace".to_string()
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
    use crate::cli::filter::CommandFilter;
    use crate::proxy::log::ProxyLogger;
    use std::collections::HashMap;

    async fn start_cli_proxy(tool_name: &str, allow: &[&str], deny: &[&str]) -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);

        let mut tools = HashMap::new();
        tools.insert(
            tool_name.to_string(),
            reload::CliTool {
                filter: CommandFilter::new(
                    &allow.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
                    &deny.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
                ),
                env_vars: vec![],
                mode: CliToolMode::Proxy,
            },
        );

        let shared = reload::new_shared(CliTools { tools });
        let state = Arc::new(CliState {
            tools: shared,
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
        panic!("cli proxy did not start");
    }

    async fn post(port: u16, tool: &str, args: &[&str]) -> (u16, String, String) {
        let client = reqwest::Client::new();
        let resp = client
            .post(format!("http://127.0.0.1:{port}/{tool}"))
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
        let port = start_cli_proxy("gh", &["pr *"], &["auth *", "api"]).await;
        let (status, _, body) = post(port, "gh", &["auth", "token"]).await;
        assert_eq!(status, 403);
        assert!(body.contains("denied"));
    }

    #[tokio::test]
    async fn deny_overrides_allow() {
        let port = start_cli_proxy("gh", &["*"], &["api"]).await;
        let (status, _, _) = post(port, "gh", &["api", "/repos"]).await;
        assert_eq!(status, 403);
    }

    #[tokio::test]
    async fn unknown_tool_returns_404() {
        let port = start_cli_proxy("gh", &["*"], &[]).await;
        let (status, _, _) = post(port, "nonexistent", &["help"]).await;
        assert_eq!(status, 404);
    }

    #[tokio::test]
    async fn empty_args_shows_help() {
        let port = start_cli_proxy("gh", &["*"], &[]).await;
        let (status, _, _) = post(port, "gh", &[]).await;
        // gh with no args shows help (exit 0 or 1 depending on tool, but not 400)
        assert!(status == 200 || status == 500); // 500 if gh not installed
    }

    #[tokio::test]
    async fn get_returns_405() {
        let port = start_cli_proxy("gh", &["*"], &[]).await;
        let client = reqwest::Client::new();
        let resp = client
            .get(format!("http://127.0.0.1:{port}/gh"))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 405);
    }
}
