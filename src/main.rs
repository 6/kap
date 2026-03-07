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
#[command(name = "kap", version, about = "Run AI agents in secure capsules")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    // -- Lifecycle --
    /// Scaffold devcontainer files into a project
    #[command(display_order = 1)]
    Init {
        /// Project directory
        #[arg(short, long, default_value = ".")]
        project_dir: String,

        /// Skip confirmation prompts
        #[arg(short, long)]
        yes: bool,
    },
    /// Start the devcontainer
    #[command(display_order = 2)]
    Up {
        /// Remove and recreate the container from scratch
        #[arg(long)]
        reset: bool,
    },
    /// Stop and remove the devcontainer
    #[command(display_order = 3)]
    Down {
        /// Project name (from `kap list`). Default: current directory.
        project: Option<String>,

        /// Also remove named volumes
        #[arg(short, long)]
        volumes: bool,
    },
    /// Run a command in the devcontainer (default: interactive shell)
    #[command(display_order = 4)]
    Exec {
        /// Project name (from `kap list`). Omit to use current directory.
        #[arg(short, long)]
        project: Option<String>,

        /// Command and arguments to run
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        cmd: Vec<String>,
    },
    /// List running devcontainers
    #[command(display_order = 5)]
    List {
        /// Show CPU and memory usage
        #[arg(short, long)]
        stats: bool,
    },

    // -- Diagnostics --
    /// Check if kap is working (runs checks via docker exec)
    #[command(display_order = 10)]
    Status,
    /// Show denied requests from the proxy log
    #[command(display_order = 11)]
    WhyDenied {
        /// Stream new denials as they happen
        #[arg(long)]
        tail: bool,

        /// Path to the proxy log
        #[arg(long, default_value = "/var/log/kap/proxy.jsonl")]
        log: String,
    },

    // -- Subsystems --
    /// Manage MCP server registrations
    #[command(display_order = 20)]
    Mcp {
        #[command(subcommand)]
        command: McpCommand,
    },
    /// Remote access for monitoring and steering from your phone
    #[command(display_order = 21)]
    Remote {
        #[command(subcommand)]
        command: RemoteCommand,
    },

    // -- Hidden (sidecar internals) --
    /// Check proxy health (runs inside the sidecar)
    #[command(hide = true)]
    SidecarCheck {
        /// Only check proxy health (for container healthcheck)
        #[arg(long)]
        proxy: bool,

        /// Check MCP servers (initialize + tools/list). Output: JSON lines.
        #[arg(long)]
        mcp: bool,

        /// Path to config file (for --mcp)
        #[arg(short, long, default_value = "/etc/kap/config.toml")]
        config: String,
    },
    /// Forward a CLI command to the kap sidecar proxy (used by shim scripts)
    #[command(hide = true)]
    SidecarCliShim {
        /// Tool name (e.g. "gh", "gt")
        tool: String,

        /// Arguments to pass to the tool
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Regenerate overlay, .env, and shims (runs as initializeCommand)
    #[command(hide = true)]
    SidecarInit {
        /// Project directory containing .devcontainer/
        #[arg(short, long, default_value = ".")]
        project_dir: String,
    },
    /// Start the forward proxy (runs inside the sidecar)
    #[command(hide = true)]
    SidecarProxy {
        /// Run in observe mode: allow all traffic, log every domain
        #[arg(long)]
        observe: bool,

        /// Path to config file
        #[arg(short, long, default_value = "/etc/kap/config.toml")]
        config: String,
    },
}

#[derive(Subcommand)]
enum RemoteCommand {
    /// Start the remote access daemon (idempotent — shows QR if already running)
    Start {
        /// Address to listen on
        #[arg(long, default_value = "0.0.0.0:19420")]
        listen: String,
    },
    /// Stop the remote access daemon
    Stop,
    /// Show daemon status and paired devices
    Status,
    /// Revoke a paired device
    Revoke {
        /// Device ID to revoke
        device_id: String,
    },
}

#[derive(Subcommand)]
enum McpCommand {
    /// Register an MCP server (OAuth 2.1 or static headers)
    #[command(override_usage = "kap mcp add <NAME> <UPSTREAM> [--header KEY=VALUE ...]")]
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
    /// Show details for a registered MCP server
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
        Command::SidecarCheck { proxy, mcp, config } => {
            if mcp {
                check::run_mcp(&config).await
            } else {
                check::run(proxy).await
            }
        }
        Command::SidecarCliShim { tool, args } => cli::shim::run(&tool, &args).await,
        Command::Down { project, volumes } => container::down(project, volumes),
        Command::Exec { project, cmd } => container::exec(project, cmd),
        Command::Init { project_dir, yes } => init::run(&project_dir, yes),
        Command::SidecarInit { project_dir } => init_env::run(&project_dir),
        Command::List { stats } => container::list(stats),
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
        Command::SidecarProxy { observe, config } => {
            // Retry config loading — Docker Desktop macOS bind mounts can be
            // temporarily unavailable when the container first starts.
            let cfg = {
                let mut last_err = None;
                let mut loaded = None;
                for attempt in 0..5 {
                    match config::Config::load(&config) {
                        Ok(c) if !c.allow_domains().is_empty() || attempt == 4 => {
                            loaded = Some(c);
                            break;
                        }
                        Ok(_) => {
                            eprintln!(
                                "[sidecar] config has no allowed domains, retrying ({}/5)...",
                                attempt + 1
                            );
                            std::thread::sleep(std::time::Duration::from_secs(1));
                        }
                        Err(e) => {
                            eprintln!(
                                "[sidecar] config load failed: {e}, retrying ({}/5)...",
                                attempt + 1
                            );
                            last_err = Some(e);
                            std::thread::sleep(std::time::Duration::from_secs(1));
                        }
                    }
                }
                match loaded {
                    Some(c) => c,
                    None => {
                        return Err(
                            last_err.unwrap_or_else(|| anyhow::anyhow!("config load failed"))
                        );
                    }
                }
            };

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
        Command::Remote { command } => {
            let data_dir = remote::auth::data_dir();
            match command {
                RemoteCommand::Start { listen } => remote::start(&listen, data_dir).await,
                RemoteCommand::Stop => remote::stop(),
                RemoteCommand::Status => {
                    remote::remote_status(&data_dir);
                    Ok(())
                }
                RemoteCommand::Revoke { device_id } => remote::revoke(&data_dir, &device_id),
            }
        }
        Command::Status => status::run(),
        Command::Up { reset } => container::up(reset),
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
                    .find(|n| n.contains("kap-kap") || n.ends_with("-kap-1"))
                    .ok_or_else(|| {
                        anyhow::anyhow!(
                            "no running kap sidecar found.\n\n  \
                         Start it with: kap up"
                        )
                    })?;
                cmd.arg(sidecar);
                cmd.args(["kap", "why-denied"]);
                if tail {
                    cmd.arg("--tail");
                }
                let status = cmd.status()?;
                std::process::exit(status.code().unwrap_or(1));
            }
        }
    }
}
