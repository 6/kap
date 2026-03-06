use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Deserialize, Serialize)]
pub struct Request {
    pub jsonrpc: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<Value>,
    pub method: String,
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub params: Value,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct Response {
    pub jsonrpc: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<Error>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct Error {
    pub code: i64,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl Response {
    pub fn error(id: Option<Value>, code: i64, message: impl Into<String>) -> Self {
        Self {
            jsonrpc: "2.0".to_string(),
            id,
            result: None,
            error: Some(Error {
                code,
                message: message.into(),
                data: None,
            }),
        }
    }
}

/// Extract tool name from a tools/call request params.
pub fn tool_call_name(params: &Value) -> Option<&str> {
    params.get("name").and_then(|v| v.as_str())
}

/// Filter a tools/list response, keeping only tools that pass the predicate.
pub fn filter_tools_list(result: &mut Value, predicate: impl Fn(&str) -> bool) {
    if let Some(tools) = result.get_mut("tools").and_then(|t| t.as_array_mut()) {
        tools.retain(|tool| {
            tool.get("name")
                .and_then(|n| n.as_str())
                .map(&predicate)
                .unwrap_or(true)
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_request() {
        let json = r#"{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{}}"#;
        let req: Request = serde_json::from_str(json).unwrap();
        assert_eq!(req.method, "tools/list");
        assert_eq!(req.id, Some(Value::from(1)));
    }

    #[test]
    fn parse_notification() {
        let json = r#"{"jsonrpc":"2.0","method":"initialized"}"#;
        let req: Request = serde_json::from_str(json).unwrap();
        assert!(req.id.is_none());
        assert_eq!(req.method, "initialized");
    }

    #[test]
    fn tool_call_name_extraction() {
        let params = serde_json::json!({"name": "get_weather", "arguments": {}});
        assert_eq!(tool_call_name(&params), Some("get_weather"));

        let empty = serde_json::json!({});
        assert_eq!(tool_call_name(&empty), None);
    }

    #[test]
    fn filter_tools_list_removes_denied() {
        let mut result = serde_json::json!({
            "tools": [
                {"name": "read_file", "description": "read"},
                {"name": "write_file", "description": "write"},
                {"name": "delete_file", "description": "delete"},
            ]
        });
        filter_tools_list(&mut result, |name| name == "read_file");
        let tools = result["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["name"], "read_file");
    }

    #[test]
    fn error_response_serializes() {
        let resp = Response::error(Some(Value::from(42)), -32602, "Tool denied");
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("-32602"));
        assert!(json.contains("Tool denied"));
    }

    #[test]
    fn batch_array_fails_to_parse() {
        let json = r#"[{"jsonrpc":"2.0","id":1,"method":"tools/list"}]"#;
        assert!(serde_json::from_str::<Request>(json).is_err());
    }

    #[test]
    fn request_with_string_id() {
        let json = r#"{"jsonrpc":"2.0","id":"abc-123","method":"tools/call","params":{"name":"test"}}"#;
        let req: Request = serde_json::from_str(json).unwrap();
        assert_eq!(req.id, Some(Value::from("abc-123")));
        assert_eq!(req.method, "tools/call");
    }

    #[test]
    fn filter_tools_list_no_tools_key() {
        let mut result = serde_json::json!({"other": "data"});
        filter_tools_list(&mut result, |_| false);
        assert_eq!(result, serde_json::json!({"other": "data"}));
    }

    #[test]
    fn filter_tools_list_retains_nameless_tools() {
        let mut result = serde_json::json!({
            "tools": [
                {"name": "read_file", "description": "read"},
                {"description": "no name field"},
            ]
        });
        filter_tools_list(&mut result, |name| name == "read_file");
        let tools = result["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 2);
    }

    #[test]
    fn error_response_with_none_id() {
        let resp = Response::error(None, -32600, "Invalid request");
        let json = serde_json::to_string(&resp).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert!(parsed.get("id").is_none());
        assert_eq!(parsed["error"]["code"], -32600);
    }
}
