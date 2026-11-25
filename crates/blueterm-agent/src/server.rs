//! BLE side of the agent: GATT application, advertising, and one connection
//! task per remote device that bridges BlueZ's per-device sockets to a
//! [`session`](crate::session).

use std::{collections::HashMap, num::NonZeroU16, sync::Arc, time::Duration};

use anyhow::{Context, Result};
use bluer::{
    Adapter, Address, ErrorKind, InternalErrorKind,
    adv::{Advertisement, AdvertisementHandle, Type as AdvType},
    gatt::{
        CharacteristicReader, CharacteristicWriter,
        local::{
            Application, ApplicationHandle, Characteristic, CharacteristicControl, CharacteristicControlEvent,
            CharacteristicNotify, CharacteristicNotifyMethod, CharacteristicWrite, CharacteristicWriteIoRequest,
            CharacteristicWriteMethod, ReqError, Service, ServiceControl, characteristic_control, service_control,
        },
    },
};
use blueterm_proto::{CONTROL, ControlMessage, Frame, MIN_PAYLOAD, NUS_RX, NUS_SERVICE, NUS_TX};
use futures::StreamExt;
use tokio::{
    signal::unix::{SignalKind, signal},
    sync::mpsc,
    task::JoinSet,
    time::{Instant, sleep},
};
use tracing::{debug, error, info, warn};

use crate::{
    config::{Config, Security},
    session::{self, Link, SessionConfig},
};

/// How long to keep trying to power on the adapter while BlueZ is busy.
const POWER_ON_TIMEOUT: Duration = Duration::from_secs(10);
const POWER_ON_RETRY: Duration = Duration::from_millis(250);

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
    power_on(&adapter).await?;

    let name = cfg.advertised_name();
    let scfg = Arc::new(SessionConfig {
        login_command: cfg.login_command.clone(),
        env: cfg.env.iter().filter_map(|e| e.split_once('=').map(|(k, v)| (k.to_owned(), v.to_owned()))).collect(),
        require_write: secure,
        hello_timeout: cfg.hello_timeout(),
        start_timeout: cfg.start_timeout(),
    });

    let mut gatt = Gatt::register(&adapter, secure, None).await.context("registering GATT application")?;

    let mut adv = Some(advertise(&adapter, &name).await?);
    info!(adapter = adapter.name(), address = %adapter.address().await?, %name, ?cfg.security, "ready");

    let (done_tx, mut done_rx) = mpsc::channel::<Address>(8);
    let mut conns: HashMap<Address, mpsc::Sender<Part>> = HashMap::new();
    let mut sigterm = signal(SignalKind::terminate())?;
    let mut sigint = signal(SignalKind::interrupt())?;

    loop {
        let [(k0, c0), (k1, c1), (k2, c2)] = &mut gatt.controls;
        let ev = tokio::select! {
            Some(ev) = c0.next() => map_event(*k0, ev),
            Some(ev) = c1.next() => map_event(*k1, ev),
            Some(ev) = c2.next() => map_event(*k2, ev),
            Some(addr) = done_rx.recv() => {
                if conns.get(&addr).is_some_and(|s| s.is_closed()) {
                    conns.remove(&addr);
                }
                conns.retain(|_, s| !s.is_closed());
                if conns.is_empty() {
                    // BlueZ remembers a bonded device's notification
                    // subscriptions after it disconnects, and ignores the
                    // client re-subscribing on its next connection. Then it
                    // never calls AcquireNotify, we get no notify socket, and
                    // the session cannot start. Re-registering the service
                    // makes BlueZ forget those subscriptions.
                    adv = None;
                    gatt = gatt.reregister(&adapter, secure).await.context("re-registering GATT application")?;
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

/// Powers on the adapter if it is off.
///
/// At boot, bluetoothd powers on the adapter itself, and refuses our request
/// as busy until it is done.
async fn power_on(adapter: &Adapter) -> Result<()> {
    if adapter.is_powered().await? {
        return Ok(());
    }
    info!(adapter = adapter.name(), "powering on adapter");
    let started = Instant::now();
    loop {
        match adapter.set_powered(true).await {
            Ok(()) => return Ok(()),
            Err(e) if is_busy(&e.kind) && started.elapsed() < POWER_ON_TIMEOUT => {
                debug!("powering on adapter failed, retrying: {e}");
                sleep(POWER_ON_RETRY).await;
            }
            Err(e) => return Err(e).context("powering on adapter (blocked by rfkill?)"),
        }
    }
}

/// True if BlueZ refused a request because a similar one is in progress.
fn is_busy(kind: &ErrorKind) -> bool {
    match kind {
        ErrorKind::InProgress => true,
        // bluer has no error kind for this one.
        ErrorKind::Internal(InternalErrorKind::DBus(name)) => name == "org.bluez.Error.Busy",
        _ => false,
    }
}

/// Our registered GATT application.
struct Gatt {
    _app: ApplicationHandle,
    /// Tells us the handle BlueZ assigned to the NUS service.
    service: ServiceControl,
    controls: [(Kind, CharacteristicControl); 3],
}

impl Gatt {
    /// Registers the NUS service, at `service_handle` if given.
    async fn register(adapter: &Adapter, secure: bool, service_handle: Option<NonZeroU16>) -> Result<Gatt> {
        let (service_ctl, service_ctl_handle) = service_control();
        let (rx_ctl, rx_handle) = characteristic_control();
        let (tx_ctl, tx_handle) = characteristic_control();
        let (ctrl_ctl, ctrl_handle) = characteristic_control();
        let app = Application {
            services: vec![Service {
                uuid: NUS_SERVICE,
                handle: service_handle,
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
                control_handle: service_ctl_handle,
                ..Default::default()
            }],
            ..Default::default()
        };
        let app = adapter.serve_gatt_application(app).await?;
        Ok(Gatt {
            _app: app,
            service: service_ctl,
            controls: [(Kind::Data, rx_ctl), (Kind::Data, tx_ctl), (Kind::Control, ctrl_ctl)],
        })
    }

    /// Unregisters the application and registers it again. BlueZ drops all
    /// per-device state of a service it removes, including the notification
    /// subscriptions of bonded devices.
    ///
    /// The service gets its old handle back, and its characteristics are
    /// always registered in the same order (this needs our patched
    /// dbus-crossroads), so the attribute database looks exactly as before.
    /// Clients cache it (macOS does, for bonded devices), and we cannot rely
    /// on BlueZ's Service Changed indication to tell them about changes:
    /// macOS can miss it on the second connection after bluetoothd restarted.
    async fn reregister(self, adapter: &Adapter, secure: bool) -> Result<Gatt> {
        let service_handle = self.service.handle().ok();
        debug!(?service_handle, "re-registering GATT application");
        // bluer unregisters in the background once the handle is dropped.
        // Until BlueZ has removed the old service, its handles are taken
        // and registering at the same handle fails, so retry for a bit.
        drop(self);
        let mut attempt = 0;
        loop {
            sleep(Duration::from_millis(100)).await;
            match Gatt::register(adapter, secure, service_handle).await {
                Ok(g) => return Ok(g),
                Err(e) if attempt < 20 => {
                    debug!("registering GATT application failed, retrying: {e}");
                    attempt += 1;
                }
                Err(e) => return Err(e),
            }
        }
    }
}

async fn advertise(adapter: &Adapter, name: &str) -> Result<AdvertisementHandle> {
    let adv = Advertisement {
        advertisement_type: AdvType::Peripheral,
        service_uuids: [NUS_SERVICE].into_iter().collect(),
        local_name: Some(name.to_owned()),
        discoverable: Some(true),
        // BlueZ's default of 1.28 s is so slow that a short scan, especially
        // by macOS, often misses us. Apple recommends 152.5 ms for quick
        // discovery.
        min_interval: Some(Duration::from_millis(100)),
        max_interval: Some(Duration::from_millis(150)),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn busy_errors() {
        let dbus = |name: &str| ErrorKind::Internal(InternalErrorKind::DBus(name.into()));
        assert!(is_busy(&dbus("org.bluez.Error.Busy")));
        assert!(is_busy(&ErrorKind::InProgress));
        assert!(!is_busy(&dbus("org.bluez.Error.Blocked")));
        assert!(!is_busy(&ErrorKind::Failed));
    }
}
