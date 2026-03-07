//! Command allowlist for the gh CLI proxy.
//!
//! `auth` and `api` are always denied (credential leak / raw API access).
//! User-configured allow list uses prefix/exact matching on space-joined args.
//! Empty allow list = no commands allowed.

/// First-arg commands that are always blocked.
const ALWAYS_DENIED: &[&str] = &["api"];

/// The only `auth` subcommand that is allowed.
const ALLOWED_AUTH_SUBCOMMANDS: &[&str] = &["status"];

pub struct GhCommandFilter {
    allow: Vec<CommandPattern>,
}

enum CommandPattern {
    Exact(String),
    Prefix(String), // "pr *" stores "pr "
}

impl GhCommandFilter {
    pub fn new(allow: &[String]) -> Self {
        Self {
            allow: allow.iter().map(|s| CommandPattern::parse(s)).collect(),
        }
    }

    pub fn is_allowed(&self, args: &[String]) -> bool {
        if args.is_empty() {
            return false;
        }
        // Hard deny: blocked top-level commands
        if ALWAYS_DENIED.contains(&args[0].as_str()) {
            return false;
        }
        // auth: only specific subcommands are allowed
        if args[0] == "auth" {
            let sub = args.get(1).map(|s| s.as_str()).unwrap_or("");
            if !ALLOWED_AUTH_SUBCOMMANDS.contains(&sub) {
                return false;
            }
        }
        let joined = args.join(" ");
        self.allow.iter().any(|p| p.matches(&joined))
    }
}

impl CommandPattern {
    fn parse(pattern: &str) -> Self {
        if let Some(prefix) = pattern.strip_suffix(" *") {
            CommandPattern::Prefix(format!("{prefix} "))
        } else if pattern.ends_with('*') {
            // bare wildcard like "*"
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
                    return true; // bare "*" matches everything
                }
                command.starts_with(prefix.as_str()) || command == prefix.trim_end()
            }
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
        let f = GhCommandFilter::new(&s(&["repo view"]));
        assert!(f.is_allowed(&s(&["repo", "view"])));
        assert!(!f.is_allowed(&s(&["repo", "list"])));
    }

    #[test]
    fn prefix_match() {
        let f = GhCommandFilter::new(&s(&["pr *"]));
        assert!(f.is_allowed(&s(&["pr", "view", "123"])));
        assert!(f.is_allowed(&s(&["pr", "list"])));
        assert!(f.is_allowed(&s(&["pr"])));
        assert!(!f.is_allowed(&s(&["issue", "list"])));
    }

    #[test]
    fn star_allows_all_non_denied() {
        let f = GhCommandFilter::new(&s(&["*"]));
        assert!(f.is_allowed(&s(&["pr", "view"])));
        assert!(f.is_allowed(&s(&["repo", "list"])));
        // auth and api still denied
        assert!(!f.is_allowed(&s(&["auth", "token"])));
        assert!(!f.is_allowed(&s(&["api", "/repos"])));
    }

    #[test]
    fn auth_mostly_denied() {
        let f = GhCommandFilter::new(&s(&["*"]));
        assert!(!f.is_allowed(&s(&["auth", "token"])));
        assert!(!f.is_allowed(&s(&["auth", "login"])));
        assert!(!f.is_allowed(&s(&["auth", "setup-git"])));
        assert!(!f.is_allowed(&s(&["auth", "logout"])));
        assert!(!f.is_allowed(&s(&["auth"])));
        // auth status is the only allowed auth subcommand
        assert!(f.is_allowed(&s(&["auth", "status"])));
    }

    #[test]
    fn api_always_denied() {
        let f = GhCommandFilter::new(&s(&["*"]));
        assert!(!f.is_allowed(&s(&["api", "/repos/owner/repo"])));
        assert!(!f.is_allowed(&s(&["api", "graphql"])));
    }

    #[test]
    fn empty_allow_denies_all() {
        let f = GhCommandFilter::new(&[]);
        assert!(!f.is_allowed(&s(&["pr", "view"])));
    }

    #[test]
    fn empty_args_denied() {
        let f = GhCommandFilter::new(&s(&["*"]));
        assert!(!f.is_allowed(&[]));
    }

    #[test]
    fn multiple_allow_patterns() {
        let f = GhCommandFilter::new(&s(&["pr *", "issue *", "repo view"]));
        assert!(f.is_allowed(&s(&["pr", "create"])));
        assert!(f.is_allowed(&s(&["issue", "list"])));
        assert!(f.is_allowed(&s(&["repo", "view"])));
        assert!(!f.is_allowed(&s(&["repo", "delete"])));
    }
}
