//! Snapshots: freeze a farm's VM, restore farms from it, and move it between hosts as an archive.

use super::{FarmManager, NotFound, bad_request, conflict, ensure_idle, store::State};
use crate::{
    farm::Role,
    model::{snapshot_labels as label, *},
};
use anyhow::{Context, Result, anyhow};
use std::{
    collections::BTreeMap,
    net::{IpAddr, Ipv4Addr},
    path::{Path, PathBuf},
};

pub(super) fn busy(status: SnapshotStatus) -> bool {
    matches!(
        status,
        SnapshotStatus::Creating
            | SnapshotStatus::Exporting
            | SnapshotStatus::Importing
            | SnapshotStatus::Deleting
    )
}

/// A snapshot a new farm can start from. Exporting only reads it.
pub(super) fn restorable<'a>(state: &'a State, id: &str) -> Result<&'a SnapshotRecord> {
    let snapshot = state
        .snapshots
        .get(id)
        .ok_or_else(|| NotFound(format!("snapshot '{id}'")))?;
    if !matches!(
        snapshot.status,
        SnapshotStatus::Ready | SnapshotStatus::Exporting
    ) || snapshot.reference.is_none()
    {
        return Err(conflict(format!("snapshot '{id}' is not ready")));
    }
    Ok(snapshot)
}

/// Operations cut short by a daemon stop cannot resume; leave each snapshot in a state the user can act on.
pub(super) fn recover_interrupted(state: &mut State) {
    for snapshot in state.snapshots.values_mut() {
        let (status, error) = match snapshot.status {
            SnapshotStatus::Creating => (
                SnapshotStatus::Failed,
                "daemon stopped while capturing; check that the source farm's VM is running, then delete this snapshot",
            ),
            SnapshotStatus::Importing => (
                SnapshotStatus::Failed,
                "daemon stopped while importing; delete this snapshot and import again",
            ),
            SnapshotStatus::Exporting => (
                SnapshotStatus::Ready,
                "daemon stopped while exporting; export again",
            ),
            SnapshotStatus::Deleting => (
                SnapshotStatus::Failed,
                "daemon stopped while deleting; delete again",
            ),
            SnapshotStatus::Ready | SnapshotStatus::Failed => continue,
        };
        snapshot.status = status;
        snapshot.error = Some(error.into());
        snapshot.updated_at = now();
    }
}

fn labels(snapshot: &SnapshotRecord) -> BTreeMap<String, String> {
    let mut labels = BTreeMap::from([
        (label::ID.into(), snapshot.id.clone()),
        (label::COMPONENTS.into(), snapshot.plan.components.join(",")),
        (label::DEPLOYED.into(), snapshot.deployed.to_string()),
        (
            label::EXPOSURE_HOST.into(),
            snapshot.source_exposure.host.clone(),
        ),
        (
            label::EXPOSURE_PORT_BASE.into(),
            snapshot.source_exposure.port_base.to_string(),
        ),
        (
            label::RELEASE.into(),
            crate::kolla::OPENSTACK_RELEASE.into(),
        ),
        // The guest image is built for the host architecture.
        (label::ARCH.into(), std::env::consts::ARCH.into()),
    ]);
    if let Some(source) = &snapshot.source_farm {
        labels.insert(label::SOURCE.into(), source.clone());
    }
    if let Some(note) = &snapshot.note {
        labels.insert(label::NOTE.into(), note.clone());
    }
    labels
}

/// Metadata of an imported archive, read back from its labels.
struct Imported {
    plan: Plan,
    deployed: bool,
    source_exposure: Exposure,
    source_farm: Option<String>,
    note: Option<String>,
}
fn parse_labels(labels: &BTreeMap<String, String>) -> Result<Imported> {
    let get = |key: &str| {
        labels
            .get(key)
            .with_context(|| format!("archive has no {key} label; it was not frozen by farm"))
    };
    let arch = get(label::ARCH)?;
    anyhow::ensure!(
        arch == std::env::consts::ARCH,
        "archive was frozen on {arch}; this host is {}",
        std::env::consts::ARCH
    );
    let components: Vec<String> = get(label::COMPONENTS)?
        .split(',')
        .map(str::to_owned)
        .collect();
    Ok(Imported {
        plan: plan(&PlanRequest { components })?,
        deployed: get(label::DEPLOYED)? == "true",
        source_exposure: Exposure {
            host: get(label::EXPOSURE_HOST)?.clone(),
            bind: IpAddr::V4(Ipv4Addr::LOCALHOST),
            port_base: get(label::EXPOSURE_PORT_BASE)?
                .parse()
                .context("invalid exposure port label")?,
        },
        source_farm: labels.get(label::SOURCE).cloned(),
        note: labels.get(label::NOTE).cloned(),
    })
}

impl FarmManager {
    pub fn snapshots(&self) -> Result<Vec<SnapshotRecord>> {
        Ok(self
            .0
            .store
            .lock()
            .map_err(|_| anyhow!("state mutex poisoned"))?
            .state
            .snapshots
            .values()
            .cloned()
            .collect())
    }
    pub fn snapshot(&self, id: &str) -> Result<SnapshotRecord> {
        self.0
            .store
            .lock()
            .map_err(|_| anyhow!("state mutex poisoned"))?
            .state
            .snapshots
            .get(id)
            .cloned()
            .ok_or_else(|| NotFound(format!("snapshot '{id}'")).into())
    }
    fn update_snapshot(&self, id: &str, f: impl FnOnce(&mut SnapshotRecord)) -> Result<()> {
        self.update(|state| {
            let snapshot = state.snapshots.get_mut(id).context("snapshot missing")?;
            f(snapshot);
            snapshot.updated_at = now();
            Ok(())
        })
    }
    /// Polls until no capture, export, import or deletion is running on the snapshot.
    pub async fn wait_snapshot(&self, id: &str) -> Result<SnapshotRecord> {
        loop {
            let snapshot = self.snapshot(id)?;
            if !busy(snapshot.status) {
                return Ok(snapshot);
            }
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        }
    }

    /// Freeze the farm's VM into a new snapshot. The VM stops for the capture and starts again.
    pub fn freeze(&self, farm_id: &str, request: SnapshotRequest) -> Result<Job> {
        request.validate().map_err(|e| bad_request(e.to_string()))?;
        let job = self.update(|state| {
            self.ensure_accepting()?;
            if state.snapshots.contains_key(&request.id) {
                return Err(conflict(format!(
                    "snapshot '{}' already exists",
                    request.id
                )));
            }
            let farm = state
                .farms
                .get_mut(farm_id)
                .ok_or_else(|| NotFound(format!("farm '{farm_id}'")))?;
            ensure_idle(farm)?;
            if farm.host_id.is_some() {
                return Err(conflict("snapshots are available for local farms only"));
            }
            if !matches!(
                farm.status,
                FarmStatus::Created | FarmStatus::Ready | FarmStatus::Failed
            ) {
                return Err(conflict("farm must be created before freezing"));
            }
            let (Some(plan), Some(exposure)) = (&farm.plan, &farm.exposure) else {
                return Err(conflict(
                    "farms imported from older versions cannot be frozen",
                ));
            };
            if farm.nodes.len() != 1
                || farm.nodes[0].role != Some(Role::AllInOne)
                || farm.nodes[0].status != "running"
            {
                return Err(conflict("the farm's VM must be running to freeze it"));
            }
            let snapshot = SnapshotRecord {
                id: request.id.clone(),
                status: SnapshotStatus::Creating,
                source_farm: Some(farm_id.into()),
                plan: plan.clone(),
                deployed: farm.status == FarmStatus::Ready,
                source_exposure: exposure.clone(),
                note: request.note.clone().filter(|n| !n.trim().is_empty()),
                reference: None,
                size_bytes: None,
                archive: None,
                error: None,
                created_at: now(),
                updated_at: now(),
            };
            let mut job = super::new_job(farm_id, JobKind::Snapshot);
            job.snapshot_id = Some(request.id.clone());
            farm.active_job = Some(job.id.clone());
            farm.error = None;
            farm.updated_at = now();
            state.snapshots.insert(snapshot.id.clone(), snapshot);
            state.jobs.insert(job.id.clone(), job.clone());
            Ok(job)
        })?;
        self.spawn(job.clone(), None);
        Ok(job)
    }

    pub(super) async fn freeze_job(&self, job: &Job, farm: FarmRecord) -> Result<()> {
        let id = job.snapshot_id.clone().context("snapshot job has no id")?;
        let snapshot = self.snapshot(&id)?;
        self.log(
            &farm.id,
            &job.id,
            None,
            "stopping the VM to capture its disks\n".into(),
        )?;
        let frozen = match self
            .0
            .backend
            .freeze(farm.id.clone(), id.clone(), labels(&snapshot))
            .await
        {
            Ok(frozen) => frozen,
            Err(error) => {
                let message = format!("{error:#}");
                self.update_snapshot(&id, |s| {
                    s.status = SnapshotStatus::Failed;
                    s.error = Some(message);
                })?;
                return Err(error);
            }
        };
        self.update_snapshot(&id, |s| {
            s.status = SnapshotStatus::Ready;
            s.reference = Some(frozen.reference);
            s.size_bytes = frozen.size_bytes;
        })?;
        self.log(
            &farm.id,
            &job.id,
            None,
            format!("snapshot {id} captured; the VM started again\n"),
        )?;
        if let Some(error) = frozen.restart_error {
            return Err(anyhow!(
                "snapshot {id} is ready, but the farm's VM did not start again: {error}"
            ));
        }
        if snapshot.deployed {
            self.0
                .backend
                .settle(
                    farm.id.clone(),
                    None,
                    snapshot.plan.components.clone(),
                    self.event_sink(job),
                )
                .await
                .with_context(|| {
                    format!("snapshot {id} is ready, but the farm's services did not answer again")
                })?;
        }
        Ok(())
    }

    pub(super) async fn restore_job(&self, job: &Job, farm: FarmRecord) -> Result<()> {
        let id = farm
            .snapshot_id
            .clone()
            .context("restored farm has no snapshot")?;
        let snapshot = self.snapshot(&id)?;
        let reference = snapshot
            .reference
            .clone()
            .ok_or_else(|| conflict(format!("snapshot '{id}' is not ready")))?;
        let plan = farm.plan.clone().context("farm has no plan")?;
        let exposure = farm.exposure.clone().context("farm has no exposure")?;
        self.log(
            &farm.id,
            &job.id,
            None,
            format!("starting a copy of snapshot {id}\n"),
        )?;
        let nodes = self
            .0
            .backend
            .restore(farm.id.clone(), reference, plan.clone(), exposure.clone())
            .await?;
        self.update(|state| {
            state.farms.get_mut(&farm.id).context("farm missing")?.nodes = nodes;
            Ok(())
        })?;
        if snapshot.deployed {
            self.log(
                &farm.id,
                &job.id,
                None,
                "VM started; moving public endpoints and waiting for OpenStack\n".into(),
            )?;
            self.0
                .backend
                .settle(
                    farm.id.clone(),
                    Some((snapshot.source_exposure.clone(), exposure)),
                    plan.components.clone(),
                    self.event_sink(job),
                )
                .await?;
        }
        Ok(())
    }

    /// Write the snapshot to an archive in the background. `path` may name a file or an existing directory.
    pub fn export_snapshot(&self, id: &str, request: ExportRequest) -> Result<SnapshotRecord> {
        let output = match request.path.filter(|p| !p.trim().is_empty()) {
            None => self.state_dir().join("exports").join(format!("{id}.msb")),
            Some(path) => {
                let path = PathBuf::from(path);
                if !path.is_absolute() {
                    return Err(bad_request("export path must be absolute"));
                }
                if path.is_dir() {
                    path.join(format!("{id}.msb"))
                } else {
                    if !path.parent().is_some_and(Path::is_dir) {
                        return Err(bad_request(format!(
                            "directory of {} does not exist",
                            path.display()
                        )));
                    }
                    path
                }
            }
        };
        if output.exists() {
            return Err(conflict(format!("{} already exists", output.display())));
        }
        let (snapshot, reference) = self.update(|state| {
            self.ensure_accepting()?;
            let snapshot = state
                .snapshots
                .get_mut(id)
                .ok_or_else(|| NotFound(format!("snapshot '{id}'")))?;
            let reference = match (&snapshot.status, &snapshot.reference) {
                (SnapshotStatus::Ready, Some(reference)) => reference.clone(),
                _ => return Err(conflict(format!("snapshot '{id}' is not ready"))),
            };
            snapshot.status = SnapshotStatus::Exporting;
            snapshot.error = None;
            snapshot.updated_at = now();
            Ok((snapshot.clone(), reference))
        })?;
        let manager = self.clone();
        let id = id.to_owned();
        tokio::spawn(async move {
            let result = manager.run_export(reference, &output).await;
            let saved = manager.update_snapshot(&id, |s| {
                s.status = SnapshotStatus::Ready;
                match result {
                    Ok(archive) => s.archive = Some(archive),
                    Err(error) => s.error = Some(format!("export failed: {error:#}")),
                }
            });
            if let Err(error) = saved {
                eprintln!("cannot record export of snapshot {id}: {error:#}");
            }
        });
        Ok(snapshot)
    }
    async fn run_export(&self, reference: String, output: &Path) -> Result<SnapshotArchive> {
        let _permit = self.0.snapshot_gate.acquire().await?;
        if let Some(dir) = output.parent() {
            std::fs::create_dir_all(dir)?;
        }
        // Publish the archive under its final name only once it is complete.
        let partial = PathBuf::from(format!("{}.partial", output.display()));
        let _ = std::fs::remove_file(&partial);
        let exported = self
            .0
            .backend
            .export_snapshot(reference, partial.clone())
            .await;
        if let Err(error) = exported {
            let _ = std::fs::remove_file(&partial);
            return Err(error);
        }
        std::fs::rename(&partial, output)?;
        Ok(SnapshotArchive {
            path: output.display().to_string(),
            size_bytes: std::fs::metadata(output)?.len(),
            exported_at: now(),
        })
    }

    /// Import an archive in the background. The ID defaults to the archive's file name.
    pub fn import_snapshot(&self, request: ImportRequest) -> Result<SnapshotRecord> {
        let path = PathBuf::from(&request.path);
        if !path.is_absolute() {
            return Err(bad_request("archive path must be absolute"));
        }
        if !path.is_file() {
            return Err(bad_request(format!("{} is not a file", path.display())));
        }
        let id = match request.id.filter(|id| !id.trim().is_empty()) {
            Some(id) => id,
            None => path
                .file_name()
                .and_then(|name| name.to_str())
                .map(|name| name.strip_suffix(".msb").unwrap_or(name).to_owned())
                .unwrap_or_default(),
        };
        validate_snapshot_id(&id).map_err(|e| {
            bad_request(format!(
                "{e}; choose an ID for this archive (file name '{}')",
                path.display()
            ))
        })?;
        let snapshot = self.update(|state| {
            self.ensure_accepting()?;
            if state.snapshots.contains_key(&id) {
                return Err(conflict(format!("snapshot '{id}' already exists")));
            }
            // Placeholders until the archive's labels are read.
            let snapshot = SnapshotRecord {
                id: id.clone(),
                status: SnapshotStatus::Importing,
                source_farm: None,
                plan: plan(&PlanRequest::default())?,
                deployed: false,
                source_exposure: Exposure {
                    host: "localhost".into(),
                    bind: IpAddr::V4(Ipv4Addr::LOCALHOST),
                    port_base: 0,
                },
                note: None,
                reference: None,
                size_bytes: None,
                archive: None,
                error: None,
                created_at: now(),
                updated_at: now(),
            };
            state.snapshots.insert(id.clone(), snapshot.clone());
            Ok(snapshot)
        })?;
        let manager = self.clone();
        tokio::spawn(async move {
            let result = manager.run_import(&path, &id).await;
            let saved = manager.update_snapshot(&id, |s| match result {
                Ok((imported, reference, size_bytes)) => {
                    s.status = SnapshotStatus::Ready;
                    s.plan = imported.plan;
                    s.deployed = imported.deployed;
                    s.source_exposure = imported.source_exposure;
                    s.source_farm = imported.source_farm;
                    s.note = imported.note;
                    s.reference = Some(reference);
                    s.size_bytes = size_bytes;
                }
                Err(error) => {
                    s.status = SnapshotStatus::Failed;
                    s.error = Some(format!("import failed: {error:#}"));
                }
            });
            if let Err(error) = saved {
                eprintln!("cannot record import of snapshot {id}: {error:#}");
            }
        });
        Ok(snapshot)
    }
    async fn run_import(&self, path: &Path, id: &str) -> Result<(Imported, String, Option<u64>)> {
        let _permit = self.0.snapshot_gate.acquire().await?;
        let imported = self
            .0
            .backend
            .import_snapshot(path.to_owned(), id.to_owned())
            .await?;
        match parse_labels(&imported.labels) {
            Ok(parsed) => Ok((parsed, imported.reference, imported.size_bytes)),
            Err(error) => {
                // Do not keep an msb snapshot no farm can be restored from.
                if let Err(cleanup) = self.0.backend.remove_snapshot(imported.reference).await {
                    eprintln!("cannot remove rejected import {id}: {cleanup:#}");
                }
                Err(error)
            }
        }
    }

    pub async fn delete_snapshot(&self, id: &str) -> Result<()> {
        let (reference, previous) = self.update(|state| {
            self.ensure_accepting()?;
            if state
                .jobs
                .values()
                .any(|j| j.status.active() && j.snapshot_id.as_deref() == Some(id))
            {
                return Err(conflict(format!(
                    "snapshot '{id}' is in use by a running job"
                )));
            }
            let snapshot = state
                .snapshots
                .get_mut(id)
                .ok_or_else(|| NotFound(format!("snapshot '{id}'")))?;
            if busy(snapshot.status) {
                return Err(conflict(format!("snapshot '{id}' is busy")));
            }
            let previous = snapshot.status;
            snapshot.status = SnapshotStatus::Deleting;
            snapshot.updated_at = now();
            Ok((snapshot.reference.clone(), previous))
        })?;
        if let Some(reference) = reference
            && let Err(error) = self.0.backend.remove_snapshot(reference).await
        {
            let message = format!("{error:#}");
            self.update_snapshot(id, |s| {
                s.status = previous;
                s.error = Some(format!("delete failed: {message}"));
            })?;
            return Err(error);
        }
        self.update(|state| {
            state.snapshots.remove(id);
            Ok(())
        })
    }
}
