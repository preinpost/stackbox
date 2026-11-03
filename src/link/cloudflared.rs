//! The coordinator's own cloudflared, which puts the relay behind a Cloudflare tunnel. It does
//! not use one installed on the machine: it fetches the latest release from GitHub (or the
//! version `FARM_CLOUDFLARED_VERSION` pins), checks it against the SHA-256 digest GitHub
//! publishes for the asset and, on macOS, against Cloudflare's code signature, and keeps it under
//! `<state dir>/bin/`. `FARM_CLOUDFLARED` names a binary to use instead, unmanaged.
//!
//! [`Tunnel`] runs it until dropped as a quick tunnel (`*.trycloudflare.com`, no account, a new
//! address each start). It checks for a new release once a day and restarts the tunnel on it.

use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::{Context, Result, anyhow, bail, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    sync::Notify,
};

const RELEASES: &str = "https://api.github.com/repos/cloudflare/cloudflared/releases";
/// Cloudflare's Apple Developer ID team, which signs its macOS releases.
const TEAM_ID: &str = "68WVV388M8";
const CHECK_EVERY: Duration = Duration::from_secs(24 * 3600);
/// A quick tunnel's name resolves a little after cloudflared registers it. Resolvers cache a
/// name that does not exist yet, so the address is handed out only after this.
const DNS_SETTLE: Duration = Duration::from_secs(if cfg!(test) { 10 } else { 15 });

#[derive(Clone, Debug, Default, Serialize, utoipa::ToSchema)]
pub struct TunnelStatus {
    /// The cloudflared in use and the latest release last seen.
    pub version: Option<String>,
    pub latest: Option<String>,
    /// UTC Unix seconds of the last release check.
    pub checked_at: Option<u64>,
    /// Whether cloudflared runs and has registered the tunnel.
    pub connected: bool,
    /// A quick tunnel's address, once Cloudflare gave it.
    pub url: Option<String>,
    pub error: Option<String>,
    /// `FARM_CLOUDFLARED` names the binary; it is not updated.
    pub unmanaged: bool,
}

struct Release {
    version: String,
    url: String,
    sha256: String,
    archive: bool,
}

/// The release asset for this machine: (name, whether it is a .tgz).
fn asset() -> Result<(String, bool)> {
    let arch = match std::env::consts::ARCH {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        other => bail!("cloudflared has no release for {other}"),
    };
    Ok(match std::env::consts::OS {
        "macos" => (format!("cloudflared-darwin-{arch}.tgz"), true),
        "linux" => (format!("cloudflared-linux-{arch}"), false),
        other => bail!("cloudflared is not managed on {other}"),
    })
}

/// `2026.10.7` as numbers, so releases compare in order.
fn version_key(version: &str) -> Vec<u64> {
    version
        .split('.')
        .map(|part| part.parse().unwrap_or(0))
        .collect()
}

fn client() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .user_agent(concat!("farm/", env!("CARGO_PKG_VERSION")))
        .build()?)
}

async fn release(pin: Option<&str>) -> Result<Release> {
    #[derive(Deserialize)]
    struct Asset {
        name: String,
        browser_download_url: String,
        #[serde(default)]
        digest: Option<String>,
    }
    #[derive(Deserialize)]
    struct Body {
        tag_name: String,
        assets: Vec<Asset>,
    }
    let url = match pin {
        Some(version) => format!("{RELEASES}/tags/{version}"),
        None => format!("{RELEASES}/latest"),
    };
    let response = client()?
        .get(&url)
        .header("Accept", "application/vnd.github+json")
        .timeout(Duration::from_secs(20))
        .send()
        .await
        .with_context(|| format!("fetch {url}"))?
        .error_for_status()?;
    let body: Body = serde_json::from_slice(&response.bytes().await?)?;
    let (name, archive) = asset()?;
    let asset = body
        .assets
        .into_iter()
        .find(|a| a.name == name)
        .with_context(|| format!("cloudflared {} has no {name}", body.tag_name))?;
    let sha256 = asset
        .digest
        .as_deref()
        .and_then(|d| d.strip_prefix("sha256:"))
        .filter(|d| d.len() == 64 && d.bytes().all(|b| b.is_ascii_hexdigit()))
        .with_context(|| format!("GitHub publishes no SHA-256 for {name}"))?
        .to_ascii_lowercase();
    ensure!(
        body.tag_name
            .bytes()
            .all(|b| b.is_ascii_digit() || b == b'.'),
        "unexpected cloudflared version '{}'",
        body.tag_name
    );
    Ok(Release {
        version: body.tag_name,
        url: asset.browser_download_url,
        sha256,
        archive,
    })
}

/// cloudflared binaries under `<state dir>/bin/`: `cloudflared-<version>`, the current one
/// linked as `cloudflared`, and the one before it kept to go back to.
pub struct Installer {
    dir: PathBuf,
}

impl Installer {
    pub fn new(state_dir: &Path) -> Self {
        Self {
            dir: state_dir.join("bin"),
        }
    }
    fn unmanaged() -> Option<PathBuf> {
        std::env::var_os("FARM_CLOUDFLARED")
            .filter(|p| !p.is_empty())
            .map(PathBuf::from)
    }
    /// The installed version, if any.
    pub fn installed(&self) -> Option<String> {
        let target = std::fs::read_link(self.dir.join("cloudflared")).ok()?;
        let name = target.file_name()?.to_str()?;
        Some(name.strip_prefix("cloudflared-")?.to_owned())
    }
    /// The binary to run, installing the latest release first if there is none.
    pub async fn ensure(&self) -> Result<PathBuf> {
        if let Some(path) = Self::unmanaged() {
            return Ok(path);
        }
        if self.installed().is_none() {
            self.update().await?;
        }
        Ok(self.dir.join("cloudflared"))
    }
    /// Install the latest (or pinned) release when it is newer than the installed one.
    /// Returns the installed version and whether it changed.
    pub async fn update(&self) -> Result<(String, bool)> {
        ensure!(Self::unmanaged().is_none(), "FARM_CLOUDFLARED is set");
        let pin = std::env::var("FARM_CLOUDFLARED_VERSION")
            .ok()
            .filter(|v| !v.is_empty());
        let release = release(pin.as_deref()).await?;
        if let Some(installed) = self.installed()
            && (installed == release.version
                || (pin.is_none() && version_key(&installed) > version_key(&release.version)))
        {
            return Ok((installed, false));
        }
        self.install(&release).await?;
        Ok((release.version, true))
    }

    async fn install(&self, release: &Release) -> Result<()> {
        std::fs::create_dir_all(&self.dir)?;
        let bytes = client()?
            .get(&release.url)
            .timeout(Duration::from_secs(300))
            .send()
            .await
            .with_context(|| format!("download {}", release.url))?
            .error_for_status()?
            .bytes()
            .await?;
        let digest: String = Sha256::digest(&bytes)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        ensure!(
            digest == release.sha256,
            "cloudflared {} does not match the SHA-256 GitHub publishes for it",
            release.version
        );
        let binary = if release.archive {
            extract(&bytes)?
        } else {
            bytes.to_vec()
        };
        let name = format!("cloudflared-{}", release.version);
        let staging = self.dir.join(format!(".{name}-{}", uuid::Uuid::new_v4()));
        let result = async {
            std::fs::write(&staging, &binary)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&staging, std::fs::Permissions::from_mode(0o755))?;
            }
            verify_signature(&staging).await?;
            let output = tokio::process::Command::new(&staging)
                .arg("--version")
                .output()
                .await
                .context("run the downloaded cloudflared")?;
            let reported = String::from_utf8_lossy(&output.stdout);
            ensure!(
                reported.contains(&format!("version {}", release.version)),
                "the downloaded cloudflared reports '{}'",
                reported.trim()
            );
            std::fs::rename(&staging, self.dir.join(&name))?;
            Ok::<_, anyhow::Error>(())
        }
        .await;
        if result.is_err() {
            let _ = std::fs::remove_file(&staging);
        }
        result?;
        let previous = self.installed();
        // Swap the link atomically, so a running start never sees it missing.
        let link = self
            .dir
            .join(format!(".cloudflared-{}", uuid::Uuid::new_v4()));
        #[cfg(unix)]
        std::os::unix::fs::symlink(&name, &link)?;
        std::fs::rename(&link, self.dir.join("cloudflared"))?;
        // Keep the new and the previous version only.
        for entry in std::fs::read_dir(&self.dir)?.flatten() {
            let file = entry.file_name().to_string_lossy().into_owned();
            if let Some(version) = file.strip_prefix("cloudflared-")
                && version != release.version
                && Some(version) != previous.as_deref()
            {
                let _ = std::fs::remove_file(entry.path());
            }
        }
        eprintln!("cloudflared {} installed", release.version);
        Ok(())
    }
}

fn extract(archive: &[u8]) -> Result<Vec<u8>> {
    use std::io::Read;
    let mut tar = tar::Archive::new(flate2::read::GzDecoder::new(archive));
    for entry in tar.entries()? {
        let mut entry = entry?;
        if entry.path()?.file_name() == Some(std::ffi::OsStr::new("cloudflared")) {
            let mut binary = Vec::new();
            entry.read_to_end(&mut binary)?;
            return Ok(binary);
        }
    }
    bail!("the cloudflared archive holds no cloudflared")
}

/// On macOS, the binary must carry Cloudflare's Developer ID signature: this holds even if
/// the GitHub release and its digests were replaced.
async fn verify_signature(path: &Path) -> Result<()> {
    if !cfg!(target_os = "macos") {
        return Ok(());
    }
    let verify = tokio::process::Command::new("codesign")
        .args(["--verify", "--strict"])
        .arg(path)
        .output()
        .await
        .context("run codesign")?;
    ensure!(
        verify.status.success(),
        "cloudflared's code signature is invalid: {}",
        String::from_utf8_lossy(&verify.stderr).trim()
    );
    let details = tokio::process::Command::new("codesign")
        .args(["-dv", "--verbose=2"])
        .arg(path)
        .output()
        .await?;
    let details = String::from_utf8_lossy(&details.stderr);
    ensure!(
        details
            .lines()
            .any(|line| line == format!("TeamIdentifier={TEAM_ID}"))
            && details
                .lines()
                .any(|line| line.starts_with("Authority=Developer ID Application: Cloudflare")),
        "cloudflared is not signed by Cloudflare"
    );
    Ok(())
}

/// The quick tunnel's address in a cloudflared log line.
fn quick_url(line: &str) -> Option<String> {
    let start = line.find("https://")?;
    let url: String = line[start..]
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || matches!(c, ':' | '/' | '.' | '-'))
        .collect();
    let host = url.strip_prefix("https://")?;
    (host.ends_with(".trycloudflare.com") && host != "api.trycloudflare.com").then_some(url)
}

/// A running tunnel to `origin`. Dropping it stops cloudflared.
pub struct Tunnel {
    status: Arc<Mutex<TunnelStatus>>,
    restart: Arc<Notify>,
    task: tokio::task::JoinHandle<()>,
    updater: tokio::task::JoinHandle<()>,
}

impl Drop for Tunnel {
    fn drop(&mut self) {
        self.task.abort();
        self.updater.abort();
    }
}

impl Tunnel {
    /// Run cloudflared for `origin` (`http://127.0.0.1:10012`). `on_url` hears each new quick
    /// tunnel address.
    pub fn start(
        state_dir: &Path,
        origin: String,
        on_url: Arc<dyn Fn(String) + Send + Sync>,
    ) -> Self {
        let installer = Arc::new(Installer::new(state_dir));
        let status = Arc::new(Mutex::new(TunnelStatus {
            version: installer.installed(),
            unmanaged: Installer::unmanaged().is_some(),
            ..Default::default()
        }));
        let restart = Arc::new(Notify::new());
        let log = state_dir.join("cloudflared.log");
        let task = tokio::spawn(supervise(
            installer.clone(),
            origin,
            status.clone(),
            restart.clone(),
            on_url,
            log,
        ));
        let updater = tokio::spawn({
            let (status, restart) = (status.clone(), restart.clone());
            async move {
                if Installer::unmanaged().is_some() {
                    return;
                }
                loop {
                    tokio::time::sleep(CHECK_EVERY).await;
                    check(&installer, &status, &restart).await;
                }
            }
        });
        Self {
            status,
            restart,
            task,
            updater,
        }
    }
    pub fn status(&self) -> TunnelStatus {
        self.status.lock().unwrap().clone()
    }
    /// Check for a new release now, restarting the tunnel on one.
    pub async fn update(&self, state_dir: &Path) -> Result<TunnelStatus> {
        let installer = Installer::new(state_dir);
        check(&installer, &self.status, &self.restart).await;
        let status = self.status();
        match &status.error {
            Some(error) if status.latest.is_none() => Err(anyhow!("{error}")),
            _ => Ok(status),
        }
    }
}

async fn check(installer: &Installer, status: &Mutex<TunnelStatus>, restart: &Notify) {
    let result = installer.update().await;
    let mut s = status.lock().unwrap();
    s.checked_at = Some(crate::model::now());
    match result {
        Ok((version, changed)) => {
            s.latest = Some(version.clone());
            if changed {
                s.version = Some(version);
                restart.notify_one();
            }
        }
        // The installed cloudflared keeps running.
        Err(error) => s.error = Some(format!("cloudflared update: {error:#}")),
    }
}

/// Stop the cloudflared a coordinator that died without stopping it left, recorded in
/// `pid_file` with the binary it ran. Another process that took its pid is left alone.
async fn reap(pid_file: &Path, binary: &Path) {
    let Ok(record) = std::fs::read_to_string(pid_file) else {
        return;
    };
    let _ = std::fs::remove_file(pid_file);
    let mut lines = record.lines();
    let (Some(Ok(pid)), Some(path)) = (lines.next().map(str::parse::<i32>), lines.next()) else {
        return;
    };
    if pid <= 1 || path != binary.display().to_string() {
        return;
    }
    let ours = || async {
        tokio::process::Command::new("ps")
            .args(["-p", &pid.to_string(), "-o", "command="])
            .output()
            .await
            .ok()
            .is_some_and(|o| {
                let command = String::from_utf8_lossy(&o.stdout);
                command.starts_with(path) && command.contains(" tunnel ")
            })
    };
    if !ours().await {
        return;
    }
    eprintln!("relay tunnel: stopping cloudflared {pid} left by an earlier coordinator");
    #[cfg(unix)]
    // SAFETY: signals a process verified above to be the cloudflared this coordinator ran.
    unsafe {
        libc::kill(pid, libc::SIGTERM);
    }
    for _ in 0..50 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        if !ours().await {
            return;
        }
    }
    #[cfg(unix)]
    // SAFETY: as above; it ignored SIGTERM for 5 s.
    unsafe {
        libc::kill(pid, libc::SIGKILL);
    }
}

async fn supervise(
    installer: Arc<Installer>,
    origin: String,
    status: Arc<Mutex<TunnelStatus>>,
    restart: Arc<Notify>,
    on_url: Arc<dyn Fn(String) + Send + Sync>,
    log: PathBuf,
) {
    let mut first = true;
    let mut delay = Duration::from_secs(2);
    loop {
        let result = async {
            let binary = installer.ensure().await?;
            if first && Installer::unmanaged().is_none() {
                first = false;
                // A tunnel that starts checks for a new release at once.
                check(&installer, &status, &restart).await;
                if let Some(version) = installer.installed() {
                    status.lock().unwrap().version = Some(version);
                }
            }
            let pid_file = log.with_file_name("cloudflared.pid");
            reap(&pid_file, &binary).await;
            let mut command = tokio::process::Command::new(&binary);
            command.args(["tunnel", "--no-autoupdate", "--url", &origin]);
            #[cfg(target_os = "linux")]
            // SAFETY: prctl is async-signal-safe and touches no memory of the parent. The
            // signal comes when the spawning thread ends; tokio's workers live as long as
            // the runtime.
            unsafe {
                command.pre_exec(|| {
                    if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM) == -1 {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
            let mut child = command
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::piped())
                .kill_on_drop(true)
                .spawn()
                .with_context(|| format!("start {}", binary.display()))?;
            if let Some(pid) = child.id() {
                let _ = std::fs::write(&pid_file, format!("{pid}\n{}\n", binary.display()));
            }
            let stderr = child.stderr.take().context("no cloudflared stderr")?;
            let mut lines = BufReader::new(stderr).lines();
            let mut found: Option<String> = None;
            let mut file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&log)?;
            loop {
                tokio::select! {
                    line = lines.next_line() => {
                        let Some(line) = line? else { break };
                        use std::io::Write;
                        let _ = writeln!(file, "{line}");
                        if let Some(url) = quick_url(&line) {
                            found = Some(url);
                        }
                        if line.contains("Registered tunnel connection") {
                            {
                                let mut s = status.lock().unwrap();
                                s.connected = true;
                                s.error = None;
                            }
                            // Announce a quick tunnel's address once, after its name resolves.
                            if let Some(url) = found.take() {
                                let (status, on_url) = (status.clone(), on_url.clone());
                                tokio::spawn(async move {
                                    tokio::time::sleep(DNS_SETTLE).await;
                                    status.lock().unwrap().url = Some(url.clone());
                                    on_url(url);
                                });
                            }
                        }
                    }
                    _ = restart.notified() => {
                        let _ = child.kill().await;
                        let _ = std::fs::remove_file(&pid_file);
                        return Ok(());
                    }
                }
            }
            let exit = child.wait().await?;
            let _ = std::fs::remove_file(&pid_file);
            bail!("cloudflared exited ({exit}); see {}", log.display())
        }
        .await;
        {
            let mut s = status.lock().unwrap();
            s.connected = false;
            s.url = None;
            match result {
                Ok(()) => delay = Duration::from_secs(2),
                Err(error) => {
                    eprintln!("relay tunnel: {error:#}");
                    s.error = Some(format!("{error:#}"));
                }
            }
        }
        tokio::time::sleep(delay).await;
        delay = (delay * 2).min(Duration::from_secs(60));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quick_tunnel_addresses_and_versions_are_read() {
        assert_eq!(
            quick_url("INF |  https://calm-river-1234.trycloudflare.com                  |")
                .as_deref(),
            Some("https://calm-river-1234.trycloudflare.com")
        );
        assert_eq!(
            quick_url("INF Requesting new quick Tunnel on trycloudflare.com..."),
            None
        );
        assert_eq!(quick_url("see https://api.trycloudflare.com/tunnel"), None);
        assert_eq!(quick_url("https://example.com"), None);
        assert!(version_key("2026.10.7") > version_key("2026.9.12"));
        assert!(version_key("2026.10.10") > version_key("2026.10.7"));
        let (name, _) = asset().unwrap();
        assert!(name.starts_with("cloudflared-"));
    }

    /// A cloudflared left by a coordinator that died is stopped; a process that only reuses
    /// its pid, or runs another binary, is not.
    #[tokio::test]
    async fn a_left_cloudflared_is_reaped_and_nothing_else() {
        let dir = tempfile::tempdir().unwrap();
        let binary = dir.path().join("cloudflared");
        std::os::unix::fs::symlink("/bin/sh", &binary).unwrap();
        let spawn = |program: &Path| {
            std::process::Command::new(program)
                .args(["-c", "sleep 30; true", "sh", "tunnel", "--no-autoupdate"])
                .spawn()
                .unwrap()
        };
        let pid_file = dir.path().join("cloudflared.pid");
        let alive = |pid: u32| unsafe { libc::kill(pid as i32, 0) == 0 };
        // Another binary under the recorded pid stays.
        let mut other = spawn(Path::new("/bin/sh"));
        std::fs::write(&pid_file, format!("{}\n{}\n", other.id(), binary.display())).unwrap();
        reap(&pid_file, &binary).await;
        assert!(alive(other.id()));
        other.kill().unwrap();
        other.wait().unwrap();
        // The recorded cloudflared is stopped and its record removed.
        let mut left = spawn(&binary);
        tokio::time::sleep(Duration::from_millis(200)).await;
        std::fs::write(&pid_file, format!("{}\n{}\n", left.id(), binary.display())).unwrap();
        reap(&pid_file, &binary).await;
        let status = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Some(status) = left.try_wait().unwrap() {
                    return status;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .unwrap();
        assert!(!status.success());
        assert!(!pid_file.exists());
    }

    /// Fetches and verifies the latest cloudflared, then opens a quick tunnel to a local
    /// server and reaches it through Cloudflare. Needs the network: `cargo test -- --ignored`.
    #[tokio::test]
    #[ignore]
    async fn a_quick_tunnel_reaches_a_local_server() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let dir = tempfile::tempdir().unwrap();
        let installer = Installer::new(dir.path());
        let (version, changed) = installer.update().await.unwrap();
        assert!(changed);
        assert_eq!(installer.installed().as_deref(), Some(version.as_str()));
        assert_eq!(installer.update().await.unwrap(), (version, false));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(
            axum::serve(
                listener,
                axum::Router::new().route("/ping", axum::routing::get(|| async { "pong" })),
            )
            .into_future(),
        );
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let tunnel = Tunnel::start(
            dir.path(),
            origin,
            Arc::new(move |url| {
                let _ = tx.send(url);
            }),
        );
        let url = tokio::time::timeout(Duration::from_secs(60), rx.recv())
            .await
            .unwrap()
            .unwrap();
        // The new hostname takes a moment to resolve.
        let mut body = String::new();
        for _ in 0..30 {
            match reqwest::get(format!("{url}/ping")).await {
                Ok(response) if response.status().is_success() => {
                    body = response.text().await.unwrap();
                    break;
                }
                Ok(response) => eprintln!("status {}", response.status()),
                Err(error) => eprintln!("get: {error:?}"),
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
        assert_eq!(body, "pong", "{:?}", tunnel.status());
        assert!(tunnel.status().connected);
    }
}
