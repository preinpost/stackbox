pub mod golden;
mod node;
mod snapshot;
mod template;
mod terminal;

use std::collections::BTreeMap;

use anyhow::{Context, Result, bail};
use futures::future::try_join_all;
use microsandbox::Sandbox;
use microsandbox::sandbox::SandboxHandle;

use node::{FARM_LABEL, describe_node, destroy_node, label};
pub use node::{
    FarmNode, GUEST_ADDRESS, GUEST_INTERFACE, IDENTITY_CONF, LEGACY_ALL_IN_ONE_SUFFIX, NodeSetup,
    connect, create_node, install_guest_files, install_links, is_node_name, node_name,
    wait_until_booted,
};
pub use snapshot::{
    FrozenSnapshot, ImportedSnapshot, NodeSnapshot, export_snapshot, freeze_farm, import_snapshot,
    remove_snapshot, restore_farm,
};
pub use template::{
    ALL_IN_ONE, COMPUTE, CONTROL, MAX_COMPUTES, NodeSpec, Role, address, hostname, suffix,
};
pub use terminal::{TerminalInput, TerminalIo, TerminalSize, attach as attach_terminal};

/// A multi-node farm's networks, each a link on every VM: `(guest device, network, addressed)`.
/// `mgmt` carries Kolla's network and API traffic on the VM's fixed address; `tenant` carries
/// Neutron's tenant VLANs through an OVS bridge, so it has no address.
pub const NETWORKS: [(&str, &str, bool); 2] = [("farm0", "mgmt", true), ("farm1", "tenant", false)];

/// Create the farm's single all-in-one VM and confirm it boots.
pub async fn create_farm(farm_id: &str, template: &[NodeSpec]) -> Result<Vec<FarmNode>> {
    create_farm_with(farm_id, template, None, &[], None, &[], None).await
}

/// `components` selects which service ports the farm publishes. A multi-node farm needs
/// `switch`, the host's link switch, to join its VMs. `sources` are msb snapshots to boot
/// each VM of `template` from, in its order; empty boots them all from the golden image.
/// With `share`, the farm spans hosts: only the share's VMs are created here, and the
/// switch joins their networks to the other hosts through the relay.
pub async fn create_farm_with(
    farm_id: &str,
    template: &[NodeSpec],
    exposure: Option<&crate::model::Exposure>,
    components: &[String],
    switch: Option<&crate::link::host::Switch>,
    sources: &[String],
    share: Option<&crate::remote::protocol::Share>,
) -> Result<Vec<FarmNode>> {
    let computes = layout(template)?;
    if !sources.is_empty() && sources.len() != template.len() {
        bail!("{} snapshots for {} VMs", sources.len(), template.len());
    }
    if !farm_handles(farm_id).await?.is_empty() {
        bail!("farm '{farm_id}' already exists");
    }
    if computes == 0 {
        let setup = NodeSetup {
            exposure,
            public_ports: public_ports(components),
            source: sources.first().cloned(),
            ..Default::default()
        };
        return Ok(vec![create_node(farm_id, template[0], &setup).await?]);
    }
    if share.is_some() && !sources.is_empty() {
        bail!("a farm that spans hosts cannot start from snapshots");
    }
    let switch = switch.context("multi-node farms need the host's link switch")?;
    if let Some(share) = share {
        switch.set_uplink(farm_id, share.uplink.clone())?;
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
            let port = format!("{farm_id}/{suffix}/{network}");
            let token = switch.add_port(&port, &format!("{farm_id}/{network}"))?;
            links.push(crate::link::guest::LinkConf {
                dev: dev.into(),
                switch: url.clone(),
                port,
                token,
                mtu: crate::link::DEFAULT_MTU,
                address: addressed.then(|| format!("{}/24", spec.address())),
            });
        }
        // The control VM serves the farm's APIs; the computes publish nothing.
        let control = spec.role == Role::Control;
        setups.push(NodeSetup {
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
            .map(|(spec, setup)| create_node(farm_id, *spec, setup)),
    )
    .await
}

/// `agent switch`, which both the `farm` and `farm-agent` binaries take.
pub fn switch_command() -> Result<Vec<std::ffi::OsString>> {
    Ok(vec![
        std::env::current_exe()
            .context("cannot locate the agent binary")?
            .into(),
        "agent".into(),
        "switch".into(),
    ])
}

/// `(port_base offset, guest port)` of each selected service, plus the data stores every farm installs.
fn public_ports(components: &[String]) -> Vec<(u16, u16)> {
    crate::model::COMPONENTS
        .iter()
        .filter(|c| components.iter().any(|id| id == c.id))
        .filter_map(|c| c.public.as_ref().map(|p| (p.offset, p.guest_port)))
        .chain(
            crate::model::INFRA_PORTS
                .iter()
                .map(|p| (p.offset, p.guest_port)),
        )
        .collect()
}

/// How many compute VMs a layout new farms can have has: 0 for one all-in-one VM, or a
/// control VM followed by computes numbered from 1. Legacy multi-VM layouts are refused.
pub fn layout(template: &[NodeSpec]) -> Result<u8> {
    match template {
        [spec] if spec.role == Role::AllInOne => Ok(0),
        [control, computes @ ..]
            if control.role == Role::Control
                && control.index.is_none()
                && !computes.is_empty()
                && computes.len() <= usize::from(MAX_COMPUTES)
                && computes.iter().enumerate().all(|(i, spec)| {
                    spec.role == Role::Compute && spec.index == Some(i as u8 + 1)
                }) =>
        {
            Ok(computes.len() as u8)
        }
        [] => bail!("a farm needs at least one VM"),
        _ => bail!(
            "legacy multi-VM layouts cannot be created; create an all-in-one or control + compute farm"
        ),
    }
}

/// The layout of a farm's observed VMs, as [`layout`]; `None` for legacy or partial farms.
pub fn observed_layout(nodes: &[FarmNode]) -> Option<u8> {
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

pub async fn get_farm(farm_id: &str) -> Result<Vec<FarmNode>> {
    Ok(farm_handles(farm_id)
        .await?
        .iter()
        .map(describe_node)
        .collect())
}

pub async fn delete_farm(farm_id: &str) -> Result<usize> {
    let handles = farm_handles(farm_id).await?;
    try_join_all(handles.iter().map(destroy_node)).await?;
    // A destroyed VM has been seen to linger in msb's list as `crashed`; only report success once it is gone.
    for _ in 0..10 {
        let left = farm_handles(farm_id).await?;
        if left.is_empty() {
            return Ok(handles.len());
        }
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        try_join_all(left.iter().map(destroy_node)).await?;
    }
    let left: Vec<_> = farm_handles(farm_id)
        .await?
        .iter()
        .map(|h| h.name().to_owned())
        .collect();
    if left.is_empty() {
        return Ok(handles.len());
    }
    bail!("VMs still listed after deletion: {}", left.join(", "))
}

pub async fn list_farms() -> Result<BTreeMap<String, usize>> {
    let mut farms = BTreeMap::new();
    for handle in all_handles().await? {
        if let Some(farm_id) = farm_of(&handle) {
            *farms.entry(farm_id).or_default() += 1;
        }
    }
    Ok(farms)
}

pub async fn scan_farms() -> Result<BTreeMap<String, Vec<FarmNode>>> {
    let mut farms: BTreeMap<String, Vec<FarmNode>> = BTreeMap::new();
    for handle in all_handles().await? {
        if let Some(id) = farm_of(&handle) {
            farms.entry(id).or_default().push(describe_node(&handle));
        }
    }
    for nodes in farms.values_mut() {
        nodes.sort_by(|a, b| a.name.cmp(&b.name));
    }
    Ok(farms)
}

/// Filters the same unfiltered listing that `scan_farms` observes. msb's label-filtered
/// listing has returned nothing for a farm whose VM was still listed, so a delete
/// "succeeded" without destroying it.
async fn farm_handles(farm_id: &str) -> Result<Vec<SandboxHandle>> {
    Ok(all_handles()
        .await?
        .into_iter()
        .filter(|handle| farm_of(handle).as_deref() == Some(farm_id))
        .collect())
}

fn farm_of(handle: &SandboxHandle) -> Option<String> {
    owning_farm(
        handle.name(),
        label(handle, FARM_LABEL).as_deref(),
        label(handle, node::ROLE_LABEL).as_deref(),
    )
}

/// Name of the VM that runs Kolla and serves the farm's APIs: the all-in-one VM (which
/// predates the farm-id naming on older farms) or a multi-node farm's control VM.
pub async fn all_in_one_name(farm_id: &str) -> Result<String> {
    let nodes = get_farm(farm_id).await?;
    let control = observed_layout(&nodes)
        .filter(|computes| *computes > 0)
        .and_then(|_| nodes.iter().find(|node| node.role == Some(Role::Control)));
    nodes
        .iter()
        .find(|node| node.role == Some(Role::AllInOne))
        .or(control)
        .map(|node| node.name.clone())
        .with_context(|| format!("farm '{farm_id}' has no all-in-one or control VM"))
}

/// A labelled all-in-one VM belongs to the farm in its name, so a snapshot copy can never
/// join its source farm. Legacy multi-VM nodes are matched by label, and only when their name agrees.
fn owning_farm(name: &str, farm: Option<&str>, role: Option<&str>) -> Option<String> {
    let farm = farm?;
    if role == Some(Role::AllInOne.as_str()) {
        let id = name.strip_suffix(LEGACY_ALL_IN_ONE_SUFFIX).unwrap_or(name);
        return crate::model::validate_id(id).is_ok().then(|| id.to_owned());
    }
    name.strip_prefix(farm)
        .is_some_and(|rest| rest.starts_with('-'))
        .then(|| farm.to_owned())
}

async fn all_handles() -> Result<Vec<SandboxHandle>> {
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
    fn restored_vms_belong_to_the_farm_in_their_name_not_their_source_label() {
        let aio = Some("all-in-one");
        assert_eq!(owning_farm("dev", Some("dev"), aio).as_deref(), Some("dev"));
        // Restored from dev's snapshot: still labelled farm=dev.
        assert_eq!(
            owning_farm("dev2", Some("dev"), aio).as_deref(),
            Some("dev2")
        );
        assert_eq!(owning_farm("dev", None, aio), None);
        assert_eq!(owning_farm("Bad", Some("dev"), aio), None);
        // VMs created before all-in-one VMs were named after their farm.
        assert_eq!(
            owning_farm("dev-all-in-one", Some("dev"), aio).as_deref(),
            Some("dev")
        );
        assert_eq!(
            owning_farm("dev2-all-in-one", Some("dev"), aio).as_deref(),
            Some("dev2")
        );
        assert_eq!(owning_farm("Bad-all-in-one", Some("dev"), aio), None);
        let lb = Some("lb");
        assert_eq!(
            owning_farm("old-lb", Some("old"), lb).as_deref(),
            Some("old")
        );
        assert_eq!(owning_farm("other-lb", Some("old"), lb), None);
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
        assert!(layout(&[CONTROL]).is_err());
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
        let node = |role, index| FarmNode {
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
        // A legacy farm with control and compute VMs is not a multi-node farm.
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
