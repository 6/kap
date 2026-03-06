use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::Path;

#[derive(Debug, Default, Deserialize, Clone)]
pub struct Config {
    #[serde(default)]
    pub proxy: ProxyConfig,
    pub mcp: Option<McpConfig>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct ProxyConfig {
    #[serde(default = "default_proxy_listen")]
    pub listen: String,
    #[serde(default)]
    pub network: NetworkConfig,
    #[serde(default)]
    pub observe: ObserveConfig,
}

#[derive(Debug, Default, Deserialize, Clone)]
pub struct NetworkConfig {
    #[serde(default)]
    pub allow: Vec<String>,
    #[serde(default)]
    pub deny: Vec<String>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct ObserveConfig {
    #[serde(default = "default_observe_log")]
    pub log: String,
}

impl Config {
    pub fn load(path: &str) -> Result<Self> {
        let path = Path::new(path);
        if path.exists() {
            let content = std::fs::read_to_string(path)
                .with_context(|| format!("reading {}", path.display()))?;
            toml::from_str(&content).with_context(|| format!("parsing {}", path.display()))
        } else {
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

#[derive(Debug, Deserialize, Clone)]
pub struct McpConfig {
    #[serde(default = "default_mcp_listen")]
    pub listen: String,
    #[serde(default = "default_mcp_auth_dir")]
    pub auth_dir: String,
    #[serde(default)]
    pub servers: Vec<McpServerConfig>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct McpServerConfig {
    pub name: String,
    pub upstream: String,
    /// Env var to use as Bearer token (e.g., "GH_TOKEN"). Skips OAuth auth file.
    pub token_env: Option<String>,
    #[serde(default)]
    pub allow_tools: Vec<String>,
    #[serde(default)]
    pub deny_tools: Vec<String>,
}

fn default_proxy_listen() -> String {
    "0.0.0.0:3128".to_string()
}

fn default_mcp_listen() -> String {
    "0.0.0.0:3129".to_string()
}

fn default_mcp_auth_dir() -> String {
    "/etc/devp/auth".to_string()
}

fn default_observe_log() -> String {
    "/var/log/devp/proxy.jsonl".to_string()
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
"#;
        let config: Config = toml::from_str(toml).unwrap();
        assert_eq!(config.proxy.listen, "0.0.0.0:9999");
        assert_eq!(
            config.proxy.network.allow,
            vec!["github.com", "crates.io", "custom.com"]
        );
        assert_eq!(config.proxy.network.deny, vec!["gist.github.com"]);
        assert_eq!(config.proxy.observe.log, "/tmp/test.jsonl");
    }

    #[test]
    fn parse_empty_defaults() {
        let config: Config = toml::from_str("").unwrap();
        assert_eq!(config.proxy.listen, "0.0.0.0:3128");
        assert!(config.proxy.network.allow.is_empty());
        assert!(config.proxy.network.deny.is_empty());
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
    fn parse_mcp_config() {
        let toml = r#"
[mcp]
listen = "0.0.0.0:4000"
auth_dir = "/tmp/auth"

[[mcp.servers]]
name = "github"
upstream = "https://mcp.github.com"
allow_tools = ["get_pull_request", "list_issues"]
deny_tools = ["create_repository"]

[[mcp.servers]]
name = "filesystem"
upstream = "https://mcp.example.com/fs"
"#;
        let config: Config = toml::from_str(toml).unwrap();
        let mcp = config.mcp.unwrap();
        assert_eq!(mcp.listen, "0.0.0.0:4000");
        assert_eq!(mcp.auth_dir, "/tmp/auth");
        assert_eq!(mcp.servers.len(), 2);

        assert_eq!(mcp.servers[0].name, "github");
        assert_eq!(mcp.servers[0].upstream, "https://mcp.github.com");
        assert_eq!(mcp.servers[0].allow_tools, vec!["get_pull_request", "list_issues"]);
        assert_eq!(mcp.servers[0].deny_tools, vec!["create_repository"]);

        assert_eq!(mcp.servers[1].name, "filesystem");
        assert!(mcp.servers[1].allow_tools.is_empty());
    }

    #[test]
    fn no_mcp_config_is_none() {
        let config: Config = toml::from_str("").unwrap();
        assert!(config.mcp.is_none());
    }

    #[test]
    fn parse_mcp_token_env() {
        let toml = r#"
[mcp]

[[mcp.servers]]
name = "github"
upstream = "https://mcp.github.com"
token_env = "GH_TOKEN"
allow_tools = ["get_pull_request"]
"#;
        let config: Config = toml::from_str(toml).unwrap();
        let mcp = config.mcp.unwrap();
        assert_eq!(mcp.servers[0].token_env.as_deref(), Some("GH_TOKEN"));
    }

    #[test]
    fn parse_mcp_no_token_env() {
        let toml = r#"
[mcp]

[[mcp.servers]]
name = "github"
upstream = "https://mcp.github.com"
"#;
        let config: Config = toml::from_str(toml).unwrap();
        let mcp = config.mcp.unwrap();
        assert!(mcp.servers[0].token_env.is_none());
    }
}
