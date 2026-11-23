//! Copies of single-VM snapshots on other hosts, so a farm or compute there can boot from a
//! template another host froze. The original stays on the host that froze it, or on the
//! local host for snapshots it imported and ones recorded before snapshots had a host.
//!
//! [`FarmManager::ensure_snapshot_on_host`] is the one way in. It asks the host's agent
//! whether its copy is still there, and otherwise brings one over, cheapest first:
//!
//! 1. The host downloads the archive from a registered bucket itself, when the snapshot
//!    was exported to one or a bucket's metadata files list it. The coordinator sends a
//!    presigned GET URL for that one object, never the bucket's secret key, which stays
//!    sealed in the vault. The agent checks the size and SHA-256 the metadata recorded.
//! 2. When there is no such archive, or the host cannot reach the bucket or got other
//!    bytes, the coordinator exports the original and streams it over the agent's own
//!    connection (SSH, the dial-in socket, or a pipe), as a shell's input travels. The agent
//!    checks its size and SHA-256 against what was exported. An original on another remote
//!    host reaches the coordinator the same way first, as that host's shell output would.
//!
//! Only a verified archive is loaded. The copy is recorded on the snapshot (`copies`), and
//! deleting the snapshot removes the copies too, as far as their hosts can be reached.

use super::{Backend, EventSink, FarmManager, HostKind, NotFound, conflict, store::State};
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

/// The registered host that keeps a snapshot's original: the remote host it was frozen on,
/// or the local host. `None` while that host is not registered.
pub(super) fn origin_host(state: &State, snapshot: &SnapshotRecord) -> Option<String> {
    match &snapshot.host_id {
        Some(id) => state.hosts.contains_key(id).then(|| id.clone()),
        None => state
            .hosts
            .values()
            .find(|stored| stored.host.kind == HostKind::Local)
            .map(|stored| stored.host.id.clone()),
    }
}
/// Why a snapshot whose original's host is not registered cannot be read.
pub(super) fn origin_missing(snapshot: &SnapshotRecord) -> anyhow::Error {
    match &snapshot.host_id {
        Some(id) => conflict(format!(
            "snapshot '{}' is kept on host '{id}', which is not registered; register that host again to use it",
            snapshot.id
        )),
        None => conflict("register the local host under Hosts to keep snapshots on it"),
    }
}
/// CPU architecture of a registered host, as its last check found it; the local host is
/// this machine. `None` for a remote host not checked yet.
pub(super) fn host_arch(state: &State, host_id: &str) -> Option<String> {
    let host = &state.hosts.get(host_id)?.host;
    match (&host.inspection, host.kind) {
        (Some(inspection), _) => Some(inspection.arch.clone()),
        (None, HostKind::Local) => Some(crate::model::current_arch()),
        (None, _) => None,
    }
}

impl FarmManager {
    /// Make sure host `host_id` keeps snapshot `snapshot_id`, copying it there if needed,
    /// and return its msb reference on that host. On the host that keeps the original this
    /// is the snapshot's own reference. Calls for the same snapshot and host wait for each other, and a copy
    /// already there is reused, so this can be called before every use. Progress goes to
    /// `sink`, such as a job's log.
    pub async fn ensure_snapshot_on_host(
        &self,
        snapshot_id: &str,
        host_id: &str,
        sink: Option<EventSink>,
    ) -> Result<String> {
        let (snapshot, kind, origin, arch) = self.read(|state| {
            let snapshot = state
                .snapshots
                .get(snapshot_id)
                .ok_or_else(|| NotFound(format!("snapshot '{snapshot_id}'")))?;
            let host = state
                .hosts
                .get(host_id)
                .ok_or_else(|| NotFound(format!("host '{host_id}'")))?;
            Ok::<_, anyhow::Error>((
                snapshot.clone(),
                host.host.clone(),
                origin_host(state, snapshot),
                host_arch(state, host_id),
            ))
        })??;
        let reference = match (&snapshot.status, &snapshot.reference) {
            (SnapshotStatus::Ready | SnapshotStatus::Exporting, Some(reference)) => {
                reference.clone()
            }
            _ => return Err(conflict(format!("snapshot '{snapshot_id}' is not ready"))),
        };
        if origin.as_deref() == Some(host_id) {
            return Ok(reference);
        }
        if !snapshot.nodes.is_empty() {
            return Err(conflict(format!(
                "snapshot '{snapshot_id}' holds several VMs; only single-VM snapshots and templates can be copied to another host"
            )));
        }
        if let Some(arch) = arch
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
            // Only now is the original needed; a host down keeps other hosts' copies usable.
            if origin.is_none() {
                return Err(origin_missing(&snapshot));
            }
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

    /// The agent of the host that keeps a snapshot's original.
    pub(super) fn origin_agent(&self, snapshot: &SnapshotRecord) -> Result<Arc<dyn Backend>> {
        let host_id = self
            .read(|state| origin_host(state, snapshot))?
            .ok_or_else(|| origin_missing(snapshot))?;
        Ok(Arc::new(self.host_agent(&host_id)?))
    }
    /// Write the archive of a snapshot's original to `path` on this machine. A single-VM
    /// snapshot is exported directly; a multi-node snapshot exports each VM's archive and
    /// packs them into a bundle tar.
    pub(super) async fn export_original(
        &self,
        snapshot: &SnapshotRecord,
        reference: String,
        path: &Path,
        sink: &Option<EventSink>,
    ) -> Result<()> {
        if snapshot.nodes.is_empty() {
            return self.export_one(snapshot, reference, path, sink).await;
        }
        let stage_dir = self
            .state_dir()
            .join("transfers")
            .join(format!("{}.bundle-export", snapshot.id));
        let _ = std::fs::remove_dir_all(&stage_dir);
        std::fs::create_dir_all(&stage_dir)?;
        let packed = async {
            let mut entries = Vec::with_capacity(snapshot.nodes.len());
            for node in &snapshot.nodes {
                let node_path = stage_dir.join(format!("{}.msb", node.node));
                self.export_one(snapshot, node.reference.clone(), &node_path, sink)
                    .await
                    .with_context(|| {
                        format!("export VM '{}' of snapshot {}", node.node, snapshot.id)
                    })?;
                entries.push((node.node.clone(), node_path));
            }
            let output = path.to_owned();
            let compute = snapshot.plan.compute;
            let gpu = snapshot.plan.gpu.clone();
            tokio::task::spawn_blocking(move || {
                super::bundle::pack(&output, &entries, compute, gpu)
            })
            .await??;
            Ok::<_, anyhow::Error>(())
        }
        .await;
        let _ = std::fs::remove_dir_all(&stage_dir);
        packed
    }

    /// Write one msb snapshot reference of `snapshot`'s original to `path` on this machine.
    /// The local host exports it there itself; another host exports it on its side and
    /// streams it over.
    async fn export_one(
        &self,
        snapshot: &SnapshotRecord,
        reference: String,
        path: &Path,
        sink: &Option<EventSink>,
    ) -> Result<()> {
        let host_id = self
            .read(|state| origin_host(state, snapshot))?
            .ok_or_else(|| origin_missing(snapshot))?;
        say(
            sink,
            format!("exporting snapshot {} on host {host_id}\n", snapshot.id),
        );
        let agent = self.host_agent(&host_id)?;
        if snapshot.host_id.is_none() {
            return agent.export_snapshot(reference, path.to_owned()).await;
        }
        let transfer_id = uuid::Uuid::new_v4().to_string();
        let exported = agent
            .export_archive(transfer_id.clone(), reference)
            .await
            .with_context(|| format!("export snapshot {} on host {host_id}", snapshot.id))?;
        let size_bytes = exported.1;
        say(
            sink,
            format!(
                "streaming {} MiB from host {host_id}\n",
                size_bytes / (1024 * 1024)
            ),
        );
        let mut reported = 0;
        agent
            .receive_archive(transfer_id, path.to_owned(), exported, |received| {
                if received >= reported + REPORT_EVERY {
                    reported = received;
                    say(
                        sink,
                        format!(
                            "streamed {} of {} MiB from host {host_id}\n",
                            received / (1024 * 1024),
                            size_bytes / (1024 * 1024)
                        ),
                    );
                }
            })
            .await
            .with_context(|| format!("stream snapshot {} from host {host_id}", snapshot.id))
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

    /// Export the original and stream the archive to the host's agent.
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
                self.export_original(snapshot, reference, &path, sink)
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

/// Whether a snapshot can boot on a host other than the one keeping its original: a single
/// VM, which is copied there first.
pub(super) fn copyable(snapshot: &SnapshotRecord) -> Result<()> {
    if !snapshot.nodes.is_empty() {
        return Err(conflict(format!(
            "snapshot '{}' holds several VMs, which start only on the host it was frozen on",
            snapshot.id
        )));
    }
    Ok(())
}
