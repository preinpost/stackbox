//! Copies of single-VM snapshots on other hosts, so a farm or compute there can boot from a
//! template the local host froze or imported.
//!
//! [`FarmManager::ensure_snapshot_on_host`] is the one way in. It asks the host's agent
//! whether its copy is still there, and otherwise brings one over, cheapest first:
//!
//! 1. The host downloads the archive from a registered bucket itself, when the snapshot
//!    was exported to one or a bucket's metadata files list it. The coordinator sends a
//!    presigned GET URL for that one object, never the bucket's secret key, which stays
//!    sealed in the vault. The agent checks the size and SHA-256 the metadata recorded.
//! 2. When there is no such archive, or the host cannot reach the bucket or got other
//!    bytes, the coordinator exports the snapshot from the local host and streams it over
//!    the agent's own connection (SSH, the dial-in socket, or a pipe), as a shell's input
//!    travels. The agent checks its size and SHA-256 against what was exported.
//!
//! Only a verified archive is loaded. The copy is recorded on the snapshot (`copies`), and
//! deleting the snapshot removes the copies too, as far as their hosts can be reached.

use super::{Backend, EventSink, FarmManager, HostKind, NotFound, conflict};
use crate::{
    kolla::DeployEvent,
    model::{CopySource, SnapshotCopy, SnapshotRecord, SnapshotStatus, now},
};
use anyhow::{Context, Result, anyhow};
use std::{path::Path, sync::Arc};

/// How long a presigned URL lets a host download an archive.
const PRESIGNED_FOR: u64 = 6 * 3600;
/// Streamed progress goes to the job log at most this often.
const REPORT_EVERY: u64 = 256 * 1024 * 1024;

/// An archive in a bucket a host can download the snapshot from.
struct BucketArchive {
    bucket: String,
    key: String,
    sha256: String,
    size_bytes: u64,
}

fn say(sink: &Option<EventSink>, message: String) {
    if let Some(sink) = sink {
        let _ = sink(DeployEvent::Output(message));
    }
}

impl FarmManager {
    /// Make sure host `host_id` keeps snapshot `snapshot_id`, copying it there if needed,
    /// and return its msb reference on that host. On the local host this is the snapshot's
    /// own reference. Calls for the same snapshot and host wait for each other, and a copy
    /// already there is reused, so this can be called before every use. Progress goes to
    /// `sink`, such as a job's log.
    pub async fn ensure_snapshot_on_host(
        &self,
        snapshot_id: &str,
        host_id: &str,
        sink: Option<EventSink>,
    ) -> Result<String> {
        let (snapshot, kind) = self.read(|state| {
            let snapshot = state
                .snapshots
                .get(snapshot_id)
                .ok_or_else(|| NotFound(format!("snapshot '{snapshot_id}'")))?;
            let host = state
                .hosts
                .get(host_id)
                .ok_or_else(|| NotFound(format!("host '{host_id}'")))?;
            Ok::<_, anyhow::Error>((snapshot.clone(), host.host.clone()))
        })??;
        let reference = match (&snapshot.status, &snapshot.reference) {
            (SnapshotStatus::Ready | SnapshotStatus::Exporting, Some(reference)) => {
                reference.clone()
            }
            _ => return Err(conflict(format!("snapshot '{snapshot_id}' is not ready"))),
        };
        if kind.kind == HostKind::Local {
            return Ok(reference);
        }
        if !snapshot.nodes.is_empty() {
            return Err(conflict(format!(
                "snapshot '{snapshot_id}' holds several VMs; only single-VM snapshots and templates can be copied to another host"
            )));
        }
        if let Some(arch) = kind.inspection.as_ref().map(|i| i.arch.as_str())
            && arch != snapshot.arch
        {
            return Err(conflict(format!(
                "snapshot '{snapshot_id}' was frozen on {} but host '{host_id}' is {arch}",
                snapshot.arch
            )));
        }
        let lock = {
            let mut locks = self
                .0
                .copy_locks
                .lock()
                .map_err(|_| anyhow!("copy lock poisoned"))?;
            locks
                .entry(format!("{snapshot_id}\n{host_id}"))
                .or_default()
                .clone()
        };
        let _copying = lock.lock().await;
        let agent = self.host_agent(host_id)?;
        if !agent.supports_template_transfer().await? {
            return Err(conflict(format!(
                "the farm-agent on host '{host_id}' predates template transfers; {}",
                match kind.kind {
                    HostKind::Agent => "update farm-agent there and connect it again",
                    _ => "install the agent again from Hosts",
                }
            )));
        }
        if let Some(found) = agent.find_snapshot(snapshot_id.into()).await? {
            self.record_copy(snapshot_id, host_id, &found, CopySource::Found, None)?;
            return Ok(found);
        }
        if let Some(sink) = &sink {
            let _ = sink(DeployEvent::Step(format!(
                "copying snapshot {snapshot_id} to host {host_id}"
            )));
        }
        let transfer_id = uuid::Uuid::new_v4().to_string();
        let mut via = None;
        if let Some(archive) = self.bucket_archive(&snapshot).await {
            say(
                &sink,
                format!(
                    "host {host_id} downloads {} from bucket {} ({} MiB)\n",
                    archive.key,
                    archive.bucket,
                    archive.size_bytes / (1024 * 1024)
                ),
            );
            let fetched = async {
                let (bucket, client) = self.s3_client(&archive.bucket)?;
                let url = client.presign_get(&archive.key, PRESIGNED_FOR)?;
                agent
                    .fetch_archive(
                        transfer_id.clone(),
                        url.to_string(),
                        bucket.ca_pem.clone(),
                        archive.sha256.clone(),
                        archive.size_bytes,
                        sink.clone().unwrap_or_else(|| Arc::new(|_| Ok(()))),
                    )
                    .await
            }
            .await;
            match fetched {
                Ok(()) => via = Some(CopySource::Bucket),
                Err(error) => say(
                    &sink,
                    format!(
                        "host {host_id} could not download it from the bucket ({error:#}); streaming it from this coordinator instead\n"
                    ),
                ),
            }
        }
        if via.is_none() {
            self.stream_snapshot(&agent, &snapshot, reference, &transfer_id, host_id, &sink)
                .await?;
            via = Some(CopySource::Stream);
        }
        say(
            &sink,
            format!("loading snapshot {snapshot_id} on host {host_id}\n"),
        );
        let imported = agent
            .import_archive(transfer_id, snapshot_id.into())
            .await
            .with_context(|| format!("load snapshot {snapshot_id} on host {host_id}"))?;
        self.record_copy(
            snapshot_id,
            host_id,
            &imported.reference,
            via.unwrap_or(CopySource::Stream),
            imported.size_bytes,
        )?;
        say(
            &sink,
            format!("snapshot {snapshot_id} is on host {host_id}\n"),
        );
        Ok(imported.reference)
    }

    /// Remove the copy of a snapshot from `host_id`. A host that is no longer registered
    /// only loses the record.
    pub async fn remove_snapshot_copy(
        &self,
        snapshot_id: &str,
        host_id: &str,
    ) -> Result<SnapshotRecord> {
        let (copy, registered) = self.read(|state| {
            if state
                .jobs
                .values()
                .any(|j| j.status.active() && j.snapshot_id.as_deref() == Some(snapshot_id))
            {
                return Err(conflict(format!(
                    "snapshot '{snapshot_id}' is in use by a running job"
                )));
            }
            let snapshot = state
                .snapshots
                .get(snapshot_id)
                .ok_or_else(|| NotFound(format!("snapshot '{snapshot_id}'")))?;
            let copy = snapshot
                .copies
                .iter()
                .find(|copy| copy.host_id == host_id)
                .cloned()
                .ok_or_else(|| {
                    NotFound(format!(
                        "copy of snapshot '{snapshot_id}' on host '{host_id}'"
                    ))
                })?;
            Ok((copy, state.hosts.contains_key(host_id)))
        })??;
        if registered {
            self.host_agent(host_id)?
                .remove_snapshot(copy.reference.clone())
                .await
                .with_context(|| format!("remove snapshot {snapshot_id} from host {host_id}"))?;
        }
        self.update(|state| {
            let snapshot = state
                .snapshots
                .get_mut(snapshot_id)
                .ok_or_else(|| NotFound(format!("snapshot '{snapshot_id}'")))?;
            snapshot.copies.retain(|copy| copy.host_id != host_id);
            snapshot.updated_at = now();
            Ok(snapshot.clone())
        })
    }

    /// Remove every copy of a snapshot that is being deleted, as far as the hosts answer.
    /// Returns what could not be removed.
    pub(super) async fn remove_snapshot_copies(&self, copies: &[SnapshotCopy]) -> Vec<String> {
        let mut left = Vec::new();
        for copy in copies {
            let removed = async {
                if !self.read(|state| state.hosts.contains_key(&copy.host_id))? {
                    return Ok(());
                }
                self.host_agent(&copy.host_id)?
                    .remove_snapshot(copy.reference.clone())
                    .await
            }
            .await;
            if let Err(error) = removed {
                left.push(format!("{}: {error:#}", copy.host_id));
            }
        }
        left
    }

    /// The host's agent for operations that belong to no farm.
    fn host_agent(&self, host_id: &str) -> Result<crate::remote::backend::AgentBackend> {
        let digest = self.read(|state| {
            state
                .hosts
                .get(host_id)
                .and_then(|host| host.host.agent.as_ref())
                .map(|agent| agent.artifact.sha256.clone())
        })?;
        self.agent(host_id, digest.as_deref(), "")
    }

    fn record_copy(
        &self,
        snapshot_id: &str,
        host_id: &str,
        reference: &str,
        via: CopySource,
        size_bytes: Option<u64>,
    ) -> Result<()> {
        self.update(|state| {
            let snapshot = state
                .snapshots
                .get_mut(snapshot_id)
                .context("snapshot disappeared during its copy")?;
            // A copy found again keeps how it first got there.
            if via == CopySource::Found
                && snapshot
                    .copies
                    .iter()
                    .any(|copy| copy.host_id == host_id && copy.reference == reference)
            {
                return Ok(());
            }
            snapshot.copies.retain(|copy| copy.host_id != host_id);
            snapshot.copies.push(SnapshotCopy {
                host_id: host_id.into(),
                reference: reference.into(),
                via,
                size_bytes: size_bytes.or(snapshot.size_bytes),
                copied_at: now(),
            });
            snapshot.updated_at = now();
            Ok(())
        })
    }

    /// The bucket archive of `snapshot` with a known SHA-256: its last export to a bucket,
    /// or an archive whose metadata file names the same snapshot.
    async fn bucket_archive(&self, snapshot: &SnapshotRecord) -> Option<BucketArchive> {
        if let Some(archive) = &snapshot.archive
            && let (Some(bucket), Some(key), Some(sha256)) =
                (&archive.bucket, &archive.key, &archive.sha256)
            && self.bucket(bucket).is_ok()
        {
            return Some(BucketArchive {
                bucket: bucket.clone(),
                key: key.clone(),
                sha256: sha256.clone(),
                size_bytes: archive.size_bytes,
            });
        }
        // Only templates are worth a bucket listing; their metadata says what they hold.
        snapshot.template?;
        for bucket in self.buckets().ok()? {
            let objects = match self.bucket_objects(&bucket.id).await {
                Ok(objects) => objects,
                Err(error) => {
                    eprintln!(
                        "cannot list bucket {} for a template copy: {error:#}",
                        bucket.id
                    );
                    continue;
                }
            };
            let found = objects.into_iter().filter_map(|o| o.metadata).find(|m| {
                m.snapshot_id == snapshot.id
                    && m.role == snapshot.template
                    && m.node == snapshot.node
                    && m.arch == snapshot.arch
                    && m.release == snapshot.release
                    && m.components == snapshot.plan.components
            });
            if let Some(metadata) = found {
                return Some(BucketArchive {
                    bucket: bucket.id,
                    key: metadata.key,
                    sha256: metadata.sha256,
                    size_bytes: metadata.size_bytes,
                });
            }
        }
        None
    }

    /// Export the snapshot on the local host and stream the archive to the host's agent.
    async fn stream_snapshot(
        &self,
        agent: &crate::remote::backend::AgentBackend,
        snapshot: &SnapshotRecord,
        reference: String,
        transfer_id: &str,
        host_id: &str,
        sink: &Option<EventSink>,
    ) -> Result<()> {
        let path = self
            .state_dir()
            .join("transfers")
            .join(format!("{transfer_id}.msb"));
        let streamed = async {
            std::fs::create_dir_all(path.parent().context("transfer path has no directory")?)?;
            {
                // Exports read whole VM disks, one at a time.
                let _permit = self.0.snapshot_gate.acquire().await?;
                say(
                    sink,
                    format!("exporting snapshot {} on the local host\n", snapshot.id),
                );
                self.local_backend()?
                    .export_snapshot(reference, path.clone())
                    .await?;
            }
            let (sha256, size_bytes) = digest(&path).await?;
            say(
                sink,
                format!(
                    "streaming {} MiB to host {host_id}\n",
                    size_bytes / (1024 * 1024)
                ),
            );
            let mut reported = 0;
            agent
                .send_archive(
                    transfer_id.into(),
                    path.clone(),
                    sha256,
                    size_bytes,
                    |sent| {
                        if sent >= reported + REPORT_EVERY || sent == size_bytes {
                            reported = sent;
                            say(
                                sink,
                                format!(
                                    "streamed {} of {} MiB to host {host_id}\n",
                                    sent / (1024 * 1024),
                                    size_bytes / (1024 * 1024)
                                ),
                            );
                        }
                    },
                )
                .await
        }
        .await;
        let _ = std::fs::remove_file(&path);
        streamed.with_context(|| format!("stream snapshot {} to host {host_id}", snapshot.id))
    }
}

/// Hex SHA-256 and size of a file.
async fn digest(path: &Path) -> Result<(String, u64)> {
    let path = path.to_owned();
    tokio::task::spawn_blocking(move || {
        use sha2::{Digest, Sha256};
        let mut file = std::fs::File::open(path)?;
        let mut hash = Sha256::new();
        let size = std::io::copy(&mut file, &mut hash)?;
        Ok((format!("{:x}", hash.finalize()), size))
    })
    .await?
}

/// Whether a snapshot can boot on a host other than the local one: a single VM, which is
/// copied there first.
pub(super) fn copyable(snapshot: &SnapshotRecord) -> Result<()> {
    if !snapshot.nodes.is_empty() {
        return Err(conflict(format!(
            "snapshot '{}' holds several VMs, which start on the local host only",
            snapshot.id
        )));
    }
    Ok(())
}
