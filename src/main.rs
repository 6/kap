mod check;
mod config;
mod init;
mod proxy;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "devp", version, about = "Egress proxy for devcontainers")]
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
        #[arg(short, long, default_value = "/etc/devp/config.toml")]
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
        Command::Init { project_dir } => init::run(&project_dir),
        Command::Check { proxy } => check::run(proxy).await,
        Command::WhyDenied { tail, log } => proxy::log::why_denied(&log, tail).await,
    }
}
