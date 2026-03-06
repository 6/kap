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
    tokio::net::TcpStream::connect("127.0.0.1:3128")
        .await
        .map_err(|_| anyhow::anyhow!("cannot connect to proxy on port 3128"))?;
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
