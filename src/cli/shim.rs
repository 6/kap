/// Client-side CLI shim. Runs in the app container.
///
/// Sends args to the kap sidecar's CLI proxy over HTTP
/// and forwards stdout/stderr/exit_code back to the caller.
use anyhow::Result;
use base64::Engine;

const DEVG_CLI_PORT: u16 = 3130;

fn sidecar_host() -> String {
    std::env::var("HTTP_PROXY")
        .ok()
        .and_then(|v| {
            v.strip_prefix("http://")
                .and_then(|rest| rest.split(':').next())
                .map(String::from)
        })
        .unwrap_or_else(|| "172.28.0.3".to_string())
}

pub async fn run(tool: &str, args: &[String]) -> Result<()> {
    let host = sidecar_host();
    let url = format!("http://{host}:{DEVG_CLI_PORT}/{tool}");

    // Bypass HTTP_PROXY - talk directly to the sidecar on the internal network
    // Send current directory so sidecar can cd into the workspace
    let cwd = std::env::current_dir()
        .ok()
        .and_then(|p| p.to_str().map(String::from))
        .unwrap_or_default();

    let client = reqwest::Client::builder().no_proxy().build()?;
    let resp = client
        .post(&url)
        .json(&serde_json::json!({"args": args, "cwd": cwd}))
        .timeout(std::time::Duration::from_secs(120))
        .send()
        .await?;

    let exit_code: i32 = resp
        .headers()
        .get("x-exit-code")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse().ok())
        .unwrap_or(1);

    let stderr_b64 = resp
        .headers()
        .get("x-stderr")
        .and_then(|v| v.to_str().ok())
        .map(String::from);

    let stdout = resp.bytes().await?;

    // Write stderr first
    if let Some(b64) = stderr_b64
        && let Ok(decoded) = base64::engine::general_purpose::STANDARD.decode(&b64)
    {
        use std::io::Write;
        let _ = std::io::stderr().write_all(&decoded);
    }

    // Write stdout
    {
        use std::io::Write;
        let _ = std::io::stdout().write_all(&stdout);
    }

    std::process::exit(exit_code);
}
