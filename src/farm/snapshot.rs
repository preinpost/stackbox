//! Disk snapshots of a farm's all-in-one VM, and farms restored from them.

use std::{collections::BTreeMap, path::Path, time::Duration};

use anyhow::{Context, Result, bail};
use microsandbox::snapshot::{LoadOpts, SaveOpts, Snapshot, SnapshotReference};
use microsandbox::{NetworkPolicy, NetworkProfile, Sandbox};

use super::node::{
    FARM_LABEL, FarmNode, GUEST_HOSTNAME, IDENTITY_CONF, ROLE_LABEL, connect, node_name,
    wait_until_booted,
};
use super::template::NodeSpec;
use crate::model::Exposure;
use crate::shell::run;

/// Clean shutdown of every Kolla container fits well within this.
const STOP_TIMEOUT: Duration = Duration::from_secs(180);

pub struct FrozenSnapshot {
    pub reference: String,
    pub size_bytes: Option<u64>,
    /// The capture succeeded, but the source VM did not start again.
    pub restart_error: Option<String>,
}
pub struct ImportedSnapshot {
    pub reference: String,
    pub labels: BTreeMap<String, String>,
    pub size_bytes: Option<u64>,
}

/// Each snapshot gets its own msb group, so imports never contend for a shared group head.
fn group(snapshot_id: &str) -> String {
    format!("farm-snap-{snapshot_id}")
}

/// Stop the farm's VM so MariaDB and RabbitMQ shut down cleanly, capture its disks
/// (root and both container disks), then start it again.
pub async fn freeze_farm(
    farm_id: &str,
    snapshot_id: &str,
    labels: &BTreeMap<String, String>,
) -> Result<FrozenSnapshot> {
    let name = super::all_in_one_name(farm_id).await?;
    let sandbox = connect(&name).await?;
    let identity = run(
        &sandbox,
        &format!("if [ -f {IDENTITY_CONF} ]; then echo yes; fi"),
    )
    .await;
    sandbox.detach().await;
    if identity?.trim() != "yes" {
        bail!(
            "{name} was created before farms had a fixed guest identity, so its OpenStack is bound to an address a restored copy cannot keep; create and deploy a new farm, then freeze that one"
        );
    }
    let handle = Sandbox::get(&name)
        .await
        .with_context(|| format!("find {name}"))?;
    handle
        .stop_with_timeout(STOP_TIMEOUT)
        .await
        .with_context(|| format!("stop {name}"))?;
    let captured = async {
        let mut builder = Snapshot::builder(snapshot_id)
            .group(group(snapshot_id))
            .from_sandbox(&name);
        for (key, value) in labels {
            builder = builder.label(key, value);
        }
        let snapshot = builder.create().await.context("capture disk snapshot")?;
        Ok::<_, anyhow::Error>(FrozenSnapshot {
            reference: snapshot.reference().value().to_owned(),
            size_bytes: snapshot.size_bytes(),
            restart_error: None,
        })
    }
    .await;
    // Bring the source farm back whether or not the capture succeeded.
    let restart_error = start(&name).await.err().map(|e| format!("{e:#}"));
    match (captured, restart_error) {
        (Ok(frozen), restart_error) => Ok(FrozenSnapshot {
            restart_error,
            ..frozen
        }),
        (Err(e), None) => Err(e),
        (Err(e), Some(restart)) => {
            Err(e.context(format!("{name} also did not start again: {restart}")))
        }
    }
}

async fn start(name: &str) -> Result<()> {
    let sandbox = Sandbox::start_detached(name)
        .await
        .with_context(|| format!("start {name}"))?;
    let booted = wait_until_booted(&sandbox).await;
    sandbox.detach().await;
    booted
}

/// Boot a copy of the snapshot as `farm_id`'s VM, publishing `exposure`'s ports.
pub async fn restore_farm(
    farm_id: &str,
    reference: &str,
    spec: NodeSpec,
    exposure: &Exposure,
    components: &[String],
) -> Result<Vec<FarmNode>> {
    if !super::farm_handles(farm_id).await?.is_empty() {
        bail!("farm '{farm_id}' already exists");
    }
    let name = node_name(farm_id, spec.role);
    // `Sandbox::restore` boots a disk snapshot without an init handoff and cannot set init or
    // labels, so the guest never reaches systemd. The creation builder accepts a snapshot
    // source through `override_snapshot` (public, though doc-hidden), with the same init,
    // hostname and labels as a created VM. The snapshot supplies the root and container disks.
    let sandbox = Sandbox::builder(&name)
        .override_snapshot(reference)
        .cpus(spec.cpus)
        .memory(spec.memory_mib)
        .init("/sbin/init")
        .hostname(GUEST_HOSTNAME)
        .network(|n| {
            let mut n = n.policy(NetworkPolicy::from_profiles([
                NetworkProfile::Public,
                NetworkProfile::Host,
            ]));
            for (offset, guest_port) in super::public_ports(components) {
                n = n.port_bind(exposure.bind, exposure.port_base + offset, guest_port);
            }
            n
        })
        .label(FARM_LABEL, farm_id)
        .label(ROLE_LABEL, spec.role.as_str())
        .detached(true)
        .create()
        .await
        .with_context(|| format!("restore {name}"))?;
    let boot = async {
        wait_until_booted(&sandbox).await?;
        let ip = run(&sandbox, "ip -4 -o addr show dev eth0 scope global | awk 'NR==1 { split($4, a, \"/\"); print a[1] }'").await?;
        Ok::<_, anyhow::Error>(ip.trim().to_owned())
    }
    .await;
    sandbox.detach().await;
    let ip = boot.with_context(|| {
        format!("{name} restore failed; VM is left for inspection and explicit deletion")
    })?;
    Ok(vec![FarmNode {
        name,
        role: Some(spec.role),
        status: "running".into(),
        memory_mib: spec.memory_mib,
        ip: Some(ip),
    }])
}

/// Write a self-contained `.msb` archive, including the golden image so it boots on another host.
pub async fn export_snapshot(reference: &str, output: &Path) -> Result<()> {
    Snapshot::save(
        reference,
        output,
        SaveOpts {
            with_image: true,
            ..Default::default()
        },
    )
    .await
    .with_context(|| format!("export snapshot to {}", output.display()))
}

pub async fn import_snapshot(archive: &Path, snapshot_id: &str) -> Result<ImportedSnapshot> {
    let handle = Snapshot::load_with_options(
        archive,
        LoadOpts {
            group: Some(group(snapshot_id)),
            ..Default::default()
        },
    )
    .await
    .with_context(|| format!("import {}", archive.display()))?;
    let reference = handle.reference();
    let snapshot = Snapshot::open_ref(reference.clone()).await?;
    Ok(ImportedSnapshot {
        reference: reference.value().to_owned(),
        labels: snapshot.labels().clone(),
        size_bytes: handle.size_bytes(),
    })
}

pub async fn remove_snapshot(reference: &str) -> Result<()> {
    Snapshot::remove_ref(SnapshotReference::auto(reference), false)
        .await
        .with_context(|| format!("remove snapshot {reference}"))
}
