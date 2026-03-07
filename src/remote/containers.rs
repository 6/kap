use anyhow::{Context, Result};
use std::process::Command;

const SANDBOX_NETWORK: &str = "devg_sandbox";

/// Find the app and sidecar containers on the devg_sandbox network.
pub fn find_containers() -> Result<(String, String)> {
    let output = Command::new("docker")
        .args(["ps", "--format", "{{.Names}}"])
        .output()
        .context("running docker ps")?;

    let names = String::from_utf8_lossy(&output.stdout);
    let mut app = None;
    let mut sidecar = None;

    for name in names.lines() {
        let name = name.trim();
        if name.is_empty() {
            continue;
        }
        let inspect = Command::new("docker")
            .args([
                "inspect",
                "--format",
                "{{json .NetworkSettings.Networks}}",
                name,
            ])
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .unwrap_or_default();
        if !inspect.contains(SANDBOX_NETWORK) {
            continue;
        }
        if name.contains("devg-devg") || name.ends_with("-devg-1") {
            sidecar = Some(name.to_string());
        } else {
            app = Some(name.to_string());
        }
    }

    match (app, sidecar) {
        (Some(a), Some(s)) => Ok((a, s)),
        _ => anyhow::bail!(
            "no running devcontainer found with devg networking.\n\n  \
             Start it with: devcontainer up"
        ),
    }
}

/// Run a command in a container and return stdout (trimmed).
pub fn exec_in(container: &str, cmd: &[&str]) -> Option<String> {
    let output = Command::new("docker")
        .arg("exec")
        .arg(container)
        .args(cmd)
        .output()
        .ok()?;
    if output.status.success() {
        Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
    } else {
        None
    }
}

/// Run a command in a container and return the exit code.
pub fn exec_exit_code(container: &str, cmd: &[&str]) -> i32 {
    Command::new("docker")
        .arg("exec")
        .arg(container)
        .args(cmd)
        .output()
        .ok()
        .and_then(|o| o.status.code())
        .unwrap_or(1)
}

/// Async version of exec_in using tokio::process.
#[allow(dead_code)]
pub async fn exec_in_async(container: &str, cmd: &[&str]) -> Option<String> {
    let output = tokio::process::Command::new("docker")
        .arg("exec")
        .arg(container)
        .args(cmd)
        .output()
        .await
        .ok()?;
    if output.status.success() {
        Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
    } else {
        None
    }
}

/// Spawn a long-running command in a container, returning the child process.
pub async fn exec_stream(container: &str, cmd: &[&str]) -> Result<tokio::process::Child> {
    let child = tokio::process::Command::new("docker")
        .arg("exec")
        .arg(container)
        .args(cmd)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .context("spawning docker exec")?;
    Ok(child)
}
