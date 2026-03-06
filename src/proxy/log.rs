use anyhow::Result;
use serde::Serialize;
use std::path::Path;
use tokio::fs::{self, OpenOptions};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

#[derive(Serialize)]
pub struct ProxyLogEntry {
    pub ts: String,
    pub domain: String,
    pub action: &'static str, // "allowed", "denied", "observed"
    pub method: String,       // "CONNECT", "GET", etc.
}

impl ProxyLogEntry {
    pub fn new(domain: &str, action: &'static str, method: &str) -> Self {
        Self {
            ts: chrono::Utc::now().to_rfc3339(),
            domain: domain.to_string(),
            action,
            method: method.to_string(),
        }
    }
}

pub struct ProxyLogger {
    path: String,
}

impl ProxyLogger {
    pub fn new(path: &str) -> Self {
        Self {
            path: path.to_string(),
        }
    }

    pub async fn log(&self, entry: &ProxyLogEntry) -> Result<()> {
        if let Some(parent) = Path::new(&self.path).parent() {
            fs::create_dir_all(parent).await?;
        }
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .await?;
        let mut line = serde_json::to_string(entry)?;
        line.push('\n');
        file.write_all(line.as_bytes()).await?;
        Ok(())
    }
}

/// Read and display denied requests from the proxy log.
pub async fn why_denied(log_path: &str, tail: bool) -> Result<()> {
    let path = Path::new(log_path);
    if !path.exists() {
        println!("No proxy log found at {log_path}");
        println!("The proxy writes logs here when it denies requests.");
        return Ok(());
    }

    let file = fs::File::open(path).await?;
    let reader = BufReader::new(file);
    let mut lines = reader.lines();
    let mut count = 0;

    while let Some(line) = lines.next_line().await? {
        if let Ok(entry) = serde_json::from_str::<serde_json::Value>(&line) {
            let action = entry.get("action").and_then(|v| v.as_str()).unwrap_or("");
            if action == "denied" {
                let ts = entry.get("ts").and_then(|v| v.as_str()).unwrap_or("?");
                let domain = entry.get("domain").and_then(|v| v.as_str()).unwrap_or("?");
                let method = entry.get("method").and_then(|v| v.as_str()).unwrap_or("?");
                println!("{ts}  {method:8} {domain}  DENIED");
                count += 1;
            }
        }
    }

    if count == 0 && !tail {
        println!("No denied requests found in {log_path}");
    }

    if tail {
        println!("--- streaming new denials (ctrl-c to stop) ---");
        // Re-open and seek to end, then poll for new lines
        use tokio::time::{Duration, sleep};
        let file = fs::File::open(path).await?;
        let metadata = file.metadata().await?;
        let mut pos = metadata.len();

        loop {
            sleep(Duration::from_millis(500)).await;
            let metadata = fs::metadata(path).await?;
            if metadata.len() > pos {
                let file = fs::File::open(path).await?;
                let reader = BufReader::new(file);
                let mut lines = reader.lines();
                let mut current_pos = 0u64;
                while let Some(line) = lines.next_line().await? {
                    current_pos += line.len() as u64 + 1;
                    if current_pos <= pos {
                        continue;
                    }
                    if let Ok(entry) = serde_json::from_str::<serde_json::Value>(&line) {
                        let action = entry.get("action").and_then(|v| v.as_str()).unwrap_or("");
                        if action == "denied" {
                            let ts = entry.get("ts").and_then(|v| v.as_str()).unwrap_or("?");
                            let domain =
                                entry.get("domain").and_then(|v| v.as_str()).unwrap_or("?");
                            let method =
                                entry.get("method").and_then(|v| v.as_str()).unwrap_or("?");
                            println!("{ts}  {method:8} {domain}  DENIED");
                        }
                    }
                }
                pos = metadata.len();
            }
        }
    }

    Ok(())
}
