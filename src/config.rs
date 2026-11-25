//! `~/.monitor-switch/config`: plain `key = value` lines, `#` starts a comment.
//!
//! ```text
//! role = macbook                  # or: mini
//! peer = other-mac.local         # the other Mac's host name (optionally :port)
//! devices = AA:BB:..., AA:BB:...  # Bluetooth addresses to hand over, keyboard first
//! hdmi_input = 18                 # mini only: VCP 0x60 value of its monitor input
//! ```

use std::path::PathBuf;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Role {
    /// On DisplayPort.
    MacBook,
    /// On HDMI; also moves the monitor off its input when handing over.
    Mini,
}

#[derive(Debug)]
pub struct Config {
    pub role: Role,
    pub peer: String,
    pub devices: Vec<String>,
    pub hdmi_input: u16,
}

/// This user's state directory. Without `HOME` the home directory comes
/// from the user database, never from the current directory.
pub fn dir() -> PathBuf {
    home().join(".monitor-switch")
}

pub use crate::home::{home, uid};

pub fn socket_path() -> PathBuf {
    dir().join("agent.sock")
}

impl Config {
    pub fn load() -> Result<Config, String> {
        let path = dir().join("config");
        let text = std::fs::read_to_string(&path)
            .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
        Config::parse(&text).map_err(|error| format!("{}: {error}", path.display()))
    }

    pub fn parse(text: &str) -> Result<Config, String> {
        let mut role = None;
        let mut peer = None;
        let mut devices = Vec::new();
        let mut hdmi_input = 18;
        for line in text.lines() {
            let line = line.split('#').next().unwrap_or("").trim();
            if line.is_empty() {
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                return Err(format!("expected key = value, got {line:?}"));
            };
            let value = value.trim();
            match key.trim() {
                "role" => {
                    role = Some(match value {
                        "macbook" => Role::MacBook,
                        "mini" => Role::Mini,
                        other => return Err(format!("unknown role {other:?}")),
                    })
                }
                "peer" => peer = Some(valid_host(value)?),
                // Older configs: an ssh destination; only the host is used.
                "peer_ssh" => peer = Some(valid_host(value.rsplit('@').next().unwrap_or(value))?),
                "devices" => {
                    devices = value
                        .split(',')
                        .map(str::trim)
                        .filter(|address| !address.is_empty())
                        .map(|address| {
                            crate::bt::normalize(address)
                                .ok_or(format!("bad Bluetooth address {address:?}"))
                        })
                        .collect::<Result<_, _>>()?
                }
                "hdmi_input" => {
                    hdmi_input = value.parse().map_err(|_| format!("bad hdmi_input {value:?}"))?
                }
                other => return Err(format!("unknown key {other:?}")),
            }
        }
        let config = Config {
            role: role.ok_or("role is missing")?,
            peer: peer.ok_or("peer is missing")?,
            devices,
            hdmi_input,
        };
        if config.devices.is_empty() {
            return Err("devices is empty".into());
        }
        Ok(config)
    }
}

/// A host name or address, optionally with `:port`.
fn valid_host(value: &str) -> Result<String, String> {
    let fine = |c: char| c.is_ascii_alphanumeric() || ".-:%[]".contains(c);
    if value.is_empty() || value.starts_with('-') || !value.chars().all(fine) {
        return Err(format!("bad peer {value:?}"));
    }
    Ok(value.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOOD: &str = "role = mini   # comment\n\npeer = mac.local\n\
                        devices = AA:BB:CC:DD:EE:01, aa-bb-cc-dd-ee-02\nhdmi_input = 17\n";

    #[test]
    fn parses_a_full_config() {
        let config = Config::parse(GOOD).unwrap();
        assert_eq!(config.role, Role::Mini);
        assert_eq!(config.peer, "mac.local");
        assert_eq!(Config::parse("role = mini\npeer_ssh = me@b.local\ndevices = 3CA6F6F1D872").unwrap().peer, "b.local");
        assert_eq!(config.devices, ["aa-bb-cc-dd-ee-01", "aa-bb-cc-dd-ee-02"]);
        assert_eq!(config.hdmi_input, 17);
    }

    #[test]
    fn hdmi_input_defaults_to_18() {
        let config = Config::parse("role = macbook\npeer_ssh = a@b\ndevices = 3CA6F6F1D872").unwrap();
        assert_eq!(config.hdmi_input, 18);
    }

    #[test]
    fn rejects_bad_input() {
        for text in [
            "role = macbook\ndevices = 3CA6F6F1D872",
            "role = laptop\npeer_ssh = a@b\ndevices = 3CA6F6F1D872",
            "role = mini\npeer = -oProxyCommand=x\ndevices = 3CA6F6F1D872",
            "role = mini\npeer = a b\ndevices = 3CA6F6F1D872",
            "role = mini\npeer_ssh = a@b\ndevices = 12:34",
            "role = mini\npeer_ssh = a@b\ndevices =",
            "role = mini\npeer_ssh = a@b\ndevices = 3CA6F6F1D872\nspeed = 2",
            "role = mini\njunk line",
        ] {
            assert!(Config::parse(text).is_err(), "accepted {text:?}");
        }
    }
}
