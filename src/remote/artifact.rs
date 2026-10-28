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
        // An artifact from before the marker still loads, but cannot be installed anywhere:
        // `artifact_built_by_daemon` refuses it with the rebuild command.
        let build_id = crate::remote::agent::artifact_build_id(&bytes).unwrap_or_default();
        Ok(Self {
            info: AgentArtifactInfo {
                sha256: format!("{:x}", Sha256::digest(&bytes)),
                arch: arch.into(),
                protocol: VERSION,
                supports_single_vm: true,
                build_id,
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

#[cfg(test)]
mod tests {
    #[test]
    fn reads_the_build_marker_embedded_in_an_artifact() {
        let stamp = crate::remote::agent::FARM_AGENT_BUILD_STAMP;
        let mut bytes = b"\x7fELF".to_vec();
        bytes.extend_from_slice(&stamp);
        bytes.extend_from_slice(b"\0trailing");
        assert_eq!(
            crate::remote::agent::artifact_build_id(&bytes).as_deref(),
            Some(crate::BUILD_ID)
        );
        // An artifact from before the marker, or a truncated one, is reported as unknown.
        assert!(crate::remote::agent::artifact_build_id(b"\x7fELF nothing here").is_none());
        let mut cut = b"farm-agent-build:".to_vec();
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
