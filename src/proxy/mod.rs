pub mod allowlist;
pub mod dns;
pub mod log;
pub mod sni;

use std::sync::Arc;

use anyhow::Result;
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response};
use hyper_util::rt::TokioIo;
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream};

use crate::config::Config;
use allowlist::Allowlist;
use log::{ProxyLogEntry, ProxyLogger};

struct ProxyState {
    allowlist: Arc<Allowlist>,
    logger: ProxyLogger,
    observe: bool,
}

pub async fn run(config: Config, observe: bool, allowlist: Arc<Allowlist>) -> Result<()> {
    let listener = TcpListener::bind(&config.proxy.listen).await?;
    run_with_listener(config, observe, allowlist, listener).await
}

async fn run_with_listener(
    config: Config,
    observe: bool,
    allowlist: Arc<Allowlist>,
    listener: TcpListener,
) -> Result<()> {
    let logger = ProxyLogger::new(&config.proxy.observe.log);

    let state = Arc::new(ProxyState {
        allowlist,
        logger,
        observe,
    });

    let listen_addr = listener.local_addr()?;
    eprintln!("[proxy] listening on {listen_addr}");
    if observe {
        eprintln!("[proxy] OBSERVE MODE: all traffic allowed, logging domains");
    }

    loop {
        let (stream, addr) = listener.accept().await?;
        let state = state.clone();

        tokio::spawn(async move {
            let io = TokioIo::new(stream);
            let service = service_fn(move |req| {
                let state = state.clone();
                async move { handle_request(req, &state, addr.to_string()).await }
            });

            #[allow(clippy::collapsible_if)]
            if let Err(e) = http1::Builder::new()
                .preserve_header_case(true)
                .title_case_headers(true)
                .serve_connection(io, service)
                .with_upgrades()
                .await
            {
                if !e.to_string().contains("error shutting down connection") {
                    eprintln!("[proxy] connection error from {addr}: {e}");
                }
            }
        });
    }
}

async fn handle_request(
    req: Request<Incoming>,
    state: &ProxyState,
    _client: String,
) -> Result<Response<Full<Bytes>>, hyper::Error> {
    if req.method() == Method::CONNECT {
        handle_connect(req, state).await
    } else {
        handle_http(req, state).await
    }
}

/// Handle HTTPS CONNECT tunneling.
async fn handle_connect(
    req: Request<Incoming>,
    state: &ProxyState,
) -> Result<Response<Full<Bytes>>, hyper::Error> {
    let host = req
        .uri()
        .authority()
        .map(|a| a.to_string())
        .unwrap_or_default();
    let domain = host.split(':').next().unwrap_or(&host);

    let allowed = state.observe || state.allowlist.is_allowed(&host);
    let action = if state.observe {
        "observed"
    } else if allowed {
        "allowed"
    } else {
        "denied"
    };

    let entry = ProxyLogEntry::new(domain, action, "CONNECT");
    let _ = state.logger.log(&entry).await;

    if !allowed {
        eprintln!("[proxy] DENIED CONNECT {host}");
        return Ok(Response::builder()
            .status(403)
            .body(Full::new(Bytes::from(format!(
                "Denied by kap: {domain} is not in the allowlist\n"
            ))))
            .unwrap());
    }

    eprintln!("[proxy] CONNECT {host}");

    // Establish tunnel with SNI validation
    let connect_domain = domain.to_string();
    let logger = state.logger.clone();
    let observe = state.observe;
    tokio::task::spawn(async move {
        match hyper::upgrade::on(req).await {
            Ok(upgraded) => {
                let mut upgraded = TokioIo::new(upgraded);
                match TcpStream::connect(&host).await {
                    Ok(mut target) => {
                        // Read the first chunk from the client (the TLS ClientHello)
                        let mut peek_buf = vec![0u8; 4096];
                        match tokio::io::AsyncReadExt::read(&mut upgraded, &mut peek_buf).await {
                            Ok(0) => return,
                            Ok(n) => {
                                let data = &peek_buf[..n];
                                if let Some(sni_host) = sni::extract_sni(data)
                                    && !sni::sni_matches_connect_domain(&sni_host, &connect_domain)
                                {
                                    eprintln!(
                                        "[proxy] SNI mismatch: CONNECT domain={connect_domain}, SNI={sni_host}"
                                    );
                                    if !observe {
                                        let entry = ProxyLogEntry::new(
                                            &connect_domain,
                                            "denied",
                                            &format!("SNI mismatch: {sni_host}"),
                                        );
                                        let _ = logger.log(&entry).await;
                                        return; // drop the connection
                                    }
                                }
                                // Forward the buffered bytes to upstream
                                if target.write_all(data).await.is_err() {
                                    return;
                                }
                            }
                            Err(_) => return,
                        }
                        let _ = tokio::io::copy_bidirectional(&mut upgraded, &mut target).await;
                    }
                    Err(e) => {
                        eprintln!("[proxy] failed to connect to {host}: {e}");
                    }
                }
            }
            Err(e) => {
                eprintln!("[proxy] upgrade failed for {host}: {e}");
            }
        }
    });

    Ok(Response::new(Full::new(Bytes::new())))
}

/// Handle plain HTTP requests (non-CONNECT).
async fn handle_http(
    req: Request<Incoming>,
    state: &ProxyState,
) -> Result<Response<Full<Bytes>>, hyper::Error> {
    let uri = req.uri().clone();
    let host = uri.host().map(|h| h.to_string()).unwrap_or_default();
    let method = req.method().clone();

    let allowed = state.observe || state.allowlist.is_allowed(&host);
    let action = if state.observe {
        "observed"
    } else if allowed {
        "allowed"
    } else {
        "denied"
    };

    let entry = ProxyLogEntry::new(&host, action, method.as_str());
    let _ = state.logger.log(&entry).await;

    if !allowed {
        eprintln!("[proxy] DENIED {method} {uri}");
        return Ok(Response::builder()
            .status(403)
            .body(Full::new(Bytes::from(format!(
                "Denied by kap: {host} is not in the allowlist\n"
            ))))
            .unwrap());
    }

    eprintln!("[proxy] {method} {uri}");

    // Forward the request
    let port = uri.port_u16().unwrap_or(80);
    let addr = format!("{host}:{port}");

    match TcpStream::connect(&addr).await {
        Ok(stream) => {
            let io = TokioIo::new(stream);
            let (mut sender, conn): (hyper::client::conn::http1::SendRequest<Full<Bytes>>, _) =
                match hyper::client::conn::http1::handshake(io).await {
                    Ok(pair) => pair,
                    Err(e) => {
                        eprintln!("[proxy] handshake error for {addr}: {e}");
                        return Ok(Response::builder()
                            .status(502)
                            .body(Full::new(Bytes::from("Bad Gateway\n")))
                            .unwrap());
                    }
                };
            tokio::spawn(conn);

            // Collect the incoming body
            let body_bytes = req.into_body().collect().await?.to_bytes();

            // Build forwarded request with just path+query (not full URI)
            let path = uri.path_and_query().map(|pq| pq.as_str()).unwrap_or("/");
            let proxy_req = Request::builder()
                .method(method)
                .uri(path)
                .header("Host", &host)
                .body(Full::new(body_bytes))
                .unwrap();

            match sender.send_request(proxy_req).await {
                Ok(resp) => {
                    let (parts, body) = resp.into_parts();
                    let body_bytes = body.collect().await?.to_bytes();
                    Ok(Response::from_parts(parts, Full::new(body_bytes)))
                }
                Err(e) => {
                    eprintln!("[proxy] upstream error for {addr}: {e}");
                    Ok(Response::builder()
                        .status(502)
                        .body(Full::new(Bytes::from("Bad Gateway\n")))
                        .unwrap())
                }
            }
        }
        Err(e) => {
            eprintln!("[proxy] connect error for {addr}: {e}");
            Ok(Response::builder()
                .status(502)
                .body(Full::new(Bytes::from(format!("Bad Gateway: {e}\n"))))
                .unwrap())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    async fn start_proxy(allow: &[&str], deny: &[&str], observe: bool) -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let mut config = Config::default();
        config.proxy.listen = format!("127.0.0.1:{port}");
        config.proxy.network.allow = allow.iter().map(|s| s.to_string()).collect();
        config.proxy.network.deny = deny.iter().map(|s| s.to_string()).collect();
        config.proxy.observe.log = "/dev/null".to_string();

        let allowlist = Arc::new(Allowlist::new(
            &config.proxy.network.allow,
            &config.proxy.network.deny,
        ));

        tokio::spawn(async move {
            let _ = run_with_listener(config, observe, allowlist, listener).await;
        });

        // Listener is already bound, just wait for accept loop to start
        for _ in 0..100 {
            if TcpStream::connect(format!("127.0.0.1:{port}"))
                .await
                .is_ok()
            {
                return port;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("proxy did not start");
    }

    async fn raw_request(port: u16, req: &str) -> String {
        let mut stream = TcpStream::connect(format!("127.0.0.1:{port}"))
            .await
            .unwrap();
        stream.write_all(req.as_bytes()).await.unwrap();
        let mut buf = vec![0u8; 4096];
        let n = tokio::time::timeout(std::time::Duration::from_secs(5), stream.read(&mut buf))
            .await
            .expect("read timed out")
            .unwrap();
        String::from_utf8_lossy(&buf[..n]).to_string()
    }

    #[tokio::test]
    async fn denies_http_to_unlisted_domain() {
        let port = start_proxy(&["allowed.test"], &[], false).await;
        let resp = raw_request(
            port,
            "GET http://denied.test/ HTTP/1.1\r\nHost: denied.test\r\n\r\n",
        )
        .await;
        assert!(resp.contains("403"), "expected 403, got: {resp}");
        assert!(resp.contains("denied.test"));
    }

    #[tokio::test]
    async fn allows_http_to_listed_domain() {
        let port = start_proxy(&["127.0.0.1"], &[], false).await;
        // Port 1 is closed, so proxy will allow but get connection refused → 502
        let resp = raw_request(
            port,
            "GET http://127.0.0.1:1/test HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n",
        )
        .await;
        assert!(!resp.contains("403"), "should not be denied, got: {resp}");
        assert!(
            resp.contains("502"),
            "expected 502 Bad Gateway, got: {resp}"
        );
    }

    #[tokio::test]
    async fn denies_connect_to_unlisted_domain() {
        let port = start_proxy(&["allowed.test"], &[], false).await;
        let resp = raw_request(
            port,
            "CONNECT denied.test:443 HTTP/1.1\r\nHost: denied.test:443\r\n\r\n",
        )
        .await;
        assert!(resp.contains("403"), "expected 403, got: {resp}");
    }

    #[tokio::test]
    async fn allows_connect_to_listed_domain() {
        let port = start_proxy(&["allowed.test"], &[], false).await;
        let resp = raw_request(
            port,
            "CONNECT allowed.test:443 HTTP/1.1\r\nHost: allowed.test:443\r\n\r\n",
        )
        .await;
        assert!(resp.contains("200"), "expected 200, got: {resp}");
    }

    #[tokio::test]
    async fn deny_overrides_allow_in_proxy() {
        let port = start_proxy(&["*.example.com"], &["blocked.example.com"], false).await;
        let resp = raw_request(
            port,
            "GET http://blocked.example.com/ HTTP/1.1\r\nHost: blocked.example.com\r\n\r\n",
        )
        .await;
        assert!(
            resp.contains("403"),
            "deny should override allow, got: {resp}"
        );
    }

    #[tokio::test]
    async fn observe_mode_allows_all() {
        let port = start_proxy(&[], &[], true).await;
        let resp = raw_request(
            port,
            "CONNECT anything.test:443 HTTP/1.1\r\nHost: anything.test:443\r\n\r\n",
        )
        .await;
        assert!(
            resp.contains("200"),
            "observe mode should allow all, got: {resp}"
        );
    }

    #[tokio::test]
    async fn connect_without_port_returns_response() {
        let port = start_proxy(&["allowed.test"], &[], false).await;
        let resp = raw_request(port, "CONNECT noport HTTP/1.1\r\nHost: noport\r\n\r\n").await;
        // "noport" is not in the allowlist, so should be denied
        assert!(resp.contains("403"), "expected 403, got: {resp}");
    }

    #[tokio::test]
    async fn http_empty_host_denied() {
        let port = start_proxy(&["allowed.test"], &[], false).await;
        let resp = raw_request(port, "GET / HTTP/1.1\r\nHost: \r\n\r\n").await;
        assert!(
            resp.contains("403"),
            "empty host should be denied, got: {resp}"
        );
    }

    #[tokio::test]
    async fn observe_mode_allows_denied_http() {
        let port = start_proxy(&[], &[], true).await;
        // HTTP to a domain with port 1 (closed) — should be allowed through (not 403)
        let resp = raw_request(
            port,
            "GET http://unlisted.test:1/path HTTP/1.1\r\nHost: unlisted.test\r\n\r\n",
        )
        .await;
        assert!(
            !resp.contains("403"),
            "observe mode should not deny HTTP, got: {resp}"
        );
        // Expect 502 since the upstream is unreachable
        assert!(
            resp.contains("502"),
            "expected 502 Bad Gateway, got: {resp}"
        );
    }

    #[tokio::test]
    async fn deny_overrides_allow_for_connect() {
        let port = start_proxy(&["*.example.com"], &["blocked.example.com"], false).await;
        let resp = raw_request(
            port,
            "CONNECT blocked.example.com:443 HTTP/1.1\r\nHost: blocked.example.com:443\r\n\r\n",
        )
        .await;
        assert!(
            resp.contains("403"),
            "deny should override allow for CONNECT, got: {resp}"
        );
    }

    #[tokio::test]
    async fn sni_mismatch_drops_tunnel() {
        // Start a mock upstream that the proxy will CONNECT to
        let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream_port = upstream.local_addr().unwrap().port();
        let upstream_addr = format!("127.0.0.1:{upstream_port}");

        // Track whether the upstream received any data
        let received = Arc::new(tokio::sync::Notify::new());
        let received_clone = received.clone();
        tokio::spawn(async move {
            if let Ok((mut conn, _)) = upstream.accept().await {
                let mut buf = [0u8; 1];
                // If we receive any byte, notify
                if tokio::io::AsyncReadExt::read(&mut conn, &mut buf)
                    .await
                    .unwrap_or(0)
                    > 0
                {
                    received_clone.notify_one();
                }
            }
        });

        let port = start_proxy(&["127.0.0.1"], &[], false).await;

        // Send CONNECT to the upstream address
        let mut stream = TcpStream::connect(format!("127.0.0.1:{port}"))
            .await
            .unwrap();
        let connect_req =
            format!("CONNECT {upstream_addr} HTTP/1.1\r\nHost: {upstream_addr}\r\n\r\n");
        stream.write_all(connect_req.as_bytes()).await.unwrap();

        // Read the 200 response
        let mut buf = vec![0u8; 4096];
        let n = tokio::time::timeout(std::time::Duration::from_secs(5), stream.read(&mut buf))
            .await
            .expect("read timed out")
            .unwrap();
        let resp = String::from_utf8_lossy(&buf[..n]);
        assert!(resp.contains("200"), "expected 200, got: {resp}");

        // Now send a TLS ClientHello with SNI "evil.com" (mismatched)
        let client_hello = sni::tests::build_client_hello("evil.com");
        stream.write_all(&client_hello).await.unwrap();

        // The proxy should drop the tunnel. Verify by trying to read — should get EOF.
        let result =
            tokio::time::timeout(std::time::Duration::from_secs(2), stream.read(&mut buf)).await;
        match result {
            Ok(Ok(0)) | Err(_) => {} // EOF or timeout — proxy dropped the connection
            Ok(Ok(n)) => {
                // Might get a close, that's fine too
                let _ = n;
            }
            Ok(Err(_)) => {} // connection reset — also fine
        }

        // Verify the upstream did NOT receive the ClientHello data
        let was_forwarded =
            tokio::time::timeout(std::time::Duration::from_millis(500), received.notified()).await;
        assert!(
            was_forwarded.is_err(),
            "upstream should NOT have received data when SNI mismatches"
        );
    }

    #[tokio::test]
    async fn sni_match_allows_tunnel() {
        // Start a mock upstream
        let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream_port = upstream.local_addr().unwrap().port();
        let upstream_addr = format!("127.0.0.1:{upstream_port}");

        let received = Arc::new(tokio::sync::Notify::new());
        let received_clone = received.clone();
        tokio::spawn(async move {
            if let Ok((mut conn, _)) = upstream.accept().await {
                let mut buf = [0u8; 1];
                if tokio::io::AsyncReadExt::read(&mut conn, &mut buf)
                    .await
                    .unwrap_or(0)
                    > 0
                {
                    received_clone.notify_one();
                }
            }
        });

        let port = start_proxy(&["127.0.0.1"], &[], false).await;

        let mut stream = TcpStream::connect(format!("127.0.0.1:{port}"))
            .await
            .unwrap();
        let connect_req =
            format!("CONNECT {upstream_addr} HTTP/1.1\r\nHost: {upstream_addr}\r\n\r\n");
        stream.write_all(connect_req.as_bytes()).await.unwrap();

        let mut buf = vec![0u8; 4096];
        let n = tokio::time::timeout(std::time::Duration::from_secs(5), stream.read(&mut buf))
            .await
            .expect("read timed out")
            .unwrap();
        let resp = String::from_utf8_lossy(&buf[..n]);
        assert!(resp.contains("200"), "expected 200, got: {resp}");

        // Send a ClientHello with SNI matching the CONNECT domain
        let client_hello = sni::tests::build_client_hello("127.0.0.1");
        stream.write_all(&client_hello).await.unwrap();

        // The upstream SHOULD receive the data
        let was_forwarded =
            tokio::time::timeout(std::time::Duration::from_secs(2), received.notified()).await;
        assert!(
            was_forwarded.is_ok(),
            "upstream should have received data when SNI matches"
        );
    }

    #[tokio::test]
    async fn http_port_defaults_to_80() {
        let port = start_proxy(&["127.0.0.1"], &[], false).await;
        // No port in URI — defaults to 80, which is likely closed → 502
        let resp = raw_request(
            port,
            "GET http://127.0.0.1/test HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n",
        )
        .await;
        assert!(!resp.contains("403"), "should not be denied, got: {resp}");
        assert!(
            resp.contains("502"),
            "expected 502 for closed port 80, got: {resp}"
        );
    }
}
