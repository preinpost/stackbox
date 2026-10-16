mod node;
mod overlay;
mod template;

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Result, bail};
use futures::future::{join_all, try_join_all};
use microsandbox::sandbox::SandboxHandle;
use microsandbox::{Sandbox, SandboxListBuilder};

use node::{FARM_LABEL, OVERLAY_PORT_LABEL, create_node, describe_node, destroy_node, label};
pub use node::{FarmNode, connect, wait_until_booted};
pub use template::{DEFAULT_TEMPLATE, NodeSpec, Role};

/// Create the farm's VMs, then confirm every node reaches every other over the overlay.
/// A failed overlay check leaves the VMs in place for debugging.
pub async fn create_farm(farm_id: &str, template: &[NodeSpec]) -> Result<Vec<FarmNode>> {
    if !farm_handles(farm_id).await?.is_empty() {
        bail!("farm '{farm_id}' already exists");
    }
    let mesh = overlay::plan(template.len(), &used_overlay_ports().await?);
    let results = join_all(template.iter().zip(&mesh).map(|(spec, endpoint)| {
        create_node(farm_id, *spec, endpoint, overlay::conf(endpoint, &mesh))
    }))
    .await;
    let (nodes, errors): (Vec<_>, Vec<_>) = results.into_iter().partition(Result::is_ok);
    if !errors.is_empty() {
        delete_farm(farm_id).await?;
        let causes: Vec<String> = errors
            .into_iter()
            .map(|e| format!("  - {:#}", e.unwrap_err()))
            .collect();
        bail!("farm '{farm_id}' creation failed\n{}", causes.join("\n"));
    }
    let nodes: Vec<FarmNode> = nodes.into_iter().map(Result::unwrap).collect();
    if let Err(error) = overlay::check(&nodes).await {
        bail!("{error:#}\nthe VMs are left running; remove them with `farm delete {farm_id}`");
    }
    Ok(nodes)
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
    Ok(handles.len())
}

pub async fn list_farms() -> Result<BTreeMap<String, usize>> {
    let mut farms = BTreeMap::new();
    for handle in all_handles(|list| list).await? {
        if let Some(farm_id) = label(&handle, FARM_LABEL) {
            *farms.entry(farm_id).or_default() += 1;
        }
    }
    Ok(farms)
}

async fn used_overlay_ports() -> Result<BTreeSet<u16>> {
    Ok(all_handles(|list| list)
        .await?
        .iter()
        .filter_map(|handle| label(handle, OVERLAY_PORT_LABEL)?.parse().ok())
        .collect())
}

async fn farm_handles(farm_id: &str) -> Result<Vec<SandboxHandle>> {
    all_handles(|list| list.label(FARM_LABEL, farm_id)).await
}

async fn all_handles(
    filter: impl Fn(SandboxListBuilder) -> SandboxListBuilder,
) -> Result<Vec<SandboxHandle>> {
    let mut handles = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let page = Sandbox::list_with(|list| {
            let list = filter(list.limit(100));
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
