/// Generate .devcontainer/.env and docker-compose.kap.yml from host environment.
///
/// Reads kap.toml to find which env vars the proxy sidecar needs
/// (from token_env and ${VAR} references in headers), then writes
/// them to .env so docker-compose passes them to the container.
///
/// Also regenerates the compose overlay (docker-compose.kap.yml) so it
/// always matches the installed kap version and kap.toml config.
use anyhow::{Context, Result};
use std::path::Path;

/// Generic CLI shim: uses argv[0] to determine the tool name, then forwards
/// to the kap binary which detects the invocation name (busybox pattern).
/// One file is mounted at /usr/local/bin/<tool> for each configured tool.
const CLI_SHIM: &str =
    "#!/bin/sh\nexec /opt/kap/kap sidecar-cli-shim \"$(basename \"$0\")\" \"$@\"\n";

pub fn run(project_dir: &str) -> Result<()> {
    let project = Path::new(project_dir);
    let devcontainer_dir = project.join(".devcontainer");
    let config_path = devcontainer_dir.join("kap.toml");
    let env_path = devcontainer_dir.join(".env");

    if !config_path.exists() {
        anyhow::bail!(
            "No kap.toml found at {}. Run `kap init` first to set up your devcontainer.",
            config_path.display()
        );
    }

    // Regenerate compose overlay (non-fatal: warn and continue if it fails)
    if let Err(e) = regenerate_overlay(&devcontainer_dir, &config_path) {
        eprintln!("[sidecar-init] warning: could not regenerate overlay: {e}");
    }

    let needed_vars = vars_from_config(&config_path)?;

    // Load existing .env values and shell patterns (# KEY=$(cmd))
    let (existing, existing_patterns) = load_env_file(&env_path);

    let mut lines: Vec<String> = Vec::new();

    for var in &needed_vars {
        // Re-evaluate shell patterns stored as comments (# KEY=$(cmd))
        if let Some(pattern) = existing_patterns.get(var.as_str()) {
            let resolved = eval_shell_substitution(pattern);
            if !resolved.is_empty() {
                lines.push(format!("# {var}={pattern}"));
                lines.push(format!("{var}={resolved}"));
                continue;
            }
        }
        // Keep existing value (or evaluate if it contains $(cmd))
        if let Some(val) = existing.get(var.as_str()) {
            if val.contains("$(") {
                let resolved = eval_shell_substitution(val);
                if !resolved.is_empty() {
                    lines.push(format!("# {var}={val}"));
                    lines.push(format!("{var}={resolved}"));
                    continue;
                }
            } else if !val.is_empty() {
                lines.push(format!("{var}={val}"));
                continue;
            }
        }
        // Otherwise try host environment
        if let Ok(val) = std::env::var(var)
            && !val.is_empty()
        {
            eprintln!("[sidecar-init] {var} (from host env)");
            lines.push(format!("{var}={val}"));
            continue;
        }
        // Last resort: try well-known shell expression (e.g. GH_TOKEN -> `gh auth token`)
        if let Some(expr) = crate::init::env_var_default(var) {
            let resolved = eval_shell_substitution(expr);
            if !resolved.is_empty() {
                eprintln!("[sidecar-init] {var} (from {expr})");
                lines.push(format!("# {var}={expr}"));
                lines.push(format!("{var}={resolved}"));
            }
        }
    }

    let content = lines.join("\n");
    if !content.is_empty() {
        std::fs::write(&env_path, content + "\n")?;
        eprintln!(
            "[sidecar-init] wrote {} vars to {}",
            lines.len(),
            env_path.display()
        );
    } else if !env_path.exists() {
        // Only create empty file if none exists
        std::fs::write(&env_path, "")?;
    }

    Ok(())
}

/// Regenerate docker-compose.kap.yml from kap.toml config.
fn regenerate_overlay(devcontainer_dir: &Path, config_path: &Path) -> Result<()> {
    let overlay_path = devcontainer_dir.join(crate::init::OVERLAY_FILENAME);

    // Read service name from devcontainer.json
    let service_name = crate::init::read_service_name(devcontainer_dir)?;

    // Read compose config from kap.toml
    let content = std::fs::read_to_string(config_path)
        .with_context(|| format!("reading {}", config_path.display()))?;
    let config: crate::config::Config =
        toml::from_str(&content).with_context(|| format!("parsing {}", config_path.display()))?;
    let compose_config = config.compose.unwrap_or_default();
    let cli_tools: Vec<String> = config
        .cli
        .as_ref()
        .map(|c| c.tools.iter().map(|t| t.name.clone()).collect())
        .unwrap_or_default();

    // Derive project root from devcontainer_dir (parent of .devcontainer/)
    let project_dir = devcontainer_dir.parent().unwrap_or(devcontainer_dir);
    let subnet_prefix = crate::init::derive_subnet(project_dir);
    let project_name = crate::init::read_project_name(devcontainer_dir);
    let overlay = crate::init::generate_overlay(
        &service_name,
        &compose_config,
        &cli_tools,
        &subnet_prefix,
        &project_name,
    );
    std::fs::write(&overlay_path, &overlay)
        .with_context(|| format!("writing {}", overlay_path.display()))?;
    eprintln!(
        "[sidecar-init] regenerated {}",
        crate::init::OVERLAY_FILENAME
    );

    // Write single generic CLI shim (mounted as each tool name in the overlay)
    if !cli_tools.is_empty() {
        let shim_path = devcontainer_dir.join("cli-shim.sh");
        std::fs::write(&shim_path, CLI_SHIM)
            .with_context(|| format!("writing {}", shim_path.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&shim_path, std::fs::Permissions::from_mode(0o755))?;
        }
        eprintln!("[sidecar-init] wrote cli-shim.sh");
    }

    Ok(())
}

/// Evaluate shell command substitutions like `$(gh auth token)`.
/// Returns the value as-is if it doesn't contain `$(...)`.
fn eval_shell_substitution(val: &str) -> String {
    if !val.contains("$(") {
        return val.to_string();
    }
    match std::process::Command::new("sh")
        .arg("-c")
        .arg(format!("printf '%s' {val}"))
        .output()
    {
        Ok(output) if output.status.success() => {
            let result = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if result.is_empty() {
                eprintln!("[sidecar-init] warning: {val} evaluated to empty");
            }
            result
        }
        _ => {
            eprintln!("[sidecar-init] warning: failed to evaluate {val}");
            String::new()
        }
    }
}

/// Load existing KEY=VALUE pairs from a .env file.
/// Also returns shell patterns from comments like `# KEY=$(cmd)`.
fn load_env_file(
    path: &Path,
) -> (
    std::collections::HashMap<String, String>,
    std::collections::HashMap<String, String>,
) {
    let mut values = std::collections::HashMap::new();
    let mut patterns = std::collections::HashMap::new();
    let Ok(content) = std::fs::read_to_string(path) else {
        return (values, patterns);
    };
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        // Extract shell patterns from comments: # KEY=$(cmd)
        if let Some(comment) = line.strip_prefix("# ") {
            if let Some((key, val)) = comment.split_once('=') {
                let val = val.trim();
                if val.contains("$(") {
                    patterns.insert(key.trim().to_string(), val.to_string());
                }
            }
            continue;
        }
        if line.starts_with('#') {
            continue;
        }
        if let Some((key, val)) = line.split_once('=') {
            values.insert(key.trim().to_string(), val.trim().to_string());
        }
    }
    (values, patterns)
}

/// Parse kap.toml and collect all env var names referenced by MCP server configs.
fn vars_from_config(path: &Path) -> Result<Vec<String>> {
    let content =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let config: crate::config::Config =
        toml::from_str(&content).with_context(|| format!("parsing {}", path.display()))?;

    let mut vars = Vec::new();

    if let Some(mcp) = &config.mcp {
        for server in &mcp.servers {
            // token_env is itself an env var name
            if let Some(ref var) = server.token_env {
                vars.push(var.clone());
            }
            // headers can contain ${VAR} references
            for value in server.headers.values() {
                extract_env_refs(value, &mut vars);
            }
        }
    }

    // CLI tools need their env vars on the sidecar
    if let Some(cli) = &config.cli {
        for tool in &cli.tools {
            for var in &tool.env {
                vars.push(var.clone());
            }
        }
    }

    vars.sort();
    vars.dedup();
    Ok(vars)
}

/// Extract ${VAR} references from a string.
fn extract_env_refs(s: &str, vars: &mut Vec<String>) {
    let mut rest = s;
    while let Some(start) = rest.find("${") {
        rest = &rest[start + 2..];
        if let Some(end) = rest.find('}') {
            let var = &rest[..end];
            if !var.is_empty() {
                vars.push(var.to_string());
            }
            rest = &rest[end + 1..];
        } else {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_env_refs_finds_vars() {
        let mut vars = Vec::new();
        extract_env_refs("Bearer ${GH_TOKEN}", &mut vars);
        assert_eq!(vars, vec!["GH_TOKEN"]);
    }

    #[test]
    fn extract_env_refs_multiple() {
        let mut vars = Vec::new();
        extract_env_refs("${FOO} and ${BAR}", &mut vars);
        assert_eq!(vars, vec!["FOO", "BAR"]);
    }

    #[test]
    fn extract_env_refs_none() {
        let mut vars = Vec::new();
        extract_env_refs("static-value", &mut vars);
        assert!(vars.is_empty());
    }

    #[test]
    fn extract_env_refs_unclosed_brace() {
        let mut vars = Vec::new();
        extract_env_refs("${UNCLOSED", &mut vars);
        assert!(vars.is_empty());
    }

    #[test]
    fn extract_env_refs_empty_var_name() {
        let mut vars = Vec::new();
        extract_env_refs("${}", &mut vars);
        assert!(vars.is_empty());
    }

    #[test]
    fn load_env_file_parses_key_value() {
        let dir = std::env::temp_dir().join(format!("kap-loadenv-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(".env");
        std::fs::write(&path, "FOO=bar\nBAZ=qux\n").unwrap();

        let (values, patterns) = load_env_file(&path);
        assert_eq!(values["FOO"], "bar");
        assert_eq!(values["BAZ"], "qux");
        assert_eq!(values.len(), 2);
        assert!(patterns.is_empty());

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn load_env_file_skips_comments_and_blanks() {
        let dir = std::env::temp_dir().join(format!("kap-loadenv2-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(".env");
        std::fs::write(&path, "# comment\n\nKEY=val\n  \n# another\n").unwrap();

        let (values, _) = load_env_file(&path);
        assert_eq!(values.len(), 1);
        assert_eq!(values["KEY"], "val");

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn load_env_file_extracts_shell_patterns() {
        let dir = std::env::temp_dir().join(format!("kap-loadenv3-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(".env");
        std::fs::write(&path, "# GH_TOKEN=$(gh auth token)\nGH_TOKEN=old_value\n").unwrap();

        let (values, patterns) = load_env_file(&path);
        assert_eq!(values["GH_TOKEN"], "old_value");
        assert_eq!(patterns["GH_TOKEN"], "$(gh auth token)");

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn shell_pattern_survives_two_loads() {
        // Simulates what happens across two init-env runs:
        // 1. .env has GH_TOKEN=$(echo hello)
        // 2. First load: evaluates to "hello", writes # GH_TOKEN=$(echo hello)\nGH_TOKEN=hello
        // 3. Second load: finds pattern in comment, re-evaluates, keeps pattern
        let dir = std::env::temp_dir().join(format!("kap-pattern-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(".env");

        // First run: raw pattern
        std::fs::write(&path, "GH_TOKEN=$(echo hello)\n").unwrap();
        let (values, patterns) = load_env_file(&path);
        assert_eq!(values["GH_TOKEN"], "$(echo hello)");
        assert!(patterns.is_empty());
        // Simulate what init-env writes
        let resolved = eval_shell_substitution(&values["GH_TOKEN"]);
        assert_eq!(resolved, "hello");
        std::fs::write(
            &path,
            format!("# GH_TOKEN=$(echo hello)\nGH_TOKEN={resolved}\n"),
        )
        .unwrap();

        // Second run: pattern should come from comment, not the raw value
        let (values2, patterns2) = load_env_file(&path);
        assert_eq!(values2["GH_TOKEN"], "hello"); // raw value
        assert_eq!(patterns2["GH_TOKEN"], "$(echo hello)"); // pattern preserved
        // Pattern takes priority - re-evaluate it
        let resolved2 = eval_shell_substitution(&patterns2["GH_TOKEN"]);
        assert_eq!(resolved2, "hello");

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn eval_shell_substitution_passthrough() {
        assert_eq!(eval_shell_substitution("plain_value"), "plain_value");
    }

    #[test]
    fn eval_shell_substitution_evaluates() {
        let result = eval_shell_substitution("$(echo test123)");
        assert_eq!(result, "test123");
    }

    #[test]
    fn load_env_file_missing_returns_empty() {
        let (values, patterns) = load_env_file(Path::new("/nonexistent/.env"));
        assert!(values.is_empty());
        assert!(patterns.is_empty());
    }

    #[test]
    fn vars_from_config_reads_toml() {
        let dir = std::env::temp_dir().join(format!("kap-initenv-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("kap.toml");
        std::fs::write(
            &path,
            r#"
[mcp]
[[mcp.servers]]
name = "a"
upstream = "https://a.com"
token_env = "A_TOKEN"

[[mcp.servers]]
name = "b"
upstream = "https://b.com"
headers = { "X-Key" = "${B_API_KEY}", "X-Other" = "${C_SECRET}" }
"#,
        )
        .unwrap();

        let vars = vars_from_config(&path).unwrap();
        assert_eq!(vars, vec!["A_TOKEN", "B_API_KEY", "C_SECRET"]);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn regenerate_overlay_from_config() {
        let dir = std::env::temp_dir().join(format!("kap-regen-{}", std::process::id()));
        let dc = dir.join(".devcontainer");
        std::fs::create_dir_all(&dc).unwrap();

        std::fs::write(dc.join("devcontainer.json"), r#"{"service": "myapp"}"#).unwrap();
        std::fs::write(
            dc.join("kap.toml"),
            r#"
[proxy.network]
allow = ["github.com"]

[compose]
build = { context = "..", dockerfile = "Dockerfile", target = "proxy" }
"#,
        )
        .unwrap();

        regenerate_overlay(&dc, &dc.join("kap.toml")).unwrap();

        let overlay = std::fs::read_to_string(dc.join(crate::init::OVERLAY_FILENAME)).unwrap();
        assert!(overlay.contains("myapp:"));
        assert!(overlay.contains("build:"));
        assert!(overlay.contains("context: .."));
        assert!(!overlay.contains("image:"));

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn regenerate_overlay_default_image() {
        let dir = std::env::temp_dir().join(format!("kap-regen-img-{}", std::process::id()));
        let dc = dir.join(".devcontainer");
        std::fs::create_dir_all(&dc).unwrap();

        std::fs::write(dc.join("devcontainer.json"), r#"{}"#).unwrap();
        std::fs::write(
            dc.join("kap.toml"),
            r#"
[proxy.network]
allow = ["github.com"]
"#,
        )
        .unwrap();

        regenerate_overlay(&dc, &dc.join("kap.toml")).unwrap();

        let overlay = std::fs::read_to_string(dc.join(crate::init::OVERLAY_FILENAME)).unwrap();
        assert!(overlay.contains("app:"));
        assert!(overlay.contains("image: ghcr.io/6/kap:latest"));

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn env_var_default_fallback_resolves_known_var() {
        // Simulate: .env is empty, host env doesn't have the var,
        // but env_var_default knows a shell expression for it.
        let dir = std::env::temp_dir().join(format!("kap-envfallback-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let env_path = dir.join(".env");
        std::fs::write(&env_path, "").unwrap();

        let (existing, existing_patterns) = load_env_file(&env_path);
        assert!(existing.is_empty());
        assert!(existing_patterns.is_empty());

        // env_var_default should return a shell expression for GH_TOKEN
        let expr = crate::init::env_var_default("GH_TOKEN");
        assert_eq!(expr, Some("$(gh auth token)"));

        // And it should be evaluable (if gh is installed)
        if let Some(expr) = expr {
            let result = eval_shell_substitution(expr);
            // We don't assert the value since gh may not be authed in CI,
            // but verify it doesn't panic
            let _ = result;
        }

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn cli_shim_uses_sidecar_cli_shim_command() {
        assert!(CLI_SHIM.contains("sidecar-cli-shim"));
        assert!(CLI_SHIM.contains("basename"));
        assert!(CLI_SHIM.starts_with("#!/bin/sh\n"));
    }
}
