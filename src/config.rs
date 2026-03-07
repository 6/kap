use anyhow::{Context, Result};
use serde::Deserialize;
use std::collections::HashMap;
use std::path::Path;

#[derive(Debug, Default, Deserialize, Clone)]
pub struct Config {
    #[serde(default)]
    pub proxy: ProxyConfig,
    pub mcp: Option<McpConfig>,
    pub compose: Option<ComposeConfig>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct ComposeConfig {
    pub image: Option<String>,
    pub build: Option<ComposeBuild>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct ComposeBuild {
    pub context: String,
    pub dockerfile: Option<String>,
    pub target: Option<String>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct ProxyConfig {
    #[serde(default = "default_proxy_listen")]
    pub listen: String,
    #[serde(default = "default_dns_listen")]
    pub dns_listen: String,
    #[serde(default = "default_dns_upstream")]
    pub dns_upstream: String,
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

    /// Collect domains from MCP server upstream URLs (from config and auth files).
    /// These are implicitly allowed so the MCP proxy can reach its upstreams.
    pub fn mcp_upstream_domains(&self) -> Vec<String> {
        let Some(ref mcp) = self.mcp else {
            return Vec::new();
        };
        let auth_dir = &mcp.auth_dir;
        mcp.servers
            .iter()
            .filter_map(|s| {
                // Try config upstream first, then fall back to auth file
                let upstream = s
                    .upstream
                    .clone()
                    .or_else(|| upstream_from_auth_file(auth_dir, &s.name));
                upstream
                    .and_then(|u| url::Url::parse(&u).ok())
                    .and_then(|u| u.host_str().map(String::from))
            })
            .collect()
    }
}

impl Default for ProxyConfig {
    fn default() -> Self {
        Self {
            listen: default_proxy_listen(),
            dns_listen: default_dns_listen(),
            dns_upstream: default_dns_upstream(),
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
    /// Upstream MCP server URL. Optional if auth file exists (resolved from there).
    pub upstream: Option<String>,
    /// Env var to use as Bearer token (e.g., "GH_TOKEN"). Skips OAuth auth file.
    pub token_env: Option<String>,
    /// Extra headers to send upstream. Values with ${VAR} are expanded from env.
    #[serde(default)]
    pub headers: HashMap<String, String>,
    #[serde(default)]
    pub allow_tools: Vec<String>,
}

/// Read the upstream URL from an auth file.
fn upstream_from_auth_file(auth_dir: &str, name: &str) -> Option<String> {
    let path = std::path::Path::new(auth_dir).join(format!("{name}.json"));
    std::fs::read_to_string(&path)
        .ok()
        .and_then(|content| serde_json::from_str::<serde_json::Value>(&content).ok())
        .and_then(|v| v["upstream"].as_str().map(String::from))
}

const DEFAULT_IMAGE: &str = "ghcr.io/6/devcontainer-guard:latest";

impl ComposeConfig {
    /// Render the YAML for the devg service's image or build section.
    pub fn image_yaml(&self, indent: &str) -> String {
        if let Some(ref build) = self.build {
            let mut lines = vec![format!("{indent}build:")];
            lines.push(format!("{indent}  context: {}", build.context));
            if let Some(ref dockerfile) = build.dockerfile {
                lines.push(format!("{indent}  dockerfile: {dockerfile}"));
            }
            if let Some(ref target) = build.target {
                lines.push(format!("{indent}  target: {target}"));
            }
            lines.join("\n")
        } else {
            let image = self.image.as_deref().unwrap_or(DEFAULT_IMAGE);
            format!("{indent}image: {image}")
        }
    }
}

impl Default for ComposeConfig {
    fn default() -> Self {
        Self {
            image: Some(DEFAULT_IMAGE.to_string()),
            build: None,
        }
    }
}

fn default_proxy_listen() -> String {
    "0.0.0.0:3128".to_string()
}

fn default_dns_listen() -> String {
    "0.0.0.0:53".to_string()
}

fn default_dns_upstream() -> String {
    "8.8.8.8:53".to_string()
}

fn default_mcp_listen() -> String {
    "0.0.0.0:3129".to_string()
}

fn default_mcp_auth_dir() -> String {
    "/etc/devg/auth".to_string()
}

fn default_observe_log() -> String {
    "/var/log/devg/proxy.jsonl".to_string()
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
        assert_eq!(
            mcp.servers[0].upstream.as_deref(),
            Some("https://mcp.github.com")
        );
        assert_eq!(
            mcp.servers[0].allow_tools,
            vec!["get_pull_request", "list_issues"]
        );

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

    #[test]
    fn malformed_toml_returns_error() {
        let result = toml::from_str::<Config>("[proxy\nbroken");
        assert!(result.is_err());
    }

    #[test]
    fn unknown_fields_ignored() {
        let toml = r#"
[proxy]
foo = "bar"
listen = "0.0.0.0:1234"
"#;
        let config: Config = toml::from_str(toml).unwrap();
        assert_eq!(config.proxy.listen, "0.0.0.0:1234");
    }

    #[test]
    fn mcp_headers_parsed() {
        let toml = r#"
[mcp]

[[mcp.servers]]
name = "test"
upstream = "https://example.com"
headers = { "X-Api-Key" = "${API_KEY}", "Accept" = "application/json" }
"#;
        let config: Config = toml::from_str(toml).unwrap();
        let mcp = config.mcp.unwrap();
        assert_eq!(mcp.servers[0].headers.len(), 2);
        assert_eq!(mcp.servers[0].headers["X-Api-Key"], "${API_KEY}");
        assert_eq!(mcp.servers[0].headers["Accept"], "application/json");
    }

    #[test]
    fn mcp_server_upstream_optional() {
        let toml = r#"
[mcp]

[[mcp.servers]]
name = "linear"
"#;
        let config: Config = toml::from_str(toml).unwrap();
        let mcp = config.mcp.unwrap();
        assert_eq!(mcp.servers[0].name, "linear");
        assert!(mcp.servers[0].upstream.is_none());
    }

    #[test]
    fn load_nonexistent_file_returns_default() {
        let config = Config::load("/nonexistent/path/devg.toml").unwrap();
        assert_eq!(config.proxy.listen, "0.0.0.0:3128");
        assert!(config.mcp.is_none());
    }

    #[test]
    fn no_compose_config_is_none() {
        let config: Config = toml::from_str("").unwrap();
        assert!(config.compose.is_none());
    }

    #[test]
    fn parse_compose_image() {
        let toml = r#"
[compose]
image = "myregistry/devg:v1"
"#;
        let config: Config = toml::from_str(toml).unwrap();
        let compose = config.compose.unwrap();
        assert_eq!(compose.image.as_deref(), Some("myregistry/devg:v1"));
        assert!(compose.build.is_none());
    }

    #[test]
    fn parse_compose_build() {
        let toml = r#"
[compose]
build = { context = "..", dockerfile = ".devcontainer/Dockerfile", target = "proxy" }
"#;
        let config: Config = toml::from_str(toml).unwrap();
        let compose = config.compose.unwrap();
        assert!(compose.image.is_none());
        let build = compose.build.unwrap();
        assert_eq!(build.context, "..");
        assert_eq!(
            build.dockerfile.as_deref(),
            Some(".devcontainer/Dockerfile")
        );
        assert_eq!(build.target.as_deref(), Some("proxy"));
    }

    #[test]
    fn compose_image_yaml_default() {
        let compose = ComposeConfig::default();
        let yaml = compose.image_yaml("    ");
        assert_eq!(yaml, format!("    image: {DEFAULT_IMAGE}"));
    }

    #[test]
    fn compose_image_yaml_build() {
        let compose = ComposeConfig {
            image: None,
            build: Some(ComposeBuild {
                context: "..".to_string(),
                dockerfile: Some(".devcontainer/Dockerfile".to_string()),
                target: Some("proxy".to_string()),
            }),
        };
        let yaml = compose.image_yaml("    ");
        assert!(yaml.contains("    build:"));
        assert!(yaml.contains("      context: .."));
        assert!(yaml.contains("      dockerfile: .devcontainer/Dockerfile"));
        assert!(yaml.contains("      target: proxy"));
    }
}
