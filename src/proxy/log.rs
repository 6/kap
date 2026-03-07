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

#[derive(Clone)]
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

const MAX_DENIED_ENTRIES: usize = 100;

/// Parse JSONL log content and return formatted denied entries.
/// Filters out health-check probes (kap-test.invalid).
fn parse_denied_entries(content: &str) -> Vec<String> {
    content
        .lines()
        .filter_map(|line| {
            let entry = serde_json::from_str::<serde_json::Value>(line).ok()?;
            let action = entry.get("action")?.as_str()?;
            if action != "denied" {
                return None;
            }
            let domain = entry.get("domain")?.as_str()?;
            if domain == "kap-test.invalid" {
                return None;
            }
            let ts = entry.get("ts").and_then(|v| v.as_str()).unwrap_or("?");
            let method = entry.get("method").and_then(|v| v.as_str()).unwrap_or("?");
            Some(format!("{ts}  {method:8} {domain}  DENIED"))
        })
        .collect()
}

/// Read and display denied requests from the proxy log.
pub async fn why_denied(log_path: &str, tail: bool) -> Result<()> {
    let path = Path::new(log_path);
    if !path.exists() {
        println!("No proxy log found at {log_path}");
        println!("The proxy writes logs here when it denies requests.");
        return Ok(());
    }

    let content = fs::read_to_string(path).await?;
    let entries = parse_denied_entries(&content);

    if entries.is_empty() && !tail {
        println!("No denied requests found in {log_path}");
    } else {
        let total = entries.len();
        if total > MAX_DENIED_ENTRIES {
            println!("({} older entries omitted)\n", total - MAX_DENIED_ENTRIES);
        }
        for entry in entries
            .iter()
            .skip(total.saturating_sub(MAX_DENIED_ENTRIES))
        {
            println!("{entry}");
        }
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
                            let domain =
                                entry.get("domain").and_then(|v| v.as_str()).unwrap_or("?");
                            if domain == "kap-test.invalid" {
                                continue;
                            }
                            let ts = entry.get("ts").and_then(|v| v.as_str()).unwrap_or("?");
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

    #[test]
    fn parse_denied_filters_only_denied() {
        let content = [
            r#"{"ts":"2026-01-01T00:00:00Z","domain":"a.com","action":"allowed","method":"GET"}"#,
            r#"{"ts":"2026-01-01T00:00:01Z","domain":"b.com","action":"denied","method":"CONNECT"}"#,
            r#"{"ts":"2026-01-01T00:00:02Z","domain":"c.com","action":"observed","method":"GET"}"#,
        ]
        .join("\n");
        let entries = parse_denied_entries(&content);
        assert_eq!(entries.len(), 1);
        assert!(entries[0].contains("b.com"));
    }

    #[test]
    fn parse_denied_skips_health_check() {
        let content = r#"{"ts":"2026-01-01T00:00:00Z","domain":"kap-test.invalid","action":"denied","method":"CONNECT"}"#;
        let entries = parse_denied_entries(content);
        assert!(entries.is_empty());
    }

    #[test]
    fn parse_denied_handles_empty_and_malformed() {
        assert!(parse_denied_entries("").is_empty());
        assert!(parse_denied_entries("not json\n{}\n").is_empty());
    }

    #[test]
    fn max_denied_entries_limit() {
        // Generate more than MAX_DENIED_ENTRIES denied entries
        let lines: Vec<String> = (0..150)
            .map(|i| {
                format!(
                    r#"{{"ts":"2026-01-01T00:00:{:02}Z","domain":"d{i}.com","action":"denied","method":"CONNECT"}}"#,
                    i % 60
                )
            })
            .collect();
        let content = lines.join("\n");
        let entries = parse_denied_entries(&content);
        assert_eq!(entries.len(), 150);

        // Verify the limit logic (last MAX_DENIED_ENTRIES shown)
        let total = entries.len();
        let shown: Vec<_> = entries
            .iter()
            .skip(total.saturating_sub(MAX_DENIED_ENTRIES))
            .collect();
        assert_eq!(shown.len(), MAX_DENIED_ENTRIES);
        // First shown entry should be #50 (skipping 0-49)
        assert!(shown[0].contains("d50.com"));
        assert!(shown[99].contains("d149.com"));
    }
}
