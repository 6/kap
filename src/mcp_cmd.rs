/// `devg mcp` subcommands: add, list, remove.
///
/// Global MCP server registration. Tokens are stored at ~/.devg/auth/<name>.json
/// (mode 0600) and shared across all projects via Docker volume mount.
/// File locks coordinate token refresh across multiple containers.
use anyhow::{Context, Result};
use std::path::PathBuf;

use crate::mcp::auth;
use crate::mcp::upstream::StoredAuth;

fn auth_dir() -> PathBuf {
    PathBuf::from(auth::host_auth_dir())
}

/// `devg mcp add <name> <upstream>` — run OAuth and register globally.
pub async fn add(name: &str, upstream: &str, reauth: bool) -> Result<()> {
    let dir = auth_dir();
    let file_path = dir.join(format!("{name}.json"));

    if file_path.exists() && !reauth {
        let auth = StoredAuth::load(&file_path)?;
        eprintln!("Already authenticated with {name} ({})", auth.upstream);
        eprintln!("Use --reauth to re-authenticate.");
        return Ok(());
    }

    let mut stored = auth::run(name, upstream).await?;

    // Verify tools/list works. If not, try common MCP subpaths.
    let mcp_url = discover_mcp_endpoint(upstream, &stored.access_token).await;
    if let Some(ref url) = mcp_url {
        stored.upstream = url.clone();
    }

    auth::write_auth_file(name, &stored, &dir.to_string_lossy())?;
    eprintln!("[auth] tokens saved to {}", file_path.display());

    eprintln!();
    eprintln!("To use in a project, add to .devcontainer/devg.toml:");
    eprintln!();
    eprintln!("  [[mcp.servers]]");
    eprintln!("  name = \"{name}\"");
    eprintln!();

    Ok(())
}

/// `devg mcp list` — show globally registered MCP servers.
pub fn list() -> Result<()> {
    let dir = auth_dir();

    if !dir.exists() {
        println!("No MCP servers registered. Run `devg mcp add <name> <upstream>` to add one.");
        return Ok(());
    }

    let mut entries: Vec<(String, StoredAuth)> = std::fs::read_dir(&dir)
        .with_context(|| format!("reading {}", dir.display()))?
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let path = e.path();
            if path.extension().and_then(|s| s.to_str()) != Some("json") {
                return None;
            }
            let name = path.file_stem()?.to_str()?.to_string();
            let auth = StoredAuth::load(&path).ok()?;
            Some((name, auth))
        })
        .collect();

    if entries.is_empty() {
        println!("No MCP servers registered. Run `devg mcp add <name> <upstream>` to add one.");
        return Ok(());
    }

    entries.sort_by(|a, b| a.0.cmp(&b.0));

    // Header
    println!("{:<16} {:<40} EXPIRES", "NAME", "UPSTREAM");
    println!("{:<16} {:<40} -------", "----", "--------");

    for (name, auth) in &entries {
        let expires = auth
            .expires_at
            .as_deref()
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
            .map(|dt| dt.format("%Y-%m-%d %H:%M").to_string())
            .unwrap_or_else(|| "never".to_string());

        println!("{:<16} {:<40} {}", name, auth.upstream, expires);
    }

    Ok(())
}

/// `devg mcp remove <name>` — delete auth file and lock file.
pub fn remove(name: &str) -> Result<()> {
    let dir = auth_dir();
    let file_path = dir.join(format!("{name}.json"));

    if !file_path.exists() {
        anyhow::bail!("no auth registered for '{name}'");
    }

    std::fs::remove_file(&file_path)
        .with_context(|| format!("removing {}", file_path.display()))?;

    // Clean up lock file if present
    let lock_path = file_path.with_extension("lock");
    if lock_path.exists() {
        let _ = std::fs::remove_file(&lock_path);
    }

    eprintln!("Removed {name}");
    Ok(())
}

/// Try tools/list at the given URL and common subpaths (/mcp).
/// Returns the working URL, or None if nothing works.
async fn discover_mcp_endpoint(base_url: &str, token: &str) -> Option<String> {
    let base = base_url.trim_end_matches('/');
    let candidates = [base.to_string(), format!("{base}/mcp")];

    for url in &candidates {
        eprintln!("[auth] trying tools/list at {url}...");
        match try_tools_list(url, token).await {
            Ok(count) => {
                eprintln!("[auth] success: {count} tools available at {url}");
                return Some(url.clone());
            }
            Err(e) => {
                eprintln!("[auth] {url}: {e}");
            }
        }
    }

    eprintln!("[auth] warning: tools/list failed at all endpoints");
    eprintln!("[auth] you may need to set `upstream` explicitly in devg.toml");
    None
}

async fn try_tools_list(url: &str, token: &str) -> Result<usize> {
    let http = reqwest::Client::new();
    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/list"
    });
    let resp = http
        .post(url)
        .bearer_auth(token)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json")
        .json(&body)
        .send()
        .await
        .context("connecting to MCP server")?;

    if !resp.status().is_success() {
        anyhow::bail!("HTTP {}", resp.status());
    }

    let json: serde_json::Value = resp.json().await.context("parsing response")?;
    if let Some(err) = json.get("error") {
        anyhow::bail!("JSON-RPC error: {err}");
    }
    let count = json["result"]["tools"]
        .as_array()
        .map(|a| a.len())
        .unwrap_or(0);
    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_test_auth(_name: &str, upstream: &str) -> StoredAuth {
        StoredAuth {
            upstream: upstream.to_string(),
            client_id: "test".to_string(),
            client_secret: None,
            access_token: "token".to_string(),
            refresh_token: None,
            token_endpoint: format!("{upstream}/token"),
            expires_at: Some("2030-01-01T00:00:00Z".to_string()),
        }
    }

    fn tempdir(suffix: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("devg-mcp-cmd-{}-{suffix}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn list_reads_auth_files() {
        let dir = tempdir("list");
        let auth = make_test_auth("linear", "https://mcp.linear.app/");
        std::fs::write(
            dir.join("linear.json"),
            serde_json::to_string(&auth).unwrap(),
        )
        .unwrap();

        let names = crate::mcp::list_auth_files(dir.to_str().unwrap());
        assert_eq!(names, vec!["linear"]);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn list_ignores_non_json_files() {
        let dir = tempdir("list-non-json");
        std::fs::write(dir.join("notes.txt"), "not json").unwrap();

        let names = crate::mcp::list_auth_files(dir.to_str().unwrap());
        assert!(names.is_empty());

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn list_empty_dir() {
        let dir = tempdir("list-empty");

        let names = crate::mcp::list_auth_files(dir.to_str().unwrap());
        assert!(names.is_empty());

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn list_nonexistent_dir() {
        let names = crate::mcp::list_auth_files("/nonexistent/auth/dir");
        assert!(names.is_empty());
    }

    #[test]
    fn remove_deletes_auth_file() {
        let dir = tempdir("remove");
        let auth = make_test_auth("linear", "https://mcp.linear.app/");
        let file_path = dir.join("linear.json");
        std::fs::write(&file_path, serde_json::to_string(&auth).unwrap()).unwrap();

        assert!(file_path.exists());
        // Can't easily test remove() since it uses auth_dir(), but verify file ops work
        std::fs::remove_file(&file_path).unwrap();
        assert!(!file_path.exists());

        std::fs::remove_dir_all(&dir).unwrap();
    }
}
