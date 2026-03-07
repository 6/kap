mod check;
mod cli;
mod config;
mod container;
mod init;
mod init_env;
mod mcp;
mod mcp_cmd;
mod proxy;
mod remote;
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

        /// Skip confirmation prompts
        #[arg(short, long)]
        yes: bool,
    },
    /// Start the devcontainer
    Up {
        /// Remove and recreate the container from scratch
        #[arg(long)]
        reset: bool,
    },
    /// Stop and remove the devcontainer
    Down {
        /// Also remove named volumes
        #[arg(short, long)]
        volumes: bool,
    },
    /// Run a command in the devcontainer (default: interactive shell)
    Exec {
        /// Command and arguments to run
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        cmd: Vec<String>,
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
    /// Forward a CLI command to the devg sidecar proxy (used by shim scripts)
    #[command(hide = true)]
    CliShim {
        /// Tool name (e.g. "gh", "gt")
        tool: String,

        /// Arguments to pass to the tool
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
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
    /// Remote access for monitoring and steering from iPhone
    Remote {
        #[command(subcommand)]
        command: RemoteCommand,
    },
}

#[derive(Subcommand)]
enum RemoteCommand {
    /// Start the remote access daemon
    Start {
        /// Address to listen on
        #[arg(long, default_value = "0.0.0.0:19420")]
        listen: String,
    },
    /// Show QR code for iPhone pairing
    Pair {
        /// Port the remote daemon is listening on
        #[arg(long, default_value_t = 19420)]
        port: u16,
    },
    /// List paired devices
    Devices,
    /// Revoke a paired device
    Revoke {
        /// Device ID to revoke
        device_id: String,
    },
}

#[derive(Subcommand)]
enum McpCommand {
    /// Register an MCP server (OAuth 2.1 or static headers)
    #[command(override_usage = "devg mcp add <NAME> <UPSTREAM> [--header KEY=VALUE ...]")]
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
            let dns_listen = cfg.proxy.dns_listen.clone();
            let dns_upstream = cfg.proxy.dns_upstream.clone();

            let mut set = tokio::task::JoinSet::new();
            set.spawn(proxy_fut);
            set.spawn(async move { proxy::dns::run(&dns_listen, &dns_upstream, allowlist).await });

            if let Some(ref mcp_cfg) = cfg.mcp {
                let logger = proxy::log::ProxyLogger::new(&cfg.proxy.observe.log);
                let mcp_cfg = mcp_cfg.clone();
                set.spawn(async move { mcp::run(&mcp_cfg, logger).await });
            }

            if let Some(ref cli_cfg) = cfg.cli {
                let logger = proxy::log::ProxyLogger::new(&cfg.proxy.observe.log);
                let cli_cfg = cli_cfg.clone();
                set.spawn(async move { cli::run(&cli_cfg, logger).await });
            }

            while let Some(result) = set.join_next().await {
                result??;
            }
            Ok(())
        }
        Command::Init { project_dir, yes } => init::run(&project_dir, yes),
        Command::Up { reset } => container::up(reset),
        Command::Down { volumes } => container::down(volumes),
        Command::Exec { cmd } => container::exec(cmd),
        Command::CliShim { tool, args } => cli::shim::run(&tool, &args).await,
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
                         Start it with: devg up"
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
        Command::Remote { command } => {
            let data_dir = remote::auth::data_dir();
            match command {
                RemoteCommand::Start { listen } => remote::run(&listen, data_dir).await,
                RemoteCommand::Pair { port } => remote::print_pair(&data_dir, port),
                RemoteCommand::Devices => {
                    remote::list_devices(&data_dir);
                    Ok(())
                }
                RemoteCommand::Revoke { device_id } => remote::revoke(&data_dir, &device_id),
            }
        }
    }
}
