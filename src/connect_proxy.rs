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
    let mut stream = reader.into_inner();

    // Bridge stdin/stdout <-> proxy socket
    let mut stream_clone = stream.try_clone().context("cloning socket")?;

    let t1 = std::thread::spawn(move || -> Result<()> {
        let mut stdin = std::io::stdin().lock();
        std::io::copy(&mut stdin, &mut stream_clone)?;
        stream_clone.shutdown(std::net::Shutdown::Write).ok();
        Ok(())
    });

    // Write directly to fd 1 to avoid Stdout's internal buffering.
    // SSH ProxyCommand needs data forwarded immediately — buffered
    // stdout would stall the SSH handshake.
    let mut stdout = unsafe { std::fs::File::from_raw_fd(1) };
    if !buffered.is_empty() {
        stdout.write_all(&buffered)?;
    }
    std::io::copy(&mut stream, &mut stdout)?;

    t1.join()
        .map_err(|_| anyhow::anyhow!("stdin thread panicked"))??;
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
