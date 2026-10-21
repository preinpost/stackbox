mod node;
mod template;

use std::collections::BTreeMap;

use anyhow::{Result, bail};
use futures::future::try_join_all;
use microsandbox::Sandbox;
use microsandbox::sandbox::SandboxHandle;

use node::{FARM_LABEL, create_node, describe_node, destroy_node, label};
pub use node::{FarmNode, connect, wait_until_booted};
pub use template::BASE_IMAGE;
pub use template::{ALL_IN_ONE, NodeSpec, Role};

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
    let ports: Vec<(u16, u16)> = crate::model::COMPONENTS
        .iter()
        .filter(|c| components.iter().any(|id| id == c.id))
        .filter_map(|c| c.public.as_ref().map(|p| (p.offset, p.guest_port)))
        .collect();
    if !farm_handles(farm_id).await?.is_empty() {
        bail!("farm '{farm_id}' already exists");
    }
    let node = create_node(farm_id, spec, exposure, &ports).await?;
    Ok(vec![node])
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
        if let Some(farm_id) = label(&handle, FARM_LABEL) {
            *farms.entry(farm_id).or_default() += 1;
        }
    }
    Ok(farms)
}

pub async fn scan_farms() -> Result<BTreeMap<String, Vec<FarmNode>>> {
    let mut farms: BTreeMap<String, Vec<FarmNode>> = BTreeMap::new();
    for handle in all_handles().await? {
        if let Some(id) = label(&handle, FARM_LABEL) {
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
        .filter(|handle| label(handle, FARM_LABEL).as_deref() == Some(farm_id))
        .collect())
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
