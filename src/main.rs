mod network;
mod protocol;

use clap::{Parser, Subcommand};
use network::{client::send, server::serve, IPVersion};
use std::path::PathBuf;

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
        /// Name of this server for advertisement.
        name: Option<String>,
    },
    Send {
        path: PathBuf,
    },
}

#[tokio::main]
async fn main() -> eyre::Result<()> {
    tracing_subscriber::fmt::init();

    let args = Args::parse();

    let ip_version = match (args.use_ipv4, args.use_ipv6) {
        (true, true) => unreachable!("clap should enforce ip version choice is mutually exclusive"),
        (true, false) => IPVersion::IPV4,
        (false, true) => IPVersion::IPV6,
        (false, false) => IPVersion::IPV6, // default if neither is set explicitly
    };

    match args.cmd {
        Command::Serve { name } => {
            let name = if let Some(name) = name {
                name
            } else {
                let hostname = hostname::get()?;
                hostname.to_string_lossy().into_owned()
            };
            serve(&name, ip_version).await
        }
        Command::Send { path } => send(ip_version, &path).await,
    }
}
