use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::Path;

#[derive(Debug, Default, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub proxy: ProxyConfig,
    #[serde(default)]
    pub credentials: CredentialConfig,
}

#[derive(Debug, Deserialize)]
pub struct ProxyConfig {
    #[serde(default = "default_proxy_listen")]
    pub listen: String,
    #[serde(default)]
    pub network: NetworkConfig,
    #[serde(default)]
    pub observe: ObserveConfig,
}

#[derive(Debug, Default, Deserialize)]
pub struct NetworkConfig {
    #[serde(default)]
    pub allow: Vec<String>,
    #[serde(default)]
    pub deny: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct ObserveConfig {
    #[serde(default = "default_observe_log")]
    pub log: String,
}

#[derive(Debug, Deserialize)]
pub struct CredentialConfig {
    #[allow(dead_code)]
    #[serde(default = "default_socket")]
    pub socket: String,
    #[serde(default = "default_host_socket")]
    pub host_socket: String,
    #[serde(default)]
    pub github: GitHubCredentialConfig,
}

#[derive(Debug, Deserialize)]
pub struct GitHubCredentialConfig {
    #[serde(default = "default_github_hosts")]
    pub hosts: Vec<String>,
}

impl Config {
    pub fn load(path: &str) -> Result<Self> {
        let path = Path::new(path);
        if path.exists() {
            let content = std::fs::read_to_string(path)
                .with_context(|| format!("reading {}", path.display()))?;
            toml::from_str(&content).with_context(|| format!("parsing {}", path.display()))
        } else {
            // Return defaults if no config file exists
            Ok(Self::default())
        }
    }

    pub fn allow_domains(&self) -> &[String] {
        &self.proxy.network.allow
    }
}

impl Default for ProxyConfig {
    fn default() -> Self {
        Self {
            listen: default_proxy_listen(),
            network: NetworkConfig::default(),
            observe: ObserveConfig::default(),
        }
    }
}

impl Default for ObserveConfig {
    fn default() -> Self {
        Self {
            log: default_observe_log(),
        }
    }
}

impl Default for CredentialConfig {
    fn default() -> Self {
        Self {
            socket: default_socket(),
            host_socket: default_host_socket(),
            github: GitHubCredentialConfig::default(),
        }
    }
}

impl Default for GitHubCredentialConfig {
    fn default() -> Self {
        Self {
            hosts: default_github_hosts(),
        }
    }
}

fn default_proxy_listen() -> String {
    "0.0.0.0:3128".to_string()
}

fn default_observe_log() -> String {
    "/var/log/devp/proxy.jsonl".to_string()
}

fn default_socket() -> String {
    "/devp-sockets/cred.sock".to_string()
}

fn default_host_socket() -> String {
    shellexpand_tilde("~/.devp-sockets/cred.sock")
}

fn default_github_hosts() -> Vec<String> {
    vec!["github.com".to_string()]
}

#[allow(clippy::collapsible_if)]
fn shellexpand_tilde(path: &str) -> String {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            return format!("{}/{rest}", home.to_string_lossy());
        }
    }
    path.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_full_config() {
        let toml = r#"
[proxy]
listen = "0.0.0.0:9999"

[proxy.network]
allow = ["github.com", "crates.io", "custom.com"]
deny = ["gist.github.com"]

[proxy.observe]
log = "/tmp/test.jsonl"

[credentials]
socket = "/tmp/cred.sock"
host_socket = "/tmp/host-cred.sock"

[credentials.github]
hosts = ["github.com", "github.example.com"]
"#;
        let config: Config = toml::from_str(toml).unwrap();
        assert_eq!(config.proxy.listen, "0.0.0.0:9999");
        assert_eq!(
            config.proxy.network.allow,
            vec!["github.com", "crates.io", "custom.com"]
        );
        assert_eq!(config.proxy.network.deny, vec!["gist.github.com"]);
        assert_eq!(config.proxy.observe.log, "/tmp/test.jsonl");
        assert_eq!(config.credentials.socket, "/tmp/cred.sock");
        assert_eq!(config.credentials.github.hosts.len(), 2);
    }

    #[test]
    fn parse_empty_defaults() {
        let config: Config = toml::from_str("").unwrap();
        assert_eq!(config.proxy.listen, "0.0.0.0:3128");
        assert!(config.proxy.network.allow.is_empty());
        assert!(config.proxy.network.deny.is_empty());
        assert_eq!(config.credentials.socket, "/devp-sockets/cred.sock");
        assert_eq!(config.credentials.github.hosts, vec!["github.com"]);
    }

    #[test]
    fn allow_domains_returns_allow_list() {
        let toml = r#"
[proxy.network]
allow = ["github.com", "custom.com"]
"#;
        let config: Config = toml::from_str(toml).unwrap();
        let domains = config.allow_domains();
        assert_eq!(domains.len(), 2);
        assert_eq!(domains[0], "github.com");
        assert_eq!(domains[1], "custom.com");
    }

    #[test]
    fn tilde_expansion() {
        let expanded = shellexpand_tilde("~/foo/bar");
        assert!(!expanded.starts_with("~/"));
        assert!(expanded.ends_with("/foo/bar"));

        let unchanged = shellexpand_tilde("/absolute/path");
        assert_eq!(unchanged, "/absolute/path");
    }
}
