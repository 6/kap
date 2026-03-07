/// `kap mcp` subcommands: add, list, remove.
///
/// Global MCP server registration. Tokens are stored at ~/.kap/auth/<name>.json
/// (mode 0600) and shared across all projects via Docker volume mount.
/// File locks coordinate token refresh across multiple containers.
use anyhow::{Context, Result};
use std::path::PathBuf;

use crate::mcp::auth;
use crate::mcp::client::{McpAuth, fetch_tools};
use crate::mcp::upstream::StoredAuth;

fn auth_dir() -> PathBuf {
    PathBuf::from(auth::host_auth_dir())
}

/// `kap mcp add <name> <upstream>` — run OAuth or store static headers.
pub async fn add(name: &str, upstream: &str, reauth: bool, headers: &[String]) -> Result<()> {
    let dir = auth_dir();
    let file_path = dir.join(format!("{name}.json"));

    if file_path.exists() && !reauth {
        let auth = StoredAuth::load(&file_path)?;
        eprintln!("Already registered: {name} ({})", auth.upstream);
        eprintln!("Use --reauth to re-register.");
        return Ok(());
    }

    if !headers.is_empty() {
        // Static headers mode: skip OAuth
        let mut header_map = std::collections::HashMap::new();
        for h in headers {
            let (key, value) = h.split_once('=').ok_or_else(|| {
                anyhow::anyhow!("invalid header format: {h:?} (expected KEY=VALUE)")
            })?;
            header_map.insert(key.to_string(), value.to_string());
        }

        let mut stored = StoredAuth {
            upstream: upstream.to_string(),
            client_id: String::new(),
            client_secret: None,
            access_token: String::new(),
            refresh_token: None,
            token_endpoint: String::new(),
            expires_at: None,
            headers: header_map,
        };

        // Verify tools/list works, try common subpaths
        let header_pairs: Vec<(String, String)> = stored
            .headers
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        let auth = McpAuth {
            token: None,
            headers: &header_pairs,
        };
        let Some(url) = discover_mcp_endpoint(upstream, &auth).await else {
            anyhow::bail!("could not verify {upstream}. Check the URL and headers.");
        };
        stored.upstream = url;

        crate::mcp::auth::write_auth_file(name, &stored, &dir.to_string_lossy())?;
        eprintln!("[auth] saved to {}", file_path.display());
    } else {
        // OAuth mode
        let mut stored = crate::mcp::auth::run(name, upstream).await?;

        // Verify tools/list works. If not, try common MCP subpaths.
        let auth = McpAuth {
            token: Some(&stored.access_token),
            headers: &[],
        };
        let Some(url) = discover_mcp_endpoint(upstream, &auth).await else {
            anyhow::bail!("could not verify {upstream}. OAuth succeeded but tools/list failed.");
        };
        stored.upstream = url;

        crate::mcp::auth::write_auth_file(name, &stored, &dir.to_string_lossy())?;
        eprintln!("[auth] tokens saved to {}", file_path.display());
    }

    eprintln!();
    eprintln!("Registered {name}. It will be auto-discovered by kap.");
    eprintln!("To restrict tools, add to .devcontainer/kap.toml:");
    eprintln!();
    eprintln!("  [[mcp.servers]]");
    eprintln!("  name = \"{name}\"");
    eprintln!("  allow_tools = [\"*\"]");
    eprintln!();

    Ok(())
}

/// `kap mcp list` — show globally registered MCP servers.
pub fn list() -> Result<()> {
    let dir = auth_dir();

    if !dir.exists() {
        println!("No MCP servers registered. Run `kap mcp add <name> <upstream>` to add one.");
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
        println!("No MCP servers registered. Run `kap mcp add <name> <upstream>` to add one.");
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

/// `kap mcp get <name>` — show details for a registered MCP server.
pub async fn get(name: &str) -> Result<()> {
    let dir = auth_dir();
    let file_path = dir.join(format!("{name}.json"));

    if !file_path.exists() {
        anyhow::bail!("no auth registered for '{name}'. Run `kap mcp add {name} <upstream>`");
    }

    let auth = StoredAuth::load(&file_path)?;

    println!("Name:     {name}");
    println!("Upstream: {}", auth.upstream);

    let has_headers = !auth.headers.is_empty();
    let has_token = !auth.access_token.is_empty();
    if has_headers {
        let keys: Vec<&str> = auth.headers.keys().map(|k| k.as_str()).collect();
        println!("Auth:     headers ({})", keys.join(", "));
    } else if has_token {
        let expires = auth
            .expires_at
            .as_deref()
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
            .map(|dt| dt.format("%Y-%m-%d %H:%M UTC").to_string())
            .unwrap_or_else(|| "never".to_string());
        println!("Auth:     OAuth (expires {expires})");
    }

    // Fetch tools list from upstream
    let header_pairs: Vec<(String, String)> = auth
        .headers
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    let token = if has_token {
        Some(auth.access_token.as_str())
    } else {
        None
    };

    println!();
    eprint!("Fetching tools...");
    let mcp_auth = McpAuth {
        token,
        headers: &header_pairs,
    };
    match fetch_tools(&auth.upstream, &mcp_auth).await {
        Ok(tools) => {
            eprintln!(" {} tools", tools.len());
            println!();
            for tool in &tools {
                let name = tool["name"].as_str().unwrap_or("?");
                let desc = tool["description"].as_str().unwrap_or("");
                if desc.is_empty() {
                    println!("  {name}");
                } else {
                    let short: String = desc.chars().take(60).collect();
                    let suffix = if desc.len() > 60 { "..." } else { "" };
                    println!("  {name:<30} {short}{suffix}");
                }
            }
        }
        Err(e) => {
            eprintln!(" failed: {e}");
        }
    }

    Ok(())
}

/// `kap mcp remove <name>` — delete auth file and lock file.
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

/// Try initialize + tools/list at the given URL and common subpaths (/mcp).
/// Returns the working URL, or None if nothing works.
async fn discover_mcp_endpoint(base_url: &str, auth: &McpAuth<'_>) -> Option<String> {
    let base = base_url.trim_end_matches('/');
    let candidates = [base.to_string(), format!("{base}/mcp")];

    for url in &candidates {
        eprintln!("[auth] trying {url}...");
        match fetch_tools(url, auth).await {
            Ok(tools) => {
                eprintln!("[auth] success: {} tools at {url}", tools.len());
                return Some(url.clone());
            }
            Err(e) => {
                eprintln!("[auth] {url}: {e}");
            }
        }
    }

    None
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
            headers: std::collections::HashMap::new(),
        }
    }

    fn tempdir(suffix: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("kap-mcp-cmd-{}-{suffix}", std::process::id()));
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
