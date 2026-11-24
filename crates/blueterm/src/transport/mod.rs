//! BLE transports. Each platform module provides a `Backend` with the same
//! inherent API:
//!
//! - `Backend::new(&Options) -> Result<Backend>`
//! - `discover(&self, timeout, stop: impl FnMut(&[Found]) -> bool) -> Result<Vec<Found>>`
//! - `connect(&self, &Found, hello: &ControlMessage) -> Result<Link>`
//!
//! and the Linux backend additionally `pair(&self, &Found)`.

use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use blueterm_proto::Frame;
use tokio::{sync::mpsc, task::JoinHandle};

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
pub use linux::Backend;

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
pub use macos::Backend;

#[derive(Debug, Clone, Default)]
pub struct Options {
    /// Linux: adapter name such as `hci0`. Ignored on macOS.
    pub adapter: Option<String>,
}

/// An agent seen while scanning.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    /// Platform identifier: the Bluetooth address on Linux, CoreBluetooth's
    /// per-Mac peripheral UUID on macOS.
    pub id: String,
    pub name: Option<String>,
    pub rssi: Option<i16>,
}

impl Found {
    pub fn matches(&self, target: &str) -> bool {
        self.id.eq_ignore_ascii_case(target) || self.name.as_deref() == Some(target)
    }

    pub fn display_name(&self) -> &str {
        self.name.as_deref().unwrap_or("(unnamed)")
    }
}

/// An established connection to an agent.
pub struct Link {
    /// Frames for the agent. Dropping it (with `rx`) disconnects.
    pub tx: mpsc::Sender<Frame>,
    /// Frames from the agent; `None` once the connection is gone.
    pub rx: mpsc::Receiver<Frame>,
    /// Completes after the connection has been torn down.
    pub task: JoinHandle<()>,
    /// True if the agent has the blueterm control characteristic (otherwise
    /// it is a plain NUS device: no resize, no exit status).
    pub has_control: bool,
}

/// After the first match, keep scanning this long to notice duplicates.
const DUPLICATE_GRACE: Duration = Duration::from_millis(1500);

/// Scans for `target` (a name or an id) and returns the unique match.
pub async fn resolve(backend: &Backend, target: &str, timeout: Duration) -> Result<Found> {
    let mut first_match: Option<Instant> = None;
    let found = backend
        .discover(timeout, |list| {
            if list.iter().any(|f| f.id.eq_ignore_ascii_case(target)) {
                return true; // ids are unique
            }
            if list.iter().any(|f| f.matches(target)) {
                return first_match.get_or_insert_with(Instant::now).elapsed() >= DUPLICATE_GRACE;
            }
            false
        })
        .await?;
    let mut matches: Vec<Found> = found.into_iter().filter(|f| f.matches(target)).collect();
    if let Some(i) = matches.iter().position(|f| f.id.eq_ignore_ascii_case(target)) {
        return Ok(matches.swap_remove(i));
    }
    match matches.len() {
        0 => bail!("no blueterm agent named {target:?} found within {}s (try `blueterm --list`)", timeout.as_secs()),
        1 => Ok(matches.remove(0)),
        _ => {
            let list: Vec<String> = matches.iter().map(|f| format!("  {}", f.id)).collect();
            bail!("several agents are named {target:?}; connect by id instead:\n{}", list.join("\n"))
        }
    }
}

/// Splits outgoing data into ATT-sized chunks.
pub(crate) fn chunks(data: &[u8], payload: usize) -> impl Iterator<Item = &[u8]> {
    data.chunks(payload.max(blueterm_proto::MIN_PAYLOAD))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matching() {
        let f = Found { id: "AA:BB:CC:DD:EE:FF".into(), name: Some("pi".into()), rssi: None };
        assert!(f.matches("pi"));
        assert!(f.matches("aa:bb:cc:dd:ee:ff"));
        assert!(!f.matches("Pi"));
        assert!(!f.matches("pi2"));
    }

    #[test]
    fn chunking_respects_minimum() {
        let data = [0u8; 50];
        assert_eq!(chunks(&data, 0).map(<[u8]>::len).collect::<Vec<_>>(), [20, 20, 10]);
        assert_eq!(chunks(&data, 182).count(), 1);
    }
}
