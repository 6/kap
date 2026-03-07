/// Wrapper commands for devcontainer lifecycle: up, down, exec, list.
///
/// Shells out to `devcontainer` CLI and `docker compose` so users
/// only need one tool (`kap`) for everything.
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Start the devcontainer.
pub fn up(reset: bool) -> Result<()> {
    require_kap_init()?;
    require_devcontainer()?;
    let workspace = workspace_folder()?;

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
        std::process::exit(status.code().unwrap_or(1));
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
pub fn down(project: Option<String>, volumes: bool) -> Result<()> {
    let project = resolve_project(project)?;

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
        std::process::exit(status.code().unwrap_or(1));
    }

    Ok(())
}

/// Run a command in the devcontainer (default: interactive shell).
pub fn exec(project: Option<String>, cmd: Vec<String>) -> Result<()> {
    require_devcontainer()?;

    let workspace = match &project {
        Some(name) => resolve_workspace(name)?,
        None => workspace_folder()?,
    };

    let shell_cmd = if cmd.is_empty() {
        vec!["/bin/bash".to_string()]
    } else {
        cmd
    };

    let mut child = Command::new("devcontainer");
    child
        .arg("exec")
        .arg("--workspace-folder")
        .arg(&workspace)
        .args(&shell_cmd);

    let status = child
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .context("running devcontainer exec")?;

    if !status.success() {
        std::process::exit(status.code().unwrap_or(1));
    }

    Ok(())
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

/// Check that `devcontainer` CLI is installed.
fn require_kap_init() -> Result<()> {
    let path = Path::new(".devcontainer/kap.toml");
    if !path.exists() {
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
pub(crate) fn find_compose_project(workspace: &Path) -> Option<String> {
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
fn derive_compose_project(workspace: &Path) -> Option<String> {
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
}
