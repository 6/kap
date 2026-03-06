pub mod allowlist;
pub mod log;

use std::sync::Arc;

use anyhow::Result;
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response};
use hyper_util::rt::TokioIo;
use tokio::net::{TcpListener, TcpStream};

use crate::config::Config;
use allowlist::Allowlist;
use log::{ProxyLogEntry, ProxyLogger};

struct ProxyState {
    allowlist: Allowlist,
    logger: ProxyLogger,
    observe: bool,
}

pub async fn run(config: Config, observe: bool) -> Result<()> {
    let allow_domains = config.allow_domains();
    let deny_domains = &config.proxy.network.deny;

    let allowlist = Allowlist::new(allow_domains, deny_domains);
    let logger = ProxyLogger::new(&config.proxy.observe.log);

    let state = Arc::new(ProxyState {
        allowlist,
        logger,
        observe,
    });

    let listen_addr = &config.proxy.listen;
    let listener = TcpListener::bind(listen_addr).await?;
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
                "Denied by devg: {domain} is not in the allowlist\n"
            ))))
            .unwrap());
    }

    eprintln!("[proxy] CONNECT {host}");

    // Establish tunnel
    tokio::task::spawn(async move {
        match hyper::upgrade::on(req).await {
            Ok(upgraded) => {
                let mut upgraded = TokioIo::new(upgraded);
                match TcpStream::connect(&host).await {
                    Ok(mut target) => {
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
                "Denied by devg: {host} is not in the allowlist\n"
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
        drop(listener);

        let mut config = Config::default();
        config.proxy.listen = format!("127.0.0.1:{port}");
        config.proxy.network.allow = allow.iter().map(|s| s.to_string()).collect();
        config.proxy.network.deny = deny.iter().map(|s| s.to_string()).collect();
        config.proxy.observe.log = "/dev/null".to_string();

        tokio::spawn(async move {
            let _ = run(config, observe).await;
        });

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
}
