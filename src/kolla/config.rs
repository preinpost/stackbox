use std::collections::BTreeMap;

use anyhow::{Context, Result, bail};

use crate::farm::{FarmNode, Role};

pub const OPENSTACK_RELEASE: &str = "2026.2";

#[derive(Clone, Debug)]
pub struct Host {
    pub name: String,
    pub role: Role,
    pub ip: String,
}

/// Running farm nodes split into the deployer and the kolla-ansible targets.
#[derive(Debug)]
pub struct Topology {
    pub deployer: String,
    pub targets: Vec<Host>,
}

impl Topology {
    pub fn from_nodes(nodes: &[FarmNode]) -> Result<Self> {
        let mut deployer = None;
        let mut targets = Vec::new();
        for node in nodes {
            let role = node
                .role
                .with_context(|| format!("{} has no role label", node.name))?;
            if node.status != "running" {
                bail!("{} is {}, expected running", node.name, node.status);
            }
            let ip = node
                .ip
                .clone()
                .with_context(|| format!("{} has no overlay address", node.name))?;
            match role {
                Role::Deployer => deployer = Some(node.name.clone()),
                _ => targets.push(Host {
                    name: node.name.clone(),
                    role,
                    ip,
                }),
            }
        }
        targets.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(Self {
            deployer: deployer.context("farm has no deployer node")?,
            targets,
        })
    }

    /// HAProxy runs on the lb node and owns its address, standing in for a keepalived VIP.
    pub fn vip(&self) -> Result<&str> {
        self.targets
            .iter()
            .find(|h| h.role == Role::Lb)
            .map(|h| h.ip.as_str())
            .context("farm has no lb node")
    }
}

/// Inventory groups each role takes over from the stock multinode inventory.
fn kolla_groups(role: Role) -> &'static [&'static str] {
    match role {
        Role::Deployer => &[],
        Role::Lb => &["network"],
        Role::Infra => &["control"],
        Role::Keystone => &["keystone"],
        Role::Glance => &["glance"],
    }
}

pub fn globals(vip: &str, arch: &str) -> String {
    let tag_suffix = if arch == "aarch64" { "-aarch64" } else { "" };
    format!(
        r#"---
kolla_base_distro: "rocky"
openstack_release: "{OPENSTACK_RELEASE}"
openstack_tag_suffix: "{tag_suffix}"
kolla_internal_vip_address: "{vip}"
network_interface: "vx0"

# quay.io/openstack.kolla images are flagged as test images by prechecks.
kolla_test_images: true
# msb keeps the guest clock in step with the host; there is no NTP daemon to detect.
prechecks_enable_host_ntp_checks: false

# A single lb node owns the VIP, so there is nothing for keepalived to fail over to.
enable_keepalived: false
enable_haproxy: true
enable_proxysql: false
enable_fluentd: false

enable_openstack_core: false
enable_mariadb: true
enable_memcached: true
enable_rabbitmq: true
enable_keystone: true
enable_glance: true
glance_backend_file: true
"#
    )
}

pub fn inventory(template: &str, topology: &Topology) -> String {
    let mut groups: BTreeMap<&str, Vec<&str>> =
        ["control", "network", "compute", "monitoring", "storage"]
            .into_iter()
            .map(|g| (g, Vec::new()))
            .collect();
    for host in &topology.targets {
        for group in ["baremetal", "common"]
            .iter()
            .chain(kolla_groups(host.role))
        {
            groups.entry(group).or_default().push(&host.name);
        }
    }

    let mut out = String::from("[farm_nodes]\n");
    for host in &topology.targets {
        out.push_str(&format!(
            "{} ansible_host={} ansible_user=root\n",
            host.name, host.ip
        ));
    }
    out.push('\n');
    out.push_str(&override_groups(template, &groups));
    out
}

/// Replace the body of each overridden group, turning `[group:children]` into a plain host list.
fn override_groups(template: &str, groups: &BTreeMap<&str, Vec<&str>>) -> String {
    let mut out = String::new();
    let mut seen = Vec::new();
    let mut skipping = false;
    for line in template.lines() {
        if let Some((name, kind)) = section(line) {
            skipping = false;
            if kind != Some("vars")
                && let Some(hosts) = groups.get(name)
            {
                push_group(&mut out, name, hosts);
                seen.push(name);
                skipping = true;
                continue;
            }
        }
        if !skipping {
            out.push_str(line);
            out.push('\n');
        }
    }
    for (name, hosts) in groups {
        if !seen.contains(name) {
            push_group(&mut out, name, hosts);
        }
    }
    out
}

fn push_group(out: &mut String, name: &str, hosts: &[&str]) {
    out.push_str(&format!("[{name}]\n"));
    for host in hosts {
        out.push_str(host);
        out.push('\n');
    }
    out.push('\n');
}

fn section(line: &str) -> Option<(&str, Option<&str>)> {
    let inner = line.trim().strip_prefix('[')?.strip_suffix(']')?;
    Some(match inner.split_once(':') {
        Some((name, kind)) => (name, Some(kind)),
        None => (inner, None),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEMPLATE: &str = "\
[control]
control01

[network]
network01

[deployment]
localhost       ansible_connection=local

[baremetal:children]
control
network

[keystone:children]
control

[glance:children]
control

[glance-api:children]
glance

[storage:vars]
foo=bar
";

    fn topology() -> Topology {
        let host = |name: &str, role, ip: &str| Host {
            name: name.into(),
            role,
            ip: ip.into(),
        };
        Topology {
            deployer: "demo-deployer".into(),
            targets: vec![
                host("demo-glance", Role::Glance, "172.16.0.14"),
                host("demo-infra", Role::Infra, "172.16.0.18"),
                host("demo-keystone", Role::Keystone, "172.16.0.6"),
                host("demo-lb", Role::Lb, "172.16.0.22"),
            ],
        }
    }

    fn group<'a>(inventory: &'a str, name: &str) -> Vec<&'a str> {
        let header = format!("[{name}]");
        inventory
            .lines()
            .skip_while(|l| *l != header)
            .skip(1)
            .take_while(|l| !l.is_empty())
            .collect()
    }

    #[test]
    fn maps_roles_onto_kolla_groups() {
        let inv = inventory(TEMPLATE, &topology());
        assert_eq!(group(&inv, "control"), ["demo-infra"]);
        assert_eq!(group(&inv, "network"), ["demo-lb"]);
        assert_eq!(group(&inv, "keystone"), ["demo-keystone"]);
        assert_eq!(group(&inv, "glance"), ["demo-glance"]);
        assert_eq!(
            group(&inv, "baremetal"),
            ["demo-glance", "demo-infra", "demo-keystone", "demo-lb"]
        );
        assert!(group(&inv, "storage").is_empty());
    }

    #[test]
    fn keeps_untouched_sections() {
        let inv = inventory(TEMPLATE, &topology());
        assert!(inv.contains("[glance-api:children]\nglance\n"));
        assert!(inv.contains("localhost       ansible_connection=local"));
        assert!(inv.contains("[storage:vars]\nfoo=bar\n"));
        assert!(!inv.contains("control01"));
        assert!(!inv.contains("[keystone:children]"));
    }

    #[test]
    fn defines_host_addresses_once() {
        let inv = inventory(TEMPLATE, &topology());
        assert_eq!(
            group(&inv, "farm_nodes")[0],
            "demo-glance ansible_host=172.16.0.14 ansible_user=root"
        );
        assert_eq!(inv.matches("ansible_host=").count(), 4);
    }

    #[test]
    fn globals_select_arch_suffix() {
        assert!(globals("172.16.0.22", "aarch64").contains("openstack_tag_suffix: \"-aarch64\""));
        assert!(globals("172.16.0.22", "x86_64").contains("openstack_tag_suffix: \"\""));
        assert!(
            globals("172.16.0.22", "x86_64")
                .contains("kolla_internal_vip_address: \"172.16.0.22\"")
        );
    }
}
