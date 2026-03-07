/// Check if kap is working.
///
/// Runs on the host. Reads local config, finds the running containers,
/// and exec's checks into the app container.
use anyhow::Result;

use crate::remote::containers::{exec_exit_code, exec_in, find_containers};

/// Read the sidecar IP from the app container's HTTP_PROXY env var.
fn proxy_ip(app: &str) -> String {
    exec_in(app, &["printenv", "HTTP_PROXY"])
        .and_then(|v| {
            v.strip_prefix("http://")
                .and_then(|rest| rest.split(':').next())
                .map(String::from)
        })
        .unwrap_or_else(|| "172.28.0.3".to_string())
}

fn ok(msg: &str, pass: &mut u32) {
    println!("\x1b[32m ok\x1b[0m  {msg}");
    *pass += 1;
}

fn bad(msg: &str, fail: &mut u32) {
    println!("\x1b[31m !\x1b[0m   {msg}");
    *fail += 1;
}

pub fn run() -> Result<()> {
    println!();

    let config = load_local_config();
    print_config_summary(&config);

    let (app, sidecar) = find_containers()?;
    let proxy_ip = proxy_ip(&app);

    let mut pass = 0;
    let mut fail = 0;

    // Network checks
    println!("  Network");

    match exec_in(&app, &["printenv", "HTTP_PROXY"]) {
        Some(val) if val.contains(&proxy_ip) => ok("HTTP_PROXY set", &mut pass),
        Some(_) => bad(
            "HTTP_PROXY points to wrong address (overlay may not be last in dockerComposeFile)",
            &mut fail,
        ),
        None => bad("HTTP_PROXY not set (overlay may not be applied)", &mut fail),
    }

    match exec_in(&app, &["cat", "/etc/resolv.conf"]) {
        Some(resolv) if resolv.contains(&proxy_ip) => ok("DNS resolver configured", &mut pass),
        _ => bad("DNS resolver not pointing to proxy", &mut fail),
    }

    if exec_exit_code(
        &app,
        &["bash", "-c", &format!("echo > /dev/tcp/{proxy_ip}/3128")],
    ) == 0
    {
        ok("proxy reachable", &mut pass);
    } else {
        bad("proxy not reachable on :3128", &mut fail);
    }

    // DNS allow test (first non-wildcard domain from config)
    let allowed_domain = config
        .proxy
        .network
        .allow
        .iter()
        .find(|d| !d.starts_with('*'))
        .cloned();

    if let Some(ref domain) = allowed_domain {
        match exec_in(&app, &["dig", "+short", "+time=3", domain]) {
            Some(out) if !out.is_empty() => ok(&format!("DNS resolves {domain}"), &mut pass),
            _ => bad(&format!("DNS failed to resolve {domain}"), &mut fail),
        }
    }

    // DNS block test (.invalid is reserved by RFC 2606)
    match exec_in(&app, &["dig", "+short", "+time=3", "kap-test.invalid"]) {
        Some(out) if out.is_empty() => ok("DNS blocks unlisted domains", &mut pass),
        None => ok("DNS blocks unlisted domains", &mut pass),
        _ => bad(
            "DNS resolved unlisted domain (forwarder may not be active)",
            &mut fail,
        ),
    }

    // HTTPS block test
    let http_code = exec_in(
        &app,
        &[
            "curl",
            "-s",
            "-o",
            "/dev/null",
            "-w",
            "%{http_code}",
            "--max-time",
            "5",
            "https://kap-test.invalid",
        ],
    );
    let code = http_code.as_deref().unwrap_or("").trim();
    if code == "403" || code == "000" || code.is_empty() {
        ok("HTTPS to unlisted domain denied", &mut pass);
    } else {
        bad(&format!("unlisted HTTPS returned HTTP {code}"), &mut fail);
    }

    // MCP checks
    if let Some(ref mcp) = config.mcp
        && !mcp.servers.is_empty()
    {
        let host_auth_dir = crate::mcp::auth::host_auth_dir();
        let available = crate::mcp::list_auth_files(&host_auth_dir);
        // Pre-flight: validate credentials for all project MCP servers
        for server in &mcp.servers {
            if let Some(ref env_var) = server.token_env {
                match std::env::var(env_var) {
                    Ok(val) if !val.is_empty() => {}
                    _ => bad(
                        &format!("{}: ${env_var} is not set or empty", server.name),
                        &mut fail,
                    ),
                }
            } else if server.headers.is_empty() {
                // OAuth server — check auth file exists
                let auth_path =
                    std::path::Path::new(&host_auth_dir).join(format!("{}.json", server.name));
                if !auth_path.exists() {
                    let hint = if available.is_empty() {
                        format!("run `kap mcp add {} <url>`", server.name)
                    } else {
                        format!(
                            "available: {}. run `kap mcp add {} <url>` or check for typos",
                            available.join(", "),
                            server.name
                        )
                    };
                    bad(
                        &format!("{}: no auth registered ({})", server.name, hint),
                        &mut fail,
                    );
                }
            }
        }

        // Check auth dir is mounted in sidecar
        let has_auth_mount = exec_exit_code(&sidecar, &["test", "-d", "/etc/kap/auth"]) == 0;
        if has_auth_mount {
            ok("auth dir mounted in sidecar", &mut pass);
        } else {
            bad(
                "auth dir not mounted (add ~/.kap/auth:/etc/kap/auth to compose volumes)",
                &mut fail,
            );
        }

        if exec_exit_code(
            &app,
            &["bash", "-c", &format!("echo > /dev/tcp/{proxy_ip}/3129")],
        ) == 0
        {
            ok("MCP proxy reachable", &mut pass);
        } else {
            bad("MCP proxy not reachable on :3129", &mut fail);
        }

        // Run `kap sidecar-check --mcp` inside the sidecar (uses reqwest, handles
        // initialize + tools/list with session IDs properly).
        if let Some(output) = exec_in(&sidecar, &["kap", "sidecar-check", "--mcp"]) {
            for line in output.lines() {
                let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
                    continue;
                };
                let name = v["name"].as_str().unwrap_or("?");
                if let Some(count) = v["tools"].as_u64() {
                    ok(&format!("{name} ({count} tools)"), &mut pass);
                } else if let Some(err) = v["error"].as_str() {
                    bad(&format!("{name}: {err}"), &mut fail);
                }
            }
        } else {
            bad("kap sidecar-check --mcp failed in sidecar", &mut fail);
        }
    }

    // CLI proxy checks
    if let Some(ref cli) = config.cli
        && !cli.tools.is_empty()
    {
        if exec_exit_code(
            &app,
            &["bash", "-c", &format!("echo > /dev/tcp/{proxy_ip}/3130")],
        ) == 0
        {
            ok("CLI proxy reachable", &mut pass);
        } else {
            bad("CLI proxy not reachable on :3130", &mut fail);
        }

        // Check each tool's shim is installed
        for tool in &cli.tools {
            if exec_exit_code(&app, &["which", &tool.name]) == 0 {
                ok(&format!("{} shim installed", tool.name), &mut pass);
            } else {
                bad(
                    &format!("{} shim not found in app container", tool.name),
                    &mut fail,
                );
            }
        }

        // Check env vars are set on sidecar
        for tool in &cli.tools {
            for var in &tool.env {
                if exec_exit_code(&sidecar, &["sh", "-c", &format!("test -n \"${var}\"")]) == 0 {
                    ok(&format!("${var} set on sidecar"), &mut pass);
                } else {
                    bad(
                        &format!("{}: ${var} not set on sidecar", tool.name),
                        &mut fail,
                    );
                }
            }
        }
    }

    // Recent denials (from sidecar proxy log)
    let denied_count = exec_in(
        &sidecar,
        &[
            "sh",
            "-c",
            "grep -c '\"denied\"' /var/log/kap/proxy.jsonl 2>/dev/null || echo 0",
        ],
    )
    .and_then(|s| s.trim().parse::<u64>().ok())
    .unwrap_or(0);

    if denied_count > 0 {
        println!();
        println!("  {denied_count} denied requests (run `kap why-denied` for details)");
    }

    // Summary
    println!();
    if fail == 0 {
        println!("  \x1b[32mall {pass} checks passed\x1b[0m");
    } else {
        println!("  \x1b[31m{fail} failed\x1b[0m, {pass} passed");
        std::process::exit(1);
    }
    println!();
    Ok(())
}

fn print_config_summary(config: &crate::config::Config) {
    let allow_count = config.proxy.network.allow.len();
    let deny_count = config.proxy.network.deny.len();
    println!("  Config");
    if allow_count == 0 {
        println!("    domains: NONE (all traffic will be blocked)");
    } else if deny_count > 0 {
        println!("    domains: {allow_count} allowed, {deny_count} denied");
    } else {
        println!("    domains: {allow_count} allowed");
    }
    if let Some(ref mcp) = config.mcp {
        if mcp.servers.is_empty() {
            println!("    mcp: no servers");
        } else {
            let host_auth_dir = crate::mcp::auth::host_auth_dir();
            let registered = crate::mcp::list_auth_files(&host_auth_dir);
            println!("    mcp:");
            for s in &mcp.servers {
                if s.token_env.is_some() || registered.contains(&s.name) {
                    println!("      \x1b[32m✓\x1b[0m {}", s.name);
                } else {
                    println!(
                        "      \x1b[31m✗\x1b[0m {} — run `kap mcp add {0} <url>`",
                        s.name
                    );
                }
            }
        }
    }
    if let Some(ref cli) = config.cli {
        let names: Vec<&str> = cli.tools.iter().map(|t| t.name.as_str()).collect();
        if !names.is_empty() {
            println!("    cli: {}", names.join(", "));
        }
    }
    println!();
}

fn load_local_config() -> crate::config::Config {
    let path = ".devcontainer/kap.toml";
    crate::config::Config::load(path).unwrap_or_default()
}
