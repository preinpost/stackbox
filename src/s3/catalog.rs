//! What a bucket holds, readable without downloading archives. Next to each `<name>.msb`
//! an export writes `<name>.json`, its metadata, last; the archive is complete once that
//! is there. `index.json` at the bucket's prefix gathers them, like a Helm repository's
//! index. The index is a cache that can always be rebuilt from the metadata files, and
//! the labels inside an archive stay the source of truth for what it holds.

use crate::model::{ComputeSize, GpuConfig, TemplateRole};
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// Version of the metadata and index formats. Readers skip files of a newer one.
pub const SCHEMA: u32 = 1;
/// Name of the catalog under the bucket's prefix.
pub const INDEX: &str = "index.json";

/// The metadata file written next to an exported archive.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct ArchiveMetadata {
    pub schema: u32,
    /// Object key of the archive this describes.
    pub key: String,
    pub size_bytes: u64,
    /// Hex SHA-256 of the archive; imports refuse an archive that does not match.
    pub sha256: String,
    pub snapshot_id: String,
    /// OpenStack release the guest was deployed with; unknown for older snapshots.
    #[serde(default)]
    pub release: Option<String>,
    /// `control` or `compute` for a template; absent for a snapshot of a whole stackbox.
    #[serde(default)]
    pub role: Option<TemplateRole>,
    /// The VM a template was frozen from (`control`, `compute-1`, ...).
    #[serde(default)]
    pub node: Option<String>,
    pub components: Vec<String>,
    /// Compute VMs of the stackbox the snapshot restores; 0 for all-in-one and templates.
    #[serde(default)]
    pub computes: u8,
    /// Compute VM size when it differs from the default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compute: Option<ComputeSize>,
    /// Virtual GPU configuration of the frozen stackbox, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gpu: Option<GpuConfig>,
    pub arch: String,
    #[serde(default)]
    pub platform: Option<String>,
    pub deployed: bool,
    #[serde(default)]
    pub source_stackbox: Option<String>,
    #[serde(default)]
    pub note: Option<String>,
    /// When the snapshot was frozen.
    pub created_at: u64,
    pub exported_at: u64,
}

/// `index.json`: every archive under the prefix with metadata, newest export first.
#[derive(Clone, Debug, Serialize, Deserialize, ToSchema)]
pub struct BucketIndex {
    pub schema: u32,
    pub generated_at: u64,
    pub entries: Vec<ArchiveMetadata>,
}
impl BucketIndex {
    pub fn new(mut entries: Vec<ArchiveMetadata>) -> Self {
        entries.sort_by(|a, b| b.exported_at.cmp(&a.exported_at).then(a.key.cmp(&b.key)));
        Self {
            schema: SCHEMA,
            generated_at: crate::model::now(),
            entries,
        }
    }
    /// An index this version can read; a missing, corrupt or newer one is rebuilt.
    pub fn parse(body: &[u8]) -> Option<Self> {
        serde_json::from_slice::<Self>(body)
            .ok()
            .filter(|index| index.schema == SCHEMA)
    }
    /// Adds or replaces the entry for `metadata.key`.
    pub fn upsert(self, metadata: ArchiveMetadata) -> Self {
        let mut entries = self.entries;
        entries.retain(|entry| entry.key != metadata.key);
        entries.push(metadata);
        Self::new(entries)
    }
}

impl ArchiveMetadata {
    pub fn parse(body: &[u8]) -> Option<Self> {
        serde_json::from_slice::<Self>(body)
            .ok()
            .filter(|metadata| metadata.schema == SCHEMA)
    }
    /// Whether the labels an archive was imported with say the same as this file.
    pub fn agrees(&self, release: Option<&str>, role: Option<TemplateRole>) -> Result<()> {
        ensure!(
            self.role == role,
            "the archive holds {} but its metadata file says {}",
            describe(role),
            describe(self.role)
        );
        if let (Some(listed), Some(release)) = (&self.release, release) {
            ensure!(
                listed == release,
                "the archive is OpenStack {release} but its metadata file says {listed}"
            );
        }
        Ok(())
    }
}
fn describe(role: Option<TemplateRole>) -> &'static str {
    match role {
        Some(TemplateRole::Control) => "a control template",
        Some(TemplateRole::Compute) => "a compute template",
        None => "a stackbox snapshot",
    }
}

/// `<name>.json` for `<name>.msb`.
pub fn metadata_key(archive: &str) -> String {
    format!("{}.json", archive.strip_suffix(".msb").unwrap_or(archive))
}
pub fn index_key(prefix: &str) -> String {
    format!("{prefix}{INDEX}")
}

/// Templates a bucket lookup is narrowed to. Unset fields match anything.
#[derive(Clone, Debug, Default, Deserialize, ToSchema)]
pub struct TemplateQuery {
    #[serde(default)]
    pub release: Option<String>,
    #[serde(default)]
    pub role: Option<TemplateRole>,
    /// Defaults to this host's, since a guest boots on its own architecture only.
    #[serde(default)]
    pub arch: Option<String>,
    /// The stackbox's services; a template matches only the same set, in any order.
    #[serde(default)]
    pub components: Option<Vec<String>>,
}
impl TemplateQuery {
    pub fn matches(&self, metadata: &ArchiveMetadata) -> bool {
        let sorted = |components: &[String]| {
            let mut components = components.to_vec();
            components.sort();
            components
        };
        let arch = self.arch.clone().unwrap_or_else(crate::model::current_arch);
        metadata.role.is_some()
            && self.role.is_none_or(|role| metadata.role == Some(role))
            && self
                .release
                .as_ref()
                .is_none_or(|release| metadata.release.as_ref() == Some(release))
            && metadata.arch == arch
            && self
                .components
                .as_ref()
                .is_none_or(|c| sorted(c) == sorted(&metadata.components))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn metadata(key: &str, role: Option<TemplateRole>) -> ArchiveMetadata {
        ArchiveMetadata {
            schema: SCHEMA,
            key: key.into(),
            size_bytes: 3,
            sha256: "00".into(),
            snapshot_id: "s".into(),
            release: Some("2025.1".into()),
            role,
            node: None,
            components: vec!["keystone".into(), "nova".into()],
            computes: 0,
            compute: None,
            gpu: None,
            arch: crate::model::current_arch(),
            platform: None,
            deployed: true,
            source_stackbox: None,
            note: None,
            created_at: 1,
            exported_at: 2,
        }
    }

    #[test]
    fn keys_and_versions() {
        assert_eq!(metadata_key("p/a.msb"), "p/a.json");
        assert_eq!(index_key("p/"), "p/index.json");
        let body = serde_json::to_vec(&metadata("p/a.msb", None)).unwrap();
        assert!(ArchiveMetadata::parse(&body).is_some());
        let mut newer = metadata("p/a.msb", None);
        newer.schema = SCHEMA + 1;
        assert!(ArchiveMetadata::parse(&serde_json::to_vec(&newer).unwrap()).is_none());
        assert!(BucketIndex::parse(b"not json").is_none());
        let index = BucketIndex::new(vec![metadata("p/a.msb", None)])
            .upsert(metadata("p/b.msb", None))
            .upsert(metadata("p/a.msb", Some(TemplateRole::Control)));
        assert_eq!(index.entries.len(), 2);
        assert_eq!(index.entries[0].role, Some(TemplateRole::Control));
    }

    #[test]
    fn queries_match_templates_only() {
        let control = metadata("c.msb", Some(TemplateRole::Control));
        let query = |role, release: &str| TemplateQuery {
            release: Some(release.into()),
            role,
            arch: None,
            components: Some(vec!["nova".into(), "keystone".into()]),
        };
        assert!(query(Some(TemplateRole::Control), "2025.1").matches(&control));
        assert!(query(None, "2025.1").matches(&control));
        assert!(!query(Some(TemplateRole::Compute), "2025.1").matches(&control));
        assert!(!query(None, "2024.2").matches(&control));
        assert!(!TemplateQuery::default().matches(&metadata("f.msb", None)));
        let other_arch = TemplateQuery {
            arch: Some("riscv64".into()),
            ..Default::default()
        };
        assert!(!other_arch.matches(&control));
        assert!(
            control
                .agrees(Some("2025.1"), Some(TemplateRole::Control))
                .is_ok()
        );
        assert!(control.agrees(Some("2025.1"), None).is_err());
        assert!(
            control
                .agrees(Some("2024.2"), Some(TemplateRole::Control))
                .is_err()
        );
    }
}
