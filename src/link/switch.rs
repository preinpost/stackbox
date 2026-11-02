//! The host's learning switch. Each connection is one port; the port's segment decides which
//! other ports see its frames. Delay and loss apply per port, as on the PoC switch.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant, SystemTime},
};

use anyhow::{Context, Result, bail};
use bytes::Bytes;
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::{
        TcpListener, TcpStream,
        tcp::{OwnedReadHalf, OwnedWriteHalf},
    },
    sync::mpsc,
    task::AbortHandle,
};

use super::{BATCH, Config, HELLO, Impair, MAX_FRAME, encode, token_hash};

/// Frames queued for a port before the switch drops, like a real switch's buffer.
const QUEUE: usize = 4096;
const HELLO_TIMEOUT: Duration = Duration::from_secs(10);
const RELOAD_EVERY: Duration = Duration::from_millis(500);

type Queued = (Option<Instant>, Bytes);

struct Live {
    segment: String,
    impair: Impair,
    tx: mpsc::Sender<Queued>,
    /// Ends the connection's reader when the port is replaced or removed.
    reader: AbortHandle,
    id: u64,
}

#[derive(Default)]
struct State {
    config: Config,
    ports: HashMap<String, Live>,
    /// MAC → port, per segment.
    macs: HashMap<String, HashMap<[u8; 6], String>>,
    rng: u64,
    next_id: u64,
}
type Shared = Arc<Mutex<State>>;

impl State {
    fn chance(&mut self) -> f64 {
        // xorshift64*: loss only needs to look random.
        self.rng ^= self.rng >> 12;
        self.rng ^= self.rng << 25;
        self.rng ^= self.rng >> 27;
        (self.rng.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 11) as f64 / (1u64 << 53) as f64
    }
    fn drop_port(&mut self, name: &str) {
        if let Some(live) = self.ports.remove(name) {
            live.reader.abort();
            if let Some(macs) = self.macs.get_mut(&live.segment) {
                macs.retain(|_, port| port != name);
            }
        }
    }
    /// Learn each source, then send to the port that owns the destination or flood the segment.
    fn forward(&mut self, from: &str, frames: Vec<Bytes>) {
        let Some(port) = self.ports.get(from) else {
            return;
        };
        let (segment, ingress_loss) = (port.segment.clone(), port.impair.loss);
        for frame in frames {
            if frame.len() < 14 || (ingress_loss > 0.0 && self.chance() < ingress_loss) {
                continue;
            }
            let dst: [u8; 6] = frame[0..6].try_into().expect("14 bytes checked");
            let src: [u8; 6] = frame[6..12].try_into().expect("14 bytes checked");
            let macs = self.macs.entry(segment.clone()).or_default();
            if src[0] & 1 == 0 {
                macs.insert(src, from.to_owned());
            }
            let targets: Vec<String> = match macs.get(&dst) {
                Some(port) if dst[0] & 1 == 0 => vec![port.clone()],
                _ => self
                    .ports
                    .iter()
                    .filter(|(name, live)| *name != from && live.segment == segment)
                    .map(|(name, _)| name.clone())
                    .collect(),
            };
            for target in targets {
                let Some(impair) = self.ports.get(&target).map(|t| t.impair) else {
                    continue;
                };
                if impair.loss > 0.0 && self.chance() < impair.loss {
                    continue;
                }
                let due = (impair.delay_ms > 0)
                    .then(|| Instant::now() + Duration::from_millis(impair.delay_ms));
                // A full queue drops the frame, as a switch does.
                let _ = self.ports[&target].tx.try_send((due, frame.clone()));
            }
        }
    }
    /// Follow a new configuration: drop ports that left it or changed, update impairments.
    fn apply(&mut self, config: Config) {
        let gone: Vec<String> = self
            .ports
            .iter()
            .filter(|(name, live)| {
                config
                    .ports
                    .get(*name)
                    .is_none_or(|p| p.segment != live.segment)
            })
            .map(|(name, _)| name.clone())
            .collect();
        for name in gone {
            eprintln!("port {name} removed");
            self.drop_port(&name);
        }
        for (name, live) in &mut self.ports {
            live.impair = config.ports[name].impair;
        }
        // A changed token ends the connection that used the old one.
        let rotated: Vec<String> = self
            .config
            .ports
            .iter()
            .filter(|(name, old)| {
                config
                    .ports
                    .get(*name)
                    .is_some_and(|new| new.token_sha256 != old.token_sha256)
            })
            .map(|(name, _)| name.clone())
            .collect();
        for name in rotated {
            self.drop_port(&name);
        }
        self.config = config;
    }
}

/// Serve ports on `listener` with the configuration at `config`, read again whenever it changes.
pub async fn serve(listener: TcpListener, config: PathBuf) -> Result<()> {
    let state: Shared = Arc::new(Mutex::new(State {
        config: read_config(&config)?,
        rng: 0x9E37_79B9_7F4A_7C15 ^ u64::from(std::process::id()),
        ..Default::default()
    }));
    tokio::spawn(follow(state.clone(), config));
    loop {
        let (stream, _) = listener.accept().await?;
        let state = state.clone();
        tokio::spawn(async move {
            if let Err(error) = connection(state, stream).await {
                eprintln!("link: {error:#}");
            }
        });
    }
}

fn read_config(path: &Path) -> Result<Config> {
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .with_context(|| format!("invalid switch configuration {}", path.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
        Err(error) => Err(error.into()),
    }
}

async fn follow(state: Shared, path: PathBuf) {
    let stamp = |path: &Path| {
        std::fs::metadata(path)
            .and_then(|m| Ok((m.modified()?, m.len())))
            .ok()
    };
    let mut seen: Option<(SystemTime, u64)> = stamp(&path);
    loop {
        tokio::time::sleep(RELOAD_EVERY).await;
        let now = stamp(&path);
        if now == seen {
            continue;
        }
        seen = now;
        match read_config(&path) {
            Ok(config) => state.lock().unwrap().apply(config),
            // The agent replaces the file whole, so this is a broken edit; keep the last one.
            Err(error) => eprintln!("link: {error:#}"),
        }
    }
}

async fn connection(state: Shared, stream: TcpStream) -> Result<()> {
    stream.set_nodelay(true)?;
    let (read, mut write) = stream.into_split();
    let mut read = BufReader::with_capacity(BATCH, read);
    let mut line = Vec::new();
    tokio::time::timeout(
        HELLO_TIMEOUT,
        (&mut read).take(512).read_until(b'\n', &mut line),
    )
    .await
    .context("no hello")??;
    // A readiness probe connects and closes without a word.
    if line.is_empty() {
        return Ok(());
    }
    let hello = String::from_utf8_lossy(&line);
    let (name, token) = match hello.trim_end().split(' ').collect::<Vec<_>>()[..] {
        [HELLO, name, token] => (name.to_owned(), token.to_owned()),
        _ => {
            let _ = write
                .write_all(b"ERR expected FARMLINK/1 <port> <token>\n")
                .await;
            bail!("bad hello");
        }
    };
    let accepted = {
        let state = state.lock().unwrap();
        state.config.ports.get(&name).and_then(|port| {
            // Compare hashes in constant time; the token is the port's only credential.
            use subtle::ConstantTimeEq;
            bool::from(
                port.token_sha256
                    .as_bytes()
                    .ct_eq(token_hash(&token).as_bytes()),
            )
            .then(|| (port.segment.clone(), port.impair))
        })
    };
    let Some((segment, impair)) = accepted else {
        let _ = write.write_all(b"ERR unknown port or wrong token\n").await;
        bail!("refused port {name}");
    };
    write.write_all(b"OK\n").await?;
    let (tx, rx) = mpsc::channel(QUEUE);
    let writer = tokio::spawn(write_frames(rx, write));
    let reader = tokio::spawn(read_frames(state.clone(), name.clone(), read));
    let id = {
        let mut state = state.lock().unwrap();
        // The guest reconnected, say after its VM restarted: the new connection wins.
        state.drop_port(&name);
        state.next_id += 1;
        let id = state.next_id;
        state.ports.insert(
            name.clone(),
            Live {
                segment,
                impair,
                tx,
                reader: reader.abort_handle(),
                id,
            },
        );
        id
    };
    eprintln!("port {name} up");
    let _ = reader.await;
    {
        let mut state = state.lock().unwrap();
        if state.ports.get(&name).is_some_and(|live| live.id == id) {
            state.drop_port(&name);
        }
    }
    writer.abort();
    eprintln!("port {name} down");
    Ok(())
}

/// Read frames and forward each run that has already arrived under one lock.
async fn read_frames(state: Shared, name: String, mut read: BufReader<OwnedReadHalf>) {
    // Delayed frames wait in their own queue, so the connection keeps being read.
    let (delayed_tx, mut delayed_rx) = mpsc::channel::<(Instant, Bytes)>(QUEUE);
    let delayed = tokio::spawn({
        let (state, name) = (state.clone(), name.clone());
        async move {
            while let Some((due, frame)) = delayed_rx.recv().await {
                tokio::time::sleep_until(due.into()).await;
                state.lock().unwrap().forward(&name, vec![frame]);
            }
        }
    });
    let _ = async {
        loop {
            let mut frames = vec![read_frame(&mut read).await?];
            while complete_frame(read.buffer()) {
                frames.push(read_frame(&mut read).await?);
            }
            let delay = state
                .lock()
                .unwrap()
                .ports
                .get(&name)
                .map_or(0, |live| live.impair.delay_ms);
            if delay == 0 {
                state.lock().unwrap().forward(&name, frames);
            } else {
                let due = Instant::now() + Duration::from_millis(delay);
                for frame in frames {
                    let _ = delayed_tx.try_send((due, frame));
                }
            }
        }
        #[allow(unreachable_code)]
        Ok::<_, std::io::Error>(())
    }
    .await;
    delayed.abort();
}

/// Whether `buffered` starts with a whole frame, which can then be read without waiting.
pub(crate) fn complete_frame(buffered: &[u8]) -> bool {
    match buffered {
        [hi, lo, rest @ ..] => rest.len() >= usize::from(u16::from_be_bytes([*hi, *lo])),
        _ => false,
    }
}

pub(crate) async fn read_frame<R: tokio::io::AsyncRead + Unpin>(
    read: &mut R,
) -> std::io::Result<Bytes> {
    let len = usize::from(read.read_u16().await?);
    let mut frame = vec![0; len];
    read.read_exact(&mut frame).await?;
    Ok(frame.into())
}

/// Send queued frames, as many per write as have accumulated, each after its due time.
async fn write_frames(mut rx: mpsc::Receiver<Queued>, mut write: OwnedWriteHalf) {
    let mut buf = Vec::with_capacity(BATCH + MAX_FRAME + 2);
    let _ = async {
        while let Some(first) = rx.recv().await {
            let mut next = Some(first);
            while let Some((due, frame)) = next.take() {
                if let Some(due) = due
                    && due > Instant::now()
                {
                    if !buf.is_empty() {
                        write.write_all(&buf).await?;
                        buf.clear();
                    }
                    tokio::time::sleep_until(due.into()).await;
                }
                encode(&mut buf, &frame);
                if buf.len() >= BATCH {
                    break;
                }
                next = rx.try_recv().ok();
            }
            write.write_all(&buf).await?;
            buf.clear();
        }
        Ok::<_, std::io::Error>(())
    }
    .await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::link::{PortConfig, token_hash};

    fn frame(dst: u8, src: u8, body: &[u8]) -> Vec<u8> {
        let mut f = vec![2, 0, 0, 0, 0, dst, 2, 0, 0, 0, 0, src, 0x08, 0x00];
        f.extend_from_slice(body);
        f
    }
    const BROADCAST: u8 = 0xff;

    struct Guest {
        read: BufReader<OwnedReadHalf>,
        write: OwnedWriteHalf,
    }
    impl Guest {
        async fn connect(addr: std::net::SocketAddr, port: &str, token: &str) -> Result<Self> {
            let stream = TcpStream::connect(addr).await?;
            let (read, mut write) = stream.into_split();
            write
                .write_all(format!("{HELLO} {port} {token}\n").as_bytes())
                .await?;
            let mut read = BufReader::new(read);
            let mut line = String::new();
            read.read_line(&mut line).await?;
            anyhow::ensure!(line == "OK\n", "{line}");
            Ok(Self { read, write })
        }
        async fn send(&mut self, frames: &[Vec<u8>]) {
            let mut buf = Vec::new();
            for f in frames {
                encode(&mut buf, f);
            }
            self.write.write_all(&buf).await.unwrap();
        }
        async fn recv(&mut self) -> Option<Bytes> {
            tokio::time::timeout(Duration::from_millis(300), read_frame(&mut self.read))
                .await
                .ok()
                .and_then(Result::ok)
        }
    }

    fn config(ports: &[(&str, &str, &str)]) -> Config {
        Config {
            ports: ports
                .iter()
                .map(|(name, segment, token)| {
                    (
                        name.to_string(),
                        PortConfig {
                            segment: segment.to_string(),
                            token_sha256: token_hash(token),
                            impair: Impair::default(),
                        },
                    )
                })
                .collect(),
        }
    }

    async fn start(config: &Config) -> (std::net::SocketAddr, PathBuf, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ports.json");
        std::fs::write(&path, serde_json::to_vec(config).unwrap()).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(serve(listener, path.clone()));
        (addr, path, dir)
    }

    #[tokio::test]
    async fn segments_learn_flood_and_stay_apart() {
        let (addr, _, _dir) = start(&config(&[
            ("f/a/m", "f/m", "ta"),
            ("f/b/m", "f/m", "tb"),
            ("f/c/m", "f/m", "tc"),
            ("g/x/m", "g/m", "tx"),
        ]))
        .await;
        assert!(Guest::connect(addr, "f/a/m", "wrong").await.is_err());
        assert!(Guest::connect(addr, "nope", "ta").await.is_err());
        let mut a = Guest::connect(addr, "f/a/m", "ta").await.unwrap();
        let mut b = Guest::connect(addr, "f/b/m", "tb").await.unwrap();
        let mut c = Guest::connect(addr, "f/c/m", "tc").await.unwrap();
        let mut x = Guest::connect(addr, "g/x/m", "tx").await.unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        // A broadcast floods its own segment only.
        a.send(&[frame(BROADCAST, 1, b"hello")]).await;
        assert_eq!(&b.recv().await.unwrap()[14..], b"hello");
        assert_eq!(&c.recv().await.unwrap()[14..], b"hello");
        assert!(x.recv().await.is_none());
        assert!(a.recv().await.is_none());
        // b answers a's learned MAC: only a gets it. A batch of frames all arrive in order.
        let batch: Vec<_> = (0..50u8).map(|i| frame(1, 2, &[i])).collect();
        b.send(&batch).await;
        for i in 0..50u8 {
            assert_eq!(&a.recv().await.unwrap()[14..], [i]);
        }
        assert!(c.recv().await.is_none());
        // Another segment cannot reach f's MACs.
        x.send(&[frame(1, 9, b"leak")]).await;
        assert!(a.recv().await.is_none());
    }

    #[tokio::test]
    async fn configuration_changes_impair_and_drop_ports() {
        let mut cfg = config(&[("f/a/m", "f/m", "ta"), ("f/b/m", "f/m", "tb")]);
        let (addr, path, _dir) = start(&cfg).await;
        let mut a = Guest::connect(addr, "f/a/m", "ta").await.unwrap();
        let mut b = Guest::connect(addr, "f/b/m", "tb").await.unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        let write = |cfg: &Config| {
            let tmp = path.with_extension("tmp");
            std::fs::write(&tmp, serde_json::to_vec(cfg).unwrap()).unwrap();
            std::fs::rename(&tmp, &path).unwrap();
        };
        // Isolate b.
        cfg.ports.get_mut("f/b/m").unwrap().impair.loss = 1.0;
        write(&cfg);
        tokio::time::sleep(RELOAD_EVERY * 3).await;
        a.send(&[frame(BROADCAST, 1, b"x")]).await;
        assert!(b.recv().await.is_none());
        // Delay b instead: frames still arrive, later.
        let port = cfg.ports.get_mut("f/b/m").unwrap();
        port.impair = Impair {
            delay_ms: 150,
            loss: 0.0,
        };
        write(&cfg);
        tokio::time::sleep(RELOAD_EVERY * 3).await;
        let sent = Instant::now();
        a.send(&[frame(BROADCAST, 1, b"y")]).await;
        let got = tokio::time::timeout(Duration::from_secs(2), read_frame(&mut b.read))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(&got[14..], b"y");
        assert!(sent.elapsed() >= Duration::from_millis(140));
        // Removing a port, or rotating its token, ends its connection.
        cfg.ports.remove("f/b/m");
        cfg.ports.get_mut("f/a/m").unwrap().token_sha256 = token_hash("new");
        write(&cfg);
        tokio::time::sleep(RELOAD_EVERY * 3).await;
        let mut byte = [0u8; 1];
        for guest in [&mut a, &mut b] {
            let n = tokio::time::timeout(Duration::from_secs(2), guest.read.read(&mut byte))
                .await
                .unwrap()
                .unwrap_or(0);
            assert_eq!(n, 0, "connection still open");
        }
        assert!(Guest::connect(addr, "f/a/m", "new").await.is_ok());
    }

    #[tokio::test]
    async fn a_reconnecting_guest_replaces_its_old_connection() {
        let (addr, _, _dir) =
            start(&config(&[("f/a/m", "f/m", "ta"), ("f/b/m", "f/m", "tb")])).await;
        let mut old = Guest::connect(addr, "f/a/m", "ta").await.unwrap();
        let mut b = Guest::connect(addr, "f/b/m", "tb").await.unwrap();
        let mut new = Guest::connect(addr, "f/a/m", "ta").await.unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        b.send(&[frame(BROADCAST, 2, b"z")]).await;
        assert_eq!(&new.recv().await.unwrap()[14..], b"z");
        assert!(old.recv().await.is_none());
    }
}
