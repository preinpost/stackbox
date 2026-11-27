//! Snapshots: freeze a stackbox's VM, restore stackboxes from it, and move it between hosts as an archive.

use super::{
    NotFound, StackboxManager, bad_request, conflict, ensure_idle,
    store::State,
    transfers::{host_arch, origin_host, origin_missing},
};
use crate::{
    kolla::{DeployEvent, StepProgress},
    model::{snapshot_labels as label, *},
    stackbox::Role,
};
use anyhow::{Context, Result, anyhow};
use std::{
    collections::BTreeMap,
    net::{IpAddr, Ipv4Addr},
    path::{Path, PathBuf},
    sync::Arc,
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

/// A snapshot a new stackbox on `host_id` can start from. Exporting only reads it.
/// A snapshot backed by a bucket archive (`archive.bucket` set) may have no local `reference`
/// yet; the target host fetches and loads it when the restore starts.
pub(super) fn restorable<'a>(
    state: &'a State,
    id: &str,
    host_id: &str,
) -> Result<&'a SnapshotRecord> {
    let snapshot = state
        .snapshots
        .get(id)
        .ok_or_else(|| NotFound(format!("snapshot '{id}'")))?;
    let has_bucket = snapshot.archive.as_ref().is_some_and(|a| {
        a.bucket
            .as_ref()
            .is_some_and(|b| state.buckets.contains_key(b))
            && a.key.is_some()
            && a.sha256.is_some()
    });
    if !matches!(
        snapshot.status,
        SnapshotStatus::Ready | SnapshotStatus::Exporting
    ) || (snapshot.reference.is_none() && !has_bucket)
    {
        return Err(conflict(format!("snapshot '{id}' is not ready")));
    }
    // A control template is restored too, as a stackbox with its control VM alone.
    if snapshot.template == Some(TemplateRole::Compute) {
        return Err(conflict(format!(
            "snapshot '{id}' is a compute template; add a compute from it to a deployed stackbox instead"
        )));
    }
    ensure_compatible(state, snapshot, host_id)?;
    Ok(snapshot)
}

/// Build the plan described by a bucket archive's metadata file.
pub(super) fn plan_from_metadata(metadata: &crate::s3::catalog::ArchiveMetadata) -> Result<Plan> {
    match metadata.role {
        Some(TemplateRole::Control) => control_layout(
            &metadata.components,
            metadata.compute,
            metadata.release.as_deref(),
        ),
        Some(TemplateRole::Compute) => Err(conflict(format!(
            "{} is a compute template and cannot start a stackbox",
            metadata.key
        ))),
        None => layout(&PlanRequest {
            pci_devices: Vec::new(),
            components: metadata.components.clone(),
            computes: metadata.computes,
            compute: metadata.compute,
            layout: None,
            release: metadata.release.clone(),
            gpu: metadata.gpu.clone(),
        }),
    }
}

/// Pick a snapshot ID for a bucket archive that does not collide with an existing record.
pub(super) fn bucket_snapshot_id(
    state: &State,
    metadata: &crate::s3::catalog::ArchiveMetadata,
    bucket_id: &str,
    key: &str,
) -> String {
    // Reuse an existing snapshot record that already points at this exact bucket archive.
    if let Some((id, _)) = state.snapshots.iter().find(|(_, s)| {
        s.status == SnapshotStatus::Ready
            && s.archive.as_ref().is_some_and(|a| {
                a.bucket.as_deref() == Some(bucket_id)
                    && a.key.as_deref() == Some(key)
                    && a.sha256.as_deref() == Some(metadata.sha256.as_str())
            })
    }) {
        return id.clone();
    }
    let base = if validate_snapshot_id(&metadata.snapshot_id).is_ok() {
        metadata.snapshot_id.clone()
    } else {
        let stem = key
            .rsplit('/')
            .next()
            .unwrap_or("bucket-snap")
            .strip_suffix(".msb")
            .unwrap_or("bucket-snap")
            .to_ascii_lowercase()
            .chars()
            .map(|c| {
                if c.is_ascii_lowercase() || c.is_ascii_digit() {
                    c
                } else {
                    '-'
                }
            })
            .collect::<String>()
            .trim_matches('-')
            .to_owned();
        if validate_snapshot_id(&stem).is_ok() {
            stem
        } else {
            "bucket-snap".to_owned()
        }
    };
    if !state.snapshots.contains_key(&base) {
        return base;
    }
    for n in 2..=999 {
        let suffix = format!("-{n}");
        let prefix = base[..base.len().min(48 - suffix.len())].trim_end_matches('-');
        let candidate = format!("{prefix}{suffix}");
        if !state.snapshots.contains_key(&candidate) {
            return candidate;
        }
    }
    format!("snap-{}", &uuid::Uuid::new_v4().simple().to_string()[..12])
}

/// A compute template a new compute of `stackbox` on `host_id` can boot from: Kolla only
/// reconfigures that VM, so its containers must be the stackbox's release and services, on the
/// host's architecture.
pub(super) fn compute_template<'a>(
    state: &'a State,
    id: &str,
    stackbox: &StackboxRecord,
    host_id: &str,
) -> Result<&'a SnapshotRecord> {
    let snapshot = state
        .snapshots
        .get(id)
        .ok_or_else(|| NotFound(format!("snapshot '{id}'")))?;
    if snapshot.template != Some(TemplateRole::Compute) {
        return Err(conflict(format!(
            "snapshot '{id}' is not a compute template; freeze one of a stackbox's compute VMs as one"
        )));
    }
    if !matches!(
        snapshot.status,
        SnapshotStatus::Ready | SnapshotStatus::Exporting
    ) || snapshot.reference.is_none()
    {
        return Err(conflict(format!("snapshot '{id}' is not ready")));
    }
    ensure_compatible(state, snapshot, host_id)?;
    let release = stackbox_release(state, stackbox);
    if snapshot.release.as_deref() != Some(release.as_str()) {
        return Err(conflict(format!(
            "compute template '{id}' is OpenStack {} but the stackbox runs {release}",
            snapshot
                .release
                .as_deref()
                .unwrap_or("of an unknown release")
        )));
    }
    let components = stackbox.plan.as_ref().map(|plan| &plan.components);
    if components != Some(&snapshot.plan.components) {
        return Err(conflict(format!(
            "compute template '{id}' was frozen from a stackbox with {} but this stackbox has {}",
            snapshot.plan.components.join(","),
            components.map(|c| c.join(",")).unwrap_or_default()
        )));
    }
    Ok(snapshot)
}

/// The OpenStack release a stackbox runs: its snapshot's when it was restored from one, or the
/// one this coordinator deploys.
fn stackbox_release(state: &State, stackbox: &StackboxRecord) -> String {
    stackbox
        .snapshot_id
        .as_ref()
        .and_then(|id| state.snapshots.get(id))
        .and_then(|snapshot| snapshot.release.clone())
        .unwrap_or_else(|| crate::kolla::OPENSTACK_RELEASE.into())
}

/// Operations cut short by a daemon stop cannot resume; leave each snapshot in a state the user can act on.
pub(super) fn recover_interrupted(state: &mut State) {
    for snapshot in state.snapshots.values_mut() {
        let (status, error) = match snapshot.status {
            SnapshotStatus::Creating => (
                SnapshotStatus::Failed,
                "daemon stopped while capturing; check that the source stackbox's VM is running, then delete this snapshot",
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
        snapshot.transfer = None;
        snapshot.status = status;
        snapshot.error = Some(error.into());
        snapshot.updated_at = now();
    }
}

/// A frozen guest carries one architecture's bootloader and kernel. A remote host not
/// checked yet is not refused here; copying the snapshot there checks again.
pub(super) fn ensure_compatible(
    state: &State,
    snapshot: &SnapshotRecord,
    host_id: &str,
) -> Result<()> {
    if let Some(arch) = host_arch(state, host_id)
        && arch != snapshot.arch
    {
        return Err(conflict(format!(
            "snapshot '{}' was frozen on {}, so its guest cannot boot on host '{host_id}', which is {arch}",
            snapshot.id, snapshot.arch
        )));
    }
    Ok(())
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
            snapshot
                .release
                .clone()
                .unwrap_or_else(|| crate::kolla::OPENSTACK_RELEASE.into()),
        ),
        // The guest image is built for the architecture of the host that froze it.
        (label::ARCH.into(), snapshot.arch.clone()),
    ]);
    if let Some(platform) = &snapshot.platform {
        labels.insert(label::PLATFORM.into(), platform.clone());
    }
    if let Some(source) = &snapshot.source_stackbox {
        labels.insert(label::SOURCE.into(), source.clone());
    }
    if let Some(note) = &snapshot.note {
        labels.insert(label::NOTE.into(), note.clone());
    }
    if let Some(template) = snapshot.template {
        labels.insert(label::ROLE.into(), template.as_str().into());
    } else if snapshot.plan.computes > 0 {
        labels.insert(label::COMPUTES.into(), snapshot.plan.computes.to_string());
    }
    labels
}

/// The metadata file of `snapshot`'s archive at `key`, saying what its labels say.
fn archive_metadata(
    snapshot: &SnapshotRecord,
    key: &str,
    uploaded: &crate::s3::client::Transferred,
    exported_at: u64,
) -> crate::s3::catalog::ArchiveMetadata {
    let labels = labels(snapshot);
    crate::s3::catalog::ArchiveMetadata {
        schema: crate::s3::catalog::SCHEMA,
        key: key.into(),
        size_bytes: uploaded.size,
        sha256: uploaded.sha256.clone(),
        snapshot_id: snapshot.id.clone(),
        release: labels.get(label::RELEASE).cloned(),
        role: snapshot.template,
        node: snapshot.node.clone(),
        components: snapshot.plan.components.clone(),
        computes: if snapshot.template.is_some() {
            0
        } else {
            snapshot.plan.computes
        },
        compute: snapshot.plan.compute,
        gpu: snapshot.plan.gpu.clone(),
        arch: labels[label::ARCH].clone(),
        platform: labels.get(label::PLATFORM).cloned(),
        deployed: snapshot.deployed,
        source_stackbox: snapshot.source_stackbox.clone(),
        note: snapshot.note.clone(),
        created_at: snapshot.created_at,
        exported_at,
    }
}

enum Target {
    File(PathBuf),
    /// `key` is resolved against the bucket's prefix when the export is accepted.
    Bucket {
        bucket: String,
        key: Option<String>,
    },
}
enum Source {
    File(PathBuf),
    Bucket { bucket: String, key: String },
}
fn given(value: Option<String>) -> Option<String> {
    value.filter(|v| !v.trim().is_empty())
}

/// Metadata of an imported archive, read back from its labels.
struct Imported {
    arch: String,
    platform: Option<String>,
    plan: Plan,
    deployed: bool,
    source_exposure: Exposure,
    source_stackbox: Option<String>,
    note: Option<String>,
    template: Option<TemplateRole>,
    node: Option<String>,
    release: Option<String>,
}

/// Result of loading an exported archive (single-VM `.msb` or multi-node bundle) into msb.
struct LoadedArchive {
    meta: Imported,
    reference: String,
    nodes: Vec<crate::stackbox::NodeSnapshot>,
    size_bytes: Option<u64>,
}

/// Remove every msb snapshot loaded by a rejected or partial import, computes before control
/// so the group head goes last.
async fn remove_loaded(
    backend: &Arc<dyn super::Backend>,
    reference: &str,
    nodes: &[crate::stackbox::NodeSnapshot],
    id: &str,
) {
    let refs: Vec<String> = if nodes.is_empty() {
        (!reference.is_empty())
            .then(|| reference.to_owned())
            .into_iter()
            .collect()
    } else {
        nodes.iter().rev().map(|n| n.reference.clone()).collect()
    };
    for r in refs {
        if let Err(cleanup) = backend.remove_snapshot(r).await {
            eprintln!("cannot remove rejected import {id}: {cleanup:#}");
        }
    }
}
fn parse_labels(
    labels: &BTreeMap<String, String>,
    bundle: Option<(u8, Option<ComputeSize>, Option<GpuConfig>)>,
) -> Result<Imported> {
    let get = |key: &str| {
        labels
            .get(key)
            .with_context(|| format!("archive has no {key} label; it was not frozen by stackbox"))
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
    let template = match labels.get(label::ROLE).map(String::as_str) {
        None => None,
        Some("control") => Some(TemplateRole::Control),
        Some("compute") => Some(TemplateRole::Compute),
        Some(role) => anyhow::bail!("archive holds a {role} VM, which stackbox cannot use"),
    };
    // Single-VM archives are all-in-one stackboxes or templates; a multi-node bundle supplies the
    // compute count, compute size and GPU config from its manifest.
    let release = labels.get(label::RELEASE).cloned();
    let plan = match (template, bundle) {
        (Some(_), Some(_)) => {
            anyhow::bail!("a multi-node bundle cannot hold a single-VM template")
        }
        (None, Some((computes, compute, gpu))) => layout(&PlanRequest {
            pci_devices: Vec::new(),
            components,
            computes,
            compute,
            layout: None,
            release,
            gpu,
        })?,
        (Some(TemplateRole::Control), None) => {
            control_layout(&components, None, release.as_deref())?
        }
        (_, None) => layout(&PlanRequest {
            pci_devices: Vec::new(),
            components,
            computes: u8::from(template.is_some()),
            compute: None,
            layout: None,
            release,
            gpu: None,
        })?,
    };
    Ok(Imported {
        arch: arch.clone(),
        platform: labels.get(label::PLATFORM).cloned(),
        plan,
        deployed: get(label::DEPLOYED)? == "true",
        source_exposure: Exposure {
            host: get(label::EXPOSURE_HOST)?.clone(),
            bind: IpAddr::V4(Ipv4Addr::LOCALHOST),
            port_base: get(label::EXPOSURE_PORT_BASE)?
                .parse()
                .context("invalid exposure port label")?,
        },
        source_stackbox: labels.get(label::SOURCE).cloned(),
        note: labels.get(label::NOTE).cloned(),
        template,
        node: template.and(labels.get(label::NODE).cloned()),
        release: labels.get(label::RELEASE).cloned(),
    })
}

impl StackboxManager {
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

    /// Freeze the stackbox's VM into a new snapshot. The VM stops for the capture and starts again.
    pub fn freeze(&self, stackbox_id: &str, request: SnapshotRequest) -> Result<Job> {
        request.validate().map_err(|e| bad_request(e.to_string()))?;
        let (job, repinned) = self.update(|state| {
            self.ensure_accepting()?;
            if state.snapshots.contains_key(&request.id) {
                return Err(conflict(format!(
                    "snapshot '{}' already exists",
                    request.id
                )));
            }
            let stackbox = state
                .stackboxes
                .get_mut(stackbox_id)
                .ok_or_else(|| NotFound(format!("stackbox '{stackbox_id}'")))?;
            ensure_idle(stackbox)?;
            let host_id = stackbox
                .host_id
                .clone()
                .ok_or_else(|| conflict("stackboxes imported from older versions cannot be frozen"))?;
            if !matches!(
                stackbox.status,
                StackboxStatus::Created | StackboxStatus::Ready | StackboxStatus::Failed
            ) {
                return Err(conflict("stackbox must be created before freezing"));
            }
            let (Some(plan), Some(exposure)) = (&stackbox.plan, &stackbox.exposure) else {
                return Err(conflict(
                    "stackboxes imported from older versions cannot be frozen",
                ));
            };
            // A passed-through device is bound to the compute VM that boots with it; a template
            // or snapshot would not carry the binding, so restoring it would lose the device.
            if !plan.pci_devices.is_empty() {
                return Err(conflict(
                    "this stackbox passes PCI devices through to compute-1, and a template or snapshot does not carry that binding; create a PCI stackbox clean instead",
                ));
            }
            if request.node.as_deref() == Some("control") {
                if !plan.multinode() {
                    return Err(conflict(
                        "an all-in-one stackbox has no control VM to freeze as a template; freeze the whole stackbox instead",
                    ));
                }
                // A control template records the stackbox without its computes, and the GPU is
                // part of the computes: restoring the template would silently drop it.
                if let Some(gpu) = &plan.gpu {
                    return Err(conflict(format!(
                        "this stackbox's computes offer a '{}' GPU, and a control template keeps neither computes nor GPU, so a stackbox restored from it would lose the GPU; create GPU stackboxes clean instead",
                        gpu.mode_name()
                    )));
                }
                if stackbox.status != StackboxStatus::Ready {
                    return Err(conflict(
                        "deploy the stackbox before freezing a control template from it",
                    ));
                }
                // Its computes stop while the control VM is captured; the snapshot stays on
                // the control VM's host.
                if !stackbox.shares.is_empty() {
                    return Err(conflict(format!(
                        "this stackbox has computes on other hosts; only a stackbox whose VMs all run on host '{host_id}' can be frozen as a control template"
                    )));
                }
            } else if let Some(node) = &request.node {
                if plan.computes == 0
                    || !plan.nodes.iter().any(|spec| {
                        crate::stackbox::suffix(spec.role, spec.index).as_deref() == Some(node.as_str())
                    })
                {
                    return Err(conflict(format!("the stackbox has no compute VM '{node}'")));
                }
                if stackbox.status != StackboxStatus::Ready {
                    return Err(conflict(
                        "deploy the stackbox before freezing a compute template from it",
                    ));
                }
            }
            // A whole-stackbox snapshot captures every VM into one snapshot on one host; a
            // stackbox whose computes span hosts cannot be frozen whole, so its computes are
            // frozen one at a time as compute templates on their own host instead.
            if request.node.is_none() && !stackbox.shares.is_empty() {
                return Err(conflict(
                    "this stackbox has computes on other hosts, and freezing a multi-host stackbox whole is not supported; freeze a compute VM as a compute template instead",
                ));
            }
            if request.node.is_none() && plan.multinode() && plan.computes == 0 {
                return Err(conflict(
                    "this stackbox has no compute VM yet; freeze its control VM as a control template instead",
                ));
            }
            let layout_matches = if plan.multinode() {
                crate::stackbox::observed_layout(&stackbox.nodes) == Some(plan.computes)
            } else {
                stackbox.nodes.len() == 1 && stackbox.nodes[0].role == Some(Role::AllInOne)
            };
            if !layout_matches || stackbox.nodes.iter().any(|node| node.status != "running") {
                return Err(conflict("the stackbox's VMs must be running to freeze it"));
            }
            let target_host = match request.node.as_deref() {
                Some(node) if node != "control" => stackbox
                    .host_of(node)
                    .map(str::to_owned)
                    .unwrap_or_else(|| host_id.clone()),
                _ => host_id.clone(),
            };
            let stored = state.hosts.get(&target_host).ok_or_else(|| {
                conflict(format!(
                    "the {} host '{target_host}' is not registered",
                    if target_host == host_id {
                        "stackbox's"
                    } else {
                        "compute's"
                    }
                ))
            })?;
            let local = stored.host.kind == crate::remote::HostKind::Local;
            if stored.host.kind == crate::remote::HostKind::Agent
                && !self.0.hub.connected(&target_host)
            {
                return Err(conflict(format!(
                    "the agent of host '{target_host}' is not connected; run `stackbox agent connect` there"
                )));
            }
            if stored
                .host
                .reachability
                .as_ref()
                .is_some_and(|r| !r.reachable)
            {
                return Err(conflict(format!(
                    "host '{target_host}' is offline; check its connection before freezing"
                )));
            }
            let arch = match (&stored.host.inspection, stored.host.kind) {
                (Some(inspection), _) => inspection.arch.clone(),
                (None, crate::remote::HostKind::Local) => current_arch(),
                (None, _) => {
                    return Err(conflict(format!(
                        "host '{target_host}' has not been checked yet; check it from Hosts first"
                    )));
                }
            };
            let platform = match &stored.host.inspection {
                Some(inspection) if !local => inspection.os.clone(),
                _ => current_platform(),
            };
            // A remote VM keeps the agent its host created it with. Move it to the host's current
            // agent once that agent matches this coordinator, as a deploy does, so the freeze
            // and later transfers of the snapshot speak this coordinator's protocol.
            let mut repinned = None;
            let pinned_sha256 = if target_host == host_id {
                stackbox.agent_sha256.as_deref()
            } else {
                stackbox
                    .shares
                    .iter()
                    .find(|share| share.host_id == target_host)
                    .and_then(|share| share.agent_sha256.as_deref())
            };
            if let Some(agent) = &stored.host.agent {
                if pinned_sha256 != Some(agent.artifact.sha256.as_str()) {
                    if agent.artifact.build_id != crate::BUILD_ID {
                        return Err(conflict(format!(
                            "the agent installed on {target_host} was built from {} but this coordinator is {}; reinstall it from Hosts first",
                            agent.artifact.build_id,
                            crate::BUILD_ID
                        )));
                    }
                    repinned = Some(agent.artifact.sha256.clone());
                    if target_host == host_id {
                        stackbox.agent_sha256 = repinned.clone();
                    } else if let Some(share) = stackbox
                        .shares
                        .iter_mut()
                        .find(|share| share.host_id == target_host)
                    {
                        share.agent_sha256 = repinned.clone();
                    }
                }
            } else if stored.host.kind == crate::remote::HostKind::Ssh
                && pinned_sha256.is_none()
            {
                return Err(conflict(format!(
                    "install the Stackbox execution agent on host '{target_host}' first"
                )));
            }
            let template = request.node.as_deref().map(|node| match node {
                "control" => TemplateRole::Control,
                _ => TemplateRole::Compute,
            });
            let snapshot = SnapshotRecord {
                id: request.id.clone(),
                status: SnapshotStatus::Creating,
                arch,
                platform: Some(platform),
                host_id: (!local).then_some(target_host.clone()),
                source_stackbox: Some(stackbox_id.into()),
                plan: match template {
                    // What a stackbox restored from it runs until it gains computes.
                    Some(TemplateRole::Control) => control_layout(
                        &plan.components,
                        plan.compute,
                        plan.release.as_deref(),
                    )
                    .map_err(|e| conflict(e.to_string()))?,
                    _ => plan.clone(),
                },
                deployed: stackbox.status == StackboxStatus::Ready,
                source_exposure: exposure.clone(),
                note: request.note.clone().filter(|n| !n.trim().is_empty()),
                reference: None,
                nodes: Vec::new(),
                template,
                node: request.node.clone(),
                legacy_compute_template: false,
                // The stackbox's own release; a stackbox recorded before releases were kept reads as
                // the one this build installs.
                release: plan
                    .release
                    .clone()
                    .or_else(|| Some(crate::kolla::OPENSTACK_RELEASE.into())),
                size_bytes: None,
                archive: None,
                transfer: None,
                copies: Vec::new(),
                error: None,
                created_at: now(),
                updated_at: now(),
            };
            let mut job = super::new_job(stackbox_id, JobKind::Snapshot);
            job.snapshot_id = Some(request.id.clone());
            job.target_host = Some(target_host);
            job.target_node = request.node.clone();
            // The stackbox says its VM is stopping, and remembers what to be once it restarts.
            stackbox.resume = Some(stackbox.status);
            stackbox.status = StackboxStatus::Snapshotting;
            stackbox.active_job = Some(job.id.clone());
            stackbox.error = None;
            stackbox.updated_at = now();
            state.snapshots.insert(snapshot.id.clone(), snapshot);
            state.jobs.insert(job.id.clone(), job.clone());
            Ok((job, repinned))
        })?;
        if let Some(digest) = repinned {
            self.log(
                &job.stackbox_id,
                &job.id,
                None,
                format!("agent: using the host's current agent {}\n", &digest[..12]),
            )?;
        }
        self.spawn(job.clone(), None);
        Ok(job)
    }

    pub(super) async fn freeze_job(&self, job: &Job, stackbox: StackboxRecord) -> Result<()> {
        let id = job.snapshot_id.clone().context("snapshot job has no id")?;
        let snapshot = self.snapshot(&id)?;
        self.log(
            &stackbox.id,
            &job.id,
            None,
            match job.target_node.as_deref() {
                Some("control") => "stopping the computes, then the control VM, to capture the control VM as a control template\n".into(),
                Some(node) => format!("stopping {node} to capture it as a compute template\n"),
                None => "stopping the VM to capture its disks\n".into(),
            },
        )?;
        let backend = match job.target_node.as_deref() {
            Some(node) => self.node_backend(&stackbox, node)?,
            None => self.backend_for(&stackbox)?,
        };
        // A compute template stopped one compute only; the control plane kept answering.
        let settle = snapshot.deployed && snapshot.template != Some(TemplateRole::Compute);
        let sink = self.event_sink(job, None);
        let steps = sink.clone();
        let frozen = match backend
            .freeze(
                stackbox.id.clone(),
                id.clone(),
                job.target_node.clone(),
                labels(&snapshot),
                // The services check after the freeze is one more step.
                Arc::new(move |event| {
                    steps(match event {
                        DeployEvent::Progress(mut progress) if settle => {
                            progress.total += 1;
                            DeployEvent::Progress(progress)
                        }
                        event => event,
                    })
                }),
            )
            .await
        {
            Ok(frozen) => frozen,
            Err(error) => {
                let target_host = job
                    .target_host
                    .as_deref()
                    .or(snapshot.host_id.as_deref())
                    .or(stackbox.host_id.as_deref())
                    .unwrap_or("unknown");
                let remote_group = job
                    .target_node
                    .as_deref()
                    .filter(|_| snapshot.host_id.is_some())
                    .map(|node| crate::stackbox::node_group(&id, node));
                let error = match remote_group {
                    Some(group) if !error.is::<crate::remote::protocol::AgentError>() => error
                        .context(format!(
                            "no response from host '{target_host}'; any partial snapshot group '{group}' on that host was left in place"
                        )),
                    Some(group) => error.context(format!(
                        "host '{target_host}' failed to capture snapshot group '{group}'"
                    )),
                    None => error,
                };
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
            s.nodes = frozen.nodes;
        })?;
        self.log(
            &stackbox.id,
            &job.id,
            None,
            format!("snapshot {id} captured; the VM started again\n"),
        )?;
        if let Some(error) = frozen.restart_error {
            return Err(anyhow!(
                "snapshot {id} is ready, but the stackbox's VM did not start again: {error}"
            ));
        }
        if settle {
            let last = self.job(&job.id)?.steps.last().map(|s| s.total);
            if let Some(total) = last {
                sink(DeployEvent::Progress(StepProgress {
                    step: "check that the services answer".into(),
                    index: total,
                    total,
                }))?;
            }
            backend
                .settle(
                    stackbox.id.clone(),
                    None,
                    snapshot.plan.components.clone(),
                    self.event_sink(job, None),
                )
                .await
                .with_context(|| {
                    format!(
                        "snapshot {id} is ready, but the stackbox's services did not answer again"
                    )
                })?;
        }
        Ok(())
    }

    pub(super) async fn restore_job(&self, job: &Job, stackbox: StackboxRecord) -> Result<()> {
        let host_id = stackbox
            .host_id
            .clone()
            .context("restored stackbox has no host")?;
        self.avoid_foreign_ports(job, &stackbox).await?;
        let stackbox = self.get(&stackbox.id)?;
        let (reference, snapshot_nodes, source_exposure, template, deployed, plan) = self
            .prepare_restore_source(job, &stackbox, &host_id)
            .await?;
        let exposure = stackbox
            .exposure
            .clone()
            .context("stackbox has no exposure")?;
        self.log(
            &stackbox.id,
            &job.id,
            None,
            format!(
                "starting a copy of {}\n",
                job.snapshot_id.as_deref().unwrap_or("bucket archive")
            ),
        )?;
        let backend = self.backend_for(&stackbox)?;
        let nodes = backend
            .restore(
                stackbox.id.clone(),
                reference,
                snapshot_nodes,
                plan.clone(),
                exposure.clone(),
            )
            .await?;
        self.update(|state| {
            state
                .stackboxes
                .get_mut(&stackbox.id)
                .context("stackbox missing")?
                .nodes = nodes;
            Ok(())
        })?;
        if template == Some(TemplateRole::Control) {
            self.log(
                &stackbox.id,
                &job.id,
                None,
                "control VM started; forgetting the computes of the stackbox it was frozen from\n"
                    .into(),
            )?;
            backend
                .forget_computes(
                    stackbox.id.clone(),
                    plan.components.clone(),
                    self.event_sink(job, None),
                )
                .await
                .context(
                    "the control VM started, but its source stackbox's computes are still registered",
                )?;
        }
        if deployed {
            self.log(
                &stackbox.id,
                &job.id,
                None,
                "VM started; moving public endpoints and waiting for OpenStack\n".into(),
            )?;
            backend
                .settle(
                    stackbox.id.clone(),
                    Some((source_exposure, exposure)),
                    plan.components.clone(),
                    self.event_sink(job, None),
                )
                .await?;
            // The snapshot was frozen without devices; the host's devices join compute-1 now.
            if !plan.pci_devices.is_empty() {
                self.log(
                    &stackbox.id,
                    &job.id,
                    None,
                    format!(
                        "passing {} PCI device(s) through to compute-1\n",
                        plan.pci_devices.len()
                    ),
                )?;
                backend
                    .attach_pci(
                        stackbox.id.clone(),
                        plan.release
                            .clone()
                            .unwrap_or_else(|| crate::kolla::OPENSTACK_RELEASE.to_owned()),
                        plan.pci_devices.clone(),
                        plan.gpu
                            .as_ref()
                            .is_some_and(|gpu| gpu.mode == crate::model::GpuMode::Mtty),
                        self.event_sink(job, None),
                    )
                    .await
                    .context(
                        "the stackbox restored, but its PCI devices did not attach to compute-1",
                    )?;
            }
        }
        Ok(())
    }

    async fn prepare_restore_source(
        &self,
        job: &Job,
        stackbox: &StackboxRecord,
        host_id: &str,
    ) -> Result<(
        String,
        Vec<crate::stackbox::NodeSnapshot>,
        Exposure,
        Option<TemplateRole>,
        bool,
        Plan,
    )> {
        let id = stackbox
            .snapshot_id
            .clone()
            .context("restored stackbox has no snapshot")?;
        let snapshot = self.snapshot(&id)?;
        self.read(|state| ensure_compatible(state, &snapshot, host_id))??;
        let (reference, snapshot_nodes) = self
            .ensure_snapshot_on_host_nodes(&id, host_id, Some(self.event_sink(job, None)))
            .await?;
        // A bucket-backed snapshot may learn its exact source_exposure and labels when first
        // loaded on the host; re-read it after ensure_snapshot_on_host_nodes.
        let snapshot = self.snapshot(&id)?;
        // The snapshot's plan, plus the devices the restore asked for (`create` put them on the
        // record); a restore without devices keeps the snapshot's plan as it was frozen.
        let mut plan = snapshot.plan.clone();
        if let Some(devices) = stackbox.plan.as_ref().map(|plan| plan.pci_devices.clone())
            && !devices.is_empty()
        {
            plan.pci_devices = devices;
        }
        self.update(|state| {
            if let Some(record) = state.stackboxes.get_mut(&stackbox.id) {
                record.plan = Some(plan.clone());
            }
            Ok(())
        })?;
        Ok((
            reference,
            snapshot_nodes,
            snapshot.source_exposure,
            snapshot.template,
            snapshot.deployed,
            plan,
        ))
    }

    /// Write the snapshot to an archive in the background, as a file or an object in a
    /// registered bucket. `path` may name a file or an existing directory.
    pub fn export_snapshot(&self, id: &str, request: ExportRequest) -> Result<SnapshotRecord> {
        let target = match (given(request.path), given(request.bucket)) {
            (Some(_), Some(_)) => {
                return Err(bad_request("export to either a path or a bucket"));
            }
            (None, Some(bucket)) => {
                // Imports list `.msb` objects only, so keep exports findable.
                let key = given(request.key).map(|key| {
                    if key.ends_with(".msb") {
                        key
                    } else {
                        format!("{key}.msb")
                    }
                });
                if let Some(key) = &key {
                    crate::s3::validate_key(key).map_err(|e| bad_request(e.to_string()))?;
                }
                Target::Bucket { bucket, key }
            }
            (path, None) => {
                if given(request.key).is_some() {
                    return Err(bad_request("an object key needs a bucket"));
                }
                Target::File(self.export_path(id, path)?)
            }
        };
        let (snapshot, reference, target) = self.update(|state| {
            self.ensure_accepting()?;
            let target = match target {
                Target::Bucket { bucket, key } => {
                    let stored = state.buckets.get(&bucket).ok_or_else(|| {
                        bad_request(format!("bucket '{bucket}' is not registered"))
                    })?;
                    let key = key.unwrap_or_else(|| format!("{}{id}.msb", stored.bucket.prefix));
                    let index = crate::s3::catalog::index_key(&stored.bucket.prefix);
                    if crate::s3::catalog::metadata_key(&key) == index {
                        return Err(bad_request(format!(
                            "{key} would put its metadata at {index}, the bucket's catalog; choose another key"
                        )));
                    }
                    Target::Bucket {
                        bucket,
                        key: Some(key),
                    }
                }
                file => file,
            };
            let snapshot = state
                .snapshots
                .get(id)
                .ok_or_else(|| NotFound(format!("snapshot '{id}'")))?;
            let reference = match (&snapshot.status, &snapshot.reference) {
                (SnapshotStatus::Ready, Some(reference)) => reference.clone(),
                _ => return Err(conflict(format!("snapshot '{id}' is not ready"))),
            };
            if origin_host(state, snapshot).is_none() {
                return Err(origin_missing(snapshot));
            }
            let snapshot = state.snapshots.get_mut(id).context("snapshot missing")?;
            snapshot.status = SnapshotStatus::Exporting;
            snapshot.error = None;
            snapshot.transfer = match &target {
                Target::Bucket {
                    bucket,
                    key: Some(key),
                } => Some(SnapshotTransfer {
                    bucket: bucket.clone(),
                    key: key.clone(),
                    done_bytes: 0,
                    total_bytes: None,
                }),
                _ => None,
            };
            snapshot.updated_at = now();
            Ok((snapshot.clone(), reference, target))
        })?;
        let manager = self.clone();
        let id = id.to_owned();
        let exported = snapshot.clone();
        tokio::spawn(async move {
            let result = match target {
                Target::File(output) => manager.run_export(&exported, reference, &output).await,
                Target::Bucket { bucket, key } => {
                    let key = key.unwrap_or_default();
                    manager.run_upload(&id, reference, &bucket, &key).await
                }
            };
            let saved = manager.update_snapshot(&id, |s| {
                s.status = SnapshotStatus::Ready;
                s.transfer = None;
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
    fn export_path(&self, id: &str, path: Option<String>) -> Result<PathBuf> {
        let output = match path {
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
        Ok(output)
    }
    async fn run_export(
        &self,
        snapshot: &SnapshotRecord,
        reference: String,
        output: &Path,
    ) -> Result<SnapshotArchive> {
        let _permit = self.0.snapshot_gate.acquire().await?;
        if let Some(dir) = output.parent() {
            std::fs::create_dir_all(dir)?;
        }
        // Publish the archive under its final name only once it is complete.
        let partial = PathBuf::from(format!("{}.partial", output.display()));
        let _ = std::fs::remove_file(&partial);
        let exported = self
            .export_original(snapshot, reference, &partial, &None)
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
            bucket: None,
            key: None,
            sha256: None,
        })
    }
    /// Where an archive passing through the coordinator is kept until its transfer ends.
    fn transfer_path(&self, id: &str, kind: &str) -> Result<PathBuf> {
        let dir = self.state_dir().join("transfers");
        std::fs::create_dir_all(&dir)?;
        let path = dir.join(format!("{id}.{kind}.msb"));
        let _ = std::fs::remove_file(&path);
        Ok(path)
    }
    /// Write the archive locally, upload it, then delete the local copy. Its metadata file
    /// goes up last, then the bucket's index is brought up to date.
    async fn run_upload(
        &self,
        id: &str,
        reference: String,
        bucket_id: &str,
        key: &str,
    ) -> Result<SnapshotArchive> {
        let _permit = self.0.snapshot_gate.acquire().await?;
        let snapshot = self.snapshot(id)?;
        let (bucket, client) = self.s3_client(bucket_id)?;
        let location = format!("s3://{}/{key}", bucket.name);
        // Like a file export, never replace what is there.
        if client.head(key).await?.is_some() {
            return Err(anyhow!("{location} already exists"));
        }
        let archive = self.transfer_path(id, "export")?;
        let uploaded = async {
            self.export_original(&snapshot, reference, &archive, &None)
                .await?;
            let upload_id = client.start_upload(key).await?;
            self.update(|state| {
                state.uploads.insert(
                    upload_id.clone(),
                    crate::s3::PendingUpload {
                        bucket: bucket_id.into(),
                        key: key.into(),
                    },
                );
                Ok(())
            })?;
            let mut reported = None;
            let uploaded = client
                .upload(&archive, key, &upload_id, |done, total| {
                    self.report(id, &mut reported, done, Some(total))
                })
                .await;
            match &uploaded {
                Err(_) => {
                    // Left unaborted, the parts would stay billed.
                    match client.abort_upload(key, &upload_id).await {
                        Ok(()) => self.forget_upload(&upload_id),
                        Err(error) => eprintln!(
                            "cannot abort the upload of {location}; it is retried at the next start: {error:#}"
                        ),
                    }
                }
                Ok(_) => self.forget_upload(&upload_id),
            }
            uploaded
        }
        .await;
        let _ = std::fs::remove_file(&archive);
        let uploaded = uploaded?;
        let exported_at = now();
        let metadata = archive_metadata(&snapshot, key, &uploaded, exported_at);
        self.publish_metadata(&bucket, &client, metadata)
            .await
            .with_context(|| {
                format!("{location} was uploaded, but its metadata file was not written; it imports like an archive from before metadata files")
            })?;
        Ok(SnapshotArchive {
            path: location,
            size_bytes: uploaded.size,
            exported_at,
            bucket: Some(bucket_id.into()),
            key: Some(key.into()),
            sha256: Some(uploaded.sha256),
        })
    }
    /// Record transfer progress, at most once per 64 MiB so the state file is not rewritten per chunk.
    fn report(&self, id: &str, reported: &mut Option<u64>, done: u64, total: Option<u64>) {
        const STEP: u64 = 64 * 1024 * 1024;
        if reported.is_some_and(|last| done < last + STEP && Some(done) != total) {
            return;
        }
        *reported = Some(done);
        let saved = self.update_snapshot(id, |s| {
            if let Some(transfer) = &mut s.transfer {
                transfer.done_bytes = done;
                transfer.total_bytes = total;
            }
        });
        if let Err(error) = saved {
            eprintln!("cannot record transfer progress of snapshot {id}: {error:#}");
        }
    }

    /// Import an archive file, or an object in a registered bucket, in the background.
    /// The ID defaults to the archive's file name.
    pub fn import_snapshot(&self, request: ImportRequest) -> Result<SnapshotRecord> {
        let source = match (given(request.path), given(request.bucket)) {
            (Some(_), Some(_)) => {
                return Err(bad_request("import from either a path or a bucket"));
            }
            (Some(path), None) => {
                if given(request.key).is_some() {
                    return Err(bad_request("an object key needs a bucket"));
                }
                let path = PathBuf::from(path);
                if !path.is_absolute() {
                    return Err(bad_request("archive path must be absolute"));
                }
                if !path.is_file() {
                    return Err(bad_request(format!("{} is not a file", path.display())));
                }
                Source::File(path)
            }
            (None, Some(bucket)) => {
                let key = given(request.key)
                    .ok_or_else(|| bad_request("give the object key to import"))?;
                crate::s3::validate_key(&key).map_err(|e| bad_request(e.to_string()))?;
                Source::Bucket { bucket, key }
            }
            (None, None) => {
                return Err(bad_request(
                    "give an archive path, or a bucket and object key",
                ));
            }
        };
        let name = match &source {
            Source::File(path) => path.display().to_string(),
            Source::Bucket { key, .. } => key.clone(),
        };
        let id = match given(request.id) {
            Some(id) => id,
            None => {
                let file = name.rsplit('/').next().unwrap_or_default();
                file.strip_suffix(".msb").unwrap_or(file).to_owned()
            }
        };
        validate_snapshot_id(&id).map_err(|e| {
            bad_request(format!(
                "{e}; choose an ID for this archive (file name '{name}')"
            ))
        })?;
        let snapshot = self.update(|state| {
            self.ensure_accepting()?;
            if state.snapshots.contains_key(&id) {
                return Err(conflict(format!("snapshot '{id}' already exists")));
            }
            // The archive is kept on this machine's agent; refuse now rather than leave a record
            // that the background import can only fail.
            if !state
                .hosts
                .values()
                .any(|stored| stored.host.kind == crate::remote::HostKind::Local)
            {
                return Err(conflict(
                    "register the local host under Hosts to keep snapshots on it",
                ));
            }
            let transfer = match &source {
                Source::Bucket { bucket, key } => {
                    if !state.buckets.contains_key(bucket) {
                        return Err(bad_request(format!("bucket '{bucket}' is not registered")));
                    }
                    Some(SnapshotTransfer {
                        bucket: bucket.clone(),
                        key: key.clone(),
                        done_bytes: 0,
                        total_bytes: None,
                    })
                }
                Source::File(_) => None,
            };
            // Placeholders until the archive's labels are read.
            let snapshot = SnapshotRecord {
                id: id.clone(),
                status: SnapshotStatus::Importing,
                arch: current_arch(),
                platform: None,
                host_id: None,
                source_stackbox: None,
                plan: plan(&PlanRequest::default())?,
                deployed: false,
                source_exposure: Exposure {
                    host: "localhost".into(),
                    bind: IpAddr::V4(Ipv4Addr::LOCALHOST),
                    port_base: 0,
                },
                note: None,
                reference: None,
                nodes: Vec::new(),
                template: None,
                node: None,
                legacy_compute_template: false,
                release: None,
                size_bytes: None,
                archive: None,
                transfer,
                copies: Vec::new(),
                error: None,
                created_at: now(),
                updated_at: now(),
            };
            state.snapshots.insert(id.clone(), snapshot.clone());
            Ok(snapshot)
        })?;
        let manager = self.clone();
        tokio::spawn(async move {
            let result = match source {
                Source::File(path) => manager.run_import(&path, &id).await.map(|r| (r, None)),
                Source::Bucket { bucket, key } => manager.run_download(&id, &bucket, &key).await,
            };
            let saved = manager.update_snapshot(&id, |s| {
                s.transfer = None;
                match result {
                    Ok((imported, archive)) => {
                        s.status = SnapshotStatus::Ready;
                        // Other hosts can download a template from where it came from.
                        s.archive = archive;
                        s.arch = imported.meta.arch;
                        s.platform = imported.meta.platform;
                        s.plan = imported.meta.plan;
                        s.deployed = imported.meta.deployed;
                        s.source_exposure = imported.meta.source_exposure;
                        s.source_stackbox = imported.meta.source_stackbox;
                        s.note = imported.meta.note;
                        s.template = imported.meta.template;
                        s.node = imported.meta.node;
                        s.release = imported.meta.release;
                        s.reference = Some(imported.reference);
                        s.nodes = imported.nodes;
                        s.size_bytes = imported.size_bytes;
                    }
                    Err(error) => {
                        s.status = SnapshotStatus::Failed;
                        s.error = Some(format!("import failed: {error:#}"));
                    }
                }
            });
            if let Err(error) = saved {
                eprintln!("cannot record import of snapshot {id}: {error:#}");
            }
        });
        Ok(snapshot)
    }
    async fn run_import(&self, path: &Path, id: &str) -> Result<LoadedArchive> {
        let _permit = self.0.snapshot_gate.acquire().await?;
        self.import_archive(path, id).await
    }
    /// Download the object, import it, then delete the downloaded copy. With a metadata
    /// file next to it, the download must match its SHA-256, and the archive's labels its
    /// release and role.
    async fn run_download(
        &self,
        id: &str,
        bucket: &str,
        key: &str,
    ) -> Result<(LoadedArchive, Option<SnapshotArchive>)> {
        let _permit = self.0.snapshot_gate.acquire().await?;
        // Fail before a long download that could not be imported.
        self.local_backend()?;
        let (_, client) = self.s3_client(bucket)?;
        let metadata = super::buckets::read_metadata(&client, key).await?;
        let archive = self.transfer_path(id, "import")?;
        let imported = async {
            let mut reported = None;
            let downloaded = client
                .download(key, &archive, |done, total| {
                    self.report(id, &mut reported, done, total)
                })
                .await?;
            if let Some(metadata) = &metadata
                && metadata.sha256 != downloaded.sha256
            {
                return Err(anyhow!(
                    "{key} has SHA-256 {} but its metadata file says {}; the archive is damaged or was replaced, so it is not imported",
                    downloaded.sha256,
                    metadata.sha256
                ));
            }
            let imported = self.import_archive(&archive, id).await?;
            if let Some(metadata) = &metadata
                && let Err(error) =
                    metadata.agrees(imported.meta.release.as_deref(), imported.meta.template)
            {
                if let Ok(backend) = self.local_backend() {
                    remove_loaded(&backend, &imported.reference, &imported.nodes, id).await;
                }
                return Err(error.context(format!("{key} does not match its metadata file")));
            }
            let bucket_record = self.bucket(bucket)?;
            let archive = metadata.as_ref().map(|metadata| SnapshotArchive {
                path: format!("s3://{}/{key}", bucket_record.name),
                size_bytes: downloaded.size,
                exported_at: metadata.exported_at,
                bucket: Some(bucket.into()),
                key: Some(key.into()),
                sha256: Some(downloaded.sha256.clone()),
            });
            Ok((imported, archive))
        }
        .await;
        let _ = std::fs::remove_file(&archive);
        imported
    }
    async fn import_archive(&self, path: &Path, id: &str) -> Result<LoadedArchive> {
        let is_bundle = {
            let path = path.to_owned();
            tokio::task::spawn_blocking(move || super::bundle::is_bundle(&path)).await??
        };
        if is_bundle {
            return self.import_bundle(path, id).await;
        }
        let backend = self.local_backend()?;
        let imported = backend
            .import_snapshot(path.to_owned(), id.to_owned())
            .await?;
        match parse_labels(&imported.labels, None) {
            Ok(meta) => Ok(LoadedArchive {
                meta,
                reference: imported.reference,
                nodes: Vec::new(),
                size_bytes: imported.size_bytes,
            }),
            Err(error) => {
                // Do not keep an msb snapshot no stackbox can be restored from.
                if let Err(cleanup) = backend.remove_snapshot(imported.reference).await {
                    eprintln!("cannot remove rejected import {id}: {cleanup:#}");
                }
                Err(error)
            }
        }
    }
    async fn import_bundle(&self, path: &Path, id: &str) -> Result<LoadedArchive> {
        let backend = self.local_backend()?;
        let stage_dir = self
            .state_dir()
            .join("transfers")
            .join(format!("{id}.bundle-import"));
        let _ = std::fs::remove_dir_all(&stage_dir);
        let result = async {
            let extracted = {
                let archive = path.to_owned();
                let dest = stage_dir.clone();
                tokio::task::spawn_blocking(move || super::bundle::unpack(&archive, &dest)).await??
            };
            let mut loaded_nodes: Vec<crate::stackbox::NodeSnapshot> =
                Vec::with_capacity(extracted.nodes.len());
            let mut control_labels: Option<BTreeMap<String, String>> = None;
            let mut compute_count: u8 = 0;
            for (node, node_path) in &extracted.nodes {
                let imported = backend
                    .import_snapshot(node_path.clone(), id.to_owned())
                    .await
                    .with_context(|| format!("import VM '{node}' of bundle {id}"));
                let imported = match imported {
                    Ok(imported) => imported,
                    Err(error) => {
                        remove_loaded(&backend, "", &loaded_nodes, id).await;
                        return Err(error);
                    }
                };
                if let Some(labeled_node) = imported.labels.get(label::NODE)
                    && labeled_node != node
                {
                    loaded_nodes.push(crate::stackbox::NodeSnapshot {
                        node: node.clone(),
                        reference: imported.reference,
                        size_bytes: imported.size_bytes,
                    });
                    remove_loaded(&backend, "", &loaded_nodes, id).await;
                    return Err(anyhow!(
                        "bundle entry '{node}' holds disks labeled for '{labeled_node}'"
                    ));
                }
                if node == "control" {
                    control_labels = Some(imported.labels);
                } else {
                    compute_count = compute_count.saturating_add(1);
                }
                loaded_nodes.push(crate::stackbox::NodeSnapshot {
                    node: node.clone(),
                    reference: imported.reference,
                    size_bytes: imported.size_bytes,
                });
            }
            let Some(labels) = control_labels else {
                remove_loaded(&backend, "", &loaded_nodes, id).await;
                return Err(anyhow!("multi-node bundle has no control VM snapshot"));
            };
            if compute_count == 0 {
                remove_loaded(&backend, "", &loaded_nodes, id).await;
                return Err(anyhow!(
                    "multi-node bundle must contain at least one compute VM snapshot"
                ));
            }
            let expected_computes: Vec<String> = (1..=compute_count)
                .map(|index| format!("compute-{index}"))
                .collect();
            let actual_computes: Vec<&str> = loaded_nodes
                .iter()
                .filter(|n| n.node != "control")
                .map(|n| n.node.as_str())
                .collect();
            if actual_computes.len() != expected_computes.len()
                || !expected_computes
                    .iter()
                    .all(|expected| actual_computes.contains(&expected.as_str()))
            {
                remove_loaded(&backend, "", &loaded_nodes, id).await;
                return Err(anyhow!(
                    "multi-node bundle compute VMs must be numbered compute-1..compute-{compute_count}"
                ));
            }
            // Control first, then compute-1, compute-2, ...
            loaded_nodes.sort_by_key(|n| {
                let index = n
                    .node
                    .strip_prefix("compute-")
                    .and_then(|s| s.parse::<u8>().ok())
                    .unwrap_or(0);
                (n.node != "control", index)
            });
            let reference = loaded_nodes[0].reference.clone();
            let size_bytes = loaded_nodes.iter().map(|n| n.size_bytes).sum();
            match parse_labels(
                &labels,
                Some((
                    compute_count,
                    extracted.manifest.compute,
                    extracted.manifest.gpu,
                )),
            ) {
                Ok(meta) => Ok(LoadedArchive {
                    meta,
                    reference,
                    nodes: loaded_nodes,
                    size_bytes,
                }),
                Err(error) => {
                    remove_loaded(&backend, &reference, &loaded_nodes, id).await;
                    Err(error)
                }
            }
        }
        .await;
        let _ = std::fs::remove_dir_all(&stage_dir);
        result
    }

    /// Removes the snapshot from the host that keeps it, then its copies on other hosts as
    /// far as those answer. The original on a remote host that is no longer registered only
    /// loses its record. Returns the copies that could not be removed, by host.
    pub async fn delete_snapshot(&self, id: &str) -> Result<Vec<String>> {
        self.remove_snapshot(id, false).await
    }
    /// Removes the snapshot's record without asking the host that keeps its disk, for a host
    /// that is gone. The disk, if any is left, stays on that host, and so do its copies.
    pub async fn forget_snapshot(&self, id: &str) -> Result<()> {
        self.remove_snapshot(id, true).await.map(|_| ())
    }
    async fn remove_snapshot(&self, id: &str, forget: bool) -> Result<Vec<String>> {
        let (references, previous, copies, snapshot) = self.update(|state| {
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
            let references: Vec<String> = if snapshot.nodes.is_empty() {
                snapshot.reference.iter().cloned().collect()
            } else {
                // The control VM's disks were captured first and head the group, which msb
                // removes only once it is the last member.
                snapshot
                    .nodes
                    .iter()
                    .rev()
                    .map(|n| n.reference.clone())
                    .collect()
            };
            Ok((
                references,
                previous,
                snapshot.copies.clone(),
                snapshot.clone(),
            ))
        })?;
        let removed = async {
            // A failed import or freeze may have left no disk at all: there is nothing on any host
            // to ask about, so only the record goes.
            let diskless = snapshot.reference.as_deref().unwrap_or_default().is_empty()
                && snapshot.nodes.is_empty()
                && snapshot.copies.is_empty();
            // A remote host that is gone takes the original with it; only the record is left.
            if forget
                || diskless
                || snapshot.host_id.is_some()
                    && self.read(|state| origin_host(state, &snapshot))?.is_none()
            {
                return Ok(());
            }
            let backend = self.origin_agent(&snapshot)?;
            for reference in references {
                backend.remove_snapshot(reference).await?;
            }
            Ok::<_, anyhow::Error>(())
        }
        .await;
        if let Err(error) = removed {
            let message = format!("{error:#}");
            self.update_snapshot(id, |s| {
                s.status = previous;
                s.error = Some(format!("delete failed: {message}"));
            })?;
            return Err(error);
        }
        let left = if forget {
            Vec::new()
        } else {
            self.remove_snapshot_copies(&copies).await
        };
        for copy in &left {
            eprintln!("snapshot {id} was deleted, but its copy on {copy}");
        }
        self.update(|state| {
            state.snapshots.remove(id);
            Ok(left)
        })
    }
}
