/// Client-side CLI shim. Runs in the app container.
///
/// Sends args to the kap sidecar's CLI proxy over HTTP.
/// Two modes:
/// - Proxy: sidecar executes the command, shim outputs stdout/stderr/exit_code
/// - Direct: sidecar returns env vars, shim exec's the real binary locally
use anyhow::Result;
use base64::Engine;
use std::path::PathBuf;

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

    // Check if sidecar says "direct mode"
    let mode = resp
        .headers()
        .get("x-mode")
        .and_then(|v| v.to_str().ok())
        .map(String::from);

    if mode.as_deref() == Some("direct") {
        return run_direct(tool, args, &resp).await;
    }

    // Proxy mode: output stdout/stderr from sidecar response
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

/// Direct mode: decode env vars from sidecar response, find the real binary,
/// and exec it (replacing this process).
async fn run_direct(tool: &str, args: &[String], resp: &reqwest::Response) -> Result<()> {
    // Decode env vars from X-Env header
    let env_vars = resp
        .headers()
        .get("x-env")
        .and_then(|v| v.to_str().ok())
        .and_then(|b64| base64::engine::general_purpose::STANDARD.decode(b64).ok())
        .and_then(|bytes| String::from_utf8(bytes).ok())
        .map(|s| {
            s.lines()
                .filter_map(|line| {
                    let (k, v) = line.split_once('=')?;
                    Some((k.to_string(), v.to_string()))
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    // Find the real binary (skip /opt/kap paths to avoid finding our own shim)
    let real_binary = find_real_binary(tool)?;

    // Exec: replace this process with the real binary
    use std::os::unix::process::CommandExt;
    let mut cmd = std::process::Command::new(&real_binary);
    cmd.args(args);
    for (k, v) in &env_vars {
        cmd.env(k, v);
    }
    let err = cmd.exec();
    anyhow::bail!("exec {}: {err}", real_binary.display());
}

/// Find the real binary by searching PATH, skipping /opt/kap paths
/// (where our shim lives).
fn find_real_binary(name: &str) -> Result<PathBuf> {
    let path = std::env::var("PATH").unwrap_or_default();
    find_binary_in_path(name, &path)
}

fn find_binary_in_path(name: &str, path: &str) -> Result<PathBuf> {
    for dir in path.split(':') {
        if dir.starts_with("/opt/kap") {
            continue;
        }
        let candidate = PathBuf::from(dir).join(name);
        if candidate.is_file() {
            return Ok(candidate);
        }
    }
    anyhow::bail!("{name}: not found in PATH (install it in your app container for direct mode)")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn tempdir(suffix: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("kap-shim-{}-{suffix}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn find_binary_skips_opt_kap() {
        let real_dir = tempdir("real-bin");

        let real_path = real_dir.join("mytool");
        fs::write(&real_path, "#!/bin/sh\necho real").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&real_path, fs::Permissions::from_mode(0o755)).unwrap();
        }

        let path = format!("/opt/kap/bin:{}", real_dir.display());
        let found = find_binary_in_path("mytool", &path).unwrap();
        assert_eq!(found, real_path);

        fs::remove_dir_all(&real_dir).unwrap();
    }

    #[test]
    fn find_binary_not_found() {
        let result = find_binary_in_path("surely_not_a_real_binary", "/nonexistent");
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("not found in PATH")
        );
    }
}
