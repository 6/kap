use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::Path;

use crate::profiles;

#[derive(Debug, Deserialize)]
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
    #[serde(default = "default_dns_listen")]
    pub dns_listen: String,
    #[serde(default = "default_dns_upstream")]
    pub dns_upstream: String,
    #[serde(default)]
    pub network: NetworkConfig,
    #[serde(default)]
    pub observe: ObserveConfig,
}

#[derive(Debug, Deserialize)]
pub struct NetworkConfig {
    #[serde(default)]
    pub profiles: Vec<String>,
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

    /// Resolve all allowed domains from profiles + explicit allow list.
    pub fn resolved_allow_domains(&self) -> Vec<String> {
        let mut domains = Vec::new();
        for profile_name in &self.proxy.network.profiles {
            if let Some(profile_domains) = profiles::get(profile_name) {
                domains.extend(profile_domains.iter().map(|s| s.to_string()));
            } else {
                eprintln!("warning: unknown profile '{profile_name}'");
            }
        }
        domains.extend(self.proxy.network.allow.clone());
        domains
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            proxy: ProxyConfig::default(),
            credentials: CredentialConfig::default(),
        }
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

impl Default for NetworkConfig {
    fn default() -> Self {
        Self {
            profiles: vec![],
            allow: vec![],
            deny: vec![],
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

fn default_dns_listen() -> String {
    "0.0.0.0:53".to_string()
}

fn default_dns_upstream() -> String {
    "8.8.8.8:53".to_string()
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

fn shellexpand_tilde(path: &str) -> String {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            return format!("{}/{rest}", home.to_string_lossy());
        }
    }
    path.to_string()
}
