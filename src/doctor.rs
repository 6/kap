/// Verify devcontainer-guard is configured correctly.
///
/// Runs on the host. Finds the running app container and exec's checks into it.
use anyhow::{Context, Result};
use std::process::Command;

const PROXY_IP: &str = "172.28.0.3";

pub fn run() -> Result<()> {
    println!("devg doctor");
    println!();

    let container = find_app_container()?;
    println!("  container: {container}");
    println!();

    let mut pass = 0;
    let mut fail = 0;

    // 1. HTTP_PROXY set correctly
    print!("  HTTP_PROXY ... ");
    match exec_in(&container, &["printenv", "HTTP_PROXY"]) {
        Some(val) if val.contains(PROXY_IP) => {
            println!("ok ({val})");
            pass += 1;
        }
        Some(val) => {
            println!("WRONG ({val})");
            println!("         expected {PROXY_IP}, overlay may not be last in dockerComposeFile");
            fail += 1;
        }
        None => {
            println!("NOT SET");
            println!("         overlay may not be applied");
            fail += 1;
        }
    }

    // 2. DNS points at proxy
    print!("  DNS resolver ... ");
    match exec_in(&container, &["cat", "/etc/resolv.conf"]) {
        Some(resolv) if resolv.contains(PROXY_IP) => {
            println!("ok");
            pass += 1;
        }
        _ => {
            println!("WRONG (expected nameserver {PROXY_IP})");
            fail += 1;
        }
    }

    // 3. Proxy reachable (TCP connect to port 3128)
    print!("  proxy reachable ... ");
    // Use bash to do a /dev/tcp connect since curl to a bare proxy is unreliable
    let code = exec_exit_code(
        &container,
        &["bash", "-c", &format!("echo > /dev/tcp/{PROXY_IP}/3128")],
    );
    if code == 0 {
        println!("ok");
        pass += 1;
    } else {
        println!("FAIL (can't reach {PROXY_IP}:3128)");
        fail += 1;
    }

    // 4. DNS resolves allowed domain
    print!("  DNS allows github.com ... ");
    match exec_in(&container, &["dig", "+short", "+time=3", "github.com"]) {
        Some(out) if !out.is_empty() => {
            println!("ok");
            pass += 1;
        }
        _ => {
            println!("FAIL");
            fail += 1;
        }
    }

    // 5. DNS blocks disallowed domain
    print!("  DNS blocks evil.test ... ");
    match exec_in(&container, &["dig", "+short", "+time=3", "evil.test"]) {
        Some(out) if out.is_empty() => {
            println!("ok (NXDOMAIN)");
            pass += 1;
        }
        None => {
            println!("ok (NXDOMAIN)");
            pass += 1;
        }
        _ => {
            println!("FAIL (resolved, should be blocked)");
            fail += 1;
        }
    }

    // 6. HTTPS to blocked domain is denied
    print!("  HTTPS blocked ... ");
    let http_code = exec_in(
        &container,
        &["curl", "-s", "-o", "/dev/null", "-w", "%{http_code}", "--max-time", "5", "https://example.com"],
    );
    let code = http_code.as_deref().unwrap_or("").trim();
    if code == "403" || code == "000" || code.is_empty() {
        println!("ok (example.com blocked)");
        pass += 1;
    } else {
        println!("FAIL (got HTTP {code}, expected 403)");
        println!("         overlay may not be last in dockerComposeFile");
        fail += 1;
    }

    println!();
    if fail == 0 {
        println!("  all {pass} checks passed");
    } else {
        println!("  {pass} passed, {fail} failed");
        std::process::exit(1);
    }
    Ok(())
}

/// Find the running app container by looking for devg's compose project.
fn find_app_container() -> Result<String> {
    // Look for containers with the devg overlay network
    let output = Command::new("docker")
        .args(["ps", "--format", "{{.Names}}"])
        .output()
        .context("running docker ps")?;

    let names = String::from_utf8_lossy(&output.stdout);

    // Find a container on the devg_sandbox network that isn't the devg sidecar
    for name in names.lines() {
        let name = name.trim();
        if name.is_empty() || name.contains("devg-devg") || name.ends_with("-devg-1") {
            continue;
        }
        // Check if it's on the devg_sandbox network
        let inspect = Command::new("docker")
            .args(["inspect", "--format", "{{json .NetworkSettings.Networks}}", name])
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .unwrap_or_default();
        if inspect.contains("devg_sandbox") {
            return Ok(name.to_string());
        }
    }

    anyhow::bail!("no running devcontainer found with devg networking. Is the devcontainer up?")
}

fn exec_in(container: &str, cmd: &[&str]) -> Option<String> {
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

fn exec_exit_code(container: &str, cmd: &[&str]) -> i32 {
    Command::new("docker")
        .arg("exec")
        .arg(container)
        .args(cmd)
        .output()
        .ok()
        .and_then(|o| o.status.code())
        .unwrap_or(1)
}
