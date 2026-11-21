use std::{path::PathBuf, process::ExitCode, time::Duration};

use anyhow::Result;
use clap::{Parser, Subcommand};
use tracing_subscriber::{EnvFilter, prelude::*};

use crate::{config::Config, pair, server};

/// Serve a login console over Bluetooth Low Energy (Nordic UART Service).
#[derive(Parser)]
#[command(version, about)]
struct Cli {
    /// Configuration file [default: /etc/blueterm/agent.toml]
    #[arg(long, short, global = true)]
    config: Option<PathBuf>,
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the agent (default; this is what the systemd unit runs).
    Run,
    /// Allow a new device to pair, showing the passkey on this terminal.
    Pair {
        /// How long to stay pairable, in seconds.
        #[arg(long, short, default_value_t = 120)]
        timeout: u64,
    },
    /// List paired devices.
    Devices,
    /// Remove a paired device.
    Unpair {
        /// Bluetooth address, as shown by `blueterm-agent devices`.
        address: bluer::Address,
    },
}

fn init_logging(daemon: bool) {
    let filter = EnvFilter::try_from_env("BLUETERM_LOG")
        .unwrap_or_else(|_| EnvFilter::new(if daemon { "info" } else { "warn" }));
    let journald =
        if daemon && std::env::var_os("JOURNAL_STREAM").is_some() { tracing_journald::layer().ok() } else { None };
    let registry = tracing_subscriber::registry().with(filter);
    match journald {
        Some(j) => registry.with(j).init(),
        None => registry.with(tracing_subscriber::fmt::layer().with_writer(std::io::stderr)).init(),
    }
}

pub fn main() -> ExitCode {
    let cli = Cli::parse();
    let cmd = cli.cmd.unwrap_or(Cmd::Run);
    init_logging(matches!(cmd, Cmd::Run));
    match run(cli.config, cmd) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            // stderr ends up in the journal when run by systemd.
            eprintln!("blueterm-agent: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn run(config: Option<PathBuf>, cmd: Cmd) -> Result<()> {
    let cfg = Config::load(config.as_deref())?;
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    rt.block_on(async move {
        let adapter = cfg.adapter.clone();
        match cmd {
            Cmd::Run => server::run(cfg).await,
            Cmd::Pair { timeout } => pair::pair(adapter.as_deref(), Duration::from_secs(timeout)).await,
            Cmd::Devices => pair::devices(adapter.as_deref()).await,
            Cmd::Unpair { address } => pair::unpair(adapter.as_deref(), address).await,
        }
    })
}
