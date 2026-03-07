/// Client-side CLI shim. Runs in the app container.
///
/// Sends args to the devg sidecar's CLI proxy over HTTP
/// and forwards stdout/stderr/exit_code back to the caller.
use anyhow::Result;
use base64::Engine;

const DEVG_HOST: &str = "172.28.0.3";
const DEVG_CLI_PORT: u16 = 3130;

pub async fn run(tool: &str, args: &[String]) -> Result<()> {
    let url = format!("http://{DEVG_HOST}:{DEVG_CLI_PORT}/{tool}");

    let client = reqwest::Client::new();
    let resp = client
        .post(&url)
        .json(&serde_json::json!({"args": args}))
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
