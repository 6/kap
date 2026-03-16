/// Minimal TCP-over-HTTP-CONNECT bridge for SSH ProxyCommand.
///
/// Used as: `ssh -o ProxyCommand='/opt/kap/kap sidecar-connect-proxy %h %p'`
///
/// Reads HTTPS_PROXY to find the kap sidecar proxy, sends a CONNECT request,
/// then bridges stdin/stdout with the tunnel.
use anyhow::{Context, Result, bail};
use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
#[cfg(unix)]
use std::os::fd::FromRawFd;

pub fn run(host: &str, port: u16) -> Result<()> {
    let proxy_addr = proxy_addr_from_env()?;
    let target = format!("{host}:{port}");

    let mut stream = TcpStream::connect(&proxy_addr)
        .with_context(|| format!("connecting to proxy {proxy_addr}"))?;

    // Send CONNECT request
    write!(
        stream,
        "CONNECT {target} HTTP/1.1\r\nHost: {target}\r\n\r\n"
    )?;
    stream.flush()?;

    // Read response status line
    let mut reader = BufReader::new(stream);
    let mut status_line = String::new();
    reader
        .read_line(&mut status_line)
        .context("reading CONNECT response")?;

    if !status_line.contains("200") {
        bail!("CONNECT failed: {}", status_line.trim());
    }

    // Drain remaining headers
    loop {
        let mut line = String::new();
        reader.read_line(&mut line)?;
        if line.trim().is_empty() {
            break;
        }
    }

    // Flush any data the BufReader already consumed past the headers
    // (e.g. SSH banner that arrived with the HTTP response).
    let buffered = reader.buffer().to_vec();
    let stream = reader.into_inner();

    // Bridge stdin → proxy using a separate fd (dup'd) so that
    // the read side stays independent.
    let stdin_fd = {
        let raw = std::os::fd::AsRawFd::as_raw_fd(&stream);
        let duped = unsafe { libc::dup(raw) };
        if duped < 0 {
            anyhow::bail!("dup() failed");
        }
        unsafe { std::fs::File::from_raw_fd(duped) }
    };

    let t1 = std::thread::spawn(move || {
        let mut stdin = std::io::stdin().lock();
        let mut writer = stdin_fd;
        let _ = std::io::copy(&mut stdin, &mut writer);
    });

    // Write directly to fd 1 to avoid Stdout's internal buffering.
    // SSH ProxyCommand needs data forwarded immediately — buffered
    // stdout would stall the SSH handshake.
    let mut stdout = unsafe { std::fs::File::from_raw_fd(1) };
    if !buffered.is_empty() {
        stdout.write_all(&buffered)?;
    }

    // Manual copy loop: read from stream, write+flush to stdout.
    // std::io::copy may not flush between reads, stalling SSH.
    let mut buf = [0u8; 8192];
    loop {
        let n = std::io::Read::read(&mut &stream, &mut buf)?;
        if n == 0 {
            break;
        }
        stdout.write_all(&buf[..n])?;
    }

    t1.join()
        .map_err(|_| anyhow::anyhow!("stdin thread panicked"))?;
    Ok(())
}

fn proxy_addr_from_env() -> Result<String> {
    let url = std::env::var("HTTPS_PROXY")
        .or_else(|_| std::env::var("https_proxy"))
        .context("HTTPS_PROXY not set — not running inside a kap container?")?;

    Ok(parse_proxy_url(&url).to_string())
}

/// Parse "http://172.28.0.3:3128" -> "172.28.0.3:3128"
fn parse_proxy_url(url: &str) -> &str {
    let addr = url
        .strip_prefix("http://")
        .or_else(|| url.strip_prefix("https://"))
        .unwrap_or(url);
    addr.trim_end_matches('/')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_proxy_addr() {
        assert_eq!(parse_proxy_url("http://172.28.0.3:3128"), "172.28.0.3:3128");
        assert_eq!(parse_proxy_url("https://10.0.0.1:3128/"), "10.0.0.1:3128");
        assert_eq!(parse_proxy_url("10.0.0.1:3128"), "10.0.0.1:3128");
    }
}
