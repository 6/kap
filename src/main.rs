mod check;
mod config;
mod cred_server;
mod credential;
mod init;
mod proxy;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "devp",
    version,
    about = "Egress proxy + credential isolation for devcontainers"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Start the forward proxy with domain allowlist and DNS forwarder
    Proxy {
        /// Run in observe mode: allow all traffic, log every domain
        #[arg(long)]
        observe: bool,

        /// Path to config file
        #[arg(short, long, default_value = "/etc/devp/config.toml")]
        config: String,
    },
    /// Start the credential server on the host (Unix socket)
    CredServer {
        /// Daemonize the server
        #[arg(long)]
        daemonize: bool,

        /// Path to config file
        #[arg(short, long, default_value = "devp.toml")]
        config: String,
    },
    /// Git credential helper (container-side)
    Credential {
        /// Credential operation: get, store, or erase
        operation: Option<String>,

        /// Install the credential helper into git config
        #[arg(long)]
        install: bool,

        /// Path to credential socket
        #[arg(long, default_value = "/devp-sockets/cred.sock")]
        socket: String,
    },
    /// Scaffold devcontainer files into a project
    Init {
        /// Project directory
        #[arg(short, long, default_value = ".")]
        project_dir: String,
    },
    /// Verify setup: proxy reachable, cred-server running, DNS working
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
        #[arg(long, default_value = "/var/log/devp/proxy.jsonl")]
        log: String,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Command::Proxy { observe, config } => {
            let cfg = config::Config::load(&config)?;
            proxy::run(cfg, observe).await
        }
        Command::CredServer { daemonize, config } => {
            let cfg = config::Config::load(&config)?;
            cred_server::run(cfg, daemonize).await
        }
        Command::Credential {
            operation,
            install,
            socket,
        } => {
            if install {
                credential::install()
            } else {
                let op = operation.unwrap_or_default();
                credential::run(&op, &socket).await
            }
        }
        Command::Init { project_dir } => init::run(&project_dir),
        Command::Check { proxy } => check::run(proxy).await,
        Command::WhyDenied { tail, log } => proxy::log::why_denied(&log, tail).await,
    }
}
