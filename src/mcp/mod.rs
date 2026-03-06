pub mod auth;
pub mod filter;
pub mod jsonrpc;
pub mod upstream;

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use anyhow::Result;
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;
use tokio::net::TcpListener;

use crate::config::McpConfig;
use crate::proxy::log::{ProxyLogEntry, ProxyLogger};
use filter::ToolFilter;
use upstream::{StoredAuth, UpstreamClient};

struct McpServer {
    client: UpstreamClient,
    filter: ToolFilter,
}

struct McpState {
    servers: HashMap<String, McpServer>,
    logger: ProxyLogger,
}

pub async fn run(config: &McpConfig, logger: ProxyLogger) -> Result<()> {
    let mut servers = HashMap::new();

    for server_cfg in &config.servers {
        // Expand ${VAR} in header values from env
        let headers: Vec<(String, String)> = server_cfg
            .headers
            .iter()
            .filter_map(|(k, v)| {
                let expanded = expand_env(v);
                if expanded.is_empty() {
                    eprintln!("[mcp] {}: skipping header {k} (empty after env expansion)", server_cfg.name);
                    None
                } else {
                    Some((k.clone(), expanded))
                }
            })
            .collect();

        // token_env takes priority, then auth file from `devp auth`, then headers-only
        let has_headers = !headers.is_empty();
        let client = if let Some(ref env_var) = server_cfg.token_env {
            match std::env::var(env_var) {
                Ok(token) if !token.is_empty() => {
                    eprintln!("[mcp] {} using token from ${env_var}", server_cfg.name);
                    UpstreamClient::with_static_token(server_cfg.upstream.clone(), token, headers)
                }
                _ => {
                    eprintln!(
                        "[mcp] skipping {}: ${env_var} is not set or empty",
                        server_cfg.name
                    );
                    continue;
                }
            }
        } else {
            let auth_path =
                Path::new(&config.auth_dir).join(format!("{}.json", server_cfg.name));
            match StoredAuth::load(&auth_path) {
                Ok(auth) => UpstreamClient::new(server_cfg.upstream.clone(), auth, headers),
                Err(e) if has_headers => {
                    eprintln!("[mcp] {} using headers only (no OAuth)", server_cfg.name);
                    UpstreamClient::with_headers_only(server_cfg.upstream.clone(), headers)
                }
                Err(e) => {
                    eprintln!(
                        "[mcp] skipping {}: no auth at {} ({e})",
                        server_cfg.name,
                        auth_path.display()
                    );
                    continue;
                }
            }
        };
        let filter = ToolFilter::new(&server_cfg.allow_tools, &server_cfg.deny_tools);

        eprintln!(
            "[mcp] {} → {}",
            server_cfg.name, server_cfg.upstream
        );
        servers.insert(server_cfg.name.clone(), McpServer { client, filter });
    }

    let state = Arc::new(McpState { servers, logger });
    let listener = TcpListener::bind(&config.listen).await?;
    eprintln!("[mcp] listening on {}", config.listen);

    loop {
        let (stream, _addr) = listener.accept().await?;
        let state = state.clone();

        tokio::spawn(async move {
            let io = TokioIo::new(stream);
            let service = service_fn(move |req| {
                let state = state.clone();
                async move { handle_request(req, &state).await }
            });

            if let Err(e) = http1::Builder::new()
                .serve_connection(io, service)
                .await
                && !e.to_string().contains("error shutting down connection")
            {
                eprintln!("[mcp] connection error: {e}");
            }
        });
    }
}

async fn handle_request(
    req: Request<Incoming>,
    state: &McpState,
) -> Result<Response<Full<Bytes>>, hyper::Error> {
    // Extract server name from path: /github → "github"
    let path = req.uri().path().trim_start_matches('/').to_string();
    let method = req.method().clone();
    let server_name = path.split('/').next().unwrap_or("").to_string();

    let Some(server) = state.servers.get(&*server_name) else {
        return Ok(json_response(
            404,
            &serde_json::json!({"error": format!("unknown MCP server: {server_name}")}),
        ));
    };

    // Only accept POST
    if method != hyper::Method::POST {
        return Ok(json_response(
            405,
            &serde_json::json!({"error": "only POST is supported"}),
        ));
    }

    let body = req.into_body().collect().await?.to_bytes();

    // Parse JSON-RPC to inspect method
    let rpc_req: jsonrpc::Request = match serde_json::from_slice(&body) {
        Ok(r) => r,
        Err(_) => {
            // Can't parse — forward as-is (might be a batch or notification)
            return forward_raw(server, &body, &state.logger, &server_name).await;
        }
    };

    match rpc_req.method.as_str() {
        "tools/call" => handle_tools_call(server, &rpc_req, &body, &state.logger, &server_name).await,
        "tools/list" => handle_tools_list(server, &body, &state.logger, &server_name).await,
        _ => forward_raw(server, &body, &state.logger, &server_name).await,
    }
}

async fn handle_tools_call(
    server: &McpServer,
    rpc_req: &jsonrpc::Request,
    body: &[u8],
    logger: &ProxyLogger,
    server_name: &str,
) -> Result<Response<Full<Bytes>>, hyper::Error> {
    let tool_name = jsonrpc::tool_call_name(&rpc_req.params).unwrap_or("unknown");

    if !server.filter.is_allowed(tool_name) {
        let entry = ProxyLogEntry::new(
            &format!("mcp/{server_name}"),
            "denied",
            &format!("tools/call:{tool_name}"),
        );
        let _ = logger.log(&entry).await;
        eprintln!("[mcp] DENIED tools/call {server_name}/{tool_name}");

        let resp = jsonrpc::Response::error(
            rpc_req.id.clone(),
            -32602,
            format!("Tool '{tool_name}' denied by devp MCP filter"),
        );
        return Ok(json_response(200, &resp));
    }

    let entry = ProxyLogEntry::new(
        &format!("mcp/{server_name}"),
        "allowed",
        &format!("tools/call:{tool_name}"),
    );
    let _ = logger.log(&entry).await;

    forward_raw(server, body, logger, server_name).await
}

async fn handle_tools_list(
    server: &McpServer,
    body: &[u8],
    logger: &ProxyLogger,
    server_name: &str,
) -> Result<Response<Full<Bytes>>, hyper::Error> {
    let entry = ProxyLogEntry::new(
        &format!("mcp/{server_name}"),
        "allowed",
        "tools/list",
    );
    let _ = logger.log(&entry).await;

    let (status, resp_body) = match server.client.forward(body).await {
        Ok(r) => r,
        Err(e) => {
            eprintln!("[mcp] upstream error for {server_name}: {e}");
            return Ok(json_response(
                502,
                &serde_json::json!({"error": format!("upstream error: {e}")}),
            ));
        }
    };

    // Filter tools from the response
    if let Ok(mut rpc_resp) = serde_json::from_slice::<jsonrpc::Response>(&resp_body) {
        if let Some(ref mut result) = rpc_resp.result {
            jsonrpc::filter_tools_list(result, |name| server.filter.is_allowed(name));
        }
        return Ok(json_response(status, &rpc_resp));
    }

    // Can't parse — return as-is
    Ok(raw_response(status, &resp_body))
}

async fn forward_raw(
    server: &McpServer,
    body: &[u8],
    _logger: &ProxyLogger,
    server_name: &str,
) -> Result<Response<Full<Bytes>>, hyper::Error> {
    match server.client.forward(body).await {
        Ok((status, resp_body)) => Ok(raw_response(status, &resp_body)),
        Err(e) => {
            eprintln!("[mcp] upstream error for {server_name}: {e}");
            Ok(json_response(
                502,
                &serde_json::json!({"error": format!("upstream error: {e}")}),
            ))
        }
    }
}

fn json_response(status: u16, body: &impl serde::Serialize) -> Response<Full<Bytes>> {
    let json = serde_json::to_vec(body).unwrap_or_default();
    Response::builder()
        .status(status)
        .header("Content-Type", "application/json")
        .body(Full::new(Bytes::from(json)))
        .unwrap()
}

fn raw_response(status: u16, body: &[u8]) -> Response<Full<Bytes>> {
    Response::builder()
        .status(status)
        .header("Content-Type", "application/json")
        .body(Full::new(Bytes::from(body.to_vec())))
        .unwrap()
}

/// Expand ${VAR} references in a string from environment variables.
fn expand_env(s: &str) -> String {
    let mut result = s.to_string();
    while let Some(start) = result.find("${") {
        let Some(end) = result[start..].find('}') else {
            break;
        };
        let var_name = &result[start + 2..start + end];
        let value = std::env::var(var_name).unwrap_or_default();
        result = format!("{}{value}{}", &result[..start], &result[start + end + 1..]);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Start a mock MCP upstream server that returns canned responses.
    async fn start_mock_upstream() -> (u16, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let handle = tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    continue;
                };
                let io = TokioIo::new(stream);
                tokio::spawn(async move {
                    let service = service_fn(|req: Request<Incoming>| async move {
                        let body = req.into_body().collect().await?.to_bytes();
                        let rpc: serde_json::Value =
                            serde_json::from_slice(&body).unwrap_or_default();
                        let method = rpc["method"].as_str().unwrap_or("");
                        let id = rpc.get("id").cloned();

                        let response = match method {
                            "initialize" => serde_json::json!({
                                "jsonrpc": "2.0",
                                "id": id,
                                "result": {
                                    "protocolVersion": "2025-03-26",
                                    "capabilities": {"tools": {}},
                                    "serverInfo": {"name": "mock", "version": "1.0"}
                                }
                            }),
                            "tools/list" => serde_json::json!({
                                "jsonrpc": "2.0",
                                "id": id,
                                "result": {
                                    "tools": [
                                        {"name": "read_file", "description": "Read a file"},
                                        {"name": "write_file", "description": "Write a file"},
                                        {"name": "delete_file", "description": "Delete a file"},
                                        {"name": "search_code", "description": "Search code"},
                                    ]
                                }
                            }),
                            "tools/call" => {
                                let tool_name =
                                    rpc["params"]["name"].as_str().unwrap_or("unknown");
                                serde_json::json!({
                                    "jsonrpc": "2.0",
                                    "id": id,
                                    "result": {
                                        "content": [{"type": "text", "text": format!("called {tool_name}")}]
                                    }
                                })
                            }
                            _ => serde_json::json!({
                                "jsonrpc": "2.0",
                                "id": id,
                                "result": {}
                            }),
                        };

                        Ok::<_, hyper::Error>(
                            Response::builder()
                                .status(200)
                                .header("Content-Type", "application/json")
                                .body(Full::new(Bytes::from(
                                    serde_json::to_vec(&response).unwrap(),
                                )))
                                .unwrap(),
                        )
                    });
                    let _ = http1::Builder::new().serve_connection(io, service).await;
                });
            }
        });

        (port, handle)
    }

    /// Start the MCP proxy with a given server config, return the proxy port.
    async fn start_mcp_proxy(
        upstream_port: u16,
        allow_tools: &[&str],
        deny_tools: &[&str],
    ) -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);

        let auth = StoredAuth {
            upstream: format!("http://127.0.0.1:{upstream_port}"),
            client_id: "test".to_string(),
            client_secret: None,
            access_token: "test_token".to_string(),
            refresh_token: None,
            token_endpoint: "http://unused/token".to_string(),
            expires_at: None,
        };

        let client = UpstreamClient::new(format!("http://127.0.0.1:{upstream_port}"), auth, vec![]);
        let filter_obj = ToolFilter::new(
            &allow_tools.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
            &deny_tools.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
        );

        let mut servers = HashMap::new();
        servers.insert(
            "test".to_string(),
            McpServer {
                client,
                filter: filter_obj,
            },
        );

        let state = Arc::new(McpState {
            servers,
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

        // Wait for proxy to be ready
        for _ in 0..100 {
            if tokio::net::TcpStream::connect(format!("127.0.0.1:{port}"))
                .await
                .is_ok()
            {
                return port;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("MCP proxy did not start");
    }

    async fn post_jsonrpc(port: u16, server_name: &str, body: &serde_json::Value) -> serde_json::Value {
        let client = reqwest::Client::new();
        let resp = client
            .post(format!("http://127.0.0.1:{port}/{server_name}"))
            .json(body)
            .send()
            .await
            .unwrap();
        resp.json().await.unwrap()
    }

    #[tokio::test]
    async fn tools_list_is_filtered() {
        let (upstream_port, _handle) = start_mock_upstream().await;
        let proxy_port =
            start_mcp_proxy(upstream_port, &["read_file", "search_code"], &[]).await;

        let resp = post_jsonrpc(
            proxy_port,
            "test",
            &serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}),
        )
        .await;

        let tools = resp["result"]["tools"].as_array().unwrap();
        let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert_eq!(names, vec!["read_file", "search_code"]);
        // write_file and delete_file should be filtered out
    }

    #[tokio::test]
    async fn tools_call_allowed_forwards() {
        let (upstream_port, _handle) = start_mock_upstream().await;
        let proxy_port =
            start_mcp_proxy(upstream_port, &["read_file"], &[]).await;

        let resp = post_jsonrpc(
            proxy_port,
            "test",
            &serde_json::json!({
                "jsonrpc": "2.0", "id": 2,
                "method": "tools/call",
                "params": {"name": "read_file", "arguments": {}}
            }),
        )
        .await;

        // Should have forwarded to upstream and gotten a result
        let text = resp["result"]["content"][0]["text"].as_str().unwrap();
        assert_eq!(text, "called read_file");
    }

    #[tokio::test]
    async fn tools_call_denied_returns_error() {
        let (upstream_port, _handle) = start_mock_upstream().await;
        let proxy_port =
            start_mcp_proxy(upstream_port, &["read_file"], &[]).await;

        let resp = post_jsonrpc(
            proxy_port,
            "test",
            &serde_json::json!({
                "jsonrpc": "2.0", "id": 3,
                "method": "tools/call",
                "params": {"name": "delete_file", "arguments": {}}
            }),
        )
        .await;

        // Should be denied with JSON-RPC error
        assert!(resp["error"].is_object());
        assert_eq!(resp["error"]["code"], -32602);
        assert!(resp["error"]["message"]
            .as_str()
            .unwrap()
            .contains("delete_file"));
    }

    #[tokio::test]
    async fn deny_overrides_allow() {
        let (upstream_port, _handle) = start_mock_upstream().await;
        let proxy_port =
            start_mcp_proxy(upstream_port, &["*"], &["delete_*"]).await;

        // read_file should be allowed (matches *)
        let resp = post_jsonrpc(
            proxy_port,
            "test",
            &serde_json::json!({
                "jsonrpc": "2.0", "id": 1,
                "method": "tools/call",
                "params": {"name": "read_file", "arguments": {}}
            }),
        )
        .await;
        assert!(resp["result"].is_object());

        // delete_file should be denied (deny overrides allow)
        let resp = post_jsonrpc(
            proxy_port,
            "test",
            &serde_json::json!({
                "jsonrpc": "2.0", "id": 2,
                "method": "tools/call",
                "params": {"name": "delete_file", "arguments": {}}
            }),
        )
        .await;
        assert!(resp["error"].is_object());
    }

    #[tokio::test]
    async fn unknown_server_returns_404() {
        let (upstream_port, _handle) = start_mock_upstream().await;
        let proxy_port = start_mcp_proxy(upstream_port, &["*"], &[]).await;

        let client = reqwest::Client::new();
        let resp = client
            .post(format!("http://127.0.0.1:{proxy_port}/nonexistent"))
            .json(&serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 404);
    }

    #[tokio::test]
    async fn non_tool_methods_forwarded_transparently() {
        let (upstream_port, _handle) = start_mock_upstream().await;
        let proxy_port =
            start_mcp_proxy(upstream_port, &["read_file"], &[]).await;

        let resp = post_jsonrpc(
            proxy_port,
            "test",
            &serde_json::json!({
                "jsonrpc": "2.0", "id": 1,
                "method": "initialize",
                "params": {"protocolVersion": "2025-03-26", "capabilities": {}, "clientInfo": {"name": "test", "version": "1.0"}}
            }),
        )
        .await;

        // initialize should be forwarded to upstream regardless of tool filter
        assert!(resp["result"]["serverInfo"].is_object());
    }
}
