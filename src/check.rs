/// Health checks for the proxy container.
use anyhow::Result;

pub async fn run(_proxy_only: bool) -> Result<()> {
    tokio::net::TcpStream::connect("127.0.0.1:3128")
        .await
        .map_err(|_| anyhow::anyhow!("cannot connect to proxy on port 3128"))?;
    Ok(())
}

/// Check each configured MCP server by sending initialize + tools/list.
/// Outputs one JSON line per server: {"name":"...","tools":N} or {"name":"...","error":"..."}.
pub async fn run_mcp(config_path: &str) -> Result<()> {
    let cfg = crate::config::Config::load(config_path)?;
    let Some(ref mcp) = cfg.mcp else {
        return Ok(());
    };

    let http = reqwest::Client::new();
    let mcp_base = "http://127.0.0.1:3129";

    let mut set = tokio::task::JoinSet::new();
    for server in &mcp.servers {
        let http = http.clone();
        let url = format!("{mcp_base}/{}", server.name);
        let name = server.name.clone();
        set.spawn(async move {
            let result = check_mcp_server(&http, &url).await;
            match result {
                Ok(count) => serde_json::json!({"name": name, "tools": count}),
                Err(e) => serde_json::json!({"name": name, "error": e.to_string()}),
            }
        });
    }

    while let Some(result) = set.join_next().await {
        if let Ok(r) = result {
            println!("{r}");
        }
    }
    Ok(())
}

async fn check_mcp_server(http: &reqwest::Client, url: &str) -> Result<usize> {
    // 1. Initialize to establish session
    let init_body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 0,
        "method": "initialize",
        "params": {
            "protocolVersion": "2025-03-26",
            "capabilities": {},
            "clientInfo": {"name": "devg-check", "version": "1.0"}
        }
    });
    let resp = http
        .post(url)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json")
        .json(&init_body)
        .timeout(std::time::Duration::from_secs(10))
        .send()
        .await?;

    // Capture session ID if returned
    let session_id = resp
        .headers()
        .get("mcp-session-id")
        .and_then(|v| v.to_str().ok())
        .map(String::from);

    if !resp.status().is_success() {
        anyhow::bail!("initialize: HTTP {}", resp.status());
    }

    // Consume body (may be SSE-framed, we don't need the initialize result)
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
        .header("Accept", "application/json")
        .json(&list_body)
        .timeout(std::time::Duration::from_secs(10));

    if let Some(ref sid) = session_id {
        req = req.header("Mcp-Session-Id", sid);
    }

    let resp = req.send().await?;
    if !resp.status().is_success() {
        anyhow::bail!("tools/list: HTTP {}", resp.status());
    }

    // Some servers return SSE-framed responses (event: message\ndata: {...})
    // even with Accept: application/json. Parse accordingly.
    let text = resp.text().await?;
    let json = parse_mcp_response(&text)?;

    if let Some(err) = json.get("error") {
        anyhow::bail!("tools/list: {err}");
    }

    let count = json["result"]["tools"]
        .as_array()
        .map(|a| a.len())
        .unwrap_or(0);
    Ok(count)
}

/// Parse an MCP response that may be plain JSON or SSE-framed.
fn parse_mcp_response(text: &str) -> Result<serde_json::Value> {
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
