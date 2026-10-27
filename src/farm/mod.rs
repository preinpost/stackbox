mod node;
mod snapshot;
mod template;
mod terminal;

use std::collections::BTreeMap;

use anyhow::{Context, Result, bail};
use futures::future::try_join_all;
use microsandbox::Sandbox;
use microsandbox::sandbox::SandboxHandle;

use node::{FARM_LABEL, create_node, describe_node, destroy_node, label};
pub use node::{
    FarmNode, GUEST_ADDRESS, GUEST_INTERFACE, IDENTITY_CONF, LEGACY_ALL_IN_ONE_SUFFIX, connect,
    is_node_name, node_name, wait_until_booted,
};
pub use snapshot::{
    FrozenSnapshot, ImportedSnapshot, export_snapshot, freeze_farm, import_snapshot,
    remove_snapshot, restore_farm,
};
pub use template::BASE_IMAGE;
pub use template::{ALL_IN_ONE, NodeSpec, Role};
pub use terminal::{TerminalInput, TerminalIo, TerminalSize, attach as attach_terminal};

/// Create the farm's single all-in-one VM and confirm it boots.
pub async fn create_farm(farm_id: &str, template: &[NodeSpec]) -> Result<Vec<FarmNode>> {
    create_farm_with(farm_id, template, None, &[]).await
}

/// `components` selects which service ports the all-in-one guest publishes.
pub async fn create_farm_with(
    farm_id: &str,
    template: &[NodeSpec],
    exposure: Option<&crate::model::Exposure>,
    components: &[String],
) -> Result<Vec<FarmNode>> {
    let spec = single_node(template)?;
    if !farm_handles(farm_id).await?.is_empty() {
        bail!("farm '{farm_id}' already exists");
    }
    let node = create_node(farm_id, spec, exposure, &public_ports(components)).await?;
    Ok(vec![node])
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

fn single_node(template: &[NodeSpec]) -> Result<NodeSpec> {
    let [spec] = template else {
        bail!("new farms require exactly one all-in-one VM");
    };
    anyhow::ensure!(
        spec.role == Role::AllInOne,
        "legacy multi-VM layouts cannot be created; create a new all-in-one farm"
    );
    Ok(*spec)
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

/// Name of the farm's all-in-one VM, which predates the farm-id naming on older farms.
pub async fn all_in_one_name(farm_id: &str) -> Result<String> {
    farm_handles(farm_id)
        .await?
        .into_iter()
        .find(|handle| label(handle, node::ROLE_LABEL).as_deref() == Some(Role::AllInOne.as_str()))
        .map(|handle| handle.name().to_owned())
        .with_context(|| format!("farm '{farm_id}' has no all-in-one VM"))
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
    fn create_rejects_legacy_or_multi_node_layouts_before_touching_the_runtime() {
        let all = ALL_IN_ONE;
        assert_eq!(single_node(&[all]).unwrap().role, Role::AllInOne);
        assert!(single_node(&[]).is_err());
        assert!(single_node(&[all, all]).is_err());
        let legacy = NodeSpec {
            role: Role::Deployer,
            ..all
        };
        assert!(
            single_node(&[legacy])
                .unwrap_err()
                .to_string()
                .contains("legacy")
        );
    }
}
