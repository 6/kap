/// Development tools for working on kap itself.
///
/// `kap dev push` builds a Linux binary from the current source and
/// deploys it to all running sidecar containers, avoiding the need
/// to publish a new Docker image for every code change.
use anyhow::{Context, Result};
use std::process::Command;

pub fn push() -> Result<()> {
    // 1. Verify we're in the kap source directory
    let dockerfile = ".devcontainer/Dockerfile";
    let is_kap_repo = std::fs::read_to_string("Cargo.toml")
        .map(|c| c.contains("name = \"kap\""))
        .unwrap_or(false);
    if !is_kap_repo || !std::path::Path::new(dockerfile).exists() {
        anyhow::bail!(
            "This must be run from the kap source directory (where Cargo.toml defines the kap crate)."
        );
    }

    // 2. Build host binary
    eprintln!("[dev] building host binary...");
    let status = Command::new("cargo")
        .args(["install", "--path", "."])
        .status()
        .context("failed to run cargo install")?;
    if !status.success() {
        anyhow::bail!("cargo install failed");
    }

    // 3. Build Linux binary via Docker
    eprintln!("[dev] building Linux binary...");
    let status = Command::new("docker")
        .args([
            "build", "--target", "proxy", "-t", "kap-dev", "-f", dockerfile, ".",
        ])
        .status()
        .context("failed to run docker build")?;
    if !status.success() {
        anyhow::bail!("docker build failed");
    }

    // 4. Extract the Linux binary from the image
    let tmp_binary = std::env::temp_dir().join("kap-dev-linux");
    let create_output = Command::new("docker")
        .args(["create", "kap-dev"])
        .output()
        .context("failed to create temp container")?;
    if !create_output.status.success() {
        anyhow::bail!("docker create failed");
    }
    let container_id = String::from_utf8_lossy(&create_output.stdout)
        .trim()
        .to_string();

    let cp_status = Command::new("docker")
        .args([
            "cp",
            &format!("{container_id}:/usr/local/bin/kap"),
            tmp_binary.to_str().unwrap(),
        ])
        .status();

    // Always clean up the temp container
    let _ = Command::new("docker").args(["rm", &container_id]).output();

    cp_status.context("failed to extract binary from image")?;

    // 5. Find all running kap sidecar containers
    let sidecars = find_all_sidecars()?;
    if sidecars.is_empty() {
        eprintln!("[dev] no running sidecar containers found");
        return Ok(());
    }

    // 6. Copy binary + restart each sidecar
    for sidecar in &sidecars {
        eprintln!("[dev] deploying to {sidecar}...");
        let status = Command::new("docker")
            .args([
                "cp",
                tmp_binary.to_str().unwrap(),
                &format!("{sidecar}:/usr/local/bin/kap"),
            ])
            .status()
            .context("docker cp failed")?;
        if !status.success() {
            eprintln!("[dev] warning: failed to copy binary to {sidecar}");
            continue;
        }

        let status = Command::new("docker")
            .args(["restart", sidecar])
            .status()
            .context("docker restart failed")?;
        if !status.success() {
            eprintln!("[dev] warning: failed to restart {sidecar}");
        }
    }

    // Clean up temp binary
    let _ = std::fs::remove_file(&tmp_binary);

    // Restart remote daemon if running (uses the newly installed host binary)
    let restarted_remote = restart_remote_daemon();

    eprintln!();
    eprintln!("  ✓ Pushed to {} sidecar(s)", sidecars.len());
    if restarted_remote {
        eprintln!("  ✓ Restarted remote daemon");
    }
    eprintln!();
    eprintln!("  ⚠ Do NOT use kap up --reset (it will pull the");
    eprintln!("    published image and overwrite the dev binary)");
    Ok(())
}

/// Restart the remote daemon if it's running. Returns true if restarted.
fn restart_remote_daemon() -> bool {
    let data_dir = crate::remote::auth::data_dir();
    let pid_file = data_dir.join("pid");

    let pid: u32 = match std::fs::read_to_string(&pid_file) {
        Ok(s) => match s.trim().parse() {
            Ok(p) => p,
            Err(_) => return false,
        },
        Err(_) => return false,
    };

    // Check if process is actually running
    if unsafe { libc::kill(pid as i32, 0) } != 0 {
        return false;
    }

    // Stop the old daemon
    unsafe {
        libc::kill(pid as i32, libc::SIGTERM);
    }
    // Brief wait for it to exit
    std::thread::sleep(std::time::Duration::from_millis(500));

    // Start the new one (detached)
    match Command::new("kap")
        .args(["remote", "start"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
    {
        Ok(_) => {
            let _ = std::fs::remove_file(&pid_file); // new process writes its own
            true
        }
        Err(_) => false,
    }
}

/// Find all running kap sidecar container names.
fn find_all_sidecars() -> Result<Vec<String>> {
    let output = Command::new("docker")
        .args(["ps", "--format", "{{.Names}}"])
        .output()
        .context("failed to run docker ps")?;
    let names = String::from_utf8_lossy(&output.stdout);
    Ok(names
        .lines()
        .filter(|n| n.contains("kap-kap") || n.ends_with("-kap-1"))
        .map(String::from)
        .collect())
}
