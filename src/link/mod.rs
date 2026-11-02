//! A farm's L2 networks between its VMs. msb does not carry traffic from one VM to another,
//! so each guest bridges a TAP device to a port of a learning switch on its host, over TCP to
//! the host's loopback (`host.microsandbox.internal`).
//!
//! - [`switch`] runs on the host as a process of its own, started by the agent ([`host`]).
//!   Ports belong to segments; frames never cross from one segment to another, so each
//!   farm network is its own broadcast domain. Ports, their tokens and impairments come from
//!   a file the agent edits.
//! - [`guest`] runs in the guest (`farm-agent agent link <conf>`) under `farm-link@.service`.
//!
//! A connection starts with `FARMLINK/1 <port> <token>\n`, answered by `OK\n` or
//! `ERR <reason>\n`. Frames follow in both directions, each prefixed by its length as a
//! big-endian u16. Writers queue frames and send what has accumulated in one write, which
//! keeps the per-frame cost low at the 1500-byte MTU.

pub mod guest;
pub mod host;
pub mod switch;

use std::collections::BTreeMap;

use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const HELLO: &str = "FARMLINK/1";
/// Largest frame the length prefix can carry.
pub const MAX_FRAME: usize = u16::MAX as usize;
/// Bytes a writer collects before it sends them in one write.
pub const BATCH: usize = 256 * 1024;
/// Guests reach the host's loopback under this name.
pub const HOST_NAME: &str = "host.microsandbox.internal";
/// Farm networks are virtual, so they use jumbo frames.
pub const DEFAULT_MTU: u16 = 9000;

/// Ports the switch accepts, by name. The agent writes it; the switch follows its changes.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub ports: BTreeMap<String, PortConfig>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PortConfig {
    /// Broadcast domain, such as `<farm>/<network>`.
    pub segment: String,
    /// SHA-256 of the port's token, in hex; the token itself lives only in the guest.
    pub token_sha256: String,
    #[serde(default)]
    pub impair: Impair,
}

/// Faults the switch adds to the frames from and to a port.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct Impair {
    /// Added to frames entering and leaving the port, so a round trip gains twice this.
    #[serde(default)]
    pub delay_ms: u64,
    /// Share of frames dropped in each direction, 0.0 to 1.0; 1.0 isolates the port.
    #[serde(default)]
    pub loss: f64,
}
impl Impair {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            (0.0..=1.0).contains(&self.loss),
            "loss must be between 0 and 1"
        );
        ensure!(self.delay_ms <= 10_000, "delay must be at most 10000 ms");
        Ok(())
    }
}

/// Port and segment names: lowercase segments joined by `/`, as farm ids are.
pub fn validate_name(name: &str) -> Result<()> {
    ensure!(
        !name.is_empty()
            && name.len() <= 160
            && name.split('/').all(|part| {
                !part.is_empty()
                    && part
                        .bytes()
                        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
            }),
        "invalid link name '{name}'"
    );
    Ok(())
}

pub fn token_hash(token: &str) -> String {
    Sha256::digest(token.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

pub fn new_token() -> Result<String> {
    let mut bytes = [0u8; 32];
    getrandom::getrandom(&mut bytes).map_err(|e| anyhow::anyhow!("no randomness: {e}"))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// Append one frame with its length prefix.
pub fn encode(buf: &mut Vec<u8>, frame: &[u8]) {
    debug_assert!(frame.len() <= MAX_FRAME);
    buf.extend_from_slice(&(frame.len() as u16).to_be_bytes());
    buf.extend_from_slice(frame);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_tokens_and_impairments_are_checked() {
        validate_name("dev/ctl/mgmt").unwrap();
        for bad in ["", "Dev", "a//b", "/a", "a/", "a b", "a/../b"] {
            assert!(validate_name(bad).is_err(), "{bad}");
        }
        let token = new_token().unwrap();
        assert_eq!(token.len(), 64);
        assert_ne!(token, new_token().unwrap());
        assert_eq!(token_hash("x"), token_hash("x"));
        assert!(
            Impair {
                loss: 1.5,
                ..Default::default()
            }
            .validate()
            .is_err()
        );
        let mut buf = Vec::new();
        encode(&mut buf, b"abc");
        assert_eq!(buf, [0, 3, b'a', b'b', b'c']);
    }
}
