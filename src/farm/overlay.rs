//! msb routes every VM on its own /30 and does not relay traffic between VMs, so a
//! farm's nodes share an L2 VXLAN mesh. Each node publishes one host UDP port for
//! its VXLAN endpoint and reaches its peers through `host.microsandbox.internal`.
//! Every farm is its own mesh, so all farms reuse the same overlay subnet.

use std::collections::BTreeSet;

use anyhow::{Result, bail};
use futures::future::try_join_all;

use super::node::{FarmNode, connect, wait_until_booted};
use crate::shell::run;

pub const CONF_PATH: &str = "/etc/farm/overlay.conf";
pub const GUEST_PORT: u16 = 4789;
const FIRST_HOST_PORT: u16 = 24700;
const SUBNET: &str = "10.250.0";
const VNI: u32 = 100;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Endpoint {
    pub ip: String,
    pub host_port: u16,
}

/// Assign overlay addresses in template order and the lowest host ports no other farm uses.
pub fn plan(count: usize, used_ports: &BTreeSet<u16>) -> Vec<Endpoint> {
    (FIRST_HOST_PORT..=u16::MAX)
        .filter(|port| !used_ports.contains(port))
        .take(count)
        .enumerate()
        .map(|(i, host_port)| Endpoint {
            ip: format!("{SUBNET}.{}", i + 1),
            host_port,
        })
        .collect()
}

pub fn conf(me: &Endpoint, mesh: &[Endpoint]) -> String {
    let peers: Vec<String> = mesh
        .iter()
        .filter(|peer| *peer != me)
        .map(|peer| peer.host_port.to_string())
        .collect();
    format!(
        "VNI={VNI}\nADDRESS={}/24\nPEER_PORTS=\"{}\"\n",
        me.ip,
        peers.join(" ")
    )
}

/// Wait for every node to boot, then ping every peer over vx0.
pub async fn check(nodes: &[FarmNode]) -> Result<()> {
    let ips: Vec<&str> = nodes.iter().filter_map(|n| n.ip.as_deref()).collect();
    try_join_all(nodes.iter().map(|node| check_node(node, &ips))).await?;
    println!(
        "overlay: all {} node(s) reach each other over vx0",
        nodes.len()
    );
    Ok(())
}

async fn check_node(node: &FarmNode, ips: &[&str]) -> Result<()> {
    let sandbox = connect(&node.name).await?;
    wait_until_booted(&sandbox).await?;
    let peers: Vec<&str> = ips
        .iter()
        .copied()
        .filter(|ip| Some(*ip) != node.ip.as_deref())
        .collect();
    let script = format!(
        r#"systemctl is-active --quiet farm-overlay || {{ systemctl status farm-overlay --no-pager >&2; exit 1; }}
           failed=""
           for ip in {}; do ping -q -c3 -i0.3 -W2 "$ip" >/dev/null || failed="$failed $ip"; done
           [ -z "$failed" ] || {{ echo "unreachable:$failed" >&2; exit 1; }}"#,
        peers.join(" ")
    );
    let result = run(&sandbox, &script).await;
    sandbox.detach().await;
    if let Err(error) = result {
        bail!("overlay check failed on {}: {error:#}", node.name);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_skips_ports_of_other_farms() {
        let used = BTreeSet::from([24700, 24702]);
        let mesh = plan(3, &used);
        let ports: Vec<u16> = mesh.iter().map(|e| e.host_port).collect();
        let ips: Vec<&str> = mesh.iter().map(|e| e.ip.as_str()).collect();
        assert_eq!(ports, [24701, 24703, 24704]);
        assert_eq!(ips, ["10.250.0.1", "10.250.0.2", "10.250.0.3"]);
    }

    #[test]
    fn conf_lists_every_peer_but_self() {
        let mesh = plan(3, &BTreeSet::new());
        assert_eq!(
            conf(&mesh[1], &mesh),
            "VNI=100\nADDRESS=10.250.0.2/24\nPEER_PORTS=\"24700 24702\"\n"
        );
    }
}
