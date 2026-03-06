/// Container-side git credential helper.
///
/// Connects to the host-side cred-server via Unix socket and relays
/// the git credential helper protocol.
use anyhow::{Context, Result};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

/// Run the credential helper: read from stdin, relay to cred-server, write to stdout.
pub async fn run(operation: &str, socket_path: &str) -> Result<()> {
    if operation != "get" {
        // We only handle "get" — store and erase are no-ops
        return Ok(());
    }

    // Read credential request from stdin
    let stdin = tokio::io::stdin();
    let reader = BufReader::new(stdin);
    let mut lines = reader.lines();
    let mut request = String::new();

    while let Some(line) = lines.next_line().await? {
        if line.is_empty() {
            break;
        }
        request.push_str(&line);
        request.push('\n');
    }
    request.push('\n'); // blank line terminates

    // Connect to cred-server
    let stream = UnixStream::connect(socket_path)
        .await
        .with_context(|| format!("connecting to credential server at {socket_path}"))?;

    let (reader, mut writer) = stream.into_split();
    writer.write_all(request.as_bytes()).await?;
    writer.shutdown().await?;

    // Read response and write to stdout
    let reader = BufReader::new(reader);
    let mut lines = reader.lines();
    let mut stdout = tokio::io::stdout();

    while let Some(line) = lines.next_line().await? {
        if line.is_empty() {
            break;
        }
        stdout.write_all(line.as_bytes()).await?;
        stdout.write_all(b"\n").await?;
    }
    stdout.flush().await?;

    Ok(())
}

/// Install the credential helper into git config.
pub fn install() -> Result<()> {
    // Write a gitconfig that uses devp as the credential helper
    let gitconfig_path = dirs_next().join(".gitconfig-devp");
    let content =
        "[credential]\n    helper = !devp credential --socket /devp-sockets/cred.sock get\n"
            .to_string();
    std::fs::write(&gitconfig_path, content)?;

    // Point GIT_CONFIG_GLOBAL to this file (set in docker-compose.yml env)
    eprintln!(
        "[credential] installed git credential helper at {}",
        gitconfig_path.display()
    );
    eprintln!(
        "[credential] ensure GIT_CONFIG_GLOBAL={} is set",
        gitconfig_path.display()
    );

    Ok(())
}

fn dirs_next() -> std::path::PathBuf {
    if let Some(home) = std::env::var_os("HOME") {
        std::path::PathBuf::from(home)
    } else {
        std::path::PathBuf::from("/home/vscode")
    }
}
