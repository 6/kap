use anyhow::{Context, Result};
use serde::Serialize;
use std::collections::HashMap;
use std::process::Command;

const SANDBOX_NETWORK: &str = "devg_sandbox";

#[derive(Debug, Clone, Serialize)]
pub struct ContainerGroup {
    pub project: String,
    pub app: String,
    pub sidecar: String,
}

/// Find the app and sidecar containers on the devg_sandbox network.
/// Returns the first matching pair (for backward compatibility with status.rs).
pub fn find_containers() -> Result<(String, String)> {
    let groups = find_all_containers()?;
    groups
        .into_iter()
        .next()
        .map(|g| (g.app, g.sidecar))
        .ok_or_else(|| {
            anyhow::anyhow!(
                "no running devcontainer found with devg networking.\n\n  \
                 Start it with: devg up"
            )
        })
}

/// Find all running devcontainer groups, keyed by Docker Compose project name.
pub fn find_all_containers() -> Result<Vec<ContainerGroup>> {
    let output = Command::new("docker")
        .args([
            "ps",
            "--format",
            r#"{{.Names}}	{{.Label "com.docker.compose.project"}}	{{.Networks}}"#,
        ])
        .output()
        .context("running docker ps")?;

    let text = String::from_utf8_lossy(&output.stdout);
    Ok(parse_container_list(&text))
}

/// Find containers for a specific project.
pub fn find_by_project(project: &str) -> Result<(String, String)> {
    let groups = find_all_containers()?;
    groups
        .into_iter()
        .find(|g| g.project == project)
        .map(|g| (g.app, g.sidecar))
        .ok_or_else(|| anyhow::anyhow!("no containers found for project {project}"))
}

/// Parse docker ps output into container groups. Pure function for testability.
fn parse_container_list(text: &str) -> Vec<ContainerGroup> {
    // project -> (app candidates, sidecar candidates)
    let mut projects: HashMap<String, (Vec<String>, Vec<String>)> = HashMap::new();

    for line in text.lines() {
        let parts: Vec<&str> = line.split('\t').collect();
        if parts.len() < 3 {
            continue;
        }
        let name = parts[0].trim();
        let project = parts[1].trim();
        let networks = parts[2].trim();

        if name.is_empty() || project.is_empty() {
            continue;
        }
        if !networks.contains(SANDBOX_NETWORK) {
            continue;
        }

        let entry = projects.entry(project.to_string()).or_default();
        if is_sidecar_name(name) {
            entry.1.push(name.to_string());
        } else {
            entry.0.push(name.to_string());
        }
    }

    let mut groups: Vec<ContainerGroup> = projects
        .into_iter()
        .filter_map(|(project, (apps, sidecars))| {
            let app = apps.into_iter().next()?;
            let sidecar = sidecars.into_iter().next()?;
            Some(ContainerGroup {
                project,
                app,
                sidecar,
            })
        })
        .collect();
    groups.sort_by(|a, b| a.project.cmp(&b.project));
    groups
}

fn is_sidecar_name(name: &str) -> bool {
    name.contains("devg-devg") || name.ends_with("-devg-1")
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_single_project() {
        let input = "myproject-app-1\tmyproject\tmyproject_devg_sandbox\n\
                      myproject-devg-1\tmyproject\tmyproject_devg_sandbox\n";
        let groups = parse_container_list(input);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].project, "myproject");
        assert_eq!(groups[0].app, "myproject-app-1");
        assert_eq!(groups[0].sidecar, "myproject-devg-1");
    }

    #[test]
    fn parse_multiple_projects() {
        let input = "alpha-app-1\talpha\talpha_devg_sandbox\n\
                      alpha-devg-1\talpha\talpha_devg_sandbox\n\
                      beta-app-1\tbeta\tbeta_devg_sandbox\n\
                      beta-devg-1\tbeta\tbeta_devg_sandbox\n";
        let groups = parse_container_list(input);
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].project, "alpha");
        assert_eq!(groups[1].project, "beta");
    }

    #[test]
    fn parse_ignores_non_sandbox_containers() {
        let input = "web-1\twebproject\tbridge\n\
                      myproject-app-1\tmyproject\tmyproject_devg_sandbox\n\
                      myproject-devg-1\tmyproject\tmyproject_devg_sandbox\n";
        let groups = parse_container_list(input);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].project, "myproject");
    }

    #[test]
    fn parse_ignores_incomplete_groups() {
        // Only has app, no sidecar
        let input = "myproject-app-1\tmyproject\tmyproject_devg_sandbox\n";
        let groups = parse_container_list(input);
        assert_eq!(groups.len(), 0);
    }

    #[test]
    fn parse_empty_input() {
        assert_eq!(parse_container_list("").len(), 0);
    }

    #[test]
    fn parse_malformed_lines() {
        let input = "just-a-name\n\
                      two\tfields\n\
                      ok-app-1\tok\tok_devg_sandbox\n\
                      ok-devg-1\tok\tok_devg_sandbox\n";
        let groups = parse_container_list(input);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].project, "ok");
    }

    #[test]
    fn parse_devg_devg_sidecar_pattern() {
        let input = "proj-app-1\tproj\tproj_devg_sandbox\n\
                      proj-devg-devg-1\tproj\tproj_devg_sandbox\n";
        let groups = parse_container_list(input);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].sidecar, "proj-devg-devg-1");
    }

    #[test]
    fn is_sidecar_name_patterns() {
        assert!(is_sidecar_name("myproject-devg-1"));
        assert!(is_sidecar_name("proj-devg-devg-1"));
        assert!(!is_sidecar_name("myproject-app-1"));
        assert!(!is_sidecar_name("myproject-web-1"));
    }

    #[test]
    fn container_group_serializes() {
        let g = ContainerGroup {
            project: "myproj".into(),
            app: "myproj-app-1".into(),
            sidecar: "myproj-devg-1".into(),
        };
        let json: serde_json::Value = serde_json::to_value(&g).unwrap();
        assert_eq!(json["project"], "myproj");
        assert_eq!(json["app"], "myproj-app-1");
        assert_eq!(json["sidecar"], "myproj-devg-1");
    }

    #[test]
    fn parse_results_sorted_by_project() {
        let input = "z-app-1\tz\tz_devg_sandbox\n\
                      z-devg-1\tz\tz_devg_sandbox\n\
                      a-app-1\ta\ta_devg_sandbox\n\
                      a-devg-1\ta\ta_devg_sandbox\n";
        let groups = parse_container_list(input);
        assert_eq!(groups[0].project, "a");
        assert_eq!(groups[1].project, "z");
    }

    #[test]
    fn parse_only_sidecar_no_app() {
        let input = "proj-devg-1\tproj\tproj_devg_sandbox\n";
        let groups = parse_container_list(input);
        assert_eq!(groups.len(), 0);
    }

    #[test]
    fn parse_multiple_networks_containing_sandbox() {
        // Docker can show comma-separated networks
        let input = "proj-app-1\tproj\tbridge,proj_devg_sandbox\n\
                      proj-devg-1\tproj\tbridge,proj_devg_sandbox\n";
        let groups = parse_container_list(input);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].project, "proj");
    }
}
