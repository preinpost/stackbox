//! The switch on each host. The agent starts it as a process of its own, so farm networks stay
//! up while agents, deploy workers and the coordinator come and go. Its files live under
//! `<agent dir>/switch/`: `switch.lock` (held while it runs), `pid`, `addr` (its loopback address,
//! kept across restarts because guests have it in their link configuration), `ports.json`
//! (written only through [`Switch`], under `ports.lock`) and `switch.log`.

use std::{
    ffi::OsString,
    fs::{self, File, OpenOptions},
    io::Write,
    net::SocketAddr,
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context, Result, anyhow, bail};
use fs2::FileExt;

use super::{
    Config, HOST_NAME, Impair, PATH, PortConfig, UplinkConfig, new_token, token_hash, validate_name,
};

pub struct Switch {
    dir: PathBuf,
}

impl Switch {
    pub fn new(agent_dir: &Path) -> Self {
        Self {
            dir: agent_dir.join("switch"),
        }
    }
    fn file(&self, name: &str, append: bool) -> Result<File> {
        fs::create_dir_all(&self.dir)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&self.dir, fs::Permissions::from_mode(0o700))?;
        }
        let mut options = OpenOptions::new();
        options.create(true);
        if append {
            options.append(true);
        } else {
            options.read(true).write(true).truncate(false);
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        Ok(options.open(self.dir.join(name))?)
    }

    /// The switch's endpoint, as guests on this host reach it.
    pub fn guest_url(&self) -> Result<String> {
        let addr = self.addr()?.context("the link switch has not started")?;
        Ok(format!("ws://{HOST_NAME}:{}{PATH}", addr.port()))
    }
    fn addr(&self) -> Result<Option<SocketAddr>> {
        match fs::read_to_string(self.dir.join("addr")) {
            Ok(text) => Ok(Some(text.trim().parse().context("invalid switch address")?)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    pub fn ports(&self) -> Result<Config> {
        Ok(match fs::read(self.dir.join("ports.json")) {
            Ok(bytes) => serde_json::from_slice(&bytes).context("invalid ports.json")?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Config::default(),
            Err(error) => return Err(error.into()),
        })
    }
    /// Change the port configuration under its lock and replace the file whole, which the
    /// running switch picks up.
    fn edit<T>(&self, change: impl FnOnce(&mut Config) -> Result<T>) -> Result<T> {
        let lock = self.file("ports.lock", false)?;
        lock.lock_exclusive()?;
        let result = (|| {
            let mut config = self.ports()?;
            let value = change(&mut config)?;
            let staging = self
                .dir
                .join(format!(".ports-{}.json", uuid::Uuid::new_v4()));
            let written = (|| {
                let mut file = OpenOptions::new()
                    .create_new(true)
                    .write(true)
                    .open(&staging)?;
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    file.set_permissions(fs::Permissions::from_mode(0o600))?;
                }
                file.write_all(&serde_json::to_vec_pretty(&config)?)?;
                file.sync_all()?;
                fs::rename(&staging, self.dir.join("ports.json"))?;
                Ok::<_, anyhow::Error>(())
            })();
            if written.is_err() {
                let _ = fs::remove_file(&staging);
            }
            written.map(|()| value)
        })();
        let _ = FileExt::unlock(&lock);
        result
    }
    /// Add a port, or replace one with a new token, and return the token for its guest.
    pub fn add_port(&self, name: &str, segment: &str) -> Result<String> {
        validate_name(name)?;
        validate_name(segment)?;
        let token = new_token()?;
        self.edit(|config| {
            config.ports.insert(
                name.to_owned(),
                PortConfig {
                    segment: segment.to_owned(),
                    token_sha256: token_hash(&token),
                    impair: Impair::default(),
                },
            );
            Ok(())
        })?;
        Ok(token)
    }
    /// Remove `prefix` and every port under it (`<prefix>/...`), and the uplink of the farm
    /// `prefix`; returns how many ports.
    pub fn remove_ports(&self, prefix: &str) -> Result<usize> {
        let under = format!("{prefix}/");
        self.edit(|config| {
            let before = config.ports.len();
            config
                .ports
                .retain(|name, _| name != prefix && !name.starts_with(&under));
            config.uplinks.remove(prefix);
            Ok(before - config.ports.len())
        })
    }
    /// Point the farm's uplink at the relay's new address, keeping its token and CA.
    pub fn set_uplink_url(&self, farm: &str, url: &str) -> Result<()> {
        anyhow::ensure!(url.starts_with("wss://"), "the relay URL must be wss://");
        self.edit(|config| {
            config
                .uplinks
                .get_mut(farm)
                .with_context(|| format!("no uplink for farm {farm}"))?
                .url = url.to_owned();
            Ok(())
        })
    }
    /// Join the farm's segments on this host to its other hosts through the relay.
    pub fn set_uplink(&self, farm: &str, uplink: UplinkConfig) -> Result<()> {
        validate_name(farm)?;
        for segment in &uplink.segments {
            validate_name(segment)?;
            anyhow::ensure!(
                segment.starts_with(&format!("{farm}/")),
                "segment {segment} is not farm {farm}'s"
            );
        }
        anyhow::ensure!(
            uplink.url.starts_with("wss://"),
            "the relay URL must be wss://"
        );
        if !uplink.ca.is_empty() {
            super::tls::validate_ca(&uplink.ca)?;
        }
        anyhow::ensure!(
            uplink.key.len() == 64 && uplink.key.bytes().all(|b| b.is_ascii_hexdigit()),
            "invalid uplink key"
        );
        self.edit(|config| {
            config.uplinks.insert(farm.to_owned(), uplink);
            Ok(())
        })
    }
    pub fn impair(&self, name: &str, impair: Impair) -> Result<()> {
        impair.validate()?;
        self.edit(|config| {
            config
                .ports
                .get_mut(name)
                .with_context(|| format!("no link port {name}"))?
                .impair = impair;
            Ok(())
        })
    }

    /// Stop a switch built from another revision, which may not read this one's
    /// configuration. Guests reconnect to its successor on the same address; their TAP
    /// devices, and the services on them, stay up.
    async fn retire_outdated(&self) -> Result<()> {
        if !self.running()? {
            return Ok(());
        }
        let build = fs::read_to_string(self.dir.join("build")).unwrap_or_default();
        if build.trim() == crate::BUILD_ID {
            return Ok(());
        }
        let pid: i32 = fs::read_to_string(self.dir.join("pid"))?
            .trim()
            .parse()
            .context("invalid switch pid")?;
        eprintln!("restarting the link switch built from '{}'", build.trim());
        #[cfg(unix)]
        // SAFETY: signals the process that holds the switch lock, recorded in its pid file.
        unsafe {
            libc::kill(pid, libc::SIGTERM);
        }
        tokio::time::timeout(Duration::from_secs(10), async {
            while self.running()? {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            Ok::<_, anyhow::Error>(())
        })
        .await
        .map_err(|_| anyhow!("the outdated link switch (pid {pid}) did not stop"))?
    }

    pub fn running(&self) -> Result<bool> {
        let lock = self.file("switch.lock", false)?;
        Ok(match lock.try_lock_exclusive() {
            Ok(()) => {
                let _ = FileExt::unlock(&lock);
                false
            }
            Err(_) => true,
        })
    }

    /// Start the switch with `command` (the agent binary and `agent switch`; the switch
    /// directory is appended) unless it runs, and return its guest endpoint.
    pub async fn ensure(&self, command: &[OsString]) -> Result<String> {
        self.retire_outdated().await?;
        if !self.running()? {
            let (program, args) = command.split_first().context("no switch command")?;
            let log = self.file("switch.log", true)?;
            let mut switch = tokio::process::Command::new(program);
            switch
                .args(args)
                .arg(&self.dir)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(log);
            #[cfg(unix)]
            // SAFETY: setsid is async-signal-safe and touches no memory of the parent.
            unsafe {
                switch.pre_exec(|| {
                    if libc::setsid() == -1 {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
            #[cfg(windows)]
            switch.creation_flags(0x0000_0008 | 0x0000_0200);
            let mut child = switch.spawn().context("cannot start the link switch")?;
            // Reaps the switch if this process outlives it, as a dial-in agent does.
            tokio::spawn(async move {
                let _ = child.wait().await;
            });
        }
        let ready = async {
            loop {
                if self.running()?
                    && let Some(addr) = self.addr()?
                    && tokio::net::TcpStream::connect(addr).await.is_ok()
                {
                    return Ok::<_, anyhow::Error>(());
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        };
        tokio::time::timeout(Duration::from_secs(15), ready)
            .await
            .map_err(|_| {
                anyhow!(
                    "the link switch did not start; see {}",
                    self.dir.join("switch.log").display()
                )
            })??;
        self.guest_url()
    }

    /// The switch process: hold the lock, listen on the recorded address (a free loopback
    /// port the first time) and serve until killed.
    pub async fn serve(&self) -> Result<()> {
        let lock = self.file("switch.lock", false)?;
        if lock.try_lock_exclusive().is_err() {
            bail!("the link switch already runs");
        }
        fs::write(self.dir.join("pid"), std::process::id().to_string())?;
        fs::write(self.dir.join("build"), crate::BUILD_ID)?;
        let listener = match self.addr()? {
            // A restart may find the port in TIME_WAIT or briefly held; guests know only it.
            Some(addr) => {
                let mut tries = 0;
                loop {
                    match tokio::net::TcpListener::bind(addr).await {
                        Ok(listener) => break listener,
                        Err(error) if tries < 60 => {
                            eprintln!("bind {addr}: {error}; retrying");
                            tries += 1;
                            tokio::time::sleep(Duration::from_secs(1)).await;
                        }
                        Err(error) => return Err(error).with_context(|| format!("bind {addr}")),
                    }
                }
            }
            None => {
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
                let addr = listener.local_addr()?;
                let staging = self.dir.join(".addr");
                fs::write(&staging, addr.to_string())?;
                fs::rename(&staging, self.dir.join("addr"))?;
                listener
            }
        };
        eprintln!(
            "link switch {} listening on {}",
            crate::BUILD_ID,
            listener.local_addr()?
        );
        let result = super::switch::serve(listener, self.dir.join("ports.json")).await;
        drop(lock);
        result
    }
}

/// `agent switch <dir>`: the switch process [`Switch::ensure`] starts.
pub async fn serve_dir(dir: &Path) -> Result<()> {
    let agent_dir = dir.parent().context("not a switch directory")?;
    Switch::new(agent_dir).serve().await
}

/// The Linux farm-agent binary guests run their links with: `FARM_GUEST_AGENT`, or on a
/// Linux host this very binary, which is built for the guests' architecture.
pub fn guest_agent() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("FARM_GUEST_AGENT").filter(|p| !p.is_empty()) {
        return Ok(path.into());
    }
    if cfg!(target_os = "linux") {
        return std::env::current_exe().context("cannot locate the agent binary");
    }
    bail!(
        "links need a Linux farm-agent for the guests; set FARM_GUEST_AGENT (task agent-build builds dist/agent/<arch>/farm-agent)"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Not a test of its own: `the_switch_runs_detached_and_keeps_its_address` starts the
    /// switch process from this test binary with the switch directory as the last argument.
    #[tokio::test]
    async fn switch_process() {
        let Some(dir) = std::env::args()
            .next_back()
            .map(PathBuf::from)
            .filter(|dir| dir.ends_with("switch") && dir.is_dir())
        else {
            return;
        };
        serve_dir(&dir).await.unwrap();
    }

    #[tokio::test]
    async fn the_switch_runs_detached_and_keeps_its_address() {
        let dir = tempfile::tempdir().unwrap();
        let switch = Switch::new(dir.path());
        let command: Vec<OsString> = vec![
            std::env::current_exe().unwrap().into(),
            "--exact".into(),
            "link::host::tests::switch_process".into(),
            "--nocapture".into(),
        ];
        let token = switch.add_port("f/a/m", "f/m").unwrap();
        assert_eq!(
            switch.ports().unwrap().ports["f/a/m"].token_sha256,
            token_hash(&token)
        );
        assert!(!switch.running().unwrap());
        let address = switch.ensure(&command).await.unwrap();
        assert!(
            address.starts_with("ws://host.microsandbox.internal:") && address.ends_with("/link"),
            "{address}"
        );
        assert!(switch.running().unwrap());
        // A second start finds it running.
        assert_eq!(switch.ensure(&command).await.unwrap(), address);
        let addr = switch.addr().unwrap().unwrap();
        // The switch serves the port with that token.
        use crate::link::switch::tests::Guest;
        Guest::connect(addr, "f/a/m", &token).await.unwrap();
        assert!(Guest::connect(addr, "f/a/m", "wrong").await.is_err());
        switch
            .impair(
                "f/a/m",
                Impair {
                    delay_ms: 5,
                    loss: 0.5,
                },
            )
            .unwrap();
        assert!(switch.impair("f/zz/m", Impair::default()).is_err());
        switch.add_port("f/b/m", "f/m").unwrap();
        switch.add_port("g/a/m", "g/m").unwrap();
        assert_eq!(switch.remove_ports("f").unwrap(), 2);
        assert_eq!(
            switch.ports().unwrap().ports.keys().collect::<Vec<_>>(),
            ["g/a/m"]
        );
        #[cfg(unix)]
        {
            let kill = || {
                let pid: i32 = fs::read_to_string(dir.path().join("switch/pid"))
                    .unwrap()
                    .parse()
                    .unwrap();
                // SAFETY: signals the switch process this test started.
                unsafe { libc::kill(pid, libc::SIGKILL) };
            };
            kill();
            tokio::time::timeout(Duration::from_secs(10), async {
                while switch.running().unwrap() {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
            })
            .await
            .unwrap();
            // Started again, the switch listens where guests expect it.
            assert_eq!(switch.ensure(&command).await.unwrap(), address);
            assert_eq!(switch.addr().unwrap().unwrap(), addr);
            kill();
        }
    }
}
