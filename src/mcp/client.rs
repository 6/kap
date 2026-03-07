/// Shared MCP client for initialize → tools/list handshake.
///
/// Used by `devg check --mcp` (through the proxy, no auth) and
/// `devg mcp add`/`devg mcp get` (direct to upstream, with auth).
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
            "clientInfo": {"name": "devg", "version": "1.0"}
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
        anyhow::bail!("initialize: HTTP {}", resp.status());
    }
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
}
