pub mod allowlist;
pub mod dns;
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
    let allow_domains = config.resolved_allow_domains();
    let deny_domains = config.proxy.network.deny.clone();

    let allowlist = Allowlist::new(&allow_domains, &deny_domains);
    let logger = ProxyLogger::new(&config.proxy.observe.log);

    let state = Arc::new(ProxyState {
        allowlist,
        logger,
        observe,
    });

    let listen_addr = &config.proxy.listen;
    let dns_listen = config.proxy.dns_listen.clone();
    let dns_upstream = config.proxy.dns_upstream.clone();

    // Start DNS forwarder in background
    let dns_allowlist = Arc::new(Allowlist::new(&allow_domains, &deny_domains));
    tokio::spawn(async move {
        if let Err(e) = dns::run(&dns_listen, &dns_upstream, dns_allowlist).await {
            eprintln!("[dns] fatal: {e}");
        }
    });

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
                "Denied by devp: {domain} is not in the allowlist\n"
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
                "Denied by devp: {host} is not in the allowlist\n"
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
