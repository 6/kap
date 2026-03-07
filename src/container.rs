/// Wrapper commands for devcontainer lifecycle: up, down, exec.
///
/// Shells out to `devcontainer` CLI and `docker compose` so users
/// only need one tool (`devg`) for everything.
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Start the devcontainer.
pub fn up(reset: bool) -> Result<()> {
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
        eprintln!("  (the container is running — try `devg status` again in a moment)");
    }

    Ok(())
}

/// Stop and remove the devcontainer.
pub fn down(volumes: bool) -> Result<()> {
    let workspace = workspace_folder()?;
    let project = find_compose_project(&workspace)
        .or_else(|| derive_compose_project(&workspace))
        .ok_or_else(|| {
            anyhow::anyhow!(
                "could not determine compose project name for {}",
                workspace.display()
            )
        })?;

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
pub fn exec(cmd: Vec<String>) -> Result<()> {
    require_devcontainer()?;
    let workspace = workspace_folder()?;

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

/// Check that `devcontainer` CLI is installed.
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
             Run `devg init` first, or cd into your project."
        );
    }
    Ok(cwd)
}

/// Find the compose project name from running containers matching this workspace.
fn find_compose_project(workspace: &Path) -> Option<String> {
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
        let p = derive_compose_project(Path::new("/Users/peter/oss/devcontainer-guard"));
        assert_eq!(p.as_deref(), Some("devcontainer-guard_devcontainer"));
    }

    #[test]
    fn derive_compose_project_root_returns_none() {
        let p = derive_compose_project(Path::new("/"));
        // Root has no file_name
        assert!(p.is_none());
    }

    #[test]
    fn workspace_folder_requires_devcontainer_json() {
        let dir = std::env::temp_dir().join(format!("devg-ws-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        // Save and restore CWD
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
