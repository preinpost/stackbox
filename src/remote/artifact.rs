use super::{AgentArtifactInfo, protocol::VERSION};
use anyhow::{Context, Result, ensure};
use sha2::{Digest, Sha256};
use std::{path::Path, sync::Arc};

pub struct Artifact {
    pub info: AgentArtifactInfo,
    pub bytes: Arc<[u8]>,
}
impl Artifact {
    pub fn load(path: &Path) -> Result<Self> {
        ensure!(
            path.metadata()?.len() <= 128 * 1024 * 1024,
            "farm-agent binary exceeds 128 MiB"
        );
        let bytes = std::fs::read(path).context("read FARM_AGENT_PATH")?;
        ensure!(
            bytes.len() > 64 && &bytes[..4] == b"\x7fELF" && bytes[4] == 2 && bytes[5] == 1,
            "FARM_AGENT_PATH must be a 64-bit little-endian Linux ELF farm-agent binary"
        );
        let arch = match u16::from_le_bytes([bytes[18], bytes[19]]) {
            183 => "aarch64",
            62 => "x86_64",
            _ => anyhow::bail!("unsupported farm-agent CPU architecture"),
        };
        Ok(Self {
            info: AgentArtifactInfo {
                sha256: format!("{:x}", Sha256::digest(&bytes)),
                arch: arch.into(),
                protocol: VERSION,
                supports_single_vm: true,
            },
            bytes: bytes.into(),
        })
    }
}
/// Load every distinct farm-agent. The same architecture may come from an explicit path and the default directory.
pub fn load_all(
    paths: &[std::path::PathBuf],
) -> Result<std::collections::BTreeMap<String, Artifact>> {
    let mut artifacts: std::collections::BTreeMap<String, Artifact> =
        std::collections::BTreeMap::new();
    for path in paths {
        if !path.is_file() {
            continue;
        }
        let artifact = Artifact::load(path)?;
        if let Some(previous) = artifacts.get(&artifact.info.arch)
            && previous.info.sha256 != artifact.info.sha256
        {
            anyhow::bail!(
                "two different farm-agent binaries are configured for {}",
                artifact.info.arch
            );
        }
        artifacts.insert(artifact.info.arch.clone(), artifact);
    }
    Ok(artifacts)
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
