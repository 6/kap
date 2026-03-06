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

    // Detect language profiles
    let profiles = detect_profiles(project);
    let profiles_toml = profiles
        .iter()
        .map(|p| format!("\"{p}\""))
        .collect::<Vec<_>>()
        .join(", ");

    // Write config
    write_file(
        &devcontainer_dir.join("devp.toml"),
        &generate_config(&profiles_toml),
    )?;

    // Write docker-compose.yml
    write_file(
        &devcontainer_dir.join("docker-compose.yml"),
        &generate_docker_compose(&project_name),
    )?;

    // Write devcontainer.json
    write_file(
        &devcontainer_dir.join("devcontainer.json"),
        &generate_devcontainer_json(&project_name),
    )?;

    // Write Dockerfile (app container)
    write_file(&devcontainer_dir.join("Dockerfile"), &generate_dockerfile())?;

    // Write Dockerfile.proxy
    write_file(
        &devcontainer_dir.join("Dockerfile.proxy"),
        &generate_dockerfile_proxy(),
    )?;

    // Write setup.sh
    let setup_path = devcontainer_dir.join("setup.sh");
    write_file(&setup_path, &generate_setup_sh())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&setup_path, std::fs::Permissions::from_mode(0o755))?;
    }

    println!("Created .devcontainer/ with:");
    println!("  devp.toml          - proxy config (edit allowed domains here)");
    println!("  docker-compose.yml   - container orchestration");
    println!("  devcontainer.json    - VS Code / devcontainer config");
    println!("  Dockerfile           - app container");
    println!("  Dockerfile.proxy     - proxy sidecar");
    println!("  setup.sh             - post-create setup (add your toolchains here)");
    println!();
    println!("Detected profiles: {profiles:?}");
    println!();
    println!("Next steps:");
    println!("  1. Review devp.toml and adjust allowed domains");
    println!("  2. Edit setup.sh to add project-specific setup");
    println!("  3. Run: devcontainer up --workspace-folder .");

    Ok(())
}

fn detect_profiles(project: &Path) -> Vec<&'static str> {
    let mut profiles = vec!["github", "ai", "apt"];

    if project.join("Gemfile").exists() || project.join("Gemfile.lock").exists() {
        profiles.push("ruby");
    }
    if project.join("package.json").exists() {
        profiles.push("node");
    }
    if project.join("Cargo.toml").exists() {
        profiles.push("rust");
    }
    if project.join("pyproject.toml").exists()
        || project.join("requirements.txt").exists()
        || project.join("setup.py").exists()
    {
        profiles.push("python");
    }
    if project.join("go.mod").exists() {
        profiles.push("go");
    }

    profiles
}

fn generate_config(profiles_toml: &str) -> String {
    format!(
        r#"# devp.toml — proxy and credential configuration

[proxy]
listen = "0.0.0.0:3128"
dns_listen = "0.0.0.0:53"
dns_upstream = "8.8.8.8:53"

[proxy.network]
profiles = [{profiles_toml}]
# Add project-specific domains:
# allow = ["internal.mycompany.com"]
# Block specific domains (overrides profiles):
# deny = ["gist.github.com"]

[proxy.observe]
log = "/var/log/devp/proxy.jsonl"

[credentials]
socket = "/devp-sockets/cred.sock"
host_socket = "~/.devp-sockets/cred.sock"

[credentials.github]
hosts = ["github.com"]
"#
    )
}

fn generate_docker_compose(project_name: &str) -> String {
    format!(
        r#"services:
  app:
    build:
      context: ..
      dockerfile: .devcontainer/Dockerfile
      args:
        DEVP_VERSION: "0.1.0"
    volumes:
      - ..:/workspaces/{project_name}:cached
      - history:/commandhistory
      - ${{HOME}}/.devp-sockets:/devp-sockets:ro
    environment:
      HTTP_PROXY: http://proxy:3128
      HTTPS_PROXY: http://proxy:3128
      http_proxy: http://proxy:3128
      https_proxy: http://proxy:3128
      NO_PROXY: localhost,127.0.0.1
      no_proxy: localhost,127.0.0.1
      GIT_CONFIG_GLOBAL: /home/vscode/.gitconfig-devp
    dns:
      - 172.28.0.2
    command: sleep infinity
    networks:
      sandbox:
        ipv4_address: 172.28.0.3
    depends_on:
      proxy:
        condition: service_healthy

  proxy:
    build:
      context: .
      dockerfile: Dockerfile.proxy
      args:
        DEVP_VERSION: "0.1.0"
    volumes:
      - ./devp.toml:/etc/devp/config.toml:ro
      - proxy-logs:/var/log/devp
    networks:
      sandbox:
        ipv4_address: 172.28.0.2
      external:
    restart: unless-stopped
    healthcheck:
      test: ["CMD", "devp", "check", "--proxy"]
      interval: 2s
      timeout: 2s
      retries: 10

volumes:
  history:
  proxy-logs:

networks:
  sandbox:
    internal: true
    ipam:
      config:
        - subnet: 172.28.0.0/16
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
  "initializeCommand": "devp cred-server --daemonize",
  "postCreateCommand": ".devcontainer/setup.sh",
  "remoteUser": "vscode"
}}
"#
    )
}

fn generate_dockerfile() -> String {
    r#"FROM ubuntu:24.04

ARG DEVP_VERSION=0.1.0
ARG TARGETARCH

# Install basics
RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates curl git zsh sudo \
    && rm -rf /var/lib/apt/lists/*

# Install devp binary
RUN curl -fsSL "https://github.com/6/devp/releases/download/v${DEVP_VERSION}/devp-linux-${TARGETARCH}" \
    -o /usr/local/bin/devp \
    && chmod +x /usr/local/bin/devp

# Create non-root user
RUN groupadd -g 1000 vscode \
    && useradd -u 1000 -g 1000 -m -s /bin/zsh vscode \
    && echo "vscode ALL=(ALL) NOPASSWD:ALL" >> /etc/sudoers

USER vscode
WORKDIR /home/vscode
"#
    .to_string()
}

fn generate_dockerfile_proxy() -> String {
    r#"FROM debian:bookworm-slim

ARG DEVP_VERSION=0.1.0
ARG TARGETARCH

RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates curl \
    && rm -rf /var/lib/apt/lists/*

RUN curl -fsSL "https://github.com/6/devp/releases/download/v${DEVP_VERSION}/devp-linux-${TARGETARCH}" \
    -o /usr/local/bin/devp \
    && chmod +x /usr/local/bin/devp

RUN mkdir -p /var/log/devp

CMD ["devp", "proxy"]
"#
    .to_string()
}

fn generate_setup_sh() -> String {
    r#"#!/bin/bash
set -e

# Configure git to use devp credential helper
devp credential --install

# Persistent shell history
sudo chown vscode:vscode /commandhistory
touch /commandhistory/.zsh_history
echo 'HISTFILE=/commandhistory/.zsh_history' >> ~/.zshrc

# --- Add your project-specific setup below ---
# Example:
# sudo apt-get update && sudo apt-get install -y ...
# mise install
"#
    .to_string()
}

fn write_file(path: &Path, content: &str) -> Result<()> {
    std::fs::write(path, content).with_context(|| format!("writing {}", path.display()))
}
