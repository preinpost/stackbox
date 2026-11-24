//! The guest end of a link: a persistent TAP device bridged to one switch port.
//!
//! `stackbox-link@<dev>.service` first runs `agent link --setup <conf>`, which creates the TAP and
//! gives it its MTU and address, then `agent link <conf>`, which attaches to it and carries its
//! frames. The device outlives the second process, so services bound to its address keep
//! running while the link restarts or reconnects.

use anyhow::{Context, Result, ensure};

/// `/etc/stackbox/link-<dev>.conf`.
#[derive(Clone, Debug, PartialEq)]
pub struct LinkConf {
    pub dev: String,
    /// The switch's WebSocket endpoint, `ws://host:port/link` (or `wss://`).
    pub switch: String,
    pub port: String,
    pub token: String,
    pub mtu: u16,
    /// `address/prefix`, or none for a device that only carries an OVS bridge.
    pub address: Option<String>,
}

impl LinkConf {
    pub fn path(dev: &str) -> String {
        format!("/etc/stackbox/link-{dev}.conf")
    }
    pub fn render(&self) -> String {
        format!(
            "DEV={}\nSWITCH={}\nPORT={}\nTOKEN={}\nMTU={}\nADDRESS={}\n",
            self.dev,
            self.switch,
            self.port,
            self.token,
            self.mtu,
            self.address.as_deref().unwrap_or("")
        )
    }
    pub fn parse(text: &str) -> Result<Self> {
        let value = |key: &str| {
            text.lines()
                .filter_map(|line| line.split_once('='))
                .find(|(k, _)| k.trim() == key)
                .map(|(_, v)| v.trim().to_owned())
                .filter(|v| !v.is_empty())
        };
        let conf = Self {
            dev: value("DEV").context("DEV missing")?,
            switch: value("SWITCH").context("SWITCH missing")?,
            port: value("PORT").context("PORT missing")?,
            token: value("TOKEN").context("TOKEN missing")?,
            mtu: value("MTU")
                .map_or(Ok(super::DEFAULT_MTU), |m| m.parse())
                .context("MTU")?,
            address: value("ADDRESS"),
        };
        conf.validate()?;
        Ok(conf)
    }
    pub fn validate(&self) -> Result<()> {
        ensure!(
            !self.dev.is_empty()
                && self.dev.len() < 16
                && self
                    .dev
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit()),
            "invalid link device '{}'",
            self.dev
        );
        super::validate_name(&self.port)?;
        ensure!(
            self.token.len() == 64 && self.token.bytes().all(|b| b.is_ascii_hexdigit()),
            "invalid link token"
        );
        ensure!((1280..=9000).contains(&self.mtu), "MTU must be 1280-9000");
        if let Some(address) = &self.address {
            let (ip, prefix) = address.split_once('/').context("ADDRESS needs a prefix")?;
            ip.parse::<std::net::Ipv4Addr>().context("ADDRESS")?;
            ensure!(
                prefix.parse::<u8>().is_ok_and(|p| p <= 32),
                "ADDRESS prefix"
            );
        }
        let uri: tokio_tungstenite::tungstenite::http::Uri =
            self.switch.parse().context("SWITCH is not a URL")?;
        ensure!(
            matches!(uri.scheme_str(), Some("ws" | "wss"))
                && uri.host().is_some_and(|h| !h.is_empty())
                && uri.query().is_none(),
            "SWITCH must be a ws:// or wss:// URL without a query"
        );
        Ok(())
    }
}

#[cfg(target_os = "linux")]
fn read(conf: &std::path::Path) -> Result<LinkConf> {
    LinkConf::parse(
        &std::fs::read_to_string(conf).with_context(|| format!("read {}", conf.display()))?,
    )
}

#[cfg(target_os = "linux")]
pub use linux::{run, setup};

#[cfg(not(target_os = "linux"))]
pub async fn setup(_conf: &std::path::Path) -> Result<()> {
    anyhow::bail!("links run in Linux guests")
}
#[cfg(not(target_os = "linux"))]
pub async fn run(_conf: &std::path::Path) -> Result<()> {
    anyhow::bail!("links run in Linux guests")
}

#[cfg(target_os = "linux")]
mod linux {
    use super::{LinkConf, read};
    use crate::link::{BATCH, MAX_FRAME, MAX_MESSAGE, decode};
    use anyhow::{Context, Result, bail};
    use futures::{SinkExt, StreamExt};
    use std::{
        os::fd::{AsRawFd, FromRawFd, OwnedFd},
        path::Path,
        time::Duration,
    };
    use tokio::io::unix::AsyncFd;
    use tokio_tungstenite::tungstenite::{
        self, Message, client::IntoClientRequest, http::header::AUTHORIZATION,
        protocol::WebSocketConfig,
    };

    const TUNSETIFF: libc::c_ulong = 0x4004_54ca;
    const TUNSETPERSIST: libc::c_ulong = 0x4004_54cb;
    const IFF_TAP: libc::c_short = 0x0002;
    const IFF_NO_PI: libc::c_short = 0x1000;

    #[repr(C)]
    struct IfReq {
        name: [u8; libc::IFNAMSIZ],
        flags: libc::c_short,
        _pad: [u8; 22],
    }

    /// Open (creating if need be) the TAP device `dev`.
    fn open(dev: &str, nonblocking: bool) -> Result<OwnedFd> {
        let mut flags = libc::O_RDWR | libc::O_CLOEXEC;
        if nonblocking {
            flags |= libc::O_NONBLOCK;
        }
        // SAFETY: a NUL-terminated path and plain flags.
        let fd = unsafe { libc::open(c"/dev/net/tun".as_ptr(), flags) };
        if fd < 0 {
            return Err(std::io::Error::last_os_error()).context("open /dev/net/tun");
        }
        // SAFETY: `fd` was just opened and is owned here only.
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        let mut req = IfReq {
            name: [0; libc::IFNAMSIZ],
            flags: IFF_TAP | IFF_NO_PI,
            _pad: [0; 22],
        };
        req.name[..dev.len()].copy_from_slice(dev.as_bytes());
        // SAFETY: TUNSETIFF reads and writes one ifreq, which `req` is.
        if unsafe { libc::ioctl(fd.as_raw_fd(), TUNSETIFF, &mut req) } < 0 {
            return Err(std::io::Error::last_os_error()).with_context(|| format!("TAP {dev}"));
        }
        Ok(fd)
    }

    fn ip(args: &[&str]) -> Result<()> {
        let status = std::process::Command::new("ip")
            .args(args)
            .status()
            .context("run ip")?;
        if !status.success() {
            bail!("ip {} failed", args.join(" "));
        }
        Ok(())
    }

    /// Create the persistent TAP and give it its MTU and address.
    pub async fn setup(conf: &Path) -> Result<()> {
        let conf = read(conf)?;
        let fd = open(&conf.dev, false)?;
        // SAFETY: TUNSETPERSIST takes an int by value.
        if unsafe { libc::ioctl(fd.as_raw_fd(), TUNSETPERSIST, 1 as libc::c_ulong) } < 0 {
            return Err(std::io::Error::last_os_error()).context("TUNSETPERSIST");
        }
        drop(fd);
        let mtu = conf.mtu.to_string();
        ip(&["link", "set", &conf.dev, "mtu", &mtu, "up"])?;
        if let Some(address) = &conf.address {
            ip(&["addr", "replace", address, "dev", &conf.dev])?;
        }
        Ok(())
    }

    struct Tap(AsyncFd<OwnedFd>);
    impl Tap {
        /// Wait for one frame, then take every frame already queued, up to [`BATCH`] bytes,
        /// each with its length prefix.
        async fn read_batch(&self, buf: &mut Vec<u8>) -> std::io::Result<()> {
            loop {
                let mut guard = self.0.readable().await?;
                loop {
                    if buf.len() >= BATCH {
                        return Ok(());
                    }
                    buf.reserve(2 + MAX_FRAME);
                    let start = buf.len();
                    let spare = buf.spare_capacity_mut();
                    // SAFETY: reads at most MAX_FRAME bytes into reserved capacity past the
                    // two-byte prefix.
                    let n = unsafe {
                        libc::read(
                            self.0.as_raw_fd(),
                            spare[2..].as_mut_ptr().cast(),
                            MAX_FRAME,
                        )
                    };
                    if n < 0 {
                        let error = std::io::Error::last_os_error();
                        if error.kind() != std::io::ErrorKind::WouldBlock {
                            return Err(error);
                        }
                        guard.clear_ready();
                        if start > 0 {
                            return Ok(());
                        }
                        break;
                    }
                    let n = n as usize;
                    // SAFETY: the prefix is written next and `n` bytes follow it.
                    unsafe { buf.set_len(start + 2 + n) };
                    buf[start..start + 2].copy_from_slice(&(n as u16).to_be_bytes());
                }
            }
        }
        async fn write(&self, frame: &[u8]) -> std::io::Result<()> {
            loop {
                let mut guard = self.0.writable().await?;
                match guard.try_io(|fd| {
                    // SAFETY: writes `frame` from a live slice.
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

    /// Carry the TAP's frames to and from the switch, reconnecting whenever the link drops.
    pub async fn run(path: &Path) -> Result<()> {
        setup(path).await?;
        let conf = read(path)?;
        let fd = open(&conf.dev, true)?;
        // SAFETY: `fd` is a valid descriptor this function owns, and only the AsyncFd uses
        // or closes it from here on.
        let fd = unsafe { AsyncFd::register(fd) }.map_err(|e| e.into_parts().1)?;
        let tap = std::sync::Arc::new(Tap(fd));
        eprintln!("TAP {} attached", conf.dev);
        loop {
            match session(&conf, &tap).await {
                Ok(()) => eprintln!("switch closed the link; reconnecting"),
                Err(error) => eprintln!("link: {error:#}; reconnecting"),
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    }

    async fn session(conf: &LinkConf, tap: &std::sync::Arc<Tap>) -> Result<()> {
        let mut request = format!("{}?port={}", conf.switch, conf.port).into_client_request()?;
        request
            .headers_mut()
            .insert(AUTHORIZATION, format!("Bearer {}", conf.token).parse()?);
        let config = WebSocketConfig::default()
            .max_message_size(Some(MAX_MESSAGE))
            .max_frame_size(Some(MAX_MESSAGE));
        let socket = match tokio_tungstenite::connect_async_with_config(request, Some(config), true)
            .await
        {
            Ok((socket, _)) => socket,
            Err(tungstenite::Error::Http(response)) => {
                bail!("switch refused {}: {}", conf.port, response.status())
            }
            Err(error) => return Err(error).with_context(|| format!("connect {}", conf.switch)),
        };
        eprintln!("connected to {} as {}", conf.switch, conf.port);
        let (mut sink, mut stream) = socket.split();
        let mut up = tokio::spawn({
            let tap = tap.clone();
            async move {
                let mut buf = Vec::with_capacity(MAX_MESSAGE);
                loop {
                    tap.read_batch(&mut buf).await?;
                    let message = std::mem::replace(&mut buf, Vec::with_capacity(MAX_MESSAGE));
                    sink.send(Message::Binary(message.into())).await?;
                }
                #[allow(unreachable_code)]
                Ok::<_, anyhow::Error>(())
            }
        });
        let down = async {
            while let Some(message) = stream.next().await {
                let data = match message? {
                    Message::Binary(data) => data,
                    Message::Close(_) => break,
                    _ => continue,
                };
                for frame in decode(&data)? {
                    // A frame the guest cannot take is lost, as on a wire.
                    let _ = tap.write(&frame).await;
                }
            }
            Ok::<_, anyhow::Error>(())
        };
        let result = tokio::select! {
            result = down => result,
            result = &mut up => result.context("link writer")?,
        };
        // A writer left behind would take the TAP's frames from the next connection's.
        up.abort();
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn configuration_round_trips_and_is_checked() {
        let conf = LinkConf {
            dev: "stackbox0".into(),
            switch: "ws://host.microsandbox.internal:41234/link".into(),
            port: "dev/ctl/mgmt".into(),
            token: "a".repeat(64),
            mtu: 9000,
            address: Some("10.254.0.10/24".into()),
        };
        assert_eq!(LinkConf::parse(&conf.render()).unwrap(), conf);
        let bridge = LinkConf {
            dev: "stackbox1".into(),
            address: None,
            ..conf.clone()
        };
        assert_eq!(LinkConf::parse(&bridge.render()).unwrap(), bridge);
        for bad in [
            LinkConf {
                dev: "stackbox0; rm".into(),
                ..conf.clone()
            },
            LinkConf {
                token: "short".into(),
                ..conf.clone()
            },
            LinkConf {
                address: Some("10.254.0.10".into()),
                ..conf.clone()
            },
            LinkConf {
                switch: "host.microsandbox.internal:41234".into(),
                ..conf.clone()
            },
            LinkConf {
                switch: "http://host:1/link".into(),
                ..conf.clone()
            },
            LinkConf {
                switch: "ws://host:1/link?port=x".into(),
                ..conf.clone()
            },
            LinkConf {
                mtu: 100,
                ..conf.clone()
            },
        ] {
            assert!(LinkConf::parse(&bad.render()).is_err(), "{bad:?}");
        }
    }
}
