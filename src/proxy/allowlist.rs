/// Domain allowlist with wildcard matching.
///
/// Supports exact matches ("github.com") and wildcard prefixes ("*.github.com").
/// Deny rules take precedence over allow rules.

pub struct Allowlist {
    allow: Vec<DomainPattern>,
    deny: Vec<DomainPattern>,
}

enum DomainPattern {
    Exact(String),
    Wildcard(String), // stores the suffix, e.g. ".github.com" for "*.github.com"
}

impl Allowlist {
    pub fn new(allow: &[String], deny: &[String]) -> Self {
        Self {
            allow: allow.iter().map(|s| DomainPattern::parse(s)).collect(),
            deny: deny.iter().map(|s| DomainPattern::parse(s)).collect(),
        }
    }

    /// Check if a domain is allowed. Deny rules take precedence.
    pub fn is_allowed(&self, domain: &str) -> bool {
        let domain = domain.to_lowercase();
        // Strip port if present
        let domain = domain.split(':').next().unwrap_or(&domain);

        if self.deny.iter().any(|p| p.matches(domain)) {
            return false;
        }
        self.allow.iter().any(|p| p.matches(domain))
    }
}

impl DomainPattern {
    fn parse(pattern: &str) -> Self {
        let pattern = pattern.to_lowercase();
        if let Some(suffix) = pattern.strip_prefix("*.") {
            DomainPattern::Wildcard(format!(".{suffix}"))
        } else if pattern.starts_with('*') {
            DomainPattern::Wildcard(pattern[1..].to_string())
        } else {
            DomainPattern::Exact(pattern)
        }
    }

    fn matches(&self, domain: &str) -> bool {
        match self {
            DomainPattern::Exact(exact) => domain == exact,
            DomainPattern::Wildcard(suffix) => {
                // "*.github.com" matches "foo.github.com" but not "github.com"
                domain.ends_with(suffix.as_str())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn allow(domains: &[&str]) -> Allowlist {
        Allowlist::new(
            &domains.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
            &[],
        )
    }

    fn allow_deny(allow_domains: &[&str], deny_domains: &[&str]) -> Allowlist {
        Allowlist::new(
            &allow_domains
                .iter()
                .map(|s| s.to_string())
                .collect::<Vec<_>>(),
            &deny_domains
                .iter()
                .map(|s| s.to_string())
                .collect::<Vec<_>>(),
        )
    }

    #[test]
    fn exact_match() {
        let al = allow(&["github.com"]);
        assert!(al.is_allowed("github.com"));
        assert!(!al.is_allowed("evil.com"));
    }

    #[test]
    fn case_insensitive() {
        let al = allow(&["GitHub.com"]);
        assert!(al.is_allowed("github.com"));
        assert!(al.is_allowed("GITHUB.COM"));
    }

    #[test]
    fn wildcard_match() {
        let al = allow(&["*.github.com"]);
        assert!(al.is_allowed("api.github.com"));
        assert!(al.is_allowed("raw.githubusercontent.github.com"));
        // Wildcard does NOT match the bare domain
        assert!(!al.is_allowed("github.com"));
    }

    #[test]
    fn strips_port() {
        let al = allow(&["github.com"]);
        assert!(al.is_allowed("github.com:443"));
    }

    #[test]
    fn deny_overrides_allow() {
        let al = allow_deny(&["*.github.com", "github.com"], &["gist.github.com"]);
        assert!(al.is_allowed("api.github.com"));
        assert!(al.is_allowed("github.com"));
        assert!(!al.is_allowed("gist.github.com"));
    }

    #[test]
    fn empty_allowlist_denies_all() {
        let al = allow(&[]);
        assert!(!al.is_allowed("github.com"));
    }

    #[test]
    fn subdomain_granularity() {
        let al = allow(&["github.com", "api.github.com"]);
        assert!(al.is_allowed("github.com"));
        assert!(al.is_allowed("api.github.com"));
        assert!(!al.is_allowed("gist.github.com"));
    }
}
