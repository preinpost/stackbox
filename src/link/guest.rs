//! The guest end of a link: a persistent TAP device bridged to one switch port.
//!
//! `farm-link@<dev>.service` first runs `agent link --setup <conf>`, which creates the TAP and
//! gives it its MTU and address, then `agent link <conf>`, which attaches to it and carries its
//! frames. The device outlives the second process, so services bound to its address keep
//! running while the link restarts or reconnects.

use anyhow::{Context, Result, ensure};

/// `/etc/farm/link-<dev>.conf`.
#[derive(Clone, Debug, PartialEq)]
pub struct LinkConf {
    pub dev: String,
    /// `host:port` of the host's switch.
    pub switch: String,
    pub port: String,
    pub token: String,
    pub mtu: u16,
    /// `address/prefix`, or none for a device that only carries an OVS bridge.
    pub address: Option<String>,
}

impl LinkConf {
    pub fn path(dev: &str) -> String {
        format!("/etc/farm/link-{dev}.conf")
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
        ensure!(
            self.switch.rsplit_once(':').is_some_and(|(host, port)| {
                !host.is_empty() && port.parse::<u16>().is_ok_and(|p| p != 0)
            }),
            "SWITCH must be host:port"
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
    use crate::link::{BATCH, HELLO, MAX_FRAME, switch::read_frame};
    use anyhow::{Context, Result, bail};
    use std::{
        os::fd::{AsRawFd, FromRawFd, OwnedFd},
        path::Path,
        time::Duration,
    };
    use tokio::{
        io::unix::AsyncFd,
        io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
        net::TcpStream,
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
        let stream = TcpStream::connect(&conf.switch)
            .await
            .with_context(|| format!("connect {}", conf.switch))?;
        stream.set_nodelay(true)?;
        let (read, mut write) = stream.into_split();
        write
            .write_all(format!("{HELLO} {} {}\n", conf.port, conf.token).as_bytes())
            .await?;
        let mut read = BufReader::with_capacity(BATCH, read);
        let mut line = String::new();
        read.read_line(&mut line).await?;
        if line != "OK\n" {
            bail!("switch refused {}: {}", conf.port, line.trim());
        }
        eprintln!("connected to {} as {}", conf.switch, conf.port);
        let up = tokio::spawn({
            let tap = tap.clone();
            async move {
                let mut buf = Vec::with_capacity(BATCH + MAX_FRAME + 2);
                loop {
                    tap.read_batch(&mut buf).await?;
                    write.write_all(&buf).await?;
                    buf.clear();
                }
                #[allow(unreachable_code)]
                Ok::<_, std::io::Error>(())
            }
        });
        let down = async {
            loop {
                let frame = read_frame(&mut read).await?;
                // A frame the guest cannot take is lost, as on a wire.
                let _ = tap.write(&frame).await;
            }
            #[allow(unreachable_code)]
            Ok::<_, std::io::Error>(())
        };
        let result = tokio::select! {
            result = down => result,
            result = up => result.context("link writer")?,
        };
        Ok(result.map(|_| ())?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn configuration_round_trips_and_is_checked() {
        let conf = LinkConf {
            dev: "farm0".into(),
            switch: "host.microsandbox.internal:41234".into(),
            port: "dev/ctl/mgmt".into(),
            token: "a".repeat(64),
            mtu: 9000,
            address: Some("10.254.0.10/24".into()),
        };
        assert_eq!(LinkConf::parse(&conf.render()).unwrap(), conf);
        let bridge = LinkConf {
            dev: "farm1".into(),
            address: None,
            ..conf.clone()
        };
        assert_eq!(LinkConf::parse(&bridge.render()).unwrap(), bridge);
        for bad in [
            LinkConf {
                dev: "farm0; rm".into(),
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
                switch: "nohost".into(),
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
