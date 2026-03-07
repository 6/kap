mod check;
mod config;
mod init;
mod init_env;
mod mcp;
mod mcp_cmd;
mod proxy;
mod status;

use std::sync::Arc;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "devg",
    version,
    about = "Network and MCP access control for devcontainers"
)]
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

        /// Check MCP servers (initialize + tools/list). Output: JSON lines.
        #[arg(long)]
        mcp: bool,

        /// Path to config file (for --mcp)
        #[arg(short, long, default_value = "/etc/devg/config.toml")]
        config: String,
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
    /// Check if devcontainer-guard is working (runs checks via docker exec)
    Status,
    /// Manage MCP server registrations
    Mcp {
        #[command(subcommand)]
        command: McpCommand,
    },
    /// Authenticate with a remote MCP server (hidden alias for `mcp add`)
    #[command(hide = true)]
    Auth {
        /// Name for this MCP server
        name: String,

        /// Upstream MCP server URL
        #[arg(long)]
        upstream: String,

        /// Directory to store auth tokens
        #[arg(long)]
        auth_dir: Option<String>,
    },
}

#[derive(Subcommand)]
enum McpCommand {
    /// Register an MCP server (OAuth 2.1 or static headers)
    Add {
        /// Name for this MCP server (e.g. "linear", "github")
        name: String,

        /// Upstream MCP server URL (e.g. "https://mcp.linear.app/")
        upstream: String,

        /// Force re-authentication even if already registered
        #[arg(long)]
        reauth: bool,

        /// Static header as KEY=VALUE (skips OAuth). Can be repeated.
        #[arg(long = "header", value_name = "KEY=VALUE")]
        headers: Vec<String>,
    },
    /// List registered MCP servers
    List,
    /// Show details for a registered MCP server (including tools)
    Get {
        /// Name of the server
        name: String,
    },
    /// Remove a registered MCP server
    Remove {
        /// Name of the server to remove
        name: String,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Command::Proxy { observe, config } => {
            let cfg = config::Config::load(&config)?;

            let mcp_domains = cfg.mcp_upstream_domains();
            let mut all_allow: Vec<String> = cfg.allow_domains().to_vec();
            all_allow.extend(mcp_domains);
            let allowlist = Arc::new(proxy::allowlist::Allowlist::new(
                &all_allow,
                &cfg.proxy.network.deny,
            ));

            let proxy_fut = proxy::run(cfg.clone(), observe, allowlist.clone());
            let dns_fut =
                proxy::dns::run(&cfg.proxy.dns_listen, &cfg.proxy.dns_upstream, allowlist);

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
        Command::Status => status::run(),
        Command::Check { proxy, mcp, config } => {
            if mcp {
                check::run_mcp(&config).await
            } else {
                check::run(proxy).await
            }
        }
        Command::WhyDenied { tail, log } => {
            if std::path::Path::new(&log).exists() {
                // Running inside the container
                proxy::log::why_denied(&log, tail).await
            } else {
                // Running on the host, exec into sidecar
                let mut cmd = std::process::Command::new("docker");
                cmd.args(["exec", "-t"]);
                // Find sidecar container
                let ps = std::process::Command::new("docker")
                    .args(["ps", "--format", "{{.Names}}"])
                    .output()?;
                let names = String::from_utf8_lossy(&ps.stdout);
                let sidecar = names
                    .lines()
                    .find(|n| n.contains("devg-devg") || n.ends_with("-devg-1"))
                    .ok_or_else(|| {
                        anyhow::anyhow!(
                            "no running devg sidecar found.\n\n  \
                         Start it with: devcontainer up --workspace-folder ."
                        )
                    })?;
                cmd.arg(sidecar);
                cmd.args(["devg", "why-denied"]);
                if tail {
                    cmd.arg("--tail");
                }
                let status = cmd.status()?;
                std::process::exit(status.code().unwrap_or(1));
            }
        }
        Command::Mcp { command } => match command {
            McpCommand::Add {
                name,
                upstream,
                reauth,
                headers,
            } => mcp_cmd::add(&name, &upstream, reauth, &headers).await,
            McpCommand::List => mcp_cmd::list(),
            McpCommand::Get { name } => mcp_cmd::get(&name).await,
            McpCommand::Remove { name } => mcp_cmd::remove(&name),
        },
        Command::Auth {
            name,
            upstream,
            auth_dir,
        } => {
            let dir = auth_dir.unwrap_or_else(mcp::auth::host_auth_dir);
            let stored = mcp::auth::run(&name, &upstream).await?;
            mcp::auth::write_auth_file(&name, &stored, &dir)?;
            eprintln!("[auth] tokens saved to {dir}/{name}.json");
            Ok(())
        }
    }
}
