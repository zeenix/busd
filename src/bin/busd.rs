extern crate busd;

use busd::{
    bus,
    config::{self, BusConfig},
};

use anyhow::Result;
use clap::{Args, Parser, ValueEnum};
#[cfg(unix)]
use tokio::{select, signal::unix::SignalKind};
use tracing::error;
#[cfg(unix)]
use tracing::{info, warn};

/// A simple D-Bus broker.
#[derive(Parser, Debug)]
#[clap(author, version, about, long_about = None)]
struct BusdArgs {
    #[command(flatten)]
    config: ConfigArg,

    /// The address to listen on.
    #[clap(short = 'a', long, value_parser)]
    address: Option<String>,

    /// The file descriptor to print the bus address to.
    #[clap(long, value_parser)]
    print_address: Option<Option<i32>>,

    /// The authentication mechanism to use.
    #[clap(long)]
    #[arg(value_enum, default_value_t = AuthMechanism::External)]
    auth_mechanism: AuthMechanism,
}

#[derive(Args, Debug)]
#[group(required = false, multiple = false)]
struct ConfigArg {
    /// The configuration file.
    #[clap(long, value_parser)]
    config_file: Option<String>,

    /// Use the standard configuration file for the per-login-session message bus.
    #[clap(long, value_parser)]
    session: bool,

    /// Use the standard configuration file for the systemwide message bus.
    #[clap(long, value_parser)]
    system: bool,
}

#[derive(Copy, Clone, Debug, ValueEnum)]
enum AuthMechanism {
    /// This is the recommended authentication mechanism on platforms where credentials can be
    /// transferred out-of-band, in particular Unix platforms that can perform credentials-passing
    /// over UNIX domain sockets.
    External,
    /// This mechanism is designed to establish that a client has the ability to read a private
    /// file owned by the user being authenticated.
    Cookie,
    /// Does not perform any authentication at all (not recommended).
    Anonymous,
}

impl From<AuthMechanism> for zbus::AuthMechanism {
    fn from(auth_mechanism: AuthMechanism) -> Self {
        match auth_mechanism {
            AuthMechanism::External => zbus::AuthMechanism::External,
            AuthMechanism::Cookie => zbus::AuthMechanism::Cookie,
            AuthMechanism::Anonymous => zbus::AuthMechanism::Anonymous,
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    busd::tracing_subscriber::init();

    let mut args = BusdArgs::parse();

    // let's make --session the default
    if !args.config.session && !args.config.system && args.config.config_file.is_none() {
        args.config.session = true;
    }
    // FIXME: make default config configurable or OS dependant
    if args.config.session {
        args.config.config_file = Some("/usr/share/dbus-1/session.conf".into());
    }
    if args.config.system {
        args.config.config_file = Some("/usr/share/dbus-1/system.conf".into());
    }

    let mut config = BusConfig::default();
    if let Some(file) = args.config.config_file {
        config = BusConfig::read(file)?;
    }

    if let Some(address) = args.address {
        config.elements.push(config::Element::Listen(address));
    }

    // FIXME: we don't support multiple <listen> atm
    let address = config
        .elements
        .iter()
        .rev()
        .find_map(|e| match e {
            config::Element::Listen(l) => Some(l),
            _ => None,
        })
        .unwrap();

    let mut bus = bus::Bus::for_address(address, args.auth_mechanism.into()).await?;

    if let Some(fd) = args.print_address {
        use std::io::Write;
        use std::os::fd::FromRawFd;

        let mut file = unsafe { std::fs::File::from_raw_fd(fd.unwrap_or(1)) };
        // make a single string, busd test code doesn't handle multiple IO!
        let addr = format!("{},guid={}\n", bus.address().to_string(), bus.guid());
        write!(file, "{}", addr)?;
        file.flush()?;
    }

    // FIXME: How to handle this gracefully on Windows?
    #[cfg(unix)]
    {
        let mut sig_int = tokio::signal::unix::signal(SignalKind::interrupt())?;

        select! {
            _ = sig_int.recv() => {
                info!("Received SIGINT, shutting down..");
            }
            res = bus.run() => match res {
                Ok(()) => warn!("Bus stopped, shutting down.."),
                Err(e) => error!("Bus stopped with an error: {}", e),
            }
        }
    }
    #[cfg(not(unix))]
    bus.run().await?;

    if let Err(e) = bus.cleanup().await {
        error!("Failed to clean up: {}", e);
    }

    Ok(())
}
