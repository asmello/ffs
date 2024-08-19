mod file_generator;
mod network;
mod protocol;
mod tui;

use clap::{Parser, Subcommand};
use network::{
    client::{broadcast_from_path, send_interactive},
    server::serve,
    IpVersion,
};
use std::{path::PathBuf, time::Duration};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter, Layer};

#[derive(Debug, Parser)]
#[command(version, about)]
struct Args {
    /// Use IPv4
    #[arg(short = '4', long)]
    use_ipv4: bool,
    /// Use IPv6. This is the default.
    #[arg(short = '6', long, conflicts_with = "use_ipv4")]
    use_ipv6: bool,
    #[command(subcommand)]
    cmd: Command,
}

#[derive(Debug, Clone, Subcommand)]
enum Command {
    Serve {
        /// Whether to allow overwriting existing files.
        #[arg(long)]
        overwrite: bool,
        /// Name of this server for advertisement.
        name: Option<String>,
    },
    Send {
        #[arg(short, long)]
        interactive: bool,
        /// Time waited for servers to reply after first transfer starts.
        #[arg(long, conflicts_with = "interactive", default_value_t = 1000)]
        grace_period_ms: u64,
        // TODO: this could be optional in interactive mode
        path: PathBuf,
    },
}

fn init() {
    if cfg!(feature = "tokio-console") {
        tracing_subscriber::registry()
            .with(console_subscriber::spawn())
            .with(tracing_subscriber::fmt::layer().with_filter(EnvFilter::from_default_env()))
            .init();
        tracing::info!("tokio-console enabled");
    } else {
        tracing_subscriber::fmt::init();
    }
}

// TODO: turmoil tests
#[tokio::main]
async fn main() -> eyre::Result<()> {
    init();

    let args = Args::parse();

    let ip_version = match (args.use_ipv4, args.use_ipv6) {
        (true, true) => unreachable!("clap should enforce ip version choice is mutually exclusive"),
        (true, false) => IpVersion::V4,
        (false, true) => IpVersion::V6,
        (false, false) => IpVersion::V6, // default if neither is set explicitly
    };

    match args.cmd {
        Command::Serve { name, overwrite } => {
            let name = if let Some(name) = name {
                name
            } else {
                let hostname = hostname::get()?;
                hostname.to_string_lossy().into_owned()
            };
            serve(&name, ip_version, overwrite).await?;
        }
        Command::Send {
            path,
            interactive,
            grace_period_ms,
        } => {
            if interactive {
                send_interactive(ip_version, &path).await;
            } else {
                let grace_period = Duration::from_millis(grace_period_ms);
                broadcast_from_path(ip_version, &path, grace_period).await?;
            }
        }
    }

    Ok(())
}
