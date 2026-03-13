use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::remote::containers;

/// A Claude Code session discovered on disk.
#[derive(Debug, Serialize, Clone)]
pub struct SessionInfo {
    pub id: String,
    pub project: String,
    pub branch: Option<String>,
    pub last_event: Option<String>,
    pub event_count: usize,
}

/// A parsed event from a Claude Code session JSONL file.
#[derive(Debug, Serialize, Clone)]
pub struct SessionEvent {
    #[serde(rename = "type")]
    pub event_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub uuid: Option<String>,
}

/// Raw JSONL line — we deserialize leniently with defaults.
#[derive(Deserialize)]
struct RawEvent {
    #[serde(rename = "type", default)]
    event_type: Option<String>,
    #[serde(default)]
    timestamp: Option<String>,
    #[serde(default)]
    uuid: Option<String>,
    #[serde(default)]
    message: Option<RawMessage>,
    #[allow(dead_code)]
    #[serde(rename = "sessionId", default)]
    session_id: Option<String>,
    #[serde(rename = "gitBranch", default)]
    git_branch: Option<String>,
    #[allow(dead_code)]
    #[serde(default)]
    content: Option<String>,
}

#[derive(Deserialize)]
struct RawMessage {
    #[serde(default)]
    role: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    content: Option<serde_json::Value>,
}

/// Discover Claude Code sessions in the app container.
/// Sessions live at ~/.claude/projects/*/  with *.jsonl files.
/// Excludes subagent sessions (under /subagents/ directories).
pub fn discover_sessions(app_container: &str) -> Result<Vec<SessionInfo>> {
    // Single docker exec: find session files (excluding subagents), then for each
    // print the path, line count, and first 5 lines separated by a marker.
    // This avoids N+1 docker exec calls which are slow (~300ms each).
    let output = containers::exec_in(
        app_container,
        &[
            "sh",
            "-c",
            r#"find /home /root -path '*/.claude/projects/*/*.jsonl' -not -path '*/subagents/*' -type f 2>/dev/null | while read -r f; do
  wc=$(wc -l < "$f" 2>/dev/null || echo 0)
  head=$(head -5 "$f" 2>/dev/null)
  printf '@@KAP_SESSION@@%s@@%s\n%s\n' "$f" "$wc" "$head"
done"#,
        ],
    )
    .unwrap_or_default();

    let mut sessions = Vec::new();

    for block in output.split("@@KAP_SESSION@@") {
        let block = block.trim();
        if block.is_empty() {
            continue;
        }

        // Format: {path}@@{line_count}\n{head lines...}
        let (header, head) = block.split_once('\n').unwrap_or((block, ""));
        let (path, wc_str) = header.rsplit_once("@@").unwrap_or((header, "0"));
        let event_count = wc_str.trim().parse::<usize>().unwrap_or(0);

        let filename = path.rsplit('/').next().unwrap_or("");
        let session_id = filename.strip_suffix(".jsonl").unwrap_or(filename);
        let project = extract_project_name(path);

        let mut branch = None;
        let mut last_ts = None;

        for line in head.lines() {
            if let Ok(evt) = serde_json::from_str::<RawEvent>(line) {
                if branch.is_none() {
                    branch = evt.git_branch;
                }
                if last_ts.is_none() {
                    last_ts = evt.timestamp;
                }
            }
        }

        sessions.push(SessionInfo {
            id: session_id.to_string(),
            project,
            branch,
            last_event: last_ts,
            event_count,
        });
    }

    Ok(sessions)
}

/// Read and parse a session's JSONL into structured events.
/// Filters out noise (progress events, meta messages) for a clean timeline.
pub fn read_session(app_container: &str, session_id: &str) -> Result<Vec<SessionEvent>> {
    // Find the session file
    let path = find_session_path(app_container, session_id)?;

    let raw = containers::exec_in(app_container, &["cat", &path])
        .ok_or_else(|| anyhow::anyhow!("failed to read session file"))?;

    Ok(parse_session_events(&raw))
}

/// Parse JSONL content into structured session events.
pub fn parse_session_events(jsonl: &str) -> Vec<SessionEvent> {
    jsonl
        .lines()
        .filter_map(|line| {
            let raw: RawEvent = serde_json::from_str(line).ok()?;
            let event_type = raw.event_type.as_deref()?;

            // Skip noise: progress events and file-history-snapshots
            match event_type {
                "progress" | "file-history-snapshot" => return None,
                _ => {}
            }

            let msg = raw.message.as_ref();
            let role = msg.and_then(|m| m.role.clone());

            // Skip meta user messages (internal commands, etc.)
            if event_type == "user"
                && role.as_deref() == Some("user")
                && let Some(m) = msg
                && let Some(serde_json::Value::String(s)) = &m.content
                && (s.starts_with("<local-command") || s.starts_with("<command-"))
            {
                return None;
            }

            let summary = extract_summary(event_type, msg);
            let model = msg.and_then(|m| m.model.clone());
            let tool_name = extract_tool_name(event_type, msg);

            Some(SessionEvent {
                event_type: event_type.to_string(),
                role,
                timestamp: raw.timestamp,
                summary,
                model,
                tool_name,
                uuid: raw.uuid,
            })
        })
        .collect()
}

/// Truncate a string: take first line, cap at `max_len` chars.
fn truncate_str(s: &str, max_len: usize) -> String {
    let first_line = s.lines().next().unwrap_or(s).trim();
    let is_multiline = s.contains('\n');
    if first_line.chars().count() <= max_len && !is_multiline {
        return first_line.to_string();
    }
    let truncated: String = first_line.chars().take(max_len).collect();
    format!("{truncated}…")
}

/// Extract a compact description from a tool_use input object.
fn compact_tool_input(tool_name: &str, input: &serde_json::Value) -> Option<String> {
    let obj = input.as_object()?;
    let raw = match tool_name {
        "Bash" => obj.get("command")?.as_str()?,
        "Read" | "Write" | "Edit" => obj.get("file_path")?.as_str()?,
        "Grep" | "Glob" => obj.get("pattern")?.as_str()?,
        "Agent" => {
            return obj
                .get("description")
                .or_else(|| obj.get("prompt"))
                .and_then(|v| v.as_str())
                .map(|s| truncate_str(s, 120));
        }
        _ => {
            return obj
                .get("command")
                .or_else(|| obj.get("file_path"))
                .or_else(|| obj.get("path"))
                .or_else(|| obj.get("query"))
                .and_then(|v| v.as_str())
                .map(|s| truncate_str(s, 120));
        }
    };
    Some(truncate_str(raw, 120))
}

/// Extract a summary from a tool_result content block.
fn tool_result_summary(item: &serde_json::Value) -> String {
    if item.get("is_error").and_then(|v| v.as_bool()) == Some(true) {
        let err = item
            .get("content")
            .and_then(|c| c.as_str())
            .unwrap_or("error");
        return format!("(error) {}", truncate_str(err, 150));
    }
    match item.get("content") {
        Some(serde_json::Value::String(s)) => {
            let trimmed = s.trim();
            if trimmed.is_empty() {
                "(empty result)".to_string()
            } else {
                truncate_str(trimmed, 200)
            }
        }
        Some(serde_json::Value::Array(arr)) => {
            for block in arr {
                if block.get("type").and_then(|t| t.as_str()) == Some("text")
                    && let Some(text) = block.get("text").and_then(|t| t.as_str())
                {
                    let trimmed = text.trim();
                    if !trimmed.is_empty() {
                        return truncate_str(trimmed, 200);
                    }
                }
            }
            "(tool result)".to_string()
        }
        _ => "(tool result)".to_string(),
    }
}

/// Extract a human-readable summary from the event.
fn extract_summary(event_type: &str, msg: Option<&RawMessage>) -> Option<String> {
    // System and queue events don't need a message field
    match event_type {
        "system" => return Some("system event".to_string()),
        "queue-operation" => return None,
        _ => {}
    }

    let msg = msg?;

    match event_type {
        "user" => {
            // User message: extract text content or tool_result
            match &msg.content {
                Some(serde_json::Value::String(s)) => Some(truncate_str(s, 200)),
                Some(serde_json::Value::Array(arr)) => {
                    for item in arr {
                        let item_type = item.get("type").and_then(|t| t.as_str());
                        match item_type {
                            Some("text") => {
                                if let Some(text) = item.get("text").and_then(|t| t.as_str()) {
                                    let trimmed = text.trim();
                                    if !trimmed.is_empty() {
                                        return Some(truncate_str(trimmed, 200));
                                    }
                                }
                            }
                            Some("tool_result") => {
                                return Some(tool_result_summary(item));
                            }
                            _ => {}
                        }
                    }
                    None
                }
                _ => None,
            }
        }
        "assistant" => {
            // Assistant message: extract text or tool_use type
            match &msg.content {
                Some(serde_json::Value::Array(arr)) => {
                    for item in arr {
                        let item_type = item.get("type").and_then(|t| t.as_str());
                        match item_type {
                            Some("text") => {
                                if let Some(text) = item.get("text").and_then(|t| t.as_str()) {
                                    let trimmed = text.trim();
                                    if !trimmed.is_empty() {
                                        return Some(truncate_str(trimmed, 200));
                                    }
                                }
                            }
                            Some("tool_use") => {
                                let name = item.get("name").and_then(|n| n.as_str());
                                if let Some(name) = name {
                                    let desc = item
                                        .get("input")
                                        .and_then(|inp| compact_tool_input(name, inp));
                                    return match desc {
                                        Some(d) => Some(d),
                                        None => Some(format!("tool_use: {name}")),
                                    };
                                }
                            }
                            Some("thinking") => {
                                return Some("thinking…".to_string());
                            }
                            _ => {}
                        }
                    }
                    None
                }
                _ => None,
            }
        }
        _ => None,
    }
}

/// Extract tool name from a tool_use assistant message.
fn extract_tool_name(event_type: &str, msg: Option<&RawMessage>) -> Option<String> {
    if event_type != "assistant" {
        return None;
    }
    let msg = msg?;
    if let Some(serde_json::Value::Array(arr)) = &msg.content {
        for item in arr {
            if item.get("type").and_then(|t| t.as_str()) == Some("tool_use") {
                return item.get("name").and_then(|n| n.as_str()).map(String::from);
            }
        }
    }
    None
}

/// Extract project name from a session file path.
fn extract_project_name(path: &str) -> String {
    // Path: .../projects/{project-hash}/{session_id}.jsonl
    let parts: Vec<&str> = path.split('/').collect();
    for (i, part) in parts.iter().enumerate() {
        if *part == "projects" && i + 1 < parts.len() {
            return parts[i + 1].to_string();
        }
    }
    "unknown".to_string()
}

/// Find the full path to a session's JSONL file in the container.
fn find_session_path(app_container: &str, session_id: &str) -> Result<String> {
    let output = containers::exec_in(
        app_container,
        &[
            "sh",
            "-c",
            &format!(
                "find /home /root -name '{session_id}.jsonl' -path '*/.claude/projects/*' 2>/dev/null | head -1"
            ),
        ],
    )
    .ok_or_else(|| anyhow::anyhow!("session {session_id} not found"))?;

    let path = output.trim().to_string();
    if path.is_empty() {
        anyhow::bail!("session {session_id} not found");
    }
    Ok(path)
}

/// Check if a Claude Code process is running in the container.
pub fn is_agent_running(app_container: &str) -> Option<u32> {
    let output = containers::exec_in(app_container, &["pgrep", "-f", "claude"])?;
    output.lines().next()?.trim().parse().ok()
}

/// Get the git diff from the app container's workspace.
pub fn get_diff(app_container: &str) -> Result<String> {
    containers::exec_in(app_container, &["git", "diff"])
        .ok_or_else(|| anyhow::anyhow!("failed to get git diff"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_user_message() {
        let jsonl = r#"{"type":"user","message":{"role":"user","content":"fix the tests"},"timestamp":"2026-03-07T10:00:00Z","uuid":"abc"}"#;
        let events = parse_session_events(jsonl);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event_type, "user");
        assert_eq!(events[0].role.as_deref(), Some("user"));
        assert_eq!(events[0].summary.as_deref(), Some("fix the tests"));
    }

    #[test]
    fn parse_assistant_text() {
        let jsonl = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"I'll fix those tests."}]},"timestamp":"2026-03-07T10:00:01Z","uuid":"def"}"#;
        let events = parse_session_events(jsonl);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event_type, "assistant");
        assert_eq!(events[0].summary.as_deref(), Some("I'll fix those tests."));
    }

    #[test]
    fn parse_assistant_tool_use() {
        let jsonl = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","name":"Read","id":"t1","input":{}}]},"timestamp":"2026-03-07T10:00:02Z","uuid":"ghi"}"#;
        let events = parse_session_events(jsonl);
        assert_eq!(events.len(), 1);
        // Empty input: falls back to "tool_use: Read"
        assert_eq!(events[0].summary.as_deref(), Some("tool_use: Read"));
        assert_eq!(events[0].tool_name.as_deref(), Some("Read"));
    }

    #[test]
    fn parse_thinking() {
        let jsonl = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"thinking","thinking":"let me think..."}]},"timestamp":"2026-03-07T10:00:03Z","uuid":"jkl"}"#;
        let events = parse_session_events(jsonl);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].summary.as_deref(), Some("thinking…"));
    }

    #[test]
    fn filters_out_progress_and_snapshots() {
        let jsonl = "
{\"type\":\"progress\",\"timestamp\":\"2026-03-07T10:00:00Z\"}
{\"type\":\"file-history-snapshot\",\"messageId\":\"abc\",\"snapshot\":{}}
{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"hello\"},\"timestamp\":\"2026-03-07T10:00:01Z\",\"uuid\":\"1\"}
";
        let events = parse_session_events(jsonl);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event_type, "user");
    }

    #[test]
    fn filters_out_meta_commands() {
        let jsonl = r#"{"type":"user","message":{"role":"user","content":"<local-command-caveat>..."},"isMeta":true,"uuid":"1","timestamp":"2026-03-07T10:00:00Z"}"#;
        let events = parse_session_events(jsonl);
        assert_eq!(events.len(), 0);
    }

    #[test]
    fn truncates_long_content() {
        let long_msg = "x".repeat(300);
        let jsonl = format!(
            r#"{{"type":"user","message":{{"role":"user","content":"{long_msg}"}},"timestamp":"2026-03-07T10:00:00Z","uuid":"1"}}"#
        );
        let events = parse_session_events(&jsonl);
        assert_eq!(events.len(), 1);
        let summary = events[0].summary.as_ref().unwrap();
        assert!(summary.len() < 210);
        assert!(summary.ends_with('…'));
    }

    #[test]
    fn extract_project_name_from_path() {
        assert_eq!(
            extract_project_name("/home/user/.claude/projects/-Users-peter-oss-foo/abc.jsonl"),
            "-Users-peter-oss-foo"
        );
    }

    #[test]
    fn parse_with_model() {
        let jsonl = r#"{"type":"assistant","message":{"role":"assistant","model":"claude-opus-4-6","content":[{"type":"text","text":"hi"}]},"timestamp":"2026-03-07T10:00:00Z","uuid":"1"}"#;
        let events = parse_session_events(jsonl);
        assert_eq!(events[0].model.as_deref(), Some("claude-opus-4-6"));
    }

    #[test]
    fn empty_jsonl_returns_empty() {
        let events = parse_session_events("");
        assert!(events.is_empty());
    }

    #[test]
    fn invalid_json_lines_skipped() {
        let jsonl = "not json\n{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"hello\"},\"uuid\":\"1\",\"timestamp\":\"t\"}\nalso not json\n";
        let events = parse_session_events(jsonl);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].summary.as_deref(), Some("hello"));
    }

    #[test]
    fn system_event_parsed() {
        let jsonl = r#"{"type":"system","subtype":"stop_hook_summary","timestamp":"2026-03-07T10:00:00Z","uuid":"1"}"#;
        let events = parse_session_events(jsonl);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event_type, "system");
        assert_eq!(events[0].summary.as_deref(), Some("system event"));
    }

    #[test]
    fn queue_operation_has_no_summary() {
        let jsonl = r#"{"type":"queue-operation","operation":"enqueue","content":"do something","timestamp":"2026-03-07T10:00:00Z"}"#;
        let events = parse_session_events(jsonl);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event_type, "queue-operation");
        assert!(events[0].summary.is_none());
    }

    #[test]
    fn tool_result_user_message_has_summary() {
        let jsonl = r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"ok"}]},"uuid":"1","timestamp":"t"}"#;
        let events = parse_session_events(jsonl);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].role.as_deref(), Some("user"));
        assert_eq!(events[0].summary.as_deref(), Some("ok"));
    }

    #[test]
    fn tool_result_error_shows_prefix() {
        let jsonl = r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"command not found","is_error":true}]},"uuid":"1","timestamp":"t"}"#;
        let events = parse_session_events(jsonl);
        assert_eq!(
            events[0].summary.as_deref(),
            Some("(error) command not found")
        );
    }

    #[test]
    fn tool_result_empty_content() {
        let jsonl = r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":""}]},"uuid":"1","timestamp":"t"}"#;
        let events = parse_session_events(jsonl);
        assert_eq!(events[0].summary.as_deref(), Some("(empty result)"));
    }

    #[test]
    fn tool_result_array_content() {
        let jsonl = r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":[{"type":"text","text":"file contents here"}]}]},"uuid":"1","timestamp":"t"}"#;
        let events = parse_session_events(jsonl);
        assert_eq!(events[0].summary.as_deref(), Some("file contents here"));
    }

    #[test]
    fn tool_use_bash_shows_command() {
        let jsonl = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","name":"Bash","id":"t1","input":{"command":"ls -la /tmp"}}]},"timestamp":"t","uuid":"1"}"#;
        let events = parse_session_events(jsonl);
        assert_eq!(events[0].summary.as_deref(), Some("ls -la /tmp"));
        assert_eq!(events[0].tool_name.as_deref(), Some("Bash"));
    }

    #[test]
    fn tool_use_read_shows_file_path() {
        let jsonl = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","name":"Read","id":"t1","input":{"file_path":"/foo/bar.rs"}}]},"timestamp":"t","uuid":"1"}"#;
        let events = parse_session_events(jsonl);
        assert_eq!(events[0].summary.as_deref(), Some("/foo/bar.rs"));
    }

    #[test]
    fn tool_use_grep_shows_pattern() {
        let jsonl = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","name":"Grep","id":"t1","input":{"pattern":"fn main"}}]},"timestamp":"t","uuid":"1"}"#;
        let events = parse_session_events(jsonl);
        assert_eq!(events[0].summary.as_deref(), Some("fn main"));
    }

    #[test]
    fn tool_use_unknown_tool_no_input() {
        let jsonl = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","name":"CustomTool","id":"t1","input":{"stuff":123}}]},"timestamp":"t","uuid":"1"}"#;
        let events = parse_session_events(jsonl);
        assert_eq!(events[0].summary.as_deref(), Some("tool_use: CustomTool"));
    }

    #[test]
    fn tool_use_multiline_bash_truncated() {
        let jsonl = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","name":"Bash","id":"t1","input":{"command":"echo hello\necho world\necho done"}}]},"timestamp":"t","uuid":"1"}"#;
        let events = parse_session_events(jsonl);
        assert_eq!(events[0].summary.as_deref(), Some("echo hello…"));
    }

    #[test]
    fn compact_tool_input_basic() {
        let input = serde_json::json!({"command": "git status"});
        assert_eq!(
            compact_tool_input("Bash", &input),
            Some("git status".to_string())
        );

        let input = serde_json::json!({"file_path": "/src/main.rs"});
        assert_eq!(
            compact_tool_input("Read", &input),
            Some("/src/main.rs".to_string())
        );

        let input = serde_json::json!({});
        assert_eq!(compact_tool_input("Bash", &input), None);
    }

    #[test]
    fn session_event_serialization_skips_none_fields() {
        let event = SessionEvent {
            event_type: "user".to_string(),
            role: Some("user".to_string()),
            timestamp: None,
            summary: Some("hello".to_string()),
            model: None,
            tool_name: None,
            uuid: None,
        };
        let json = serde_json::to_string(&event).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert!(v.get("timestamp").is_none());
        assert!(v.get("model").is_none());
        assert!(v.get("tool_name").is_none());
        assert_eq!(v["type"], "user");
        assert_eq!(v["summary"], "hello");
    }

    #[test]
    fn extract_project_name_no_projects_dir() {
        assert_eq!(extract_project_name("/some/random/path.jsonl"), "unknown");
    }
}
