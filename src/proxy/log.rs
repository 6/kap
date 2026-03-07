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
        file.flush().await?;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_entry_serializes_to_json() {
        let entry = ProxyLogEntry::new("github.com", "allowed", "CONNECT");
        let json = serde_json::to_string(&entry).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["domain"], "github.com");
        assert_eq!(parsed["action"], "allowed");
        assert_eq!(parsed["method"], "CONNECT");
        assert!(parsed["ts"].is_string());
    }

    #[test]
    fn log_entry_actions() {
        for action in &["allowed", "denied", "observed"] {
            let entry = ProxyLogEntry::new("test.com", action, "GET");
            assert_eq!(entry.action, *action);
            assert_eq!(entry.domain, "test.com");
            assert_eq!(entry.method, "GET");
        }
    }

    #[tokio::test]
    async fn logger_writes_jsonl_to_file() {
        let dir = std::env::temp_dir().join(format!("kap-log-write-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("test.jsonl");

        let logger = ProxyLogger::new(path.to_str().unwrap());
        let entry = ProxyLogEntry::new("example.com", "allowed", "CONNECT");
        logger.log(&entry).await.unwrap();

        let content = std::fs::read_to_string(&path).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(content.trim()).unwrap();
        assert_eq!(parsed["domain"], "example.com");
        assert_eq!(parsed["action"], "allowed");
        assert_eq!(parsed["method"], "CONNECT");

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn logger_appends_multiple_entries() {
        let dir = std::env::temp_dir().join(format!("kap-log-append-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("test.jsonl");

        let logger = ProxyLogger::new(path.to_str().unwrap());
        logger
            .log(&ProxyLogEntry::new("a.com", "allowed", "GET"))
            .await
            .unwrap();
        logger
            .log(&ProxyLogEntry::new("b.com", "denied", "CONNECT"))
            .await
            .unwrap();

        let content = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = content.trim().lines().collect();
        assert_eq!(lines.len(), 2);
        let first: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        let second: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(first["domain"], "a.com");
        assert_eq!(second["domain"], "b.com");

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn logger_creates_parent_dirs() {
        let dir = std::env::temp_dir().join(format!("kap-log-mkdir-{}", std::process::id()));
        let path = dir.join("sub").join("deep").join("log.jsonl");
        // dir doesn't exist yet
        assert!(!dir.exists());

        let logger = ProxyLogger::new(path.to_str().unwrap());
        logger
            .log(&ProxyLogEntry::new("test.com", "denied", "GET"))
            .await
            .unwrap();

        assert!(path.exists());

        std::fs::remove_dir_all(&dir).unwrap();
    }
}
