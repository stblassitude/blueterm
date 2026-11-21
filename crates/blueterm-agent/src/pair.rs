//! Pairing and bond management, run interactively by an administrator.

use std::{collections::HashSet, io::Write, time::Duration};

use anyhow::{Context, Result};
use bluer::{
    Adapter, Address,
    agent::{Agent, ReqError},
};
use futures::FutureExt;
use tokio::time::{Instant, interval, sleep_until};

async fn adapter(name: Option<&str>) -> Result<(bluer::Session, Adapter)> {
    let session = bluer::Session::new().await.context("connecting to BlueZ over D-Bus (is bluetoothd running?)")?;
    let adapter = match name {
        Some(n) => session.adapter(n)?,
        None => session.default_adapter().await.context("no Bluetooth adapter found")?,
    };
    Ok((session, adapter))
}

async fn paired(adapter: &Adapter) -> Result<HashSet<Address>> {
    let mut set = HashSet::new();
    for addr in adapter.device_addresses().await? {
        if adapter.device(addr)?.is_paired().await.unwrap_or(false) {
            set.insert(addr);
        }
    }
    Ok(set)
}

/// Asks a yes/no question on the controlling terminal.
async fn confirm(prompt: String) -> bool {
    tokio::task::spawn_blocking(move || {
        print!("{prompt} [y/N] ");
        let _ = std::io::stdout().flush();
        let mut line = String::new();
        std::io::stdin().read_line(&mut line).is_ok() && matches!(line.trim(), "y" | "Y" | "yes")
    })
    .await
    .unwrap_or(false)
}

/// Makes the adapter pairable for `timeout` with an agent that displays
/// passkeys / asks for numeric comparison on this terminal. Both pairing
/// methods are MITM-protected, which `security = "authenticated"` requires.
pub async fn pair(adapter_name: Option<&str>, timeout: Duration) -> Result<()> {
    let (session, adapter) = adapter(adapter_name).await?;
    let agent = Agent {
        request_default: true,
        display_passkey: Some(Box::new(|req| {
            async move {
                println!("\nPasskey for {}: {:06}  -- enter it on the other device.", req.device, req.passkey);
                Ok(())
            }
            .boxed()
        })),
        request_confirmation: Some(Box::new(|req| {
            async move {
                let ok = confirm(format!(
                    "\nDevice {} wants to pair. Does it show the passkey {:06}?",
                    req.device, req.passkey
                ))
                .await;
                if ok { Ok(()) } else { Err(ReqError::Rejected) }
            }
            .boxed()
        })),
        // Just Works pairing would not give us an authenticated bond; refuse it.
        request_authorization: Some(Box::new(|req| {
            async move {
                println!("\nRefusing unauthenticated pairing from {}.", req.device);
                Err(ReqError::Rejected)
            }
            .boxed()
        })),
        ..Default::default()
    };
    let _agent = session.register_agent(agent).await.context("registering pairing agent")?;

    let was_pairable = adapter.is_pairable().await?;
    let before = paired(&adapter).await?;
    adapter.set_pairable_timeout(timeout.as_secs() as u32).await?;
    adapter.set_pairable(true).await.context("making adapter pairable")?;
    println!(
        "Adapter {} ({}) is pairable for {}s.\n\
         Make sure blueterm-agent is running (systemctl status blueterm-agent), then connect\n\
         from the other device, e.g. `blueterm {}`. Press Ctrl-C to stop.",
        adapter.name(),
        adapter.address().await?,
        timeout.as_secs(),
        rustix::system::uname().nodename().to_string_lossy(),
    );

    let deadline = Instant::now() + timeout;
    let mut tick = interval(Duration::from_millis(500));
    let mut result = Ok(());
    let new = loop {
        tokio::select! {
            _ = tick.tick() => {
                let now = paired(&adapter).await?;
                let new: Vec<Address> = now.difference(&before).copied().collect();
                if !new.is_empty() {
                    break new;
                }
            }
            _ = sleep_until(deadline) => {
                result = Err(anyhow::anyhow!("no device paired within {}s", timeout.as_secs()));
                break Vec::new();
            }
            _ = tokio::signal::ctrl_c() => {
                result = Err(anyhow::anyhow!("interrupted"));
                break Vec::new();
            }
        }
    };
    for addr in &new {
        let dev = adapter.device(*addr)?;
        dev.set_trusted(true).await?;
        let name = dev.alias().await.unwrap_or_default();
        println!("Paired with {addr} ({name}).");
    }
    adapter.set_pairable(was_pairable).await?;
    result
}

pub async fn devices(adapter_name: Option<&str>) -> Result<()> {
    let (_session, adapter) = adapter(adapter_name).await?;
    let mut any = false;
    for addr in paired(&adapter).await? {
        let dev = adapter.device(addr)?;
        let alias = dev.alias().await.unwrap_or_default();
        let trusted = if dev.is_trusted().await.unwrap_or(false) { "trusted" } else { "" };
        let connected = if dev.is_connected().await.unwrap_or(false) { "connected" } else { "" };
        println!("{addr}  {alias:<24} {trusted:<8} {connected}");
        any = true;
    }
    if !any {
        println!("No paired devices.");
    }
    Ok(())
}

pub async fn unpair(adapter_name: Option<&str>, addr: Address) -> Result<()> {
    let (_session, adapter) = adapter(adapter_name).await?;
    adapter.remove_device(addr).await.with_context(|| format!("removing {addr}"))?;
    println!("Removed {addr}.");
    Ok(())
}
