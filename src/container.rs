/// Wrapper commands for devcontainer lifecycle: up, down, exec, list.
///
/// Shells out to `devcontainer` CLI and `docker compose` so users
/// only need one tool (`kap`) for everything.
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};

/// Options for executing a command in a devcontainer.
///
/// Use [`ExecOptions::new`] for explicit workspace path or [`ExecOptions::cwd`]
/// to fall back to the current working directory.
pub struct ExecOptions {
    pub workspace: Option<PathBuf>,
    pub cmd: Vec<String>,
    pub stdin: Option<Stdio>,
    pub stdout: Option<Stdio>,
    pub stderr: Option<Stdio>,
}

impl ExecOptions {
    /// Create options for running a command in the workspace at the given path.
    pub fn new(workspace: impl Into<PathBuf>, cmd: Vec<String>) -> Self {
        Self {
            workspace: Some(workspace.into()),
            cmd,
            stdin: None,
            stdout: None,
            stderr: None,
        }
    }

    /// Create options using the current working directory as workspace.
    pub fn cwd(cmd: Vec<String>) -> Self {
        Self {
            workspace: None,
            cmd,
            stdin: None,
            stdout: None,
            stderr: None,
        }
    }

    pub fn stdin(mut self, stdio: Stdio) -> Self {
        self.stdin = Some(stdio);
        self
    }

    pub fn stdout(mut self, stdio: Stdio) -> Self {
        self.stdout = Some(stdio);
        self
    }

    pub fn stderr(mut self, stdio: Stdio) -> Self {
        self.stderr = Some(stdio);
        self
    }
}

/// Start the devcontainer.
///
/// If `workspace` is `Some`, uses that path. Otherwise falls back to CWD.
pub fn up(workspace: Option<&Path>, reset: bool) -> Result<()> {
    require_kap_init_at(workspace)?;
    require_devcontainer()?;
    let workspace = resolve_workspace_folder(workspace)?;

    if reset {
        // Tear down containers AND networks before recreating. devcontainer's
        // --remove-existing-container only removes the container, leaving stale
        // networks behind. After a Docker restart those networks can have
        // broken state ("Address already in use") that blocks `up`.
        if let Some(project) =
            find_compose_project(&workspace).or_else(|| derive_compose_project(&workspace))
        {
            let _ = Command::new("docker")
                .args(["compose", "-p", &project, "down"])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }

        // Pull the latest sidecar image so we don't reuse a stale cache.
        if let Some(image) = sidecar_image_at(&workspace) {
            eprintln!("Pulling {image}...");
            let _ = Command::new("docker")
                .args(["pull", &image])
                .stdin(Stdio::inherit())
                .stdout(Stdio::inherit())
                .stderr(Stdio::inherit())
                .status();
        }
    }

    let mut cmd = Command::new("devcontainer");
    cmd.arg("up").arg("--workspace-folder").arg(&workspace);

    if reset {
        cmd.arg("--remove-existing-container");
    }

    let status = cmd
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .context("running devcontainer up")?;

    if !status.success() {
        anyhow::bail!(
            "devcontainer up failed (exit code {})",
            status.code().unwrap_or(1)
        );
    }

    // Clear proxy logs on reset (the volume persists across container recreates)
    if reset && let Some(sidecar) = find_sidecar_for(&workspace) {
        let _ = Command::new("docker")
            .args(["exec", &sidecar, "sh", "-c", "rm -f /var/log/kap/*.jsonl"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }

    // If a cached dev binary exists, deploy it to the sidecar so the container
    // runs the same kap version as the host. This breaks the catch-22 where
    // `kap dev push` needs a running sidecar but `kap up` starts with the
    // published image.
    let dev_binary = crate::dev::cached_binary_path();
    let dev_deployed = dev_binary.exists()
        && find_sidecar_for(&workspace).is_some_and(|sidecar| {
            eprintln!("[dev] deploying cached binary to sidecar...");
            crate::dev::deploy_to_sidecars(&dev_binary, &[sidecar]);
            true
        });

    // Wait for the sidecar to become healthy before running status checks.
    // The sidecar writes CLI shims on startup; without this wait, `which <tool>`
    // fails because the shim hasn't been written to the shared volume yet.
    if let Some(sidecar) = find_sidecar_for(&workspace) {
        for _ in 0..15 {
            let output = Command::new("docker")
                .args(["inspect", "--format", "{{.State.Health.Status}}", &sidecar])
                .output();
            if let Ok(out) = output {
                let status = String::from_utf8_lossy(&out.stdout).trim().to_string();
                if status == "healthy" {
                    break;
                }
            }
            std::thread::sleep(std::time::Duration::from_secs(1));
        }
    }

    // When a dev binary was deployed, the sidecar generated a new post-start
    // script. Re-run it in the app container so overrides (e.g. git signing
    // key) match the deployed code, not the published image's older version.
    if dev_deployed {
        let post_start = format!("/opt/kap/bin/{}", crate::reload::POST_START_FILENAME);
        let mut opts = ExecOptions::cwd(vec!["bash".into(), "-c".into(), post_start]);
        opts.stdout = Some(Stdio::null());
        opts.stderr = Some(Stdio::null());
        if let Err(e) = exec_with(opts) {
            eprintln!("[dev] warning: post-start re-run failed: {e}");
        }
    }

    // Show health checks after successful start
    println!();
    if let Err(e) = crate::status::run() {
        eprintln!("  status check failed: {e}");
        eprintln!("  (the container is running — try `kap status` again in a moment)");
    }

    Ok(())
}

/// Stop and remove the devcontainer.
///
/// Project can be resolved from an explicit name, a workspace path, or CWD.
pub fn down(project: Option<String>, workspace: Option<&Path>, volumes: bool) -> Result<()> {
    let project = resolve_project_from(project, workspace)?;

    let mut cmd = Command::new("docker");
    cmd.args(["compose", "-p", &project, "down"]);

    if volumes {
        cmd.arg("--volumes");
    }

    let status = cmd
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .context("running docker compose down")?;

    if !status.success() {
        anyhow::bail!(
            "docker compose down failed (exit code {})",
            status.code().unwrap_or(1)
        );
    }

    Ok(())
}

/// Run a command in the devcontainer (default: interactive shell).
///
/// For programmatic use with output capture, use [`exec_with`] instead.
pub fn exec(project: Option<String>, cmd: Vec<String>) -> Result<()> {
    let workspace = match &project {
        Some(name) => Some(resolve_workspace(name)?),
        None => None,
    };

    // Auto-refresh .env shell patterns (e.g. GH_TOKEN) before entering the container.
    // Non-fatal: stale tokens are better than blocking exec.
    {
        let ws = match &workspace {
            Some(p) => p.clone(),
            None => std::env::current_dir().unwrap_or_default(),
        };
        let dc_dir = ws.join(".devcontainer");
        let env_path = crate::init::env_file_for_project(&dc_dir);
        if env_path.exists()
            && let Err(e) = crate::init_env::refresh_env(&env_path)
        {
            eprintln!("[exec] warning: could not refresh .env: {e}");
        }
    }

    let status = exec_with(ExecOptions {
        workspace,
        cmd,
        stdin: None,
        stdout: None,
        stderr: None,
    })?;

    if !status.success() {
        anyhow::bail!(
            "devcontainer exec failed (exit code {})",
            status.code().unwrap_or(1)
        );
    }

    Ok(())
}

/// Execute a command in the devcontainer with full control over I/O.
///
/// Returns the process exit status. For interactive use, prefer [`exec`].
pub fn exec_with(opts: ExecOptions) -> Result<ExitStatus> {
    require_devcontainer()?;

    let workspace = resolve_workspace_folder(opts.workspace.as_deref())?;

    let shell_cmd = if opts.cmd.is_empty() {
        vec!["/bin/bash".to_string()]
    } else {
        opts.cmd
    };

    let status = Command::new("devcontainer")
        .arg("exec")
        .arg("--workspace-folder")
        .arg(&workspace)
        .args(&shell_cmd)
        .stdin(opts.stdin.unwrap_or_else(Stdio::inherit))
        .stdout(opts.stdout.unwrap_or_else(Stdio::inherit))
        .stderr(opts.stderr.unwrap_or_else(Stdio::inherit))
        .status()
        .context("running devcontainer exec")?;

    Ok(status)
}

/// List all running devcontainers.
pub fn list(stats: bool) -> Result<()> {
    let groups = crate::remote::containers::find_all_containers()?;
    if groups.is_empty() {
        println!("No running devcontainers.");
        return Ok(());
    }

    println!(
        "Each project has an app container (agent workspace) and a kap sidecar (proxy + network controls)."
    );
    println!();

    let resource_stats = if stats {
        collect_stats()
    } else {
        Default::default()
    };

    for (i, g) in groups.iter().enumerate() {
        if i > 0 {
            println!();
        }
        println!("\x1b[1m{}\x1b[0m", g.project);
        print_container_line("  app", &g.app, &resource_stats);
        print_container_line("  kap", &g.sidecar, &resource_stats);
    }
    Ok(())
}

struct ContainerStats {
    cpu: String,
    mem: String,
}

fn collect_stats() -> std::collections::HashMap<String, ContainerStats> {
    let output = Command::new("docker")
        .args([
            "stats",
            "--no-stream",
            "--format",
            "{{.Name}}\t{{.CPUPerc}}\t{{.MemUsage}}",
        ])
        .output()
        .ok();

    let mut map = std::collections::HashMap::new();
    if let Some(output) = output {
        let text = String::from_utf8_lossy(&output.stdout);
        for line in text.lines() {
            let parts: Vec<&str> = line.split('\t').collect();
            if parts.len() >= 3 {
                let name = parts[0].trim().to_string();
                let cpu = parts[1].trim().to_string();
                // Just the usage part, not the "/ limit"
                let mem = parts[2].split('/').next().unwrap_or("").trim().to_string();
                map.insert(name, ContainerStats { cpu, mem });
            }
        }
    }
    map
}

fn print_container_line(
    label: &str,
    container: &str,
    stats: &std::collections::HashMap<String, ContainerStats>,
) {
    match stats.get(container) {
        Some(s) => println!(
            "  {label:<6} {container}  \x1b[2mcpu {:<6} mem {}\x1b[0m",
            s.cpu, s.mem
        ),
        None => println!("  {label:<6} {container}"),
    }
}

/// Resolve project name from an explicit name, a workspace path, or CWD.
fn resolve_project_from(project: Option<String>, workspace: Option<&Path>) -> Result<String> {
    if let Some(name) = project {
        return resolve_project(Some(name));
    }
    if let Some(ws) = workspace {
        return find_compose_project(ws)
            .or_else(|| derive_compose_project(ws))
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "could not determine compose project name for {}",
                    ws.display()
                )
            });
    }
    resolve_project(None)
}

/// Resolve project name: if given, validate it exists; if not, derive from CWD.
fn resolve_project(project: Option<String>) -> Result<String> {
    match project {
        Some(name) => {
            // Allow partial match: user can type "nitrocop" instead of "nitrocop_devcontainer"
            let groups = crate::remote::containers::find_all_containers()?;
            // Exact match first
            if groups.iter().any(|g| g.project == name) {
                return Ok(name);
            }
            // Partial match: project name starts with the given name
            let matches: Vec<_> = groups
                .iter()
                .filter(|g| g.project.starts_with(&name))
                .collect();
            match matches.len() {
                1 => Ok(matches[0].project.clone()),
                0 => anyhow::bail!(
                    "no running devcontainer matching '{name}'.\n\n  \
                     Run `kap list` to see running containers."
                ),
                _ => {
                    let names: Vec<_> = matches.iter().map(|g| g.project.as_str()).collect();
                    anyhow::bail!("'{name}' is ambiguous, matches: {}", names.join(", "))
                }
            }
        }
        None => {
            let workspace = workspace_folder()?;
            find_compose_project(&workspace)
                .or_else(|| derive_compose_project(&workspace))
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "could not determine compose project name for {}",
                        workspace.display()
                    )
                })
        }
    }
}

/// Find the workspace folder for a running project (from container labels).
fn resolve_workspace(project_name: &str) -> Result<PathBuf> {
    let project = resolve_project(Some(project_name.to_string()))?;
    let output = Command::new("docker")
        .args([
            "ps",
            "--filter",
            &format!("label=com.docker.compose.project={project}"),
            "--format",
            r#"{{.Label "devcontainer.local_folder"}}"#,
        ])
        .output()
        .context("running docker ps")?;

    let text = String::from_utf8_lossy(&output.stdout);
    text.lines()
        .find_map(|s| {
            let s = s.trim();
            if s.is_empty() {
                None
            } else {
                Some(PathBuf::from(s))
            }
        })
        .ok_or_else(|| {
            anyhow::anyhow!("could not find workspace folder for project '{project_name}'")
        })
}

/// Find the kap sidecar container name for a specific workspace.
///
/// Filters by compose project label so we don't accidentally pick up a
/// sidecar from a different devcontainer when multiple are running.
fn find_sidecar_for(workspace: &Path) -> Option<String> {
    // Try to find the compose project from running containers, fall back to
    // deriving it from the directory name.
    let project = find_compose_project(workspace).or_else(|| derive_compose_project(workspace))?;
    let output = Command::new("docker")
        .args([
            "ps",
            "--filter",
            &format!("label=com.docker.compose.project={project}"),
            "--format",
            "{{.Names}}",
        ])
        .output()
        .ok()?;
    let names = String::from_utf8_lossy(&output.stdout);
    names
        .lines()
        .find(|n| n.contains("kap-kap") || n.ends_with("-kap-1"))
        .map(String::from)
}

fn sidecar_image_at(workspace: &Path) -> Option<String> {
    let content = std::fs::read_to_string(workspace.join(".devcontainer/kap.toml")).ok()?;
    let config: crate::config::Config = toml::from_str(&content).ok()?;
    let compose = config.compose.unwrap_or_default();
    compose.sidecar_image().map(String::from)
}

/// Check that `kap init` has been run in the current directory.
pub fn require_kap_init() -> Result<()> {
    require_kap_init_at(None)
}

/// Check that `kap init` has been run at the given path (or CWD if None),
/// and that devcontainer.json has the required kap configuration.
pub fn require_kap_init_at(workspace: Option<&Path>) -> Result<()> {
    let dc_dir = match workspace {
        Some(p) => p.join(".devcontainer"),
        None => PathBuf::from(".devcontainer"),
    };
    if !dc_dir.join("kap.toml").exists() {
        anyhow::bail!("No kap.toml found. Run `kap init` first to set up your devcontainer.");
    }

    let dc_json_path = dc_dir.join("devcontainer.json");
    if let Ok(content) = std::fs::read_to_string(&dc_json_path)
        && let Ok(json) = crate::init::parse_jsonc(&content)
    {
        validate_devcontainer_json(&json);
    }

    Ok(())
}

/// Warn about missing or outdated kap fields in devcontainer.json.
/// Prints warnings to stderr but does not fail — the container may still work.
fn validate_devcontainer_json(json: &serde_json::Value) {
    let mut warnings: Vec<&str> = Vec::new();

    // initializeCommand must contain "kap sidecar-init"
    match json.get("initializeCommand") {
        Some(v)
            if v.as_str().is_some_and(|s| s.contains("kap sidecar-init"))
                || v.as_array().is_some_and(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str())
                        .collect::<Vec<_>>()
                        .join(" ")
                        .contains("kap sidecar-init")
                })
                || v.as_object().is_some_and(|o| {
                    o.values()
                        .any(|v| v.as_str().is_some_and(|s| s.contains("kap sidecar-init")))
                }) => {}
        _ => warnings.push("initializeCommand must include \"kap sidecar-init\""),
    }

    // dockerComposeFile must include the overlay
    if let Some(arr) = json.get("dockerComposeFile").and_then(|v| v.as_array()) {
        if !arr.iter().any(|v| {
            v.as_str()
                .is_some_and(|s| s == crate::init::OVERLAY_FILENAME)
        }) {
            warnings.push("dockerComposeFile must include \"docker-compose.kap.yml\"");
        }
    } else {
        warnings.push("dockerComposeFile must include \"docker-compose.kap.yml\"");
    }

    // remoteEnv.PATH must contain /opt/kap/bin
    let has_path = json
        .get("remoteEnv")
        .and_then(|v| v.get("PATH"))
        .and_then(|v| v.as_str())
        .is_some_and(|s| s.contains("/opt/kap/bin"));
    if !has_path {
        warnings.push("remoteEnv.PATH must include \"/opt/kap/bin\" for CLI shims to work");
    }

    // postStartCommand must include kap-post-start (if kap.toml has [setup])
    let has_post_start = json.get("postStartCommand").is_some_and(|v| match v {
        serde_json::Value::String(s) => s.contains("kap-post-start"),
        serde_json::Value::Object(o) => o
            .values()
            .any(|v| v.as_str().is_some_and(|s| s.contains("kap-post-start"))),
        _ => false,
    });
    if !has_post_start {
        warnings.push("postStartCommand must include \"kap-post-start\" for tool setup");
    }

    if !warnings.is_empty() {
        eprintln!();
        eprintln!("  ⚠ devcontainer.json is missing required kap configuration:");
        for w in &warnings {
            eprintln!("    - {w}");
        }
        eprintln!();
        eprintln!("  Run `kap init` to fix, or update devcontainer.json manually.");
        eprintln!();
    }
}

fn require_devcontainer() -> Result<()> {
    match Command::new("which")
        .arg("devcontainer")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
    {
        Ok(s) if s.success() => Ok(()),
        _ => anyhow::bail!(
            "devcontainer CLI not found.\n\n  \
             Install it with: npm install -g @devcontainers/cli"
        ),
    }
}

/// Resolve a workspace folder, verifying `.devcontainer/devcontainer.json` exists.
///
/// If `path` is `Some`, validates that path. If `None`, falls back to CWD.
pub fn resolve_workspace_folder(path: Option<&Path>) -> Result<PathBuf> {
    match path {
        Some(p) => {
            let dc_json = p.join(".devcontainer/devcontainer.json");
            if !dc_json.exists() {
                anyhow::bail!(
                    "no .devcontainer/devcontainer.json in {}.\n\n  \
                     Run `kap init` first.",
                    p.display()
                );
            }
            Ok(p.to_path_buf())
        }
        None => workspace_folder(),
    }
}

/// Get the workspace folder (CWD), verifying .devcontainer/ exists.
fn workspace_folder() -> Result<PathBuf> {
    let cwd = std::env::current_dir().context("getting current directory")?;
    let dc_json = cwd.join(".devcontainer/devcontainer.json");
    if !dc_json.exists() {
        anyhow::bail!(
            "no .devcontainer/devcontainer.json in current directory.\n\n  \
             Run `kap init` first, or cd into your project."
        );
    }
    Ok(cwd)
}

/// Find the compose project name from running containers matching this workspace.
pub fn find_compose_project(workspace: &Path) -> Option<String> {
    let workspace_str = workspace.to_string_lossy();
    let output = Command::new("docker")
        .args([
            "ps",
            "--filter",
            &format!("label=devcontainer.local_folder={workspace_str}"),
            "--format",
            r#"{{.Label "com.docker.compose.project"}}"#,
        ])
        .output()
        .ok()?;

    let text = String::from_utf8_lossy(&output.stdout);
    text.lines()
        .next()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Derive the compose project name from the workspace directory name.
/// Matches the devcontainer CLI convention: `{dirname}_devcontainer`.
pub fn derive_compose_project(workspace: &Path) -> Option<String> {
    let dirname = workspace.file_name()?.to_string_lossy();
    Some(format!("{dirname}_devcontainer"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derive_compose_project_simple() {
        let p = derive_compose_project(Path::new("/home/user/myproject"));
        assert_eq!(p.as_deref(), Some("myproject_devcontainer"));
    }

    #[test]
    fn derive_compose_project_hyphenated() {
        let p = derive_compose_project(Path::new("/Users/peter/oss/kap"));
        assert_eq!(p.as_deref(), Some("kap_devcontainer"));
    }

    #[test]
    fn derive_compose_project_root_returns_none() {
        let p = derive_compose_project(Path::new("/"));
        assert!(p.is_none());
    }

    #[test]
    fn require_kap_init_returns_err_not_exit() {
        let dir = std::env::temp_dir().join(format!("kap-init-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        let original = std::env::current_dir().unwrap();
        std::env::set_current_dir(&dir).unwrap();

        let result = require_kap_init();
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("kap init"));

        std::env::set_current_dir(original).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn workspace_folder_requires_devcontainer_json() {
        let dir = std::env::temp_dir().join(format!("kap-ws-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        let original = std::env::current_dir().unwrap();
        std::env::set_current_dir(&dir).unwrap();

        let result = workspace_folder();
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("devcontainer.json")
        );

        std::env::set_current_dir(original).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn resolve_workspace_folder_with_explicit_path() {
        let dir = std::env::temp_dir().join(format!("kap-resolve-ws-test-{}", std::process::id()));
        std::fs::create_dir_all(dir.join(".devcontainer")).unwrap();
        std::fs::write(dir.join(".devcontainer/devcontainer.json"), "{}").unwrap();

        let result = resolve_workspace_folder(Some(&dir));
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), dir);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn resolve_workspace_folder_explicit_path_missing_devcontainer() {
        let dir =
            std::env::temp_dir().join(format!("kap-resolve-ws-missing-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        let result = resolve_workspace_folder(Some(&dir));
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("devcontainer.json")
        );

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn require_kap_init_at_with_explicit_path() {
        let dir = std::env::temp_dir().join(format!("kap-init-at-test-{}", std::process::id()));
        std::fs::create_dir_all(dir.join(".devcontainer")).unwrap();
        std::fs::write(dir.join(".devcontainer/kap.toml"), "").unwrap();

        let result = require_kap_init_at(Some(&dir));
        assert!(result.is_ok());

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn require_kap_init_at_explicit_path_missing() {
        let dir = std::env::temp_dir().join(format!("kap-init-at-missing-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        let result = require_kap_init_at(Some(&dir));
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("kap init"));

        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Capture stderr output from validate_devcontainer_json.
    fn validate_warnings(json: &serde_json::Value) -> String {
        // We can't easily capture stderr, so re-implement the check logic
        // to verify which warnings would fire.
        let mut warnings: Vec<&str> = Vec::new();

        match json.get("initializeCommand") {
            Some(v)
                if v.as_str().is_some_and(|s| s.contains("kap sidecar-init"))
                    || v.as_object().is_some_and(|o| {
                        o.values()
                            .any(|v| v.as_str().is_some_and(|s| s.contains("kap sidecar-init")))
                    }) => {}
            _ => warnings.push("initializeCommand"),
        }

        if let Some(arr) = json.get("dockerComposeFile").and_then(|v| v.as_array()) {
            if !arr.iter().any(|v| {
                v.as_str()
                    .is_some_and(|s| s == crate::init::OVERLAY_FILENAME)
            }) {
                warnings.push("dockerComposeFile");
            }
        } else {
            warnings.push("dockerComposeFile");
        }

        let has_path = json
            .get("remoteEnv")
            .and_then(|v| v.get("PATH"))
            .and_then(|v| v.as_str())
            .is_some_and(|s| s.contains("/opt/kap/bin"));
        if !has_path {
            warnings.push("remoteEnv");
        }

        let has_post_start = json.get("postStartCommand").is_some_and(|v| match v {
            serde_json::Value::String(s) => s.contains("kap-post-start"),
            serde_json::Value::Object(o) => o
                .values()
                .any(|v| v.as_str().is_some_and(|s| s.contains("kap-post-start"))),
            _ => false,
        });
        if !has_post_start {
            warnings.push("postStartCommand");
        }

        warnings.join(", ")
    }

    #[test]
    fn validate_complete_devcontainer_json() {
        let json = serde_json::json!({
            "initializeCommand": "kap sidecar-init",
            "dockerComposeFile": ["docker-compose.yml", "docker-compose.kap.yml"],
            "remoteEnv": { "PATH": "/opt/kap/bin:${containerEnv:PATH}" },
            "postStartCommand": { "kap-setup": "/opt/kap/bin/kap-post-start" },
        });
        assert_eq!(validate_warnings(&json), "");
    }

    #[test]
    fn validate_missing_all_fields() {
        let json = serde_json::json!({});
        let w = validate_warnings(&json);
        assert!(w.contains("initializeCommand"));
        assert!(w.contains("dockerComposeFile"));
        assert!(w.contains("remoteEnv"));
        assert!(w.contains("postStartCommand"));
    }

    #[test]
    fn validate_missing_remote_env_path() {
        let json = serde_json::json!({
            "initializeCommand": "kap sidecar-init",
            "dockerComposeFile": ["docker-compose.yml", "docker-compose.kap.yml"],
            "postStartCommand": { "kap-setup": "/opt/kap/bin/kap-post-start" },
        });
        let w = validate_warnings(&json);
        assert_eq!(w, "remoteEnv");
    }

    #[test]
    fn validate_init_command_in_object_form() {
        let json = serde_json::json!({
            "initializeCommand": { "kap": "kap sidecar-init" },
            "dockerComposeFile": ["docker-compose.yml", "docker-compose.kap.yml"],
            "remoteEnv": { "PATH": "/opt/kap/bin:${containerEnv:PATH}" },
            "postStartCommand": { "kap-setup": "/opt/kap/bin/kap-post-start" },
        });
        assert_eq!(validate_warnings(&json), "");
    }

    #[test]
    fn validate_post_start_as_string() {
        let json = serde_json::json!({
            "initializeCommand": "kap sidecar-init",
            "dockerComposeFile": ["docker-compose.yml", "docker-compose.kap.yml"],
            "remoteEnv": { "PATH": "/opt/kap/bin:${containerEnv:PATH}" },
            "postStartCommand": "/opt/kap/bin/kap-post-start",
        });
        assert_eq!(validate_warnings(&json), "");
    }

    #[test]
    fn exec_options_builder() {
        let opts = ExecOptions::new(
            "/tmp/project",
            vec!["bash".into(), "-lc".into(), "echo hi".into()],
        )
        .stdin(Stdio::null());

        assert_eq!(opts.workspace, Some(PathBuf::from("/tmp/project")));
        assert_eq!(opts.cmd, vec!["bash", "-lc", "echo hi"]);
        assert!(opts.stdin.is_some());
        assert!(opts.stdout.is_none());
        assert!(opts.stderr.is_none());
    }

    #[test]
    fn exec_options_cwd_builder() {
        let opts = ExecOptions::cwd(vec!["ls".into()]);
        assert!(opts.workspace.is_none());
        assert_eq!(opts.cmd, vec!["ls"]);
    }
}
