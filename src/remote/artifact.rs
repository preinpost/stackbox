use super::{AgentArtifactInfo, protocol::VERSION};
use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

/// Where releases publish their agents: a GitHub release per version, tagged `v<version>`, whose
/// `manifest.json` names each agent as `v<version>/<asset>`.
pub const RELEASE_URL: &str = "https://github.com/preinpost/stackbox/releases/download";
const MAX_SIZE: u64 = 128 * 1024 * 1024;

pub struct Artifact {
    pub info: AgentArtifactInfo,
    pub bytes: Arc<[u8]>,
}
impl Artifact {
    pub fn load(path: &Path) -> Result<Self> {
        ensure!(
            path.metadata()?.len() <= MAX_SIZE,
            "stackbox-agent binary exceeds 128 MiB"
        );
        let bytes = std::fs::read(path)
            .with_context(|| format!("read the stackbox-agent at {}", path.display()))?;
        Self::parse(bytes).with_context(|| path.display().to_string())
    }
    pub fn parse(bytes: Vec<u8>) -> Result<Self> {
        let (os, arch) = target(&bytes)?;
        // An artifact from before the marker still loads, but cannot be installed anywhere:
        // `artifact_built_by_daemon` refuses it with the rebuild command.
        let build_id = crate::remote::agent::artifact_build_id(&bytes).unwrap_or_default();
        Ok(Self {
            info: AgentArtifactInfo {
                sha256: format!("{:x}", Sha256::digest(&bytes)),
                os: os.into(),
                arch: arch.into(),
                protocol: VERSION,
                supports_single_vm: true,
                build_id,
            },
            bytes: bytes.into(),
        })
    }
}

/// The OS and CPU a stackbox-agent runs on, from its executable header.
fn target(bytes: &[u8]) -> Result<(&'static str, &'static str)> {
    let u16_at = |at: usize| Some(u16::from_le_bytes(bytes.get(at..at + 2)?.try_into().ok()?));
    let u32_at = |at: usize| Some(u32::from_le_bytes(bytes.get(at..at + 4)?.try_into().ok()?));
    if bytes.starts_with(b"\x7fELF") {
        ensure!(
            bytes.len() > 64 && bytes[4] == 2 && bytes[5] == 1,
            "a Linux stackbox-agent must be a 64-bit little-endian ELF binary"
        );
        return match u16_at(18) {
            Some(183) => Ok(("linux", "aarch64")),
            Some(62) => Ok(("linux", "x86_64")),
            _ => bail!("unsupported stackbox-agent CPU architecture"),
        };
    }
    // 64-bit Mach-O; a universal binary is not accepted.
    if bytes.starts_with(&[0xcf, 0xfa, 0xed, 0xfe]) {
        return match u32_at(4) {
            Some(0x0100_000c) => Ok(("macos", "aarch64")),
            Some(0x0100_0007) => Ok(("macos", "x86_64")),
            _ => bail!("unsupported stackbox-agent CPU architecture"),
        };
    }
    if bytes.starts_with(b"MZ")
        && let Some(pe) = u32_at(0x3c).map(|offset| offset as usize)
        && bytes.get(pe..pe + 4) == Some(b"PE\0\0")
    {
        return match u16_at(pe + 4) {
            Some(0xaa64) => Ok(("windows", "aarch64")),
            Some(0x8664) => Ok(("windows", "x86_64")),
            _ => bail!("unsupported stackbox-agent CPU architecture"),
        };
    }
    bail!("not a Linux (ELF), macOS (Mach-O) or Windows (PE) stackbox-agent binary")
}

/// The agent binaries in each directory under `dir`, such as `dist/agent/<target>/stackbox-agent`.
pub fn binaries_in(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut paths: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .flat_map(|path| ["stackbox-agent", "stackbox-agent.exe"].map(|name| path.join(name)))
        .filter(|path| path.is_file())
        .collect();
    paths.sort();
    paths
}

/// Load every distinct stackbox-agent, keyed by platform. The same platform may come from an
/// explicit path and the default directory.
pub fn load_all(paths: &[PathBuf]) -> Result<BTreeMap<String, Artifact>> {
    let mut artifacts: BTreeMap<String, Artifact> = BTreeMap::new();
    for path in paths {
        if !path.is_file() {
            continue;
        }
        let artifact = Artifact::load(path)?;
        let platform = artifact.info.platform();
        if let Some(previous) = artifacts.get(&platform)
            && previous.info.sha256 != artifact.info.sha256
        {
            bail!("two different stackbox-agent binaries are configured for {platform}");
        }
        artifacts.insert(platform, artifact);
    }
    Ok(artifacts)
}

/// An agent of this build that a release published; downloaded the first time a host needs it.
pub struct Published {
    pub info: AgentArtifactInfo,
    url: String,
    size: u64,
    fetched: tokio::sync::OnceCell<Arc<Artifact>>,
}

#[derive(Deserialize)]
struct Manifest {
    build_id: String,
    agents: BTreeMap<String, ManifestAgent>,
}
#[derive(Deserialize)]
struct ManifestAgent {
    path: String,
    sha256: String,
    size: u64,
}

fn client() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .build()?)
}

/// This version's published agents, by platform. A version that was never released has none.
/// A release built from other sources than this binary (a local dev build) publishes none for it
/// either, so such a build falls back to its own agents.
pub async fn fetch_published(base: &str) -> Result<BTreeMap<String, Published>> {
    let base = base.trim_end_matches('/');
    let url = format!("{base}/v{}/manifest.json", env!("CARGO_PKG_VERSION"));
    let response = client()?
        .get(&url)
        .timeout(Duration::from_secs(20))
        .send()
        .await
        .with_context(|| format!("fetch {url}"))?;
    // GitHub answers 404 for a missing release asset; a private mirror may answer 403.
    if matches!(response.status().as_u16(), 403 | 404) {
        return Ok(BTreeMap::new());
    }
    let manifest: Manifest = response
        .error_for_status()
        .with_context(|| format!("fetch {url}"))?
        .json()
        .await
        .with_context(|| format!("parse {url}"))?;
    published(manifest, base)
}

fn published(manifest: Manifest, base: &str) -> Result<BTreeMap<String, Published>> {
    if manifest.build_id != crate::BUILD_ID {
        return Ok(BTreeMap::new());
    }
    let mut agents = BTreeMap::new();
    for (platform, agent) in manifest.agents {
        let (os, arch) = platform
            .split_once('-')
            .filter(|(os, arch)| {
                ["linux", "macos", "windows"].contains(os) && ["aarch64", "x86_64"].contains(arch)
            })
            .with_context(|| format!("unknown stackbox-agent platform {platform}"))?;
        validate_digest(&agent.sha256)?;
        ensure!(
            agent.size <= MAX_SIZE
                && !agent.path.starts_with('/')
                && agent
                    .path
                    .split('/')
                    .all(|part| !part.is_empty() && part != ".."),
            "invalid published stackbox-agent entry for {platform}"
        );
        let info = AgentArtifactInfo {
            sha256: agent.sha256,
            os: os.into(),
            arch: arch.into(),
            protocol: VERSION,
            supports_single_vm: true,
            build_id: crate::BUILD_ID.into(),
        };
        agents.insert(
            platform,
            Published {
                info,
                url: format!("{base}/{}", agent.path),
                size: agent.size,
                fetched: Default::default(),
            },
        );
    }
    Ok(agents)
}

impl Published {
    /// The agent, from `cache` (keyed by sha256) or else downloaded into it.
    pub async fn get(&self, cache: &Path) -> Result<Arc<Artifact>> {
        self.fetched
            .get_or_try_init(|| async {
                let dir = cache.join(&self.info.sha256);
                let file = dir.join(if self.info.os == "windows" {
                    "stackbox-agent.exe"
                } else {
                    "stackbox-agent"
                });
                let cached = std::fs::read(&file)
                    .ok()
                    .filter(|bytes| format!("{:x}", Sha256::digest(bytes)) == self.info.sha256);
                let bytes = match cached {
                    Some(bytes) => bytes,
                    None => {
                        let bytes = self.download().await?;
                        std::fs::create_dir_all(&dir)?;
                        let partial = dir.join(".partial");
                        std::fs::write(&partial, &bytes)?;
                        std::fs::rename(&partial, &file)?;
                        bytes
                    }
                };
                let artifact = Artifact::parse(bytes)?;
                ensure!(
                    artifact.info.platform() == self.info.platform()
                        && artifact.info.build_id == self.info.build_id,
                    "the published {} stackbox-agent is a {} binary of build {}",
                    self.info.platform(),
                    artifact.info.platform(),
                    artifact.info.build_id
                );
                Ok(Arc::new(artifact))
            })
            .await
            .cloned()
    }
    async fn download(&self) -> Result<Vec<u8>> {
        let bytes = client()?
            .get(&self.url)
            .timeout(Duration::from_secs(300))
            .send()
            .await
            .and_then(|response| response.error_for_status())
            .with_context(|| format!("download {}", self.url))?
            .bytes()
            .await
            .with_context(|| format!("download {}", self.url))?;
        ensure!(
            bytes.len() as u64 == self.size
                && format!("{:x}", Sha256::digest(&bytes)) == self.info.sha256,
            "{} does not match its published size and sha256",
            self.url
        );
        Ok(bytes.into())
    }
}

pub fn validate_digest(value: &str) -> Result<()> {
    ensure!(
        value.len() == 64
            && value
                .bytes()
                .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c)),
        "invalid agent digest"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stamped(mut header: Vec<u8>) -> Vec<u8> {
        header.resize(128, 0);
        header.extend(
            format!(
                "{}{}\0",
                crate::remote::agent::BUILD_STAMP_PREFIX,
                crate::BUILD_ID
            )
            .bytes(),
        );
        header
    }
    fn elf(machine: u16) -> Vec<u8> {
        let mut bytes = b"\x7fELF\x02\x01".to_vec();
        bytes.resize(20, 0);
        bytes[18..20].copy_from_slice(&machine.to_le_bytes());
        stamped(bytes)
    }
    fn mach_o(cpu: u32) -> Vec<u8> {
        let mut bytes = vec![0xcf, 0xfa, 0xed, 0xfe];
        bytes.extend(cpu.to_le_bytes());
        stamped(bytes)
    }
    fn pe(machine: u16) -> Vec<u8> {
        let mut bytes = b"MZ".to_vec();
        bytes.resize(0x40, 0);
        bytes[0x3c..0x40].copy_from_slice(&0x40u32.to_le_bytes());
        bytes.extend(b"PE\0\0");
        bytes.extend(machine.to_le_bytes());
        stamped(bytes)
    }

    #[test]
    fn reads_the_platform_from_the_executable_header() {
        for (bytes, platform) in [
            (elf(62), "linux-x86_64"),
            (elf(183), "linux-aarch64"),
            (mach_o(0x0100_000c), "macos-aarch64"),
            (pe(0x8664), "windows-x86_64"),
        ] {
            let artifact = Artifact::parse(bytes).unwrap();
            assert_eq!(artifact.info.platform(), platform);
            assert_eq!(artifact.info.build_id, crate::BUILD_ID);
        }
        assert!(Artifact::parse(b"#!/bin/sh\necho stackbox-agent\n".to_vec()).is_err());
        assert!(Artifact::parse(elf(40)).is_err());
        // A PE header pointer past the end of the file is not a Windows binary.
        let mut cut = pe(0x8664);
        cut[0x3c..0x40].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(Artifact::parse(cut).is_err());
    }

    #[test]
    fn finds_agents_in_every_target_directory() {
        let dir = tempfile::tempdir().unwrap();
        for (target, name, bytes) in [
            ("x86_64", "stackbox-agent", elf(62)),
            ("macos-aarch64", "stackbox-agent", mach_o(0x0100_000c)),
            ("windows-x86_64", "stackbox-agent.exe", pe(0x8664)),
        ] {
            std::fs::create_dir(dir.path().join(target)).unwrap();
            std::fs::write(dir.path().join(target).join(name), bytes).unwrap();
        }
        std::fs::write(dir.path().join("stray-file"), b"").unwrap();
        let loaded = load_all(&binaries_in(dir.path())).unwrap();
        assert_eq!(
            loaded.keys().collect::<Vec<_>>(),
            ["linux-x86_64", "macos-aarch64", "windows-x86_64"]
        );
    }

    #[test]
    fn a_manifest_lists_only_well_formed_agents_of_this_build() {
        let manifest = |build_id: &str, platform: &str, path: &str| Manifest {
            build_id: build_id.into(),
            agents: [(
                platform.to_owned(),
                ManifestAgent {
                    path: path.into(),
                    sha256: "a".repeat(64),
                    size: 1,
                },
            )]
            .into(),
        };
        let id = crate::BUILD_ID;
        let version = env!("CARGO_PKG_VERSION");
        let agents = published(
            manifest(
                id,
                "macos-aarch64",
                &format!("v{version}/stackbox-agent-macos-aarch64"),
            ),
            "https://example.test/download",
        )
        .unwrap();
        let agent = &agents["macos-aarch64"];
        assert_eq!(agent.info.os, "macos");
        assert_eq!(
            agent.url,
            format!("https://example.test/download/v{version}/stackbox-agent-macos-aarch64")
        );
        // A release of another build publishes nothing for this one.
        assert!(
            published(manifest("other", "linux-x86_64", "a"), "b")
                .unwrap()
                .is_empty()
        );
        assert!(published(manifest(id, "darwin-aarch64", "a"), "b").is_err());
        assert!(published(manifest(id, "linux-x86_64", "../a"), "b").is_err());
        assert!(published(manifest(id, "linux-x86_64", "/a"), "b").is_err());
    }

    #[test]
    fn reads_the_build_marker_embedded_in_an_artifact() {
        let stamp = crate::remote::agent::STACKBOX_AGENT_BUILD_STAMP;
        let mut bytes = b"\x7fELF".to_vec();
        bytes.extend_from_slice(&stamp);
        bytes.extend_from_slice(b"\0trailing");
        assert_eq!(
            crate::remote::agent::artifact_build_id(&bytes).as_deref(),
            Some(crate::BUILD_ID)
        );
        // A PE binary also holds the bare prefix ahead of the stamp.
        let mut pe = b"MZ stackbox-agent-build:\0\0\0:\0?\xa6 ".to_vec();
        pe.extend_from_slice(&stamp);
        assert_eq!(
            crate::remote::agent::artifact_build_id(&pe).as_deref(),
            Some(crate::BUILD_ID)
        );
        // An artifact from before the marker, or a truncated one, is reported as unknown.
        assert!(crate::remote::agent::artifact_build_id(b"\x7fELF nothing here").is_none());
        let mut cut = b"stackbox-agent-build:".to_vec();
        cut.extend_from_slice(b"abc");
        assert!(crate::remote::agent::artifact_build_id(&cut).is_none());
    }

    #[test]
    fn a_stale_artifact_is_refused_with_the_rebuild_command() {
        let stale =
            crate::manager::hosts::artifact_built_by_daemon_for_test("aarch64", "old-build");
        let error = stale.unwrap_err().to_string();
        assert!(error.contains("built from old-build"), "{error}");
        assert!(error.contains("task agent-build"), "{error}");
    }
}
