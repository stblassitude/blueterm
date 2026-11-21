//! Agent configuration (`/etc/blueterm/agent.toml`).

use std::{path::Path, time::Duration};

use anyhow::{Context, Result};
use serde::Deserialize;

pub const DEFAULT_PATH: &str = "/etc/blueterm/agent.toml";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Security {
    /// Characteristics require an encrypted, MITM-protected (bonded) link.
    Authenticated,
    /// No link security at all. For bench testing only.
    Open,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Advertised name; defaults to the hostname.
    pub name: Option<String>,
    /// Adapter to use, e.g. `hci0`; defaults to BlueZ's default adapter.
    pub adapter: Option<String>,
    pub security: Security,
    pub max_sessions: usize,
    pub login_command: Vec<String>,
    /// How long to wait for a `Hello` after the client subscribed (open mode).
    pub hello_timeout_ms: u64,
    /// How long a connected client may take to start a session before it is
    /// disconnected.
    pub start_timeout_s: u64,
    /// Environment passed to the login command (besides `TERM`).
    pub env: Vec<String>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            name: None,
            adapter: None,
            security: Security::Authenticated,
            max_sessions: 1,
            login_command: vec!["/bin/login".into()],
            hello_timeout_ms: 1000,
            start_timeout_s: 60,
            env: vec!["PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin".into()],
        }
    }
}

impl Config {
    /// Loads the config file. A missing file at the default path yields the
    /// defaults; a missing file at an explicitly given path is an error.
    pub fn load(path: Option<&Path>) -> Result<Config> {
        let (path, explicit) = match path {
            Some(p) => (p, true),
            None => (Path::new(DEFAULT_PATH), false),
        };
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound && !explicit => return Ok(Config::default()),
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        let cfg: Config = toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        cfg.validate().with_context(|| format!("in {}", path.display()))?;
        Ok(cfg)
    }

    fn validate(&self) -> Result<()> {
        anyhow::ensure!(!self.login_command.is_empty(), "login_command must not be empty");
        anyhow::ensure!(self.max_sessions >= 1, "max_sessions must be at least 1");
        for e in &self.env {
            anyhow::ensure!(e.contains('='), "env entry {e:?} must have the form NAME=value");
        }
        Ok(())
    }

    pub fn hello_timeout(&self) -> Duration {
        Duration::from_millis(self.hello_timeout_ms)
    }

    pub fn start_timeout(&self) -> Duration {
        Duration::from_secs(self.start_timeout_s)
    }

    /// The advertised name: configured, else the hostname, truncated so it
    /// fits a scan response.
    pub fn advertised_name(&self) -> String {
        let name = match &self.name {
            Some(n) => n.clone(),
            None => rustix::system::uname().nodename().to_string_lossy().into_owned(),
        };
        truncate_utf8(&name, 29).to_owned()
    }
}

fn truncate_utf8(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_parse_from_shipped_file() {
        let cfg: Config = toml::from_str(include_str!("../../../packaging/linux/agent.toml")).unwrap();
        cfg.validate().unwrap();
        assert_eq!(cfg.security, Security::Authenticated);
        assert_eq!(cfg.login_command, ["/bin/login"]);
    }

    #[test]
    fn overrides() {
        let cfg: Config = toml::from_str("security = \"open\"\nname = \"pi\"\nmax_sessions = 2\n").unwrap();
        assert_eq!(cfg.security, Security::Open);
        assert_eq!(cfg.advertised_name(), "pi");
        assert_eq!(cfg.max_sessions, 2);
    }

    #[test]
    fn rejects_unknown_keys() {
        assert!(toml::from_str::<Config>("secruity = \"open\"\n").is_err());
    }

    #[test]
    fn truncates_on_char_boundary() {
        assert_eq!(truncate_utf8("ääää", 3), "ä");
        assert_eq!(truncate_utf8("abc", 3), "abc");
    }
}
