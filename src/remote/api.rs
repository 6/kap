use std::sync::Arc;

use anyhow::Result;
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::{Request, Response, StatusCode};
use serde::{Deserialize, Serialize};

use super::RemoteState;
use crate::remote::{agent, containers};

type Body = Full<Bytes>;

pub async fn handle(
    req: Request<hyper::body::Incoming>,
    state: &Arc<RemoteState>,
) -> Result<Response<Body>> {
    let path = req.uri().path().to_string();
    let method = req.method().clone();

    match (method, path.as_str()) {
        (hyper::Method::GET, "/api/status") => handle_status(state).await,
        (hyper::Method::GET, "/api/logs") => handle_logs(&req, state).await,
        (hyper::Method::GET, "/api/logs/denied") => handle_logs_denied(state).await,
        (hyper::Method::POST, "/api/pair") => handle_pair(state).await,
        (hyper::Method::GET, "/api/agent/sessions") => handle_agent_sessions(state).await,
        (hyper::Method::GET, p) if p.starts_with("/api/agent/session/") => {
            let rest = &p["/api/agent/session/".len()..];
            if let Some(id) = rest.strip_suffix("/diff") {
                handle_agent_diff(id, state).await
            } else {
                handle_agent_session(rest, state).await
            }
        }
        (hyper::Method::POST, p) if p.starts_with("/api/agent/session/") => {
            let rest = p["/api/agent/session/".len()..].to_string();
            if let Some(id) = rest.strip_suffix("/cancel") {
                handle_agent_cancel(id, state).await
            } else if let Some(id) = rest.strip_suffix("/message") {
                handle_agent_message(req, id, state).await
            } else {
                Ok(json_response(
                    StatusCode::NOT_FOUND,
                    &ErrorBody { error: "not found" },
                ))
            }
        }
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

async fn handle_pair(state: &Arc<RemoteState>) -> Result<Response<Body>> {
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

async fn handle_agent_sessions(_state: &Arc<RemoteState>) -> Result<Response<Body>> {
    let (app, _sidecar) = match containers::find_containers() {
        Ok(pair) => pair,
        Err(_) => {
            return Ok(json_response(
                StatusCode::OK,
                &Vec::<agent::SessionInfo>::new(),
            ));
        }
    };

    let sessions = agent::discover_sessions(&app).unwrap_or_default();
    Ok(json_response(StatusCode::OK, &sessions))
}

async fn handle_agent_session(
    session_id: &str,
    _state: &Arc<RemoteState>,
) -> Result<Response<Body>> {
    let (app, _sidecar) = match containers::find_containers() {
        Ok(pair) => pair,
        Err(_) => {
            return Ok(json_response(
                StatusCode::SERVICE_UNAVAILABLE,
                &ErrorBody {
                    error: "no containers running",
                },
            ));
        }
    };

    match agent::read_session(&app, session_id) {
        Ok(events) => Ok(json_response(StatusCode::OK, &events)),
        Err(e) => Ok(json_response(
            StatusCode::NOT_FOUND,
            &ErrorBodyOwned {
                error: e.to_string(),
            },
        )),
    }
}

async fn handle_agent_diff(session_id: &str, _state: &Arc<RemoteState>) -> Result<Response<Body>> {
    let (app, _sidecar) = match containers::find_containers() {
        Ok(pair) => pair,
        Err(_) => {
            return Ok(json_response(
                StatusCode::SERVICE_UNAVAILABLE,
                &ErrorBody {
                    error: "no containers running",
                },
            ));
        }
    };

    let _ = session_id; // diff is repo-wide, not session-specific
    match agent::get_diff(&app) {
        Ok(diff) => Ok(json_response(StatusCode::OK, &DiffResponse { diff })),
        Err(e) => Ok(json_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            &ErrorBodyOwned {
                error: e.to_string(),
            },
        )),
    }
}

#[derive(Serialize)]
struct DiffResponse {
    diff: String,
}

async fn handle_agent_cancel(
    session_id: &str,
    _state: &Arc<RemoteState>,
) -> Result<Response<Body>> {
    let (app, _sidecar) = match containers::find_containers() {
        Ok(pair) => pair,
        Err(_) => {
            return Ok(json_response(
                StatusCode::SERVICE_UNAVAILABLE,
                &ErrorBody {
                    error: "no containers running",
                },
            ));
        }
    };

    let _ = session_id; // cancel kills any running claude process
    let pid = agent::is_agent_running(&app);
    match pid {
        Some(_) => {
            let exit = containers::exec_exit_code(&app, &["pkill", "-INT", "-f", "claude"]);
            if exit == 0 {
                Ok(json_response(
                    StatusCode::OK,
                    &CancelResponse {
                        cancelled: true,
                        message: "SIGINT sent to agent",
                    },
                ))
            } else {
                Ok(json_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    &ErrorBody {
                        error: "failed to send signal",
                    },
                ))
            }
        }
        None => Ok(json_response(
            StatusCode::OK,
            &CancelResponse {
                cancelled: false,
                message: "no agent process running",
            },
        )),
    }
}

#[derive(Serialize)]
struct CancelResponse {
    cancelled: bool,
    message: &'static str,
}

#[derive(Deserialize)]
struct MessageRequest {
    message: String,
}

#[derive(Serialize)]
struct MessageResponse {
    sent: bool,
    session_id: String,
}

async fn handle_agent_message(
    req: Request<hyper::body::Incoming>,
    session_id: &str,
    _state: &Arc<RemoteState>,
) -> Result<Response<Body>> {
    let (app, _sidecar) = match containers::find_containers() {
        Ok(pair) => pair,
        Err(_) => {
            return Ok(json_response(
                StatusCode::SERVICE_UNAVAILABLE,
                &ErrorBody {
                    error: "no containers running",
                },
            ));
        }
    };

    // Check agent is not currently running (can only send between turns)
    if agent::is_agent_running(&app).is_some() {
        return Ok(json_response(
            StatusCode::CONFLICT,
            &ErrorBody {
                error: "agent is currently running; wait for it to finish before sending a message",
            },
        ));
    }

    // Parse the request body
    let body_bytes = req.into_body().collect().await?.to_bytes();
    let msg_req: MessageRequest = match serde_json::from_slice(&body_bytes) {
        Ok(r) => r,
        Err(e) => {
            return Ok(json_response(
                StatusCode::BAD_REQUEST,
                &ErrorBodyOwned {
                    error: format!("invalid JSON: {e}"),
                },
            ));
        }
    };

    if msg_req.message.trim().is_empty() {
        return Ok(json_response(
            StatusCode::BAD_REQUEST,
            &ErrorBody {
                error: "message cannot be empty",
            },
        ));
    }

    // Launch claude --resume in detached mode so the HTTP request returns immediately
    let session_id_owned = session_id.to_string();
    let exit = containers::exec_exit_code(
        &app,
        &[
            "sh",
            "-c",
            &format!(
                "nohup claude --resume {} --dangerously-skip-permissions -p {} > /dev/null 2>&1 &",
                shell_escape(&session_id_owned),
                shell_escape(&msg_req.message),
            ),
        ],
    );

    if exit == 0 {
        Ok(json_response(
            StatusCode::OK,
            &MessageResponse {
                sent: true,
                session_id: session_id_owned,
            },
        ))
    } else {
        Ok(json_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            &ErrorBody {
                error: "failed to launch claude --resume",
            },
        ))
    }
}

/// Shell-escape a string for safe use in sh -c.
fn shell_escape(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

#[derive(Serialize)]
struct ErrorBodyOwned {
    error: String,
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

    #[test]
    fn parse_query_param_non_numeric() {
        assert_eq!(parse_query_param("limit=abc", "limit"), None);
        assert_eq!(parse_query_param("limit=-1", "limit"), None);
    }

    #[test]
    fn json_response_sets_content_type() {
        let resp = json_response(StatusCode::OK, &ErrorBody { error: "test" });
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers().get("Content-Type").unwrap(),
            "application/json"
        );
    }

    #[test]
    fn json_response_serializes_body() {
        use http_body_util::BodyExt;

        let resp = json_response(StatusCode::NOT_FOUND, &ErrorBody { error: "nope" });
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        let body = resp.into_body();
        let collected = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(body.collect())
            .unwrap();
        let bytes = collected.to_bytes();
        let raw = String::from_utf8_lossy(&bytes);
        let parsed: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(parsed["error"], "nope");
    }

    #[test]
    fn json_response_owned_error() {
        let resp = json_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            &ErrorBodyOwned {
                error: "something broke".to_string(),
            },
        );
        assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[test]
    fn cancel_response_serializes() {
        let resp = CancelResponse {
            cancelled: true,
            message: "done",
        };
        let json = serde_json::to_string(&resp).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["cancelled"], true);
        assert_eq!(v["message"], "done");
    }

    #[test]
    fn diff_response_serializes() {
        let resp = DiffResponse {
            diff: "+added line\n-removed line".to_string(),
        };
        let json = serde_json::to_string(&resp).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert!(v["diff"].as_str().unwrap().contains("+added"));
    }

    #[test]
    fn shell_escape_simple() {
        assert_eq!(shell_escape("hello"), "'hello'");
    }

    #[test]
    fn shell_escape_with_quotes() {
        assert_eq!(shell_escape("it's"), "'it'\\''s'");
    }

    #[test]
    fn shell_escape_with_special_chars() {
        assert_eq!(shell_escape("a; rm -rf /"), "'a; rm -rf /'");
    }

    #[test]
    fn message_request_deserializes() {
        let json = r#"{"message":"fix the tests"}"#;
        let req: MessageRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.message, "fix the tests");
    }

    #[test]
    fn message_request_missing_field_fails() {
        let json = r#"{"prompt":"fix the tests"}"#;
        let result = serde_json::from_str::<MessageRequest>(json);
        assert!(result.is_err());
    }

    #[test]
    fn message_response_serializes() {
        let resp = MessageResponse {
            sent: true,
            session_id: "abc-123".to_string(),
        };
        let json = serde_json::to_string(&resp).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["sent"], true);
        assert_eq!(v["session_id"], "abc-123");
    }

    #[test]
    fn pair_response_serializes() {
        let resp = PairResponse {
            session_token: "tok123".to_string(),
            device_id: "dev456".to_string(),
        };
        let json = serde_json::to_string(&resp).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["session_token"], "tok123");
        assert_eq!(v["device_id"], "dev456");
    }
}
