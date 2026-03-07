/// Tool name filtering with allow/deny lists.
///
/// Same model as the domain allowlist: deny overrides allow.
/// Empty allow list = no tools allowed. Use `["*"]` to allow all.
pub struct ToolFilter {
    allow: Vec<ToolPattern>,
    deny: Vec<ToolPattern>,
}

enum ToolPattern {
    Exact(String),
    Prefix(String), // "search_*" → stores "search_"
}

impl ToolFilter {
    pub fn new(allow: &[String], deny: &[String]) -> Self {
        Self {
            allow: allow.iter().map(|s| ToolPattern::parse(s)).collect(),
            deny: deny.iter().map(|s| ToolPattern::parse(s)).collect(),
        }
    }

    /// Check if a tool name is allowed. Deny overrides allow.
    pub fn is_allowed(&self, name: &str) -> bool {
        if self.deny.iter().any(|p| p.matches(name)) {
            return false;
        }
        self.allow.iter().any(|p| p.matches(name))
    }
}

impl ToolPattern {
    fn parse(pattern: &str) -> Self {
        if let Some(prefix) = pattern.strip_suffix('*') {
            ToolPattern::Prefix(prefix.to_string())
        } else {
            ToolPattern::Exact(pattern.to_string())
        }
    }

    fn matches(&self, name: &str) -> bool {
        match self {
            ToolPattern::Exact(exact) => name == exact,
            ToolPattern::Prefix(prefix) => name.starts_with(prefix.as_str()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn exact_match() {
        let f = ToolFilter::new(&s(&["read_file"]), &[]);
        assert!(f.is_allowed("read_file"));
        assert!(!f.is_allowed("write_file"));
    }

    #[test]
    fn wildcard_prefix() {
        let f = ToolFilter::new(&s(&["search_*"]), &[]);
        assert!(f.is_allowed("search_code"));
        assert!(f.is_allowed("search_files"));
        assert!(!f.is_allowed("read_file"));
    }

    #[test]
    fn empty_allow_denies_all() {
        let f = ToolFilter::new(&[], &[]);
        assert!(!f.is_allowed("anything"));
    }

    #[test]
    fn star_allows_all() {
        let f = ToolFilter::new(&s(&["*"]), &[]);
        assert!(f.is_allowed("anything"));
        assert!(f.is_allowed(""));
    }

    #[test]
    fn deny_overrides_allow() {
        let f = ToolFilter::new(&s(&["*"]), &s(&["delete_*"]));
        assert!(f.is_allowed("read_file"));
        assert!(!f.is_allowed("delete_repo"));
        assert!(!f.is_allowed("delete_file"));
    }

    #[test]
    fn tool_with_special_chars() {
        let f = ToolFilter::new(&s(&["get/pull_request", "list-issues"]), &[]);
        assert!(f.is_allowed("get/pull_request"));
        assert!(f.is_allowed("list-issues"));
        assert!(!f.is_allowed("get/push_request"));
    }

    #[test]
    fn prefix_pattern_no_match_on_empty() {
        let f = ToolFilter::new(&s(&["read_*"]), &[]);
        assert!(!f.is_allowed(""));
        assert!(f.is_allowed("read_file"));
    }
}
