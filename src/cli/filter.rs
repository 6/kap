//! Command allow/deny filtering for CLI proxy.
//!
//! Deny overrides allow (same as domain deny).
//! Empty allow = no commands allowed.

pub struct CommandFilter {
    allow: Vec<CommandPattern>,
    deny: Vec<CommandPattern>,
}

enum CommandPattern {
    Exact(String),
    Prefix(String), // "pr *" stores "pr "
}

impl CommandFilter {
    pub fn new(allow: &[String], deny: &[String]) -> Self {
        Self {
            allow: allow.iter().map(|s| CommandPattern::parse(s)).collect(),
            deny: deny.iter().map(|s| CommandPattern::parse(s)).collect(),
        }
    }

    pub fn is_allowed(&self, args: &[String]) -> bool {
        if args.is_empty() {
            return false;
        }
        let joined = args.join(" ");
        // Deny: exact match on joined string OR prefix match on first arg
        if self
            .deny
            .iter()
            .any(|p| p.matches(&joined) || p.matches_first_arg(&args[0]))
        {
            return false;
        }
        self.allow.iter().any(|p| p.matches(&joined))
    }
}

impl CommandPattern {
    fn parse(pattern: &str) -> Self {
        if let Some(prefix) = pattern.strip_suffix(" *") {
            CommandPattern::Prefix(format!("{prefix} "))
        } else if pattern.ends_with('*') {
            CommandPattern::Prefix(pattern.strip_suffix('*').unwrap().to_string())
        } else {
            CommandPattern::Exact(pattern.to_string())
        }
    }

    fn matches(&self, command: &str) -> bool {
        match self {
            CommandPattern::Exact(exact) => command == exact,
            CommandPattern::Prefix(prefix) => {
                if prefix.is_empty() {
                    return true;
                }
                command.starts_with(prefix.as_str()) || command == prefix.trim_end()
            }
        }
    }

    /// For deny patterns: an exact pattern like "api" should deny "api anything".
    fn matches_first_arg(&self, first_arg: &str) -> bool {
        match self {
            CommandPattern::Exact(exact) => first_arg == exact,
            CommandPattern::Prefix(_) => false, // already handled by matches()
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
        let f = CommandFilter::new(&s(&["repo view"]), &[]);
        assert!(f.is_allowed(&s(&["repo", "view"])));
        assert!(!f.is_allowed(&s(&["repo", "list"])));
    }

    #[test]
    fn prefix_match() {
        let f = CommandFilter::new(&s(&["pr *"]), &[]);
        assert!(f.is_allowed(&s(&["pr", "view", "123"])));
        assert!(f.is_allowed(&s(&["pr", "list"])));
        assert!(f.is_allowed(&s(&["pr"])));
        assert!(!f.is_allowed(&s(&["issue", "list"])));
    }

    #[test]
    fn deny_overrides_allow() {
        let f = CommandFilter::new(&s(&["*"]), &s(&["auth *", "api"]));
        assert!(f.is_allowed(&s(&["pr", "view"])));
        assert!(!f.is_allowed(&s(&["auth", "token"])));
        assert!(!f.is_allowed(&s(&["auth", "login"])));
        assert!(!f.is_allowed(&s(&["api", "/repos"])));
    }

    #[test]
    fn exact_deny_allows_siblings() {
        let f = CommandFilter::new(&s(&["*"]), &s(&["auth token", "auth login", "api"]));
        assert!(f.is_allowed(&s(&["auth", "status"])));
        assert!(!f.is_allowed(&s(&["auth", "token"])));
        assert!(!f.is_allowed(&s(&["api", "/repos"])));
    }

    #[test]
    fn star_allows_all() {
        let f = CommandFilter::new(&s(&["*"]), &[]);
        assert!(f.is_allowed(&s(&["anything"])));
    }

    #[test]
    fn empty_allow_denies_all() {
        let f = CommandFilter::new(&[], &[]);
        assert!(!f.is_allowed(&s(&["pr", "view"])));
    }

    #[test]
    fn empty_args_denied() {
        let f = CommandFilter::new(&s(&["*"]), &[]);
        assert!(!f.is_allowed(&[]));
    }

    #[test]
    fn multiple_patterns() {
        let f = CommandFilter::new(&s(&["pr *", "issue *", "repo view"]), &[]);
        assert!(f.is_allowed(&s(&["pr", "create"])));
        assert!(f.is_allowed(&s(&["issue", "list"])));
        assert!(f.is_allowed(&s(&["repo", "view"])));
        assert!(!f.is_allowed(&s(&["repo", "delete"])));
    }
}
