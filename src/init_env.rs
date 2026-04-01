/// Generate .devcontainer/.env and docker-compose.kap.yml from host environment.
///
/// Reads kap.toml to find which env vars the proxy sidecar needs
/// (from ${VAR} references in headers and CLI tool env lists), then writes
/// them to .env so docker-compose passes them to the container.
///
/// Also regenerates the compose overlay (docker-compose.kap.yml) so it
/// always matches the installed kap version and kap.toml config.
use anyhow::{Context, Result};
use std::collections::HashMap;
use std::path::Path;

pub fn run(project_dir: &str) -> Result<()> {
    let project = Path::new(project_dir);
    let devcontainer_dir = project.join(".devcontainer");
    let config_path = devcontainer_dir.join("kap.toml");
    let env_path = crate::init::env_file_for_project(&devcontainer_dir);

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

    let (needed_vars, env_overrides) = vars_from_config(&config_path)?;

    // Load existing .env values and shell patterns (# KEY=$(cmd))
    let (existing, existing_patterns) = load_env_file(&env_path);

    let mut lines: Vec<String> = Vec::new();

    for var in &needed_vars {
        // 1. Explicit [env] override from kap.toml (highest priority)
        if let Some(val) = env_overrides.get(var.as_str())
            && let Some(resolved) = resolve_env_value(var, val)
        {
            if val.contains("$(") {
                // Shell expression — write pattern comment so refresh_env() re-evaluates
                lines.push(format!("# {var}={val}"));
            }
            lines.push(format!("{var}={resolved}"));
            continue;
        }
        // 2. Re-evaluate shell patterns stored as comments (# KEY=$(cmd))
        if let Some(pattern) = existing_patterns.get(var.as_str()) {
            let resolved = eval_shell_substitution(pattern);
            if !resolved.is_empty() {
                lines.push(format!("# {var}={pattern}"));
                lines.push(format!("{var}={resolved}"));
                continue;
            }
        }
        // 3. Keep existing value (or evaluate if it contains $(cmd))
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
        // 4. Try host environment
        if let Ok(val) = std::env::var(var)
            && !val.is_empty()
        {
            eprintln!("[sidecar-init] {var} (from host env)");
            lines.push(format!("{var}={val}"));
            continue;
        }
        // 5. Last resort: try well-known shell expression (e.g. GH_TOKEN -> `gh auth token`)
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

    let config = crate::config::Config::load(&config_path.to_string_lossy())?;
    let compose_config = config.compose.unwrap_or_default();

    // Derive project root from devcontainer_dir (parent of .devcontainer/)
    let project_dir = devcontainer_dir.parent().unwrap_or(devcontainer_dir);
    let (sandbox_prefix, external_prefix) = crate::init::find_available_subnets(project_dir);
    let project_name = crate::init::read_project_name(devcontainer_dir);
    let ssh_auth_sock = if config.ssh_agent {
        crate::init::detect_ssh_auth_sock()
    } else {
        None
    };
    let global_config = crate::config::has_global_config();
    let overlay = crate::init::generate_overlay(
        &service_name,
        &compose_config,
        &sandbox_prefix,
        &external_prefix,
        &project_name,
        ssh_auth_sock.as_deref(),
        global_config,
    );
    std::fs::write(&overlay_path, &overlay)
        .with_context(|| format!("writing {}", overlay_path.display()))?;
    eprintln!(
        "[sidecar-init] regenerated {}",
        crate::init::OVERLAY_FILENAME
    );

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

/// Resolve an `[env]` value from kap.toml. Supports three forms:
/// - `${VAR}` — env var reference from host
/// - `$(cmd)` — shell expression
/// - `"literal"` — static value
fn resolve_env_value(name: &str, val: &str) -> Option<String> {
    if val.contains("$(") {
        let resolved = eval_shell_substitution(val);
        if resolved.is_empty() {
            eprintln!("[sidecar-init] warning: [env] {name} expression evaluated to empty");
            return None;
        }
        Some(resolved)
    } else if val.contains("${") {
        // Expand ${VAR} references from host environment
        let resolved = expand_env_refs(val);
        if resolved.is_empty() {
            eprintln!("[sidecar-init] warning: [env] {name} env ref resolved to empty");
            return None;
        }
        Some(resolved)
    } else {
        // Static value
        Some(val.to_string())
    }
}

/// Expand `${VAR}` references in a string using host environment variables.
fn expand_env_refs(s: &str) -> String {
    let mut result = s.to_string();
    let mut rest = s;
    while let Some(start) = rest.find("${") {
        let after = &rest[start + 2..];
        if let Some(end) = after.find('}') {
            let var = &after[..end];
            if let Ok(val) = std::env::var(var) {
                result = result.replace(&format!("${{{var}}}"), &val);
            } else {
                eprintln!("[sidecar-init] warning: ${{{var}}} not found in host environment");
                result = result.replace(&format!("${{{var}}}"), "");
            }
            rest = &after[end + 1..];
        } else {
            break;
        }
    }
    result
}

/// Parse kap.toml (with global merge) and collect env var names + explicit [env] overrides.
fn vars_from_config(path: &Path) -> Result<(Vec<String>, HashMap<String, String>)> {
    let config = crate::config::Config::load(&path.to_string_lossy())
        .with_context(|| format!("loading {}", path.display()))?;

    let mut vars = Vec::new();

    // Vars from [env] section
    for key in config.env.keys() {
        vars.push(key.clone());
    }

    if let Some(mcp) = &config.mcp {
        for server in &mcp.servers {
            // headers can contain ${VAR} references
            for value in server.headers.values() {
                extract_env_refs(value, &mut vars);
            }
        }
    }

    // CLI tools need their env vars on the sidecar
    if let Some(cli) = &config.cli {
        for tool in &cli.tools {
            if tool.env.is_empty() && tool.mode == crate::config::CliToolMode::Direct {
                // Auto-resolve from DETECTABLE_TOOLS (e.g. gh → GH_TOKEN)
                vars.extend(crate::init::default_env_for_tool(&tool.name));
            } else {
                for var in &tool.env {
                    vars.push(var.clone());
                }
            }
        }
    }

    let env_overrides = config.env.clone();
    vars.sort();
    vars.dedup();
    Ok((vars, env_overrides))
}

/// Re-evaluate shell patterns in an existing .env file.
///
/// Finds comment lines like `# GH_TOKEN=$(gh auth token)`, evaluates the
/// shell expression on the host, and updates the resolved value on the next
/// line. Returns the number of vars refreshed.
pub fn refresh_env(env_path: &Path) -> Result<usize> {
    let (existing_values, patterns) = load_env_file(env_path);

    if patterns.is_empty() {
        return Ok(0);
    }

    // Re-read original file content to preserve structure
    let content = std::fs::read_to_string(env_path)
        .with_context(|| format!("reading {}", env_path.display()))?;

    let mut output_lines: Vec<String> = Vec::new();
    let mut refreshed = 0usize;
    let mut skip_next_value_for: Option<String> = None;

    for line in content.lines() {
        let trimmed = line.trim();

        // If we just wrote a refreshed pattern comment, skip the old resolved value line
        if let Some(ref var) = skip_next_value_for {
            if trimmed.starts_with(&format!("{var}=")) {
                skip_next_value_for = None;
                continue;
            }
            // Line wasn't the expected value line — stop skipping
            skip_next_value_for = None;
        }

        // Check if this is a pattern comment: # VAR=$(cmd)
        if let Some(comment) = trimmed.strip_prefix("# ")
            && let Some((key, val)) = comment.split_once('=')
        {
            let key = key.trim();
            let val = val.trim();
            if val.contains("$(") {
                let resolved = eval_shell_substitution(val);
                if !resolved.is_empty() {
                    output_lines.push(format!("# {key}={val}"));
                    output_lines.push(format!("{key}={resolved}"));
                    refreshed += 1;
                    skip_next_value_for = Some(key.to_string());
                    continue;
                }
            }
        }

        output_lines.push(line.to_string());
    }

    // Also handle vars that have patterns but no comment line yet
    // (e.g. first run: raw GH_TOKEN=$(gh auth token) without # prefix)
    // This case is already handled by the normal line passthrough.

    if refreshed > 0 {
        let new_content = output_lines.join("\n") + "\n";
        // Atomic write: .env.tmp → rename
        let tmp_path = env_path.with_extension("env.tmp");
        std::fs::write(&tmp_path, &new_content)
            .with_context(|| format!("writing {}", tmp_path.display()))?;
        std::fs::rename(&tmp_path, env_path).with_context(|| {
            format!("renaming {} to {}", tmp_path.display(), env_path.display())
        })?;

        // Also check for vars whose resolved value changed
        let (new_values, _) = load_env_file(env_path);
        for (key, new_val) in &new_values {
            if let Some(old_val) = existing_values.get(key)
                && old_val != new_val
            {
                eprintln!("[env] refreshed {key}");
            }
        }
    }

    Ok(refreshed)
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
allow = ["*"]
headers = { "X-Key" = "${A_API_KEY}" }

[[mcp.servers]]
name = "b"
allow = ["*"]
headers = { "X-Key" = "${B_API_KEY}", "X-Other" = "${C_SECRET}" }
"#,
        )
        .unwrap();

        let (vars, _) = vars_from_config(&path).unwrap();
        assert_eq!(vars, vec!["A_API_KEY", "B_API_KEY", "C_SECRET"]);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn vars_from_config_direct_mode_auto_resolves_env() {
        let dir = std::env::temp_dir().join(format!("kap-initenv-direct-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("kap.toml");
        std::fs::write(
            &path,
            r#"
[cli]
[[cli.tools]]
name = "gh"
mode = "direct"
"#,
        )
        .unwrap();

        let (vars, _) = vars_from_config(&path).unwrap();
        assert_eq!(vars, vec!["GH_TOKEN"]); // auto-resolved from DETECTABLE_TOOLS

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
    fn refresh_env_evaluates_patterns() {
        let dir = std::env::temp_dir().join(format!("kap-refresh-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(".env");
        std::fs::write(
            &path,
            "# MY_VAR=$(echo refreshed)\nMY_VAR=stale\nSTATIC=keep\n",
        )
        .unwrap();

        let count = refresh_env(&path).unwrap();
        assert_eq!(count, 1);

        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("MY_VAR=refreshed"));
        assert!(content.contains("# MY_VAR=$(echo refreshed)"));
        assert!(content.contains("STATIC=keep"));

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn refresh_env_no_patterns_is_noop() {
        let dir = std::env::temp_dir().join(format!("kap-refresh-noop-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(".env");
        std::fs::write(&path, "STATIC=value\n").unwrap();

        let count = refresh_env(&path).unwrap();
        assert_eq!(count, 0);

        let content = std::fs::read_to_string(&path).unwrap();
        assert_eq!(content, "STATIC=value\n");

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn refresh_env_multiple_patterns() {
        let dir = std::env::temp_dir().join(format!("kap-refresh-multi-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(".env");
        std::fs::write(&path, "# A=$(echo aa)\nA=old_a\n# B=$(echo bb)\nB=old_b\n").unwrap();

        let count = refresh_env(&path).unwrap();
        assert_eq!(count, 2);

        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("A=aa"));
        assert!(content.contains("B=bb"));

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
    fn resolve_env_value_static() {
        let result = resolve_env_value("TEST", "static_value");
        assert_eq!(result, Some("static_value".to_string()));
    }

    #[test]
    fn resolve_env_value_shell_expression() {
        let result = resolve_env_value("TEST", "$(echo hello)");
        assert_eq!(result, Some("hello".to_string()));
    }

    #[test]
    fn resolve_env_value_env_ref() {
        // SAFETY: test-only, single-threaded access to unique var name
        unsafe { std::env::set_var("KAP_TEST_RESOLVE_VAR", "from_env") };
        let result = resolve_env_value("TEST", "${KAP_TEST_RESOLVE_VAR}");
        assert_eq!(result, Some("from_env".to_string()));
        unsafe { std::env::remove_var("KAP_TEST_RESOLVE_VAR") };
    }

    #[test]
    fn resolve_env_value_env_ref_missing() {
        // SAFETY: test-only, single-threaded access to unique var name
        unsafe { std::env::remove_var("KAP_TEST_MISSING_VAR") };
        let result = resolve_env_value("TEST", "${KAP_TEST_MISSING_VAR}");
        // Missing env ref resolves to empty → None
        assert_eq!(result, None);
    }

    #[test]
    fn expand_env_refs_basic() {
        // SAFETY: test-only, single-threaded access to unique var name
        unsafe { std::env::set_var("KAP_TEST_EXPAND_A", "aaa") };
        let result = expand_env_refs("prefix-${KAP_TEST_EXPAND_A}-suffix");
        assert_eq!(result, "prefix-aaa-suffix");
        unsafe { std::env::remove_var("KAP_TEST_EXPAND_A") };
    }

    #[test]
    fn expand_env_refs_multiple() {
        // SAFETY: test-only, single-threaded access to unique var names
        unsafe {
            std::env::set_var("KAP_TEST_EXP_X", "xx");
            std::env::set_var("KAP_TEST_EXP_Y", "yy");
        }
        let result = expand_env_refs("${KAP_TEST_EXP_X}-${KAP_TEST_EXP_Y}");
        assert_eq!(result, "xx-yy");
        unsafe {
            std::env::remove_var("KAP_TEST_EXP_X");
            std::env::remove_var("KAP_TEST_EXP_Y");
        }
    }

    #[test]
    fn vars_from_config_includes_env_section() {
        let dir = std::env::temp_dir().join(format!("kap-env-section-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("kap.toml");
        std::fs::write(
            &path,
            r#"
[env]
MY_TOKEN = "static_val"
"#,
        )
        .unwrap();

        let (vars, overrides) = vars_from_config(&path).unwrap();
        assert!(vars.contains(&"MY_TOKEN".to_string()));
        assert_eq!(overrides["MY_TOKEN"], "static_val");

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn env_override_beats_existing_env_file() {
        let dir = std::env::temp_dir().join(format!("kap-env-override-{}", std::process::id()));
        let dc = dir.join(".devcontainer");
        std::fs::create_dir_all(&dc).unwrap();

        // Write a config with [env] override
        std::fs::write(
            dc.join("kap.toml"),
            r#"
[env]
MY_VAR = "$(echo from_config)"
"#,
        )
        .unwrap();

        let env_path = crate::init::env_file_for_project(&dc);
        // Pre-populate .env with a different value
        std::fs::write(&env_path, "MY_VAR=old_value\n").unwrap();

        // Write minimal devcontainer.json (needed by regenerate_overlay)
        std::fs::write(dc.join("devcontainer.json"), "{}").unwrap();

        run(dir.to_str().unwrap()).unwrap();

        let content = std::fs::read_to_string(&env_path).unwrap();
        assert!(
            content.contains("MY_VAR=from_config"),
            "expected [env] override, got: {content}"
        );
        // Shell expression should have pattern comment for refresh_env()
        assert!(content.contains("# MY_VAR=$(echo from_config)"));

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn env_override_static_value() {
        let dir = std::env::temp_dir().join(format!("kap-env-static-{}", std::process::id()));
        let dc = dir.join(".devcontainer");
        std::fs::create_dir_all(&dc).unwrap();

        std::fs::write(
            dc.join("kap.toml"),
            r#"
[env]
MY_STATIC = "literal_value"
"#,
        )
        .unwrap();
        std::fs::write(dc.join("devcontainer.json"), "{}").unwrap();

        run(dir.to_str().unwrap()).unwrap();

        let env_path = crate::init::env_file_for_project(&dc);
        let content = std::fs::read_to_string(&env_path).unwrap();
        assert!(
            content.contains("MY_STATIC=literal_value"),
            "expected static value, got: {content}"
        );
        // Static values should NOT have a pattern comment
        assert!(!content.contains("# MY_STATIC="));

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn env_override_env_ref() {
        let dir = std::env::temp_dir().join(format!("kap-env-ref-{}", std::process::id()));
        let dc = dir.join(".devcontainer");
        std::fs::create_dir_all(&dc).unwrap();

        // SAFETY: test-only, single-threaded access to unique var name
        unsafe { std::env::set_var("KAP_TEST_PROJECT_TOKEN", "project_pat_123") };
        std::fs::write(
            dc.join("kap.toml"),
            r#"
[env]
GH_TOKEN = "${KAP_TEST_PROJECT_TOKEN}"
"#,
        )
        .unwrap();
        std::fs::write(dc.join("devcontainer.json"), "{}").unwrap();

        run(dir.to_str().unwrap()).unwrap();

        let env_path = crate::init::env_file_for_project(&dc);
        let content = std::fs::read_to_string(&env_path).unwrap();
        assert!(
            content.contains("GH_TOKEN=project_pat_123"),
            "expected env ref expansion, got: {content}"
        );

        // SAFETY: test-only cleanup
        unsafe { std::env::remove_var("KAP_TEST_PROJECT_TOKEN") };
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
