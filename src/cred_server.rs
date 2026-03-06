/// Host-side credential server.
///
/// Listens on a Unix socket and serves git credentials using `gh auth token`.
/// The token never enters the container — only the credential helper response
/// crosses the socket boundary.
use anyhow::{Context, Result};
use std::collections::HashMap;
use std::path::Path;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;

use crate::config::Config;

pub async fn run(config: Config, daemonize: bool) -> Result<()> {
    let socket_path = shellexpand_tilde(&config.credentials.host_socket);

    if daemonize {
        // Check if already running
        if Path::new(&socket_path).exists() {
            // Try connecting to see if it's alive
            if tokio::net::UnixStream::connect(&socket_path).await.is_ok() {
                eprintln!("[cred-server] already running at {socket_path}");
                return Ok(());
            }
            // Stale socket, remove it
            std::fs::remove_file(&socket_path)?;
        }

        // Fork into background
        unsafe {
            let pid = libc::fork();
            if pid < 0 {
                anyhow::bail!("fork failed");
            }
            if pid > 0 {
                // Parent — exit
                eprintln!("[cred-server] started in background (pid {pid}) at {socket_path}");
                return Ok(());
            }
            // Child — continue as daemon
            libc::setsid();
        }
    }

    // Ensure socket directory exists
    if let Some(parent) = Path::new(&socket_path).parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating socket directory: {}", parent.display()))?;
    }

    // Remove stale socket
    if Path::new(&socket_path).exists() {
        std::fs::remove_file(&socket_path)?;
    }

    let listener = UnixListener::bind(&socket_path)?;
    // Make socket readable by the container user
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&socket_path, std::fs::Permissions::from_mode(0o666))?;
    }

    eprintln!("[cred-server] listening on {socket_path}");

    let allowed_hosts: Vec<String> = config.credentials.github.hosts.clone();

    loop {
        let (stream, _) = listener.accept().await?;
        let allowed_hosts = allowed_hosts.clone();

        tokio::spawn(async move {
            if let Err(e) = handle_client(stream, &allowed_hosts).await {
                eprintln!("[cred-server] client error: {e}");
            }
        });
    }
}

async fn handle_client(stream: tokio::net::UnixStream, allowed_hosts: &[String]) -> Result<()> {
    let (reader, mut writer) = stream.into_split();
    let reader = BufReader::new(reader);
    let mut lines = reader.lines();

    // Read git credential helper protocol: key=value pairs, blank line terminates
    let mut params = HashMap::new();
    while let Some(line) = lines.next_line().await? {
        if line.is_empty() {
            break;
        }
        if let Some((key, value)) = line.split_once('=') {
            params.insert(key.to_string(), value.to_string());
        }
    }

    let host = params.get("host").map(|s| s.as_str()).unwrap_or("");
    let protocol = params.get("protocol").map(|s| s.as_str()).unwrap_or("");

    if protocol != "https" || !allowed_hosts.iter().any(|h| h == host) {
        // Return empty response (no credentials)
        writer.write_all(b"\n").await?;
        return Ok(());
    }

    // Get token from gh auth
    match get_gh_token().await {
        Ok(token) => {
            let response = format!(
                "protocol=https\nhost={host}\nusername=x-access-token\npassword={token}\n\n"
            );
            writer.write_all(response.as_bytes()).await?;
        }
        Err(e) => {
            eprintln!("[cred-server] failed to get gh token: {e}");
            writer.write_all(b"\n").await?;
        }
    }

    Ok(())
}

async fn get_gh_token() -> Result<String> {
    let output = tokio::process::Command::new("gh")
        .args(["auth", "token"])
        .output()
        .await
        .context("running 'gh auth token'")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("gh auth token failed: {stderr}");
    }

    let token = String::from_utf8(output.stdout)?.trim().to_string();
    if token.is_empty() {
        anyhow::bail!("gh auth token returned empty");
    }
    Ok(token)
}

#[allow(clippy::collapsible_if)]
fn shellexpand_tilde(path: &str) -> String {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            return format!("{}/{rest}", home.to_string_lossy());
        }
    }
    path.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn temp_socket(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("devp-test-{}-{name}.sock", std::process::id()))
    }

    #[tokio::test]
    async fn rejects_non_https_protocol() {
        let path = temp_socket("proto");
        let _ = std::fs::remove_file(&path);
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        let hosts = vec!["github.com".to_string()];

        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            handle_client(stream, &hosts).await.unwrap();
        });

        let mut client = tokio::net::UnixStream::connect(&path).await.unwrap();
        client
            .write_all(b"protocol=http\nhost=github.com\n\n")
            .await
            .unwrap();

        let mut response = Vec::new();
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            client.read_to_end(&mut response),
        )
        .await
        .expect("timed out")
        .unwrap();

        assert_eq!(response, b"\n");
        server.await.unwrap();
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn rejects_unlisted_host() {
        let path = temp_socket("host");
        let _ = std::fs::remove_file(&path);
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        let hosts = vec!["github.com".to_string()];

        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            handle_client(stream, &hosts).await.unwrap();
        });

        let mut client = tokio::net::UnixStream::connect(&path).await.unwrap();
        client
            .write_all(b"protocol=https\nhost=evil.com\n\n")
            .await
            .unwrap();

        let mut response = Vec::new();
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            client.read_to_end(&mut response),
        )
        .await
        .expect("timed out")
        .unwrap();

        assert_eq!(response, b"\n");
        server.await.unwrap();
        let _ = std::fs::remove_file(&path);
    }
}
