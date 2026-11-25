pub mod golden;
mod node;
mod snapshot;
mod template;
mod terminal;

use std::collections::BTreeMap;

use anyhow::{Context, Result, bail};
use futures::future::try_join_all;
use microsandbox::Sandbox;

use crate::shell::run;
pub use node::{
    GUEST_ADDRESS, GUEST_INTERFACE, IDENTITY_CONF, LEGACY_ALL_IN_ONE_SUFFIX, NodeSetup,
    StackboxNode, connect, create_node, detach, install_guest_files, install_links, is_node_name,
    node_name, wait_until_booted,
};
use node::{STACKBOX_LABEL, describe_node, destroy_node, identity_conf, settings};
pub use snapshot::{
    FrozenSnapshot, ImportedSnapshot, NodeSnapshot, export_snapshot, freeze_compute,
    freeze_control, freeze_stackbox, import_snapshot, remove_snapshot, restore_stackbox,
    snapshot_labels,
};
pub use template::{
    ALL_IN_ONE, COMPUTE, CONTROL, MAX_COMPUTES, NodeSpec, Role, address, hostname, suffix,
};
pub use terminal::{
    ScriptSession, TerminalInput, TerminalIo, TerminalSize, attach as attach_terminal, run_script,
};

/// A multi-node stackbox's networks, each a link on every VM: `(guest device, network, addressed)`.
/// `mgmt` carries Kolla's network and API traffic on the VM's fixed address; `tenant` carries
/// Neutron's tenant VLANs through an OVS bridge, so it has no address.
pub const NETWORKS: [(&str, &str, bool); 2] =
    [("stackbox0", "mgmt", true), ("stackbox1", "tenant", false)];

/// Create the stackbox's single all-in-one VM and confirm it boots.
pub async fn create_stackbox(
    stackbox_id: &str,
    template: &[NodeSpec],
) -> Result<Vec<StackboxNode>> {
    create_stackbox_with(stackbox_id, template, None, &[], None, &[], None, &[]).await
}

/// `components` selects which service ports the stackbox publishes. A multi-node stackbox needs
/// `switch`, the host's link switch, to join its VMs. `sources` are msb snapshots to boot
/// each VM of `template` from, in its order; empty boots them all from the golden image.
/// With `share`, the stackbox spans hosts: only the share's VMs are created here, and the
/// switch joins their networks to the other hosts through the relay. `pci_devices` go to
/// compute-1 only, since one physical device cannot sit in two VMs.
#[allow(clippy::too_many_arguments)] // Each argument is a distinct part of the stackbox's placement.
pub async fn create_stackbox_with(
    stackbox_id: &str,
    template: &[NodeSpec],
    exposure: Option<&crate::model::Exposure>,
    components: &[String],
    switch: Option<&crate::link::host::Switch>,
    sources: &[String],
    share: Option<&crate::remote::protocol::Share>,
    pci_devices: &[String],
) -> Result<Vec<StackboxNode>> {
    layout(template)?;
    if !sources.is_empty() && sources.len() != template.len() {
        bail!("{} snapshots for {} VMs", sources.len(), template.len());
    }
    if !stackbox_handles(stackbox_id).await?.is_empty() {
        bail!("stackbox '{stackbox_id}' already exists");
    }
    if template[0].role == Role::AllInOne {
        let setup = NodeSetup {
            exposure,
            public_ports: public_ports(components),
            source: sources.first().cloned(),
            ..Default::default()
        };
        return Ok(vec![create_node(stackbox_id, template[0], &setup).await?]);
    }
    // A share of one compute may boot from a compute template; nothing else that spans
    // hosts starts from snapshots.
    if let Some(share) = share
        && !sources.is_empty()
        && !(share.nodes.len() == 1 && share.nodes[0].starts_with("compute-"))
    {
        bail!("a stackbox that spans hosts cannot start from snapshots");
    }
    let switch = switch.context("multi-node stackboxes need the host's link switch")?;
    if let Some(share) = share {
        switch.set_uplink(stackbox_id, share.uplink.clone())?;
    }
    let url = switch.ensure(&switch_command()?).await?;
    let here = |spec: &NodeSpec| {
        share.is_none_or(|share| {
            suffix(spec.role, spec.index).is_some_and(|s| share.nodes.contains(&s))
        })
    };
    let mut setups = Vec::new();
    for (i, spec) in template.iter().enumerate().filter(|(_, spec)| here(spec)) {
        let suffix = suffix(spec.role, spec.index).context("multi-node VMs have a suffix")?;
        let mut links = Vec::new();
        for (dev, network, addressed) in NETWORKS {
            let port = format!("{stackbox_id}/{suffix}/{network}");
            let token = switch.add_port(&port, &format!("{stackbox_id}/{network}"))?;
            links.push(crate::link::guest::LinkConf {
                dev: dev.into(),
                switch: url.clone(),
                port,
                token,
                mtu: crate::link::DEFAULT_MTU,
                address: addressed.then(|| format!("{}/24", spec.address())),
            });
        }
        // The control VM serves the stackbox's APIs; the computes publish nothing.
        let control = spec.role == Role::Control;
        setups.push(NodeSetup {
            pci_devices: pci_devices_of(spec, pci_devices),
            exposure: exposure.filter(|_| control),
            public_ports: if control {
                public_ports(components)
            } else {
                Vec::new()
            },
            peers: template
                .iter()
                .filter(|other| other.hostname() != spec.hostname())
                .map(|other| (other.address(), other.hostname()))
                .collect(),
            links,
            source: sources.get(i).cloned(),
        });
    }
    try_join_all(
        template
            .iter()
            .filter(|spec| here(spec))
            .zip(&setups)
            .map(|(spec, setup)| create_node(stackbox_id, *spec, setup)),
    )
    .await
}

/// The PCI devices a VM of the stackbox receives: compute-1's, and none for any other VM.
fn pci_devices_of(spec: &NodeSpec, pci_devices: &[String]) -> Vec<String> {
    if spec.role == Role::Compute && spec.index == Some(1) {
        pci_devices.to_vec()
    } else {
        Vec::new()
    }
}

/// Add compute VMs to an existing multi-node stackbox on this host. The switch is already
/// running, and new ports are picked up from its configuration file. An existing VM with
/// the requested suffix is adopted instead of created again, so a retried expansion
/// continues where the last attempt stopped. `source` is a compute template snapshot to
/// boot the new VMs from instead of the golden image. `pci_devices` reach compute-1 only,
/// and a template cannot carry them, so compute-1 never boots from one.
pub async fn add_nodes(
    stackbox_id: &str,
    template: &[NodeSpec],
    add: &[String],
    switch: &crate::link::host::Switch,
    source: Option<&str>,
    pci_devices: &[String],
) -> Result<Vec<StackboxNode>> {
    if add.is_empty() {
        return Ok(Vec::new());
    }
    layout(template)?;
    let url = switch.ensure(&switch_command()?).await?;
    let existing: BTreeMap<String, StackboxNode> = get_stackbox(stackbox_id)
        .await?
        .into_iter()
        .map(|node| (node.name.clone(), node))
        .collect();
    let mut created = Vec::new();
    for node in add {
        let spec = template
            .iter()
            .find(|spec| suffix(spec.role, spec.index).as_ref() == Some(node))
            .with_context(|| format!("the stackbox's plan has no VM '{node}'"))?;
        let name = node_name(stackbox_id, spec.role, spec.index);
        if let Some(node) = existing.get(&name) {
            if node.status == "running" {
                created.push(node.clone());
                continue;
            }
            bail!(
                "{name} exists but is {}; start or delete the stackbox's VM before retrying",
                node.status
            );
        }
        let mut links = Vec::new();
        for (dev, network, addressed) in NETWORKS {
            let port = format!("{stackbox_id}/{node}/{network}");
            let token = switch.add_port(&port, &format!("{stackbox_id}/{network}"))?;
            links.push(crate::link::guest::LinkConf {
                dev: dev.into(),
                switch: url.clone(),
                port,
                token,
                mtu: crate::link::DEFAULT_MTU,
                address: addressed.then(|| format!("{}/24", spec.address())),
            });
        }
        // A VM booted from a compute template gets the same devices and vfio files as a clean one:
        // create_node attaches the devices and runs install_vfio_files after the boot.
        let pci = pci_devices_of(spec, pci_devices);
        let setup = NodeSetup {
            peers: template
                .iter()
                .filter(|other| other.hostname() != spec.hostname())
                .map(|other| (other.address(), other.hostname()))
                .collect(),
            links,
            source: source.map(str::to_owned),
            pci_devices: pci,
            ..Default::default()
        };
        created.push(create_node(stackbox_id, *spec, &setup).await?);
    }
    Ok(created)
}

/// Refresh a VM's fixed identity and `/etc/hosts` peers after the stackbox gained a compute.
pub async fn update_identity(
    sandbox: &Sandbox,
    spec: &NodeSpec,
    template: &[NodeSpec],
) -> Result<()> {
    let publish = matches!(spec.role, Role::Control | Role::AllInOne);
    let peers: Vec<(String, String)> = template
        .iter()
        .filter(|other| other.hostname() != spec.hostname())
        .map(|other| (other.address(), other.hostname()))
        .collect();
    sandbox
        .fs()
        .write(IDENTITY_CONF, identity_conf(spec, publish, &peers))
        .await?;
    run(sandbox, "systemctl restart stackbox-hostname").await?;
    Ok(())
}

/// `agent switch`, which both the `stackbox` and `stackbox-agent` binaries take.
pub fn switch_command() -> Result<Vec<std::ffi::OsString>> {
    Ok(vec![
        std::env::current_exe()
            .context("cannot locate the agent binary")?
            .into(),
        "agent".into(),
        "switch".into(),
    ])
}

/// `(port_base offset, guest port)` of each selected service, plus the data stores every stackbox installs.
fn public_ports(components: &[String]) -> Vec<(u16, u16)> {
    crate::model::COMPONENTS
        .iter()
        .filter(|c| components.iter().any(|id| id == c.id))
        .flat_map(|c| c.public.iter().map(|p| (p.offset, p.guest_port)))
        .chain(
            crate::model::INFRA_PORTS
                .iter()
                .map(|p| (p.offset, p.guest_port)),
        )
        .collect()
}

/// How many compute VMs a layout new stackboxes can have has: 0 for one all-in-one VM, or a
/// control VM followed by computes numbered from 1. A control VM alone, restored from a
/// control template, has none yet. Legacy multi-VM layouts are refused.
pub fn layout(template: &[NodeSpec]) -> Result<u8> {
    match template {
        [spec] if spec.role == Role::AllInOne => Ok(0),
        [control, computes @ ..]
            if control.role == Role::Control
                && control.index.is_none()
                && computes.len() <= usize::from(MAX_COMPUTES)
                && computes.iter().enumerate().all(|(i, spec)| {
                    spec.role == Role::Compute && spec.index == Some(i as u8 + 1)
                }) =>
        {
            Ok(computes.len() as u8)
        }
        [] => bail!("a stackbox needs at least one VM"),
        _ => bail!(
            "legacy multi-VM layouts cannot be created; create an all-in-one or control + compute stackbox"
        ),
    }
}

/// The layout of a stackbox's observed VMs, as [`layout`]; `None` for legacy or partial stackboxes.
pub fn observed_layout(nodes: &[StackboxNode]) -> Option<u8> {
    let mut specs: Vec<NodeSpec> = nodes
        .iter()
        .map(|node| {
            Some(NodeSpec {
                role: node.role?,
                index: node.index,
                ..ALL_IN_ONE
            })
        })
        .collect::<Option<_>>()?;
    specs.sort_by_key(|spec| {
        (
            spec.role != Role::AllInOne && spec.role != Role::Control,
            spec.index,
        )
    });
    layout(&specs).ok()
}

pub async fn get_stackbox(stackbox_id: &str) -> Result<Vec<StackboxNode>> {
    let mut nodes = Vec::new();
    for sandbox in stackbox_handles(stackbox_id).await? {
        nodes.push(describe_node(&sandbox).await);
    }
    Ok(nodes)
}

pub async fn delete_stackbox(stackbox_id: &str) -> Result<usize> {
    let handles = stackbox_handles(stackbox_id).await?;
    try_join_all(handles.iter().map(destroy_node)).await?;
    // A destroyed VM has been seen to linger in msb's list as `crashed`; only report success once it is gone.
    for _ in 0..10 {
        let left = stackbox_handles(stackbox_id).await?;
        if left.is_empty() {
            return Ok(handles.len());
        }
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        try_join_all(left.iter().map(destroy_node)).await?;
    }
    let left: Vec<_> = stackbox_handles(stackbox_id)
        .await?
        .iter()
        .map(|h| h.name().to_owned())
        .collect();
    if left.is_empty() {
        return Ok(handles.len());
    }
    bail!("VMs still listed after deletion: {}", left.join(", "))
}

pub async fn list_stackboxes() -> Result<BTreeMap<String, usize>> {
    let mut stackboxes = BTreeMap::new();
    for handle in all_handles().await? {
        if let Some(stackbox_id) = stackbox_of(&handle).await {
            *stackboxes.entry(stackbox_id).or_default() += 1;
        }
    }
    Ok(stackboxes)
}

pub async fn scan_stackboxes() -> Result<BTreeMap<String, Vec<StackboxNode>>> {
    let mut stackboxes: BTreeMap<String, Vec<StackboxNode>> = BTreeMap::new();
    for handle in all_handles().await? {
        if let Some(id) = stackbox_of(&handle).await {
            stackboxes
                .entry(id)
                .or_default()
                .push(describe_node(&handle).await);
        }
    }
    for nodes in stackboxes.values_mut() {
        nodes.sort_by(|a, b| a.name.cmp(&b.name));
    }
    Ok(stackboxes)
}

/// Filters the same unfiltered listing that `scan_stackboxes` observes. msb's label-filtered
/// listing has returned nothing for a stackbox whose VM was still listed, so a delete
/// "succeeded" without destroying it.
async fn stackbox_handles(stackbox_id: &str) -> Result<Vec<Sandbox>> {
    let mut handles = Vec::new();
    for handle in all_handles().await? {
        if stackbox_of(&handle).await.as_deref() == Some(stackbox_id) {
            handles.push(handle);
        }
    }
    Ok(handles)
}

async fn stackbox_of(handle: &Sandbox) -> Option<String> {
    let (labels, _) = settings(handle).await;
    owning_stackbox(
        handle.name(),
        labels.get(STACKBOX_LABEL).map(String::as_str),
        labels.get(node::ROLE_LABEL).map(String::as_str),
    )
}

/// Name of the VM that runs Kolla and serves the stackbox's APIs: the all-in-one VM (which
/// predates the stackbox-id naming on older stackboxes) or a multi-node stackbox's control VM.
pub async fn all_in_one_name(stackbox_id: &str) -> Result<String> {
    let nodes = get_stackbox(stackbox_id).await?;
    let control = observed_layout(&nodes)
        .and_then(|_| nodes.iter().find(|node| node.role == Some(Role::Control)));
    nodes
        .iter()
        .find(|node| node.role == Some(Role::AllInOne))
        .or(control)
        .map(|node| node.name.clone())
        .with_context(|| format!("stackbox '{stackbox_id}' has no all-in-one or control VM"))
}

/// A labelled all-in-one VM belongs to the stackbox in its name, so a snapshot copy can never
/// join its source stackbox. Legacy multi-VM nodes are matched by label, and only when their name agrees.
fn owning_stackbox(name: &str, stackbox: Option<&str>, role: Option<&str>) -> Option<String> {
    let stackbox = stackbox?;
    if role == Some(Role::AllInOne.as_str()) {
        let id = name.strip_suffix(LEGACY_ALL_IN_ONE_SUFFIX).unwrap_or(name);
        return crate::model::validate_id(id).is_ok().then(|| id.to_owned());
    }
    name.strip_prefix(stackbox)
        .is_some_and(|rest| rest.starts_with('-'))
        .then(|| stackbox.to_owned())
}

async fn all_handles() -> Result<Vec<Sandbox>> {
    let mut handles = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let page = Sandbox::list_with(|list| {
            let list = list.limit(100);
            match cursor.clone() {
                Some(c) => list.cursor(c),
                None => list,
            }
        })
        .await?;
        handles.extend(page.sandboxes);
        match page.next_cursor {
            Some(next) => cursor = Some(next),
            None => return Ok(handles),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restored_vms_belong_to_the_stackbox_in_their_name_not_their_source_label() {
        let aio = Some("all-in-one");
        assert_eq!(
            owning_stackbox("dev", Some("dev"), aio).as_deref(),
            Some("dev")
        );
        // Restored from dev's snapshot: still labelled stackbox=dev.
        assert_eq!(
            owning_stackbox("dev2", Some("dev"), aio).as_deref(),
            Some("dev2")
        );
        assert_eq!(owning_stackbox("dev", None, aio), None);
        assert_eq!(owning_stackbox("Bad", Some("dev"), aio), None);
        // VMs created before all-in-one VMs were named after their stackbox.
        assert_eq!(
            owning_stackbox("dev-all-in-one", Some("dev"), aio).as_deref(),
            Some("dev")
        );
        assert_eq!(
            owning_stackbox("dev2-all-in-one", Some("dev"), aio).as_deref(),
            Some("dev2")
        );
        assert_eq!(owning_stackbox("Bad-all-in-one", Some("dev"), aio), None);
        let lb = Some("lb");
        assert_eq!(
            owning_stackbox("old-lb", Some("old"), lb).as_deref(),
            Some("old")
        );
        assert_eq!(owning_stackbox("other-lb", Some("old"), lb), None);
    }

    #[test]
    fn create_takes_all_in_one_or_control_and_numbered_computes_only() {
        let compute = |index| NodeSpec {
            index: Some(index),
            ..COMPUTE
        };
        assert_eq!(layout(&[ALL_IN_ONE]).unwrap(), 0);
        assert_eq!(layout(&[CONTROL, compute(1), compute(2)]).unwrap(), 2);
        assert!(layout(&[]).is_err());
        assert!(layout(&[ALL_IN_ONE, ALL_IN_ONE]).is_err());
        // A control template's stackbox starts without computes.
        assert_eq!(layout(&[CONTROL]).unwrap(), 0);
        assert!(layout(&[CONTROL, compute(2)]).is_err());
        assert!(layout(&[CONTROL, COMPUTE]).is_err());
        assert!(layout(&[CONTROL, compute(1), compute(2), compute(3), compute(4)]).is_err());
        let legacy = NodeSpec {
            role: Role::Deployer,
            ..ALL_IN_ONE
        };
        assert!(
            layout(&[legacy])
                .unwrap_err()
                .to_string()
                .contains("legacy")
        );
        let node = |role, index| StackboxNode {
            name: String::new(),
            role: Some(role),
            index,
            status: "running".into(),
            memory_mib: 0,
            ip: None,
        };
        // Observed nodes come sorted by name, not by role.
        assert_eq!(
            observed_layout(&[
                node(Role::Compute, Some(2)),
                node(Role::Compute, Some(1)),
                node(Role::Control, None)
            ]),
            Some(2)
        );
        assert_eq!(observed_layout(&[node(Role::AllInOne, None)]), Some(0));
        assert_eq!(observed_layout(&[node(Role::Control, None)]), Some(0));
        // A legacy stackbox with control and compute VMs is not a multi-node stackbox.
        assert_eq!(
            observed_layout(&[
                node(Role::Deployer, None),
                node(Role::Control, None),
                node(Role::Compute, None)
            ]),
            None
        );
        assert_eq!(
            observed_layout(&[node(Role::Control, None), node(Role::Compute, None)]),
            None
        );
    }
}
