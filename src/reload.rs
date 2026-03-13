/// Hot-reload kap.toml config without container restart.
///
/// Polls the config file's mtime every 2 seconds and swaps shared state
/// atomically when changes are detected. Also manages CLI shim scripts
/// on the shared kap-bin volume.
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::SystemTime;

use crate::cli::filter::CommandFilter;
use crate::config::{CliToolMode, Config};
use crate::mcp::filter::ToolFilter;
use crate::proxy::allowlist::Allowlist;

// ---------------------------------------------------------------------------
// Shared<T>: a double-Arc pattern for cheap reads + atomic swaps
// ---------------------------------------------------------------------------

/// A value that can be read cheaply and swapped atomically.
/// Readers clone the inner Arc (~1ns atomic increment).
/// Writers briefly hold the RwLock to swap the Arc pointer.
pub type Shared<T> = Arc<RwLock<Arc<T>>>;

pub fn new_shared<T>(val: T) -> Shared<T> {
    Arc::new(RwLock::new(Arc::new(val)))
}

pub fn load<T>(shared: &Shared<T>) -> Arc<T> {
    shared.read().unwrap().clone()
}

pub fn store<T>(shared: &Shared<T>, val: T) {
    *shared.write().unwrap() = Arc::new(val);
}

// ---------------------------------------------------------------------------
// CLI tool state (shared between CLI proxy and reloader)
// ---------------------------------------------------------------------------

pub struct CliTool {
    pub filter: CommandFilter,
    pub env_vars: Vec<String>,
    pub mode: CliToolMode,
}

pub struct CliTools {
    pub tools: HashMap<String, CliTool>,
}

impl CliTools {
    pub fn from_config(cfg: &Config) -> Self {
        let mut tools = HashMap::new();
        if let Some(ref cli) = cfg.cli {
            for tool_cfg in &cli.tools {
                let env_vars = if tool_cfg.env.is_empty() && tool_cfg.mode == CliToolMode::Direct {
                    crate::init::default_env_for_tool(&tool_cfg.name)
                } else {
                    tool_cfg.env.clone()
                };
                tools.insert(
                    tool_cfg.name.clone(),
                    CliTool {
                        filter: CommandFilter::new(&tool_cfg.allow, &tool_cfg.deny),
                        env_vars,
                        mode: tool_cfg.mode.clone(),
                    },
                );
            }
        }
        Self { tools }
    }
}

// ---------------------------------------------------------------------------
// MCP tool filters (shared between MCP proxy and reloader)
// ---------------------------------------------------------------------------

pub struct McpFilters {
    pub filters: HashMap<String, ToolFilter>,
}

impl McpFilters {
    pub fn from_config(cfg: &Config) -> Self {
        let mut filters = HashMap::new();
        if let Some(ref mcp) = cfg.mcp {
            for server in &mcp.servers {
                filters.insert(
                    server.name.clone(),
                    ToolFilter::new(&server.allow, &server.deny),
                );
            }
        }
        Self { filters }
    }
}

// ---------------------------------------------------------------------------
// Config watcher
// ---------------------------------------------------------------------------

fn file_mtime(path: &str) -> Option<SystemTime> {
    std::fs::metadata(path).ok().and_then(|m| m.modified().ok())
}

pub async fn watch_config(
    path: String,
    allowlist: Shared<Allowlist>,
    cli_tools: Shared<CliTools>,
    mcp_filters: Shared<McpFilters>,
    shim_dir: PathBuf,
) {
    let mut last_mtime = file_mtime(&path);
    loop {
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        let mtime = file_mtime(&path);
        if mtime == last_mtime {
            continue;
        }
        last_mtime = mtime;
        match Config::load(&path) {
            Ok(cfg) => {
                // Rebuild allowlist (include MCP upstream domains)
                let mcp_domains = cfg.mcp_upstream_domains();
                let mut all_allow: Vec<String> = cfg.allow_domains().to_vec();
                all_allow.extend(mcp_domains);
                store(
                    &allowlist,
                    Allowlist::new(&all_allow, &cfg.proxy.network.deny),
                );

                // Rebuild CLI tools
                store(&cli_tools, CliTools::from_config(&cfg));

                // Rebuild MCP filters
                store(&mcp_filters, McpFilters::from_config(&cfg));

                // Update shim scripts on shared volume
                if let Err(e) = write_shims(&cfg, &shim_dir) {
                    eprintln!("[sidecar] failed to update shims: {e}");
                }

                eprintln!("[sidecar] config reloaded");
            }
            Err(e) => {
                eprintln!("[sidecar] config reload failed: {e}");
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Shim management (write shim scripts to shared volume)
// ---------------------------------------------------------------------------

const SHIM_SCRIPT: &str = r#"#!/bin/sh
exec /opt/kap/kap sidecar-cli-shim "$(basename "$0")" "$@"
"#;

/// Write shim scripts for all configured CLI tools to the shim directory.
/// Removes stale shims for tools no longer in the config.
pub fn write_shims(cfg: &Config, shim_dir: &Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(shim_dir)?;

    // Collect configured tool names
    let tool_names: Vec<String> = cfg
        .cli
        .as_ref()
        .map(|c| c.tools.iter().map(|t| t.name.clone()).collect())
        .unwrap_or_default();

    // Write shims for configured tools
    for name in &tool_names {
        let shim_path = shim_dir.join(name);
        // Only write if missing or content differs (avoid unnecessary writes)
        let needs_write = std::fs::read_to_string(&shim_path)
            .map(|content| content != SHIM_SCRIPT)
            .unwrap_or(true);
        if needs_write {
            std::fs::write(&shim_path, SHIM_SCRIPT)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&shim_path, std::fs::Permissions::from_mode(0o755))?;
            }
            eprintln!("[sidecar] wrote shim: {}", shim_path.display());
        }
    }

    // Remove stale shims (files in shim_dir not in tool_names, excluding the kap binary)
    if let Ok(entries) = std::fs::read_dir(shim_dir) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name == "kap" {
                continue; // keep the kap binary itself
            }
            if !tool_names.contains(&name) {
                let _ = std::fs::remove_file(entry.path());
                eprintln!("[sidecar] removed stale shim: {name}");
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tempdir(suffix: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("kap-reload-{}-{suffix}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn shared_load_store() {
        let shared = new_shared(42u32);
        assert_eq!(*load(&shared), 42);
        store(&shared, 99);
        assert_eq!(*load(&shared), 99);
    }

    #[test]
    fn cli_tools_from_config() {
        let toml = r#"
[cli]
[[cli.tools]]
name = "gh"
mode = "direct"
allow = ["*"]

[[cli.tools]]
name = "aws"
allow = ["s3 *"]
deny = ["iam *"]
env = ["AWS_ACCESS_KEY_ID"]
"#;
        let cfg: Config = toml::from_str(toml).unwrap();
        let tools = CliTools::from_config(&cfg);
        assert_eq!(tools.tools.len(), 2);

        let gh = &tools.tools["gh"];
        assert_eq!(gh.mode, CliToolMode::Direct);
        assert_eq!(gh.env_vars, vec!["GH_TOKEN"]); // auto-resolved

        let aws = &tools.tools["aws"];
        assert_eq!(aws.mode, CliToolMode::Proxy);
        assert_eq!(aws.env_vars, vec!["AWS_ACCESS_KEY_ID"]);
    }

    #[test]
    fn mcp_filters_from_config() {
        let toml = r#"
[mcp]
[[mcp.servers]]
name = "github"
allow = ["get_*"]
deny = ["delete_*"]
"#;
        let cfg: Config = toml::from_str(toml).unwrap();
        let filters = McpFilters::from_config(&cfg);
        assert_eq!(filters.filters.len(), 1);
        assert!(filters.filters.contains_key("github"));
    }

    #[test]
    fn write_shims_creates_scripts() {
        let dir = tempdir("shims-create");
        let toml = r#"
[cli]
[[cli.tools]]
name = "gh"
[[cli.tools]]
name = "aws"
"#;
        let cfg: Config = toml::from_str(toml).unwrap();
        write_shims(&cfg, &dir).unwrap();

        let gh = std::fs::read_to_string(dir.join("gh")).unwrap();
        assert_eq!(gh, SHIM_SCRIPT);
        let aws = std::fs::read_to_string(dir.join("aws")).unwrap();
        assert_eq!(aws, SHIM_SCRIPT);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir.join("gh"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o755, 0o755);
        }

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn write_shims_removes_stale() {
        let dir = tempdir("shims-stale");

        // Write an initial shim
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("old-tool"), "stale").unwrap();
        // Keep a "kap" binary (should not be removed)
        std::fs::write(dir.join("kap"), "binary").unwrap();

        let toml = r#"
[cli]
[[cli.tools]]
name = "gh"
"#;
        let cfg: Config = toml::from_str(toml).unwrap();
        write_shims(&cfg, &dir).unwrap();

        assert!(dir.join("gh").exists());
        assert!(!dir.join("old-tool").exists()); // removed
        assert!(dir.join("kap").exists()); // preserved

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn write_shims_idempotent() {
        let dir = tempdir("shims-idempotent");
        let toml = r#"
[cli]
[[cli.tools]]
name = "gh"
"#;
        let cfg: Config = toml::from_str(toml).unwrap();
        write_shims(&cfg, &dir).unwrap();
        let mtime1 = std::fs::metadata(dir.join("gh"))
            .unwrap()
            .modified()
            .unwrap();

        // Small delay to ensure mtime would change if file was rewritten
        std::thread::sleep(std::time::Duration::from_millis(50));

        write_shims(&cfg, &dir).unwrap();
        let mtime2 = std::fs::metadata(dir.join("gh"))
            .unwrap()
            .modified()
            .unwrap();

        // Should not have rewritten (content unchanged)
        assert_eq!(mtime1, mtime2);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn write_shims_no_cli_config() {
        let dir = tempdir("shims-none");
        let cfg: Config = toml::from_str("").unwrap();
        write_shims(&cfg, &dir).unwrap();
        // Dir should be empty (no tools configured)
        let entries: Vec<_> = std::fs::read_dir(&dir).unwrap().collect();
        assert!(entries.is_empty());

        std::fs::remove_dir_all(&dir).unwrap();
    }
}
