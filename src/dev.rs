/// Internal dev helpers used by `container.rs` for auto-deploying cached
/// dev binaries on `kap up --reset`. Build workflow lives in `mise.toml`.
use std::path::PathBuf;
use std::process::Command;

/// Path to the cached dev Linux binary.
pub fn cached_binary_path() -> PathBuf {
    std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/tmp"))
        .join(".kap/dev/kap-linux")
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
