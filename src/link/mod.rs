//! A farm's L2 networks between its VMs. msb does not carry traffic from one VM to another,
//! so each guest bridges a TAP device to a port of a learning switch on its host, over a
//! WebSocket to the host's loopback (`host.microsandbox.internal`).
//!
//! - [`switch`] runs on the host as a process of its own, started by the agent ([`host`]).
//!   Ports belong to segments; frames never cross from one segment to another, so each
//!   farm network is its own broadcast domain. Ports, their tokens and impairments come from
//!   a file the agent edits.
//! - [`guest`] runs in the guest (`farm-agent agent link <conf>`) under `farm-link@.service`.
//!
//! A guest opens `GET /link?port=<port>` with `Authorization: Bearer <token>`; the switch
//! answers 401 to an unknown port or a wrong token. WebSocket keeps the link able to cross
//! HTTP proxies, TLS and load balancers when it leaves the host. Each binary message carries
//! one or more Ethernet frames, each prefixed by its length as a big-endian u16: writers send
//! every frame queued so far in one message, which keeps the per-frame cost low at the
//! 1500-byte MTU.
//!
//! A farm whose VMs sit on several hosts joins their switches through the coordinator
//! ([`relay`]): each switch keeps an uplink per such farm open to `GET /api/v1/link-relay`,
//! and the coordinator passes every message to the farm's other hosts. An uplink carries
//! several segments, so each of its messages starts with the segment's name (see
//! [`encode_trunk`]); the switches learn the MACs beyond it as they do a port's. Uplinks
//! always use TLS, with the coordinator's own CA ([`tls`]).

pub mod cloudflared;
pub mod guest;
pub mod host;
pub mod relay;
pub mod seal;
pub mod switch;
pub mod tls;

use std::collections::BTreeMap;

use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Path of the switch's WebSocket endpoint.
pub const PATH: &str = "/link";
/// Path of the coordinator's relay between hosts' switches.
pub const RELAY_PATH: &str = "/api/v1/link-relay";
/// Largest frame the length prefix can carry.
pub const MAX_FRAME: usize = u16::MAX as usize;
/// Bytes a writer collects before it sends them in one message.
pub const BATCH: usize = 256 * 1024;
/// A message holds a batch plus the frame that pushed it over [`BATCH`].
pub const MAX_MESSAGE: usize = BATCH + MAX_FRAME + 2;
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
    /// Uplinks to the coordinator's relay, by farm id.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub uplinks: BTreeMap<String, UplinkConfig>,
}

/// A switch's connection to the relay for one farm that spans hosts.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UplinkConfig {
    /// `wss://<coordinator>/api/v1/link-relay?farm=<id>&host=<host>`.
    pub url: String,
    /// The coordinator's relay CA in PEM, which the uplink alone trusts; empty for a relay
    /// behind a TLS proxy, whose certificate comes from a public CA.
    #[serde(default)]
    pub ca: String,
    /// The relay checks it against the hash it keeps; the switch needs it in full.
    pub token: String,
    /// Seals the uplink's messages end to end with the relay, in hex ([`seal`]).
    pub key: String,
    /// The farm's segments this host carries over the uplink.
    pub segments: Vec<String>,
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

/// Start a trunk message for `segment`: its name's length as one byte, then the name.
/// Frames follow as [`encode`] writes them.
pub fn encode_trunk(buf: &mut Vec<u8>, segment: &str) {
    debug_assert!(segment.len() <= 160);
    buf.push(segment.len() as u8);
    buf.extend_from_slice(segment.as_bytes());
}

/// The segment of a trunk message and its frames.
pub fn decode_trunk(message: &bytes::Bytes) -> Result<(&str, Vec<bytes::Bytes>)> {
    ensure!(!message.is_empty(), "empty trunk message");
    let len = usize::from(message[0]);
    ensure!(message.len() > len, "truncated segment name");
    let segment = std::str::from_utf8(&message[1..1 + len])?;
    validate_name(segment)?;
    Ok((segment, decode(&message.slice(1 + len..))?))
}

/// Append one frame with its length prefix.
pub fn encode(buf: &mut Vec<u8>, frame: &[u8]) {
    debug_assert!(frame.len() <= MAX_FRAME);
    buf.extend_from_slice(&(frame.len() as u16).to_be_bytes());
    buf.extend_from_slice(frame);
}

/// The frames of one message, sharing its buffer.
pub fn decode(message: &bytes::Bytes) -> Result<Vec<bytes::Bytes>> {
    let mut frames = Vec::new();
    let mut at = 0;
    while at < message.len() {
        ensure!(message.len() - at >= 2, "truncated frame length");
        let len = usize::from(u16::from_be_bytes([message[at], message[at + 1]]));
        at += 2;
        ensure!(message.len() - at >= len, "truncated frame");
        frames.push(message.slice(at..at + len));
        at += len;
    }
    Ok(frames)
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
        encode(&mut buf, b"");
        encode(&mut buf, b"de");
        let message = bytes::Bytes::from(buf);
        assert_eq!(decode(&message).unwrap(), [&b"abc"[..], b"", b"de"]);
        assert!(decode(&message.slice(..message.len() - 1)).is_err());
        assert!(decode(&bytes::Bytes::from_static(&[0])).is_err());
        let mut trunk = Vec::new();
        encode_trunk(&mut trunk, "f/mgmt");
        encode(&mut trunk, b"abc");
        let trunk = bytes::Bytes::from(trunk);
        let (segment, frames) = decode_trunk(&trunk).unwrap();
        assert_eq!(
            (segment, frames),
            ("f/mgmt", vec![bytes::Bytes::from_static(b"abc")])
        );
        assert!(decode_trunk(&trunk.slice(..3)).is_err());
        assert!(decode_trunk(&bytes::Bytes::from_static(b"\x02F/")).is_err());
        assert!(decode_trunk(&bytes::Bytes::new()).is_err());
    }
}
