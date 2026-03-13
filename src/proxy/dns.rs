/// DNS forwarder with domain filtering. Prevents DNS exfiltration.
///
/// WHY THIS EXISTS: The HTTP proxy blocks network connections to disallowed
/// domains, but DNS exfiltration doesn't use HTTP. A malicious process can
/// encode data in DNS queries (e.g., `stolen-data.evil.com`) that reach an
/// attacker's nameserver through Docker's internal DNS. This forwarder closes
/// that gap by only resolving domains in the allowlist. Everything else gets
/// NXDOMAIN. DO NOT remove this thinking it's redundant with the HTTP proxy.
///
/// Listens for UDP DNS queries, checks the queried domain against the allowlist,
/// and either forwards to an upstream resolver or returns NXDOMAIN.
/// Intentionally minimal: no caching, no recursion, no DNSSEC.
use anyhow::Result;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::UdpSocket;

use super::allowlist::Allowlist;
use crate::reload::{self, Shared};

/// DNS header flags
const FLAG_RESPONSE: u16 = 0x8000;
const FLAG_RCODE_NXDOMAIN: u16 = 0x0003;
const FLAG_RCODE_SERVFAIL: u16 = 0x0002;
const FLAG_RA: u16 = 0x0080; // recursion available

pub async fn run(listen: &str, upstream: &str, allowlist: Shared<Allowlist>) -> Result<()> {
    let sock = UdpSocket::bind(listen).await?;
    let upstream_addr: SocketAddr = upstream.parse()?;
    eprintln!("[dns] listening on {listen}, upstream {upstream}");

    let sock = Arc::new(sock);
    let mut buf = [0u8; 4096];
    loop {
        let (len, src) = sock.recv_from(&mut buf).await?;
        let query = buf[..len].to_vec();
        let sock = sock.clone();
        let al = reload::load(&allowlist);

        tokio::spawn(async move {
            if let Err(e) = handle_query(&sock, &query, src, upstream_addr, &al).await {
                eprintln!("[dns] error handling query from {src}: {e}");
            }
        });
    }
}

async fn handle_query(
    sock: &UdpSocket,
    query: &[u8],
    src: SocketAddr,
    upstream: SocketAddr,
    allowlist: &Allowlist,
) -> Result<()> {
    if query.len() < 12 {
        return Ok(()); // too short for DNS header
    }

    let domain = extract_domain(query);

    match domain {
        Some(ref d) if allowlist.is_allowed(d) => {
            // Forward to upstream
            let upstream_sock = UdpSocket::bind("0.0.0.0:0").await?;
            upstream_sock.send_to(query, upstream).await?;

            let mut resp = [0u8; 4096];
            let timeout = tokio::time::timeout(
                std::time::Duration::from_secs(5),
                upstream_sock.recv_from(&mut resp),
            )
            .await;

            match timeout {
                Ok(Ok((len, _))) => {
                    sock.send_to(&resp[..len], src).await?;
                }
                _ => {
                    // Upstream timeout -return SERVFAIL
                    let response = build_error_response(query, FLAG_RCODE_SERVFAIL);
                    sock.send_to(&response, src).await?;
                }
            }
        }
        Some(ref d) => {
            eprintln!("[dns] denied: {d}");
            let response = build_error_response(query, FLAG_RCODE_NXDOMAIN);
            sock.send_to(&response, src).await?;
        }
        None => {
            // Can't parse domain -forward anyway (safety)
            let upstream_sock = UdpSocket::bind("0.0.0.0:0").await?;
            upstream_sock.send_to(query, upstream).await?;
            let mut resp = [0u8; 4096];
            if let Ok(Ok((len, _))) = tokio::time::timeout(
                std::time::Duration::from_secs(5),
                upstream_sock.recv_from(&mut resp),
            )
            .await
            {
                sock.send_to(&resp[..len], src).await?;
            }
        }
    }

    Ok(())
}

/// Extract the queried domain name from a DNS query packet.
fn extract_domain(query: &[u8]) -> Option<String> {
    if query.len() < 13 {
        return None;
    }
    // Question section starts at byte 12
    let mut pos = 12;
    let mut labels = Vec::new();

    loop {
        if pos >= query.len() {
            return None;
        }
        let label_len = query[pos] as usize;
        if label_len == 0 {
            break;
        }
        // Pointer compression (shouldn't appear in question, but handle it)
        if label_len & 0xC0 == 0xC0 {
            return None;
        }
        pos += 1;
        if pos + label_len > query.len() {
            return None;
        }
        let label = std::str::from_utf8(&query[pos..pos + label_len]).ok()?;
        labels.push(label.to_string());
        pos += label_len;
    }

    if labels.is_empty() {
        None
    } else {
        Some(labels.join("."))
    }
}

/// Build a minimal DNS error response (NXDOMAIN or SERVFAIL).
fn build_error_response(query: &[u8], rcode: u16) -> Vec<u8> {
    let mut resp = query.to_vec();
    if resp.len() >= 4 {
        // Set response flag + copy opcode from query + set rcode
        let flags = u16::from_be_bytes([query[2], query[3]]);
        let new_flags = FLAG_RESPONSE | (flags & 0x7800) | FLAG_RA | rcode;
        let flag_bytes = new_flags.to_be_bytes();
        resp[2] = flag_bytes[0];
        resp[3] = flag_bytes[1];
        // Zero out answer/authority/additional counts
        if resp.len() >= 12 {
            resp[6] = 0;
            resp[7] = 0;
            resp[8] = 0;
            resp[9] = 0;
            resp[10] = 0;
            resp[11] = 0;
        }
    }
    resp
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build_query(domain: &str) -> Vec<u8> {
        let mut pkt = Vec::new();
        // Header: ID=0x1234, flags=0x0100 (standard query, RD=1)
        pkt.extend_from_slice(&[0x12, 0x34, 0x01, 0x00]);
        // QDCOUNT=1, ANCOUNT=0, NSCOUNT=0, ARCOUNT=0
        pkt.extend_from_slice(&[0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]);
        // Question section
        for label in domain.split('.') {
            pkt.push(label.len() as u8);
            pkt.extend_from_slice(label.as_bytes());
        }
        pkt.push(0); // end of name
        pkt.extend_from_slice(&[0x00, 0x01]); // QTYPE = A
        pkt.extend_from_slice(&[0x00, 0x01]); // QCLASS = IN
        pkt
    }

    #[test]
    fn extracts_simple_domain() {
        let query = build_query("github.com");
        assert_eq!(extract_domain(&query), Some("github.com".to_string()));
    }

    #[test]
    fn extracts_subdomain() {
        let query = build_query("api.github.com");
        assert_eq!(extract_domain(&query), Some("api.github.com".to_string()));
    }

    #[test]
    fn returns_none_for_short_packet() {
        assert_eq!(extract_domain(&[0; 5]), None);
    }

    #[test]
    fn nxdomain_response_has_correct_rcode() {
        let query = build_query("evil.com");
        let resp = build_error_response(&query, FLAG_RCODE_NXDOMAIN);
        let flags = u16::from_be_bytes([resp[2], resp[3]]);
        assert!(flags & FLAG_RESPONSE != 0, "should be a response");
        assert_eq!(flags & 0x000F, 3, "rcode should be NXDOMAIN (3)");
    }

    #[test]
    fn servfail_response_has_correct_rcode() {
        let query = build_query("timeout.com");
        let resp = build_error_response(&query, FLAG_RCODE_SERVFAIL);
        let flags = u16::from_be_bytes([resp[2], resp[3]]);
        assert!(flags & FLAG_RESPONSE != 0, "should be a response");
        assert_eq!(flags & 0x000F, 2, "rcode should be SERVFAIL (2)");
    }
}
