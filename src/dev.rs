/// Development tools for working on kap itself.
///
/// `kap dev push` builds a Linux binary from the current source,
/// caches it at `~/.kap/dev/kap-linux`, and deploys it to all running
/// sidecar containers. If no sidecars are running, the binary is still
/// built and cached so that the next `kap up` can deploy it automatically.
use anyhow::{Context, Result};
use std::path::PathBuf;
use std::process::Command;

/// Path to the cached dev Linux binary.
pub fn cached_binary_path() -> PathBuf {
    dirs_home().join(".kap").join("dev").join("kap-linux")
}

fn dirs_home() -> PathBuf {
    std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/tmp"))
}

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

    // 2. Find all running kap sidecar containers
    let sidecars = find_all_sidecars()?;

    // 3. Build host binary
    eprintln!("[dev] building host binary...");
    let status = Command::new("cargo")
        .args(["install", "--path", "."])
        .status()
        .context("failed to run cargo install")?;
    if !status.success() {
        anyhow::bail!("cargo install failed");
    }

    // 4. Build Linux binary via Docker
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

    // 5. Extract the Linux binary from the image and cache it
    let cache_path = cached_binary_path();
    std::fs::create_dir_all(cache_path.parent().unwrap())?;

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
            cache_path.to_str().unwrap(),
        ])
        .status();

    // Always clean up the temp container
    let _ = Command::new("docker").args(["rm", &container_id]).output();

    cp_status.context("failed to extract binary from image")?;
    eprintln!("[dev] cached Linux binary at {}", cache_path.display());

    // 6. Deploy to running sidecars (if any)
    if sidecars.is_empty() {
        eprintln!("[dev] no running sidecars — binary cached for next `kap up`");
    } else {
        deploy_to_sidecars(&cache_path, &sidecars);
    }

    // Restart remote daemon if running (uses the newly installed host binary)
    let restarted_remote = restart_remote_daemon();

    eprintln!();
    if !sidecars.is_empty() {
        eprintln!("  ✓ Pushed to {} sidecar(s)", sidecars.len());
    }
    eprintln!("  ✓ Cached at {}", cache_path.display());
    if restarted_remote {
        eprintln!("  ✓ Restarted remote daemon");
    }
    eprintln!();
    eprintln!("  ⚠ Do NOT use kap up --reset (it will pull the");
    eprintln!("    published image and overwrite the dev binary)");
    Ok(())
}

/// Deploy a cached Linux binary to the given sidecar containers.
pub fn deploy_to_sidecars(binary_path: &std::path::Path, sidecars: &[String]) {
    for sidecar in sidecars {
        eprintln!("[dev] deploying to {sidecar}...");
        let status = Command::new("docker")
            .args([
                "cp",
                binary_path.to_str().unwrap(),
                &format!("{sidecar}:/usr/local/bin/kap"),
            ])
            .status();
        if status.map(|s| !s.success()).unwrap_or(true) {
            eprintln!("[dev] warning: failed to copy binary to {sidecar}");
            continue;
        }

        let status = Command::new("docker").args(["restart", sidecar]).status();
        if status.map(|s| !s.success()).unwrap_or(true) {
            eprintln!("[dev] warning: failed to restart {sidecar}");
        }
    }
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
