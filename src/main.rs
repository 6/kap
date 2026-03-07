mod check;
mod config;
mod doctor;
mod init;
mod init_env;
mod mcp;
mod proxy;

use std::sync::Arc;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "devg", version, about = "Network and MCP access control for devcontainers")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Start the forward proxy with domain allowlist
    Proxy {
        /// Run in observe mode: allow all traffic, log every domain
        #[arg(long)]
        observe: bool,

        /// Path to config file
        #[arg(short, long, default_value = "/etc/devg/config.toml")]
        config: String,
    },
    /// Scaffold devcontainer files into a project
    Init {
        /// Project directory
        #[arg(short, long, default_value = ".")]
        project_dir: String,
    },
    /// Check proxy health (for container healthcheck)
    Check {
        /// Only check proxy health (for container healthcheck)
        #[arg(long)]
        proxy: bool,
    },
    /// Show denied requests from the proxy log
    WhyDenied {
        /// Stream new denials as they happen
        #[arg(long)]
        tail: bool,

        /// Path to the proxy log
        #[arg(long, default_value = "/var/log/devg/proxy.jsonl")]
        log: String,
    },
    /// Generate .devcontainer/.env with host credentials (for initializeCommand)
    InitEnv {
        /// Project directory containing .devcontainer/
        #[arg(short, long, default_value = ".")]
        project_dir: String,
    },
    /// Verify devcontainer-guard is configured correctly (run from app container)
    Doctor,
    /// Authenticate with a remote MCP server (OAuth 2.1)
    Auth {
        /// Name for this MCP server (used in config and token storage)
        name: String,

        /// Upstream MCP server URL
        #[arg(long)]
        upstream: String,

        /// Directory to store auth tokens
        #[arg(long)]
        auth_dir: Option<String>,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Command::Proxy { observe, config } => {
            let cfg = config::Config::load(&config)?;

            let allowlist = Arc::new(proxy::allowlist::Allowlist::new(
                cfg.allow_domains(),
                &cfg.proxy.network.deny,
            ));

            let proxy_fut = proxy::run(cfg.clone(), observe, allowlist.clone());
            let dns_fut = proxy::dns::run(
                &cfg.proxy.dns_listen,
                &cfg.proxy.dns_upstream,
                allowlist,
            );

            if let Some(ref mcp_cfg) = cfg.mcp {
                let logger = proxy::log::ProxyLogger::new(&cfg.proxy.observe.log);
                let mcp_fut = mcp::run(mcp_cfg, logger);
                tokio::try_join!(proxy_fut, dns_fut, mcp_fut)?;
            } else {
                tokio::try_join!(proxy_fut, dns_fut)?;
            }
            Ok(())
        }
        Command::Init { project_dir } => init::run(&project_dir),
        Command::InitEnv { project_dir } => init_env::run(&project_dir),
        Command::Doctor => doctor::run(),
        Command::Check { proxy } => check::run(proxy).await,
        Command::WhyDenied { tail, log } => proxy::log::why_denied(&log, tail).await,
        Command::Auth {
            name,
            upstream,
            auth_dir,
        } => {
            let dir = auth_dir.unwrap_or_else(mcp::auth::host_auth_dir);
            mcp::auth::run(&name, &upstream, &dir).await
        }
    }
}
