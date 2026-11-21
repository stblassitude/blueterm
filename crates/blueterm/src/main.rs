//! blueterm: connect to a blueterm-agent console over Bluetooth Low Energy.

mod escape;
mod session;
mod term;
mod transport;

use std::{process::ExitCode, time::Duration};

use anyhow::{Context, Result, bail};
use blueterm_proto::{ControlMessage, DEFAULT_SIZE, DEFAULT_TERM, is_valid_term};
use clap::{Args, Parser, Subcommand};
use tokio::{
    signal::unix::{SignalKind, signal},
    sync::mpsc,
};
use tracing_subscriber::EnvFilter;

use crate::{
    session::Outcome,
    transport::{Backend, Options},
};

/// Log in to a machine running blueterm-agent, over Bluetooth Low Energy.
///
/// `blueterm <NAME>` connects to the agent advertising that name.
#[derive(Parser)]
#[command(version, about, args_conflicts_with_subcommands = true)]
struct Cli {
    #[command(flatten)]
    common: Common,
    #[command(subcommand)]
    cmd: Option<Cmd>,
    #[command(flatten)]
    connect: ConnectArgs,
}

#[derive(Args, Clone)]
struct Common {
    /// Bluetooth adapter to use (Linux only), e.g. hci0
    #[arg(long, global = true)]
    adapter: Option<String>,
    /// More logging on stderr (-v: debug, -vv: trace). Note: this garbles the
    /// terminal session.
    #[arg(short, long, action = clap::ArgAction::Count, global = true)]
    verbose: u8,
}

#[derive(Subcommand)]
enum Cmd {
    /// List nearby agents.
    Scan {
        /// How long to scan, in seconds.
        #[arg(long, short, default_value_t = 5)]
        timeout: u64,
    },
    /// Connect to an agent (default command).
    Connect(ConnectArgs),
    /// Pair with an agent (Linux; on macOS pairing happens on first connect).
    Pair {
        /// Agent name or id
        target: String,
        /// How long to scan for the agent, in seconds.
        #[arg(long, default_value_t = 10)]
        scan_timeout: u64,
    },
}

#[derive(Args, Clone)]
struct ConnectArgs {
    /// Agent name (as shown by `blueterm scan`) or id
    target: Option<String>,
    /// Terminal type to request [default: $TERM]
    #[arg(long)]
    term: Option<String>,
    /// Escape character, or "none"
    #[arg(long, short, default_value = "~")]
    escape: String,
    /// How long to scan for the agent, in seconds.
    #[arg(long, default_value_t = 10)]
    scan_timeout: u64,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let level = match cli.common.verbose {
        0 => "warn",
        1 => "blueterm=debug,info",
        _ => "trace",
    };
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_env("BLUETERM_LOG").unwrap_or_else(|_| EnvFilter::new(level)))
        .with_writer(std::io::stderr)
        .init();

    let rt = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("blueterm: {e}");
            return ExitCode::from(255);
        }
    };
    let code = match rt.block_on(run(cli)) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("blueterm: {e:#}");
            255
        }
    };
    // Don't wait for the blocking stdin reader thread.
    std::process::exit(code);
}

async fn run(cli: Cli) -> Result<i32> {
    let opts = Options { adapter: cli.common.adapter.clone() };
    match cli.cmd {
        Some(Cmd::Scan { timeout }) => scan(&opts, Duration::from_secs(timeout)).await,
        Some(Cmd::Connect(args)) => connect(&opts, args).await,
        Some(Cmd::Pair { target, scan_timeout }) => pair(&opts, &target, Duration::from_secs(scan_timeout)).await,
        None => connect(&opts, cli.connect).await,
    }
}

async fn scan(opts: &Options, timeout: Duration) -> Result<i32> {
    let backend = Backend::new(opts).await?;
    eprintln!("Scanning for {}s...", timeout.as_secs());
    let mut found = backend.discover(timeout, |_| false).await?;
    found.sort_by_key(|f| std::cmp::Reverse(f.rssi));
    if found.is_empty() {
        eprintln!("No agents found.");
        return Ok(1);
    }
    println!("{:<24} {:>5}  ID", "NAME", "RSSI");
    for f in found {
        let rssi = f.rssi.map(|r| r.to_string()).unwrap_or_default();
        println!("{:<24} {:>5}  {}", f.display_name(), rssi, f.id);
    }
    Ok(0)
}

#[cfg(target_os = "linux")]
async fn pair(opts: &Options, target: &str, scan_timeout: Duration) -> Result<i32> {
    let backend = Backend::new(opts).await?;
    eprintln!("Looking for {target}...");
    let found = transport::resolve(&backend, target, scan_timeout).await?;
    backend.pair(&found).await?;
    Ok(0)
}

#[cfg(not(target_os = "linux"))]
async fn pair(_opts: &Options, _target: &str, _scan_timeout: Duration) -> Result<i32> {
    bail!(
        "on macOS, pairing happens when you first connect: run `sudo blueterm-agent pair` on the agent, \
         then `blueterm <name>` here and accept the system's pairing request"
    )
}

fn parse_escape(s: &str) -> Result<Option<u8>> {
    match s {
        "none" => Ok(None),
        s if s.len() == 1 && s.is_ascii() => Ok(Some(s.as_bytes()[0])),
        s if s.len() == 2 && s.starts_with('^') && s.is_ascii() => {
            Ok(Some(s.as_bytes()[1].to_ascii_uppercase() & 0x1f))
        }
        _ => bail!("escape character must be a single character, ^X, or \"none\""),
    }
}

async fn connect(opts: &Options, args: ConnectArgs) -> Result<i32> {
    let Some(target) = args.target else {
        bail!("no agent given; usage: blueterm <NAME>  (see `blueterm scan` and `blueterm --help`)");
    };
    let escape = parse_escape(&args.escape)?;
    let term = args.term.or_else(|| std::env::var("TERM").ok()).filter(|t| is_valid_term(t));
    let term = term.as_deref().unwrap_or(DEFAULT_TERM);
    let (rows, cols) = term::window_size().unwrap_or(DEFAULT_SIZE);

    let backend = Backend::new(opts).await?;
    eprintln!("Looking for {target}...");
    let found = transport::resolve(&backend, &target, Duration::from_secs(args.scan_timeout)).await?;
    let name = found.display_name().to_owned();
    eprintln!("Connecting to {name} ({})...", found.id);
    let mut link = backend.connect(&found, &ControlMessage::hello(rows, cols, term)).await?;
    match escape {
        Some(e) => eprintln!(
            "Connected to {name}. Escape character is '{}'; type {}? for help.",
            escape_name(e),
            escape_name(e)
        ),
        None => eprintln!("Connected to {name}."),
    }
    if !link.has_control {
        eprintln!("(Plain NUS device: no window size or exit status. Press Enter for a prompt.)");
    }

    let (resize_tx, resize_rx) = mpsc::channel(4);
    let mut winch = signal(SignalKind::window_change()).context("installing SIGWINCH handler")?;
    tokio::spawn(async move {
        while winch.recv().await.is_some() {
            if let Some(size) = term::window_size()
                && resize_tx.send(size).await.is_err()
            {
                break;
            }
        }
    });

    let raw =
        if term::stdin_is_tty() { Some(term::RawMode::enable().context("setting terminal raw mode")?) } else { None };
    let io = session::Io { input: tokio::io::stdin(), output: tokio::io::stdout(), resize: resize_rx };
    let outcome = session::run(&link.tx, &mut link.rx, io, escape).await;
    drop(raw);

    let transport::Link { tx, rx, task, .. } = link;
    drop((tx, rx));
    let _ = tokio::time::timeout(Duration::from_secs(3), task).await;

    Ok(match outcome? {
        Outcome::Ended(status) => {
            eprintln!("\nConnection to {name} closed.");
            status.clamp(0, 255)
        }
        Outcome::UserDisconnect => {
            eprintln!("\nDisconnected from {name}.");
            0
        }
        Outcome::Lost => {
            eprintln!("\nConnection to {name} lost.");
            255
        }
    })
}

fn escape_name(e: u8) -> String {
    if e < 0x20 { format!("^{}", (e | 0x40) as char) } else { (e as char).to_string() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escape_parsing() {
        assert_eq!(parse_escape("~").unwrap(), Some(b'~'));
        assert_eq!(parse_escape("^]").unwrap(), Some(0x1d));
        assert_eq!(parse_escape("^a").unwrap(), Some(0x01));
        assert_eq!(parse_escape("none").unwrap(), None);
        assert!(parse_escape("ab").is_err());
        assert_eq!(escape_name(0x1d), "^]");
        assert_eq!(escape_name(b'~'), "~");
    }

    #[test]
    fn cli_parses() {
        use clap::CommandFactory;
        Cli::command().debug_assert();
        let cli = Cli::try_parse_from(["blueterm", "myhost"]).unwrap();
        assert_eq!(cli.connect.target.as_deref(), Some("myhost"));
        let cli = Cli::try_parse_from(["blueterm", "scan", "-t", "2"]).unwrap();
        assert!(matches!(cli.cmd, Some(Cmd::Scan { timeout: 2 })));
    }
}
