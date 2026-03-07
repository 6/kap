/// Scaffold devcontainer-guard files into a project.
///
/// Two modes:
/// - New project (no .devcontainer/): creates everything from scratch
/// - Existing project (.devcontainer/ exists): creates devg.toml + overlay compose,
///   prints instructions for what to add to devcontainer.json
use anyhow::{Context, Result};
use std::path::Path;

pub fn run(project_dir: &str) -> Result<()> {
    let project = Path::new(project_dir);
    let devcontainer_dir = project.join(".devcontainer");

    if devcontainer_dir.exists() {
        run_existing(project, &devcontainer_dir)
    } else {
        run_new(project, &devcontainer_dir)
    }
}

/// Existing project: create overlay files, print instructions.
fn run_existing(_project: &Path, devcontainer_dir: &Path) -> Result<()> {
    let devcontainer_json_path = devcontainer_dir.join("devcontainer.json");
    if !devcontainer_json_path.exists() {
        anyhow::bail!(
            ".devcontainer/ exists but has no devcontainer.json at {}",
            devcontainer_json_path.display()
        );
    }

    // Read service name and compose file from devcontainer.json
    let dc_content = std::fs::read_to_string(&devcontainer_json_path)
        .with_context(|| format!("reading {}", devcontainer_json_path.display()))?;
    let dc_json: serde_json::Value = serde_json::from_str(&dc_content)
        .with_context(|| format!("parsing {}", devcontainer_json_path.display()))?;

    let service_name = dc_json["service"]
        .as_str()
        .unwrap_or("app")
        .to_string();

    // Build the dockerComposeFile array for the instructions
    let compose_files: Vec<String> = if let Some(arr) = dc_json["dockerComposeFile"].as_array() {
        arr.iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect()
    } else if let Some(s) = dc_json["dockerComposeFile"].as_str() {
        vec![s.to_string()]
    } else {
        vec!["docker-compose.yml".to_string()]
    };

    let devg_toml_path = devcontainer_dir.join("devg.toml");
    let overlay_path = devcontainer_dir.join("docker-compose.devg.yml");

    if devg_toml_path.exists() {
        anyhow::bail!(
            "devg.toml already exists at {}. Remove it to re-initialize.",
            devg_toml_path.display()
        );
    }

    write_file(&devg_toml_path, &generate_config(DEFAULT_DOMAINS))?;
    write_file(&overlay_path, &generate_overlay(&service_name))?;

    // Update devcontainer.json: add overlay to dockerComposeFile, add initializeCommand
    let mut dc_obj = dc_json.clone();
    let mut all_compose: Vec<serde_json::Value> = compose_files
        .iter()
        .map(|f| serde_json::Value::String(f.clone()))
        .collect();
    all_compose.push(serde_json::Value::String(
        "docker-compose.devg.yml".to_string(),
    ));
    dc_obj["dockerComposeFile"] = serde_json::Value::Array(all_compose);

    let mut notes: Vec<String> = Vec::new();

    if dc_obj.get("initializeCommand").is_some() {
        notes.push("initializeCommand already set. Add `devg init-env` to your existing command.".to_string());
    } else {
        dc_obj["initializeCommand"] = serde_json::Value::String("devg init-env".to_string());
    }

    let updated = serde_json::to_string_pretty(&dc_obj)?;
    write_file(&devcontainer_json_path, &format!("{updated}\n"))?;

    println!();
    println!("Created .devcontainer/devg.toml");
    println!("Created .devcontainer/docker-compose.devg.yml");
    println!("Updated .devcontainer/devcontainer.json");

    for note in &notes {
        println!();
        println!("  NOTE: {note}");
    }

    println!();
    println!("Next:");
    println!("  devcontainer up --workspace-folder .");
    println!("  devg doctor   # verify everything is wired correctly");

    Ok(())
}

/// New project: create everything from scratch.
fn run_new(project: &Path, devcontainer_dir: &Path) -> Result<()> {
    std::fs::create_dir_all(devcontainer_dir)?;

    let project_name = project
        .canonicalize()
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_string()))
        .unwrap_or_else(|| "my-project".to_string());

    write_file(
        &devcontainer_dir.join("devg.toml"),
        &generate_config(DEFAULT_DOMAINS),
    )?;
    write_file(
        &devcontainer_dir.join("docker-compose.yml"),
        &generate_docker_compose(&project_name),
    )?;
    write_file(
        &devcontainer_dir.join("devcontainer.json"),
        &generate_devcontainer_json(&project_name),
    )?;

    println!("Created .devcontainer/ with:");
    println!("  devg.toml            - guard config (edit allowed domains here)");
    println!("  docker-compose.yml   - container orchestration");
    println!("  devcontainer.json    - devcontainer config");
    println!();
    println!("Next steps:");
    println!("  1. Review devg.toml and adjust allowed domains");
    println!("  2. Run: devcontainer up --workspace-folder .");

    Ok(())
}

const DEFAULT_DOMAINS: &[&str] = &[
    // GitHub
    "github.com",
    "*.github.com",
    "*.githubusercontent.com",
    // AI
    "anthropic.com",
    "*.anthropic.com",
    "claude.ai",
    "*.claude.ai",
    "claude.com",
    "*.claude.com",
    "openai.com",
    "*.openai.com",
    "generativelanguage.googleapis.com",
    // APT
    "*.ubuntu.com",
    "*.debian.org",
    // Ruby
    "rubygems.org",
    "*.rubygems.org",
    "bundler.io",
    "*.ruby-lang.org",
    "rubyonrails.org",
    "*.rubyonrails.org",
    // Node
    "*.npmjs.org",
    "*.npmjs.com",
    "nodejs.org",
    "*.yarnpkg.com",
    // Rust
    "crates.io",
    "*.crates.io",
    "rustup.rs",
    "*.rust-lang.org",
    // Python
    "pypi.org",
    "*.pypi.org",
    "*.pythonhosted.org",
    // Go
    "proxy.golang.org",
    "sum.golang.org",
    "storage.googleapis.com",
    // Java
    "repo.maven.apache.org",
    "*.maven.org",
    "plugins.gradle.org",
    "services.gradle.org",
    "downloads.gradle-dn.com",
    // CocoaPods
    "cocoapods.org",
    "*.cocoapods.org",
];

fn generate_config(domains: &[&str]) -> String {
    let allow_toml = domains
        .iter()
        .map(|d| format!("  \"{d}\""))
        .collect::<Vec<_>>()
        .join(",\n");

    format!(
        r#"# devg.toml: devcontainer-guard configuration

[proxy.network]
allow = [
{allow_toml},
]
# deny overrides allow:
# deny = ["gist.github.com"]
"#
    )
}

/// Generate the overlay compose file for existing projects.
fn generate_overlay(service_name: &str) -> String {
    format!(
        r#"# Generated by devg init. Adds network isolation, DNS filtering, and MCP proxy.
# Merged with your existing docker-compose via dockerComposeFile array in devcontainer.json.
# This file MUST be last in the array so its settings take precedence.
services:
  # Adds proxy env vars and DNS to your existing service
  {service_name}:
    environment:
      # Use static IP because app DNS goes through devg (hostnames won't resolve)
      HTTP_PROXY: http://172.28.0.3:3128
      HTTPS_PROXY: http://172.28.0.3:3128
      http_proxy: http://172.28.0.3:3128
      https_proxy: http://172.28.0.3:3128
      NO_PROXY: localhost,127.0.0.1
      no_proxy: localhost,127.0.0.1
    # DNS goes through devg's filtered forwarder (only resolves allowed domains)
    dns:
      - 172.28.0.3
    networks:
      devg_sandbox:
        ipv4_address: 172.28.0.2
    depends_on:
      devg:
        condition: service_healthy

  # Proxy sidecar: domain proxy (:3128), DNS forwarder (:53), MCP proxy (:3129)
  devg:
    image: ghcr.io/6/devg:latest
    volumes:
      - ./devg.toml:/etc/devg/config.toml:ro
    # Credentials from devg init-env (GH_TOKEN, API keys, etc.)
    env_file:
      - path: .env
        required: false
    networks:
      devg_sandbox:
        ipv4_address: 172.28.0.3
      devg_external:
    restart: unless-stopped
    healthcheck:
      test: ["CMD", "devg", "check", "--proxy"]
      interval: 2s
      timeout: 2s
      retries: 10

# Static subnet so we can use fixed IPs for DNS and proxy references.
# Internal to Docker, not your host network.
networks:
  devg_sandbox:
    internal: true  # no default gateway, no route to internet
    ipam:
      config:
        - subnet: 172.28.0.0/24
  devg_external:
    driver: bridge
"#
    )
}

/// Generate the standalone compose file for new projects.
fn generate_docker_compose(project_name: &str) -> String {
    format!(
        r#"services:
  app:
    image: mcr.microsoft.com/devcontainers/base:ubuntu
    volumes:
      - ..:/workspaces/{project_name}:cached
      # 1Password SSH agent (macOS). On Linux, use $SSH_AUTH_SOCK instead.
      - ${{HOME}}/Library/Group Containers/2BUA8C4S2C.com.1password/t/agent.sock:/ssh-agent:ro
    environment:
      SSH_AUTH_SOCK: /ssh-agent
      # Use static IP because app DNS goes through devg (hostnames won't resolve)
      HTTP_PROXY: http://172.28.0.3:3128
      HTTPS_PROXY: http://172.28.0.3:3128
      http_proxy: http://172.28.0.3:3128
      https_proxy: http://172.28.0.3:3128
      NO_PROXY: localhost,127.0.0.1
      no_proxy: localhost,127.0.0.1
    # DNS goes through devg's filtered forwarder (only resolves allowed domains)
    dns:
      - 172.28.0.3
    command: sleep infinity
    networks:
      devg_sandbox:
        ipv4_address: 172.28.0.2
    depends_on:
      devg:
        condition: service_healthy

  # Proxy sidecar: domain proxy (:3128), DNS forwarder (:53), MCP proxy (:3129)
  devg:
    image: ghcr.io/6/devg:latest
    volumes:
      - ./devg.toml:/etc/devg/config.toml:ro
    # Credentials from devg init-env (GH_TOKEN, API keys, etc.)
    env_file:
      - path: .env
        required: false
    networks:
      devg_sandbox:
        ipv4_address: 172.28.0.3
      devg_external:
    restart: unless-stopped
    healthcheck:
      test: ["CMD", "devg", "check", "--proxy"]
      interval: 2s
      timeout: 2s
      retries: 10

# Static subnet so we can use fixed IPs for DNS and proxy references.
# Internal to Docker, not your host network.
networks:
  devg_sandbox:
    internal: true  # no default gateway, no route to internet
    ipam:
      config:
        - subnet: 172.28.0.0/24
  devg_external:
    driver: bridge
"#
    )
}

fn generate_devcontainer_json(project_name: &str) -> String {
    format!(
        r#"{{
  "name": "{project_name}",
  "dockerComposeFile": "docker-compose.yml",
  "service": "app",
  "workspaceFolder": "/workspaces/{project_name}",
  "initializeCommand": "devg init-env",
  "remoteUser": "vscode"
}}
"#
    )
}

fn write_file(path: &Path, content: &str) -> Result<()> {
    std::fs::write(path, content).with_context(|| format!("writing {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn default_domains_covers_all_ecosystems() {
        let domains = DEFAULT_DOMAINS;
        assert!(domains.contains(&"github.com"));
        assert!(domains.contains(&"anthropic.com"));
        assert!(domains.contains(&"crates.io"));
        assert!(domains.contains(&"*.npmjs.org"));
        assert!(domains.contains(&"pypi.org"));
        assert!(domains.contains(&"proxy.golang.org"));
        assert!(domains.contains(&"rubygems.org"));
        assert!(domains.contains(&"cocoapods.org"));
        assert!(domains.contains(&"repo.maven.apache.org"));
    }

    #[test]
    fn new_project_scaffolds_all_files() {
        let dir = tempdir("scaffold-new");
        run(dir.to_str().unwrap()).unwrap();
        let dc = dir.join(".devcontainer");
        assert!(dc.join("devg.toml").exists());
        assert!(dc.join("docker-compose.yml").exists());
        assert!(dc.join("devcontainer.json").exists());

        let config = fs::read_to_string(dc.join("devg.toml")).unwrap();
        assert!(config.contains("allow ="));

        let compose = fs::read_to_string(dc.join("docker-compose.yml")).unwrap();
        assert!(compose.contains("devg:"));
        assert!(compose.contains("devg_sandbox:"));

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn existing_project_creates_overlay_and_updates_json() {
        let dir = tempdir("scaffold-existing");
        let dc = dir.join(".devcontainer");
        fs::create_dir_all(&dc).unwrap();
        fs::write(
            dc.join("devcontainer.json"),
            r#"{"service": "myapp", "dockerComposeFile": "compose.yaml"}"#,
        )
        .unwrap();

        run(dir.to_str().unwrap()).unwrap();

        assert!(dc.join("devg.toml").exists());
        assert!(dc.join("docker-compose.devg.yml").exists());
        assert!(!dc.join("docker-compose.yml").exists());

        let overlay = fs::read_to_string(dc.join("docker-compose.devg.yml")).unwrap();
        assert!(overlay.contains("myapp:"));
        assert!(overlay.contains("devg:"));
        assert!(overlay.contains("devg_sandbox:"));

        // devcontainer.json should be updated with overlay and initializeCommand
        let updated: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(dc.join("devcontainer.json")).unwrap())
                .unwrap();
        let compose_arr = updated["dockerComposeFile"].as_array().unwrap();
        assert_eq!(compose_arr.len(), 2);
        assert_eq!(compose_arr[0], "compose.yaml");
        assert_eq!(compose_arr[1], "docker-compose.devg.yml");
        assert_eq!(updated["initializeCommand"], "devg init-env");

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn existing_project_appends_to_compose_array() {
        let dir = tempdir("scaffold-array");
        let dc = dir.join(".devcontainer");
        fs::create_dir_all(&dc).unwrap();
        fs::write(
            dc.join("devcontainer.json"),
            r#"{"service": "api", "dockerComposeFile": ["docker-compose.yml", "docker-compose.override.yml"]}"#,
        )
        .unwrap();

        run(dir.to_str().unwrap()).unwrap();

        let updated: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(dc.join("devcontainer.json")).unwrap())
                .unwrap();
        let compose_arr = updated["dockerComposeFile"].as_array().unwrap();
        assert_eq!(compose_arr.len(), 3);
        assert_eq!(compose_arr[0], "docker-compose.yml");
        assert_eq!(compose_arr[1], "docker-compose.override.yml");
        assert_eq!(compose_arr[2], "docker-compose.devg.yml");

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn existing_project_fails_if_devg_toml_exists() {
        let dir = tempdir("scaffold-exists");
        let dc = dir.join(".devcontainer");
        fs::create_dir_all(&dc).unwrap();
        fs::write(dc.join("devcontainer.json"), r#"{"service": "app"}"#).unwrap();
        fs::write(dc.join("devg.toml"), "").unwrap();

        assert!(run(dir.to_str().unwrap()).is_err());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn existing_project_fails_if_no_devcontainer_json() {
        let dir = tempdir("scaffold-no-json");
        let dc = dir.join(".devcontainer");
        fs::create_dir_all(&dc).unwrap();
        // .devcontainer/ exists but no devcontainer.json

        assert!(run(dir.to_str().unwrap()).is_err());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn overlay_uses_default_service_name() {
        let dir = tempdir("scaffold-default-svc");
        let dc = dir.join(".devcontainer");
        fs::create_dir_all(&dc).unwrap();
        // No "service" field in json
        fs::write(dc.join("devcontainer.json"), r#"{}"#).unwrap();

        run(dir.to_str().unwrap()).unwrap();

        let overlay = fs::read_to_string(dc.join("docker-compose.devg.yml")).unwrap();
        assert!(overlay.contains("app:"));

        fs::remove_dir_all(&dir).unwrap();
    }

    fn tempdir(suffix: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("devg-test-{}-{suffix}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }
}
