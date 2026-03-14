/// `kap upgrade` — update all running sidecar containers to the latest
/// published image without rebuilding the full devcontainer.
///
/// Steps:
///   1. Pull the latest sidecar image
///   2. Extract the kap binary from it
///   3. docker cp into each running sidecar
///   4. docker restart each sidecar
use anyhow::{Context, Result};
use std::process::Command;

use crate::config::DEFAULT_IMAGE;

pub fn run() -> Result<()> {
    // 1. Find running sidecars
    let sidecars = find_all_sidecars()?;
    if sidecars.is_empty() {
        eprintln!("No running kap sidecar containers found.");
        return Ok(());
    }

    // Show current versions
    eprintln!(
        "Found {} sidecar(s). Checking versions...\n",
        sidecars.len()
    );
    let host_version = env!("CARGO_PKG_VERSION");
    eprintln!("  host:  kap {host_version}");
    for sc in &sidecars {
        let v = sidecar_version(sc).unwrap_or_else(|| "unknown".into());
        eprintln!("  {sc}:  kap {v}");
    }
    eprintln!();

    // 2. Pull latest image
    eprintln!("Pulling {DEFAULT_IMAGE}...");
    let status = Command::new("docker")
        .args(["pull", DEFAULT_IMAGE])
        .status()
        .context("failed to run docker pull")?;
    if !status.success() {
        anyhow::bail!("docker pull failed");
    }

    // 3. Extract binary from the image
    eprintln!("Extracting binary...");
    let tmp_binary = std::env::temp_dir().join("kap-upgrade-linux");
    let create_output = Command::new("docker")
        .args(["create", DEFAULT_IMAGE])
        .output()
        .context("failed to create temp container")?;
    if !create_output.status.success() {
        anyhow::bail!("docker create failed");
    }
    let container_id = String::from_utf8_lossy(&create_output.stdout)
        .trim()
        .to_string();

    let cp_result = Command::new("docker")
        .args([
            "cp",
            &format!("{container_id}:/usr/local/bin/kap"),
            tmp_binary.to_str().unwrap(),
        ])
        .status();

    // Always clean up the temp container
    let _ = Command::new("docker").args(["rm", &container_id]).output();

    cp_result.context("failed to extract binary from image")?;

    // 4. Deploy to each sidecar
    for sc in &sidecars {
        eprintln!("Upgrading {sc}...");
        let status = Command::new("docker")
            .args([
                "cp",
                tmp_binary.to_str().unwrap(),
                &format!("{sc}:/usr/local/bin/kap"),
            ])
            .status()
            .context("docker cp failed")?;
        if !status.success() {
            eprintln!("  warning: failed to copy binary to {sc}");
            continue;
        }

        let status = Command::new("docker")
            .args(["restart", sc])
            .status()
            .context("docker restart failed")?;
        if !status.success() {
            eprintln!("  warning: failed to restart {sc}");
        }
    }

    // Clean up
    let _ = std::fs::remove_file(&tmp_binary);

    // Show updated versions
    eprintln!("\nUpgraded {} sidecar(s):\n", sidecars.len());
    for sc in &sidecars {
        let v = sidecar_version(sc).unwrap_or_else(|| "unknown".into());
        eprintln!("  {sc}:  kap {v}");
    }

    Ok(())
}

fn sidecar_version(container: &str) -> Option<String> {
    let output = Command::new("docker")
        .args(["exec", container, "kap", "--version"])
        .output()
        .ok()?;
    let raw = String::from_utf8_lossy(&output.stdout);
    Some(parse_version(&raw))
}

fn find_all_sidecars() -> Result<Vec<String>> {
    let output = Command::new("docker")
        .args(["ps", "--format", "{{.Names}}"])
        .output()
        .context("failed to run docker ps")?;
    let names = String::from_utf8_lossy(&output.stdout);
    Ok(filter_sidecar_names(&names))
}

fn filter_sidecar_names(input: &str) -> Vec<String> {
    input
        .lines()
        .filter(|n| n.contains("kap-kap") || n.ends_with("-kap-1"))
        .map(String::from)
        .collect()
}

fn parse_version(raw: &str) -> String {
    raw.strip_prefix("kap ").unwrap_or(raw).trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filter_sidecar_names_matches_kap_kap() {
        let input = "myproject-kap-kap-1\nmyproject-app-1\nother-container";
        let result = filter_sidecar_names(input);
        assert_eq!(result, vec!["myproject-kap-kap-1"]);
    }

    #[test]
    fn filter_sidecar_names_matches_kap_suffix() {
        let input = "myproject-kap-1\nmyproject-app-1";
        let result = filter_sidecar_names(input);
        assert_eq!(result, vec!["myproject-kap-1"]);
    }

    #[test]
    fn filter_sidecar_names_multiple_projects() {
        let input = "proj-a-kap-kap-1\nproj-a-app-1\nproj-b-kap-kap-1\nproj-b-app-1";
        let result = filter_sidecar_names(input);
        assert_eq!(result, vec!["proj-a-kap-kap-1", "proj-b-kap-kap-1"]);
    }

    #[test]
    fn filter_sidecar_names_empty_input() {
        assert!(filter_sidecar_names("").is_empty());
    }

    #[test]
    fn filter_sidecar_names_no_sidecars() {
        let input = "postgres-1\nredis-1\nmyapp-1";
        assert!(filter_sidecar_names(input).is_empty());
    }

    #[test]
    fn parse_version_strips_prefix() {
        assert_eq!(parse_version("kap 0.0.1"), "0.0.1");
    }

    #[test]
    fn parse_version_prerelease() {
        assert_eq!(parse_version("kap 0.0.1-pre14"), "0.0.1-pre14");
    }

    #[test]
    fn parse_version_no_prefix() {
        assert_eq!(parse_version("0.0.1"), "0.0.1");
    }

    #[test]
    fn parse_version_with_whitespace() {
        assert_eq!(parse_version("kap 0.0.1\n"), "0.0.1");
    }

    #[test]
    fn parse_version_empty() {
        assert_eq!(parse_version(""), "");
    }
}
