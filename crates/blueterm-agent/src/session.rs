//! One login session, independent of the BLE transport.
//!
//! The transport hands us a [`Link`]: frames from the client arrive on `rx`,
//! frames for the client go to `tx`. `rx` returning `None` means the client is
//! gone. Returning from [`run`] (dropping `tx`) tells the transport the session
//! is over.

use std::time::Duration;

use anyhow::{Context, Result};
use blueterm_proto::{ControlMessage, DEFAULT_SIZE, DEFAULT_TERM, Frame, PROTOCOL_VERSION};
use rustix::process::Signal;
use tokio::{
    sync::mpsc,
    time::{Instant, sleep, sleep_until, timeout},
};
use tracing::{debug, info, warn};

use crate::pty;

pub struct Link {
    pub rx: mpsc::Receiver<Frame>,
    pub tx: mpsc::Sender<Frame>,
}

#[derive(Debug, Clone)]
pub struct SessionConfig {
    pub login_command: Vec<String>,
    pub env: Vec<(String, String)>,
    /// If true, only a write from the client (Hello or data) starts the
    /// session. Used in authenticated mode, because notification
    /// subscriptions cannot be protected by BlueZ, but writes can.
    pub require_write: bool,
    /// Open mode: start with defaults if no Hello arrives within this time.
    pub hello_timeout: Duration,
    /// Give up on a client that has not started a session within this time.
    pub start_timeout: Duration,
}

/// How long to keep relaying output after the login process exited, to
/// flush what it wrote last.
const DRAIN_TIME: Duration = Duration::from_millis(300);
/// How long to wait for the session's processes after hanging up.
const HANGUP_GRACE: Duration = Duration::from_secs(3);

enum Start {
    Hello { rows: u16, cols: u16, term: String },
    Default,
}

pub async fn run(cfg: &SessionConfig, mut link: Link) -> Result<()> {
    let Some(start) = wait_for_start(cfg, &mut link.rx).await? else {
        return Ok(());
    };
    let (rows, cols, term, hello) = match start {
        Start::Hello { rows, cols, term } => (rows, cols, term, true),
        Start::Default => (DEFAULT_SIZE.0, DEFAULT_SIZE.1, DEFAULT_TERM.to_owned(), false),
    };

    let mut env = cfg.env.clone();
    env.push(("TERM".into(), term.clone()));
    let pty::Spawned { pty, mut child } = match pty::spawn(&cfg.login_command, &env, rows, cols) {
        Ok(s) => s,
        Err(e) => {
            let msg = format!("cannot start {:?}: {e}", cfg.login_command[0]);
            let _ = link.tx.send(Frame::Control(ControlMessage::Error(msg.clone()))).await;
            anyhow::bail!(msg);
        }
    };
    info!(pid = child.id(), %term, rows, cols, "session started");
    if hello {
        let ack = ControlMessage::HelloAck { version: PROTOCOL_VERSION, flags: 0 };
        if link.tx.send(Frame::Control(ack)).await.is_err() {
            return hang_up(child).await;
        }
    }

    let mut buf = vec![0u8; 4096];
    let mut exit_status = None;
    let mut drain_until = None;
    loop {
        tokio::select! {
            r = pty.read(&mut buf) => {
                let n = r.context("reading pty")?;
                if n == 0 {
                    break; // every process holding the terminal is gone
                }
                if link.tx.send(Frame::Data(buf[..n].to_vec())).await.is_err() {
                    return hang_up(child).await;
                }
            }
            f = link.rx.recv() => match f {
                None => {
                    info!("client went away");
                    return hang_up(child).await;
                }
                Some(Frame::Data(d)) => pty.write_all(&d).await.context("writing pty")?,
                Some(Frame::Control(ControlMessage::Resize { rows, cols }))
                | Some(Frame::Control(ControlMessage::Hello { rows, cols, .. })) => {
                    debug!(rows, cols, "resize");
                    if let Err(e) = pty.resize(rows, cols) {
                        warn!("resize failed: {e}");
                    }
                }
                Some(Frame::Control(m)) => debug!(?m, "ignoring control message"),
            },
            st = child.wait(), if exit_status.is_none() => {
                exit_status = Some(st.context("waiting for login process")?);
                drain_until = Some(Instant::now() + DRAIN_TIME);
            }
            _ = sleep_until(drain_until.unwrap_or_else(Instant::now)), if drain_until.is_some() => break,
        }
    }

    // The terminal is closed or the login process is done. Make sure nothing
    // is left behind on it, and collect the exit status.
    drop(pty);
    let status = match exit_status {
        Some(st) => pty::status_code(st),
        None => match timeout(HANGUP_GRACE, child.wait()).await {
            Ok(st) => pty::status_code(st.context("waiting for login process")?),
            Err(_) => {
                pty::signal_group(&child, Signal::HUP);
                reap(child).await
            }
        },
    };
    info!(status, "session ended");
    let _ = link.tx.send(Frame::Control(ControlMessage::SessionEnded { status })).await;
    Ok(())
}

async fn wait_for_start(cfg: &SessionConfig, rx: &mut mpsc::Receiver<Frame>) -> Result<Option<Start>> {
    let deadline = Instant::now() + if cfg.require_write { cfg.start_timeout } else { cfg.hello_timeout };
    loop {
        tokio::select! {
            f = rx.recv() => match f {
                None => return Ok(None),
                Some(Frame::Control(ControlMessage::Hello { version, rows, cols, term })) => {
                    debug!(version, rows, cols, %term, "hello");
                    let (rows, cols) = if rows == 0 || cols == 0 { DEFAULT_SIZE } else { (rows, cols) };
                    return Ok(Some(Start::Hello { rows, cols, term }));
                }
                // A generic NUS client: the first keystroke (typically Enter)
                // wakes up the console, like on a serial line. It is not passed
                // on, so it does not end up as an empty login name.
                Some(Frame::Data(_)) => return Ok(Some(Start::Default)),
                Some(Frame::Control(m)) => debug!(?m, "ignoring control message before start"),
            },
            _ = sleep_until(deadline) => {
                if cfg.require_write {
                    anyhow::bail!("client did not start a session within {:?}", cfg.start_timeout);
                }
                return Ok(Some(Start::Default));
            }
        }
    }
}

/// The client is gone: hang up the terminal like a modem would, then reap.
async fn hang_up(child: tokio::process::Child) -> Result<()> {
    pty::signal_group(&child, Signal::HUP);
    let status = reap(child).await;
    info!(status, "session hung up");
    Ok(())
}

async fn reap(mut child: tokio::process::Child) -> i32 {
    match timeout(HANGUP_GRACE, child.wait()).await {
        Ok(Ok(st)) => pty::status_code(st),
        Ok(Err(_)) => 255,
        Err(_) => {
            warn!("session did not exit after SIGHUP, killing it");
            pty::signal_group(&child, Signal::KILL);
            let _ = child.kill().await;
            sleep(Duration::from_millis(100)).await;
            255
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(cmd: &str, require_write: bool) -> SessionConfig {
        SessionConfig {
            login_command: vec!["/bin/bash".into(), "-c".into(), cmd.into()],
            env: vec![("PATH".into(), "/usr/bin:/bin".into())],
            require_write,
            hello_timeout: Duration::from_millis(100),
            start_timeout: Duration::from_millis(500),
        }
    }

    struct Client {
        to_agent: mpsc::Sender<Frame>,
        from_agent: mpsc::Receiver<Frame>,
    }

    fn link() -> (Link, Client) {
        let (to_agent, rx) = mpsc::channel(16);
        let (tx, from_agent) = mpsc::channel(16);
        (Link { rx, tx }, Client { to_agent, from_agent })
    }

    /// Collects output until SessionEnded; returns (output, status).
    async fn collect(c: &mut Client) -> (String, Option<i32>) {
        let mut out = Vec::new();
        while let Ok(Some(f)) = timeout(Duration::from_secs(10), c.from_agent.recv()).await {
            match f {
                Frame::Data(d) => out.extend(d),
                Frame::Control(ControlMessage::SessionEnded { status }) => {
                    return (String::from_utf8_lossy(&out).into_owned(), Some(status));
                }
                Frame::Control(_) => {}
            }
        }
        (String::from_utf8_lossy(&out).into_owned(), None)
    }

    #[tokio::test]
    async fn hello_sets_term_and_size_and_reports_exit() {
        let (l, mut c) = link();
        let cfg = cfg("echo \"T=$TERM\"; stty size; tty >/dev/null && echo ctty; exit 3", false);
        let task = tokio::spawn(async move { run(&cfg, l).await });
        c.to_agent.send(Frame::Control(ControlMessage::hello(33, 99, "xterm-256color"))).await.unwrap();
        let first = timeout(Duration::from_secs(5), c.from_agent.recv()).await.unwrap().unwrap();
        assert_eq!(first, Frame::Control(ControlMessage::HelloAck { version: PROTOCOL_VERSION, flags: 0 }));
        let (out, status) = collect(&mut c).await;
        assert!(out.contains("T=xterm-256color"), "{out:?}");
        assert!(out.contains("33 99"), "{out:?}");
        assert!(out.contains("ctty"), "{out:?}");
        assert_eq!(status, Some(3));
        task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn input_and_resize_reach_the_pty() {
        let (l, mut c) = link();
        let cfg = cfg("read line; echo \"got=$line\"; stty size", false);
        let task = tokio::spawn(async move { run(&cfg, l).await });
        c.to_agent.send(Frame::Control(ControlMessage::hello(24, 80, "vt100"))).await.unwrap();
        c.to_agent.send(Frame::Control(ControlMessage::Resize { rows: 40, cols: 120 })).await.unwrap();
        c.to_agent.send(Frame::Data(b"hello\r".to_vec())).await.unwrap();
        let (out, status) = collect(&mut c).await;
        assert!(out.contains("got=hello"), "{out:?}");
        assert!(out.contains("40 120"), "{out:?}");
        assert_eq!(status, Some(0));
        task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn open_mode_starts_with_defaults_after_timeout() {
        let (l, mut c) = link();
        let cfg = cfg("echo \"T=$TERM\"; stty size", false);
        let task = tokio::spawn(async move { run(&cfg, l).await });
        let (out, status) = collect(&mut c).await;
        assert!(out.contains("T=vt100"), "{out:?}");
        assert!(out.contains("24 80"), "{out:?}");
        assert_eq!(status, Some(0));
        task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn authenticated_mode_requires_a_write() {
        let (l, _c) = link();
        let cfg = cfg("echo should-not-run", true);
        let err = run(&cfg, l).await.unwrap_err();
        assert!(err.to_string().contains("did not start"), "{err}");
    }

    #[tokio::test]
    async fn first_keystroke_starts_session_and_is_swallowed() {
        let (l, mut c) = link();
        let cfg = cfg("if read -t 1 line; then echo \"got=[$line]\"; else echo timeout; fi", true);
        let task = tokio::spawn(async move { run(&cfg, l).await });
        c.to_agent.send(Frame::Data(b"\r".to_vec())).await.unwrap();
        let (out, status) = collect(&mut c).await;
        // read timed out: the wake-up key was not forwarded
        assert!(out.contains("timeout"), "{out:?}");
        assert_eq!(status, Some(0));
        task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn client_disconnect_hangs_up_session() {
        let (l, c) = link();
        let cfg = cfg("trap 'exit 0' HUP; while :; do sleep 0.1; done", false);
        let task = tokio::spawn(async move { run(&cfg, l).await });
        c.to_agent.send(Frame::Control(ControlMessage::hello(24, 80, "vt100"))).await.unwrap();
        sleep(Duration::from_millis(300)).await;
        drop(c);
        timeout(Duration::from_secs(5), task).await.expect("session did not end").unwrap().unwrap();
    }

    #[tokio::test]
    async fn missing_login_command_reports_error() {
        let (l, mut c) = link();
        let mut cfg = cfg("", false);
        cfg.login_command = vec!["/nonexistent/login".into()];
        let task = tokio::spawn(async move { run(&cfg, l).await });
        c.to_agent.send(Frame::Control(ControlMessage::hello(24, 80, "vt100"))).await.unwrap();
        let f = timeout(Duration::from_secs(5), c.from_agent.recv()).await.unwrap().unwrap();
        assert!(matches!(f, Frame::Control(ControlMessage::Error(_))), "{f:?}");
        assert!(task.await.unwrap().is_err());
    }
}
