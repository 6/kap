/// Shared MCP client for initialize → tools/list handshake.
///
/// Used by `kap sidecar-check --mcp` (through the proxy, no auth) and
/// `kap mcp add`/`kap mcp get` (direct to upstream, with auth).
use anyhow::{Context, Result};

/// Auth to send with MCP requests.
pub struct McpAuth<'a> {
    pub token: Option<&'a str>,
    pub headers: &'a [(String, String)],
}

impl<'a> McpAuth<'a> {
    pub fn none() -> Self {
        Self {
            token: None,
            headers: &[],
        }
    }
}

/// Fetch the tools list from an MCP server (initialize → tools/list).
/// Returns the full tools array.
pub async fn fetch_tools(url: &str, auth: &McpAuth<'_>) -> Result<Vec<serde_json::Value>> {
    let http = reqwest::Client::new();

    // 1. Initialize to establish session
    let init_body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 0,
        "method": "initialize",
        "params": {
            "protocolVersion": "2025-03-26",
            "capabilities": {},
            "clientInfo": {"name": "kap", "version": "1.0"}
        }
    });
    let mut req = http
        .post(url)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .json(&init_body)
        .timeout(std::time::Duration::from_secs(10));
    req = apply_auth(req, auth);
    let resp = req.send().await.context("initialize")?;

    let session_id = resp
        .headers()
        .get("mcp-session-id")
        .and_then(|v| v.to_str().ok())
        .map(String::from);

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        let reason = serde_json::from_str::<serde_json::Value>(&body)
            .ok()
            .and_then(|v| v["reason"].as_str().map(String::from));
        if let Some(reason) = reason {
            anyhow::bail!("initialize: HTTP {status} ({reason})");
        } else {
            anyhow::bail!("initialize: HTTP {status}");
        }
    }
    // Drain the response body (may be SSE-streamed) before sending tools/list
    let _ = resp.text().await;

    // 2. tools/list with session
    let list_body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/list"
    });
    let mut req = http
        .post(url)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .json(&list_body)
        .timeout(std::time::Duration::from_secs(10));
    req = apply_auth(req, auth);
    if let Some(ref sid) = session_id {
        req = req.header("Mcp-Session-Id", sid);
    }

    let resp = req.send().await.context("tools/list")?;
    if !resp.status().is_success() {
        anyhow::bail!("tools/list: HTTP {}", resp.status());
    }

    let text = resp.text().await?;
    let json = parse_mcp_response(&text)?;

    if let Some(err) = json.get("error") {
        anyhow::bail!("tools/list: {err}");
    }

    Ok(json["result"]["tools"]
        .as_array()
        .cloned()
        .unwrap_or_default())
}

fn apply_auth(mut req: reqwest::RequestBuilder, auth: &McpAuth<'_>) -> reqwest::RequestBuilder {
    if let Some(t) = auth.token {
        req = req.bearer_auth(t);
    }
    for (k, v) in auth.headers {
        req = req.header(k.as_str(), v.as_str());
    }
    req
}

/// Parse an MCP response that may be plain JSON or SSE-framed.
pub fn parse_mcp_response(text: &str) -> Result<serde_json::Value> {
    // Try plain JSON first
    if let Ok(v) = serde_json::from_str(text) {
        return Ok(v);
    }
    // Try SSE: extract the last "data: " line
    for line in text.lines().rev() {
        if let Some(data) = line.strip_prefix("data: ")
            && let Ok(v) = serde_json::from_str(data)
        {
            return Ok(v);
        }
    }
    anyhow::bail!("cannot parse response as JSON or SSE")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_plain_json() {
        let json = r#"{"jsonrpc":"2.0","id":1,"result":{"tools":[]}}"#;
        let v = parse_mcp_response(json).unwrap();
        assert_eq!(v["id"], 1);
    }

    #[test]
    fn parse_sse_framed() {
        let sse =
            "event: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"tools\":[]}}\n\n";
        let v = parse_mcp_response(sse).unwrap();
        assert_eq!(v["id"], 1);
    }

    #[test]
    fn parse_garbage_fails() {
        assert!(parse_mcp_response("not json or sse").is_err());
    }

    #[tokio::test]
    async fn fetch_tools_404_with_reason_includes_reason_in_error() {
        // Start a mock server that returns 404 with a reason field
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    continue;
                };
                let io = hyper_util::rt::TokioIo::new(stream);
                tokio::spawn(async move {
                    let _ = hyper::server::conn::http1::Builder::new()
                        .serve_connection(
                            io,
                            hyper::service::service_fn(|_req| async {
                                let body = serde_json::json!({
                                    "error": "unknown MCP server: broken",
                                    "reason": "reading /etc/kap/auth/broken.json: No such file"
                                });
                                Ok::<_, hyper::Error>(
                                    hyper::Response::builder()
                                        .status(404)
                                        .header("Content-Type", "application/json")
                                        .body(http_body_util::Full::new(bytes::Bytes::from(
                                            serde_json::to_vec(&body).unwrap(),
                                        )))
                                        .unwrap(),
                                )
                            }),
                        )
                        .await;
                });
            }
        });

        // Wait for server
        for _ in 0..100 {
            if tokio::net::TcpStream::connect(format!("127.0.0.1:{port}"))
                .await
                .is_ok()
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        let auth = McpAuth::none();
        let err = fetch_tools(&format!("http://127.0.0.1:{port}"), &auth)
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("404"), "should contain status: {msg}");
        assert!(msg.contains("No such file"), "should contain reason: {msg}");
    }

    #[tokio::test]
    async fn fetch_tools_404_without_reason_shows_status_only() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    continue;
                };
                let io = hyper_util::rt::TokioIo::new(stream);
                tokio::spawn(async move {
                    let _ = hyper::server::conn::http1::Builder::new()
                        .serve_connection(
                            io,
                            hyper::service::service_fn(|_req| async {
                                let body = serde_json::json!({"error": "not found"});
                                Ok::<_, hyper::Error>(
                                    hyper::Response::builder()
                                        .status(404)
                                        .header("Content-Type", "application/json")
                                        .body(http_body_util::Full::new(bytes::Bytes::from(
                                            serde_json::to_vec(&body).unwrap(),
                                        )))
                                        .unwrap(),
                                )
                            }),
                        )
                        .await;
                });
            }
        });

        for _ in 0..100 {
            if tokio::net::TcpStream::connect(format!("127.0.0.1:{port}"))
                .await
                .is_ok()
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        let auth = McpAuth::none();
        let err = fetch_tools(&format!("http://127.0.0.1:{port}"), &auth)
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("404"), "should contain status: {msg}");
        assert!(!msg.contains("reason"), "should not contain reason: {msg}");
    }
}
