use anyhow::{Context, Result, bail};

use crate::farm::{FarmNode, Role};

pub const OPENSTACK_RELEASE: &str = "2026.2";

/// A farm has one host, which also runs the local Ansible controller.
#[derive(Debug)]
pub struct Topology {
    pub name: String,
    pub ip: Option<String>,
}

impl Topology {
    pub fn from_nodes(nodes: &[FarmNode]) -> Result<Self> {
        if nodes.len() != 1 || nodes.first().and_then(|node| node.role) != Some(Role::AllInOne) {
            bail!(
                "deployment requires one all-in-one VM; legacy multi-VM farms cannot be deployed with this version (create a new farm)"
            );
        }
        let node = &nodes[0];
        if node.status != "running" {
            bail!("{} is {}, expected running", node.name, node.status);
        }
        // SDK inventory does not expose a live DHCP address. Discover eth0 after
        // connecting rather than retaining an obsolete overlay or cached address.
        Ok(Self {
            name: node.name.clone(),
            ip: None,
        })
    }

    pub fn set_address(&mut self, address: &str) -> Result<()> {
        let address = address
            .trim()
            .parse::<std::net::Ipv4Addr>()
            .context("all-in-one VM has no valid eth0 IPv4 address")?;
        if address.is_unspecified() || address.is_loopback() {
            bail!("all-in-one eth0 must have a non-loopback IPv4 address");
        }
        self.ip = Some(address.to_string());
        Ok(())
    }

    pub fn vip(&self) -> Result<&str> {
        self.ip
            .as_deref()
            .context("all-in-one eth0 address has not been discovered")
    }
}

pub fn globals(vip: &str, arch: &str, kvm: bool, components: &[String]) -> String {
    let tag_suffix = if arch == "aarch64" { "-aarch64" } else { "" };
    format!(
        r#"---
kolla_base_distro: "rocky"
openstack_release: "{OPENSTACK_RELEASE}"
openstack_tag_suffix: "{tag_suffix}"
kolla_internal_vip_address: "{vip}"
network_interface: "eth0"
api_interface: "eth0"
database_address: "{vip}"
kolla_external_vip_address: "{vip}"
kolla_external_vip_interface: "eth0"

# quay.io/openstack.kolla images are flagged as test images by prechecks.
kolla_test_images: true
# msb keeps the guest clock in step with the host; there is no NTP daemon to detect.
prechecks_enable_host_ntp_checks: false

# The API services bind directly to this VM; SDK port mappings expose them.
enable_keepalived: false
enable_haproxy: false
enable_proxysql: false
enable_fluentd: false

# Bound process and cache counts for the compact all-in-one memory budget.
openstack_service_workers: 1
openstack_service_rpc_workers: 1
memcached_max_memory: 64
rabbitmq_server_additional_erl_args: "+S 1:1 +sbwt none +sbwtdcpu none +sbwtdio none"

enable_openstack_core: false
enable_mariadb: true
enable_memcached: true
enable_rabbitmq: true
enable_keystone: true
enable_glance: true
glance_backend_file: true
{}{}{}"#,
        ["placement", "horizon", "neutron", "nova"]
            .iter()
            .map(|id| format!("enable_{id}: {}\n", components.iter().any(|c| c == id)))
            .collect::<String>(),
        if components.iter().any(|c| c == "neutron") {
            // The guest kernel has no openvswitch datapath, so OVS forwards in userspace.
            // `farm-ext` is a veth the compute node routes and masquerades for the external network.
            concat!(
                "neutron_plugin_agent: \"openvswitch\"\n",
                "ovs_datapath: \"netdev\"\n",
                "neutron_external_interface: \"farm-ext\"\n",
                "openvswitch_vswitchd_extra_volumes: \"{{ default_extra_volumes + ['/etc/farm/ovs-extend-start:/usr/local/bin/kolla_openvswitch_extend_start:ro'] }}\"\n",
                // The kernel lacks xt_connmark; see neutron_sitecustomize.py.
                "neutron_l3_agent_extra_volumes: \"{{ neutron_extra_volumes + ['/etc/farm/neutron-sitecustomize.py:/var/lib/kolla/venv/lib64/python3.12/site-packages/sitecustomize.py:ro'] }}\"\n",
            )
        } else {
            ""
        },
        if components.iter().any(|c| c == "nova") {
            if kvm {
                "nova_compute_virt_type: \"kvm\"\nnova_libvirt_cpu_mode: \"host-passthrough\"\nnova_console: \"none\"\n"
            } else {
                // Without /dev/kvm in the guest, fall back to qemu TCG without host-passthrough.
                "nova_compute_virt_type: \"qemu\"\nnova_libvirt_cpu_mode: \"none\"\nnova_console: \"none\"\n"
            }
        } else {
            ""
        }
    )
}

/// Kolla merges this override after its dynamic 40%-of-host-RAM default.
pub const GALERA_CONFIG: &str = "[mysqld]\ninnodb_buffer_pool_size = 512M\nmax_connections = 256\nkey_buffer_size = 16M\nmax_heap_table_size = 16M\ntmp_table_size = 16M\nwsrep_slave_threads = 1\n";

#[cfg(test)]
mod tests {
    use super::*;

    fn node(role: Role) -> FarmNode {
        FarmNode {
            name: format!("demo-{role}"),
            role: Some(role),
            status: "running".into(),
            memory_mib: 3072,
            ip: None,
        }
    }

    #[test]
    fn requires_one_running_all_in_one_and_discovers_address_live() {
        let mut topology = Topology::from_nodes(&[node(Role::AllInOne)]).unwrap();
        assert!(topology.vip().is_err());
        topology.set_address("192.168.127.2\n").unwrap();
        assert_eq!(topology.vip().unwrap(), "192.168.127.2");
        assert!(topology.set_address("127.0.0.1").is_err());
        assert!(topology.set_address("not-an-address").is_err());
        assert!(Topology::from_nodes(&[]).is_err());
        assert!(
            Topology::from_nodes(&[node(Role::Deployer)])
                .unwrap_err()
                .to_string()
                .contains("legacy multi-VM")
        );
        assert!(Topology::from_nodes(&[node(Role::AllInOne), node(Role::AllInOne)]).is_err());
        let mut stopped = node(Role::AllInOne);
        stopped.status = "stopped".into();
        assert!(Topology::from_nodes(&[stopped]).is_err());
    }

    #[test]
    fn direct_services_use_eth0_and_bounded_shared_resources() {
        let base = ["keystone", "glance"].map(String::from);
        let globals = globals("192.168.127.2", "aarch64", false, &base);
        assert!(globals.contains("openstack_tag_suffix: \"-aarch64\""));
        assert!(globals.contains("network_interface: \"eth0\""));
        assert!(globals.contains("database_address: \"192.168.127.2\""));
        assert!(globals.contains("enable_haproxy: false"));
        assert!(globals.contains("enable_keepalived: false"));
        assert!(globals.contains("enable_proxysql: false"));
        assert!(globals.contains("openstack_service_workers: 1"));
        assert!(GALERA_CONFIG.contains("innodb_buffer_pool_size = 512M"));
        assert!(!globals.contains("vx0"));
    }

    #[test]
    fn enables_only_selected_optional_services_and_keeps_netdev_workarounds() {
        let selected = ["keystone", "glance", "horizon", "neutron"].map(String::from);
        let globals = globals("192.168.127.2", "x86_64", false, &selected);
        for id in ["horizon", "neutron"] {
            assert!(globals.contains(&format!("enable_{id}: true\n")));
        }
        for id in ["placement", "nova"] {
            assert!(globals.contains(&format!("enable_{id}: false\n")));
        }
        assert!(globals.contains("ovs_datapath: \"netdev\""));
        assert!(globals.contains("neutron-sitecustomize.py"));
        assert!(globals.contains("ovs-extend-start"));
        assert!(!globals.contains("nova_compute_virt_type"));
    }

    #[test]
    fn nova_uses_kvm_when_available_and_falls_back_to_qemu_tcg() {
        let selected = ["keystone", "glance", "nova"].map(String::from);
        let kvm_globals = globals("192.168.127.2", "aarch64", true, &selected);
        assert!(kvm_globals.contains("enable_nova: true\n"));
        assert!(kvm_globals.contains("nova_compute_virt_type: \"kvm\"\n"));
        assert!(kvm_globals.contains("nova_libvirt_cpu_mode: \"host-passthrough\"\n"));
        assert!(kvm_globals.contains("nova_console: \"none\"\n"));

        let tcg_globals = globals("192.168.127.2", "aarch64", false, &selected);
        assert!(tcg_globals.contains("enable_nova: true\n"));
        assert!(tcg_globals.contains("nova_compute_virt_type: \"qemu\"\n"));
        assert!(tcg_globals.contains("nova_libvirt_cpu_mode: \"none\"\n"));
        assert!(tcg_globals.contains("nova_console: \"none\"\n"));
    }
}
