//! macOS backend: CoreBluetooth via btleplug.

use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use blueterm_proto::{CONTROL, ControlMessage, Frame, NUS_RX, NUS_SERVICE, NUS_TX};
use btleplug::{
    api::{Central, CentralEvent, CentralState, Characteristic, Manager as _, Peripheral as _, ScanFilter, WriteType},
    platform::{Adapter, Manager, Peripheral, PeripheralId},
};
use futures::StreamExt;
use tokio::{sync::mpsc, time::timeout};
use tracing::debug;
use uuid::Uuid;

use super::{Found, Link, Options, chunks};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
/// ATT header of a Write Command.
const ATT_WRITE_HEADER: usize = 3;

pub struct Backend {
    adapter: Adapter,
}

fn map_err(e: btleplug::Error) -> anyhow::Error {
    match e {
        btleplug::Error::PermissionDenied => anyhow!(
            "Bluetooth access denied. Allow your terminal app under System Settings → \
             Privacy & Security → Bluetooth, then try again."
        ),
        e => anyhow!(e),
    }
}

impl Backend {
    pub async fn new(opts: &Options) -> Result<Backend> {
        if opts.adapter.is_some() {
            tracing::warn!("--adapter is ignored on macOS");
        }
        let manager = Manager::new().await.map_err(map_err)?;
        let adapter =
            manager.adapters().await.map_err(map_err)?.into_iter().next().context("no Bluetooth adapter found")?;
        // CoreBluetooth reports the real state shortly after start-up, or,
        // the first time, after the user answered the permission dialog.
        let mut state = adapter.adapter_state().await.map_err(map_err)?;
        let mut waited = Duration::ZERO;
        while state == CentralState::Unknown && waited < Duration::from_secs(60) {
            if waited == Duration::from_secs(1) {
                eprintln!("Waiting for Bluetooth (if macOS asks whether your terminal may use Bluetooth, allow it)...");
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
            waited += Duration::from_millis(100);
            state = adapter.adapter_state().await.map_err(map_err)?;
        }
        match state {
            CentralState::PoweredOn => Ok(Backend { adapter }),
            CentralState::PoweredOff => bail!("Bluetooth is turned off."),
            CentralState::Unknown => bail!(
                "Bluetooth is not available. If macOS did not ask for permission, allow your terminal app \
                 under System Settings → Privacy & Security → Bluetooth."
            ),
        }
    }

    pub async fn discover(&self, limit: Duration, mut stop: impl FnMut(&[Found]) -> bool) -> Result<Vec<Found>> {
        self.adapter.start_scan(ScanFilter { services: vec![NUS_SERVICE] }).await.map_err(map_err)?;
        let deadline = tokio::time::Instant::now() + limit;
        let result = loop {
            tokio::time::sleep(Duration::from_millis(250)).await;
            let list = self.collect().await?;
            if stop(&list) || tokio::time::Instant::now() >= deadline {
                break list;
            }
        };
        let _ = self.adapter.stop_scan().await;
        Ok(result)
    }

    async fn collect(&self) -> Result<Vec<Found>> {
        let mut out = Vec::new();
        for p in self.adapter.peripherals().await.map_err(map_err)? {
            let Ok(Some(props)) = p.properties().await else { continue };
            if !props.services.contains(&NUS_SERVICE) || props.rssi.is_none() {
                continue;
            }
            out.push(Found {
                id: p.id().to_string(),
                name: props.advertisement_name.or(props.local_name),
                rssi: props.rssi,
            });
        }
        Ok(out)
    }

    async fn peripheral(&self, found: &Found) -> Result<Peripheral> {
        let uuid: Uuid = found.id.parse().with_context(|| format!("invalid peripheral id {:?}", found.id))?;
        self.adapter.peripheral(&PeripheralId::from(uuid)).await.map_err(map_err)
    }

    pub async fn connect(&self, found: &Found, hello: &ControlMessage) -> Result<Link> {
        let p = self.peripheral(found).await?;
        let events = self.adapter.events().await.map_err(map_err)?;
        if !p.is_connected().await.map_err(map_err)? {
            timeout(CONNECT_TIMEOUT, p.connect()).await.context("connection timed out")?.map_err(map_err)?;
        }
        let res = setup(&p, hello, events).await;
        if res.is_err() {
            let _ = p.disconnect().await;
        }
        res
    }
}

fn find(p: &Peripheral, uuid: Uuid) -> Option<Characteristic> {
    p.characteristics().into_iter().find(|c| c.uuid == uuid && c.service_uuid == NUS_SERVICE)
}

async fn setup(
    p: &Peripheral,
    hello: &ControlMessage,
    mut events: std::pin::Pin<Box<dyn futures::Stream<Item = CentralEvent> + Send>>,
) -> Result<Link> {
    timeout(CONNECT_TIMEOUT, p.discover_services()).await.context("service discovery timed out")?.map_err(map_err)?;
    let rx_char = find(p, NUS_RX).context("device does not offer the Nordic UART Service")?;
    let tx_char = find(p, NUS_TX).context("device has an incomplete Nordic UART Service")?;
    let ctrl_char = find(p, CONTROL);

    let mut notifications = p.notifications().await.map_err(map_err)?;
    p.subscribe(&tx_char).await.map_err(map_err).context("subscribing to NUS TX")?;
    if let Some(c) = &ctrl_char {
        p.subscribe(c).await.map_err(map_err).context("subscribing to control characteristic")?;
        // A write with response: if the agent requires an authenticated link,
        // macOS starts pairing here (the system shows a dialog).
        p.write(c, &hello.encode(), WriteType::WithResponse).await.map_err(|e| {
            anyhow!(
                "{e}\nThe agent only accepts paired devices. Run `sudo blueterm-agent pair` on it, \
                 then connect again and accept the pairing request on this Mac."
            )
        })?;
    }
    let payload = (p.mtu() as usize).saturating_sub(ATT_WRITE_HEADER);
    debug!(mtu = p.mtu(), payload, "connected");

    let has_control = ctrl_char.is_some();
    let (tx, mut out_rx) = mpsc::channel::<Frame>(16);
    let (in_tx, rx) = mpsc::channel::<Frame>(32);
    let p = p.clone();
    let id = p.id();
    let task = tokio::spawn(async move {
        loop {
            tokio::select! {
                n = notifications.next() => match n {
                    None => break,
                    Some(n) if n.uuid == NUS_TX => {
                        if in_tx.send(Frame::Data(n.value)).await.is_err() { break }
                    }
                    Some(n) if n.uuid == CONTROL => match ControlMessage::decode(&n.value) {
                        Ok(Some(m)) => if in_tx.send(Frame::Control(m)).await.is_err() { break },
                        Ok(None) => {}
                        Err(e) => debug!("bad control message: {e}"),
                    },
                    Some(_) => {}
                },
                ev = events.next() => match ev {
                    Some(CentralEvent::DeviceDisconnected(d)) if d == id => break,
                    None => break,
                    _ => {}
                },
                f = out_rx.recv() => match f {
                    None => break,
                    Some(Frame::Data(d)) => {
                        // btleplug queues writes without response until
                        // CoreBluetooth is ready to send, so this does not drop data.
                        let mut failed = false;
                        for c in chunks(&d, payload) {
                            if p.write(&rx_char, c, WriteType::WithoutResponse).await.is_err() {
                                failed = true;
                                break;
                            }
                        }
                        if failed { break }
                    }
                    Some(Frame::Control(m)) => {
                        if let Some(c) = &ctrl_char
                            && let Err(e) = p.write(c, &m.encode(), WriteType::WithResponse).await {
                                debug!("control write failed: {e}");
                            }
                    }
                },
            }
        }
        drop(in_tx);
        let _ = p.disconnect().await;
    });
    Ok(Link { tx, rx, task, has_control })
}
