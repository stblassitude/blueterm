//! Relays between the local terminal and a [`Link`], independent of the BLE
//! backend.

use anyhow::Result;
use blueterm_proto::{ControlMessage, Frame};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    sync::mpsc,
};

use crate::escape::{Action, Escaper, help_text};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// The remote login session ended with this status.
    Ended(i32),
    /// The user typed the disconnect escape.
    UserDisconnect,
    /// The connection dropped.
    Lost,
}

pub struct Io<R, W> {
    pub input: R,
    pub output: W,
    pub resize: mpsc::Receiver<(u16, u16)>,
    /// Called for `~^Z`; returns once the process has been resumed.
    pub suspend: Box<dyn FnMut() -> std::io::Result<()> + Send>,
}

pub async fn run<R, W>(
    tx: &mpsc::Sender<Frame>,
    rx: &mut mpsc::Receiver<Frame>,
    io: Io<R, W>,
    escape: Option<u8>,
) -> Result<Outcome>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let Io { mut input, mut output, mut resize, mut suspend } = io;
    let mut esc = Escaper::new(escape);
    let mut buf = vec![0u8; 1024];
    let mut input_open = true;
    let mut resize_open = true;
    loop {
        tokio::select! {
            r = input.read(&mut buf), if input_open => {
                let n = r?;
                if n == 0 {
                    // There is no way to send EOF over a serial line; keep
                    // showing output until the remote side is done.
                    input_open = false;
                    continue;
                }
                for action in esc.feed(&buf[..n]) {
                    match action {
                        Action::Send(d) => {
                            if tx.send(Frame::Data(d)).await.is_err() {
                                return Ok(Outcome::Lost);
                            }
                        }
                        Action::Disconnect => return Ok(Outcome::UserDisconnect),
                        Action::Help => {
                            output.write_all(help_text(escape.unwrap_or(b'~')).as_bytes()).await?;
                            output.flush().await?;
                        }
                        Action::Suspend => {
                            output.write_all(b"\r\n[suspend blueterm]\r\n").await?;
                            output.flush().await?;
                            suspend()?;
                        }
                    }
                }
            }
            f = rx.recv() => match f {
                None => return Ok(Outcome::Lost),
                Some(Frame::Data(d)) => {
                    output.write_all(&d).await?;
                    output.flush().await?;
                }
                Some(Frame::Control(ControlMessage::SessionEnded { status })) => return Ok(Outcome::Ended(status)),
                Some(Frame::Control(ControlMessage::Error(msg))) => {
                    output.write_all(format!("\r\n[blueterm-agent: {msg}]\r\n").as_bytes()).await?;
                    output.flush().await?;
                }
                Some(Frame::Control(m)) => tracing::debug!(?m, "control message"),
            },
            r = resize.recv(), if resize_open => match r {
                None => resize_open = false,
                Some((rows, cols)) => {
                    if tx.send(Frame::Control(ControlMessage::Resize { rows, cols })).await.is_err() {
                        return Ok(Outcome::Lost);
                    }
                }
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::{io::duplex, time::timeout};

    struct Harness {
        keyboard: tokio::io::DuplexStream,
        screen: tokio::io::DuplexStream,
        resize: mpsc::Sender<(u16, u16)>,
        from_client: mpsc::Receiver<Frame>,
        to_client: mpsc::Sender<Frame>,
        task: tokio::task::JoinHandle<Result<Outcome>>,
        suspended: std::sync::mpsc::Receiver<()>,
    }

    fn start() -> Harness {
        let (keyboard, input) = duplex(1024);
        let (output, screen) = duplex(1024);
        let (resize, resize_rx) = mpsc::channel(4);
        let (tx, from_client) = mpsc::channel(16);
        let (to_client, mut rx) = mpsc::channel(16);
        let (suspended_tx, suspended) = std::sync::mpsc::channel();
        let suspend = Box::new(move || {
            let _ = suspended_tx.send(());
            Ok(())
        });
        let task = tokio::spawn(async move {
            run(&tx, &mut rx, Io { input, output, resize: resize_rx, suspend }, Some(b'~')).await
        });
        Harness { keyboard, screen, resize, from_client, to_client, task, suspended }
    }

    async fn outcome(h: Harness) -> Outcome {
        timeout(Duration::from_secs(5), h.task).await.unwrap().unwrap().unwrap()
    }

    #[tokio::test]
    async fn relays_both_ways_and_reports_status() {
        let mut h = start();
        h.keyboard.write_all(b"ls\r").await.unwrap();
        assert_eq!(h.from_client.recv().await, Some(Frame::Data(b"ls\r".to_vec())));
        h.to_client.send(Frame::Data(b"file\r\n".to_vec())).await.unwrap();
        let mut got = [0u8; 6];
        h.screen.read_exact(&mut got).await.unwrap();
        assert_eq!(&got, b"file\r\n");
        h.to_client.send(Frame::Control(ControlMessage::SessionEnded { status: 7 })).await.unwrap();
        assert_eq!(outcome(h).await, Outcome::Ended(7));
    }

    #[tokio::test]
    async fn resize_is_forwarded() {
        let mut h = start();
        h.resize.send((50, 100)).await.unwrap();
        assert_eq!(h.from_client.recv().await, Some(Frame::Control(ControlMessage::Resize { rows: 50, cols: 100 })));
        drop(h.to_client);
        h.to_client = mpsc::channel(1).0;
        assert_eq!(outcome(h).await, Outcome::Lost);
    }

    #[tokio::test]
    async fn escape_disconnects() {
        let mut h = start();
        h.keyboard.write_all(b"\r~.").await.unwrap();
        assert_eq!(h.from_client.recv().await, Some(Frame::Data(b"\r".to_vec())));
        assert_eq!(outcome(h).await, Outcome::UserDisconnect);
    }

    #[tokio::test]
    async fn escape_suspends() {
        let mut h = start();
        h.keyboard.write_all(b"~\x1aok").await.unwrap();
        assert_eq!(h.from_client.recv().await, Some(Frame::Data(b"ok".to_vec())));
        h.suspended.try_recv().expect("suspend was called");
        let mut got = [0u8; 22];
        h.screen.read_exact(&mut got).await.unwrap();
        assert_eq!(&got, b"\r\n[suspend blueterm]\r\n");
        h.to_client.send(Frame::Control(ControlMessage::SessionEnded { status: 0 })).await.unwrap();
        assert_eq!(outcome(h).await, Outcome::Ended(0));
    }

    #[tokio::test]
    async fn stdin_eof_keeps_output_flowing() {
        let mut h = start();
        drop(h.keyboard);
        h.keyboard = duplex(1).0;
        h.to_client.send(Frame::Data(b"still here".to_vec())).await.unwrap();
        let mut got = [0u8; 10];
        h.screen.read_exact(&mut got).await.unwrap();
        assert_eq!(&got, b"still here");
        h.to_client.send(Frame::Control(ControlMessage::SessionEnded { status: 0 })).await.unwrap();
        assert_eq!(outcome(h).await, Outcome::Ended(0));
    }
}
