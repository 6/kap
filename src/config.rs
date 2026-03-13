use anyhow::{Context, Result};
use serde::Deserialize;
use std::collections::HashMap;
use std::path::Path;

#[derive(Debug, Default, Deserialize, Clone)]
pub struct Config {
    /// Forward the host SSH agent into the container (default: true).
    #[serde(default = "default_true")]
    pub ssh_agent: bool,
    #[serde(default)]
    pub proxy: ProxyConfig,
    pub mcp: Option<McpConfig>,
    pub compose: Option<ComposeConfig>,
    pub cli: Option<CliConfig>,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Deserialize, Clone)]
#[allow(dead_code)] // listen is deserialized but read by the CLI proxy via hardcoded constant
pub struct CliConfig {
    #[serde(default = "default_cli_listen")]
    pub listen: String,
    #[serde(default)]
    pub tools: Vec<CliToolConfig>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct CliToolConfig {
    pub name: String,
    #[serde(default)]
    pub mode: CliToolMode,
    #[serde(default)]
    pub allow: Vec<String>,
    #[serde(default)]
    pub deny: Vec<String>,
    #[serde(default)]
    pub env: Vec<String>,
}

#[derive(Debug, Deserialize, Clone, Default, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum CliToolMode {
    #[default]
    Proxy,
    Direct,
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

/// Well-known paths for the global config file.
/// Checked in order; the first that exists wins.
const GLOBAL_CONFIG_PATHS: &[&str] = &[
    "/etc/kap/global.toml", // inside sidecar container (mounted from host)
];

/// Return the host-side global config path: ~/.kap/kap.toml
fn home_global_config() -> Option<String> {
    std::env::var("HOME")
        .ok()
        .map(|h| format!("{h}/.kap/kap.toml"))
}

impl Config {
    /// Load project config and merge with global config (~/.kap/kap.toml).
    pub fn load(path: &str) -> Result<Self> {
        let mut config = Self::load_file(path)?;
        if let Some(global) = Self::find_global() {
            config.merge_global(global);
        }
        Ok(config)
    }

    /// Load a single config file without global merge.
    pub(crate) fn load_file(path: &str) -> Result<Self> {
        let path = Path::new(path);
        if path.exists() {
            let content = std::fs::read_to_string(path)
                .with_context(|| format!("reading {}", path.display()))?;
            toml::from_str(&content).with_context(|| format!("parsing {}", path.display()))
        } else {
            Ok(Self::default())
        }
    }

    /// Try well-known global config paths, return the first that exists.
    fn find_global() -> Option<Self> {
        let candidates: Vec<String> = GLOBAL_CONFIG_PATHS
            .iter()
            .map(|s| s.to_string())
            .chain(home_global_config())
            .collect();
        for path in &candidates {
            if Path::new(path).exists()
                && let Ok(cfg) = Self::load_file(path)
            {
                return Some(cfg);
            }
        }
        None
    }

    /// Merge global config into self.
    /// Vec fields are unioned; for named items (CLI tools, MCP servers),
    /// project entries override global entries with the same name.
    fn merge_global(&mut self, global: Config) {
        // Domains: prepend global, project domains come after
        let mut allow = global.proxy.network.allow;
        allow.append(&mut self.proxy.network.allow);
        self.proxy.network.allow = allow;

        let mut deny = global.proxy.network.deny;
        deny.append(&mut self.proxy.network.deny);
        self.proxy.network.deny = deny;

        // CLI tools: add global tools not overridden by project
        if let Some(global_cli) = global.cli {
            let project_cli = self.cli.get_or_insert_with(|| CliConfig {
                listen: default_cli_listen(),
                tools: Vec::new(),
            });
            for gtool in global_cli.tools {
                if !project_cli.tools.iter().any(|t| t.name == gtool.name) {
                    project_cli.tools.push(gtool);
                }
            }
        }

        // MCP servers: add global servers not overridden by project
        if let Some(global_mcp) = global.mcp {
            let project_mcp = self.mcp.get_or_insert_with(|| McpConfig {
                listen: default_mcp_listen(),
                auth_dir: default_mcp_auth_dir(),
                servers: Vec::new(),
            });
            for gserver in global_mcp.servers {
                if !project_mcp.servers.iter().any(|s| s.name == gserver.name) {
                    project_mcp.servers.push(gserver);
                }
            }
        }
    }

    pub fn allow_domains(&self) -> &[String] {
        &self.proxy.network.allow
    }

    /// Collect domains from MCP server upstream URLs (from auth files).
    /// These are implicitly allowed so the MCP proxy can reach its upstreams.
    pub fn mcp_upstream_domains(&self) -> Vec<String> {
        let Some(ref mcp) = self.mcp else {
            return Vec::new();
        };
        let auth_dir = &mcp.auth_dir;
        mcp.servers
            .iter()
            .filter_map(|s| {
                upstream_from_auth_file(auth_dir, &s.name)
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
    /// Extra headers to send upstream. Values with ${VAR} are expanded from env.
    #[serde(default)]
    pub headers: HashMap<String, String>,
    #[serde(default)]
    pub allow: Vec<String>,
    #[serde(default)]
    pub deny: Vec<String>,
}

/// Read the upstream URL from an auth file.
fn upstream_from_auth_file(auth_dir: &str, name: &str) -> Option<String> {
    let path = std::path::Path::new(auth_dir).join(format!("{name}.json"));
    std::fs::read_to_string(&path)
        .ok()
        .and_then(|content| serde_json::from_str::<serde_json::Value>(&content).ok())
        .and_then(|v| v["upstream"].as_str().map(String::from))
}

pub const DEFAULT_IMAGE: &str = "ghcr.io/6/kap:latest";

impl ComposeConfig {
    /// Return the pullable sidecar image, or `None` if using a local build.
    pub fn sidecar_image(&self) -> Option<&str> {
        if self.build.is_some() {
            None
        } else {
            Some(self.image.as_deref().unwrap_or(DEFAULT_IMAGE))
        }
    }

    /// Render the YAML for the kap service's image or build section.
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
    "/etc/kap/auth".to_string()
}

fn default_cli_listen() -> String {
    "0.0.0.0:3130".to_string()
}

fn default_observe_log() -> String {
    "/var/log/kap/proxy.jsonl".to_string()
}

/// Check whether a global config file exists at ~/.kap/kap.toml.
pub fn has_global_config() -> bool {
    home_global_config()
        .map(|p| Path::new(&p).exists())
        .unwrap_or(false)
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
        assert!(config.ssh_agent); // defaults to true
        assert_eq!(config.proxy.listen, "0.0.0.0:3128");
        assert!(config.proxy.network.allow.is_empty());
        assert!(config.proxy.network.deny.is_empty());
    }

    #[test]
    fn ssh_agent_can_be_disabled() {
        let config: Config = toml::from_str("ssh_agent = false").unwrap();
        assert!(!config.ssh_agent);
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
allow = ["get_pull_request", "list_issues"]

[[mcp.servers]]
name = "filesystem"
"#;
        let config: Config = toml::from_str(toml).unwrap();
        let mcp = config.mcp.unwrap();
        assert_eq!(mcp.listen, "0.0.0.0:4000");
        assert_eq!(mcp.auth_dir, "/tmp/auth");
        assert_eq!(mcp.servers.len(), 2);

        assert_eq!(mcp.servers[0].name, "github");
        assert_eq!(
            mcp.servers[0].allow,
            vec!["get_pull_request", "list_issues"]
        );

        assert_eq!(mcp.servers[1].name, "filesystem");
        assert!(mcp.servers[1].allow.is_empty());
    }

    #[test]
    fn no_mcp_config_is_none() {
        let config: Config = toml::from_str("").unwrap();
        assert!(config.mcp.is_none());
    }

    #[test]
    fn parse_mcp_deny() {
        let toml = r#"
[mcp]

[[mcp.servers]]
name = "github"
allow = ["*"]
deny = ["delete_*"]
"#;
        let config: Config = toml::from_str(toml).unwrap();
        let mcp = config.mcp.unwrap();
        assert_eq!(mcp.servers[0].allow, vec!["*"]);
        assert_eq!(mcp.servers[0].deny, vec!["delete_*"]);
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
headers = { "X-Api-Key" = "${API_KEY}", "Accept" = "application/json" }
"#;
        let config: Config = toml::from_str(toml).unwrap();
        let mcp = config.mcp.unwrap();
        assert_eq!(mcp.servers[0].headers.len(), 2);
        assert_eq!(mcp.servers[0].headers["X-Api-Key"], "${API_KEY}");
        assert_eq!(mcp.servers[0].headers["Accept"], "application/json");
    }

    #[test]
    fn load_nonexistent_file_returns_default() {
        let config = Config::load_file("/nonexistent/path/kap.toml").unwrap();
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
image = "myregistry/kap:v1"
"#;
        let config: Config = toml::from_str(toml).unwrap();
        let compose = config.compose.unwrap();
        assert_eq!(compose.image.as_deref(), Some("myregistry/kap:v1"));
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
    fn sidecar_image_default() {
        let compose = ComposeConfig::default();
        assert_eq!(compose.sidecar_image(), Some(DEFAULT_IMAGE));
    }

    #[test]
    fn sidecar_image_custom() {
        let compose = ComposeConfig {
            image: Some("myregistry/kap:v1".to_string()),
            build: None,
        };
        assert_eq!(compose.sidecar_image(), Some("myregistry/kap:v1"));
    }

    #[test]
    fn sidecar_image_build_returns_none() {
        let compose = ComposeConfig {
            image: None,
            build: Some(ComposeBuild {
                context: "..".to_string(),
                dockerfile: None,
                target: None,
            }),
        };
        assert_eq!(compose.sidecar_image(), None);
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

    #[test]
    fn no_cli_config_is_none() {
        let config: Config = toml::from_str("").unwrap();
        assert!(config.cli.is_none());
    }

    #[test]
    fn parse_cli_config() {
        let toml = r#"
[cli]

[[cli.tools]]
name = "gh"
allow = ["pr *", "issue *"]
deny = ["auth *", "api"]
env = ["GH_TOKEN"]

[[cli.tools]]
name = "gt"
allow = ["*"]
env = ["GH_TOKEN"]
"#;
        let config: Config = toml::from_str(toml).unwrap();
        let cli = config.cli.unwrap();
        assert_eq!(cli.listen, "0.0.0.0:3130");
        assert_eq!(cli.tools.len(), 2);
        assert_eq!(cli.tools[0].name, "gh");
        assert_eq!(cli.tools[0].allow, vec!["pr *", "issue *"]);
        assert_eq!(cli.tools[0].deny, vec!["auth *", "api"]);
        assert_eq!(cli.tools[0].env, vec!["GH_TOKEN"]);
        assert_eq!(cli.tools[1].name, "gt");
    }

    #[test]
    fn merge_global_allow_domains() {
        let mut project: Config = toml::from_str(
            r#"
[proxy.network]
allow = ["project.com"]
"#,
        )
        .unwrap();
        let global: Config = toml::from_str(
            r#"
[proxy.network]
allow = ["global.com", "*.corp.com"]
"#,
        )
        .unwrap();
        project.merge_global(global);
        assert_eq!(
            project.proxy.network.allow,
            vec!["global.com", "*.corp.com", "project.com"]
        );
    }

    #[test]
    fn merge_global_deny_domains() {
        let mut project: Config = toml::from_str(
            r#"
[proxy.network]
deny = ["bad-project.com"]
"#,
        )
        .unwrap();
        let global: Config = toml::from_str(
            r#"
[proxy.network]
deny = ["bad-global.com"]
"#,
        )
        .unwrap();
        project.merge_global(global);
        assert_eq!(
            project.proxy.network.deny,
            vec!["bad-global.com", "bad-project.com"]
        );
    }

    #[test]
    fn merge_global_cli_tools_additive() {
        let mut project: Config = toml::from_str(
            r#"
[cli]
[[cli.tools]]
name = "gh"
allow = ["pr *"]
"#,
        )
        .unwrap();
        let global: Config = toml::from_str(
            r#"
[cli]
[[cli.tools]]
name = "aws"
allow = ["s3 *"]
"#,
        )
        .unwrap();
        project.merge_global(global);
        let tools = &project.cli.unwrap().tools;
        assert_eq!(tools.len(), 2);
        assert_eq!(tools[0].name, "gh");
        assert_eq!(tools[1].name, "aws");
    }

    #[test]
    fn merge_global_cli_tools_dedup() {
        let mut project: Config = toml::from_str(
            r#"
[cli]
[[cli.tools]]
name = "gh"
allow = ["pr *"]
"#,
        )
        .unwrap();
        let global: Config = toml::from_str(
            r#"
[cli]
[[cli.tools]]
name = "gh"
allow = ["issue *"]
"#,
        )
        .unwrap();
        project.merge_global(global);
        let tools = &project.cli.unwrap().tools;
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "gh");
        assert_eq!(tools[0].allow, vec!["pr *"]); // project wins
    }

    #[test]
    fn merge_global_mcp_servers_dedup() {
        let mut project: Config = toml::from_str(
            r#"
[mcp]
[[mcp.servers]]
name = "github"
allow = ["get_*"]
"#,
        )
        .unwrap();
        let global: Config = toml::from_str(
            r#"
[mcp]
[[mcp.servers]]
name = "github"
allow = ["*"]

[[mcp.servers]]
name = "linear"
allow = ["*"]
"#,
        )
        .unwrap();
        project.merge_global(global);
        let servers = &project.mcp.unwrap().servers;
        assert_eq!(servers.len(), 2);
        assert_eq!(servers[0].name, "github");
        assert_eq!(servers[0].allow, vec!["get_*"]); // project wins
        assert_eq!(servers[1].name, "linear"); // global-only, added
    }

    #[test]
    fn merge_global_no_cli_in_project() {
        let mut project: Config = toml::from_str("").unwrap();
        assert!(project.cli.is_none());
        let global: Config = toml::from_str(
            r#"
[cli]
[[cli.tools]]
name = "aws"
allow = ["s3 *"]
"#,
        )
        .unwrap();
        project.merge_global(global);
        let tools = &project.cli.unwrap().tools;
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "aws");
    }

    #[test]
    fn merge_global_empty_is_noop() {
        let mut project: Config = toml::from_str(
            r#"
[proxy.network]
allow = ["project.com"]
"#,
        )
        .unwrap();
        let global: Config = toml::from_str("").unwrap();
        project.merge_global(global);
        assert_eq!(project.proxy.network.allow, vec!["project.com"]);
        assert!(project.cli.is_none());
    }

    #[test]
    fn parse_cli_tool_mode_direct() {
        let toml = r#"
[cli]

[[cli.tools]]
name = "gh"
mode = "direct"
env = ["GH_TOKEN"]
"#;
        let config: Config = toml::from_str(toml).unwrap();
        let cli = config.cli.unwrap();
        assert_eq!(cli.tools[0].mode, CliToolMode::Direct);
    }

    #[test]
    fn parse_cli_tool_mode_proxy_explicit() {
        let toml = r#"
[cli]

[[cli.tools]]
name = "gh"
mode = "proxy"
"#;
        let config: Config = toml::from_str(toml).unwrap();
        let cli = config.cli.unwrap();
        assert_eq!(cli.tools[0].mode, CliToolMode::Proxy);
    }

    #[test]
    fn parse_cli_tool_mode_default_is_proxy() {
        let toml = r#"
[cli]

[[cli.tools]]
name = "gh"
"#;
        let config: Config = toml::from_str(toml).unwrap();
        let cli = config.cli.unwrap();
        assert_eq!(cli.tools[0].mode, CliToolMode::Proxy);
    }
}
