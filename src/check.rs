/// Health and setup verification.
use anyhow::Result;

pub async fn run(proxy_only: bool) -> Result<()> {
    if proxy_only {
        // Lightweight check for container healthcheck
        return check_proxy_listening().await;
    }

    let mut all_ok = true;

    // Check proxy
    print!("proxy .............. ");
    match check_proxy_listening().await {
        Ok(()) => println!("OK"),
        Err(e) => {
            println!("FAIL: {e}");
            all_ok = false;
        }
    }

    // Check DNS
    print!("dns ................ ");
    match check_dns().await {
        Ok(()) => println!("OK"),
        Err(e) => {
            println!("FAIL: {e}");
            all_ok = false;
        }
    }

    // Check credential socket
    print!("cred-server ........ ");
    match check_cred_server().await {
        Ok(()) => println!("OK"),
        Err(e) => {
            println!("FAIL: {e}");
            all_ok = false;
        }
    }

    // Check git credential helper
    print!("git credential ..... ");
    match check_git_credential() {
        Ok(()) => println!("OK"),
        Err(e) => {
            println!("FAIL: {e}");
            all_ok = false;
        }
    }

    println!();
    if all_ok {
        println!("All checks passed.");
    } else {
        println!("Some checks failed. Review the errors above.");
        std::process::exit(1);
    }

    Ok(())
}

async fn check_proxy_listening() -> Result<()> {
    // Try connecting to the proxy port
    tokio::net::TcpStream::connect("127.0.0.1:3128")
        .await
        .map_err(|_| anyhow::anyhow!("cannot connect to proxy on port 3128"))?;
    Ok(())
}

async fn check_dns() -> Result<()> {
    // Send a simple DNS query to localhost:53
    let sock = tokio::net::UdpSocket::bind("0.0.0.0:0").await?;
    // Build a minimal DNS query for "github.com"
    let query = build_dns_query("github.com");
    sock.send_to(&query, "127.0.0.1:53").await?;

    let mut buf = [0u8; 512];
    let timeout = tokio::time::timeout(std::time::Duration::from_secs(3), sock.recv_from(&mut buf))
        .await
        .map_err(|_| anyhow::anyhow!("DNS query timed out"))?;

    let (len, _) = timeout?;
    if len < 12 {
        anyhow::bail!("DNS response too short");
    }
    // Check that it's a response (QR bit set)
    if buf[2] & 0x80 == 0 {
        anyhow::bail!("DNS response missing QR flag");
    }
    Ok(())
}

async fn check_cred_server() -> Result<()> {
    let socket_path = "/devp-sockets/cred.sock";
    if !std::path::Path::new(socket_path).exists() {
        anyhow::bail!("socket not found at {socket_path}");
    }
    tokio::net::UnixStream::connect(socket_path)
        .await
        .map_err(|e| anyhow::anyhow!("cannot connect to cred-server: {e}"))?;
    Ok(())
}

fn check_git_credential() -> Result<()> {
    let output = std::process::Command::new("git")
        .args(["config", "--get", "credential.helper"])
        .output()?;
    let helper = String::from_utf8_lossy(&output.stdout);
    if helper.contains("devp") {
        Ok(())
    } else {
        anyhow::bail!("git credential helper not configured (got: {helper})")
    }
}

fn build_dns_query(domain: &str) -> Vec<u8> {
    let mut pkt = Vec::new();
    // Header
    pkt.extend_from_slice(&[0xAB, 0xCD]); // ID
    pkt.extend_from_slice(&[0x01, 0x00]); // flags: standard query, RD=1
    pkt.extend_from_slice(&[0x00, 0x01]); // QDCOUNT=1
    pkt.extend_from_slice(&[0x00, 0x00, 0x00, 0x00, 0x00, 0x00]); // AN/NS/AR=0
    // Question
    for label in domain.split('.') {
        pkt.push(label.len() as u8);
        pkt.extend_from_slice(label.as_bytes());
    }
    pkt.push(0); // end
    pkt.extend_from_slice(&[0x00, 0x01]); // QTYPE=A
    pkt.extend_from_slice(&[0x00, 0x01]); // QCLASS=IN
    pkt
}
