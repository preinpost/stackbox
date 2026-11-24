//! The guest kernel farms boot from.
//!
//! microsandbox ships a libkrunfw whose kernel has neither VFIO nor mdev, so the kernel's
//! `mtty` mediated device cannot be bound as a vGPU. farm therefore boots every VM from a
//! libkrunfw built from the fragments in `scripts/libkrunfw/fragments` (see
//! `docs/libkrunfw.md`): preparing a host installs it under
//! `~/.local/share/farm/libkrunfw/<id>/`, and the coordinator and every host agent hand the
//! pair `msb` + that library to the SDK by themselves. The microsandbox install at
//! `~/.microsandbox` is never written.
//!
//! A process started with [`LIBKRUNFW_VAR`] keeps booting the library it names, which is how
//! a kernel built somewhere other than the repository is tried. A kernel an earlier msb
//! could load is not used: the manifest records the msb it was built for, and a mismatch
//! leaves the host on the stock kernel until preparation upgrades msb.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use sha2::{Digest, Sha256};

/// Environment variable naming the libkrunfw library this process boots from, overriding the
/// installed kernel. Hosts prepared by farm get their own installation instead.
pub const LIBKRUNFW_VAR: &str = "FARM_LIBKRUNFW_PATH";

/// Directory holding built kernels, laid out as `scripts/build-libkrunfw.sh` writes them:
/// `<dir>/<arch>/manifest.txt` with the artifacts beside it. Defaults to the build output
/// above the running binary (`target/libkrunfw` for `target/<profile>/farm`).
pub const ARTIFACTS_VAR: &str = "FARM_LIBKRUNFW_DIR";

/// Where a host keeps installed kernels, under its home directory. One directory per kernel
/// id, a hash of the manifest, so two builds never share an installation.
pub const INSTALL_DIR: &str = ".local/share/farm/libkrunfw";

/// Where this process's kernel came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KernelSource {
    /// [`LIBKRUNFW_VAR`] named it.
    Override,
    /// Host preparation installed it under [`INSTALL_DIR`].
    Installed,
    /// The stock microsandbox kernel: no mtty mediated device.
    Stock,
}

/// The kernel this process boots with, and why it is not the installed one when it is not.
#[derive(Clone, Debug)]
pub struct Selection {
    pub source: KernelSource,
    pub path: Option<PathBuf>,
    /// The installation's manifest hash, for an installed kernel.
    pub id: Option<String>,
    /// Why an installed kernel was found but not used. Absent when none is installed.
    pub reason: Option<String>,
}

/// One architecture's manifest, as `scripts/build-libkrunfw.sh` writes it.
#[derive(Clone, Debug)]
pub struct Manifest {
    pub text: String,
    /// The msb version the kernel was built for; `None` for `msb none`.
    pub msb: Option<String>,
    pub revision: Option<String>,
    pub arch: String,
    /// `(relative path, sha256)` for each linked library.
    pub artifacts: Vec<(String, String)>,
}

impl Manifest {
    pub fn parse(text: &str) -> Result<Self> {
        let mut msb = None;
        let mut revision = None;
        let mut arch = None;
        let mut artifacts = Vec::new();
        for line in text.lines() {
            let mut fields = line.split_whitespace();
            match fields.next() {
                Some("msb") => {
                    msb = fields.next().map(str::to_owned).filter(|v| v != "none");
                }
                Some("revision") => revision = fields.next().map(str::to_owned),
                Some("arch") => arch = fields.next().map(str::to_owned),
                Some("artifact") => {
                    let path = fields.next();
                    let kind = fields.next();
                    let sha256 = fields.next();
                    if let (Some(path), Some("sha256"), Some(sha256)) = (path, kind, sha256) {
                        artifacts.push((path.to_owned(), sha256.to_ascii_lowercase()));
                    }
                }
                _ => {}
            }
        }
        Ok(Self {
            text: text.to_owned(),
            msb,
            revision,
            arch: arch.context("the kernel manifest states no architecture")?,
            artifacts,
        })
    }

    /// The library for `os`, as the relative path the build wrote.
    fn artifact_for(&self, os: &str) -> Option<(&str, &str)> {
        self.artifacts
            .iter()
            .find(|(path, _)| match os {
                "macos" => path.ends_with(".dylib"),
                _ => path
                    .rsplit('/')
                    .next()
                    .is_some_and(|name| name.starts_with("libkrunfw.so.")),
            })
            .map(|(path, sha)| (path.as_str(), sha.as_str()))
    }
}

/// A kernel this build can install on a host, with its manifest and verified library.
#[derive(Clone, Debug)]
pub struct Artifact {
    /// Hash of the manifest text: the installation's directory name.
    pub id: String,
    pub manifest: String,
    pub library: PathBuf,
    pub filename: String,
    pub sha256: String,
    pub msb: String,
    pub arch: String,
}

/// The directory a host's installed kernels live in.
pub fn install_root(home: &Path) -> PathBuf {
    home.join(INSTALL_DIR)
}

/// This machine's home directory.
pub fn home() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|path| !path.as_os_str().is_empty())
}

/// A kernel id no directory traversal can escape: the manifest's hash is 16 hex digits.
pub fn validate_id(id: &str) -> Result<()> {
    ensure!(
        id.len() == 16
            && id
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
        "invalid farm kernel id"
    );
    Ok(())
}

/// A library file name that stays inside its installation directory.
pub fn validate_filename(name: &str) -> Result<()> {
    ensure!(
        !name.is_empty()
            && name.len() <= 128
            && !name.starts_with('.')
            && name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-' || b == b'_'),
        "invalid farm kernel library name"
    );
    Ok(())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn sha256_file(path: &Path) -> Result<String> {
    let mut file = std::fs::File::open(path)
        .with_context(|| format!("read the farm kernel at {}", path.display()))?;
    let mut hash = Sha256::new();
    std::io::copy(&mut file, &mut hash)?;
    Ok(hex(&hash.finalize()))
}

/// The id of a build: the manifest hash. A rebuild with other fragments or a revision gets
/// another installation instead of replacing one a running host may still boot.
pub fn id_for(manifest: &str) -> String {
    let digest = Sha256::digest(manifest.as_bytes());
    hex(&digest[..8])
}

/// The build root for this binary: [`ARTIFACTS_VAR`], else a `libkrunfw` directory above the
/// executable (`target/libkrunfw` for `target/<profile>/farm`).
pub fn artifacts_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os(ARTIFACTS_VAR).filter(|dir| !dir.is_empty()) {
        return Some(PathBuf::from(dir));
    }
    let mut dir = std::env::current_exe().ok()?.parent()?.to_path_buf();
    for _ in 0..3 {
        let candidate = dir.join("libkrunfw");
        if candidate.is_dir() {
            return Some(candidate);
        }
        dir = dir.parent()?.to_path_buf();
    }
    None
}

/// The artifact for `os` and `arch` under a build root, or `None` when no kernel was built
/// for it. A manifest that names another msb is refused: the library may not load there.
pub fn artifact_for(dir: &Path, os: &str, arch: &str) -> Result<Option<Artifact>> {
    let base = dir.join(arch);
    let manifest_path = base.join("manifest.txt");
    let text = match std::fs::read_to_string(&manifest_path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| format!("read {}", manifest_path.display()));
        }
    };
    let manifest =
        Manifest::parse(&text).with_context(|| format!("read {}", manifest_path.display()))?;
    ensure!(
        manifest.arch == arch,
        "{} states architecture {}, not {arch}",
        manifest_path.display(),
        manifest.arch
    );
    let Some(msb) = manifest.msb.clone() else {
        bail!(
            "{} does not state the msb it was built for; rebuild it with scripts/build-libkrunfw.sh (pass --msb VERSION if it was built with --revision)",
            manifest_path.display()
        );
    };
    ensure!(
        msb == crate::remote::MSB_VERSION,
        "the kernel in {} was built for msb {msb}, and this farm runs msb {}; rebuild it against this msb or keep it as {LIBKRUNFW_VAR}",
        base.display(),
        crate::remote::MSB_VERSION
    );
    let (relative, sha256) = manifest
        .artifact_for(os)
        .with_context(|| format!("{} has no library for {os}", manifest_path.display()))?;
    let filename = relative.rsplit('/').next().unwrap_or(relative).to_owned();
    validate_filename(&filename)?;
    let library = base.join(&filename);
    ensure!(
        library.is_file(),
        "the kernel manifest names {}, which is missing; rebuild it",
        library.display()
    );
    let actual = sha256_file(&library)?;
    ensure!(
        actual == sha256,
        "{} has SHA-256 {actual}, and its manifest says {sha256}; rebuild it",
        library.display()
    );
    Ok(Some(Artifact {
        id: id_for(&text),
        manifest: text,
        library,
        filename,
        sha256: sha256.to_owned(),
        msb,
        arch: arch.to_owned(),
    }))
}

/// Lock the installation directory while it changes; another request may prepare the same
/// host at the same time.
fn lock(path: &Path) -> Result<std::fs::File> {
    use fs2::FileExt;
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path)
        .with_context(|| format!("open {}", path.display()))?;
    file.lock_exclusive()?;
    Ok(file)
}

/// Install a kernel for this host under `home`, replacing kernels of other builds.
pub fn install(artifact: &Artifact, home: &Path) -> Result<PathBuf> {
    install_into(artifact, &install_root(home))
}

/// Install into an existing kernels directory. The library is published before the
/// manifest, so an interrupted install is never selected.
fn install_into(artifact: &Artifact, root: &Path) -> Result<PathBuf> {
    validate_id(&artifact.id)?;
    validate_filename(&artifact.filename)?;
    std::fs::create_dir_all(root)?;
    let _lock = lock(&root.join(".install.lock"))?;
    let dir = root.join(&artifact.id);
    std::fs::create_dir_all(&dir)?;
    let library = dir.join(&artifact.filename);
    let current =
        library.is_file() && sha256_file(&library).is_ok_and(|sha| sha == artifact.sha256);
    if !current {
        let staging = dir.join(format!(".{}.partial", artifact.filename));
        std::fs::copy(&artifact.library, &staging)
            .with_context(|| format!("copy {}", artifact.library.display()))?;
        ensure!(
            sha256_file(&staging)? == artifact.sha256,
            "the copy of {} does not match its manifest",
            artifact.library.display()
        );
        std::fs::rename(&staging, &library)?;
    }
    let staging = dir.join(".manifest.txt.partial");
    std::fs::write(&staging, &artifact.manifest)?;
    std::fs::rename(&staging, dir.join("manifest.txt"))?;
    // One installation is one answer for the host: kernels of other builds would otherwise
    // compete for the same boot, and a host that reinstalls should not grow forever. The one
    // this install replaces stays, because a VM booting from it may be opening its library
    // right now; the next install removes it, so a host keeps at most two.
    let mut superseded: Vec<(std::time::SystemTime, PathBuf)> = std::fs::read_dir(root)?
        .flatten()
        .filter(|entry| entry.path().is_dir() && entry.file_name() != artifact.id.as_str())
        .filter(|entry| entry.path().join("manifest.txt").is_file())
        .map(|entry| (installed_at(&entry.path()), entry.path()))
        .collect();
    superseded.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
    for (_, path) in superseded.into_iter().rev().skip(1) {
        std::fs::remove_dir_all(path)?;
    }
    Ok(library)
}

/// When an installation last changed. An install keeps the kernel it replaced, so the
/// manifest's time is what orders the two; [`choose`] takes the newest.
fn installed_at(dir: &Path) -> std::time::SystemTime {
    std::fs::metadata(dir.join("manifest.txt"))
        .and_then(|meta| meta.modified())
        .unwrap_or(std::time::SystemTime::UNIX_EPOCH)
}

/// The msb version an executable reports, as inspection reads it.
fn msb_version_at(path: &Path) -> Option<String> {
    let output = std::process::Command::new(path)
        .arg("--version")
        .stdin(std::process::Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8_lossy(&output.stdout)
        .split_whitespace()
        .last()
        .filter(|v| {
            !v.is_empty() && v.len() < 32 && v.bytes().all(|b| b.is_ascii_digit() || b == b'.')
        })
        .map(str::to_owned)
}

/// Choose a kernel from an override, an installation under `root`, or the stock kernel.
/// `msb_version` is what the host actually runs: a kernel built for another msb is refused.
pub fn choose(
    override_path: Option<&Path>,
    root: &Path,
    os: &str,
    arch: &str,
    msb_version: Option<&str>,
) -> Selection {
    if let Some(path) = override_path {
        return Selection {
            source: KernelSource::Override,
            path: Some(path.to_owned()),
            id: None,
            reason: None,
        };
    }
    let mut reason = None;
    let mut installed: Vec<PathBuf> = std::fs::read_dir(root)
        .map(|entries| entries.flatten().map(|entry| entry.path()).collect())
        .unwrap_or_default();
    // The newest installation wins: an install keeps the one it replaced, so a host can
    // briefly hold two while a VM may still be booting from the older library.
    installed.sort_by(|a, b| installed_at(a).cmp(&installed_at(b)).then_with(|| a.cmp(b)));
    for dir in installed.into_iter().rev() {
        let manifest_path = dir.join("manifest.txt");
        let Ok(text) = std::fs::read_to_string(&manifest_path) else {
            continue;
        };
        let manifest = match Manifest::parse(&text) {
            Ok(manifest) => manifest,
            Err(error) => {
                reason = Some(format!(
                    "The farm kernel in {} has an unreadable manifest: {error}",
                    dir.display()
                ));
                continue;
            }
        };
        if manifest.arch != arch {
            reason = Some(format!(
                "The farm kernel in {} is built for {}, and this host is {arch}",
                dir.display(),
                manifest.arch
            ));
            continue;
        }
        if manifest.msb.as_deref() != msb_version {
            reason = Some(format!(
                "The farm kernel in {} was built for msb {}, and this host runs {}",
                dir.display(),
                manifest.msb.as_deref().unwrap_or("no msb"),
                msb_version.unwrap_or("no msb")
            ));
            continue;
        }
        let Some((relative, _)) = manifest.artifact_for(os) else {
            reason = Some(format!(
                "The farm kernel in {} has no {os} library",
                dir.display()
            ));
            continue;
        };
        let filename = relative.rsplit('/').next().unwrap_or(relative);
        let library = dir.join(filename);
        if !library.is_file() {
            reason = Some(format!(
                "The farm kernel in {} names {}, which is missing; prepare the host again",
                dir.display(),
                library.display()
            ));
            continue;
        }
        return Selection {
            source: KernelSource::Installed,
            path: Some(library),
            id: dir
                .file_name()
                .map(|name| name.to_string_lossy().into_owned()),
            reason: None,
        };
    }
    Selection {
        source: KernelSource::Stock,
        path: None,
        id: None,
        reason,
    }
}

/// The kernel this process boots from, before the SDK resolves its runtime.
pub fn select_in_process() -> Selection {
    let root = home().map(|home| install_root(&home)).unwrap_or_default();
    let msb = msb_version_at(&msb_path());
    choose(
        libkrunfw_path().as_deref(),
        &root,
        std::env::consts::OS,
        std::env::consts::ARCH,
        msb.as_deref(),
    )
}

/// Environment variable value naming the kernel this process was started with, if any.
pub fn libkrunfw_path() -> Option<PathBuf> {
    std::env::var_os(LIBKRUNFW_VAR)
        .map(PathBuf::from)
        .filter(|path| !path.as_os_str().is_empty())
}

/// The msb executable microsandbox drives: `MSB_PATH`, else the install under `~/.microsandbox`.
pub fn msb_path() -> PathBuf {
    std::env::var_os("MSB_PATH")
        .map(PathBuf::from)
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| {
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .unwrap_or_default()
                .join(".microsandbox")
                .join("bin")
                .join("msb")
        })
}

/// Point the microsandbox SDK at the configured kernel. Runs once per process, before the
/// SDK resolves its runtime: the SDK keeps the first value it is given, and its own
/// `MSB_LIBKRUNFW_PATH`/`MSB_PATH` environment variables still take precedence.
pub fn configure_sdk() -> Result<Option<PathBuf>> {
    let selection = select_in_process();
    if let Some(reason) = &selection.reason {
        eprintln!(
            "farm: {reason}; booting the stock microsandbox kernel, so mtty GPUs and mdev devices are unavailable"
        );
    }
    let Some(library) = selection.path else {
        return Ok(None);
    };
    ensure!(
        library.is_file(),
        "{}",
        match selection.source {
            KernelSource::Override => format!(
                "{LIBKRUNFW_VAR} names {}, which is not a file; build the kernel (docs/libkrunfw.md) or unset it",
                library.display()
            ),
            _ => format!(
                "the installed farm kernel {} is missing; prepare the host again (docs/libkrunfw.md)",
                library.display()
            ),
        }
    );
    let msb = msb_path();
    ensure!(
        msb.is_file(),
        "a farm kernel needs an msb executable beside it; {} is not a file (set MSB_PATH)",
        msb.display()
    );
    microsandbox::config::set_sdk_msb_path(&msb);
    microsandbox::config::set_sdk_libkrunfw_path(&library);
    Ok(Some(library))
}

/// The artifact id an installed kernel's library path names, as in
/// `~/.local/share/farm/libkrunfw/<id>/libkrunfw.so.5.6.1`. A kernel kept anywhere else,
/// such as an override under `~/.microsandbox/lib`, has no installation id.
pub fn installed_id(library: &Path) -> Option<String> {
    let dir = library.parent()?;
    if dir.parent()?.file_name()? != "libkrunfw" {
        return None;
    }
    Some(dir.file_name()?.to_str()?.to_owned())
}

/// Whether installing msb on a host would write over the kernel at `library`.
///
/// The installer only touches `~/.microsandbox/bin` and `~/.microsandbox/lib`; a kernel kept
/// anywhere else survives an msb install. Farm's own installations live in
/// `~/.local/share/farm/libkrunfw`, but [`LIBKRUNFW_VAR`] may still point into the install
/// directory. The host's home is not part of its inspection, so any `.microsandbox/lib` path
/// counts: it belongs to some host's install.
pub fn install_would_replace(library: &Path) -> bool {
    let parts: Vec<&str> = library
        .components()
        .filter_map(|part| match part {
            std::path::Component::Normal(name) => name.to_str(),
            _ => None,
        })
        .collect();
    parts
        .windows(2)
        .any(|pair| pair[0] == ".microsandbox" && pair[1] == "lib")
}

/// Where kernels published by farm are served from: `<kid>/manifest.txt`, the library under
/// `<kid>/<os>-<arch>/`, and per-msb indexes under `msb-<version>/<os>-<arch>.json`.
pub const RELEASE_URL: &str = "https://public-bucket.preinpost.in/farm-libkrunfw";

/// Environment variable overriding [`RELEASE_URL`]. Empty turns the published kernels off.
pub const URL_VAR: &str = "FARM_LIBKRUNFW_URL";

/// The published kernel base URL the environment selects: unset means [`RELEASE_URL`], an
/// empty value means none, anything else is used without trailing slashes.
pub fn release_url() -> Option<String> {
    release_url_from(std::env::var(URL_VAR).ok().as_deref())
}

fn release_url_from(value: Option<&str>) -> Option<String> {
    match value {
        None => Some(RELEASE_URL.into()),
        Some(url) => {
            Some(url.trim().trim_end_matches('/').to_owned()).filter(|url| !url.is_empty())
        }
    }
}

/// The index a published kernel is announced by, as `scripts/libkrunfw-index.sh` writes it.
#[derive(serde::Deserialize)]
struct Index {
    kid: String,
    msb: String,
    os: String,
    arch: String,
    manifest_path: String,
    artifact_path: String,
    sha256: String,
    size: u64,
}

/// The largest library a published kernel may be. A real one is about 25 MiB.
const MAX_PUBLISHED_SIZE: u64 = 128 * 1024 * 1024;

fn lower_hex(value: &str, len: usize) -> bool {
    value.len() == len
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// A path inside the published bucket: relative, no `..` or `.`, printable ASCII only.
fn safe_relative(path: &str) -> Result<()> {
    ensure!(
        !path.is_empty()
            && path.len() <= 512
            && !path.starts_with('/')
            && !path.contains('\\')
            && path.bytes().all(|b| b.is_ascii_graphic())
            && path
                .split('/')
                .all(|part| !part.is_empty() && part != ".." && part != "."),
        "invalid published kernel path {path:?}"
    );
    Ok(())
}

impl Index {
    fn validate(&self, os: &str, arch: &str, msb_version: &str) -> Result<()> {
        ensure!(
            self.msb == msb_version && self.os == os && self.arch == arch,
            "the published kernel index is for {}-{} msb {}, not {os}-{arch} msb {msb_version}",
            self.os,
            self.arch,
            self.msb
        );
        ensure!(lower_hex(&self.kid, 16), "invalid published kernel id");
        ensure!(
            lower_hex(&self.sha256, 64),
            "invalid published kernel sha256"
        );
        ensure!(
            self.size > 0 && self.size <= MAX_PUBLISHED_SIZE,
            "invalid published kernel size {}",
            self.size
        );
        safe_relative(&self.manifest_path)?;
        safe_relative(&self.artifact_path)?;
        let filename = self.artifact_path.rsplit('/').next().unwrap_or_default();
        validate_filename(filename)?;
        Ok(())
    }
}

fn client() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(10))
        .build()?)
}

/// The body at `url`, or `None` when the bucket has no object there (403 or 404).
async fn get_optional(
    url: &str,
    limit: u64,
    timeout: std::time::Duration,
) -> Result<Option<Vec<u8>>> {
    let response = client()?
        .get(url)
        .timeout(timeout)
        .send()
        .await
        .with_context(|| format!("download {url}"))?;
    if matches!(response.status().as_u16(), 403 | 404) {
        return Ok(None);
    }
    let response = response
        .error_for_status()
        .with_context(|| format!("download {url}"))?;
    if response.content_length().is_some_and(|len| len > limit) {
        bail!("{url} is larger than {limit} bytes");
    }
    let bytes = response
        .bytes()
        .await
        .with_context(|| format!("download {url}"))?;
    ensure!(
        (bytes.len() as u64) <= limit,
        "{url} is larger than {limit} bytes"
    );
    Ok(Some(bytes.into()))
}

/// The artifact `os`-`arch` of msb `msb_version` published under `base_url`, verified and
/// cached under `cache_dir/<kid>/`. `os` is the bucket's name for the platform (`linux` or
/// `darwin`). `None` when nothing is published for that build. A cached kernel is reused
/// without downloading its manifest or library again; the index is always read, since it is
/// what names the current kernel.
pub async fn fetch_published(
    base_url: &str,
    os: &str,
    arch: &str,
    msb_version: &str,
    cache_dir: &Path,
) -> Result<Option<Artifact>> {
    let base = base_url.trim_end_matches('/');
    let index_url = format!("{base}/msb-{msb_version}/{os}-{arch}.json");
    let Some(body) =
        get_optional(&index_url, 64 * 1024, std::time::Duration::from_secs(20)).await?
    else {
        return Ok(None);
    };
    let index: Index = serde_json::from_slice(&body)
        .with_context(|| format!("read the published kernel index {index_url}"))?;
    index.validate(os, arch, msb_version)?;
    let filename = index
        .artifact_path
        .rsplit('/')
        .next()
        .unwrap_or_default()
        .to_owned();
    // The manifest's own os name for the library: a dylib on macOS, a .so elsewhere.
    let manifest_os = if os == "darwin" { "macos" } else { os };
    let dir = cache_dir.join(&index.kid);
    let library = dir.join(&filename);
    let manifest_file = dir.join("manifest.txt");
    let cached = std::fs::read_to_string(&manifest_file).ok().filter(|text| {
        published_manifest(text, &index, manifest_os, msb_version).is_ok()
            && sha256_file(&library).is_ok_and(|sha| sha == index.sha256)
    });
    if let Some(text) = cached {
        return Ok(Some(Artifact {
            id: index.kid.clone(),
            manifest: text,
            library,
            filename,
            sha256: index.sha256.clone(),
            msb: msb_version.into(),
            arch: arch.into(),
        }));
    }

    let manifest_url = format!("{base}/{}", index.manifest_path);
    let manifest_bytes = get_optional(
        &manifest_url,
        1024 * 1024,
        std::time::Duration::from_secs(20),
    )
    .await?
    .with_context(|| format!("the published kernel manifest {manifest_url} is missing"))?;
    let text = String::from_utf8(manifest_bytes)
        .with_context(|| format!("{manifest_url} is not UTF-8"))?;
    published_manifest(&text, &index, manifest_os, msb_version)?;
    let artifact_url = format!("{base}/{}", index.artifact_path);
    let bytes = get_optional(
        &artifact_url,
        index.size,
        std::time::Duration::from_secs(300),
    )
    .await?
    .with_context(|| format!("the published kernel {artifact_url} is missing"))?;
    ensure!(
        bytes.len() as u64 == index.size && format!("{:x}", Sha256::digest(&bytes)) == index.sha256,
        "{artifact_url} does not match its published size and sha256"
    );

    std::fs::create_dir_all(&dir)?;
    let partial = dir.join(format!(".{filename}.partial"));
    std::fs::write(&partial, &bytes)?;
    std::fs::rename(&partial, &library)?;
    let partial = dir.join(".manifest.txt.partial");
    std::fs::write(&partial, &text)?;
    std::fs::rename(&partial, &manifest_file)?;
    Ok(Some(Artifact {
        id: index.kid,
        manifest: text,
        library,
        filename,
        sha256: index.sha256,
        msb: msb_version.into(),
        arch: arch.into(),
    }))
}

/// Checks a published manifest against the index that named it: its id, msb, architecture,
/// and that it lists the library the index names, with the index's sha256.
fn published_manifest(
    text: &str,
    index: &Index,
    manifest_os: &str,
    msb_version: &str,
) -> Result<()> {
    ensure!(
        id_for(text) == index.kid,
        "the published manifest does not hash to kernel id {}",
        index.kid
    );
    let manifest = Manifest::parse(text)?;
    ensure!(
        manifest.msb.as_deref() == Some(msb_version),
        "the published kernel was built for msb {:?}, not {msb_version}",
        manifest.msb
    );
    ensure!(
        manifest.arch == index.arch,
        "the published kernel is for {}, not {}",
        manifest.arch,
        index.arch
    );
    let (path, sha256) = manifest
        .artifact_for(manifest_os)
        .with_context(|| format!("the published kernel has no library for {manifest_os}"))?;
    safe_relative(path)?;
    ensure!(
        path.rsplit('/').next() == index.artifact_path.rsplit('/').next() && sha256 == index.sha256,
        "the published manifest does not list the library its index names"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote::MSB_VERSION as MSB;

    fn manifest(msb: &str, arch: &str, artifact: &str, sha256: &str) -> String {
        format!(
            "revision 56bf194627a07996dc4ce21211fe86ada4c76080\nkernel linux-6.12.113\narch {arch}\nkbuild-arch arm64\nmsb {msb}\nfragment scripts/libkrunfw/fragments/vfio-mtty.config sha256 {}\nartifact {artifact} sha256 {sha256}\n",
            "a".repeat(64)
        )
    }

    /// Writes a build directory as scripts/build-libkrunfw.sh leaves one, and describes it
    /// the way a successful `artifact_for` would. Tests may ask for another msb's kernel,
    /// which `artifact_for` itself would refuse.
    fn build(dir: &Path, arch: &str, os: &str, msb: &str, bytes: &[u8]) -> Artifact {
        let base = dir.join(arch);
        std::fs::create_dir_all(&base).unwrap();
        let name = match os {
            "macos" => "libkrunfw.5.dylib",
            _ => "libkrunfw.so.5.6.1",
        };
        std::fs::write(base.join(name), bytes).unwrap();
        let sha = hex(&Sha256::digest(bytes));
        let text = manifest(msb, arch, &format!("{arch}/{name}"), &sha);
        std::fs::write(base.join("manifest.txt"), &text).unwrap();
        let parsed = Manifest::parse(&text).unwrap();
        Artifact {
            id: id_for(&text),
            manifest: text,
            library: base.join(name),
            filename: name.into(),
            sha256: sha,
            msb: parsed.msb.unwrap(),
            arch: arch.into(),
        }
    }

    /// Installs a fake kernel under an installation root, as preparation would on a host.
    fn install_fake(root: &Path, arch: &str, os: &str, msb: &str, salt: &str) -> String {
        let dir = tempfile::tempdir().unwrap();
        let artifact = build(dir.path(), arch, os, msb, salt.as_bytes());
        install_into(&artifact, root).unwrap();
        artifact.id
    }

    #[test]
    fn an_override_wins_over_an_installation_and_stock_is_the_last_resort() {
        let home = tempfile::tempdir().unwrap();
        let root = install_root(home.path());
        std::fs::create_dir_all(&root).unwrap();
        let id = install_fake(&root, "aarch64", "macos", MSB, "kernel");
        let installed = choose(None, &root, "macos", "aarch64", Some(MSB));
        assert_eq!(installed.source, KernelSource::Installed);
        assert_eq!(installed.id.as_deref(), Some(id.as_str()));
        assert!(installed.path.unwrap().is_file());

        // An explicitly named kernel is what the process boots, installed or not.
        let override_path = Path::new("/srv/kernels/libkrunfw.5.dylib");
        let chosen = choose(Some(override_path), &root, "macos", "aarch64", Some(MSB));
        assert_eq!(chosen.source, KernelSource::Override);
        assert_eq!(chosen.path.as_deref(), Some(override_path));

        // Without either, the stock kernel boots and the host says why it is not the farm's.
        let empty = tempfile::tempdir().unwrap();
        let stock = choose(
            None,
            &install_root(empty.path()),
            "macos",
            "aarch64",
            Some(MSB),
        );
        assert_eq!(stock.source, KernelSource::Stock);
        assert!(stock.path.is_none());
        assert!(stock.reason.is_none());
    }

    #[test]
    fn a_kernel_built_for_another_msb_is_refused_with_the_reason() {
        let home = tempfile::tempdir().unwrap();
        let root = install_root(home.path());
        std::fs::create_dir_all(&root).unwrap();
        install_fake(&root, "aarch64", "macos", "0.7.12", "older");
        let chosen = choose(None, &root, "macos", "aarch64", Some(MSB));
        assert_eq!(chosen.source, KernelSource::Stock);
        assert!(chosen.path.is_none());
        let reason = chosen.reason.unwrap();
        assert!(reason.contains("built for msb 0.7.12"), "{reason}");
        assert!(reason.contains(&format!("runs {MSB}")), "{reason}");
        // The same installation is used as soon as the host runs the msb it was built for.
        assert_eq!(
            choose(None, &root, "macos", "aarch64", Some("0.7.12")).source,
            KernelSource::Installed
        );
        // Another architecture's kernel is not a fallback either.
        let chosen = choose(None, &root, "macos", "x86_64", Some("0.7.12"));
        assert_eq!(chosen.source, KernelSource::Stock);
        assert!(chosen.reason.unwrap().contains("built for aarch64"));
    }

    /// Backdates an installation's manifest, as an earlier preparation would have left it.
    fn age(root: &Path, id: &str, seconds_ago: u64) {
        let file = std::fs::File::options()
            .write(true)
            .open(root.join(id).join("manifest.txt"))
            .unwrap();
        let when = std::time::SystemTime::now() - std::time::Duration::from_secs(seconds_ago);
        file.set_modified(when).unwrap();
    }

    /// The installed kernel ids, sorted.
    fn installed_ids(root: &Path) -> Vec<String> {
        let mut ids: Vec<_> = std::fs::read_dir(root)
            .unwrap()
            .flatten()
            .filter(|entry| entry.path().is_dir())
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        ids.sort();
        ids
    }

    #[test]
    fn installing_puts_the_manifest_hash_under_the_farm_data_directory() {
        let dir = tempfile::tempdir().unwrap();
        let artifact = build(dir.path(), "aarch64", "macos", MSB, b"first kernel");
        assert!(artifact.id.len() == 16);
        validate_id(&artifact.id).unwrap();
        validate_filename(&artifact.filename).unwrap();
        let home = tempfile::tempdir().unwrap();
        let root = install_root(home.path());
        let library = install(&artifact, home.path()).unwrap();
        assert_eq!(
            library,
            install_root(home.path())
                .join(&artifact.id)
                .join("libkrunfw.5.dylib")
        );
        assert_eq!(std::fs::read(&library).unwrap(), b"first kernel");
        let installed_manifest =
            std::fs::read_to_string(library.parent().unwrap().join("manifest.txt")).unwrap();
        assert_eq!(installed_manifest, artifact.manifest);
        // Nothing is written into the microsandbox install.
        assert!(!home.path().join(".microsandbox").exists());
        age(&root, &artifact.id, 3600);

        // A second build supersedes the first, but the first stays: a VM that is booting from
        // it may be opening the library right now, and the install races that boot.
        let mut newer = build(dir.path(), "aarch64", "macos", MSB, b"second kernel");
        newer.manifest = newer.manifest.replace("linux-6.12.113", "linux-6.12.114");
        newer.id = id_for(&newer.manifest);
        let newer_library = install(&newer, home.path()).unwrap();
        assert_ne!(newer_library, library);
        assert!(
            library.exists(),
            "the replaced kernel is kept for a boot in flight"
        );
        assert!(newer_library.is_file());
        let mut both = vec![artifact.id.clone(), newer.id.clone()];
        both.sort();
        assert_eq!(installed_ids(&root), both);
        // The newest is what a host boots.
        assert_eq!(
            choose(None, &root, "macos", "aarch64", Some(MSB))
                .id
                .as_deref(),
            Some(newer.id.as_str())
        );

        // The next install removes the one two installs ago, so a host keeps two at most.
        age(&root, &newer.id, 1800);
        let mut third = build(dir.path(), "aarch64", "macos", MSB, b"third kernel");
        third.manifest = third.manifest.replace("linux-6.12.113", "linux-6.12.115");
        third.id = id_for(&third.manifest);
        install(&third, home.path()).unwrap();
        assert!(!library.exists(), "the kernel two installs old is removed");
        let mut remaining = vec![newer.id.clone(), third.id.clone()];
        remaining.sort();
        assert_eq!(installed_ids(&root), remaining);
        assert_eq!(
            choose(None, &root, "macos", "aarch64", Some(MSB))
                .id
                .as_deref(),
            Some(third.id.as_str())
        );
    }

    #[test]
    fn an_artifact_is_refused_when_its_msb_or_checksum_does_not_match() {
        let dir = tempfile::tempdir().unwrap();
        build(dir.path(), "x86_64", "linux", "0.7.12", b"kernel");
        let error = artifact_for(dir.path(), "linux", "x86_64")
            .unwrap_err()
            .to_string();
        assert!(error.contains("built for msb 0.7.12"), "{error}");
        assert!(error.contains(MSB), "{error}");

        let dir = tempfile::tempdir().unwrap();
        let artifact = build(dir.path(), "x86_64", "linux", MSB, b"kernel");
        std::fs::write(&artifact.library, b"tampered").unwrap();
        let error = artifact_for(dir.path(), "linux", "x86_64")
            .unwrap_err()
            .to_string();
        assert!(error.contains("SHA-256"), "{error}");
        // A build tree without a kernel for the host is simply not an artifact.
        assert!(
            artifact_for(dir.path(), "linux", "aarch64")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn a_manifest_without_an_msb_is_not_installed_by_default() {
        let text = manifest(MSB, "aarch64", "aarch64/libkrunfw.5.dylib", "a")
            .replace(&format!("msb {MSB}\n"), "");
        let parsed = Manifest::parse(&text).unwrap();
        assert_eq!(parsed.msb, None);
        assert_eq!(parsed.arch, "aarch64");
        assert_eq!(parsed.artifacts.len(), 1);
        let none = Manifest::parse(
            "revision x\narch aarch64\nmsb none\nartifact a/libkrunfw.5.dylib sha256 b\n",
        )
        .unwrap();
        assert_eq!(none.msb, None);
    }

    #[test]
    fn the_msb_path_falls_back_to_the_default_install() {
        // The environment of a test process is shared; only assert the default shape when
        // MSB_PATH is unset there.
        if std::env::var_os("MSB_PATH").is_none()
            && let Some(home) = std::env::var_os("HOME")
        {
            assert_eq!(
                msb_path(),
                PathBuf::from(home).join(".microsandbox/bin/msb")
            );
        }
    }

    #[test]
    fn a_kernel_inside_the_msb_install_would_be_replaced() {
        // Host preparation asks this of the path an agent host reports, whose home it does
        // not know. Farm's own kernels live outside the install and survive.
        assert!(install_would_replace(Path::new(
            "/home/farm/.microsandbox/lib/libkrunfw.dylib"
        )));
        assert!(install_would_replace(Path::new(
            "~/.microsandbox/lib/libkrunfw.so.5.6.1"
        )));
        assert!(!install_would_replace(Path::new(
            "/srv/kernels/libkrunfw.5.dylib"
        )));
        assert!(!install_would_replace(Path::new(
            "/home/farm/.local/share/farm/libkrunfw/0123456789abcdef/libkrunfw.5.dylib"
        )));
        assert!(!install_would_replace(Path::new(
            "/home/farm/.microsandbox/bin/libkrunfw.5.dylib"
        )));
    }
}

#[cfg(test)]
mod published_tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::sync::{Arc, Mutex};

    const LIBRARY: &[u8] = b"published kernel fixture";

    /// An object store: the files it serves, and every path asked for, in order.
    #[derive(Default)]
    struct Bucket {
        files: Mutex<BTreeMap<String, Vec<u8>>>,
        requests: Mutex<Vec<String>>,
    }

    async fn serve(bucket: Arc<Bucket>) -> String {
        let app = axum::Router::new().fallback(move |uri: axum::http::Uri| {
            let bucket = bucket.clone();
            async move {
                use axum::response::IntoResponse;
                let path = uri.path().to_owned();
                bucket.requests.lock().unwrap().push(path.clone());
                let file = bucket.files.lock().unwrap().get(&path).cloned();
                match file {
                    Some(bytes) => bytes.into_response(),
                    None => axum::http::StatusCode::NOT_FOUND.into_response(),
                }
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await });
        url
    }

    /// A published kernel for `msb` and the bucket layout `fetch_published` reads.
    struct Published {
        manifest: String,
        kid: String,
        index: serde_json::Value,
    }

    fn published(msb: &str) -> Published {
        let sha = format!("{:x}", Sha256::digest(LIBRARY));
        let manifest = format!(
            "revision f0493b6f1873ed6031bc0bb87568db0095a6d3b6\nmsb {msb}\nkernel linux-6.12.118\narch x86_64\nkbuild-arch x86_64\ntoolchain native\nartifact x86_64/libkrunfw.so.5.6.1 sha256 {sha}\n"
        );
        let kid = id_for(&manifest);
        let index = serde_json::json!({
            "kid": kid,
            "msb": msb,
            "os": "linux",
            "arch": "x86_64",
            "revision": "f0493b6f1873ed6031bc0bb87568db0095a6d3b6",
            "manifest_path": format!("{kid}/manifest.txt"),
            "artifact_path": format!("{kid}/linux-x86_64/libkrunfw.so.5.6.1"),
            "sha256": sha,
            "size": LIBRARY.len(),
        });
        Published {
            manifest,
            kid,
            index,
        }
    }

    fn bucket_with(published: &Published, index: serde_json::Value) -> Arc<Bucket> {
        let bucket = Arc::new(Bucket::default());
        {
            let mut files = bucket.files.lock().unwrap();
            files.insert(
                format!("/msb-{}/linux-x86_64.json", crate::remote::MSB_VERSION),
                index.to_string().into_bytes(),
            );
            files.insert(
                format!("/{}/manifest.txt", published.kid),
                published.manifest.clone().into_bytes(),
            );
            files.insert(
                format!("/{}/linux-x86_64/libkrunfw.so.5.6.1", published.kid),
                LIBRARY.to_vec(),
            );
        }
        bucket
    }

    fn requests(bucket: &Bucket) -> Vec<String> {
        bucket.requests.lock().unwrap().clone()
    }

    #[test]
    fn the_release_url_follows_the_environment_rule() {
        assert_eq!(release_url_from(None).as_deref(), Some(RELEASE_URL));
        assert_eq!(release_url_from(Some("")), None);
        assert_eq!(release_url_from(Some("   ")), None);
        assert_eq!(
            release_url_from(Some(" http://127.0.0.1:9/kernels/ ")).as_deref(),
            Some("http://127.0.0.1:9/kernels")
        );
    }

    #[tokio::test]
    async fn a_published_kernel_is_downloaded_verified_and_cached() {
        let published = published(crate::remote::MSB_VERSION);
        let bucket = bucket_with(&published, published.index.clone());
        let base = serve(bucket.clone()).await;
        let cache = tempfile::tempdir().unwrap();
        let fetched = fetch_published(
            &base,
            "linux",
            "x86_64",
            crate::remote::MSB_VERSION,
            cache.path(),
        )
        .await
        .unwrap()
        .expect("the kernel is published");
        assert_eq!(fetched.id, published.kid);
        assert_eq!(fetched.filename, "libkrunfw.so.5.6.1");
        assert_eq!(std::fs::read(&fetched.library).unwrap(), LIBRARY);
        assert_eq!(fetched.manifest, published.manifest);
        assert_eq!(requests(&bucket).len(), 3);

        // A second fetch reads the index again, which says which kernel is current, but
        // downloads neither the manifest nor the library.
        let again = fetch_published(
            &base,
            "linux",
            "x86_64",
            crate::remote::MSB_VERSION,
            cache.path(),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(again.library, fetched.library);
        let asked = requests(&bucket);
        assert_eq!(asked.len(), 4, "{asked:?}");
        assert_eq!(
            asked[3],
            format!("/msb-{}/linux-x86_64.json", crate::remote::MSB_VERSION)
        );

        // A cached library that no longer matches is downloaded again, not reused.
        std::fs::write(&fetched.library, b"tampered").unwrap();
        fetch_published(
            &base,
            "linux",
            "x86_64",
            crate::remote::MSB_VERSION,
            cache.path(),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(
            requests(&bucket).len(),
            7,
            "the index, manifest and library are read again"
        );
        assert_eq!(std::fs::read(&fetched.library).unwrap(), LIBRARY);
    }

    #[tokio::test]
    async fn a_build_with_no_published_index_is_none() {
        let published = published(crate::remote::MSB_VERSION);
        let base = serve(bucket_with(&published, published.index.clone())).await;
        let cache = tempfile::tempdir().unwrap();
        assert!(
            fetch_published(
                &base,
                "linux",
                "aarch64",
                crate::remote::MSB_VERSION,
                cache.path()
            )
            .await
            .unwrap()
            .is_none()
        );
        assert!(
            fetch_published(&base, "linux", "x86_64", "0.7.13", cache.path())
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn a_library_that_does_not_match_its_checksum_is_refused() {
        let published = published(crate::remote::MSB_VERSION);
        let bucket = bucket_with(&published, published.index.clone());
        bucket.files.lock().unwrap().insert(
            format!("/{}/linux-x86_64/libkrunfw.so.5.6.1", published.kid),
            b"tampered kernel fixture".to_vec(),
        );
        let base = serve(bucket).await;
        let cache = tempfile::tempdir().unwrap();
        let error = fetch_published(
            &base,
            "linux",
            "x86_64",
            crate::remote::MSB_VERSION,
            cache.path(),
        )
        .await
        .unwrap_err();
        assert!(
            format!("{error:#}").contains("size and sha256"),
            "{error:#}"
        );
        assert!(
            !cache
                .path()
                .join(&published.kid)
                .join("libkrunfw.so.5.6.1")
                .exists()
        );
    }

    #[tokio::test]
    async fn an_index_that_names_another_msb_or_escapes_the_bucket_is_refused() {
        let published = published(crate::remote::MSB_VERSION);
        let cache = tempfile::tempdir().unwrap();

        let mut other_msb = published.index.clone();
        other_msb["msb"] = "0.7.13".into();
        let bucket = bucket_with(&published, other_msb);
        let base = serve(bucket.clone()).await;
        let error = fetch_published(
            &base,
            "linux",
            "x86_64",
            crate::remote::MSB_VERSION,
            cache.path(),
        )
        .await
        .unwrap_err();
        assert!(format!("{error:#}").contains("msb"), "{error:#}");
        assert_eq!(requests(&bucket).len(), 1, "no artifact is asked for");

        for (field, value) in [
            ("artifact_path", "../../etc/passwd"),
            ("manifest_path", "/etc/passwd"),
            ("artifact_path", "a/./b\tc"),
        ] {
            let mut index = published.index.clone();
            index[field] = value.into();
            let bucket = bucket_with(&published, index);
            let base = serve(bucket.clone()).await;
            assert!(
                fetch_published(
                    &base,
                    "linux",
                    "x86_64",
                    crate::remote::MSB_VERSION,
                    cache.path()
                )
                .await
                .is_err(),
                "{field}={value:?}"
            );
            assert_eq!(requests(&bucket).len(), 1, "{field}={value:?}");
        }
    }

    #[tokio::test]
    async fn a_manifest_for_another_msb_or_with_a_wrong_checksum_is_refused() {
        let cache = tempfile::tempdir().unwrap();
        // The index and the manifest hash agree, but the manifest was built for another msb.
        let older = published("0.7.13");
        let mut lying = older.index.clone();
        lying["msb"] = crate::remote::MSB_VERSION.into();
        let bucket = bucket_with(&older, lying);
        let base = serve(bucket).await;
        assert!(
            fetch_published(
                &base,
                "linux",
                "x86_64",
                crate::remote::MSB_VERSION,
                cache.path()
            )
            .await
            .is_err()
        );

        // The index's sha256 disagrees with the manifest's, though the library matches its
        // own bytes.
        let good = published(crate::remote::MSB_VERSION);
        let mut wrong = good.index.clone();
        wrong["sha256"] = "0".repeat(64).into();
        let base = serve(bucket_with(&good, wrong)).await;
        assert!(
            fetch_published(
                &base,
                "linux",
                "x86_64",
                crate::remote::MSB_VERSION,
                cache.path()
            )
            .await
            .is_err()
        );
    }
}

#[cfg(test)]
mod kid_tests {
    use super::*;

    /// The CI job names each kernel with `scripts/libkrunfw-publish.sh kid`, which is
    /// `sha256sum | cut -c1-16`; this pins that rule to `id_for`.
    #[test]
    fn the_kernel_id_is_the_first_sixteen_hex_digits_of_the_manifest_sha256() {
        assert_eq!(id_for("abc"), "ba7816bf8f01cfea");
    }

    /// Checks the checked-in build's manifest when a local build exists (`dist/` is not
    /// committed); the id it gets is the one the bucket serves that manifest under.
    #[test]
    fn a_local_build_gets_the_id_the_publish_script_names_it() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("dist/libkrunfw/x86_64/manifest.txt");
        let Ok(text) = std::fs::read_to_string(&path) else {
            return;
        };
        assert_eq!(id_for(&text), "bfc3b7dc77ee5383");
    }
}
