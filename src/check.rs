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

    for server in &mcp.servers {
        let url = format!("{mcp_base}/{}", server.name);
        let result = check_mcp_server(&http, &url).await;
        match result {
            Ok(count) => {
                println!(
                    "{}",
                    serde_json::json!({"name": server.name, "tools": count})
                );
            }
            Err(e) => {
                println!(
                    "{}",
                    serde_json::json!({"name": server.name, "error": e.to_string()})
                );
            }
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

    let json: serde_json::Value = resp.json().await?;
    if let Some(err) = json.get("error") {
        anyhow::bail!("tools/list: {err}");
    }

    let count = json["result"]["tools"]
        .as_array()
        .map(|a| a.len())
        .unwrap_or(0);
    Ok(count)
}
