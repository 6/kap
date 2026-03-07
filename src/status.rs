/// Check if devcontainer-guard is working.
///
/// Runs on the host. Reads local config, finds the running containers,
/// and exec's checks into the app container.
use anyhow::{Context, Result};
use std::process::Command;

const PROXY_IP: &str = "172.28.0.3";

pub fn run() -> Result<()> {
    println!("devg status");
    println!();

    // Config summary (read from host)
    let config = load_local_config();
    print_config_summary(&config);

    // Find containers
    let (app, sidecar) = find_containers()?;
    println!("  containers:");
    println!("    app:   {app}");
    println!("    proxy: {sidecar}");
    println!();

    let mut pass = 0;
    let mut fail = 0;

    // Network checks (exec into app container)
    println!("  network:");

    print!("    HTTP_PROXY ... ");
    match exec_in(&app, &["printenv", "HTTP_PROXY"]) {
        Some(val) if val.contains(PROXY_IP) => {
            println!("ok");
            pass += 1;
        }
        Some(val) => {
            println!("WRONG ({val})");
            println!("           overlay may not be last in dockerComposeFile");
            fail += 1;
        }
        None => {
            println!("NOT SET");
            fail += 1;
        }
    }

    print!("    DNS resolver ... ");
    match exec_in(&app, &["cat", "/etc/resolv.conf"]) {
        Some(resolv) if resolv.contains(PROXY_IP) => {
            println!("ok");
            pass += 1;
        }
        _ => {
            println!("WRONG (expected {PROXY_IP})");
            fail += 1;
        }
    }

    print!("    proxy reachable ... ");
    if exec_exit_code(&app, &["bash", "-c", &format!("echo > /dev/tcp/{PROXY_IP}/3128")]) == 0 {
        println!("ok");
        pass += 1;
    } else {
        println!("FAIL");
        fail += 1;
    }

    print!("    DNS allows github.com ... ");
    match exec_in(&app, &["dig", "+short", "+time=3", "github.com"]) {
        Some(out) if !out.is_empty() => {
            println!("ok");
            pass += 1;
        }
        _ => {
            println!("FAIL");
            fail += 1;
        }
    }

    print!("    DNS blocks evil.test ... ");
    match exec_in(&app, &["dig", "+short", "+time=3", "evil.test"]) {
        Some(out) if out.is_empty() => {
            println!("ok");
            pass += 1;
        }
        None => {
            println!("ok");
            pass += 1;
        }
        _ => {
            println!("FAIL (resolved, should be blocked)");
            fail += 1;
        }
    }

    print!("    HTTPS blocked ... ");
    let http_code = exec_in(
        &app,
        &["curl", "-s", "-o", "/dev/null", "-w", "%{http_code}", "--max-time", "5", "https://example.com"],
    );
    let code = http_code.as_deref().unwrap_or("").trim();
    if code == "403" || code == "000" || code.is_empty() {
        println!("ok");
        pass += 1;
    } else {
        println!("FAIL (HTTP {code})");
        fail += 1;
    }

    // MCP checks
    if let Some(ref mcp) = config.mcp
        && !mcp.servers.is_empty()
    {
            println!();
            println!("  mcp:");

            print!("    endpoint reachable ... ");
            if exec_exit_code(&app, &["bash", "-c", &format!("echo > /dev/tcp/{PROXY_IP}/3129")]) == 0 {
                println!("ok");
                pass += 1;
            } else {
                println!("FAIL");
                fail += 1;
            }

            for server in &mcp.servers {
                print!("    {} ... ", server.name);
                let resp = exec_in(
                    &app,
                    &[
                        "curl", "-s", "--noproxy", "*", "--max-time", "5",
                        "-X", "POST",
                        &format!("http://{PROXY_IP}:3129/{}", server.name),
                        "-H", "Content-Type: application/json",
                        "-d", r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#,
                    ],
                );
                match resp {
                    Some(body) if body.contains("\"tools\"") => {
                        let tool_count = body.matches("\"name\"").count();
                        println!("ok ({tool_count} tools)");
                        pass += 1;
                    }
                    Some(body) if body.contains("unknown MCP server") => {
                        println!("NOT LOADED (check auth/credentials)");
                        fail += 1;
                    }
                    Some(body) if body.contains("\"error\"") => {
                        println!("ERROR (upstream returned error)");
                        fail += 1;
                    }
                    _ => {
                        println!("FAIL (no response)");
                        fail += 1;
                    }
                }
            }
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

fn print_config_summary(config: &crate::config::Config) {
    let allow_count = config.proxy.network.allow.len();
    let deny_count = config.proxy.network.deny.len();
    println!("  config:");
    if allow_count == 0 {
        println!("    domains: NONE (no domains allowed, all traffic will be blocked)");
    } else {
        println!("    domains: {allow_count} allowed, {deny_count} denied");
    }
    if let Some(ref mcp) = config.mcp {
        let names: Vec<&str> = mcp.servers.iter().map(|s| s.name.as_str()).collect();
        if names.is_empty() {
            println!("    mcp: no servers configured");
        } else {
            println!("    mcp: {}", names.join(", "));
        }
    } else {
        println!("    mcp: none");
    }
    println!();
}

fn load_local_config() -> crate::config::Config {
    let path = ".devcontainer/devg.toml";
    crate::config::Config::load(path).unwrap_or_default()
}

/// Find the app and sidecar containers.
fn find_containers() -> Result<(String, String)> {
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
            .args(["inspect", "--format", "{{json .NetworkSettings.Networks}}", name])
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .unwrap_or_default();
        if !inspect.contains("devg_sandbox") {
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
             Start it with: devcontainer up --workspace-folder ."
        ),
    }
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
