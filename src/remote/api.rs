use std::sync::Arc;

use anyhow::Result;
use bytes::Bytes;
use http_body_util::Full;
use hyper::{Request, Response, StatusCode};
use serde::Serialize;

use super::RemoteState;
use crate::remote::containers;

type Body = Full<Bytes>;

pub async fn handle(
    req: &Request<hyper::body::Incoming>,
    state: &Arc<RemoteState>,
) -> Result<Response<Body>> {
    let path = req.uri().path();
    let method = req.method();

    match (method, path) {
        (&hyper::Method::GET, "/api/status") => handle_status(state).await,
        (&hyper::Method::GET, "/api/logs") => handle_logs(req, state).await,
        (&hyper::Method::GET, "/api/logs/denied") => handle_logs_denied(state).await,
        (&hyper::Method::POST, p) if p.starts_with("/api/pair") => handle_pair(req, state).await,
        _ => Ok(json_response(
            StatusCode::NOT_FOUND,
            &ErrorBody { error: "not found" },
        )),
    }
}

#[derive(Serialize)]
struct ErrorBody {
    error: &'static str,
}

#[derive(Serialize)]
struct StatusResponse {
    containers: ContainerStatus,
    proxy: ProxyStatus,
}

#[derive(Serialize)]
struct ContainerStatus {
    app: Option<ContainerInfo>,
    sidecar: Option<ContainerInfo>,
}

#[derive(Serialize)]
struct ContainerInfo {
    name: String,
    status: String,
}

#[derive(Serialize)]
struct ProxyStatus {
    listening: bool,
    denied_count: u64,
}

async fn handle_status(state: &Arc<RemoteState>) -> Result<Response<Body>> {
    let (app_name, sidecar_name) = match containers::find_containers() {
        Ok(pair) => pair,
        Err(_) => {
            let status = StatusResponse {
                containers: ContainerStatus {
                    app: None,
                    sidecar: None,
                },
                proxy: ProxyStatus {
                    listening: false,
                    denied_count: 0,
                },
            };
            return Ok(json_response(StatusCode::OK, &status));
        }
    };

    // Check if proxy is reachable
    let proxy_up = containers::exec_exit_code(
        &app_name,
        &["bash", "-c", "echo > /dev/tcp/172.28.0.3/3128"],
    ) == 0;

    // Get denied count from sidecar
    let denied_count = containers::exec_in(
        &sidecar_name,
        &[
            "sh",
            "-c",
            "grep -c '\"denied\"' /var/log/devg/proxy.jsonl 2>/dev/null || echo 0",
        ],
    )
    .and_then(|s| s.trim().parse::<u64>().ok())
    .unwrap_or(0);

    let _ = state; // state will be used more in later phases

    let status = StatusResponse {
        containers: ContainerStatus {
            app: Some(ContainerInfo {
                name: app_name,
                status: "running".to_string(),
            }),
            sidecar: Some(ContainerInfo {
                name: sidecar_name,
                status: "running".to_string(),
            }),
        },
        proxy: ProxyStatus {
            listening: proxy_up,
            denied_count,
        },
    };

    Ok(json_response(StatusCode::OK, &status))
}

async fn handle_logs(
    req: &Request<hyper::body::Incoming>,
    _state: &Arc<RemoteState>,
) -> Result<Response<Body>> {
    let query = req.uri().query().unwrap_or("");
    let limit = parse_query_param(query, "limit").unwrap_or(100);

    let (_app, sidecar) = match containers::find_containers() {
        Ok(pair) => pair,
        Err(_) => {
            return Ok(json_response(
                StatusCode::OK,
                &Vec::<serde_json::Value>::new(),
            ));
        }
    };

    let raw =
        containers::exec_in(&sidecar, &["cat", "/var/log/devg/proxy.jsonl"]).unwrap_or_default();

    let entries: Vec<serde_json::Value> = raw
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .take(limit)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();

    Ok(json_response(StatusCode::OK, &entries))
}

async fn handle_logs_denied(_state: &Arc<RemoteState>) -> Result<Response<Body>> {
    let (_app, sidecar) = match containers::find_containers() {
        Ok(pair) => pair,
        Err(_) => {
            return Ok(json_response(
                StatusCode::OK,
                &Vec::<serde_json::Value>::new(),
            ));
        }
    };

    let raw =
        containers::exec_in(&sidecar, &["cat", "/var/log/devg/proxy.jsonl"]).unwrap_or_default();

    let entries: Vec<serde_json::Value> = raw
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter(|v| v.get("action").and_then(|a| a.as_str()) == Some("denied"))
        .collect();

    Ok(json_response(StatusCode::OK, &entries))
}

#[derive(Serialize)]
struct PairResponse {
    session_token: String,
    device_id: String,
}

async fn handle_pair(
    _req: &Request<hyper::body::Incoming>,
    state: &Arc<RemoteState>,
) -> Result<Response<Body>> {
    // The auth middleware already validated this is a pairing token.
    // Issue a session token and pair the device.
    let device_name = "iPhone"; // TODO: parse from request body
    let session_token = super::auth::pair_device(&state.data_dir, device_name)?;

    let devices = super::auth::load_devices(&state.data_dir);
    let device_id = devices.last().map(|d| d.id.clone()).unwrap_or_default();

    Ok(json_response(
        StatusCode::OK,
        &PairResponse {
            session_token,
            device_id,
        },
    ))
}

fn json_response<T: Serialize>(status: StatusCode, body: &T) -> Response<Body> {
    let json = serde_json::to_string(body).unwrap_or_else(|_| "{}".to_string());
    Response::builder()
        .status(status)
        .header("Content-Type", "application/json")
        .body(Full::new(Bytes::from(json)))
        .unwrap()
}

fn parse_query_param(query: &str, key: &str) -> Option<usize> {
    query.split('&').find_map(|pair| {
        let (k, v) = pair.split_once('=')?;
        if k == key { v.parse().ok() } else { None }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_query_param_works() {
        assert_eq!(parse_query_param("limit=50&after=123", "limit"), Some(50));
        assert_eq!(parse_query_param("limit=50&after=123", "after"), Some(123));
        assert_eq!(parse_query_param("limit=50", "missing"), None);
        assert_eq!(parse_query_param("", "limit"), None);
    }
}
