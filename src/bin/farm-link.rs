//! PoC: a farm's L2 network carried over WebSocket.
//!
//! `switch` runs on the host: a learning Ethernet switch whose ports are WebSocket links, with
//! per-port delay and loss set through `POST /impair`. `link` runs in a guest and bridges a TAP
//! device to one port. `sink` and `send` measure plain TCP from a guest to the host, the
//! ceiling the msb network sets for any link.

use anyhow::{Context, Result, bail};
use axum::{
    Router,
    extract::{
        Query, State,
        ws::{Message, WebSocket, WebSocketUpgrade},
    },
    routing::{get, post},
};
use bytes::Bytes;
use futures::{SinkExt, StreamExt};
use serde::Deserialize;
use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    sync::mpsc,
};

/// Frames queued per port before the switch drops, like a real switch's buffer.
const QUEUE: usize = 4096;

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let arg = |name: &str| {
        args.iter()
            .position(|a| a == name)
            .and_then(|i| args.get(i + 1))
            .cloned()
    };
    match args.first().map(String::as_str) {
        Some("switch") => switch(arg("--listen").unwrap_or("0.0.0.0:7700".into()).parse()?).await,
        Some("link") => {
            link(
                &arg("--switch").context("--switch ws://host:port/link?name=NAME")?,
                &arg("--dev").unwrap_or("farm0".into()),
            )
            .await
        }
        Some("sink") => sink(arg("--listen").unwrap_or("0.0.0.0:7701".into()).parse()?).await,
        Some("send") => {
            send(
                &arg("--to").context("--to host:port")?,
                arg("--seconds").unwrap_or("10".into()).parse()?,
            )
            .await
        }
        _ => bail!("usage: farm-link switch|link|sink|send"),
    }
}

// ---- switch ----

#[derive(Clone, Copy, Default, Deserialize, Debug)]
struct Impair {
    #[serde(default)]
    delay_ms: u64,
    /// 0.0-1.0, applied to each direction.
    #[serde(default)]
    loss: f64,
}

struct Port {
    tx: mpsc::Sender<Bytes>,
    impair: Impair,
}

#[derive(Default)]
struct Switch {
    ports: HashMap<String, Port>,
    macs: HashMap<[u8; 6], String>,
    rng: u64,
}
impl Switch {
    fn chance(&mut self) -> f64 {
        // xorshift64*; the PoC only needs loss to look random.
        self.rng ^= self.rng >> 12;
        self.rng ^= self.rng << 25;
        self.rng ^= self.rng >> 27;
        (self.rng.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 11) as f64 / (1u64 << 53) as f64
    }
    /// Learn the source, then forward to the port that owns the destination or flood.
    fn forward(&mut self, from: &str, frame: Bytes) {
        if frame.len() < 14 {
            return;
        }
        let dst: [u8; 6] = frame[0..6].try_into().unwrap();
        let src: [u8; 6] = frame[6..12].try_into().unwrap();
        if src[0] & 1 == 0 {
            self.macs.insert(src, from.to_owned());
        }
        let targets: Vec<String> = match self.macs.get(&dst) {
            Some(port) if dst[0] & 1 == 0 => vec![port.clone()],
            _ => self.ports.keys().filter(|p| *p != from).cloned().collect(),
        };
        let ingress_loss = self.ports.get(from).map_or(0.0, |p| p.impair.loss);
        if ingress_loss > 0.0 && self.chance() < ingress_loss {
            return;
        }
        for target in targets {
            if target == from {
                continue;
            }
            let loss = self.ports.get(&target).map_or(0.0, |p| p.impair.loss);
            if loss > 0.0 && self.chance() < loss {
                continue;
            }
            if let Some(port) = self.ports.get(&target) {
                // A full queue drops the frame, as a switch does.
                let _ = port.tx.try_send(frame.clone());
            }
        }
    }
}

type Shared = Arc<Mutex<Switch>>;

async fn switch(listen: SocketAddr) -> Result<()> {
    let state: Shared = Arc::new(Mutex::new(Switch {
        rng: 0x9E37_79B9_7F4A_7C15 ^ std::process::id() as u64,
        ..Default::default()
    }));
    let app = Router::new()
        .route("/link", get(upgrade))
        .route("/impair", post(impair))
        .route("/ports", get(ports))
        .with_state(state);
    use axum::serve::ListenerExt;
    // Frames are small and latency-bound; Nagle would hold them back.
    let listener = tokio::net::TcpListener::bind(listen).await?.tap_io(|tcp| {
        let _ = tcp.set_nodelay(true);
    });
    eprintln!("switch listening on {listen}");
    axum::serve(listener, app).await?;
    Ok(())
}

#[derive(Deserialize)]
struct LinkQuery {
    name: String,
}
async fn upgrade(
    State(state): State<Shared>,
    Query(query): Query<LinkQuery>,
    ws: WebSocketUpgrade,
) -> axum::response::Response {
    ws.max_message_size(1 << 20)
        .on_upgrade(move |socket| port(state, query.name, socket))
}

/// Delay that applies to frames from and to a port, read when each frame passes.
fn delay_of(state: &Shared, name: &str) -> Duration {
    Duration::from_millis(
        state
            .lock()
            .unwrap()
            .ports
            .get(name)
            .map_or(0, |p| p.impair.delay_ms),
    )
}

async fn port(state: Shared, name: String, socket: WebSocket) {
    let (mut sink, mut stream) = socket.split();
    let (tx, mut rx) = mpsc::channel::<Bytes>(QUEUE);
    state.lock().unwrap().ports.insert(
        name.clone(),
        Port {
            tx,
            impair: Impair::default(),
        },
    );
    eprintln!("port {name} up");
    // Egress: frames to this port wait out its delay, in order.
    let egress_state = state.clone();
    let egress_name = name.clone();
    let egress = tokio::spawn(async move {
        let (delayed_tx, mut delayed_rx) = mpsc::channel::<(Instant, Bytes)>(QUEUE);
        let writer = tokio::spawn(async move {
            while let Some((due, frame)) = delayed_rx.recv().await {
                wait_until(due).await;
                if sink.send(Message::Binary(frame)).await.is_err() {
                    break;
                }
            }
        });
        while let Some(frame) = rx.recv().await {
            let due = Instant::now() + delay_of(&egress_state, &egress_name);
            if delayed_tx.try_send((due, frame)).is_err() {
                continue;
            }
        }
        writer.abort();
    });
    // Ingress: frames from this port wait out its delay before switching, in order.
    let (ingress_tx, mut ingress_rx) = mpsc::channel::<(Instant, Bytes)>(QUEUE);
    let ingress_state = state.clone();
    let ingress_name = name.clone();
    let ingress = tokio::spawn(async move {
        while let Some((due, frame)) = ingress_rx.recv().await {
            wait_until(due).await;
            ingress_state.lock().unwrap().forward(&ingress_name, frame);
        }
    });
    while let Some(Ok(message)) = stream.next().await {
        if let Message::Binary(frame) = message {
            let due = Instant::now() + delay_of(&state, &name);
            let _ = ingress_tx.try_send((due, frame));
        }
    }
    {
        let mut switch = state.lock().unwrap();
        switch.ports.remove(&name);
        switch.macs.retain(|_, port| port != &name);
    }
    egress.abort();
    ingress.abort();
    eprintln!("port {name} down");
}

/// Timers tick in milliseconds, so an undelayed frame must not touch one.
async fn wait_until(due: Instant) {
    if due > Instant::now() {
        tokio::time::sleep_until(due.into()).await;
    }
}

#[derive(Deserialize)]
struct ImpairQuery {
    name: String,
    // Not flattened: query strings then lose their numbers.
    #[serde(default)]
    delay_ms: u64,
    #[serde(default)]
    loss: f64,
}
async fn impair(State(state): State<Shared>, Query(query): Query<ImpairQuery>) -> String {
    match state.lock().unwrap().ports.get_mut(&query.name) {
        Some(port) => {
            port.impair = Impair {
                delay_ms: query.delay_ms,
                loss: query.loss,
            };
            format!("{}: {:?}\n", query.name, port.impair)
        }
        None => format!("no port {}\n", query.name),
    }
}
async fn ports(State(state): State<Shared>) -> String {
    let switch = state.lock().unwrap();
    let mut out: Vec<String> = switch
        .ports
        .iter()
        .map(|(name, port)| format!("{name}: {:?}", port.impair))
        .collect();
    out.sort();
    out.join("\n") + "\n"
}

// ---- link (guest) ----

#[cfg(target_os = "linux")]
mod tap {
    use anyhow::{Result, bail};
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use tokio::io::unix::AsyncFd;

    pub struct Tap(AsyncFd<OwnedFd>);
    impl Tap {
        pub fn open(name: &str) -> Result<Self> {
            const TUNSETIFF: libc::c_ulong = 0x4004_54ca;
            #[repr(C)]
            struct IfReq {
                name: [u8; libc::IFNAMSIZ],
                flags: libc::c_short,
                _pad: [u8; 22],
            }
            if name.len() >= libc::IFNAMSIZ {
                bail!("interface name too long");
            }
            let fd = unsafe {
                libc::open(
                    c"/dev/net/tun".as_ptr(),
                    libc::O_RDWR | libc::O_NONBLOCK | libc::O_CLOEXEC,
                )
            };
            if fd < 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            let fd = unsafe { OwnedFd::from_raw_fd(fd) };
            let mut request = IfReq {
                name: [0; libc::IFNAMSIZ],
                flags: (libc::IFF_TAP | libc::IFF_NO_PI) as libc::c_short,
                _pad: [0; 22],
            };
            request.name[..name.len()].copy_from_slice(name.as_bytes());
            if unsafe { libc::ioctl(fd.as_raw_fd(), TUNSETIFF as _, &mut request) } < 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            Ok(Self(AsyncFd::try_new(fd).map_err(|e| e.into_parts().1)?))
        }
        pub async fn read(&self, buf: &mut [u8]) -> std::io::Result<usize> {
            loop {
                let mut guard = self.0.readable().await?;
                match guard.try_io(|fd| {
                    let n =
                        unsafe { libc::read(fd.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) };
                    if n < 0 {
                        Err(std::io::Error::last_os_error())
                    } else {
                        Ok(n as usize)
                    }
                }) {
                    Ok(result) => return result,
                    Err(_would_block) => continue,
                }
            }
        }
        pub async fn write(&self, frame: &[u8]) -> std::io::Result<()> {
            loop {
                let mut guard = self.0.writable().await?;
                match guard.try_io(|fd| {
                    let n =
                        unsafe { libc::write(fd.as_raw_fd(), frame.as_ptr().cast(), frame.len()) };
                    if n < 0 {
                        Err(std::io::Error::last_os_error())
                    } else {
                        Ok(())
                    }
                }) {
                    Ok(result) => return result,
                    Err(_would_block) => continue,
                }
            }
        }
    }
}

#[cfg(target_os = "linux")]
async fn link(url: &str, dev: &str) -> Result<()> {
    use tokio_tungstenite::tungstenite::Message;
    let tap = Arc::new(tap::Tap::open(dev).with_context(|| format!("open TAP {dev}"))?);
    eprintln!("TAP {dev} open");
    loop {
        match tokio_tungstenite::connect_async_with_config(url, None, true).await {
            Ok((socket, _)) => {
                eprintln!("connected to {url}");
                let (mut sink, mut stream) = socket.split();
                let reader = tap.clone();
                let up = tokio::spawn(async move {
                    let mut buf = vec![0u8; 65_536];
                    loop {
                        let n = reader.read(&mut buf).await?;
                        sink.send(Message::Binary(Bytes::copy_from_slice(&buf[..n])))
                            .await?;
                    }
                    #[allow(unreachable_code)]
                    Ok::<_, anyhow::Error>(())
                });
                while let Some(Ok(message)) = stream.next().await {
                    if let Message::Binary(frame) = message {
                        let _ = tap.write(&frame).await;
                    }
                }
                up.abort();
                eprintln!("disconnected; retrying");
            }
            Err(error) => eprintln!("connect failed: {error}; retrying"),
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}
#[cfg(not(target_os = "linux"))]
async fn link(_url: &str, _dev: &str) -> Result<()> {
    bail!("link needs a Linux guest")
}

// ---- baseline ----

async fn sink(listen: SocketAddr) -> Result<()> {
    let listener = tokio::net::TcpListener::bind(listen).await?;
    eprintln!("sink listening on {listen}");
    loop {
        let (mut socket, peer) = listener.accept().await?;
        tokio::spawn(async move {
            let started = Instant::now();
            let mut total = 0u64;
            let mut buf = vec![0u8; 1 << 20];
            while let Ok(n) = socket.read(&mut buf).await {
                if n == 0 {
                    break;
                }
                total += n as u64;
            }
            let secs = started.elapsed().as_secs_f64();
            eprintln!(
                "{peer}: {:.1} MiB in {secs:.1}s = {:.0} Mbit/s",
                total as f64 / 1048576.0,
                total as f64 * 8.0 / secs / 1e6
            );
        });
    }
}

async fn send(to: &str, seconds: u64) -> Result<()> {
    let mut socket = tokio::net::TcpStream::connect(to).await?;
    let buf = vec![0u8; 1 << 20];
    let started = Instant::now();
    let mut total = 0u64;
    while started.elapsed() < Duration::from_secs(seconds) {
        socket.write_all(&buf).await?;
        total += buf.len() as u64;
    }
    socket.shutdown().await?;
    let secs = started.elapsed().as_secs_f64();
    println!(
        "sent {:.1} MiB in {secs:.1}s = {:.0} Mbit/s",
        total as f64 / 1048576.0,
        total as f64 * 8.0 / secs / 1e6
    );
    Ok(())
}
