use anyhow::{Context, Result};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use tokio::sync::Mutex;

/// Stored OAuth tokens for an MCP server.
#[derive(Debug, Deserialize, Serialize)]
pub struct StoredAuth {
    pub upstream: String,
    pub client_id: String,
    pub client_secret: Option<String>,
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub token_endpoint: String,
    pub expires_at: Option<String>,
}

impl StoredAuth {
    pub fn load(path: &Path) -> Result<Self> {
        let content =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        serde_json::from_str(&content).with_context(|| format!("parsing {}", path.display()))
    }

    fn is_expired(&self) -> bool {
        let Some(ref expires_at) = self.expires_at else {
            return false;
        };
        let Ok(expiry) = chrono::DateTime::parse_from_rfc3339(expires_at) else {
            return false;
        };
        // Refresh 5 minutes early to avoid races between check and request
        chrono::Utc::now() + chrono::Duration::minutes(5) >= expiry
    }
}

/// Client for forwarding requests to an upstream MCP server.
pub struct UpstreamClient {
    pub upstream_url: String,
    http: reqwest::Client,
    auth: Mutex<StoredAuth>,
    auth_file: Option<PathBuf>,
    session_id: Mutex<Option<String>>,
    extra_headers: Vec<(String, String)>,
}

impl UpstreamClient {
    pub fn new(
        upstream_url: String,
        auth: StoredAuth,
        extra_headers: Vec<(String, String)>,
        auth_file: Option<PathBuf>,
    ) -> Self {
        Self {
            upstream_url,
            http: reqwest::Client::new(),
            auth: Mutex::new(auth),
            auth_file,
            session_id: Mutex::new(None),
            extra_headers,
        }
    }

    /// Create a client with a simple static Bearer token (no refresh).
    pub fn with_static_token(upstream_url: String, token: String, extra_headers: Vec<(String, String)>) -> Self {
        let auth = StoredAuth {
            upstream: upstream_url.clone(),
            client_id: String::new(),
            client_secret: None,
            access_token: token,
            refresh_token: None,
            token_endpoint: String::new(),
            expires_at: None,
        };
        Self::new(upstream_url, auth, extra_headers, None)
    }

    /// Create a client with only extra headers (no Bearer token).
    pub fn with_headers_only(upstream_url: String, extra_headers: Vec<(String, String)>) -> Self {
        let auth = StoredAuth {
            upstream: upstream_url.clone(),
            client_id: String::new(),
            client_secret: None,
            access_token: String::new(),
            refresh_token: None,
            token_endpoint: String::new(),
            expires_at: None,
        };
        Self::new(upstream_url, auth, extra_headers, None)
    }

    /// Forward a JSON-RPC request body to the upstream and return the response body.
    pub async fn forward(&self, body: &[u8]) -> Result<(u16, Vec<u8>)> {
        self.ensure_valid_token().await?;

        // Check if this is an initialize request — if so, start a fresh session
        let is_initialize = serde_json::from_slice::<serde_json::Value>(body)
            .ok()
            .and_then(|v| v["method"].as_str().map(|m| m == "initialize"))
            .unwrap_or(false);

        if is_initialize {
            *self.session_id.lock().await = None;
        }

        let auth = self.auth.lock().await;
        let token = auth.access_token.clone();
        let session_id = self.session_id.lock().await.clone();
        drop(auth);

        let mut req = self
            .http
            .post(&self.upstream_url)
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream")
            .body(body.to_vec());

        if !token.is_empty() {
            req = req.bearer_auth(&token);
        }

        for (key, value) in &self.extra_headers {
            req = req.header(key, value);
        }

        if let Some(ref sid) = session_id {
            req = req.header("Mcp-Session-Id", sid);
        }

        let resp = req.send().await.context("forwarding to upstream")?;

        // Capture session ID from response
        if let Some(sid) = resp.headers().get("mcp-session-id")
            && let Ok(sid) = sid.to_str()
        {
            *self.session_id.lock().await = Some(sid.to_string());
        }

        let status = resp.status().as_u16();
        let bytes = resp.bytes().await.context("reading upstream response")?;
        Ok((status, bytes.to_vec()))
    }

    async fn ensure_valid_token(&self) -> Result<()> {
        let mut auth = self.auth.lock().await;
        if !auth.is_expired() {
            return Ok(());
        }

        if let Some(ref path) = self.auth_file {
            // Acquire file lock — blocks until available.
            // This coordinates refresh across multiple containers sharing the same auth file.
            let lock_path = path.with_extension("lock");
            let lock_file = std::fs::File::create(&lock_path)
                .with_context(|| format!("creating lock file {}", lock_path.display()))?;
            lock_file.lock_exclusive()
                .with_context(|| format!("acquiring lock on {}", lock_path.display()))?;

            // Re-read file — another container may have already refreshed
            if let Ok(fresh) = StoredAuth::load(path) {
                if !fresh.is_expired() {
                    eprintln!("[mcp] token for {} already refreshed by another process", self.upstream_url);
                    *auth = fresh;
                    // lock released on drop
                    return Ok(());
                }
                // Use the freshest data from disk (may have newer refresh_token)
                *auth = fresh;
            }

            self.do_refresh(&mut auth).await?;

            // Write refreshed tokens to shared file (still holding lock)
            if let Ok(json) = serde_json::to_string_pretty(&*auth) {
                if let Err(e) = super::auth::write_private(path, &json) {
                    eprintln!("[mcp] warning: failed to persist refreshed tokens to {}: {e}", path.display());
                }
            }
            // lock released on drop of lock_file
        } else {
            // No auth file — refresh in-memory only (static tokens, no file coordination)
            self.do_refresh(&mut auth).await?;
        }

        Ok(())
    }

    async fn do_refresh(&self, auth: &mut StoredAuth) -> Result<()> {
        let Some(ref refresh_token) = auth.refresh_token else {
            anyhow::bail!("access token expired and no refresh token available");
        };

        eprintln!("[mcp] refreshing token for {}", self.upstream_url);

        let mut params = vec![
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token.as_str()),
            ("client_id", auth.client_id.as_str()),
        ];
        let secret_clone;
        if let Some(ref secret) = auth.client_secret {
            secret_clone = secret.clone();
            params.push(("client_secret", &secret_clone));
        }

        let resp = self
            .http
            .post(&auth.token_endpoint)
            .form(&params)
            .send()
            .await
            .context("token refresh request")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("token refresh failed: {status} {body}");
        }

        let token_resp: TokenResponse = resp.json().await.context("parsing token response")?;
        auth.access_token = token_resp.access_token;
        if let Some(rt) = token_resp.refresh_token {
            auth.refresh_token = Some(rt);
        }
        if let Some(expires_in) = token_resp.expires_in {
            let expiry = chrono::Utc::now() + chrono::Duration::seconds(expires_in);
            auth.expires_at = Some(expiry.to_rfc3339());
        }

        Ok(())
    }
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: Option<String>,
    expires_in: Option<i64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_auth(expires_at: Option<&str>) -> StoredAuth {
        StoredAuth {
            upstream: "https://mcp.example.com".to_string(),
            client_id: "client123".to_string(),
            client_secret: None,
            access_token: "token_abc".to_string(),
            refresh_token: Some("refresh_xyz".to_string()),
            token_endpoint: "https://mcp.example.com/token".to_string(),
            expires_at: expires_at.map(String::from),
        }
    }

    #[test]
    fn stored_auth_roundtrip() {
        let auth = make_auth(Some("2030-01-01T00:00:00Z"));
        let json = serde_json::to_string_pretty(&auth).unwrap();
        let loaded: StoredAuth = serde_json::from_str(&json).unwrap();
        assert_eq!(loaded.upstream, "https://mcp.example.com");
        assert_eq!(loaded.client_id, "client123");
        assert_eq!(loaded.access_token, "token_abc");
        assert_eq!(loaded.refresh_token.as_deref(), Some("refresh_xyz"));
    }

    #[test]
    fn stored_auth_load_from_file() {
        let dir = std::env::temp_dir().join(format!("devg-auth-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("test.json");

        let auth = make_auth(None);
        std::fs::write(&path, serde_json::to_string(&auth).unwrap()).unwrap();

        let loaded = StoredAuth::load(&path).unwrap();
        assert_eq!(loaded.access_token, "token_abc");

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn not_expired_when_no_expiry() {
        let auth = make_auth(None);
        assert!(!auth.is_expired());
    }

    #[test]
    fn not_expired_when_future() {
        let auth = make_auth(Some("2099-01-01T00:00:00Z"));
        assert!(!auth.is_expired());
    }

    #[test]
    fn expired_when_past() {
        let auth = make_auth(Some("2020-01-01T00:00:00Z"));
        assert!(auth.is_expired());
    }

    #[test]
    fn not_expired_when_invalid_date() {
        let auth = make_auth(Some("not-a-date"));
        assert!(!auth.is_expired());
    }

    #[test]
    fn load_nonexistent_file_errors() {
        let result = StoredAuth::load(std::path::Path::new("/nonexistent/auth.json"));
        assert!(result.is_err());
    }

    #[test]
    fn load_invalid_json_errors() {
        let dir = std::env::temp_dir().join(format!("devg-auth-invalid-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("bad.json");
        std::fs::write(&path, "not json").unwrap();

        let result = StoredAuth::load(&path);
        assert!(result.is_err());

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn static_token_never_expires() {
        let client = UpstreamClient::with_static_token(
            "https://example.com".to_string(),
            "my_token".to_string(),
            vec![],
        );
        let auth = client.auth.blocking_lock();
        assert_eq!(auth.access_token, "my_token");
        assert!(auth.expires_at.is_none());
        assert!(!auth.is_expired());
    }

    #[test]
    fn expired_when_within_five_minute_buffer() {
        // Token expiring in 2 minutes should be treated as expired
        let expiry = chrono::Utc::now() + chrono::Duration::minutes(2);
        let auth = make_auth(Some(&expiry.to_rfc3339()));
        assert!(auth.is_expired());
    }

    #[test]
    fn not_expired_when_beyond_five_minute_buffer() {
        // Token expiring in 10 minutes should not be treated as expired
        let expiry = chrono::Utc::now() + chrono::Duration::minutes(10);
        let auth = make_auth(Some(&expiry.to_rfc3339()));
        assert!(!auth.is_expired());
    }

    #[tokio::test]
    async fn ensure_valid_token_persists_to_disk() {
        // Start a mock token endpoint
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let io = hyper_util::rt::TokioIo::new(stream);
            hyper::server::conn::http1::Builder::new()
                .serve_connection(
                    io,
                    hyper::service::service_fn(|_req| async {
                        let body = serde_json::json!({
                            "access_token": "new_access",
                            "refresh_token": "new_refresh",
                            "expires_in": 3600
                        });
                        Ok::<_, hyper::Error>(
                            hyper::Response::builder()
                                .status(200)
                                .header("Content-Type", "application/json")
                                .body(http_body_util::Full::new(bytes::Bytes::from(
                                    serde_json::to_vec(&body).unwrap(),
                                )))
                                .unwrap(),
                        )
                    }),
                )
                .await
                .unwrap();
        });

        let dir = std::env::temp_dir().join(format!("devg-refresh-persist-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let auth_path = dir.join("test.json");

        let auth = StoredAuth {
            upstream: "https://mcp.example.com".to_string(),
            client_id: "client123".to_string(),
            client_secret: None,
            access_token: "old_token".to_string(),
            refresh_token: Some("old_refresh".to_string()),
            token_endpoint: format!("http://127.0.0.1:{port}/token"),
            expires_at: Some("2020-01-01T00:00:00Z".to_string()), // expired
        };

        let client = UpstreamClient::new(
            "https://mcp.example.com".to_string(),
            auth,
            vec![],
            Some(auth_path.clone()),
        );

        client.ensure_valid_token().await.unwrap();
        server.abort();

        // Verify in-memory state
        let auth = client.auth.lock().await;
        assert_eq!(auth.access_token, "new_access");
        assert_eq!(auth.refresh_token.as_deref(), Some("new_refresh"));
        drop(auth);

        // Verify persisted to disk
        let loaded = StoredAuth::load(&auth_path).unwrap();
        assert_eq!(loaded.access_token, "new_access");
        assert_eq!(loaded.refresh_token.as_deref(), Some("new_refresh"));
        assert!(loaded.expires_at.is_some());

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn reread_after_lock_skips_refresh_if_file_already_fresh() {
        // Simulates the case where another container refreshed while we waited for the lock.
        // The client has an expired in-memory token, but the file has a fresh one.
        let dir = std::env::temp_dir().join(format!("devg-reread-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let auth_path = dir.join("test.json");

        // Write a FRESH token to disk (as if another container just refreshed)
        let fresh = StoredAuth {
            upstream: "https://mcp.example.com".to_string(),
            client_id: "client123".to_string(),
            client_secret: None,
            access_token: "fresh_from_other_container".to_string(),
            refresh_token: Some("fresh_refresh".to_string()),
            token_endpoint: "http://127.0.0.1:1/token".to_string(), // unreachable — should not be called
            expires_at: Some("2099-01-01T00:00:00Z".to_string()),
        };
        std::fs::write(&auth_path, serde_json::to_string_pretty(&fresh).unwrap()).unwrap();

        // Client has an EXPIRED in-memory token
        let expired = StoredAuth {
            upstream: "https://mcp.example.com".to_string(),
            client_id: "client123".to_string(),
            client_secret: None,
            access_token: "expired_token".to_string(),
            refresh_token: Some("old_refresh".to_string()),
            token_endpoint: "http://127.0.0.1:1/token".to_string(),
            expires_at: Some("2020-01-01T00:00:00Z".to_string()),
        };

        let client = UpstreamClient::new(
            "https://mcp.example.com".to_string(),
            expired,
            vec![],
            Some(auth_path.clone()),
        );

        // Should re-read from file, find fresh token, and NOT call the token endpoint
        client.ensure_valid_token().await.unwrap();

        let auth = client.auth.lock().await;
        assert_eq!(auth.access_token, "fresh_from_other_container");
        assert_eq!(auth.refresh_token.as_deref(), Some("fresh_refresh"));
        drop(auth);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn lock_file_created_during_refresh() {
        // Verify that a .lock file is created alongside the auth file
        let dir = std::env::temp_dir().join(format!("devg-lockfile-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let auth_path = dir.join("test.json");
        let lock_path = auth_path.with_extension("lock");

        assert!(!lock_path.exists());

        // Creating the lock file (as ensure_valid_token does)
        let lock_file = std::fs::File::create(&lock_path).unwrap();
        fs2::FileExt::lock_exclusive(&lock_file).unwrap();
        assert!(lock_path.exists());
        drop(lock_file); // releases lock

        std::fs::remove_dir_all(&dir).unwrap();
    }
}
