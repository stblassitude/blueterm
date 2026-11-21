//! BLE side of the agent: GATT application, advertising, and one connection
//! task per remote device that bridges BlueZ's per-device sockets to a
//! [`session`](crate::session).

use std::{collections::HashMap, sync::Arc, time::Duration};

use anyhow::{Context, Result};
use bluer::{
    Adapter, Address,
    adv::{Advertisement, AdvertisementHandle, Type as AdvType},
    gatt::{
        CharacteristicReader, CharacteristicWriter,
        local::{
            Application, Characteristic, CharacteristicControl, CharacteristicControlEvent, CharacteristicNotify,
            CharacteristicNotifyMethod, CharacteristicWrite, CharacteristicWriteIoRequest, CharacteristicWriteMethod,
            ReqError, Service, characteristic_control,
        },
    },
};
use blueterm_proto::{CONTROL, ControlMessage, Frame, MIN_PAYLOAD, NUS_RX, NUS_SERVICE, NUS_TX};
use futures::StreamExt;
use tokio::{
    signal::unix::{SignalKind, signal},
    sync::mpsc,
    task::JoinSet,
    time::sleep,
};
use tracing::{debug, error, info, warn};

use crate::{
    config::{Config, Security},
    session::{self, Link, SessionConfig},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// NUS RX/TX: terminal data.
    Data,
    /// blueterm control characteristic.
    Control,
}

/// A per-device socket handed over by BlueZ.
enum Part {
    Reader(Kind, CharacteristicReader),
    Writer(Kind, CharacteristicWriter),
}

enum Event {
    Write(Kind, CharacteristicWriteIoRequest),
    Notify(Kind, CharacteristicWriter),
}

impl Event {
    fn device(&self) -> Address {
        match self {
            Event::Write(_, r) => r.device_address(),
            Event::Notify(_, w) => w.device_address(),
        }
    }
}

fn map_event(kind: Kind, ev: CharacteristicControlEvent) -> Event {
    match ev {
        CharacteristicControlEvent::Write(r) => Event::Write(kind, r),
        CharacteristicControlEvent::Notify(w) => Event::Notify(kind, w),
    }
}

fn write_props(secure: bool) -> CharacteristicWrite {
    CharacteristicWrite {
        write: true,
        write_without_response: true,
        encrypt_authenticated_write: secure,
        method: CharacteristicWriteMethod::Io,
        ..Default::default()
    }
}

fn notify_props() -> CharacteristicNotify {
    CharacteristicNotify { notify: true, method: CharacteristicNotifyMethod::Io, ..Default::default() }
}

pub async fn run(cfg: Config) -> Result<()> {
    if !rustix::process::geteuid().is_root() {
        warn!("not running as root; the login command will probably fail");
    }
    let secure = cfg.security == Security::Authenticated;
    if !secure {
        warn!("security = \"open\": ANYONE IN RADIO RANGE CAN OPEN A LOGIN SESSION. Use only for testing.");
    }

    let bt = bluer::Session::new().await.context("connecting to BlueZ over D-Bus (is bluetoothd running?)")?;
    let adapter = match &cfg.adapter {
        Some(name) => bt.adapter(name)?,
        None => bt.default_adapter().await.context("no Bluetooth adapter found")?,
    };
    if !adapter.is_powered().await? {
        info!(adapter = adapter.name(), "powering on adapter");
        adapter.set_powered(true).await.context("powering on adapter (blocked by rfkill?)")?;
    }

    let name = cfg.advertised_name();
    let scfg = Arc::new(SessionConfig {
        login_command: cfg.login_command.clone(),
        env: cfg.env.iter().filter_map(|e| e.split_once('=').map(|(k, v)| (k.to_owned(), v.to_owned()))).collect(),
        require_write: secure,
        hello_timeout: cfg.hello_timeout(),
        start_timeout: cfg.start_timeout(),
    });

    let (rx_ctl, rx_handle) = characteristic_control();
    let (tx_ctl, tx_handle) = characteristic_control();
    let (ctrl_ctl, ctrl_handle) = characteristic_control();
    let app = Application {
        services: vec![Service {
            uuid: NUS_SERVICE,
            primary: true,
            characteristics: vec![
                Characteristic {
                    uuid: NUS_RX,
                    write: Some(write_props(secure)),
                    control_handle: rx_handle,
                    ..Default::default()
                },
                Characteristic {
                    uuid: NUS_TX,
                    notify: Some(notify_props()),
                    control_handle: tx_handle,
                    ..Default::default()
                },
                Characteristic {
                    uuid: CONTROL,
                    write: Some(write_props(secure)),
                    notify: Some(notify_props()),
                    control_handle: ctrl_handle,
                    ..Default::default()
                },
            ],
            ..Default::default()
        }],
        ..Default::default()
    };
    let _app = adapter.serve_gatt_application(app).await.context("registering GATT application")?;

    let mut adv = Some(advertise(&adapter, &name).await?);
    info!(adapter = adapter.name(), address = %adapter.address().await?, %name, ?cfg.security, "ready");

    let (done_tx, mut done_rx) = mpsc::channel::<Address>(8);
    let mut conns: HashMap<Address, mpsc::Sender<Part>> = HashMap::new();
    let mut controls: [(Kind, CharacteristicControl); 3] =
        [(Kind::Data, rx_ctl), (Kind::Data, tx_ctl), (Kind::Control, ctrl_ctl)];
    let mut sigterm = signal(SignalKind::terminate())?;
    let mut sigint = signal(SignalKind::interrupt())?;

    loop {
        let [(k0, c0), (k1, c1), (k2, c2)] = &mut controls;
        let ev = tokio::select! {
            Some(ev) = c0.next() => map_event(*k0, ev),
            Some(ev) = c1.next() => map_event(*k1, ev),
            Some(ev) = c2.next() => map_event(*k2, ev),
            Some(addr) = done_rx.recv() => {
                if conns.get(&addr).is_some_and(|s| s.is_closed()) {
                    conns.remove(&addr);
                }
                if adv.is_none() && conns.len() < cfg.max_sessions {
                    adv = Some(advertise(&adapter, &name).await?);
                }
                continue;
            }
            _ = sigterm.recv() => break,
            _ = sigint.recv() => break,
        };

        let addr = ev.device();
        let sender = match conns.get(&addr).filter(|s| !s.is_closed()) {
            Some(s) => s.clone(),
            None => {
                conns.retain(|_, s| !s.is_closed());
                if conns.len() >= cfg.max_sessions {
                    warn!(%addr, "rejecting device: all {} session(s) in use", cfg.max_sessions);
                    if let Event::Write(_, req) = ev {
                        req.reject(ReqError::NotPermitted);
                    }
                    continue;
                }
                info!(%addr, "device connected");
                let (tx, rx) = mpsc::channel(8);
                tokio::spawn(connection(addr, rx, adapter.clone(), scfg.clone(), done_tx.clone()));
                conns.insert(addr, tx.clone());
                if conns.len() >= cfg.max_sessions {
                    adv = None; // stop advertising while full
                }
                tx
            }
        };
        let part = match ev {
            Event::Write(kind, req) => match req.accept() {
                Ok(r) => Part::Reader(kind, r),
                Err(e) => {
                    warn!(%addr, "accepting write socket failed: {e}");
                    continue;
                }
            },
            Event::Notify(kind, w) => Part::Writer(kind, w),
        };
        let _ = sender.send(part).await;
    }

    info!("shutting down");
    drop(adv);
    for (addr, _) in conns {
        if let Ok(dev) = adapter.device(addr) {
            let _ = dev.disconnect().await;
        }
    }
    Ok(())
}

async fn advertise(adapter: &Adapter, name: &str) -> Result<AdvertisementHandle> {
    let adv = Advertisement {
        advertisement_type: AdvType::Peripheral,
        service_uuids: [NUS_SERVICE].into_iter().collect(),
        local_name: Some(name.to_owned()),
        discoverable: Some(true),
        ..Default::default()
    };
    adapter.advertise(adv).await.context("registering LE advertisement")
}

/// Bridges one device's sockets to a session.
async fn connection(
    addr: Address,
    mut parts: mpsc::Receiver<Part>,
    adapter: Adapter,
    cfg: Arc<SessionConfig>,
    done: mpsc::Sender<Address>,
) {
    let (to_session, session_rx) = mpsc::channel::<Frame>(32);
    let (session_tx, mut from_session) = mpsc::channel::<Frame>(8);
    let mut link = Some(Link { rx: session_rx, tx: session_tx });
    let mut session: Option<tokio::task::JoinHandle<()>> = None;
    let mut readers = JoinSet::new();
    let mut data_out: Option<CharacteristicWriter> = None;
    let mut ctrl_out: Option<CharacteristicWriter> = None;
    let mut session_over = false;
    let give_up = sleep(cfg.start_timeout + Duration::from_secs(5));
    tokio::pin!(give_up);

    loop {
        tokio::select! {
            p = parts.recv() => match p {
                None => break,
                Some(Part::Reader(kind, r)) => {
                    debug!(%addr, ?kind, mtu = r.mtu(), "write socket acquired");
                    readers.spawn(pump_reader(kind, r, to_session.clone()));
                }
                Some(Part::Writer(Kind::Data, w)) => {
                    debug!(%addr, mtu = w.mtu(), "notify socket acquired");
                    data_out = Some(w);
                    if let Some(link) = link.take() {
                        let cfg = cfg.clone();
                        session = Some(tokio::spawn(async move {
                            if let Err(e) = session::run(&cfg, link).await {
                                warn!(%addr, "session: {e:#}");
                            }
                        }));
                    }
                }
                Some(Part::Writer(Kind::Control, w)) => ctrl_out = Some(w),
            },
            f = from_session.recv() => match f {
                None => {
                    session_over = true;
                    break;
                }
                Some(Frame::Data(d)) => {
                    let Some(w) = &data_out else { continue };
                    if let Err(e) = send_chunked(w, &d).await {
                        debug!(%addr, "notify failed: {e}");
                        break;
                    }
                }
                Some(Frame::Control(m)) => {
                    let Some(w) = &ctrl_out else { continue };
                    let mut b = m.encode();
                    b.truncate(w.mtu().max(MIN_PAYLOAD));
                    if let Err(e) = w.send(&b).await {
                        debug!(%addr, "control notify failed: {e}");
                    }
                }
            },
            _ = async { data_out.as_ref().unwrap().closed().await }, if data_out.is_some() => {
                debug!(%addr, "client unsubscribed or disconnected");
                break;
            }
            _ = &mut give_up, if session.is_none() => {
                debug!(%addr, "no session started, giving up");
                break;
            }
        }
    }

    // Stop accepting sockets right away: if the device reconnects while we
    // tear down, the main loop must start a fresh connection task.
    parts.close();

    // Tell the session the client is gone (if it is still running) by closing
    // every sender of its input, then wait for it to clean up.
    readers.abort_all();
    drop(to_session);
    drop(link);
    // Nobody reads session output any more; make its sends fail instead of
    // blocking, so it notices and hangs up.
    drop(from_session);
    if let Some(s) = session
        && let Err(e) = s.await
    {
        error!(%addr, "session task failed: {e}");
    }
    if session_over {
        // Let the last notifications (SessionEnded) go out before hanging up.
        sleep(Duration::from_millis(300)).await;
    }
    drop((data_out, ctrl_out));
    if let Ok(dev) = adapter.device(addr)
        && dev.is_connected().await.unwrap_or(false)
    {
        info!(%addr, "disconnecting device");
        let _ = dev.disconnect().await;
    }
    info!(%addr, "connection closed");
    let _ = done.send(addr).await;
}

async fn send_chunked(w: &CharacteristicWriter, data: &[u8]) -> std::io::Result<()> {
    for chunk in data.chunks(w.mtu().max(MIN_PAYLOAD)) {
        w.send(chunk).await?;
    }
    Ok(())
}

async fn pump_reader(kind: Kind, r: CharacteristicReader, to_session: mpsc::Sender<Frame>) {
    loop {
        let v = match r.recv().await {
            Ok(v) if !v.is_empty() => v,
            _ => break,
        };
        let frame = match kind {
            Kind::Data => Frame::Data(v),
            Kind::Control => match ControlMessage::decode(&v) {
                Ok(Some(m)) => Frame::Control(m),
                Ok(None) => continue,
                Err(e) => {
                    warn!(addr = %r.device_address(), "bad control message: {e}");
                    continue;
                }
            },
        };
        if to_session.send(frame).await.is_err() {
            break;
        }
    }
}
