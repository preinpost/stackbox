//! End-to-end encryption of an uplink, between a host's switch and the coordinator's relay,
//! whatever carries it: the relay's own TLS, or a proxy or Cloudflare tunnel that ends TLS
//! before it and so sees the WebSocket messages.
//!
//! Each (farm, host) has a 32-byte key, made by the coordinator and handed to the host's
//! agent with the farm. On connecting, each side first sends a random 16-byte hello in the
//! clear. Every later message is `counter (8, big-endian) | nonce (24) | XChaCha20-Poly1305
//! ciphertext and tag`, authenticated with the direction, the receiver's hello and the
//! counter. The receiver takes counters 0, 1, 2, ... only, so a message from another connection,
//! or one replayed, reordered or dropped within this one, ends the connection.

use anyhow::{Context, Result, ensure};
use chacha20poly1305::{
    KeyInit, XChaCha20Poly1305, XNonce,
    aead::{Aead, Payload},
};

pub const HELLO: usize = 16;
/// Bytes a sealed message adds.
pub const OVERHEAD: usize = 8 + 24 + 16;

/// Which way a message goes, so one direction's messages never pass for the other's.
#[derive(Clone, Copy)]
pub enum Direction {
    ToRelay = 0,
    ToSwitch = 1,
}

pub fn new_key() -> Result<String> {
    super::new_token()
}

pub fn hello() -> Result<[u8; HELLO]> {
    let mut hello = [0u8; HELLO];
    getrandom::getrandom(&mut hello).map_err(|e| anyhow::anyhow!("no randomness: {e}"))?;
    Ok(hello)
}

fn cipher(key: &str) -> Result<XChaCha20Poly1305> {
    ensure!(
        key.len() == 64 && key.bytes().all(|b| b.is_ascii_hexdigit()),
        "invalid uplink key"
    );
    let bytes: Vec<u8> = (0..32)
        .map(|i| u8::from_str_radix(&key[2 * i..2 * i + 2], 16))
        .collect::<Result<_, _>>()?;
    Ok(XChaCha20Poly1305::new_from_slice(&bytes).expect("32-byte key"))
}

fn aad(direction: Direction, hello: &[u8; HELLO], counter: u64) -> [u8; 1 + HELLO + 8] {
    let mut aad = [0u8; 1 + HELLO + 8];
    aad[0] = direction as u8;
    aad[1..1 + HELLO].copy_from_slice(hello);
    aad[1 + HELLO..].copy_from_slice(&counter.to_be_bytes());
    aad
}

/// Seals the messages one side sends, for the peer whose hello it received.
pub struct Sealer {
    cipher: XChaCha20Poly1305,
    direction: Direction,
    peer: [u8; HELLO],
    counter: u64,
}

impl Sealer {
    pub fn new(key: &str, direction: Direction, peer: [u8; HELLO]) -> Result<Self> {
        Ok(Self {
            cipher: cipher(key)?,
            direction,
            peer,
            counter: 0,
        })
    }
    pub fn seal(&mut self, plain: &[u8]) -> Result<Vec<u8>> {
        let mut nonce = [0u8; 24];
        getrandom::getrandom(&mut nonce).map_err(|e| anyhow::anyhow!("no randomness: {e}"))?;
        let aad = aad(self.direction, &self.peer, self.counter);
        let sealed = self
            .cipher
            .encrypt(
                &XNonce::from(nonce),
                Payload {
                    msg: plain,
                    aad: &aad,
                },
            )
            .map_err(|_| anyhow::anyhow!("cannot seal an uplink message"))?;
        let mut message = Vec::with_capacity(OVERHEAD + plain.len());
        message.extend_from_slice(&self.counter.to_be_bytes());
        message.extend_from_slice(&nonce);
        message.extend_from_slice(&sealed);
        self.counter += 1;
        Ok(message)
    }
}

/// Opens the messages one side receives, after sending it `own` hello.
pub struct Opener {
    cipher: XChaCha20Poly1305,
    direction: Direction,
    own: [u8; HELLO],
    next: u64,
}

impl Opener {
    pub fn new(key: &str, direction: Direction, own: [u8; HELLO]) -> Result<Self> {
        Ok(Self {
            cipher: cipher(key)?,
            direction,
            own,
            next: 0,
        })
    }
    pub fn open(&mut self, message: &[u8]) -> Result<Vec<u8>> {
        ensure!(message.len() >= OVERHEAD, "truncated uplink message");
        let counter = u64::from_be_bytes(message[..8].try_into().expect("8 bytes"));
        ensure!(
            counter == self.next,
            "uplink message out of order or replayed"
        );
        let aad = aad(self.direction, &self.own, counter);
        let plain = self
            .cipher
            .decrypt(
                &XNonce::from(<[u8; 24]>::try_from(&message[8..32]).expect("24 bytes")),
                Payload {
                    msg: &message[32..],
                    aad: &aad,
                },
            )
            .ok()
            .context("uplink message fails authentication")?;
        self.next += 1;
        Ok(plain)
    }
}

/// The peer's hello, the first message on an uplink.
pub fn read_hello(message: &[u8]) -> Result<[u8; HELLO]> {
    message
        .try_into()
        .ok()
        .context("the uplink did not start with a hello")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_open_once_in_order_with_their_key_and_session() {
        let key = new_key().unwrap();
        let (relay_hello, other_session) = (hello().unwrap(), hello().unwrap());
        let mut sealer = Sealer::new(&key, Direction::ToRelay, relay_hello).unwrap();
        let mut opener = Opener::new(&key, Direction::ToRelay, relay_hello).unwrap();
        let first = sealer.seal(b"one").unwrap();
        let second = sealer.seal(b"two").unwrap();
        assert_eq!(first.len(), OVERHEAD + 3);
        assert!(!first.windows(3).any(|w| w == b"one"));
        // Out of order, then in order; a replay fails.
        assert!(opener.open(&second).is_err());
        assert_eq!(opener.open(&first).unwrap(), b"one");
        assert!(opener.open(&first).is_err());
        assert_eq!(opener.open(&second).unwrap(), b"two");
        // Another key, another session, the other direction or a flipped bit fail.
        let fresh = || Sealer::new(&key, Direction::ToRelay, relay_hello).unwrap();
        let message = fresh().seal(b"x").unwrap();
        let mut tampered = message.clone();
        *tampered.last_mut().unwrap() ^= 1;
        for (key, direction, own, message) in [
            (
                new_key().unwrap(),
                Direction::ToRelay,
                relay_hello,
                &message,
            ),
            (key.clone(), Direction::ToRelay, other_session, &message),
            (key.clone(), Direction::ToSwitch, relay_hello, &message),
            (key.clone(), Direction::ToRelay, relay_hello, &tampered),
        ] {
            assert!(
                Opener::new(&key, direction, own)
                    .unwrap()
                    .open(message)
                    .is_err()
            );
        }
        assert!(Sealer::new("short", Direction::ToRelay, relay_hello).is_err());
    }
}
