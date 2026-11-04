//! Disk snapshots of a farm's VMs, and farms restored from them.

use std::{collections::BTreeMap, path::Path, time::Duration};

use anyhow::{Context, Result, bail};
use futures::future::{join_all, try_join_all};
use microsandbox::snapshot::{LoadOpts, SaveOpts, Snapshot, SnapshotReference};
use microsandbox::{NetworkPolicy, NetworkProfile, Sandbox};

use super::node::{
    FARM_LABEL, FarmNode, GUEST_HOSTNAME, IDENTITY_CONF, ROLE_LABEL, connect, create_nested,
    node_name, wait_until_booted,
};
use super::template::{NodeSpec, Role, suffix};
use crate::model::{Exposure, snapshot_labels};
use crate::shell::run;

/// Clean shutdown of every Kolla container fits well within this.
const STOP_TIMEOUT: Duration = Duration::from_secs(180);

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct FrozenSnapshot {
    pub reference: String,
    pub size_bytes: Option<u64>,
    /// The capture succeeded, but the source VM did not start again.
    pub restart_error: Option<String>,
    /// A multi-node farm's VMs, control first; `reference` is then the control VM's.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub nodes: Vec<NodeSnapshot>,
}
/// One VM's disks in a multi-node snapshot.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize, utoipa::ToSchema)]
pub struct NodeSnapshot {
    /// The VM it was taken from: `control`, `compute-1`, ...
    pub node: String,
    pub reference: String,
    pub size_bytes: Option<u64>,
}
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
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
    let nodes = super::get_farm(farm_id).await?;
    if super::observed_layout(&nodes).is_some_and(|computes| computes > 0) {
        return freeze_nodes(farm_id, snapshot_id, labels, nodes).await;
    }
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
            nodes: Vec::new(),
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

/// The `farm-link@` units of a multi-node VM's links.
fn link_units() -> String {
    super::NETWORKS
        .iter()
        .map(|(dev, _, _)| format!("farm-link@{dev}"))
        .collect::<Vec<_>>()
        .join(" ")
}

async fn run_in(name: &str, script: &str) -> Result<()> {
    let sandbox = connect(name).await?;
    let result = run(&sandbox, script).await;
    sandbox.detach().await;
    result
        .map(drop)
        .with_context(|| format!("{name}: {script}"))
}

/// Freeze a multi-node farm: computes stop before the control VM, so nova-compute leaves
/// RabbitMQ before it goes, every VM's disks are captured into the snapshot's group, and the
/// control VM starts again before the computes.
///
/// A restored VM must not join its source farm's links: the switch hands a port to whoever
/// connects to it last. So the links are disabled on disk while frozen; the source VMs
/// enable them again after they boot, and a restored copy only once it has its own ports.
async fn freeze_nodes(
    farm_id: &str,
    snapshot_id: &str,
    labels: &BTreeMap<String, String>,
    mut nodes: Vec<super::FarmNode>,
) -> Result<FrozenSnapshot> {
    nodes.sort_by_key(|node| (node.role != Some(Role::Control), node.index));
    let vms: Vec<(String, String)> = nodes
        .iter()
        .map(|node| {
            let role = node.role.context("multi-node VM has no role")?;
            let suffix = suffix(role, node.index).context("multi-node VM has no suffix")?;
            Ok((node_name(farm_id, role, node.index), suffix))
        })
        .collect::<Result<_>>()?;
    let units = link_units();
    let (disable, enable) = (
        format!("systemctl disable {units}"),
        format!("systemctl enable {units}"),
    );
    let disabled = try_join_all(vms.iter().map(|(name, _)| run_in(name, &disable))).await;
    if let Err(error) = disabled {
        join_all(vms.iter().map(|(name, _)| run_in(name, &enable))).await;
        return Err(error.context("disable the links before freezing"));
    }
    let (control, computes) = vms.split_first().context("farm has no VMs")?;
    let stopped = async {
        try_join_all(computes.iter().map(|(name, _)| stop(name))).await?;
        stop(&control.0).await
    }
    .await;
    let captured = match stopped {
        Ok(()) => capture_nodes(snapshot_id, labels, &vms).await,
        Err(error) => Err(error),
    };
    // Bring the source farm back whether or not the capture succeeded.
    let mut restart_errors = Vec::new();
    if let Err(error) = start_with_links(&control.0, &units).await {
        restart_errors.push(format!("{error:#}"));
    }
    for result in join_all(
        computes
            .iter()
            .map(|(name, _)| start_with_links(name, &units)),
    )
    .await
    {
        if let Err(error) = result {
            restart_errors.push(format!("{error:#}"));
        }
    }
    let restart_error = (!restart_errors.is_empty()).then(|| restart_errors.join("; "));
    match (captured, restart_error) {
        (Ok(nodes), restart_error) => Ok(FrozenSnapshot {
            reference: nodes[0].reference.clone(),
            size_bytes: nodes.iter().map(|n| n.size_bytes).sum(),
            restart_error,
            nodes,
        }),
        (Err(e), None) => Err(e),
        (Err(e), Some(restart)) => {
            Err(e.context(format!("the farm also did not start again: {restart}")))
        }
    }
}

/// Freeze one compute VM of a multi-node farm as a compute template: a single-VM snapshot
/// that later expansions boot a new compute from. The farm's other VMs keep running.
///
/// The captured disk still carries the source's per-node identity (Nova's `compute_id`,
/// the machine ID, OVS's system-id): a stopped VM's disk cannot be edited, and removing
/// them from the running source would make it register again. So, like the links, Docker
/// is disabled on disk while frozen. A compute booted from the template then starts no
/// Kolla container until its identity is wiped (see `kolla::deploy_compute`); the source
/// enables both again after it boots.
pub async fn freeze_compute(
    farm_id: &str,
    snapshot_id: &str,
    node: &str,
    labels: &BTreeMap<String, String>,
) -> Result<FrozenSnapshot> {
    let index: u8 = node
        .strip_prefix("compute-")
        .and_then(|index| index.parse().ok())
        .with_context(|| format!("{node} is not a compute VM"))?;
    let name = node_name(farm_id, Role::Compute, Some(index));
    // Instances would come back on every compute made from the template.
    run_in(&name, NO_INSTANCES).await?;
    let units = link_units();
    let (disable, enable) = (
        format!("systemctl disable {units} {DOCKER_UNITS}"),
        format!("systemctl enable {units} {DOCKER_UNITS}"),
    );
    if let Err(error) = run_in(&name, &disable).await {
        let _ = run_in(&name, &enable).await;
        return Err(error.context("disable the links and Docker before freezing"));
    }
    let captured = match stop(&name).await {
        Ok(()) => {
            let mut builder = Snapshot::builder(snapshot_id)
                .group(group(snapshot_id))
                .from_sandbox(&name)
                .label(snapshot_labels::NODE, node);
            for (key, value) in labels {
                builder = builder.label(key, value);
            }
            builder
                .create()
                .await
                .map(|snapshot| FrozenSnapshot {
                    reference: snapshot.reference().value().to_owned(),
                    size_bytes: snapshot.size_bytes(),
                    restart_error: None,
                    nodes: Vec::new(),
                })
                .map_err(|error| anyhow::Error::from(error).context(format!("capture {name}")))
        }
        Err(error) => Err(error),
    };
    // Bring the source compute back whether or not the capture succeeded.
    let restart_error = async {
        start_with_links(&name, &units).await?;
        run_in(
            &name,
            &format!("{enable} && systemctl start docker.service"),
        )
        .await
    }
    .await
    .err()
    .map(|e| format!("{e:#}"));
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

/// Docker's units, kept off in a compute template until its identity is wiped.
const DOCKER_UNITS: &str = "docker.service docker.socket";

const NO_INSTANCES: &str = r#"if docker ps --format '{{.Names}}' | grep -qx nova_libvirt; then
  n=$(docker exec nova_libvirt virsh list --all --name | grep -c . || true)
  if [ "$n" != 0 ]; then
    echo "$n instance(s) run on this compute; delete them before freezing it as a template" >&2
    exit 1
  fi
fi"#;

async fn stop(name: &str) -> Result<()> {
    Sandbox::get(name)
        .await
        .with_context(|| format!("find {name}"))?
        .stop_with_timeout(STOP_TIMEOUT)
        .await
        .with_context(|| format!("stop {name}"))
}

/// Capture each VM in order, removing what was captured if one fails.
async fn capture_nodes(
    snapshot_id: &str,
    labels: &BTreeMap<String, String>,
    vms: &[(String, String)],
) -> Result<Vec<NodeSnapshot>> {
    let mut nodes = Vec::new();
    for (name, suffix) in vms {
        let mut builder = Snapshot::builder(format!("{snapshot_id}-{suffix}"))
            .group(group(snapshot_id))
            .from_sandbox(name)
            .label(snapshot_labels::NODE, suffix);
        for (key, value) in labels {
            builder = builder.label(key, value);
        }
        match builder.create().await {
            Ok(snapshot) => nodes.push(NodeSnapshot {
                node: suffix.clone(),
                reference: snapshot.reference().value().to_owned(),
                size_bytes: snapshot.size_bytes(),
            }),
            Err(error) => {
                for node in &nodes {
                    let _ = remove_snapshot(&node.reference).await;
                }
                return Err(anyhow::Error::from(error).context(format!("capture {name}")));
            }
        }
    }
    Ok(nodes)
}

async fn start_with_links(name: &str, units: &str) -> Result<()> {
    start(name).await?;
    // farm-identity ran without farm0 at boot, so publish the ports again once it exists.
    run_in(
        name,
        &format!("for dev in farm0 farm1; do if ip -d link show $dev 2>/dev/null | grep -qw dummy; then ip link del $dev; fi; done\nsystemctl enable --now {units} && systemctl restart farm-identity farm-hostname"),
    )
    .await
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
    let name = node_name(farm_id, spec.role, spec.index);
    // `Sandbox::restore` boots a disk snapshot without an init handoff and cannot set init or
    // labels, so the guest never reaches systemd. The creation builder accepts a snapshot
    // source through `override_snapshot` (public, though doc-hidden), with the same init,
    // hostname and labels as a created VM. The snapshot supplies the root and container disks.
    let sandbox = create_nested(|| {
        Sandbox::builder(&name)
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
    })
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
        index: spec.index,
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
    match Snapshot::remove_ref(SnapshotReference::auto(reference), false).await {
        // Already removed, e.g. with `msb snapshot rm`: only the coordinator's record is left.
        Err(microsandbox::MicrosandboxError::SnapshotNotFound(_)) => Ok(()),
        result => result.with_context(|| format!("remove snapshot {reference}")),
    }
}
