use crate::mcp::client::{McpAuth, fetch_tools};
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

    let mcp_base = "http://127.0.0.1:3129";

    let mut set = tokio::task::JoinSet::new();
    for server in &mcp.servers {
        let name = server.name.clone();
        let url = format!("{mcp_base}/{name}");
        set.spawn(async move {
            // No auth needed — goes through the proxy which handles credentials
            let auth = McpAuth::none();
            match fetch_tools(&url, &auth).await {
                Ok(tools) => serde_json::json!({"name": name, "tools": tools.len()}),
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
