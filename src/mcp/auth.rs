/// OAuth 2.1 authorization flow for MCP servers.
///
/// Implements: metadata discovery, dynamic client registration, PKCE,
/// authorization code flow with localhost callback.
use anyhow::{Context, Result};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use hyper::service::service_fn;
use rand::RngCore;
use sha2::{Digest, Sha256};
use std::path::Path;
use url::Url;

use super::upstream::StoredAuth;

/// Run the OAuth flow for a named MCP server and store the resulting tokens.
pub async fn run(name: &str, upstream: &str, auth_dir: &str) -> Result<()> {
    let upstream_url = Url::parse(upstream).context("invalid upstream URL")?;
    let http = reqwest::Client::new();

    // 1. Discover OAuth metadata
    eprintln!("[auth] discovering OAuth metadata for {upstream}");
    let metadata = discover_metadata(&http, &upstream_url).await?;
    eprintln!(
        "[auth] authorization_endpoint: {}",
        metadata.authorization_endpoint
    );
    eprintln!("[auth] token_endpoint: {}", metadata.token_endpoint);

    // 2. Dynamic client registration
    eprintln!("[auth] registering client");
    let callback_port = find_available_port().await?;
    let redirect_uri = format!("http://127.0.0.1:{callback_port}/callback");

    let registration = register_client(&http, &metadata, &redirect_uri).await?;
    eprintln!("[auth] client_id: {}", registration.client_id);

    // 3. Generate PKCE
    let (code_verifier, code_challenge) = generate_pkce();

    // 4. Build authorization URL
    let mut auth_url = Url::parse(&metadata.authorization_endpoint)
        .context("invalid authorization_endpoint")?;
    auth_url.query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair("client_id", &registration.client_id)
        .append_pair("redirect_uri", &redirect_uri)
        .append_pair("code_challenge", &code_challenge)
        .append_pair("code_challenge_method", "S256");
    if let Some(ref scope) = metadata.scopes_supported
        && !scope.is_empty()
    {
        auth_url
            .query_pairs_mut()
            .append_pair("scope", &scope.join(" "));
    }

    // 5. Start callback server and wait for authorization code
    eprintln!();
    eprintln!("Open this URL in your browser to authorize:");
    eprintln!();
    eprintln!("  {auth_url}");
    eprintln!();

    let code = wait_for_callback(callback_port).await?;
    eprintln!("[auth] received authorization code");

    // 6. Exchange code for tokens
    eprintln!("[auth] exchanging code for tokens");
    let mut params = vec![
        ("grant_type", "authorization_code"),
        ("code", code.as_str()),
        ("redirect_uri", redirect_uri.as_str()),
        ("client_id", registration.client_id.as_str()),
        ("code_verifier", code_verifier.as_str()),
    ];
    let secret_ref;
    if let Some(ref secret) = registration.client_secret {
        secret_ref = secret.clone();
        params.push(("client_secret", &secret_ref));
    }

    let resp = http
        .post(&metadata.token_endpoint)
        .form(&params)
        .send()
        .await
        .context("token exchange request")?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("token exchange failed: {status} {body}");
    }

    let token_resp: TokenResponse = resp.json().await.context("parsing token response")?;

    // 7. Store tokens
    let expires_at = token_resp.expires_in.map(|secs| {
        let expiry = chrono::Utc::now() + chrono::Duration::seconds(secs);
        expiry.to_rfc3339()
    });

    let stored = StoredAuth {
        upstream: upstream.to_string(),
        client_id: registration.client_id,
        client_secret: registration.client_secret,
        access_token: token_resp.access_token,
        refresh_token: token_resp.refresh_token,
        token_endpoint: metadata.token_endpoint,
        expires_at,
    };

    let auth_path = Path::new(auth_dir);
    std::fs::create_dir_all(auth_path)
        .with_context(|| format!("creating {}", auth_path.display()))?;

    let file_path = auth_path.join(format!("{name}.json"));
    let json = serde_json::to_string_pretty(&stored)?;
    std::fs::write(&file_path, &json)
        .with_context(|| format!("writing {}", file_path.display()))?;

    eprintln!("[auth] tokens saved to {}", file_path.display());
    eprintln!("[auth] done");

    Ok(())
}

#[derive(Debug)]
struct OAuthMetadata {
    authorization_endpoint: String,
    token_endpoint: String,
    registration_endpoint: Option<String>,
    scopes_supported: Option<Vec<String>>,
}

async fn discover_metadata(http: &reqwest::Client, upstream_url: &Url) -> Result<OAuthMetadata> {
    // Authorization base URL = upstream URL without the path
    let base = format!("{}://{}", upstream_url.scheme(), upstream_url.authority());
    let well_known = format!("{base}/.well-known/oauth-authorization-server");

    let resp = http.get(&well_known).send().await;
    match resp {
        Ok(r) if r.status().is_success() => {
            let metadata: serde_json::Value = r.json().await.context("parsing metadata")?;
            Ok(OAuthMetadata {
                authorization_endpoint: metadata["authorization_endpoint"]
                    .as_str()
                    .context("missing authorization_endpoint")?
                    .to_string(),
                token_endpoint: metadata["token_endpoint"]
                    .as_str()
                    .context("missing token_endpoint")?
                    .to_string(),
                registration_endpoint: metadata["registration_endpoint"]
                    .as_str()
                    .map(|s| s.to_string()),
                scopes_supported: metadata["scopes_supported"]
                    .as_array()
                    .map(|arr| arr.iter().filter_map(|v| v.as_str().map(String::from)).collect()),
            })
        }
        _ => {
            // Fallback to defaults per MCP spec
            Ok(OAuthMetadata {
                authorization_endpoint: format!("{base}/authorize"),
                token_endpoint: format!("{base}/token"),
                registration_endpoint: Some(format!("{base}/register")),
                scopes_supported: None,
            })
        }
    }
}

#[derive(Debug)]
struct ClientRegistration {
    client_id: String,
    client_secret: Option<String>,
}

async fn register_client(
    http: &reqwest::Client,
    metadata: &OAuthMetadata,
    redirect_uri: &str,
) -> Result<ClientRegistration> {
    let Some(ref registration_endpoint) = metadata.registration_endpoint else {
        anyhow::bail!(
            "server does not support dynamic client registration. \
             Provide pre-registered client credentials in the auth file."
        );
    };

    let body = serde_json::json!({
        "client_name": "devg",
        "redirect_uris": [redirect_uri],
        "grant_types": ["authorization_code", "refresh_token"],
        "response_types": ["code"],
        "token_endpoint_auth_method": "none",
    });

    let resp = http
        .post(registration_endpoint)
        .json(&body)
        .send()
        .await
        .context("client registration request")?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("client registration failed: {status} {body}");
    }

    let reg: serde_json::Value = resp.json().await.context("parsing registration response")?;
    Ok(ClientRegistration {
        client_id: reg["client_id"]
            .as_str()
            .context("missing client_id in registration response")?
            .to_string(),
        client_secret: reg["client_secret"].as_str().map(|s| s.to_string()),
    })
}

fn generate_pkce() -> (String, String) {
    let mut verifier_bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut verifier_bytes);
    let code_verifier = URL_SAFE_NO_PAD.encode(verifier_bytes);

    let mut hasher = Sha256::new();
    hasher.update(code_verifier.as_bytes());
    let code_challenge = URL_SAFE_NO_PAD.encode(hasher.finalize());

    (code_verifier, code_challenge)
}

async fn find_available_port() -> Result<u16> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();
    drop(listener);
    Ok(port)
}

/// Start a temporary HTTP server and wait for the OAuth callback.
/// Returns the authorization code.
async fn wait_for_callback(port: u16) -> Result<String> {
    use hyper::server::conn::http1;
    use hyper_util::rt::TokioIo;
    use std::sync::Arc;
    use tokio::sync::oneshot;

    let listener = tokio::net::TcpListener::bind(format!("127.0.0.1:{port}")).await?;
    let (tx, rx) = oneshot::channel::<String>();
    let tx = Arc::new(tokio::sync::Mutex::new(Some(tx)));

    eprintln!("[auth] waiting for callback on port {port}...");

    // Accept connections until we get the code
    let server = tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                continue;
            };
            let tx = tx.clone();

            let io = TokioIo::new(stream);
            let service = service_fn(move |req: hyper::Request<hyper::body::Incoming>| {
                let tx = tx.clone();
                async move {
                    let query = req.uri().query().unwrap_or("");
                    let code = url::form_urlencoded::parse(query.as_bytes())
                        .find(|(key, _)| key == "code")
                        .map(|(_, value)| value.to_string());

                    if let Some(code) = code {
                        if let Some(tx) = tx.lock().await.take() {
                            let _ = tx.send(code);
                        }
                        Ok::<_, hyper::Error>(
                            hyper::Response::builder()
                                .status(200)
                                .header("Content-Type", "text/html")
                                .body(http_body_util::Full::new(bytes::Bytes::from(
                                    "<html><body><h1>Authorization successful!</h1>\
                                     <p>You can close this tab.</p></body></html>",
                                )))
                                .unwrap(),
                        )
                    } else {
                        Ok(hyper::Response::builder()
                            .status(400)
                            .body(http_body_util::Full::new(bytes::Bytes::from(
                                "Missing authorization code",
                            )))
                            .unwrap())
                    }
                }
            });

            tokio::spawn(async move {
                let _ = http1::Builder::new().serve_connection(io, service).await;
            });
        }
    });

    let code = rx.await.context("callback channel closed")?;
    server.abort();
    Ok(code)
}

#[derive(serde::Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: Option<String>,
    expires_in: Option<i64>,
}

fn default_auth_dir() -> String {
    if let Some(home) = std::env::var_os("HOME") {
        format!("{}/.devg/auth", home.to_string_lossy())
    } else {
        ".devg/auth".to_string()
    }
}

/// Get the default auth directory for the host (used by `devg auth`).
pub fn host_auth_dir() -> String {
    default_auth_dir()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_verifier_and_challenge_are_valid() {
        let (verifier, challenge) = generate_pkce();

        // Verifier is base64url-encoded 32 bytes = 43 chars
        assert_eq!(verifier.len(), 43);
        assert!(verifier.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'));

        // Challenge is SHA-256 of verifier, base64url = 43 chars
        assert_eq!(challenge.len(), 43);

        // Verify the relationship: challenge == base64url(sha256(verifier))
        let mut hasher = Sha256::new();
        hasher.update(verifier.as_bytes());
        let expected = URL_SAFE_NO_PAD.encode(hasher.finalize());
        assert_eq!(challenge, expected);
    }

    #[test]
    fn pkce_is_random() {
        let (v1, _) = generate_pkce();
        let (v2, _) = generate_pkce();
        assert_ne!(v1, v2);
    }

    #[tokio::test]
    async fn callback_server_extracts_code() {
        let port = find_available_port().await.unwrap();

        let server = tokio::spawn(async move { wait_for_callback(port).await });

        // Give server a moment to bind
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // Simulate browser callback
        let client = reqwest::Client::new();
        let resp = client
            .get(format!(
                "http://127.0.0.1:{port}/callback?code=test_auth_code_123&state=abc"
            ))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);

        let code = server.await.unwrap().unwrap();
        assert_eq!(code, "test_auth_code_123");
    }
}
