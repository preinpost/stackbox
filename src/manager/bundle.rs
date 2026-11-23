//! Multi-node snapshot bundle archives.
//!
//! A single-VM snapshot exports directly as microsandbox's own `.msb` archive. A multi-node
//! snapshot holds one msb snapshot per VM (`control`, `compute-1`, ...), so exporting it
//! packs each VM's `.msb` archive into a plain tar file whose first entry is
//! [`MANIFEST_ENTRY`]. The outer tar is uncompressed because every inner `.msb` is already
//! zstd-compressed.
//!
//! Imports detect a bundle by checking whether the archive is a tar whose first entry is
//! [`MANIFEST_ENTRY`]; every other archive passes straight to microsandbox as a single-VM
//! `.msb`.

use crate::model::{ComputeSize, GpuConfig};
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::{
    fs::File,
    io::{Read, Write},
    path::{Path, PathBuf},
};

/// First tar entry of a multi-node snapshot bundle.
pub const MANIFEST_ENTRY: &str = "farm-multinode-bundle.json";
/// Current bundle manifest schema version.
pub const BUNDLE_SCHEMA: u32 = 1;
/// Upper bound on manifest size so a crafted archive cannot exhaust memory.
const MAX_MANIFEST_BYTES: u64 = 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BundleManifest {
    pub schema: u32,
    pub nodes: Vec<BundleNode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compute: Option<ComputeSize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gpu: Option<GpuConfig>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BundleNode {
    /// VM suffix: `control`, `compute-1`, ...
    pub node: String,
    /// Relative entry path inside the tar (`nodes/<node>.msb`).
    pub file: String,
}

/// Extracted contents of a multi-node bundle, written under a caller-owned directory.
#[derive(Debug)]
pub struct ExtractedBundle {
    pub manifest: BundleManifest,
    /// `(node suffix, extracted .msb file path)` in manifest order (`control` first).
    pub nodes: Vec<(String, PathBuf)>,
}

/// Write a multi-node bundle tar at `output` containing `manifest` and each `(node, path)`
/// archive in order.
pub fn pack(
    output: &Path,
    nodes: &[(String, PathBuf)],
    compute: Option<ComputeSize>,
    gpu: Option<GpuConfig>,
) -> Result<()> {
    ensure!(!nodes.is_empty(), "bundle requires at least one node");
    let manifest = BundleManifest {
        schema: BUNDLE_SCHEMA,
        nodes: nodes
            .iter()
            .map(|(node, _)| {
                validate_node_name(node)?;
                Ok(BundleNode {
                    node: node.clone(),
                    file: format!("nodes/{node}.msb"),
                })
            })
            .collect::<Result<Vec<_>>>()?,
        compute,
        gpu,
    };
    let manifest_bytes = serde_json::to_vec_pretty(&manifest)?;
    let file = File::create(output)
        .with_context(|| format!("create bundle archive {}", output.display()))?;
    let mut builder = tar::Builder::new(file);

    let mut header = tar::Header::new_gnu();
    header.set_size(manifest_bytes.len() as u64);
    header.set_mode(0o644);
    header.set_cksum();
    builder
        .append_data(&mut header, MANIFEST_ENTRY, manifest_bytes.as_slice())
        .context("write bundle manifest")?;

    for ((_, path), entry) in nodes.iter().zip(&manifest.nodes) {
        let mut src = File::open(path)
            .with_context(|| format!("open node archive {} for {}", path.display(), entry.node))?;
        builder
            .append_file(&entry.file, &mut src)
            .with_context(|| format!("append {} to bundle", entry.file))?;
    }

    let mut file = builder.into_inner().context("finish bundle tar")?;
    file.flush()?;
    file.sync_all()?;
    Ok(())
}

/// Returns `true` when `path` is a farm multi-node bundle tar (first tar entry is
/// [`MANIFEST_ENTRY`]). Plain or zstd-compressed single-VM `.msb` archives return `false`.
pub fn is_bundle(path: &Path) -> Result<bool> {
    let mut file = File::open(path).with_context(|| format!("open archive {}", path.display()))?;
    let mut magic = [0u8; 4];
    if file.read_exact(&mut magic).is_err() {
        return Ok(false);
    }
    // Single-VM `.msb` archives are zstd-compressed by default.
    if magic == [0x28, 0xb5, 0x2f, 0xfd] {
        return Ok(false);
    }
    drop(file);
    let file = File::open(path).with_context(|| format!("open archive {}", path.display()))?;
    let mut archive = tar::Archive::new(file);
    let Ok(mut entries) = archive.entries() else {
        return Ok(false);
    };
    let Some(first) = entries.next() else {
        return Ok(false);
    };
    let Ok(entry) = first else {
        return Ok(false);
    };
    let Ok(entry_path) = entry.path() else {
        return Ok(false);
    };
    Ok(entry_path == Path::new(MANIFEST_ENTRY))
}

/// Extract a multi-node bundle tar into `dest_dir`, validating the manifest and every node
/// archive entry.
pub fn unpack(archive_path: &Path, dest_dir: &Path) -> Result<ExtractedBundle> {
    std::fs::create_dir_all(dest_dir)?;
    let file = File::open(archive_path)
        .with_context(|| format!("open bundle {}", archive_path.display()))?;
    let mut archive = tar::Archive::new(file);
    let mut entries = archive
        .entries()
        .with_context(|| format!("read tar entries of {}", archive_path.display()))?;

    let first = entries
        .next()
        .context("bundle archive is empty")?
        .context("read bundle manifest header")?;
    ensure!(
        first.path()? == Path::new(MANIFEST_ENTRY),
        "bundle archive does not start with {MANIFEST_ENTRY}"
    );
    ensure!(
        first.header().entry_type().is_file(),
        "{MANIFEST_ENTRY} must be a regular file"
    );
    ensure!(
        first.size() <= MAX_MANIFEST_BYTES,
        "{MANIFEST_ENTRY} exceeds size limit"
    );
    let mut manifest_bytes = Vec::new();
    first
        .take(MAX_MANIFEST_BYTES + 1)
        .read_to_end(&mut manifest_bytes)
        .context("read bundle manifest")?;
    let manifest: BundleManifest =
        serde_json::from_slice(&manifest_bytes).context("invalid bundle manifest")?;
    ensure!(
        manifest.schema == BUNDLE_SCHEMA,
        "unsupported multi-node bundle schema {} (expected {BUNDLE_SCHEMA})",
        manifest.schema
    );
    ensure!(
        !manifest.nodes.is_empty(),
        "bundle manifest lists no VM archives"
    );
    let mut expected_files = std::collections::BTreeMap::new();
    for entry in &manifest.nodes {
        validate_node_name(&entry.node)?;
        let expected_path = format!("nodes/{}.msb", entry.node);
        ensure!(
            entry.file == expected_path,
            "invalid node archive path '{}' for '{}'",
            entry.file,
            entry.node
        );
        if expected_files
            .insert(entry.file.clone(), entry.node.clone())
            .is_some()
        {
            bail!("bundle manifest lists node '{}' more than once", entry.node);
        }
    }

    let mut extracted = std::collections::BTreeMap::new();
    for item in entries {
        let mut entry = item.context("read bundle entry")?;
        ensure!(
            entry.header().entry_type().is_file(),
            "bundle contains a non-file entry"
        );
        let rel = entry.path()?.into_owned();
        let rel_str = rel
            .to_str()
            .context("bundle entry path is not valid UTF-8")?;
        let node = expected_files
            .get(rel_str)
            .with_context(|| format!("unexpected entry '{rel_str}' in bundle"))?;
        let out_path = dest_dir.join(format!("{node}.msb"));
        if extracted.contains_key(node) {
            bail!("bundle contains duplicate entry for node '{node}'");
        }
        let mut out = File::create(&out_path)
            .with_context(|| format!("create extracted archive {}", out_path.display()))?;
        std::io::copy(&mut entry, &mut out)
            .with_context(|| format!("extract {rel_str} from bundle"))?;
        out.flush()?;
        extracted.insert(node.clone(), out_path);
    }

    let mut nodes = Vec::with_capacity(manifest.nodes.len());
    for entry in &manifest.nodes {
        let path = extracted
            .remove(&entry.node)
            .with_context(|| format!("bundle is missing archive for node '{}'", entry.node))?;
        nodes.push((entry.node.clone(), path));
    }

    Ok(ExtractedBundle { manifest, nodes })
}

fn validate_node_name(node: &str) -> Result<()> {
    if node == "control" {
        return Ok(());
    }
    let valid_compute = node
        .strip_prefix("compute-")
        .and_then(|n| n.parse::<u8>().ok())
        .is_some_and(|index| (1..=crate::farm::MAX_COMPUTES).contains(&index));
    ensure!(valid_compute, "invalid multi-node VM name '{node}'");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{GpuConfig, GpuMode};

    #[test]
    fn round_trip_pack_detect_and_unpack() {
        let dir = tempfile::tempdir().unwrap();
        let ctl = dir.path().join("ctl.msb");
        let cmp = dir.path().join("cmp.msb");
        std::fs::write(&ctl, b"control-bytes").unwrap();
        std::fs::write(&cmp, b"compute-bytes").unwrap();
        let bundle = dir.path().join("farm.msb");
        let gpu = Some(GpuConfig {
            mode: GpuMode::Mtty,
            count: 2,
            traits: None,
            devices: None,
            resource_class: None,
        });
        let compute = Some(ComputeSize {
            cpus: 4,
            memory_mib: 6144,
            disk_mib: 40960,
        });

        pack(
            &bundle,
            &[("control".into(), ctl), ("compute-1".into(), cmp)],
            compute,
            gpu.clone(),
        )
        .unwrap();
        assert!(is_bundle(&bundle).unwrap());

        let out_dir = dir.path().join("extracted");
        let extracted = unpack(&bundle, &out_dir).unwrap();
        assert_eq!(extracted.manifest.schema, BUNDLE_SCHEMA);
        assert_eq!(extracted.manifest.compute, compute);
        assert_eq!(extracted.manifest.gpu, gpu);
        assert_eq!(extracted.nodes.len(), 2);
        assert_eq!(extracted.nodes[0].0, "control");
        assert_eq!(extracted.nodes[1].0, "compute-1");
        assert_eq!(
            std::fs::read(&extracted.nodes[0].1).unwrap(),
            b"control-bytes"
        );
        assert_eq!(
            std::fs::read(&extracted.nodes[1].1).unwrap(),
            b"compute-bytes"
        );
    }

    #[test]
    fn single_vm_archives_are_not_detected_as_bundles() {
        let dir = tempfile::tempdir().unwrap();
        let zstd = dir.path().join("zstd.msb");
        std::fs::write(&zstd, [0x28, 0xb5, 0x2f, 0xfd, 0, 1, 2, 3]).unwrap();
        assert!(!is_bundle(&zstd).unwrap());

        let plain = dir.path().join("plain.msb");
        std::fs::write(&plain, b"{\"farm.snapshot.id\":\"x\"}").unwrap();
        assert!(!is_bundle(&plain).unwrap());

        // A plain tar whose first entry is not the bundle manifest is not a bundle either.
        let other_tar = dir.path().join("other.msb");
        {
            let mut b = tar::Builder::new(File::create(&other_tar).unwrap());
            let mut h = tar::Header::new_gnu();
            h.set_size(4);
            h.set_mode(0o644);
            h.set_cksum();
            b.append_data(&mut h, "inventory.json", &b"test"[..])
                .unwrap();
            b.finish().unwrap();
        }
        assert!(!is_bundle(&other_tar).unwrap());
    }

    #[test]
    fn unpack_rejects_missing_and_unexpected_entries() {
        let dir = tempfile::tempdir().unwrap();
        let bad = dir.path().join("missing.msb");
        {
            let manifest = BundleManifest {
                schema: BUNDLE_SCHEMA,
                nodes: vec![BundleNode {
                    node: "control".into(),
                    file: "nodes/control.msb".into(),
                }],
                compute: None,
                gpu: None,
            };
            let bytes = serde_json::to_vec(&manifest).unwrap();
            let mut b = tar::Builder::new(File::create(&bad).unwrap());
            let mut h = tar::Header::new_gnu();
            h.set_size(bytes.len() as u64);
            h.set_mode(0o644);
            h.set_cksum();
            b.append_data(&mut h, MANIFEST_ENTRY, bytes.as_slice())
                .unwrap();
            b.finish().unwrap();
        }
        let err = unpack(&bad, &dir.path().join("out")).unwrap_err();
        assert!(
            err.to_string()
                .contains("missing archive for node 'control'")
        );
    }

    #[test]
    fn pack_rejects_invalid_node_names() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("a.msb");
        std::fs::write(&src, b"x").unwrap();
        let err = pack(
            &dir.path().join("out.msb"),
            &[("../evil".into(), src)],
            None,
            None,
        )
        .unwrap_err();
        assert!(err.to_string().contains("invalid multi-node VM name"));
    }
}
