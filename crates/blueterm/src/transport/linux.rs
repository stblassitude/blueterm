//! Linux backend: BlueZ via bluer, central role.

use std::{
    collections::BTreeSet,
    io::Write,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, anyhow, bail};
use bluer::{
    Adapter, AdapterEvent, Address, Device, DiscoveryFilter, DiscoveryTransport,
    agent::{Agent, ReqError},
    gatt::remote::Characteristic,
};
use blueterm_proto::{CONTROL, ControlMessage, Frame, NUS_RX, NUS_SERVICE, NUS_TX};
use futures::{FutureExt, StreamExt};
use tokio::{sync::mpsc, time::timeout};
use tracing::debug;

use super::{Found, Link, Options, chunks};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);

pub struct Backend {
    session: bluer::Session,
    adapter: Adapter,
}

impl Backend {
    pub async fn new(opts: &Options) -> Result<Backend> {
        let session = bluer::Session::new()
            .await
            .context("cannot reach BlueZ over D-Bus (is bluetoothd running? are you allowed to use it?)")?;
        let adapter = match &opts.adapter {
            Some(n) => session.adapter(n)?,
            None => session.default_adapter().await.context("no Bluetooth adapter found")?,
        };
        if !adapter.is_powered().await? {
            bail!(
                "Bluetooth adapter {} is powered off. Turn it on with `bluetoothctl power on` \
                 (if that fails, check `rfkill list`).",
                adapter.name()
            );
        }
        Ok(Backend { session, adapter })
    }

    pub async fn discover(&self, limit: Duration, mut stop: impl FnMut(&[Found]) -> bool) -> Result<Vec<Found>> {
        self.adapter
            .set_discovery_filter(DiscoveryFilter {
                uuids: [NUS_SERVICE].into_iter().collect(),
                transport: DiscoveryTransport::Le,
                ..Default::default()
            })
            .await?;
        let own = self.adapter.address().await?;
        let events = self.adapter.discover_devices().await.context("starting discovery")?;
        tokio::pin!(events);
        let deadline = tokio::time::sleep(limit);
        tokio::pin!(deadline);
        let mut tick = tokio::time::interval(Duration::from_millis(250));
        let mut seen = BTreeSet::new();
        loop {
            tokio::select! {
                ev = events.next() => match ev {
                    Some(AdapterEvent::DeviceAdded(addr)) if addr != own => { seen.insert(addr); }
                    Some(_) => {}
                    None => break,
                },
                _ = tick.tick() => {
                    let list = self.collect(&seen).await;
                    if stop(&list) {
                        return Ok(list);
                    }
                }
                _ = &mut deadline => break,
            }
        }
        Ok(self.collect(&seen).await)
    }

    /// Devices that advertise NUS and were heard during this scan (BlueZ
    /// also reports cached devices; those have no RSSI).
    async fn collect(&self, seen: &BTreeSet<Address>) -> Vec<Found> {
        let mut out = Vec::new();
        for &addr in seen {
            let Ok(dev) = self.adapter.device(addr) else { continue };
            let has_nus = dev.uuids().await.ok().flatten().is_some_and(|u| u.contains(&NUS_SERVICE));
            let rssi = dev.rssi().await.ok().flatten();
            if !has_nus || rssi.is_none() {
                continue;
            }
            let name = dev.name().await.ok().flatten();
            out.push(Found { id: addr.to_string(), name, rssi });
        }
        out
    }

    fn device(&self, found: &Found) -> Result<Device> {
        let addr: Address = found.id.parse().map_err(|_| anyhow!("invalid Bluetooth address {:?}", found.id))?;
        Ok(self.adapter.device(addr)?)
    }

    pub async fn connect(&self, found: &Found, hello: &ControlMessage) -> Result<Link> {
        let dev = self.device(found)?;
        if !dev.is_connected().await? {
            timeout(CONNECT_TIMEOUT, dev.connect()).await.context("connection timed out")?.context("connecting")?;
        }
        let res = setup(&dev, hello).await;
        if res.is_err() {
            let _ = dev.disconnect().await;
        }
        res
    }

    /// Pairs and bonds with the agent, prompting on this terminal.
    pub async fn pair(&self, found: &Found) -> Result<()> {
        let agent = Agent {
            request_default: true,
            request_passkey: Some(Box::new(|req| {
                async move {
                    let line = prompt(format!("Enter the passkey shown by blueterm-agent for {}: ", req.device)).await;
                    line.trim().parse::<u32>().map_err(|_| ReqError::Rejected)
                }
                .boxed()
            })),
            display_passkey: Some(Box::new(|req| {
                async move {
                    println!("Passkey: {:06}  -- enter it on {}.", req.passkey, req.device);
                    Ok(())
                }
                .boxed()
            })),
            request_confirmation: Some(Box::new(|req| {
                async move {
                    let line = prompt(format!(
                        "Does blueterm-agent on {} show the passkey {:06}? [y/N] ",
                        req.device, req.passkey
                    ))
                    .await;
                    if matches!(line.trim(), "y" | "Y" | "yes") { Ok(()) } else { Err(ReqError::Rejected) }
                }
                .boxed()
            })),
            ..Default::default()
        };
        let _agent = self.session.register_agent(agent).await.context("registering pairing agent")?;
        let dev = self.device(found)?;
        if dev.is_paired().await? {
            println!("Already paired with {} ({}).", found.display_name(), found.id);
        } else {
            dev.pair().await.context("pairing failed (is `blueterm-agent pair` running on the other side?)")?;
            println!("Paired with {} ({}).", found.display_name(), found.id);
        }
        dev.set_trusted(true).await?;
        let _ = dev.disconnect().await;
        Ok(())
    }
}

async fn prompt(text: String) -> String {
    tokio::task::spawn_blocking(move || {
        print!("{text}");
        let _ = std::io::stdout().flush();
        let mut line = String::new();
        let _ = std::io::stdin().read_line(&mut line);
        line
    })
    .await
    .unwrap_or_default()
}

async fn find_characteristics(dev: &Device) -> Result<(Characteristic, Characteristic, Option<Characteristic>)> {
    let start = Instant::now();
    while !dev.is_services_resolved().await? {
        if start.elapsed() > CONNECT_TIMEOUT {
            bail!("timed out resolving GATT services");
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    for svc in dev.services().await? {
        if svc.uuid().await? != NUS_SERVICE {
            continue;
        }
        let (mut rx, mut tx, mut ctrl) = (None, None, None);
        for c in svc.characteristics().await? {
            match c.uuid().await? {
                NUS_RX => rx = Some(c),
                NUS_TX => tx = Some(c),
                CONTROL => ctrl = Some(c),
                _ => {}
            }
        }
        return match (rx, tx) {
            (Some(rx), Some(tx)) => Ok((rx, tx, ctrl)),
            _ => bail!("device has an incomplete Nordic UART Service"),
        };
    }
    bail!("device does not offer the Nordic UART Service")
}

fn auth_hint(e: bluer::Error) -> anyhow::Error {
    let s = e.to_string();
    if s.contains("Authenticat") || s.contains("Not paired") || s.contains("Insufficient") || s.contains("rejected") {
        anyhow!(
            "{e}\nThe agent only accepts paired devices. Run `sudo blueterm-agent pair` on it, then `blueterm pair <name>` here."
        )
    } else {
        anyhow!(e).context("starting session")
    }
}

async fn setup(dev: &Device, hello: &ControlMessage) -> Result<Link> {
    let (rx_char, tx_char, ctrl_char) = find_characteristics(dev).await?;
    let data_in = tx_char.notify_io().await.context("subscribing to NUS TX")?;
    let ctrl_in = match &ctrl_char {
        Some(c) => Some(c.notify_io().await.context("subscribing to control characteristic")?),
        None => None,
    };
    // A write with response: if the agent requires an authenticated link,
    // this is where BlueZ elevates security (or fails).
    if let Some(c) = &ctrl_char {
        c.write(&hello.encode()).await.map_err(auth_hint)?;
    }
    let data_out = rx_char.write_io().await.context("opening NUS RX for writing")?;
    debug!(write_mtu = data_out.mtu(), notify_mtu = data_in.mtu(), "connected");

    let has_control = ctrl_char.is_some();
    let (tx, mut out_rx) = mpsc::channel::<Frame>(16);
    let (in_tx, rx) = mpsc::channel::<Frame>(32);
    let dev = dev.clone();
    let task = tokio::spawn(async move {
        let mut ctrl_in = ctrl_in;
        loop {
            tokio::select! {
                r = data_in.recv() => match r {
                    Ok(v) if !v.is_empty() => if in_tx.send(Frame::Data(v)).await.is_err() { break },
                    _ => break,
                },
                r = async { ctrl_in.as_ref().unwrap().recv().await }, if ctrl_in.is_some() => match r {
                    Ok(v) if !v.is_empty() => match ControlMessage::decode(&v) {
                        Ok(Some(m)) => if in_tx.send(Frame::Control(m)).await.is_err() { break },
                        Ok(None) => {}
                        Err(e) => debug!("bad control message: {e}"),
                    },
                    _ => ctrl_in = None,
                },
                f = out_rx.recv() => match f {
                    None => break,
                    Some(Frame::Data(d)) => {
                        let mut failed = false;
                        for c in chunks(&d, data_out.mtu()) {
                            if data_out.send(c).await.is_err() {
                                failed = true;
                                break;
                            }
                        }
                        if failed { break }
                    }
                    Some(Frame::Control(m)) => {
                        if let Some(c) = &ctrl_char
                            && let Err(e) = c.write(&m.encode()).await {
                                debug!("control write failed: {e}");
                            }
                    }
                },
            }
        }
        drop(in_tx);
        let _ = dev.disconnect().await;
    });
    Ok(Link { tx, rx, task, has_control })
}
