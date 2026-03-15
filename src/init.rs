/// Scaffold kap files into a project.
///
/// Two modes:
/// - New project (no .devcontainer/): creates everything from scratch
/// - Existing project (.devcontainer/ exists): creates kap.toml + overlay compose,
///   updates devcontainer.json
use crate::config::ComposeConfig;
use anyhow::{Context, Result};
use std::path::Path;

pub const OVERLAY_FILENAME: &str = "docker-compose.kap.yml";

/// Parse JSONC (JSON with trailing commas and // comments) into serde_json::Value.
/// devcontainer.json commonly uses JSONC syntax.
pub fn parse_jsonc(input: &str) -> serde_json::Result<serde_json::Value> {
    // Strip // line comments
    let mut cleaned = String::with_capacity(input.len());
    for line in input.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("//") {
            continue;
        }
        cleaned.push_str(line);
        cleaned.push('\n');
    }
    // Remove trailing commas: , followed by optional whitespace/newlines then ] or }
    let bytes = cleaned.as_bytes();
    let mut result = String::with_capacity(cleaned.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b',' {
            // Look ahead past whitespace/newlines for ] or }
            let mut j = i + 1;
            while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                j += 1;
            }
            if j < bytes.len() && (bytes[j] == b']' || bytes[j] == b'}') {
                // Skip the comma (trailing comma)
                i += 1;
                continue;
            }
        }
        result.push(bytes[i] as char);
        i += 1;
    }
    serde_json::from_str(&result)
}

pub fn run(project_dir: &str, yes: bool, force: bool) -> Result<()> {
    let project = Path::new(project_dir);
    let devcontainer_dir = project.join(".devcontainer");

    if devcontainer_dir.exists() {
        run_existing(project, &devcontainer_dir, yes, force)
    } else {
        run_new(project, &devcontainer_dir, yes)
    }
}

fn confirm(prompt: &str) -> bool {
    use std::io::Write;
    print!("{prompt} [Y/n] ");
    std::io::stdout().flush().ok();
    let mut input = String::new();
    if std::io::stdin().read_line(&mut input).is_err() {
        return false;
    }
    let input = input.trim().to_lowercase();
    input.is_empty() || input == "y" || input == "yes"
}

/// Read the project name from devcontainer.json's "name" field.
/// Falls back to the parent directory name.
pub fn read_project_name(devcontainer_dir: &Path) -> String {
    let path = devcontainer_dir.join("devcontainer.json");
    if let Ok(content) = std::fs::read_to_string(&path)
        && let Ok(json) = parse_jsonc(&content)
        && let Some(name) = json["name"].as_str()
    {
        return name.to_string();
    }
    // Fall back to parent directory name
    devcontainer_dir
        .parent()
        .and_then(|p| p.file_name())
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "dev".to_string())
}

/// Read the service name from devcontainer.json. Defaults to "app".
pub fn read_service_name(devcontainer_dir: &Path) -> Result<String> {
    let path = devcontainer_dir.join("devcontainer.json");
    let content =
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let json: serde_json::Value =
        parse_jsonc(&content).with_context(|| format!("parsing {}", path.display()))?;
    Ok(json["service"].as_str().unwrap_or("app").to_string())
}

/// Derive a unique /24 subnet from the project directory path.
/// Returns a prefix like "172.28.0" (without trailing dot).
/// Range: 172.18.0.0/24 - 172.31.255.0/24 (avoids Docker's default 172.17.x.x).
pub fn derive_subnet(project_dir: &Path) -> String {
    let path = project_dir
        .canonicalize()
        .unwrap_or_else(|_| project_dir.to_path_buf());
    let path_str = path.to_string_lossy();

    let mut hash: u32 = 0;
    for byte in path_str.bytes() {
        hash = hash.wrapping_mul(31).wrapping_add(byte as u32);
    }

    let second = 18 + (hash % 14) as u8; // 18-31
    let third = ((hash >> 8) % 256) as u8; // 0-255
    format!("172.{second}.{third}")
}

/// Generate the overlay compose YAML for the kap sidecar.
pub fn generate_overlay(
    service_name: &str,
    compose: &ComposeConfig,
    subnet_prefix: &str,
    project_name: &str,
    ssh_auth_sock: Option<&str>,
    global_config: bool,
) -> String {
    let image_yaml = compose.image_yaml("    ");

    // App service volumes: kap-bin (for shims + kap binary) + SSH agent socket
    let app_volumes = {
        let mut entries: Vec<String> = vec!["      - kap-bin:/opt/kap:ro".to_string()];
        if let Some(sock) = ssh_auth_sock {
            entries.push(format!("      - {sock}:/ssh-agent:ro"));
        }
        format!("\n    volumes:\n{}", entries.join("\n"))
    };
    let ssh_env = if ssh_auth_sock.is_some() {
        "\n      SSH_AUTH_SOCK: /ssh-agent"
    } else {
        ""
    };
    let global_config_volume = if global_config {
        "\n      - ${{HOME}}/.kap/kap.toml:/etc/kap/global.toml:ro"
    } else {
        ""
    };
    let app_ip = format!("{subnet_prefix}.2");
    let sidecar_ip = format!("{subnet_prefix}.3");
    let subnet = format!("{subnet_prefix}.0/24");
    format!(
        r#"# Generated by kap — DO NOT EDIT. Regenerated on each `kap sidecar-init` run.
# Adds network isolation, DNS filtering, and MCP proxy.
# Merged with your existing docker-compose via dockerComposeFile array in devcontainer.json.
# This file MUST be last in the array so its settings take precedence.
services:
  # Adds proxy env vars and DNS to your existing service
  {service_name}:
    hostname: {project_name}
    environment:
      # Use static IP because app DNS goes through kap (hostnames won't resolve)
      HTTP_PROXY: http://{sidecar_ip}:3128
      HTTPS_PROXY: http://{sidecar_ip}:3128
      http_proxy: http://{sidecar_ip}:3128
      https_proxy: http://{sidecar_ip}:3128
      NO_PROXY: localhost,127.0.0.1
      no_proxy: localhost,127.0.0.1{ssh_env}
    # DNS goes through kap's filtered forwarder (only resolves allowed domains)
    dns:
      - {sidecar_ip}
    networks:
      kap_sandbox:
        ipv4_address: {app_ip}{app_volumes}
    depends_on:
      kap:
        condition: service_healthy

  # Proxy sidecar: domain proxy (:3128), DNS forwarder (:53), MCP proxy (:3129)
  kap:
{image_yaml}
    volumes:
      - ./kap.toml:/etc/kap/config.toml:ro{global_config_volume}
      - ${{HOME}}/.kap/auth:/etc/kap/auth
      - proxy-logs:/var/log/kap
      - kap-bin:/opt/kap
      - ..:/workspace:ro
    entrypoint: ["sh", "-c", "cp /usr/local/bin/kap /opt/kap/kap && exec kap sidecar-proxy"]
    env_file:
      - path: .env
        required: false
    networks:
      kap_sandbox:
        ipv4_address: {sidecar_ip}
      kap_external:
    restart: unless-stopped
    healthcheck:
      test: ["CMD", "kap", "sidecar-check", "--proxy"]
      interval: 2s
      timeout: 2s
      retries: 10

volumes:
  proxy-logs:
  kap-bin:

# Static subnet so we can use fixed IPs for DNS and proxy references.
# Internal to Docker, not your host network.
networks:
  kap_sandbox:
    internal: true  # no default gateway, no route to internet
    ipam:
      config:
        - subnet: {subnet}
  kap_external:
    driver: bridge
"#
    )
}

/// Detect the SSH agent socket path for Docker volume mounting.
///
/// On macOS, uses Docker Desktop's built-in SSH agent forwarding
/// (`/run/host-services/ssh-auth.sock`) which avoids VM socket-sharing
/// issues with bind-mounted host sockets. The host SSH agent must be
/// visible to Docker Desktop (e.g. via a LaunchAgent that sets
/// SSH_AUTH_SOCK globally).
///
/// On Linux, Docker runs natively so we bind-mount $SSH_AUTH_SOCK directly.
pub fn detect_ssh_auth_sock() -> Option<String> {
    if cfg!(target_os = "macos") {
        // Docker Desktop handles SSH forwarding internally — only mount if
        // the host has an SSH agent running (the Docker-side path always
        // exists inside the VM when Docker Desktop's SSH agent is enabled).
        std::env::var("SSH_AUTH_SOCK")
            .ok()
            .filter(|p| !p.is_empty() && Path::new(p).exists())
            .map(|_| "/run/host-services/ssh-auth.sock".to_string())
    } else {
        std::env::var("SSH_AUTH_SOCK")
            .ok()
            .filter(|p| !p.is_empty() && Path::new(p).exists())
    }
}

/// Append kap-generated entries to .gitignore if not already present.
pub fn gitignore_overlay(project_dir: &Path) -> Result<()> {
    let gitignore_path = project_dir.join(".gitignore");
    let entries = [
        format!(".devcontainer/{OVERLAY_FILENAME}"),
        ".devcontainer/.env".to_string(),
    ];

    let existing = if gitignore_path.exists() {
        std::fs::read_to_string(&gitignore_path)?
    } else {
        String::new()
    };

    let new_entries: Vec<&str> = entries
        .iter()
        .filter(|e| !existing.lines().any(|line| line.trim() == e.as_str()))
        .map(|e| e.as_str())
        .collect();

    if new_entries.is_empty() {
        return Ok(());
    }

    let separator = if existing.is_empty() || existing.ends_with('\n') {
        ""
    } else {
        "\n"
    };
    let block = new_entries.join("\n");
    std::fs::write(
        &gitignore_path,
        format!("{existing}{separator}\n# Generated by kap\n{block}\n"),
    )?;
    Ok(())
}

struct SetupOptions {
    install_claude_code: bool,
    install_codex: bool,
    install_gh: bool,
    ssh_signing: bool,
}

impl SetupOptions {
    fn any_enabled(&self) -> bool {
        self.install_claude_code || self.install_codex || self.install_gh || self.ssh_signing
    }
}

/// Check if the host uses a custom gpg.ssh.program that won't exist in the container
/// (e.g. 1Password's op-ssh-sign, Secretive, etc.). Returns the program name if set
/// and it's not already a portable binary like ssh-keygen or gpg.
fn detect_ssh_signing_program() -> Option<String> {
    let output = std::process::Command::new("git")
        .args(["config", "--global", "gpg.ssh.program"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let program = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if program.is_empty() {
        return None;
    }
    // ssh-keygen and gpg already work in containers — no override needed
    let basename = program.rsplit('/').next().unwrap_or(&program);
    if basename == "ssh-keygen" || basename == "gpg" || basename == "gpg2" {
        return None;
    }
    Some(program)
}

fn is_1password_ssh_program(program: &str) -> bool {
    program.contains("op-ssh-sign")
}

/// On macOS, create the 1Password SSH agent LaunchAgent if missing.
/// This symlinks the 1Password agent socket to $SSH_AUTH_SOCK so Docker Desktop
/// can forward it into containers.
fn ensure_1password_launch_agent() -> Result<()> {
    if !cfg!(target_os = "macos") {
        return Ok(());
    }
    let home = std::env::var("HOME").unwrap_or_default();
    let plist_dir = Path::new(&home).join("Library/LaunchAgents");
    let plist_path = plist_dir.join("com.1password.SSH_AUTH_SOCK.plist");
    if plist_path.exists() {
        return Ok(());
    }
    std::fs::create_dir_all(&plist_dir)?;
    let plist_content = include_str!("../static/com.1password.SSH_AUTH_SOCK.plist");
    std::fs::write(&plist_path, plist_content)?;
    let status = std::process::Command::new("launchctl")
        .args(["load", "-w"])
        .arg(&plist_path)
        .status();
    match status {
        Ok(s) if s.success() => {
            println!("  Created 1Password SSH agent LaunchAgent.");
            println!("  Restart Docker Desktop for SSH agent forwarding to take effect.");
        }
        _ => {
            println!(
                "  Wrote {} but launchctl load failed. Run manually:",
                plist_path.display()
            );
            println!("    launchctl load -w {}", plist_path.display());
        }
    }
    Ok(())
}

fn prompt_setup_options(yes: bool) -> SetupOptions {
    let ssh_program = detect_ssh_signing_program();
    let gh_detected = !detect_cli_tools().is_empty();

    if yes {
        return SetupOptions {
            install_claude_code: true,
            install_codex: false,
            install_gh: gh_detected,
            ssh_signing: ssh_program.is_some(),
        };
    }

    println!();
    println!("  Optional setup (runs on each container start, idempotent):");
    println!();

    let install_claude_code = confirm("  Install Claude Code?");
    let install_codex = confirm("  Install Codex (OpenAI)?");
    let install_gh = if gh_detected {
        confirm("  Install GitHub CLI in container?")
    } else {
        false
    };

    let ssh_signing = if let Some(ref program) = ssh_program {
        let basename = program.rsplit('/').next().unwrap_or(program);
        println!("  SSH commit signing detected (gpg.ssh.program = {basename}).");
        println!(
            "  This program won't exist in the container; kap can override it with ssh-keygen."
        );
        confirm("  Configure git commit signing in container?")
    } else {
        false
    };

    SetupOptions {
        install_claude_code,
        install_codex,
        install_gh,
        ssh_signing,
    }
}

/// If SSH signing is enabled and 1Password is the SSH program, ensure the
/// macOS LaunchAgent exists so Docker Desktop can forward the agent.
fn maybe_setup_1password_agent() {
    if let Some(program) = detect_ssh_signing_program()
        && is_1password_ssh_program(&program)
        && let Err(e) = ensure_1password_launch_agent()
    {
        eprintln!("  Warning: could not create LaunchAgent: {e}");
    }
}

/// Check whether a devcontainer.json value already contains `kap sidecar-init`.
/// Handles string, array (exec form), and object (parallel commands) formats.
fn has_kap_sidecar_init(value: &serde_json::Value) -> bool {
    match value {
        serde_json::Value::String(s) => s.contains("kap sidecar-init"),
        serde_json::Value::Array(arr) => {
            let joined = arr
                .iter()
                .filter_map(|v| v.as_str())
                .collect::<Vec<_>>()
                .join(" ");
            joined.contains("kap sidecar-init")
        }
        serde_json::Value::Object(obj) => obj.values().any(has_kap_sidecar_init),
        _ => false,
    }
}

fn generate_post_start_command(options: &SetupOptions) -> serde_json::Value {
    let mut obj = serde_json::Map::new();
    if options.install_claude_code {
        obj.insert(
            "claude-code".to_string(),
            serde_json::Value::String(
                concat!(
                    "command -v claude >/dev/null 2>&1 || curl -fsSL https://claude.ai/install.sh | bash; ",
                    "[ -f ~/.claude.json ] || echo '{\"hasCompletedOnboarding\":true}' > ~/.claude.json",
                )
                .to_string(),
            ),
        );
    }
    if options.install_codex {
        obj.insert(
            "codex".to_string(),
            serde_json::Value::String(
                "command -v codex >/dev/null 2>&1 || { command -v npm >/dev/null 2>&1 && npm install -g @openai/codex || true; }"
                    .to_string(),
            ),
        );
    }
    if options.install_gh {
        obj.insert(
            "gh".to_string(),
            serde_json::Value::String(
                concat!(
                    "command -v gh >/dev/null 2>&1 || { ",
                    "curl -fsSL https://cli.github.com/packages/githubcli-archive-keyring.gpg ",
                    "| sudo dd of=/usr/share/keyrings/githubcli-archive-keyring.gpg 2>/dev/null ",
                    "&& echo \"deb [arch=$(dpkg --print-architecture) ",
                    "signed-by=/usr/share/keyrings/githubcli-archive-keyring.gpg] ",
                    "https://cli.github.com/packages stable main\" ",
                    "| sudo tee /etc/apt/sources.list.d/github-cli.list >/dev/null ",
                    "&& sudo apt-get update -qq && sudo apt-get install -y gh; }",
                )
                .to_string(),
            ),
        );
    }
    if options.ssh_signing {
        obj.insert(
            "ssh-signing".to_string(),
            serde_json::Value::String(
                concat!(
                    "git config --global gpg.ssh.program /usr/bin/ssh-keygen; ",
                    "KEY=$(git config --global user.signingkey 2>/dev/null || true); ",
                    "[ -n \"$KEY\" ] && echo \"$KEY\" > ~/.ssh-signing-key.pub ",
                    "&& git config --global user.signingkey ~/.ssh-signing-key.pub || true",
                )
                .to_string(),
            ),
        );
    }
    serde_json::Value::Object(obj)
}

/// Existing project: create config, generate overlay, update devcontainer.json.
fn run_existing(project: &Path, devcontainer_dir: &Path, yes: bool, force: bool) -> Result<()> {
    let devcontainer_json_path = devcontainer_dir.join("devcontainer.json");
    if !devcontainer_json_path.exists() {
        anyhow::bail!(
            ".devcontainer/ exists but has no devcontainer.json at {}",
            devcontainer_json_path.display()
        );
    }

    let kap_toml_path = devcontainer_dir.join("kap.toml");
    if kap_toml_path.exists() && !force {
        anyhow::bail!(
            "kap.toml already exists at {}. Use --force to overwrite.",
            kap_toml_path.display()
        );
    }

    let dc_content = std::fs::read_to_string(&devcontainer_json_path)?;
    let dc_json: serde_json::Value = parse_jsonc(&dc_content)?;

    let image_based = dc_json.get("image").is_some() && dc_json.get("dockerComposeFile").is_none();

    // If image-based, ask to convert to compose mode
    if image_based {
        let image = dc_json["image"]
            .as_str()
            .unwrap_or("mcr.microsoft.com/devcontainers/base:ubuntu");

        println!();
        println!(
            "  Your devcontainer uses \x1b[1mimage\x1b[0m mode, but kap requires Docker Compose."
        );
        println!("  I'll convert it for you:");
        println!();
        println!("    \x1b[32m+\x1b[0m Create docker-compose.yml  (image: {image})");
        println!(
            "    \x1b[33m~\x1b[0m Update devcontainer.json   (add service, workspaceFolder, dockerComposeFile)"
        );
        println!(
            "    \x1b[31m-\x1b[0m Remove \"image\" field        (moved to docker-compose.yml)"
        );
        println!();

        if !yes && !confirm("  Proceed?") {
            anyhow::bail!("aborted");
        }

        let compose_path = devcontainer_dir.join("docker-compose.yml");
        if !compose_path.exists() {
            write_file(
                &compose_path,
                &format!(
                    "services:\n  app:\n    image: {image}\n    volumes:\n      - ..:/workspace:cached\n    command: sleep infinity\n"
                ),
            )?;
        }
    }

    let service_name = if image_based {
        "app".to_string()
    } else {
        read_service_name(devcontainer_dir)?
    };

    let compose_files: Vec<String> = if image_based {
        vec!["docker-compose.yml".to_string()]
    } else if let Some(arr) = dc_json["dockerComposeFile"].as_array() {
        arr.iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect()
    } else if let Some(s) = dc_json["dockerComposeFile"].as_str() {
        vec![s.to_string()]
    } else {
        vec!["docker-compose.yml".to_string()]
    };

    // Write kap.toml (auto-detects CLI tools like gh)
    let config_content = generate_config();
    write_file(&kap_toml_path, &config_content)?;

    // Detect CLI tools for .env
    let detected = detect_cli_tools();

    // Generate overlay (using default ComposeConfig — user can customize in kap.toml later)
    let overlay_path = devcontainer_dir.join(OVERLAY_FILENAME);
    let compose_config = ComposeConfig::default();
    let subnet_prefix = derive_subnet(project);
    let project_name = read_project_name(devcontainer_dir);
    let ssh_auth_sock = detect_ssh_auth_sock();
    let global_config = crate::config::has_global_config();
    write_file(
        &overlay_path,
        &generate_overlay(
            &service_name,
            &compose_config,
            &subnet_prefix,
            &project_name,
            ssh_auth_sock.as_deref(),
            global_config,
        ),
    )?;

    // Write .env with shell patterns for detected CLI tools (only if it doesn't exist)
    let env_path = devcontainer_dir.join(".env");
    if !env_path.exists() {
        let env_content = generate_env_file(&detected);
        write_file(&env_path, &env_content)?;
    }

    // Update devcontainer.json
    let mut dc_obj = dc_json.clone();

    // Remove image field (now in docker-compose.yml)
    if image_based {
        dc_obj.as_object_mut().unwrap().shift_remove("image");
    }

    let mut all_compose: Vec<serde_json::Value> = compose_files
        .iter()
        .map(|f| serde_json::Value::String(f.clone()))
        .collect();
    if !compose_files.iter().any(|f| f == OVERLAY_FILENAME) {
        all_compose.push(serde_json::Value::String(OVERLAY_FILENAME.to_string()));
    }
    dc_obj["dockerComposeFile"] = serde_json::Value::Array(all_compose);
    dc_obj["service"] = serde_json::Value::String(service_name);
    if dc_obj.get("workspaceFolder").is_none() {
        dc_obj["workspaceFolder"] = serde_json::Value::String("/workspace".to_string());
    }

    let mut notes: Vec<String> = Vec::new();

    if let Some(existing) = dc_obj.get("initializeCommand") {
        if !has_kap_sidecar_init(existing) {
            notes.push(
                "initializeCommand already set. Add `kap sidecar-init` to your existing command."
                    .to_string(),
            );
        }
    } else {
        dc_obj["initializeCommand"] = serde_json::Value::String("kap sidecar-init".to_string());
    }

    // Optional setup: AI tools and SSH signing
    let setup = prompt_setup_options(yes);
    if setup.any_enabled() {
        let kap_commands = generate_post_start_command(&setup);
        let existing_is_object = dc_obj
            .get("postStartCommand")
            .is_some_and(|v| v.is_object());

        if dc_obj.get("postStartCommand").is_none() {
            dc_obj["postStartCommand"] = kap_commands;
        } else if existing_is_object {
            // Auto-merge named keys into existing object
            let kap_obj = kap_commands.as_object().unwrap();
            let merged = dc_obj["postStartCommand"].as_object_mut().unwrap();
            for (key, value) in kap_obj {
                if !merged.contains_key(key) {
                    merged.insert(key.clone(), value.clone());
                }
            }
        } else {
            // String or array — can't auto-merge. Build the replacement object
            // with the user's existing command plus kap's commands.
            let existing = dc_obj.get("postStartCommand").unwrap();
            let existing_str = match existing {
                serde_json::Value::String(s) => s.clone(),
                other => serde_json::to_string(other).unwrap(),
            };
            let mut replacement = serde_json::Map::new();
            replacement.insert(
                "existing".to_string(),
                serde_json::Value::String(existing_str),
            );
            for (key, value) in kap_commands.as_object().unwrap() {
                replacement.insert(key.clone(), value.clone());
            }
            let json =
                serde_json::to_string_pretty(&serde_json::Value::Object(replacement)).unwrap();
            notes.push(format!(
                "postStartCommand already set. Replace it with:\n  \"postStartCommand\": {}",
                json.replace('\n', "\n  ")
            ));
        }
    }
    if setup.ssh_signing {
        maybe_setup_1password_agent();
    }

    // Prepend /opt/kap/bin to PATH so CLI shims on the shared volume take precedence.
    // remoteEnv applies to all devcontainer exec and terminal sessions.
    let remote_env = dc_obj
        .as_object_mut()
        .unwrap()
        .entry("remoteEnv")
        .or_insert_with(|| serde_json::json!({}));
    if remote_env.get("PATH").is_none() {
        remote_env["PATH"] =
            serde_json::Value::String("/opt/kap/bin:${containerEnv:PATH}".to_string());
    }

    let updated = serde_json::to_string_pretty(&dc_obj)?;
    write_file(&devcontainer_json_path, &format!("{updated}\n"))?;

    // Add overlay to .gitignore
    gitignore_overlay(project)?;

    println!();
    println!("Created .devcontainer/kap.toml");
    println!("Created .devcontainer/{OVERLAY_FILENAME} (generated, gitignored)");
    println!("Updated .devcontainer/devcontainer.json");

    for note in &notes {
        println!();
        println!("  NOTE: {note}");
    }

    println!();
    println!("Next:");
    println!("  kap up");

    Ok(())
}

/// New project: create everything from scratch.
fn run_new(project: &Path, devcontainer_dir: &Path, yes: bool) -> Result<()> {
    std::fs::create_dir_all(devcontainer_dir)?;

    let project_name = project
        .canonicalize()
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_string()))
        .unwrap_or_else(|| "my-project".to_string());

    let setup = prompt_setup_options(yes);

    write_file(&devcontainer_dir.join("kap.toml"), &generate_config())?;
    write_file(
        &devcontainer_dir.join("docker-compose.yml"),
        &generate_app_compose(&project_name),
    )?;
    write_file(
        &devcontainer_dir.join("devcontainer.json"),
        &generate_devcontainer_json(&project_name, &setup),
    )?;
    if setup.ssh_signing {
        maybe_setup_1password_agent();
    }

    // Generate the overlay so things work immediately
    let compose_config = ComposeConfig::default();
    let subnet_prefix = derive_subnet(project);
    let ssh_auth_sock = detect_ssh_auth_sock();
    let global_config = crate::config::has_global_config();
    write_file(
        &devcontainer_dir.join(OVERLAY_FILENAME),
        &generate_overlay(
            "app",
            &compose_config,
            &subnet_prefix,
            &project_name,
            ssh_auth_sock.as_deref(),
            global_config,
        ),
    )?;

    // Write .env with shell patterns for detected CLI tools
    let detected = detect_cli_tools();
    let env_content = generate_env_file(&detected);
    write_file(&devcontainer_dir.join(".env"), &env_content)?;

    // Add overlay to .gitignore
    gitignore_overlay(project)?;

    println!("Created .devcontainer/ with:");
    println!("  kap.toml            - kap config (edit allowed domains here)");
    println!("  docker-compose.yml   - app container definition");
    println!("  devcontainer.json    - devcontainer config");
    println!("  {OVERLAY_FILENAME}   - kap sidecar (generated, gitignored)");
    println!();
    println!("Next steps:");
    println!("  1. Review kap.toml and adjust allowed domains");
    println!("  2. Run: kap up");

    Ok(())
}

struct DomainGroup {
    label: &'static str,
    /// Each entry is `"domain"` or `"domain # inline comment"`.
    /// The `#` separator is split at config-generation time to produce a TOML inline comment.
    domains: &'static [&'static str],
}

const DEFAULT_DOMAIN_GROUPS: &[DomainGroup] = &[
    DomainGroup {
        label: "GitHub",
        domains: &[
            "github.com",
            "*.github.com",
            "*.githubusercontent.com",
            "*.blob.core.windows.net # GitHub Actions artifact downloads",
        ],
    },
    DomainGroup {
        label: "AI providers",
        domains: &[
            "anthropic.com",
            "*.anthropic.com",
            "claude.ai",
            "*.claude.ai",
            "claude.com",
            "*.claude.com",
            "openai.com",
            "*.openai.com",
            "generativelanguage.googleapis.com",
            "storage.googleapis.com",
        ],
    },
    DomainGroup {
        label: "APT",
        domains: &["*.ubuntu.com", "*.debian.org"],
    },
    DomainGroup {
        label: "Dev tools",
        domains: &["mise.jdx.dev", "mise-versions.jdx.dev"],
    },
    DomainGroup {
        label: "Sigstore (software signature verification)",
        domains: &["tuf-repo-cdn.sigstore.dev"],
    },
    DomainGroup {
        label: "Ruby",
        domains: &[
            "rubygems.org",
            "*.rubygems.org",
            "bundler.io",
            "*.ruby-lang.org",
            "rubyonrails.org",
            "*.rubyonrails.org",
        ],
    },
    DomainGroup {
        label: "Node",
        domains: &["*.npmjs.org", "*.npmjs.com", "nodejs.org", "*.yarnpkg.com"],
    },
    DomainGroup {
        label: "Rust",
        domains: &[
            "crates.io",
            "*.crates.io",
            "rustup.rs",
            "*.rustup.rs",
            "*.rust-lang.org",
        ],
    },
    DomainGroup {
        label: "Python",
        domains: &["pypi.org", "*.pypi.org", "*.pythonhosted.org"],
    },
    DomainGroup {
        label: "Go",
        domains: &["proxy.golang.org", "sum.golang.org"],
    },
    DomainGroup {
        label: "Java",
        domains: &[
            "repo.maven.apache.org",
            "*.maven.org",
            "plugins.gradle.org",
            "services.gradle.org",
            "downloads.gradle-dn.com",
        ],
    },
    DomainGroup {
        label: "CocoaPods",
        domains: &["cocoapods.org", "*.cocoapods.org"],
    },
];

#[cfg(test)]
fn all_default_domains() -> Vec<&'static str> {
    DEFAULT_DOMAIN_GROUPS
        .iter()
        .flat_map(|g| {
            g.domains.iter().map(|d| match d.split_once(" # ") {
                Some((domain, _)) => domain,
                None => d,
            })
        })
        .collect()
}

struct DetectedTool {
    name: &'static str,
    env: &'static [&'static str],
    allow: &'static [&'static str],
    deny: &'static [&'static str],
    /// Default shell expressions for env vars (written to .env during init).
    env_defaults: &'static [(&'static str, &'static str)],
}

const DETECTABLE_TOOLS: &[DetectedTool] = &[DetectedTool {
    name: "gh",
    env: &["GH_TOKEN"],
    allow: &["*"],
    deny: &["auth token", "auth login", "auth logout", "auth refresh"],
    env_defaults: &[("GH_TOKEN", "$(gh auth token)")],
}];

/// Look up the default shell expression for an env var (e.g. "GH_TOKEN" -> "$(gh auth token)").
pub fn env_var_default(var: &str) -> Option<&'static str> {
    DETECTABLE_TOOLS
        .iter()
        .flat_map(|t| t.env_defaults.iter())
        .find(|(name, _)| *name == var)
        .map(|(_, expr)| *expr)
}

/// Return the default env var names for a tool (from DETECTABLE_TOOLS).
/// E.g. "gh" → ["GH_TOKEN"].
pub fn default_env_for_tool(tool_name: &str) -> Vec<String> {
    DETECTABLE_TOOLS
        .iter()
        .find(|t| t.name == tool_name)
        .map(|t| t.env.iter().map(|e| e.to_string()).collect())
        .unwrap_or_default()
}

fn detect_cli_tools() -> Vec<&'static DetectedTool> {
    DETECTABLE_TOOLS
        .iter()
        .filter(|t| {
            std::process::Command::new("which")
                .arg(t.name)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .is_ok_and(|s| s.success())
        })
        .collect()
}

fn generate_config() -> String {
    // Build allow list with category comments
    let mut allow_lines: Vec<String> = Vec::new();
    for (i, group) in DEFAULT_DOMAIN_GROUPS.iter().enumerate() {
        if i > 0 {
            allow_lines.push(String::new()); // blank line between groups
        }
        allow_lines.push(format!("  # {}", group.label));
        for (j, domain) in group.domains.iter().enumerate() {
            let comma = if i == DEFAULT_DOMAIN_GROUPS.len() - 1 && j == group.domains.len() - 1 {
                "" // no trailing comma on last entry
            } else {
                ","
            };
            if let Some((d, comment)) = domain.split_once(" # ") {
                allow_lines.push(format!("  \"{d}\"{comma}  # {comment}"));
            } else {
                allow_lines.push(format!("  \"{domain}\"{comma}"));
            }
        }
    }
    let allow_toml = allow_lines.join("\n");

    let detected = detect_cli_tools();
    let cli_section = if detected.is_empty() {
        r#"
# --- CLI tool proxying (credentials stay on sidecar, never enter app container) ---
# Uncomment to proxy a CLI tool:
# [cli]
# [[cli.tools]]
# name = "gh"
# mode = "proxy"
# env = ["GH_TOKEN"]
# allow = ["*"]
# deny = ["auth token", "auth login", "auth logout", "auth refresh"]
"#
        .to_string()
    } else {
        let tools: Vec<String> = detected
            .iter()
            .map(|t| {
                let env = t
                    .env
                    .iter()
                    .map(|e| format!("\"{e}\""))
                    .collect::<Vec<_>>()
                    .join(", ");
                let allow = t
                    .allow
                    .iter()
                    .map(|a| format!("\"{a}\""))
                    .collect::<Vec<_>>()
                    .join(", ");
                let deny_line = if t.deny.is_empty() {
                    String::new()
                } else {
                    let deny = t
                        .deny
                        .iter()
                        .map(|d| format!("\"{d}\""))
                        .collect::<Vec<_>>()
                        .join(", ");
                    format!("\ndeny = [{deny}]")
                };
                format!(
                    "\n[[cli.tools]]\nname = \"{}\"\nmode = \"proxy\"\nenv = [{}]\nallow = [{}]{deny_line}",
                    t.name, env, allow
                )
            })
            .collect();
        format!(
            "\n# --- CLI tool proxying (credentials stay on sidecar, never enter app container) ---\n[cli]{}\n",
            tools.join("\n")
        )
    };

    format!(
        r#"# kap.toml — network and tool policy for this devcontainer

# Forward host SSH agent into the container (for git over SSH, commit signing, etc.)
# On macOS, uses Docker Desktop's built-in SSH forwarding.
# On Linux, bind-mounts $SSH_AUTH_SOCK directly.
ssh_agent = true

[proxy.network]
# Domains the container can reach. Wildcards supported (*.example.com).
# Everything else is blocked — both HTTP/HTTPS and DNS.
allow = [
{allow_toml}
]
# deny overrides allow:
# deny = ["gist.github.com"]
# Global defaults from ~/.kap/kap.toml are merged automatically.

# --- MCP servers (tool-level filtering for remote MCP) ---
# Register with `kap mcp add <url>`, then restrict tools:
# [mcp]
# [[mcp.servers]]
# name = "github"
# allow = ["get_pull_request", "list_issues"]
{cli_section}"#
    )
}

/// Generate the app-only compose file for new projects.
/// The kap sidecar is in the generated overlay (docker-compose.kap.yml).
fn generate_app_compose(project_name: &str) -> String {
    format!(
        r#"services:
  app:
    image: mcr.microsoft.com/devcontainers/base:ubuntu
    volumes:
      - ..:/workspaces/{project_name}:cached
    command: sleep infinity
"#
    )
}

fn generate_devcontainer_json(project_name: &str, setup: &SetupOptions) -> String {
    let post_start = if setup.any_enabled() {
        let cmd = generate_post_start_command(setup);
        format!(
            ",\n  \"postStartCommand\": {}",
            serde_json::to_string(&cmd).unwrap()
        )
    } else {
        String::new()
    };
    format!(
        r#"{{
  "name": "{project_name}",
  "dockerComposeFile": ["docker-compose.yml", "{OVERLAY_FILENAME}"],
  "service": "app",
  "workspaceFolder": "/workspaces/{project_name}",
  "initializeCommand": "kap sidecar-init",
  "remoteUser": "vscode",
  "remoteEnv": {{
    "PATH": "/opt/kap/bin:${{containerEnv:PATH}}"
  }}{post_start}
}}
"#
    )
}

fn generate_env_file(detected: &[&DetectedTool]) -> String {
    let mut lines: Vec<String> = Vec::new();
    for tool in detected {
        for (var, expr) in tool.env_defaults {
            lines.push(format!("{var}={expr}"));
        }
    }
    lines.join("\n")
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
        let domains = all_default_domains();
        assert!(domains.contains(&"github.com"));
        assert!(domains.contains(&"anthropic.com"));
        assert!(domains.contains(&"crates.io"));
        assert!(domains.contains(&"*.npmjs.org"));
        assert!(domains.contains(&"pypi.org"));
        assert!(domains.contains(&"proxy.golang.org"));
        assert!(domains.contains(&"rubygems.org"));
        assert!(domains.contains(&"cocoapods.org"));
        assert!(domains.contains(&"repo.maven.apache.org"));
        assert!(domains.contains(&"mise.jdx.dev"));
    }

    #[test]
    fn generate_config_has_category_comments() {
        let config = generate_config();
        assert!(config.contains("# GitHub"));
        assert!(config.contains("# AI providers"));
        assert!(config.contains("# APT"));
        assert!(config.contains("# Dev tools"));
        assert!(config.contains("# Ruby"));
        assert!(config.contains("# Node"));
        assert!(config.contains("# Rust"));
        assert!(config.contains("# Python"));
        assert!(config.contains("# Go"));
        assert!(config.contains("# Java"));
        assert!(config.contains("# CocoaPods"));
        assert!(config.contains("# --- MCP servers"));
        // Inline domain comments should appear in generated config
        assert!(
            config.contains("\"*.blob.core.windows.net\",  # GitHub Actions artifact downloads"),
            "inline comment missing for windows.net domain"
        );
    }

    #[test]
    fn new_project_scaffolds_all_files() {
        let dir = tempdir("scaffold-new");
        run(dir.to_str().unwrap(), true, false).unwrap();
        let dc = dir.join(".devcontainer");
        assert!(dc.join("kap.toml").exists());
        assert!(dc.join("docker-compose.yml").exists());
        assert!(dc.join("devcontainer.json").exists());
        assert!(dc.join(OVERLAY_FILENAME).exists());

        let config = fs::read_to_string(dc.join("kap.toml")).unwrap();
        assert!(config.contains("allow ="));

        // App compose should NOT contain kap service (it's in the overlay)
        let compose = fs::read_to_string(dc.join("docker-compose.yml")).unwrap();
        assert!(compose.contains("app:"));
        assert!(!compose.contains("kap:"));

        // Overlay should contain kap service
        let overlay = fs::read_to_string(dc.join(OVERLAY_FILENAME)).unwrap();
        assert!(overlay.contains("kap:"));
        assert!(overlay.contains("kap_sandbox:"));

        // devcontainer.json should reference both compose files
        let dcjson: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(dc.join("devcontainer.json")).unwrap())
                .unwrap();
        let compose_arr = dcjson["dockerComposeFile"].as_array().unwrap();
        assert_eq!(compose_arr.len(), 2);
        assert_eq!(compose_arr[0], "docker-compose.yml");
        assert_eq!(compose_arr[1], OVERLAY_FILENAME);

        // .gitignore should contain overlay
        let gitignore = fs::read_to_string(dir.join(".gitignore")).unwrap();
        assert!(gitignore.contains(OVERLAY_FILENAME));

        // .env should be created
        assert!(dc.join(".env").exists());

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

        run(dir.to_str().unwrap(), true, false).unwrap();

        assert!(dc.join("kap.toml").exists());
        assert!(dc.join(OVERLAY_FILENAME).exists());
        assert!(!dc.join("docker-compose.yml").exists());

        let overlay = fs::read_to_string(dc.join(OVERLAY_FILENAME)).unwrap();
        assert!(overlay.contains("myapp:"));
        assert!(overlay.contains("kap:"));
        assert!(overlay.contains("kap_sandbox:"));

        // devcontainer.json should be updated with overlay and initializeCommand
        let updated: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(dc.join("devcontainer.json")).unwrap())
                .unwrap();
        let compose_arr = updated["dockerComposeFile"].as_array().unwrap();
        assert_eq!(compose_arr.len(), 2);
        assert_eq!(compose_arr[0], "compose.yaml");
        assert_eq!(compose_arr[1], OVERLAY_FILENAME);
        assert_eq!(updated["initializeCommand"], "kap sidecar-init");

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn existing_project_no_duplicate_overlay_entry() {
        let dir = tempdir("scaffold-no-dup");
        let dc = dir.join(".devcontainer");
        fs::create_dir_all(&dc).unwrap();
        fs::write(
            dc.join("devcontainer.json"),
            format!(
                r#"{{"service": "app", "dockerComposeFile": ["docker-compose.yml", "{OVERLAY_FILENAME}"], "workspaceFolder": "/workspaces/myproject", "initializeCommand": "kap sidecar-init"}}"#
            ),
        )
        .unwrap();

        run(dir.to_str().unwrap(), true, false).unwrap();

        let updated: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(dc.join("devcontainer.json")).unwrap())
                .unwrap();
        let compose_arr = updated["dockerComposeFile"].as_array().unwrap();
        assert_eq!(compose_arr.len(), 2); // no duplicate
        assert_eq!(compose_arr[1], OVERLAY_FILENAME);
        // workspaceFolder should be preserved, not overwritten
        assert_eq!(updated["workspaceFolder"], "/workspaces/myproject");

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

        run(dir.to_str().unwrap(), true, false).unwrap();

        let updated: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(dc.join("devcontainer.json")).unwrap())
                .unwrap();
        let compose_arr = updated["dockerComposeFile"].as_array().unwrap();
        assert_eq!(compose_arr.len(), 3);
        assert_eq!(compose_arr[0], "docker-compose.yml");
        assert_eq!(compose_arr[1], "docker-compose.override.yml");
        assert_eq!(compose_arr[2], OVERLAY_FILENAME);

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn existing_project_fails_if_kap_toml_exists() {
        let dir = tempdir("scaffold-exists");
        let dc = dir.join(".devcontainer");
        fs::create_dir_all(&dc).unwrap();
        fs::write(dc.join("devcontainer.json"), r#"{"service": "app"}"#).unwrap();
        fs::write(dc.join("kap.toml"), "").unwrap();

        assert!(run(dir.to_str().unwrap(), true, false).is_err());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn existing_project_force_overwrites_kap_toml() {
        let dir = tempdir("scaffold-force");
        let dc = dir.join(".devcontainer");
        fs::create_dir_all(&dc).unwrap();
        fs::write(dc.join("devcontainer.json"), r#"{"service": "app"}"#).unwrap();
        fs::write(dc.join("kap.toml"), "# old config").unwrap();

        run(dir.to_str().unwrap(), true, true).unwrap();
        let content = fs::read_to_string(dc.join("kap.toml")).unwrap();
        assert!(content.contains("[proxy.network]")); // fresh config
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn existing_project_fails_if_no_devcontainer_json() {
        let dir = tempdir("scaffold-no-json");
        let dc = dir.join(".devcontainer");
        fs::create_dir_all(&dc).unwrap();
        // .devcontainer/ exists but no devcontainer.json

        assert!(run(dir.to_str().unwrap(), true, false).is_err());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn overlay_uses_default_service_name() {
        let dir = tempdir("scaffold-default-svc");
        let dc = dir.join(".devcontainer");
        fs::create_dir_all(&dc).unwrap();
        // No "service" field in json
        fs::write(dc.join("devcontainer.json"), r#"{}"#).unwrap();

        run(dir.to_str().unwrap(), true, false).unwrap();

        let overlay = fs::read_to_string(dc.join(OVERLAY_FILENAME)).unwrap();
        assert!(overlay.contains("app:"));

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn existing_image_based_converts_to_compose() {
        let dir = tempdir("scaffold-image-based");
        let dc = dir.join(".devcontainer");
        fs::create_dir_all(&dc).unwrap();
        // Use multiple keys after "image" to verify shift_remove preserves order
        // (swap_remove would move remoteUser into image's position, before postCreateCommand)
        fs::write(
            dc.join("devcontainer.json"),
            r#"{"name": "test", "image": "mcr.microsoft.com/devcontainers/base:ubuntu-24.04", "postCreateCommand": "echo hi", "remoteUser": "vscode"}"#,
        )
        .unwrap();

        run(dir.to_str().unwrap(), true, false).unwrap();

        // docker-compose.yml should be created with the image
        let compose = fs::read_to_string(dc.join("docker-compose.yml")).unwrap();
        assert!(compose.contains("mcr.microsoft.com/devcontainers/base:ubuntu-24.04"));
        assert!(compose.contains("app:"));

        // devcontainer.json should have compose fields, no image
        let updated: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(dc.join("devcontainer.json")).unwrap())
                .unwrap();
        assert!(updated.get("image").is_none());
        assert_eq!(updated["service"], "app");
        assert_eq!(updated["workspaceFolder"], "/workspace");
        let compose_arr = updated["dockerComposeFile"].as_array().unwrap();
        assert_eq!(compose_arr.len(), 2);
        assert_eq!(compose_arr[0], "docker-compose.yml");
        assert_eq!(compose_arr[1], OVERLAY_FILENAME);

        // Key order should be preserved: shift_remove keeps postCreateCommand before remoteUser
        let raw = fs::read_to_string(dc.join("devcontainer.json")).unwrap();
        let name_pos = raw.find("\"name\"").unwrap();
        let service_pos = raw.find("\"service\"").unwrap();
        assert!(name_pos < service_pos);
        let post_create_pos = raw.find("\"postCreateCommand\"").unwrap();
        let remote_user_pos = raw.find("\"remoteUser\"").unwrap();
        assert!(
            post_create_pos < remote_user_pos,
            "postCreateCommand should stay before remoteUser (shift_remove preserves order)"
        );

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn overlay_with_build_config() {
        let compose = ComposeConfig {
            image: None,
            build: Some(crate::config::ComposeBuild {
                context: "..".to_string(),
                dockerfile: Some(".devcontainer/Dockerfile".to_string()),
                target: Some("proxy".to_string()),
            }),
        };
        let overlay = generate_overlay("app", &compose, "172.28.0", "test-project", None, false);
        assert!(overlay.contains("build:"));
        assert!(overlay.contains("context: .."));
        assert!(overlay.contains("dockerfile: .devcontainer/Dockerfile"));
        assert!(overlay.contains("target: proxy"));
        assert!(!overlay.contains("image:"));
    }

    #[test]
    fn overlay_with_default_image() {
        let compose = ComposeConfig::default();
        let overlay = generate_overlay("app", &compose, "172.28.0", "test-project", None, false);
        assert!(overlay.contains("image: ghcr.io/6/kap:latest"));
        assert!(!overlay.contains("build:"));
    }

    #[test]
    fn overlay_includes_proxy_logs_volume() {
        let compose = ComposeConfig::default();
        let overlay = generate_overlay("app", &compose, "172.28.0", "test-project", None, false);
        assert!(overlay.contains("proxy-logs:/var/log/kap"));
        assert!(overlay.contains("volumes:\n  proxy-logs:"));
    }

    #[test]
    fn gitignore_overlay_creates_file() {
        let dir = tempdir("gitignore-create");
        gitignore_overlay(&dir).unwrap();
        let content = fs::read_to_string(dir.join(".gitignore")).unwrap();
        assert!(content.contains(OVERLAY_FILENAME));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn gitignore_overlay_appends_to_existing() {
        let dir = tempdir("gitignore-append");
        fs::write(dir.join(".gitignore"), "target/\n").unwrap();
        gitignore_overlay(&dir).unwrap();
        let content = fs::read_to_string(dir.join(".gitignore")).unwrap();
        assert!(content.starts_with("target/\n"));
        assert!(content.contains(OVERLAY_FILENAME));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn gitignore_overlay_is_idempotent() {
        let dir = tempdir("gitignore-idempotent");
        gitignore_overlay(&dir).unwrap();
        gitignore_overlay(&dir).unwrap();
        let content = fs::read_to_string(dir.join(".gitignore")).unwrap();
        let count = content.matches(OVERLAY_FILENAME).count();
        assert_eq!(count, 1);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn derive_subnet_is_deterministic() {
        let dir = tempdir("subnet-det");
        let a = derive_subnet(&dir);
        let b = derive_subnet(&dir);
        assert_eq!(a, b);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn derive_subnet_differs_for_different_dirs() {
        let dir1 = tempdir("subnet-a");
        let dir2 = tempdir("subnet-b");
        let s1 = derive_subnet(&dir1);
        let s2 = derive_subnet(&dir2);
        // Not guaranteed to differ for all inputs, but these specific names should
        assert_ne!(s1, s2);
        fs::remove_dir_all(&dir1).unwrap();
        fs::remove_dir_all(&dir2).unwrap();
    }

    #[test]
    fn derive_subnet_in_valid_range() {
        let dir = tempdir("subnet-range");
        let prefix = derive_subnet(&dir);
        let parts: Vec<&str> = prefix.split('.').collect();
        assert_eq!(parts[0], "172");
        let second: u8 = parts[1].parse().unwrap();
        assert!((18..=31).contains(&second));
        let _third: u8 = parts[2].parse().unwrap(); // 0-255, always valid
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn overlay_uses_custom_subnet() {
        let compose = ComposeConfig::default();
        let overlay = generate_overlay("app", &compose, "172.25.42", "test-project", None, false);
        assert!(overlay.contains("172.25.42.2")); // app IP
        assert!(overlay.contains("172.25.42.3")); // sidecar IP
        assert!(overlay.contains("172.25.42.0/24")); // subnet
        assert!(!overlay.contains("172.28.0")); // no old default
        assert!(overlay.contains("hostname: test-project"));
    }

    #[test]
    fn read_project_name_from_devcontainer_json() {
        let dir = tempdir("project-name");
        fs::write(
            dir.join("devcontainer.json"),
            r#"{"name": "my-cool-project"}"#,
        )
        .unwrap();
        assert_eq!(read_project_name(&dir), "my-cool-project");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn read_project_name_falls_back_to_dir_name() {
        let dir = tempdir("project-name-fallback");
        // No devcontainer.json — should fall back to parent dir name
        assert_eq!(
            read_project_name(&dir),
            dir.parent()
                .unwrap()
                .file_name()
                .unwrap()
                .to_string_lossy()
                .to_string()
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn read_project_name_no_name_field() {
        let dir = tempdir("project-name-nofield");
        fs::write(dir.join("devcontainer.json"), r#"{"service": "app"}"#).unwrap();
        // Falls back to directory name since "name" field is missing
        let name = read_project_name(&dir);
        assert!(!name.is_empty());
        assert_ne!(name, "dev"); // should get the actual dir name, not the hardcoded fallback
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn generate_env_file_with_detected_tools() {
        let tool = DetectedTool {
            name: "gh",
            env: &["GH_TOKEN"],
            allow: &["*"],
            deny: &[],
            env_defaults: &[("GH_TOKEN", "$(gh auth token)")],
        };
        let content = generate_env_file(&[&tool]);
        assert_eq!(content, "GH_TOKEN=$(gh auth token)");
    }

    #[test]
    fn generate_env_file_empty_when_no_tools() {
        let content = generate_env_file(&[]);
        assert_eq!(content, "");
    }

    #[test]
    fn env_var_default_known_var() {
        assert_eq!(env_var_default("GH_TOKEN"), Some("$(gh auth token)"));
    }

    #[test]
    fn env_var_default_unknown_var() {
        assert_eq!(env_var_default("UNKNOWN_VAR"), None);
    }

    #[test]
    fn overlay_mounts_kap_bin_and_shims_when_tools_configured() {
        let compose = ComposeConfig::default();
        let overlay = generate_overlay("app", &compose, "172.28.0", "test", None, false);
        // kap-bin volume always mounted (shims written by sidecar at runtime)
        assert!(overlay.contains("kap-bin:/opt/kap:ro"));
        // No configs: section — shims are managed via write_shims + remoteEnv.PATH
        assert!(!overlay.contains("configs:"));
        assert!(!overlay.contains("cli-shim"));
    }

    #[test]
    fn overlay_contains_hostname() {
        let compose = ComposeConfig::default();
        let overlay = generate_overlay("app", &compose, "172.28.0", "my-project", None, false);
        assert!(overlay.contains("hostname: my-project"));
    }

    #[test]
    fn overlay_includes_ssh_agent_when_set() {
        let compose = ComposeConfig::default();
        let overlay = generate_overlay(
            "app",
            &compose,
            "172.28.0",
            "test",
            Some("/run/host-services/ssh-auth.sock"),
            false,
        );
        assert!(overlay.contains("/run/host-services/ssh-auth.sock:/ssh-agent:ro"));
        assert!(overlay.contains("SSH_AUTH_SOCK: /ssh-agent"));
    }

    #[test]
    fn overlay_omits_ssh_agent_when_unset() {
        let compose = ComposeConfig::default();
        let overlay = generate_overlay("app", &compose, "172.28.0", "test", None, false);
        assert!(!overlay.contains("ssh-agent"));
        assert!(!overlay.contains("SSH_AUTH_SOCK"));
    }

    #[test]
    fn overlay_includes_ssh_agent_and_kap_bin_volumes() {
        let compose = ComposeConfig::default();
        let overlay = generate_overlay(
            "app",
            &compose,
            "172.28.0",
            "test",
            Some("/run/host-services/ssh-auth.sock"),
            false,
        );
        assert!(overlay.contains("kap-bin:/opt/kap:ro"));
        assert!(overlay.contains("/run/host-services/ssh-auth.sock:/ssh-agent:ro"));
        assert!(overlay.contains("SSH_AUTH_SOCK: /ssh-agent"));
    }

    #[test]
    fn overlay_includes_global_config_mount() {
        let compose = ComposeConfig::default();
        let overlay = generate_overlay("app", &compose, "172.28.0", "test", None, true);
        assert!(overlay.contains("/.kap/kap.toml:/etc/kap/global.toml:ro"));
    }

    #[test]
    fn overlay_omits_global_config_when_disabled() {
        let compose = ComposeConfig::default();
        let overlay = generate_overlay("app", &compose, "172.28.0", "test", None, false);
        assert!(!overlay.contains("global.toml"));
    }

    #[test]
    fn parse_jsonc_handles_trailing_commas() {
        let input = r#"{
  "name": "test",
  "mounts": [
    "source=a,target=b",
    "source=c,target=d",
  ],
  "containerEnv": {
    "FOO": "bar",
    "BAZ": "qux",
  },
}"#;
        let val = parse_jsonc(input).unwrap();
        assert_eq!(val["name"], "test");
        assert_eq!(val["containerEnv"]["FOO"], "bar");
        assert_eq!(val["mounts"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn parse_jsonc_strips_line_comments() {
        let input = r#"{
  // This is a comment
  "name": "test"
}"#;
        let val = parse_jsonc(input).unwrap();
        assert_eq!(val["name"], "test");
    }

    #[test]
    fn parse_jsonc_handles_plain_json() {
        let input = r#"{"name": "test", "arr": [1, 2]}"#;
        let val = parse_jsonc(input).unwrap();
        assert_eq!(val["name"], "test");
    }

    #[test]
    fn setup_options_any_enabled() {
        let none = SetupOptions {
            install_claude_code: false,
            install_codex: false,
            install_gh: false,
            ssh_signing: false,
        };
        assert!(!none.any_enabled());

        let claude = SetupOptions {
            install_claude_code: true,
            install_codex: false,
            install_gh: false,
            ssh_signing: false,
        };
        assert!(claude.any_enabled());

        let codex = SetupOptions {
            install_claude_code: false,
            install_codex: true,
            install_gh: false,
            ssh_signing: false,
        };
        assert!(codex.any_enabled());

        let ssh = SetupOptions {
            install_claude_code: false,
            install_codex: false,
            install_gh: false,
            ssh_signing: true,
        };
        assert!(ssh.any_enabled());

        let gh = SetupOptions {
            install_claude_code: false,
            install_codex: false,
            install_gh: true,
            ssh_signing: false,
        };
        assert!(gh.any_enabled());
    }

    #[test]
    fn post_start_command_claude_only() {
        let opts = SetupOptions {
            install_claude_code: true,
            install_codex: false,
            install_gh: false,
            ssh_signing: false,
        };
        let cmd = generate_post_start_command(&opts);
        let obj = cmd.as_object().unwrap();
        assert_eq!(obj.len(), 1);
        let claude_cmd = obj["claude-code"].as_str().unwrap();
        assert!(claude_cmd.contains("claude.ai/install.sh"));
        assert!(claude_cmd.contains("hasCompletedOnboarding"));
    }

    #[test]
    fn post_start_command_codex_only() {
        let opts = SetupOptions {
            install_claude_code: false,
            install_codex: true,
            install_gh: false,
            ssh_signing: false,
        };
        let cmd = generate_post_start_command(&opts);
        let obj = cmd.as_object().unwrap();
        assert_eq!(obj.len(), 1);
        assert!(obj["codex"].as_str().unwrap().contains("@openai/codex"));
    }

    #[test]
    fn post_start_command_ssh_signing_only() {
        let opts = SetupOptions {
            install_claude_code: false,
            install_codex: false,
            install_gh: false,
            ssh_signing: true,
        };
        let cmd = generate_post_start_command(&opts);
        let obj = cmd.as_object().unwrap();
        assert_eq!(obj.len(), 1);
        assert!(obj["ssh-signing"].as_str().unwrap().contains("ssh-keygen"));
    }

    #[test]
    fn post_start_command_gh_only() {
        let opts = SetupOptions {
            install_claude_code: false,
            install_codex: false,
            install_gh: true,
            ssh_signing: false,
        };
        let cmd = generate_post_start_command(&opts);
        let obj = cmd.as_object().unwrap();
        assert_eq!(obj.len(), 1);
        let gh_cmd = obj["gh"].as_str().unwrap();
        assert!(gh_cmd.starts_with("command -v gh"));
        assert!(gh_cmd.contains("github-cli"));
    }

    #[test]
    fn post_start_command_all_enabled() {
        let opts = SetupOptions {
            install_claude_code: true,
            install_codex: true,
            install_gh: true,
            ssh_signing: true,
        };
        let cmd = generate_post_start_command(&opts);
        let obj = cmd.as_object().unwrap();
        assert_eq!(obj.len(), 4);
        assert!(obj.contains_key("claude-code"));
        assert!(obj.contains_key("codex"));
        assert!(obj.contains_key("gh"));
        assert!(obj.contains_key("ssh-signing"));
    }

    #[test]
    fn devcontainer_json_with_setup_has_post_start() {
        let setup = SetupOptions {
            install_claude_code: true,
            install_codex: false,
            install_gh: false,
            ssh_signing: false,
        };
        let json_str = generate_devcontainer_json("test", &setup);
        let json: serde_json::Value = serde_json::from_str(&json_str).unwrap();
        let psc = json["postStartCommand"].as_object().unwrap();
        assert!(psc.contains_key("claude-code"));
    }

    #[test]
    fn devcontainer_json_without_setup_has_no_post_start() {
        let setup = SetupOptions {
            install_claude_code: false,
            install_codex: false,
            install_gh: false,
            ssh_signing: false,
        };
        let json_str = generate_devcontainer_json("test", &setup);
        let json: serde_json::Value = serde_json::from_str(&json_str).unwrap();
        assert!(json.get("postStartCommand").is_none());
    }

    #[test]
    fn post_start_command_claude_is_idempotent() {
        let opts = SetupOptions {
            install_claude_code: true,
            install_codex: false,
            install_gh: false,
            ssh_signing: false,
        };
        let cmd = generate_post_start_command(&opts);
        let s = cmd["claude-code"].as_str().unwrap();
        // Must check before installing (idempotent guard)
        assert!(s.starts_with("command -v claude"));
    }

    #[test]
    fn post_start_command_codex_is_idempotent() {
        let opts = SetupOptions {
            install_claude_code: false,
            install_codex: true,
            install_gh: false,
            ssh_signing: false,
        };
        let cmd = generate_post_start_command(&opts);
        let s = cmd["codex"].as_str().unwrap();
        // Must check before installing (idempotent guard)
        assert!(s.starts_with("command -v codex"));
        // Must also check that npm exists
        assert!(s.contains("command -v npm"));
    }

    #[test]
    fn post_start_command_ssh_overrides_program_and_extracts_key() {
        let opts = SetupOptions {
            install_claude_code: false,
            install_codex: false,
            install_gh: false,
            ssh_signing: true,
        };
        let cmd = generate_post_start_command(&opts);
        let s = cmd["ssh-signing"].as_str().unwrap();
        // Must override gpg.ssh.program to ssh-keygen
        assert!(s.contains("gpg.ssh.program /usr/bin/ssh-keygen"));
        // Must extract signing key to a file
        assert!(s.contains("user.signingkey"));
        assert!(s.contains(".ssh-signing-key.pub"));
    }

    #[test]
    fn devcontainer_json_with_all_setup_has_all_commands() {
        let setup = SetupOptions {
            install_claude_code: true,
            install_codex: true,
            install_gh: true,
            ssh_signing: true,
        };
        let json_str = generate_devcontainer_json("myproject", &setup);
        let json: serde_json::Value = serde_json::from_str(&json_str).unwrap();
        let psc = json["postStartCommand"].as_object().unwrap();
        assert_eq!(psc.len(), 4);
        assert!(psc.contains_key("claude-code"));
        assert!(psc.contains_key("codex"));
        assert!(psc.contains_key("gh"));
        assert!(psc.contains_key("ssh-signing"));
        // Other fields should still be present
        assert_eq!(json["name"], "myproject");
        assert_eq!(json["initializeCommand"], "kap sidecar-init");
    }

    #[test]
    fn devcontainer_json_always_has_remote_env_path() {
        let setup = SetupOptions {
            install_claude_code: false,
            install_codex: false,
            install_gh: false,
            ssh_signing: false,
        };
        let json_str = generate_devcontainer_json("test", &setup);
        let json: serde_json::Value = serde_json::from_str(&json_str).unwrap();
        // remoteEnv.PATH is always present (not conditional on CLI tools)
        assert!(
            json["remoteEnv"]["PATH"]
                .as_str()
                .unwrap()
                .contains("/opt/kap/bin")
        );
    }

    #[test]
    fn devcontainer_json_is_valid_json_with_setup() {
        // Ensure the string-template approach produces valid JSON for all combos
        for (claude, codex, gh, ssh) in [
            (false, false, false, false),
            (true, false, false, false),
            (false, true, false, false),
            (false, false, true, false),
            (false, false, false, true),
            (true, true, true, true),
        ] {
            let setup = SetupOptions {
                install_claude_code: claude,
                install_codex: codex,
                install_gh: gh,
                ssh_signing: ssh,
            };
            let json_str = generate_devcontainer_json("test", &setup);
            let parsed: Result<serde_json::Value, _> = serde_json::from_str(&json_str);
            assert!(
                parsed.is_ok(),
                "Invalid JSON for claude={claude}, codex={codex}, gh={gh}, ssh={ssh}: {json_str}"
            );
        }
    }

    #[test]
    fn is_1password_ssh_program_matches() {
        assert!(is_1password_ssh_program(
            "/Applications/1Password.app/Contents/MacOS/op-ssh-sign"
        ));
        assert!(is_1password_ssh_program("op-ssh-sign"));
        assert!(!is_1password_ssh_program("/usr/bin/ssh-keygen"));
        assert!(!is_1password_ssh_program("secretive"));
        assert!(!is_1password_ssh_program("gpg"));
    }

    #[test]
    fn existing_project_with_post_start_command_not_overwritten() {
        let dir = tempdir("setup-existing-poststartcmd");
        let dc = dir.join(".devcontainer");
        fs::create_dir_all(&dc).unwrap();
        fs::write(
            dc.join("devcontainer.json"),
            r#"{"service": "app", "postStartCommand": "echo existing"}"#,
        )
        .unwrap();

        // --yes enables claude code, but postStartCommand already set
        run(dir.to_str().unwrap(), true, false).unwrap();

        let updated: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(dc.join("devcontainer.json")).unwrap())
                .unwrap();
        // Should preserve existing postStartCommand
        assert_eq!(updated["postStartCommand"], "echo existing");

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn has_kap_sidecar_init_string() {
        assert!(has_kap_sidecar_init(&serde_json::json!("kap sidecar-init")));
        assert!(has_kap_sidecar_init(&serde_json::json!(
            "kap sidecar-init && echo done"
        )));
        assert!(!has_kap_sidecar_init(&serde_json::json!("echo hello")));
    }

    #[test]
    fn has_kap_sidecar_init_array() {
        assert!(has_kap_sidecar_init(&serde_json::json!([
            "kap",
            "sidecar-init"
        ])));
        assert!(!has_kap_sidecar_init(&serde_json::json!(["echo", "hello"])));
    }

    #[test]
    fn has_kap_sidecar_init_object() {
        assert!(has_kap_sidecar_init(
            &serde_json::json!({"kap": "kap sidecar-init", "other": "echo hi"})
        ));
        assert!(!has_kap_sidecar_init(
            &serde_json::json!({"setup": "echo hi"})
        ));
    }

    #[test]
    fn existing_project_init_command_already_correct_no_warning() {
        let dir = tempdir("init-cmd-already-correct");
        let dc = dir.join(".devcontainer");
        fs::create_dir_all(&dc).unwrap();
        fs::write(
            dc.join("devcontainer.json"),
            r#"{"service": "app", "initializeCommand": "kap sidecar-init"}"#,
        )
        .unwrap();

        run(dir.to_str().unwrap(), true, false).unwrap();

        let updated: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(dc.join("devcontainer.json")).unwrap())
                .unwrap();
        assert_eq!(updated["initializeCommand"], "kap sidecar-init");

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn existing_project_post_start_object_auto_merged() {
        let dir = tempdir("poststart-obj-merge");
        let dc = dir.join(".devcontainer");
        fs::create_dir_all(&dc).unwrap();
        fs::write(
            dc.join("devcontainer.json"),
            r#"{"service": "app", "postStartCommand": {"my-setup": "echo custom"}}"#,
        )
        .unwrap();

        // --yes enables setup options, which should be merged into existing object
        run(dir.to_str().unwrap(), true, false).unwrap();

        let updated: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(dc.join("devcontainer.json")).unwrap())
                .unwrap();
        let psc = updated["postStartCommand"].as_object().unwrap();
        // Existing key preserved
        assert_eq!(psc["my-setup"], "echo custom");
        // Kap keys merged in
        assert!(psc.contains_key("claude-code"));

        fs::remove_dir_all(&dir).unwrap();
    }

    fn tempdir(suffix: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("kap-test-{}-{suffix}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }
}
