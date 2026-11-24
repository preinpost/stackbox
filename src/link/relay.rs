//! The coordinator's relay between the switches of a stackbox that spans hosts. Each host's switch
//! keeps an uplink open per such stackbox (`GET /api/v1/link-relay?stackbox=<id>&host=<host>` with the
//! uplink's token); the relay passes every trunk message to the stackbox's other hosts, whose
//! switches deliver its frames. Messages are sealed end to end with each host's own key
//! ([`super::seal`]); the relay opens them, routes by segment and seals them anew for each
//! receiving host, so a TLS proxy in front of it sees only ciphertext. It learns nothing itself: with a stackbox on two hosts, each
//! message has one place to go.

use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use anyhow::{Result, anyhow, ensure};
use axum::{
    Router,
    extract::{
        Query, State,
        ws::{Message, WebSocket, WebSocketUpgrade},
    },
    http::{HeaderMap, StatusCode, header::AUTHORIZATION},
    response::{IntoResponse, Response},
    routing::get,
};
use bytes::Bytes;
use futures::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

use super::{MAX_MESSAGE, decode_trunk, token_hash};

/// Messages queued for a host before the relay drops, as the switches do.
const QUEUE: usize = 1024;
/// Proxies such as Cloudflare close a WebSocket that stays quiet for about 100 s.
pub const PING_EVERY: Duration = Duration::from_secs(30);

/// Who terminates the relay's TLS.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum RelayTlsMode {
    /// The relay itself, with this coordinator's private CA; switches trust that CA alone.
    Private,
    /// A proxy in front of it, such as cloudflared run by hand, with a public certificate.
    /// The relay listens without TLS, on loopback by default, and switches trust public CAs.
    Proxy,
    /// A Cloudflare quick tunnel the coordinator runs: no account, a new
    /// `*.trycloudflare.com` address each start, which the coordinator hands the switches.
    QuickTunnel,
}
impl RelayTlsMode {
    /// Whether the relay listens without TLS, behind something that has it.
    pub fn proxied(self) -> bool {
        self != Self::Private
    }
    pub fn tunnel(self) -> bool {
        self == Self::QuickTunnel
    }
}

/// Where stackboxes that span hosts join, set from the portal or `PUT /api/v1/relay`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct RelaySettings {
    /// The relay as other hosts reach it: `https://<host>[:port]`. A quick tunnel's comes
    /// from Cloudflare, so it is left empty.
    #[serde(default)]
    pub url: String,
    pub mode: RelayTlsMode,
    /// Where it listens; `0.0.0.0:10011` with `private`, `127.0.0.1:10012` with `proxy`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<String>)]
    pub bind: Option<SocketAddr>,
}
impl RelaySettings {
    pub fn bind(&self) -> SocketAddr {
        self.bind.unwrap_or_else(|| match self.mode {
            RelayTlsMode::Private => SocketAddr::from(([0, 0, 0, 0], 10011)),
            _ => SocketAddr::from(([127, 0, 0, 1], 10012)),
        })
    }
    pub fn validate(&self) -> Result<()> {
        if self.mode == RelayTlsMode::QuickTunnel {
            ensure!(
                self.url.is_empty(),
                "a quick tunnel's URL comes from Cloudflare; leave it empty"
            );
            return Ok(());
        }
        uplink_url(&self.url, "check", "check")?;
        super::tls::url_host(&self.url)?;
        Ok(())
    }
}

/// The relay's listener: TLS with `tls`, or plain behind a proxy.
pub async fn listen(
    relay: Arc<Relay>,
    bind: SocketAddr,
    tls: Option<Arc<rustls::ServerConfig>>,
) -> Result<(SocketAddr, tokio::task::JoinHandle<()>)> {
    let listener = tokio::net::TcpListener::bind(bind)
        .await
        .map_err(|e| anyhow!("bind the relay on {bind}: {e}"))?;
    let addr = listener.local_addr()?;
    let app = router(relay);
    let task = match tls {
        Some(config) => {
            let listener = super::tls::TlsListener::new(listener, config)?;
            tokio::spawn(async move {
                if let Err(error) = axum::serve(listener, app).await {
                    eprintln!("relay stopped: {error}");
                }
            })
        }
        None => tokio::spawn(async move {
            use axum::serve::ListenerExt;
            let listener = listener.tap_io(|tcp| {
                let _ = tcp.set_nodelay(true);
            });
            if let Err(error) = axum::serve(listener, app).await {
                eprintln!("relay stopped: {error}");
            }
        }),
    };
    Ok((addr, task))
}

/// The relay's one route.
pub fn router(relay: Arc<Relay>) -> Router {
    Router::new()
        .route(super::RELAY_PATH, get(upgrade))
        .fallback(|| async { (StatusCode::NOT_FOUND, "route not found\n") })
        .with_state(relay)
}

#[derive(Deserialize)]
struct UplinkQuery {
    stackbox: String,
    host: String,
}

/// A host's switch joins a stackbox that spans hosts here, with that stackbox's uplink token.
async fn upgrade(
    State(relay): State<Arc<Relay>>,
    Query(query): Query<UplinkQuery>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    let token = headers
        .get(AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or_default();
    let Ok(accepted) = relay.accept(&query.stackbox, &query.host, token) else {
        return (StatusCode::UNAUTHORIZED, "uplink token required\n").into_response();
    };
    let limit = super::MAX_MESSAGE + 256;
    ws.max_message_size(limit)
        .max_frame_size(limit)
        .on_upgrade(move |socket| relay.serve(accepted, socket))
}

#[derive(Default)]
struct Stackbox {
    /// Token hash and sealing key of each host's uplink.
    tokens: HashMap<String, (String, String)>,
    /// Connected uplinks by host.
    uplinks: HashMap<String, (u64, mpsc::Sender<Bytes>)>,
}

#[derive(Default)]
pub struct Relay {
    stackboxes: Mutex<HashMap<String, Stackbox>>,
    next: AtomicU64,
}

/// An accepted uplink, from [`Relay::accept`] until [`Relay::serve`] ends.
pub struct Accepted {
    stackbox: String,
    host: String,
    id: u64,
    key: String,
    rx: mpsc::Receiver<Bytes>,
}

impl Relay {
    /// Let `host` join `stackbox`'s segments with the token hashed as `token_sha256`, its
    /// messages sealed with `key`, replacing its previous token and key.
    pub fn authorize(&self, stackbox: &str, host: &str, token_sha256: &str, key: &str) {
        let mut stackboxes = self.stackboxes.lock().unwrap();
        let entry = stackboxes.entry(stackbox.to_owned()).or_default();
        let new = (token_sha256.to_owned(), key.to_owned());
        if entry.tokens.insert(host.to_owned(), new.clone()) != Some(new) {
            // An uplink authenticated with the old token ends.
            entry.uplinks.remove(host);
        }
    }
    /// Close the stackbox's uplinks and refuse new ones.
    pub fn forget(&self, stackbox: &str) {
        self.stackboxes.lock().unwrap().remove(stackbox);
    }
    /// Hosts whose uplink for `stackbox` is connected.
    pub fn connected(&self, stackbox: &str) -> Vec<String> {
        let stackboxes = self.stackboxes.lock().unwrap();
        let mut hosts: Vec<String> = stackboxes
            .get(stackbox)
            .map(|f| f.uplinks.keys().cloned().collect())
            .unwrap_or_default();
        hosts.sort();
        hosts
    }

    /// Check an uplink's token; a host that reconnects replaces its older uplink.
    pub fn accept(&self, stackbox: &str, host: &str, token: &str) -> Result<Accepted> {
        let mut stackboxes = self.stackboxes.lock().unwrap();
        let entry = stackboxes
            .get_mut(stackbox)
            .ok_or_else(|| anyhow!("no relay for stackbox {stackbox}"))?;
        let (expected, key) = entry
            .tokens
            .get(host)
            .cloned()
            .ok_or_else(|| anyhow!("host {host} has no uplink for stackbox {stackbox}"))?;
        use subtle::ConstantTimeEq;
        ensure!(
            bool::from(expected.as_bytes().ct_eq(token_hash(token).as_bytes())),
            "wrong uplink token"
        );
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = mpsc::channel(QUEUE);
        entry.uplinks.insert(host.to_owned(), (id, tx));
        Ok(Accepted {
            stackbox: stackbox.to_owned(),
            host: host.to_owned(),
            id,
            key,
            rx,
        })
    }

    /// Pass the uplink's messages to the stackbox's other hosts and theirs to it, until either
    /// side closes or the uplink is replaced or forgotten.
    pub async fn serve(self: Arc<Self>, accepted: Accepted, socket: WebSocket) {
        let Accepted {
            stackbox,
            host,
            id,
            key,
            mut rx,
        } = accepted;
        let (mut sink, mut stream) = socket.split();
        let prefix = format!("{stackbox}/");
        // Each side's hello first; every later message is sealed against the other's.
        let session = async {
            use super::seal::{Direction, Opener, Sealer, hello, read_hello};
            let own = hello()?;
            sink.send(Message::Binary(Bytes::copy_from_slice(&own)))
                .await?;
            let peer = loop {
                match tokio::time::timeout(Duration::from_secs(15), stream.next()).await? {
                    Some(Ok(Message::Binary(data))) => break read_hello(&data)?,
                    Some(Ok(Message::Close(_)) | Err(_)) | None => {
                        anyhow::bail!("closed before its hello")
                    }
                    Some(Ok(_)) => continue,
                }
            };
            Ok::<_, anyhow::Error>((
                Sealer::new(&key, Direction::ToSwitch, peer)?,
                Opener::new(&key, Direction::ToRelay, own)?,
            ))
        };
        let (mut sealer, mut opener) = match session.await {
            Ok(session) => session,
            Err(error) => {
                eprintln!("relay: uplink {stackbox} from {host}: {error:#}");
                self.release(&stackbox, &host, id);
                return;
            }
        };
        eprintln!("relay: uplink {stackbox} from {host} up");
        let mut ping = tokio::time::interval(PING_EVERY);
        ping.tick().await;
        loop {
            tokio::select! {
                _ = ping.tick() => {
                    if sink.send(Message::Ping(Bytes::new())).await.is_err() {
                        break;
                    }
                }
                message = rx.recv() => {
                    let Some(message) = message else { break };
                    let Ok(sealed) = sealer.seal(&message) else { break };
                    if sink.send(Message::Binary(sealed.into())).await.is_err() {
                        break;
                    }
                }
                message = stream.next() => {
                    let data = match message {
                        Some(Ok(Message::Binary(data))) => data,
                        Some(Ok(Message::Close(_)) | Err(_)) | None => break,
                        Some(Ok(_)) => continue,
                    };
                    let plain = match opener.open(&data) {
                        Ok(plain) if plain.len() <= MAX_MESSAGE + 256 => Bytes::from(plain),
                        Ok(_) => {
                            eprintln!("relay: uplink {stackbox} from {host} sent an oversized message; closing");
                            break;
                        }
                        Err(error) => {
                            eprintln!("relay: uplink {stackbox} from {host}: {error:#}; closing");
                            break;
                        }
                    };
                    if !decode_trunk(&plain).is_ok_and(|(segment, _)| segment.starts_with(&prefix)) {
                        eprintln!("relay: uplink {stackbox} from {host} sent a bad message; closing");
                        break;
                    }
                    self.pass(&stackbox, &host, plain);
                }
            }
        }
        self.release(&stackbox, &host, id);
        eprintln!("relay: uplink {stackbox} from {host} down");
    }

    /// Forget the uplink `id` unless a newer one replaced it.
    fn release(&self, stackbox: &str, host: &str, id: u64) {
        let mut stackboxes = self.stackboxes.lock().unwrap();
        if let Some(entry) = stackboxes.get_mut(stackbox)
            && entry.uplinks.get(host).is_some_and(|(live, _)| *live == id)
        {
            entry.uplinks.remove(host);
        }
    }

    fn pass(&self, stackbox: &str, from: &str, message: Bytes) {
        let stackboxes = self.stackboxes.lock().unwrap();
        let Some(entry) = stackboxes.get(stackbox) else {
            return;
        };
        for (host, (_, tx)) in &entry.uplinks {
            if host != from {
                // A full queue drops the message, as a switch drops frames.
                let _ = tx.try_send(message.clone());
            }
        }
    }
}

/// `wss://<relay>/api/v1/link-relay?stackbox=<id>&host=<host>` for the relay's base URL as other
/// hosts reach it (`https://host:port`).
pub fn uplink_url(base: &str, stackbox: &str, host: &str) -> Result<String> {
    let base = base.trim_end_matches('/');
    let (scheme, rest) = base
        .split_once("://")
        .ok_or_else(|| anyhow!("the relay URL needs a scheme, such as https://10.0.0.5:10011"))?;
    ensure!(
        matches!(scheme, "https" | "wss"),
        "the relay URL {base} must be https://; the relay always uses TLS"
    );
    ensure!(
        !rest.is_empty() && !rest.contains(['/', '?', '#']),
        "the relay URL {base} must be only a scheme, host and port"
    );
    super::validate_name(stackbox)?;
    crate::model::validate_id(host)?;
    Ok(format!(
        "wss://{rest}{}?stackbox={stackbox}&host={host}",
        super::RELAY_PATH
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uplink_urls_always_use_tls() {
        assert_eq!(
            uplink_url("https://stackbox.example:10011/", "f", "h1").unwrap(),
            "wss://stackbox.example:10011/api/v1/link-relay?stackbox=f&host=h1"
        );
        assert!(uplink_url("http://10.0.0.1:10011", "f", "h1").is_err());
        assert!(uplink_url("stackbox.example", "f", "h1").is_err());
        assert!(uplink_url("https://x/path", "f", "h1").is_err());
        assert_eq!(
            crate::link::tls::url_host("https://10.0.0.1:10011").unwrap(),
            "10.0.0.1"
        );
        assert_eq!(
            crate::link::tls::url_host("https://[::1]:1/").unwrap(),
            "::1"
        );
        assert_eq!(
            crate::link::tls::url_host("https://stackbox.example").unwrap(),
            "stackbox.example"
        );
    }
}
