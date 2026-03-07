/// Verify devcontainer-guard is configured correctly.
///
/// Run from the app container to check proxy, DNS, and network isolation.
use anyhow::Result;
use std::process::Command;

const PROXY_IP: &str = "172.28.0.3";

pub fn run() -> Result<()> {
    println!("devg doctor");
    println!();

    let mut pass = 0;
    let mut fail = 0;

    // 1. HTTP_PROXY set correctly
    print!("  HTTP_PROXY ... ");
    match std::env::var("HTTP_PROXY") {
        Ok(val) if val.contains(PROXY_IP) => {
            println!("ok ({val})");
            pass += 1;
        }
        Ok(val) => {
            println!("WRONG ({val}, expected {PROXY_IP})");
            fail += 1;
        }
        Err(_) => {
            println!("NOT SET (expected http://{PROXY_IP}:3128)");
            fail += 1;
        }
    }

    // 2. DNS points at proxy
    print!("  DNS resolver ... ");
    let resolv = std::fs::read_to_string("/etc/resolv.conf").unwrap_or_default();
    if resolv.contains(PROXY_IP) {
        println!("ok (nameserver {PROXY_IP})");
        pass += 1;
    } else {
        println!("WRONG (expected nameserver {PROXY_IP})");
        println!("         dns: may not be set in docker-compose overlay");
        fail += 1;
    }

    // 3. Proxy is reachable
    print!("  proxy reachable ... ");
    if tcp_connect(PROXY_IP, 3128) {
        println!("ok ({PROXY_IP}:3128)");
        pass += 1;
    } else {
        println!("FAIL (can't reach {PROXY_IP}:3128)");
        println!("         devg sidecar may not be running");
        fail += 1;
    }

    // 4. DNS resolves allowed domain
    print!("  DNS allows github.com ... ");
    if dns_resolves("github.com") {
        println!("ok");
        pass += 1;
    } else {
        println!("FAIL (could not resolve)");
        fail += 1;
    }

    // 5. DNS blocks disallowed domain
    print!("  DNS blocks evil.test ... ");
    if !dns_resolves("evil.test") {
        println!("ok (NXDOMAIN)");
        pass += 1;
    } else {
        println!("FAIL (resolved, should be blocked)");
        println!("         DNS forwarder may not be active");
        fail += 1;
    }

    // 6. HTTPS to blocked domain is denied
    print!("  HTTPS blocked (example.com) ... ");
    match curl_status("https://example.com") {
        Some(403) | Some(0) => {
            println!("ok (blocked)");
            pass += 1;
        }
        Some(code) => {
            println!("FAIL (got HTTP {code}, expected 403)");
            println!("         docker-compose.devg.yml may not be last in dockerComposeFile");
            fail += 1;
        }
        None => {
            println!("ok (connection refused)");
            pass += 1;
        }
    }

    println!();
    if fail == 0 {
        println!("  all {pass} checks passed");
    } else {
        println!("  {pass} passed, {fail} failed");
    }

    if fail > 0 {
        std::process::exit(1);
    }
    Ok(())
}

fn tcp_connect(host: &str, port: u16) -> bool {
    std::net::TcpStream::connect_timeout(
        &format!("{host}:{port}").parse().unwrap(),
        std::time::Duration::from_secs(2),
    )
    .is_ok()
}

fn dns_resolves(domain: &str) -> bool {
    let output = Command::new("dig")
        .args(["+short", "+time=3", domain])
        .output();
    match output {
        Ok(o) => !String::from_utf8_lossy(&o.stdout).trim().is_empty(),
        Err(_) => {
            // dig not available, try getent
            Command::new("getent")
                .args(["hosts", domain])
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false)
        }
    }
}

fn curl_status(url: &str) -> Option<u16> {
    let output = Command::new("curl")
        .args(["-s", "-o", "/dev/null", "-w", "%{http_code}", "--max-time", "5", url])
        .output()
        .ok()?;
    let code_str = String::from_utf8_lossy(&output.stdout);
    code_str.trim().parse().ok()
}
