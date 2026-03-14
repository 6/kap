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

    // On --reset, pull the latest sidecar image so we don't reuse a stale cache.
    if reset && let Some(image) = sidecar_image_at(&workspace) {
        eprintln!("Pulling {image}...");
        let _ = Command::new("docker")
            .args(["pull", &image])
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .status();
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
    if reset && let Some(sidecar) = find_sidecar() {
        let _ = Command::new("docker")
            .args(["exec", &sidecar, "sh", "-c", "rm -f /var/log/kap/*.jsonl"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
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

/// Find the kap sidecar container name from running containers.
fn find_sidecar() -> Option<String> {
    let output = Command::new("docker")
        .args(["ps", "--format", "{{.Names}}"])
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

/// Check that `kap init` has been run at the given path (or CWD if None).
pub fn require_kap_init_at(workspace: Option<&Path>) -> Result<()> {
    let kap_toml = match workspace {
        Some(p) => p.join(".devcontainer/kap.toml"),
        None => PathBuf::from(".devcontainer/kap.toml"),
    };
    if !kap_toml.exists() {
        anyhow::bail!("No kap.toml found. Run `kap init` first to set up your devcontainer.");
    }
    Ok(())
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
        let dir =
            std::env::temp_dir().join(format!("kap-resolve-ws-test-{}", std::process::id()));
        std::fs::create_dir_all(dir.join(".devcontainer")).unwrap();
        std::fs::write(dir.join(".devcontainer/devcontainer.json"), "{}").unwrap();

        let result = resolve_workspace_folder(Some(&dir));
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), dir);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn resolve_workspace_folder_explicit_path_missing_devcontainer() {
        let dir = std::env::temp_dir()
            .join(format!("kap-resolve-ws-missing-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        let result = resolve_workspace_folder(Some(&dir));
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("devcontainer.json"));

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn require_kap_init_at_with_explicit_path() {
        let dir =
            std::env::temp_dir().join(format!("kap-init-at-test-{}", std::process::id()));
        std::fs::create_dir_all(dir.join(".devcontainer")).unwrap();
        std::fs::write(dir.join(".devcontainer/kap.toml"), "").unwrap();

        let result = require_kap_init_at(Some(&dir));
        assert!(result.is_ok());

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn require_kap_init_at_explicit_path_missing() {
        let dir = std::env::temp_dir()
            .join(format!("kap-init-at-missing-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        let result = require_kap_init_at(Some(&dir));
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("kap init"));

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn exec_options_builder() {
        let opts = ExecOptions::new("/tmp/project", vec!["bash".into(), "-lc".into(), "echo hi".into()])
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
