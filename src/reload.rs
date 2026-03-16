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

/// File fingerprint: mtime + size. On macOS with Docker Desktop,
/// bind-mount mtime propagation is unreliable, so we also check size.
/// For same-size edits, we fall back to content hashing.
fn file_fingerprint(path: &str) -> Option<(SystemTime, u64, u64)> {
    let meta = std::fs::metadata(path).ok()?;
    let mtime = meta.modified().ok()?;
    let size = meta.len();
    // Cheap content hash: read first+last 4KB and hash with file size
    let hash = std::fs::read(path)
        .ok()
        .map(|data| {
            let mut h: u64 = data.len() as u64;
            for &b in data.iter().take(4096).chain(data.iter().rev().take(4096)) {
                h = h.wrapping_mul(31).wrapping_add(b as u64);
            }
            h
        })
        .unwrap_or(0);
    Some((mtime, size, hash))
}

pub async fn watch_config(
    path: String,
    allowlist: Shared<Allowlist>,
    cli_tools: Shared<CliTools>,
    mcp_filters: Shared<McpFilters>,
    shim_dir: PathBuf,
) {
    let mut last_fingerprint = file_fingerprint(&path);
    loop {
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        let fingerprint = file_fingerprint(&path);
        if fingerprint == last_fingerprint {
            continue;
        }
        last_fingerprint = fingerprint;
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

                // Update shim scripts and post-start script on shared volume
                if let Err(e) = write_shims(&cfg, &shim_dir) {
                    eprintln!("[sidecar] failed to update shims: {e}");
                }
                if let Err(e) = write_post_start_script(&cfg, &shim_dir) {
                    eprintln!("[sidecar] failed to update post-start script: {e}");
                }
                if let Err(e) = write_gitconfig(&cfg, &shim_dir) {
                    eprintln!("[sidecar] failed to update gitconfig: {e}");
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

/// Write `/opt/kap/bin/kap-post-start` script based on `[setup]` and top-level config.
/// The script is referenced by devcontainer.json postStartCommand and runs
/// inside the app container on each start.
pub fn write_post_start_script(cfg: &Config, shim_dir: &Path) -> anyhow::Result<()> {
    let script_path = shim_dir.join(POST_START_FILENAME);
    let setup = cfg.setup.as_ref();

    // If nothing to do, remove stale script and return
    let setup_enabled = setup.is_some_and(|s| s.claude_code || s.codex || s.gh);
    if !setup_enabled && !cfg.ssh_agent {
        let _ = std::fs::remove_file(&script_path);
        return Ok(());
    }

    let mut lines: Vec<&str> = vec![
        "#!/bin/bash",
        "set -e",
        "",
        "# Generated by kap from kap.toml — do not edit.",
        "# Regenerated by the sidecar on startup and config reload.",
        "",
        "# Ensure /opt/kap/bin is on PATH for interactive shells.",
        "# remoteEnv.PATH only applies to devcontainer exec, but login shells",
        "# (zsh, bash) reset PATH via /etc/profile. This drop-in restores it.",
        "echo 'export PATH=\"/opt/kap/bin:$PATH\"' | sudo tee /etc/profile.d/kap-path.sh >/dev/null",
    ];

    if let Some(setup) = setup {
        // Check for the real binary, excluding /opt/kap/bin shims.
        // The shims mask the real binary in `command -v`, so we filter PATH.
        lines.extend_from_slice(&[
            "",
            "# PATH without kap shims (for install checks)",
            "REAL_PATH=$(echo \"$PATH\" | tr ':' '\\n' | grep -v /opt/kap | tr '\\n' ':')",
        ]);

        if setup.claude_code {
            lines.extend_from_slice(&[
                "",
                "# Install Claude Code",
                "PATH=\"$REAL_PATH\" command -v claude >/dev/null 2>&1 || curl -fsSL https://claude.ai/install.sh | bash",
                "# Ensure onboarding is skipped (installer creates the file without this flag)",
                "if command -v python3 >/dev/null 2>&1; then",
                "  python3 -c \"import json,pathlib; p=pathlib.Path.home()/'.claude.json'; d=json.loads(p.read_text()) if p.exists() else {}; d['hasCompletedOnboarding']=True; p.write_text(json.dumps(d))\"",
                "elif [ ! -f ~/.claude.json ]; then",
                "  echo '{\"hasCompletedOnboarding\":true}' > ~/.claude.json",
                "fi",
            ]);
        }

        if setup.codex {
            lines.extend_from_slice(&[
                "",
                "# Install Codex",
                "PATH=\"$REAL_PATH\" command -v codex >/dev/null 2>&1 || { command -v npm >/dev/null 2>&1 && npm install -g @openai/codex || true; }",
            ]);
        }

        if setup.gh {
            lines.extend_from_slice(&[
                "",
                "# Install GitHub CLI",
                "PATH=\"$REAL_PATH\" command -v gh >/dev/null 2>&1 || {",
                "  curl -fsSL https://cli.github.com/packages/githubcli-archive-keyring.gpg \\",
                "    | sudo dd of=/usr/share/keyrings/githubcli-archive-keyring.gpg 2>/dev/null",
                "  echo \"deb [arch=$(dpkg --print-architecture) signed-by=/usr/share/keyrings/githubcli-archive-keyring.gpg] https://cli.github.com/packages stable main\" \\",
                "    | sudo tee /etc/apt/sources.list.d/github-cli.list >/dev/null",
                "  sudo apt-get update -qq && sudo apt-get install -y gh",
                "}",
            ]);
        }
    }

    if cfg.ssh_agent {
        lines.extend_from_slice(&[
            "",
            "# Route SSH through the kap proxy (all hosts, not just git).",
            "# Only write if no user-provided SSH config exists.",
            "if [ ! -f ~/.ssh/config ]; then",
            "  mkdir -p ~/.ssh && chmod 700 ~/.ssh",
            "  cat > ~/.ssh/config << 'SSHEOF'",
            "Host *",
            "    ProxyCommand /opt/kap/kap sidecar-connect-proxy %h %p",
            "    StrictHostKeyChecking accept-new",
            "SSHEOF",
            "  chmod 600 ~/.ssh/config",
            "fi",
        ]);
    }

    if cfg.ssh_signing {
        lines.extend_from_slice(&[
            "",
            "# SSH commit signing: write signingkey to a file (ssh-keygen needs a path,",
            "# not an inline key) and override it via ~/.gitconfig-kap. The sidecar writes",
            "# the main wrapper (/opt/kap/gitconfig) with gpg.ssh.program override;",
            "# GIT_CONFIG_GLOBAL is set by the compose overlay so it works in all contexts.",
            "KEY=$(GIT_CONFIG_GLOBAL= git config --global user.signingkey 2>/dev/null || true)",
            "EMAIL=$(GIT_CONFIG_GLOBAL= git config --global user.email 2>/dev/null || true)",
            "if [ -n \"$KEY\" ]; then",
            "  echo \"$KEY\" > ~/.ssh-signing-key.pub",
            "  git config -f ~/.gitconfig-kap user.signingkey ~/.ssh-signing-key.pub",
            "  if [ -n \"$EMAIL\" ]; then",
            "    echo \"$EMAIL $KEY\" > ~/.ssh-allowed-signers",
            "    git config -f ~/.gitconfig-kap gpg.ssh.allowedSignersFile ~/.ssh-allowed-signers",
            "  fi",
            "fi",
        ]);
    } else {
        lines.extend_from_slice(&[
            "",
            "# Auto-disable commit signing if the configured program doesn't exist in the container",
            "SIGN_PROG=$(GIT_CONFIG_GLOBAL= git config --global gpg.ssh.program 2>/dev/null || true)",
            "if [ -n \"$SIGN_PROG\" ] && ! command -v \"$SIGN_PROG\" >/dev/null 2>&1; then",
            "  git config -f ~/.gitconfig-kap commit.gpgsign false",
            "fi",
        ]);
    }

    let content = lines.join("\n") + "\n";

    // Only write if content changed
    let needs_write = std::fs::read_to_string(&script_path)
        .map(|existing| existing != content)
        .unwrap_or(true);
    if needs_write {
        std::fs::create_dir_all(shim_dir)?;
        std::fs::write(&script_path, &content)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script_path, std::fs::Permissions::from_mode(0o755))?;
        }
        eprintln!("[sidecar] wrote {POST_START_FILENAME}");
    }

    Ok(())
}

pub const POST_START_FILENAME: &str = "kap-post-start";
pub const GITCONFIG_FILENAME: &str = "gitconfig";

/// Write `/opt/kap/gitconfig` — a git config wrapper that includes the user's
/// `~/.gitconfig` and overrides settings that don't work inside the container.
///
/// The compose overlay sets `GIT_CONFIG_GLOBAL=/opt/kap/gitconfig` on the app
/// container, so this file is used as the global git config in all contexts
/// (interactive shells, `devcontainer exec`, scripts).
///
/// When `ssh_signing = true`: overrides `gpg.ssh.program` to `/usr/bin/ssh-keygen`.
/// When `ssh_signing = false`: just a passthrough (post-start script may add
/// `[commit] gpgsign = false` to `~/.gitconfig-kap` if the signing program is missing).
///
/// Both variants include `~/.gitconfig-kap` for overrides that need to be written
/// from inside the app container (e.g. `user.signingkey` file path).
pub fn write_gitconfig(cfg: &Config, shim_dir: &Path) -> anyhow::Result<()> {
    // Write to /opt/kap/gitconfig (volume root), not /opt/kap/bin/gitconfig
    let path = shim_dir
        .parent()
        .unwrap_or(shim_dir)
        .join(GITCONFIG_FILENAME);
    let content = if cfg.ssh_signing {
        // Override gpg.ssh.program (the host's macOS binary doesn't exist in Linux).
        // ~/.gitconfig-kap is written by the post-start script with user.signingkey.
        "[include]\n    path = ~/.gitconfig\n[include]\n    path = ~/.gitconfig-kap\n[gpg \"ssh\"]\n    program = /usr/bin/ssh-keygen\n"
    } else {
        // Passthrough: ~/.gitconfig-kap may disable signing if the program is missing.
        "[include]\n    path = ~/.gitconfig\n[include]\n    path = ~/.gitconfig-kap\n"
    };
    let needs_write = std::fs::read_to_string(&path)
        .map(|existing| existing != content)
        .unwrap_or(true);
    if needs_write {
        std::fs::create_dir_all(shim_dir)?;
        std::fs::write(&path, content)?;
        eprintln!("[sidecar] wrote {GITCONFIG_FILENAME}");
    }
    Ok(())
}

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
            if name == "kap" || name == POST_START_FILENAME {
                continue; // keep the kap binary and post-start script
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

    #[test]
    fn file_fingerprint_detects_content_change() {
        let dir = tempdir("fingerprint-change");
        let path = dir.join("test.toml");
        std::fs::write(&path, "version = 1").unwrap();
        let fp1 = file_fingerprint(path.to_str().unwrap());

        // Change content (same length to test that hash detects it)
        std::fs::write(&path, "version = 2").unwrap();
        let fp2 = file_fingerprint(path.to_str().unwrap());

        assert!(fp1.is_some());
        assert!(fp2.is_some());
        assert_ne!(fp1, fp2);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn file_fingerprint_stable_for_same_content() {
        let dir = tempdir("fingerprint-stable");
        let path = dir.join("test.toml");
        std::fs::write(&path, "version = 1").unwrap();
        let fp1 = file_fingerprint(path.to_str().unwrap());
        let fp2 = file_fingerprint(path.to_str().unwrap());
        assert_eq!(fp1, fp2);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn file_fingerprint_nonexistent_returns_none() {
        assert!(file_fingerprint("/nonexistent/path.toml").is_none());
    }

    #[test]
    fn post_start_script_claude_and_gh() {
        let dir = tempdir("post-start-claude-gh");
        let toml = r#"
[setup]
claude_code = true
gh = true
"#;
        let cfg: Config = toml::from_str(toml).unwrap();
        write_post_start_script(&cfg, &dir).unwrap();

        let script = std::fs::read_to_string(dir.join(POST_START_FILENAME)).unwrap();
        assert!(script.starts_with("#!/bin/bash"));
        assert!(script.contains("claude.ai/install.sh"));
        assert!(script.contains("hasCompletedOnboarding"));
        assert!(script.contains("github-cli"));
        assert!(!script.contains("codex"));
        assert!(!script.contains("ssh-keygen"));

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir.join(POST_START_FILENAME))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o755, 0o755);
        }

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn post_start_script_all_enabled() {
        let dir = tempdir("post-start-all");
        let toml = r#"
ssh_signing = true

[setup]
claude_code = true
codex = true
gh = true
"#;
        let cfg: Config = toml::from_str(toml).unwrap();
        write_post_start_script(&cfg, &dir).unwrap();

        let script = std::fs::read_to_string(dir.join(POST_START_FILENAME)).unwrap();
        assert!(script.contains("claude.ai/install.sh"));
        assert!(script.contains("@openai/codex"));
        assert!(script.contains("github-cli"));
        assert!(script.contains(".ssh-signing-key.pub"));
        assert!(script.contains("git config -f ~/.gitconfig-kap user.signingkey"));

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn post_start_script_no_setup_removes_stale() {
        let dir = tempdir("post-start-none");
        let script_path = dir.join(POST_START_FILENAME);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&script_path, "stale script").unwrap();

        let cfg: Config = toml::from_str("ssh_agent = false").unwrap();
        write_post_start_script(&cfg, &dir).unwrap();

        assert!(!script_path.exists());

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn post_start_script_idempotent() {
        let dir = tempdir("post-start-idempotent");
        let toml = r#"
[setup]
claude_code = true
"#;
        let cfg: Config = toml::from_str(toml).unwrap();
        write_post_start_script(&cfg, &dir).unwrap();
        let mtime1 = std::fs::metadata(dir.join(POST_START_FILENAME))
            .unwrap()
            .modified()
            .unwrap();

        std::thread::sleep(std::time::Duration::from_millis(50));

        write_post_start_script(&cfg, &dir).unwrap();
        let mtime2 = std::fs::metadata(dir.join(POST_START_FILENAME))
            .unwrap()
            .modified()
            .unwrap();

        assert_eq!(mtime1, mtime2);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn post_start_script_ssh_signing_override() {
        let dir = tempdir("post-start-signing-override");
        let toml = "ssh_signing = true";
        let cfg: Config = toml::from_str(toml).unwrap();
        write_post_start_script(&cfg, &dir).unwrap();

        let script = std::fs::read_to_string(dir.join(POST_START_FILENAME)).unwrap();
        assert!(script.contains(".ssh-signing-key.pub"));
        assert!(script.contains("git config -f ~/.gitconfig-kap user.signingkey"));
        // Should NOT contain the auto-disable branch
        assert!(!script.contains("gpgsign = false"));

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn post_start_script_ssh_signing_auto_disable() {
        let dir = tempdir("post-start-auto-disable");
        let toml = "ssh_signing = false";
        let cfg: Config = toml::from_str(toml).unwrap();
        write_post_start_script(&cfg, &dir).unwrap();

        let script = std::fs::read_to_string(dir.join(POST_START_FILENAME)).unwrap();
        assert!(script.contains("SIGN_PROG="));
        assert!(script.contains("git config -f ~/.gitconfig-kap commit.gpgsign false"));
        assert!(!script.contains("ssh-keygen"));

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn write_gitconfig_ssh_signing_true() {
        let dir = tempdir("gitconfig-signing");
        let bin = dir.join("bin");
        std::fs::create_dir_all(&bin).unwrap();

        let cfg: Config = toml::from_str("ssh_signing = true").unwrap();
        write_gitconfig(&cfg, &bin).unwrap();

        // Written to parent of shim_dir (volume root)
        let content = std::fs::read_to_string(dir.join(GITCONFIG_FILENAME)).unwrap();
        assert!(content.contains("[include]\n    path = ~/.gitconfig"));
        assert!(content.contains("[include]\n    path = ~/.gitconfig-kap"));
        assert!(content.contains("program = /usr/bin/ssh-keygen"));

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn write_gitconfig_ssh_signing_false() {
        let dir = tempdir("gitconfig-passthrough");
        let bin = dir.join("bin");
        std::fs::create_dir_all(&bin).unwrap();

        let cfg: Config = toml::from_str("ssh_signing = false").unwrap();
        write_gitconfig(&cfg, &bin).unwrap();

        let content = std::fs::read_to_string(dir.join(GITCONFIG_FILENAME)).unwrap();
        assert!(content.contains("[include]\n    path = ~/.gitconfig"));
        assert!(content.contains("[include]\n    path = ~/.gitconfig-kap"));
        assert!(!content.contains("ssh-keygen"));

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn write_shims_preserves_post_start_script() {
        let dir = tempdir("shims-preserve-post-start");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(POST_START_FILENAME), "#!/bin/bash\n").unwrap();

        let cfg: Config = toml::from_str("").unwrap();
        write_shims(&cfg, &dir).unwrap();

        // post-start script should not be removed by stale shim cleanup
        assert!(dir.join(POST_START_FILENAME).exists());

        std::fs::remove_dir_all(&dir).unwrap();
    }
}
