/// Generate .devcontainer/.env from host environment.
///
/// Reads devg.toml to find which env vars the proxy sidecar needs
/// (from token_env and ${VAR} references in headers), then writes
/// them to .env so docker-compose passes them to the container.
use anyhow::{Context, Result};
use std::path::Path;

pub fn run(project_dir: &str) -> Result<()> {
    let project = Path::new(project_dir);
    let config_path = project.join(".devcontainer/devg.toml");
    let env_path = project.join(".devcontainer/.env");

    let needed_vars = if config_path.exists() {
        vars_from_config(&config_path)?
    } else {
        Vec::new()
    };

    // Load existing .env so we don't overwrite manually set values
    let existing = load_env_file(&env_path);

    let mut lines: Vec<String> = Vec::new();

    for var in &needed_vars {
        // Keep existing value if present
        if let Some(val) = existing.get(var.as_str()) {
            lines.push(format!("{var}={val}"));
            continue;
        }
        // Otherwise try host environment
        if let Ok(val) = std::env::var(var)
            && !val.is_empty()
        {
            eprintln!("[init-env] {var} (from host env)");
            lines.push(format!("{var}={val}"));
        }
    }

    let content = lines.join("\n");
    if !content.is_empty() {
        std::fs::write(&env_path, content + "\n")?;
        eprintln!("[init-env] wrote {} vars to {}", lines.len(), env_path.display());
    } else if !env_path.exists() {
        // Only create empty file if none exists
        std::fs::write(&env_path, "")?;
    }

    Ok(())
}

/// Load existing KEY=VALUE pairs from a .env file.
fn load_env_file(path: &Path) -> std::collections::HashMap<String, String> {
    let mut map = std::collections::HashMap::new();
    let Ok(content) = std::fs::read_to_string(path) else {
        return map;
    };
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((key, val)) = line.split_once('=') {
            map.insert(key.trim().to_string(), val.trim().to_string());
        }
    }
    map
}

/// Parse devg.toml and collect all env var names referenced by MCP server configs.
fn vars_from_config(path: &Path) -> Result<Vec<String>> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("reading {}", path.display()))?;
    let config: crate::config::Config = toml::from_str(&content)
        .with_context(|| format!("parsing {}", path.display()))?;

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
        let dir = std::env::temp_dir().join(format!("devg-loadenv-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(".env");
        std::fs::write(&path, "FOO=bar\nBAZ=qux\n").unwrap();

        let map = load_env_file(&path);
        assert_eq!(map["FOO"], "bar");
        assert_eq!(map["BAZ"], "qux");
        assert_eq!(map.len(), 2);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn load_env_file_skips_comments_and_blanks() {
        let dir = std::env::temp_dir().join(format!("devg-loadenv2-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(".env");
        std::fs::write(&path, "# comment\n\nKEY=val\n  \n# another\n").unwrap();

        let map = load_env_file(&path);
        assert_eq!(map.len(), 1);
        assert_eq!(map["KEY"], "val");

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn load_env_file_missing_returns_empty() {
        let map = load_env_file(Path::new("/nonexistent/.env"));
        assert!(map.is_empty());
    }

    #[test]
    fn vars_from_config_reads_toml() {
        let dir = std::env::temp_dir().join(format!("devg-initenv-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("devg.toml");
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
}
