/// Scaffold devcontainer files into a project.
use anyhow::{Context, Result};
use std::path::Path;

pub fn run(project_dir: &str) -> Result<()> {
    let project = Path::new(project_dir);
    let devcontainer_dir = project.join(".devcontainer");

    if devcontainer_dir.exists() {
        anyhow::bail!(
            ".devcontainer/ already exists at {}. Remove it first or edit the files directly.",
            devcontainer_dir.display()
        );
    }

    std::fs::create_dir_all(&devcontainer_dir)?;

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
    println!("  devcontainer.json    - VS Code / devcontainer config");
    println!();
    println!("Next steps:");
    println!("  1. Review devg.toml and adjust allowed domains");
    println!("  2. Run: devcontainer up --workspace-folder .");

    Ok(())
}

// Default domains included in every generated config.
// These are all safe package registry / toolchain / AI domains.
const DEFAULT_DOMAINS: &[&str] = &[
    // GitHub
    "github.com",
    "*.github.com",
    "*.githubusercontent.com",
    // AI
    "anthropic.com",
    "*.anthropic.com",
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
        r#"# devg.toml — devcontainer-guard configuration

[proxy.network]
allow = [
{allow_toml},
]
# deny overrides allow:
# deny = ["gist.github.com"]
"#
    )
}

fn generate_docker_compose(project_name: &str) -> String {
    format!(
        r#"services:
  app:
    image: mcr.microsoft.com/devcontainers/base:ubuntu
    volumes:
      - ..:/workspaces/{project_name}:cached
      # Docker Desktop SSH agent (macOS/Windows). On Linux, use $SSH_AUTH_SOCK instead.
      - /run/host-services/ssh-auth.sock:/ssh-agent
    environment:
      SSH_AUTH_SOCK: /ssh-agent
      HTTP_PROXY: http://proxy:3128
      HTTPS_PROXY: http://proxy:3128
      http_proxy: http://proxy:3128
      https_proxy: http://proxy:3128
      NO_PROXY: localhost,127.0.0.1
      no_proxy: localhost,127.0.0.1
    command: sleep infinity
    networks:
      sandbox:
    depends_on:
      proxy:
        condition: service_healthy

  proxy:
    image: ghcr.io/6/devg:latest
    volumes:
      - ./devg.toml:/etc/devg/config.toml:ro
    networks:
      sandbox:
      external:
    restart: unless-stopped
    healthcheck:
      test: ["CMD", "devg", "check", "--proxy"]
      interval: 2s
      timeout: 2s
      retries: 10

networks:
  sandbox:
    internal: true
  external:
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
    fn scaffolds_all_files() {
        let dir = tempdir("scaffold");
        run(dir.to_str().unwrap()).unwrap();
        let dc = dir.join(".devcontainer");
        assert!(dc.join("devg.toml").exists());
        assert!(dc.join("docker-compose.yml").exists());
        assert!(dc.join("devcontainer.json").exists());

        let config = fs::read_to_string(dc.join("devg.toml")).unwrap();
        assert!(config.contains("allow ="));

        let compose = fs::read_to_string(dc.join("docker-compose.yml")).unwrap();
        assert!(compose.contains("services:"));
        assert!(compose.contains("proxy:"));

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn init_fails_if_devcontainer_exists() {
        let dir = tempdir("exists");
        run(dir.to_str().unwrap()).unwrap();
        assert!(run(dir.to_str().unwrap()).is_err());
        fs::remove_dir_all(&dir).unwrap();
    }

    fn tempdir(suffix: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("devg-test-{}-{suffix}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }
}
