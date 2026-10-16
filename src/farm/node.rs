use anyhow::{Context, Result};
use microsandbox::sandbox::{DestroyOptions, SandboxHandle};
use microsandbox::{NetworkPolicy, NetworkProfile, Sandbox};

use super::overlay::{self, Endpoint};
use super::template::{BASE_IMAGE, NodeSpec, Role};
use crate::shell::run;

pub const FARM_LABEL: &str = "farm";
pub const ROLE_LABEL: &str = "farm.role";
pub const OVERLAY_IP_LABEL: &str = "farm.overlay.ip";
pub const OVERLAY_PORT_LABEL: &str = "farm.overlay.port";

// msb's default root is an overlay; Docker's overlayfs cannot nest on it,
// so container storage gets its own ext4 disks.
const CONTAINER_DIRS: &[&str] = &["/var/lib/docker", "/var/lib/containerd"];

#[derive(Debug)]
pub struct FarmNode {
    pub name: String,
    pub role: Option<Role>,
    pub status: String,
    /// Address on the farm's vx0 overlay.
    pub ip: Option<String>,
}

pub fn node_name(farm_id: &str, role: Role) -> String {
    format!("{farm_id}-{role}")
}

pub fn label(handle: &SandboxHandle, key: &str) -> Option<String> {
    handle.config().ok()?.spec.labels.get(key).cloned()
}

pub async fn create_node(
    farm_id: &str,
    spec: NodeSpec,
    endpoint: &Endpoint,
    overlay_conf: String,
) -> Result<FarmNode> {
    let name = node_name(farm_id, spec.role);
    let mut builder = Sandbox::builder(&name)
        .image(BASE_IMAGE)
        .cpus(spec.cpus)
        .memory(spec.memory_mib)
        .root_disk(spec.disk_mib)
        .init("/sbin/init")
        .hostname(&name)
        // `host` lets vx0 reach peers' published VXLAN ports through the host alias.
        .network(|n| {
            n.policy(NetworkPolicy::from_profiles([
                NetworkProfile::Public,
                NetworkProfile::Host,
            ]))
            .port_udp(endpoint.host_port, overlay::GUEST_PORT)
        })
        .patch(|p| p.text(overlay::CONF_PATH, overlay_conf, None, true))
        .label(FARM_LABEL, farm_id)
        .label(ROLE_LABEL, spec.role.as_str())
        .label(OVERLAY_IP_LABEL, endpoint.ip.as_str())
        .label(OVERLAY_PORT_LABEL, endpoint.host_port.to_string())
        .detached(true);
    if let Some(size) = spec.container_disk_mib {
        for dir in CONTAINER_DIRS {
            builder = builder.volume(*dir, |m| m.owned_with(|v| v.disk().size(size)));
        }
    }
    let sandbox = builder
        .create()
        .await
        .with_context(|| format!("create {name}"))?;
    sandbox.detach().await;
    Ok(FarmNode {
        name,
        role: Some(spec.role),
        status: "running".into(),
        ip: Some(endpoint.ip.clone()),
    })
}

pub fn describe_node(handle: &SandboxHandle) -> FarmNode {
    FarmNode {
        name: handle.name().to_owned(),
        role: label(handle, ROLE_LABEL).and_then(|r| r.parse().ok()),
        status: format!("{:?}", handle.status_snapshot()).to_lowercase(),
        ip: label(handle, OVERLAY_IP_LABEL),
    }
}

pub async fn destroy_node(handle: &SandboxHandle) -> Result<()> {
    handle
        .destroy_with(DestroyOptions {
            force: true,
            ..Default::default()
        })
        .await
        .with_context(|| format!("destroy {}", handle.name()))
}

pub async fn connect(name: &str) -> Result<Sandbox> {
    let handle = Sandbox::get(name)
        .await
        .with_context(|| format!("find {name}"))?;
    handle
        .connect()
        .await
        .with_context(|| format!("connect {name}"))
}

pub async fn wait_until_booted(sandbox: &Sandbox) -> Result<()> {
    // Right after boot the system bus may not exist yet, so retry until systemd answers.
    let state = run(
        sandbox,
        r#"for _ in $(seq 120); do
             state=$(systemctl is-system-running --wait 2>/dev/null)
             case "$state" in running|degraded) echo "$state"; exit 0;; esac
             sleep 1
           done
           echo "systemd not ready: $state" >&2; exit 1"#,
    )
    .await?;
    println!("{}: systemd {}", sandbox.name(), state.trim());
    Ok(())
}
