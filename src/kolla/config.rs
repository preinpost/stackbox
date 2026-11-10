use anyhow::{Context, Result, bail};

use crate::farm::{FarmNode, Role};

pub const OPENSTACK_RELEASE: &str = "2026.2";

/// A multi-node farm's compute VM, as Kolla reaches it over the management link.
#[derive(Clone, Debug, PartialEq)]
pub struct Compute {
    /// msb VM name.
    pub name: String,
    pub hostname: String,
    pub address: String,
}

/// The VM that runs Kolla and the control plane (all-in-one, or a multi-node farm's control
/// VM), and a multi-node farm's computes.
#[derive(Debug)]
pub struct Topology {
    pub name: String,
    pub ip: Option<String>,
    /// Interface holding `ip`: the fixed `farm0` service address, or eth0 on older farms.
    pub interface: String,
    pub computes: Vec<Compute>,
    /// A control VM and its computes, if any yet, rather than one all-in-one VM.
    pub multinode: bool,
}

impl Topology {
    pub fn from_nodes(nodes: &[FarmNode]) -> Result<Self> {
        if crate::farm::observed_layout(nodes).is_none() {
            bail!(
                "deployment requires one all-in-one VM, or a control VM and its compute VMs; legacy multi-VM farms cannot be deployed with this version (create a new farm)"
            );
        }
        if let Some(node) = nodes.iter().find(|node| node.status != "running") {
            bail!("{} is {}, expected running", node.name, node.status);
        }
        // A control VM without computes yet, restored from a control template, still deploys
        // as a multi-node farm.
        let multinode = nodes.iter().any(|node| node.role == Some(Role::Control));
        let deployer = if multinode {
            Role::Control
        } else {
            Role::AllInOne
        };
        let node = nodes
            .iter()
            .find(|node| node.role == Some(deployer))
            .context("no control VM")?;
        let mut computes: Vec<Compute> = nodes
            .iter()
            .filter(|node| node.role == Some(Role::Compute))
            .map(|node| Compute {
                name: node.name.clone(),
                hostname: crate::farm::hostname(Role::Compute, node.index),
                address: crate::farm::address(Role::Compute, node.index),
            })
            .collect();
        computes.sort_by(|a, b| a.hostname.cmp(&b.hostname));
        // SDK inventory does not expose a live DHCP address. Discover eth0 after
        // connecting rather than retaining an obsolete overlay or cached address.
        Ok(Self {
            name: node.name.clone(),
            ip: None,
            interface: "eth0".into(),
            computes,
            multinode,
        })
    }
    pub fn multinode(&self) -> bool {
        self.multinode
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

/// `multinode`: a control VM and computes joined by the farm's links, with tenant networks
/// as VLANs on each VM's tenant link.
pub fn globals(
    vip: &str,
    interface: &str,
    arch: &str,
    kvm: bool,
    components: &[String],
    multinode: bool,
    release: &crate::kolla::releases::ReleaseInfo,
) -> String {
    // Only the releases whose Rocky images are published for aarch64 carry the suffix; the
    // series the farm builds itself and the older maintained ones are amd64 only on Rocky.
    let tag_suffix = if arch == "aarch64" && release.arm64 {
        "-aarch64"
    } else {
        ""
    };
    // Kolla's images are tagged with the series name on the legacy releases and with the
    // year.release name from 2023.1 on.
    let image_tag = &release.series;
    let distro = release.base_distro();
    // The tag's default registry carries no end-of-life images. A series the farm builds and
    // publishes itself names its own registry and namespace; the image tag then follows its
    // base distro (`zed-rocky-9`) rather than the tag's default.
    let images = match &release.images {
        Some(images) => format!(
            "# Kolla images the farm built and published for this series.\ndocker_registry: \"{registry}\"\ndocker_namespace: \"{namespace}\"\n",
            registry = images.registry,
            namespace = images.namespace,
        ),
        None => String::new(),
    };
    let enabled = ["placement", "horizon", "neutron", "nova"]
        .iter()
        .map(|id| format!("enable_{id}: {}\n", components.iter().any(|c| c == id)))
        .collect::<String>();
    let links = if multinode {
        // Tenant networks are VLANs on each VM's `farm1` link (physnet1, br-tenant): VXLAN in
        // the userspace datapath would need the tunnel address on an OVS bridge. The bridges
        // are inventory host variables, which globals.yml (extra vars) would override.
        "tunnel_interface: \"farm0\"\nneutron_tenant_network_types: \"vlan\"\nenable_neutron_provider_networks: true\n"
    } else {
        ""
    };
    let external = if components.iter().any(|c| c == "neutron") && !multinode {
        "neutron_external_interface: \"farm-ext\"\n"
    } else {
        ""
    };
    let neutron = if components.iter().any(|c| c == "neutron") {
        // The guest kernel has no openvswitch datapath, so OVS forwards in userspace.
        // `farm-ext` is a veth the network node routes and masquerades for the external network.
        // The Neutron patch lives in the image's own Python: `python3.9` for the farm's Rocky 9
        // build, the release's minor on its other images.
        format!(
            concat!(
                "neutron_plugin_agent: \"openvswitch\"\n",
                "ovs_datapath: \"netdev\"\n",
                "openvswitch_vswitchd_extra_volumes: \"{{{{ default_extra_volumes + ['/etc/farm/ovs-extend-start:/usr/local/bin/kolla_openvswitch_extend_start:ro'] }}}}\"\n",
                // The kernel lacks xt_connmark; see neutron_sitecustomize.py.
                "neutron_l3_agent_extra_volumes: \"{{{{ neutron_extra_volumes + ['/etc/farm/neutron-sitecustomize.py:/var/lib/kolla/venv/lib64/python{python}/site-packages/sitecustomize.py:ro'] }}}}\"\n",
            ),
            python = release.image_python()
        )
    } else {
        String::new()
    };
    // Releases before 2025.1 predate this guest: their host-OS precheck lists Rocky 9 only,
    // their baremetal role imports `selinux` from a venv without the binding, and their Neutron
    // defaults load ip6_tables with modprobe, which the guest kernel has built in and no kmod.
    // The farm chooses the guest, and every other precheck still runs.
    let older_guest_globals = if release.release.as_str() < "2025.1" {
        "# This release's list of supported host distributions predates this guest.\nprechecks_enable_host_os_checks: false\n# This guest has no SELinux, and the venv's Python has no `selinux` binding; the role's\n# no-op would fail importing it.\nchange_selinux: false\n# The guest kernel has ip6_tables built in and no kmod, so `modprobe` is absent.\nneutron_modules_default: []\n"
    } else {
        ""
    };
    // Docker publishes docker-ce for the RHEL rebuilds under /linux/centos; the tags whose
    // baremetal role is in-tree build the repository URL from the distribution name (rocky),
    // where the package is missing. 2024.2 and newer read the Galaxy collection's default.
    let eol_docker = if release.eol {
        "# docker-ce is published under /linux/centos for RHEL rebuilds; /linux/rocky lacks it.\ndocker_yum_url: \"https://download.docker.com/linux/centos\"\n"
    } else {
        ""
    };
    // The Docker SDK role's build dependencies miss this guest's Rocky 10 twice. 2024.1's
    // collection (unmaintained/2024.1) names them only when it is handed a virtualenv, but its
    // pip step otherwise runs on kolla-ansible's own Python, where that list is skipped: the
    // dbus-python wheel stops on the missing `dbus-1` pkg-config entry. 2024.2's (6.3.0) names
    // `dbus-glib-devel` and `dbus-x11`, which Rocky 10 no longer ships, and needs no virtualenv
    // of its own. 2025.1 gates those two on Rocky 9 and a maintained branch reads the fixed
    // list, so both keep the role's own answer.
    let docker_sdk = match release.release.as_str() {
        "2024.1" => {
            "docker_sdk_packages_default:\n  - python3-setuptools\n  - python3-pip\n  - dbus-devel\n  - glib2-devel\n  - gcc\n  - python3-devel\n"
        }
        "2024.2" => {
            "docker_sdk_packages_dbus_deps_redhat:\n  - dbus-daemon\n  - dbus-devel\n  - gcc\n  - glib2-devel\n  - python3-devel\n"
        }
        _ => "",
    };
    // What an older series adds between the NTP line and the API comment; a maintained branch
    // writes an empty line there, as it deployed before. Each block already ends in a newline,
    // so the placeholder's own line separates them from the comment when they are present.
    let guest_overrides = format!("{older_guest_globals}{eol_docker}{docker_sdk}");
    // These series derive `enable_heat` from `enable_openstack_core`, which the farm turns off
    // below; naming the flag keeps a series whose published images leave heat out from pulling
    // one anyway. A maintained branch keeps the flag implicit, as it deployed before.
    let heat = if release.release.as_str() < "2025.1" {
        "enable_heat: false\n"
    } else {
        ""
    };
    let nova = if components.iter().any(|c| c == "nova") {
        if kvm {
            "nova_compute_virt_type: \"kvm\"\nnova_libvirt_cpu_mode: \"host-passthrough\"\nnova_console: \"none\"\n"
        } else {
            // Without /dev/kvm in the guest, fall back to qemu TCG without host-passthrough.
            "nova_compute_virt_type: \"qemu\"\nnova_libvirt_cpu_mode: \"none\"\nnova_console: \"none\"\n"
        }
    } else {
        ""
    };
    format!(
        r#"---
kolla_base_distro: "{distro}"
{images}openstack_release: "{image_tag}"
openstack_tag_suffix: "{tag_suffix}"
kolla_internal_vip_address: "{vip}"
network_interface: "{interface}"
api_interface: "{interface}"
database_address: "{vip}"
kolla_external_vip_address: "{vip}"
kolla_external_vip_interface: "{interface}"

# quay.io/openstack.kolla images are flagged as test images by prechecks.
kolla_test_images: true
# msb keeps the guest clock in step with the host; there is no NTP daemon to detect.
prechecks_enable_host_ntp_checks: false
{guest_overrides}
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
{heat}enable_mariadb: true
enable_memcached: true
enable_rabbitmq: true
enable_keystone: true
enable_glance: true
glance_backend_file: true
{enabled}{links}{external}{neutron}{nova}"#
    )
}

/// Neutron on a multi-node farm: tenant VLANs on physnet1 (every VM's `farm1` link, bridged
/// as br-tenant) and the external flat network on physnet2 (br-ex on the control VM).
pub const MULTINODE_ML2: &str = "[ml2_type_vlan]\nnetwork_vlan_ranges = physnet1:100:199\n[ml2_type_flat]\nflat_networks = physnet2\n";
/// The Open vSwitch agents of a multi-node farm run no tunnels. Kolla's default hybrid
/// iptables firewall needs physdev, which the guest kernel lacks; the OVS firewall uses the
/// userspace datapath's own conntrack.
pub const MULTINODE_OVS_AGENT: &str = "[agent]\ntunnel_types =\nl2_population = false\n[securitygroup]\nfirewall_driver = openvswitch\n";

/// Kolla's multinode inventory with the groups that place services filled in: the control
/// VM runs the control plane, network node and Kolla itself; Nova runs on the computes.
pub fn multinode_inventory(template: &str, control: &str, computes: &[Compute]) -> Result<String> {
    let compute_hosts: Vec<String> = computes
        .iter()
        .map(|c| {
            format!(
                "{} ansible_host={} neutron_external_interface=farm1 neutron_bridge_name=br-tenant",
                c.hostname, c.address
            )
        })
        .collect();
    let groups = [
        (
            "control",
            format!(
                "{control} ansible_connection=local neutron_external_interface=farm1,farm-ext neutron_bridge_name=br-tenant,br-ex"
            ),
        ),
        ("network", control.to_owned()),
        ("compute", compute_hosts.join("\n")),
        ("monitoring", control.to_owned()),
        ("storage", control.to_owned()),
    ];
    let mut out = String::new();
    let mut lines = template.lines();
    let mut filled = 0;
    while let Some(line) = lines.next() {
        out.push_str(line);
        out.push('\n');
        let Some((_, body)) = groups
            .iter()
            .find(|(group, _)| line == format!("[{group}]"))
        else {
            continue;
        };
        // The template's hosts run up to the group's first blank line.
        for skipped in lines.by_ref() {
            if skipped.trim().is_empty() {
                break;
            }
        }
        // A control template's copy has no compute host yet.
        if !body.is_empty() {
            out.push_str(body);
            out.push('\n');
        }
        out.push('\n');
        filled += 1;
    }
    if filled != groups.len() {
        bail!("unexpected Kolla multinode inventory; review the farm's inventory rewrite");
    }
    Ok(out)
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
            index: None,
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
    fn multi_node_farms_deploy_from_their_control_vm_to_numbered_computes() {
        let compute = |index| FarmNode {
            name: format!("demo-compute-{index}"),
            index: Some(index),
            ..node(Role::Compute)
        };
        let control = FarmNode {
            name: "demo-control".into(),
            ..node(Role::Control)
        };
        let topology = Topology::from_nodes(&[compute(2), control.clone(), compute(1)]).unwrap();
        assert_eq!(topology.name, "demo-control");
        assert!(topology.multinode());
        assert_eq!(
            topology.computes,
            [
                Compute {
                    name: "demo-compute-1".into(),
                    hostname: "farm-compute-1".into(),
                    address: "10.254.0.11".into(),
                },
                Compute {
                    name: "demo-compute-2".into(),
                    hostname: "farm-compute-2".into(),
                    address: "10.254.0.12".into(),
                },
            ]
        );
        let mut stopped = compute(2);
        stopped.status = "stopped".into();
        assert!(Topology::from_nodes(&[control.clone(), compute(1), stopped]).is_err());
        assert!(Topology::from_nodes(&[control.clone(), compute(2)]).is_err());
        // A control template's copy has no compute yet, and is still multi-node.
        let alone = Topology::from_nodes(std::slice::from_ref(&control)).unwrap();
        assert_eq!(alone.name, "demo-control");
        assert!(alone.multinode() && alone.computes.is_empty());

        let selected = ["keystone", "glance", "placement", "neutron", "nova"].map(String::from);
        let globals = globals(
            "10.254.0.10",
            "farm0",
            "aarch64",
            false,
            &selected,
            true,
            &crate::kolla::releases::info(OPENSTACK_RELEASE),
        );
        assert!(globals.contains("tunnel_interface: \"farm0\"\n"));
        assert!(globals.contains("neutron_tenant_network_types: \"vlan\"\n"));
        assert!(globals.contains("enable_neutron_provider_networks: true\n"));
        // Extra vars would override the inventory's per-host bridges.
        assert!(!globals.contains("neutron_external_interface"));
        let single = globals_single(&selected);
        assert!(single.contains("neutron_external_interface: \"farm-ext\"\n"));
        assert!(!single.contains("tunnel_interface"));
        assert!(MULTINODE_ML2.contains("network_vlan_ranges = physnet1:100:199"));
        assert!(MULTINODE_ML2.contains("flat_networks = physnet2"));
        assert!(MULTINODE_OVS_AGENT.contains("tunnel_types =\n"));

        // The head of Kolla's inventory/multinode, which the rewrite keys on.
        let template = "# These initial groups are the only groups required to be modified.\n[control]\n# These hostname must be resolvable from your deployment host\ncontrol01\ncontrol02\ncontrol03\n\n# The above can also be specified as follows:\n#control[01:03]     ansible_user=kolla\n\n[network]\nnetwork01\nnetwork02\n\n[compute]\ncompute01\n\n[monitoring]\nmonitoring01\n\n[storage]\nstorage01\n\n[deployment]\nlocalhost       ansible_connection=local\n\n[baremetal:children]\ncontrol\n";
        let inventory =
            multinode_inventory(template, "farm-control", &topology_computes()).unwrap();
        assert!(inventory.contains("[control]\nfarm-control ansible_connection=local neutron_external_interface=farm1,farm-ext neutron_bridge_name=br-tenant,br-ex\n\n"));
        assert!(inventory.contains("[network]\nfarm-control\n\n"));
        assert!(inventory.contains("[compute]\nfarm-compute-1 ansible_host=10.254.0.11 neutron_external_interface=farm1 neutron_bridge_name=br-tenant\n\n"));
        assert!(inventory.contains("[storage]\nfarm-control\n\n[deployment]\nlocalhost"));
        assert!(inventory.contains("#control[01:03]"));
        for gone in [
            "control01",
            "network01",
            "compute01",
            "monitoring01",
            "storage01",
        ] {
            assert!(!inventory.contains(&format!("\n{gone}\n")), "{gone}");
        }
        assert!(multinode_inventory("[control]\nx\n", "farm-control", &[]).is_err());
        let alone = multinode_inventory(template, "farm-control", &[]).unwrap();
        assert!(alone.contains("[compute]\n\n[monitoring]\nfarm-control\n"));
    }

    fn globals_single(selected: &[String]) -> String {
        globals(
            "10.254.0.2",
            "farm0",
            "aarch64",
            false,
            selected,
            false,
            &crate::kolla::releases::info(OPENSTACK_RELEASE),
        )
    }

    fn topology_computes() -> Vec<Compute> {
        vec![Compute {
            name: "demo-compute-1".into(),
            hostname: "farm-compute-1".into(),
            address: "10.254.0.11".into(),
        }]
    }

    #[test]
    fn direct_services_use_eth0_and_bounded_shared_resources() {
        let base = ["keystone", "glance"].map(String::from);
        let globals = globals(
            "192.168.127.2",
            "eth0",
            "aarch64",
            false,
            &base,
            false,
            &crate::kolla::releases::info(OPENSTACK_RELEASE),
        );
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
    fn fixed_identity_binds_every_service_to_the_dummy_interface() {
        let base = ["keystone", "glance"].map(String::from);
        let globals = globals(
            "10.254.0.2",
            "farm0",
            "aarch64",
            false,
            &base,
            false,
            &crate::kolla::releases::info(OPENSTACK_RELEASE),
        );
        for key in [
            "network_interface",
            "api_interface",
            "kolla_external_vip_interface",
        ] {
            assert!(globals.contains(&format!("{key}: \"farm0\"\n")), "{key}");
        }
        assert!(globals.contains("kolla_internal_vip_address: \"10.254.0.2\""));
        assert!(!globals.contains("eth0"));
    }

    #[test]
    fn enables_only_selected_optional_services_and_keeps_netdev_workarounds() {
        let selected = ["keystone", "glance", "horizon", "neutron"].map(String::from);
        let globals = globals(
            "192.168.127.2",
            "eth0",
            "x86_64",
            false,
            &selected,
            false,
            &crate::kolla::releases::info(OPENSTACK_RELEASE),
        );
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
        // The current branch runs on the guest as it is; only older series (2024.x and the
        // end-of-life tags) need their host checks and module loads relaxed.
        assert!(globals.matches("prechecks_enable_host_os_checks").count() == 0);
        for absent in [
            "prechecks_enable_host_os_checks",
            "docker_yum_url",
            "change_selinux",
            "enable_heat",
            "docker_sdk_",
        ] {
            assert!(!globals.contains(absent), "{absent}");
        }
        let legacy = self::globals(
            "192.168.127.2",
            "eth0",
            "x86_64",
            false,
            &selected,
            false,
            &crate::kolla::releases::info("2020.1"),
        );
        assert_eq!(
            legacy
                .matches("prechecks_enable_host_os_checks: false")
                .count(),
            1
        );
        assert_eq!(legacy.matches("docker_yum_url:").count(), 1);
        assert!(legacy.contains("docker_yum_url: \"https://download.docker.com/linux/centos\"\n"));
        // The tag's baremetal role would load the `selinux` module in a venv that has no
        // binding for it; the guest kernel has none either.
        assert_eq!(legacy.matches("change_selinux: false").count(), 1);
        // No kmod in the guest; the tags' Neutron role would call modprobe for ip6_tables.
        assert_eq!(legacy.matches("neutron_modules_default: []").count(), 1);
    }

    #[test]
    fn a_maintained_release_writes_the_globals_it_had() {
        // The end-of-life image work must not change the file a maintained release already
        // deployed with: only a series the farm builds itself names a registry. The fixture
        // is this function's output at the commit before that work, blank line and all, so an
        // equality failure means the shipped file changed rather than the fixture following
        // the code.
        let selected = [
            "keystone",
            "glance",
            "placement",
            "horizon",
            "neutron",
            "nova",
        ]
        .map(String::from);
        let text = globals(
            "10.254.0.2",
            "farm0",
            "x86_64",
            false,
            &selected,
            false,
            &crate::kolla::releases::info(OPENSTACK_RELEASE),
        );
        assert_eq!(
            text,
            include_str!("../../tests/fixtures/kolla/globals-2026.2.yml")
        );
        assert!(!text.contains("docker_registry"));
        assert!(!text.contains("docker_namespace"));
    }

    #[test]
    fn a_series_the_farm_builds_pulls_from_its_registry_on_its_base_distro() {
        let selected = [
            "keystone",
            "glance",
            "placement",
            "horizon",
            "neutron",
            "nova",
        ]
        .map(String::from);
        let zed = globals(
            "192.168.127.2",
            "eth0",
            "x86_64",
            false,
            &selected,
            false,
            &crate::kolla::releases::info("2022.2"),
        );
        // kolla-ansible 15.6.0 computes `zed-rocky-9` from these two values.
        assert!(zed.contains("kolla_base_distro: \"rocky\"\n"));
        assert!(zed.contains("docker_registry: \"ghcr.io\"\n"));
        assert!(zed.contains("docker_namespace: \"preinpost/kolla\"\n"));
        assert!(zed.contains("openstack_release: \"zed\"\n"));
        // The L3 patch goes into the Rocky 9 image's venv, not the Ubuntu one's.
        assert!(zed.contains("python3.9/site-packages/sitecustomize.py"));
        assert!(!zed.contains("python3.10/site-packages"));
        // The guest has no kmod; the tag's Neutron role would call modprobe for ip6_tables.
        assert_eq!(zed.matches("neutron_modules_default: []").count(), 1);
        // The series carries no heat image; `enable_openstack_core: false` alone would leave
        // the flag to kolla-ansible's indirection.
        assert_eq!(zed.matches("enable_heat: false").count(), 1);

        // 2024.1/2024.2 are supported, but quay no longer carries them: the farm builds
        // Rocky 9 images and kolla-ansible computes the tag `2024.x-rocky-9`.
        for release in ["2024.1", "2024.2"] {
            let text = globals(
                "192.168.127.2",
                "eth0",
                "x86_64",
                false,
                &selected,
                false,
                &crate::kolla::releases::info(release),
            );
            assert!(text.contains("kolla_base_distro: \"rocky\"\n"), "{release}");
            assert!(text.contains("docker_registry: \"ghcr.io\"\n"), "{release}");
            assert!(
                text.contains("docker_namespace: \"preinpost/kolla\"\n"),
                "{release}"
            );
            assert!(
                text.contains(&format!("openstack_release: \"{release}\"\n")),
                "{release}"
            );
            assert!(text.contains("openstack_tag_suffix: \"\"\n"), "{release}");
            // The L3 patch goes into the Rocky 9 image's venv, not the guest tooling's 3.12.
            assert!(
                text.contains("python3.9/site-packages/sitecustomize.py"),
                "{release}"
            );
            // It predates this guest (Rocky 10), so the host-OS, SELinux and module
            // overrides apply; the collection already names Docker's /linux/centos.
            assert_eq!(
                text.matches("prechecks_enable_host_os_checks: false")
                    .count(),
                1,
                "{release}"
            );
            assert_eq!(
                text.matches("change_selinux: false").count(),
                1,
                "{release}"
            );
            assert_eq!(
                text.matches("neutron_modules_default: []").count(),
                1,
                "{release}"
            );
            assert!(!text.contains("docker_yum_url"), "{release}");
            // Its published image set leaves heat out.
            assert_eq!(text.matches("enable_heat: false").count(), 1, "{release}");
            // Only 2024.1's collection skips dbus-python's build dependencies when it has no
            // virtualenv, and only 2024.2's names the two Rocky 10 dropped.
            let key = match release {
                "2024.1" => "docker_sdk_packages_default:",
                _ => "docker_sdk_packages_dbus_deps_redhat:",
            };
            assert_eq!(text.matches(key).count(), 1, "{release}");
            assert_eq!(
                text.matches("docker_sdk_").count(),
                1,
                "{release}: one override, not both"
            );
        }

        // A series the farm has not built keeps exactly what it pulled before: the tag's own
        // registry and its Ubuntu images.
        let yoga = globals(
            "192.168.127.2",
            "eth0",
            "x86_64",
            false,
            &selected,
            false,
            &crate::kolla::releases::info("2022.1"),
        );
        assert!(yoga.contains("kolla_base_distro: \"ubuntu\"\n"));
        assert!(yoga.contains("python3.10/site-packages/sitecustomize.py"));
        assert!(!yoga.contains("docker_registry"));
        assert!(!yoga.contains("docker_namespace"));
    }

    #[test]
    fn nova_uses_kvm_when_available_and_falls_back_to_qemu_tcg() {
        let selected = ["keystone", "glance", "nova"].map(String::from);
        let kvm_globals = globals(
            "192.168.127.2",
            "eth0",
            "aarch64",
            true,
            &selected,
            false,
            &crate::kolla::releases::info(OPENSTACK_RELEASE),
        );
        assert!(kvm_globals.contains("enable_nova: true\n"));
        assert!(kvm_globals.contains("nova_compute_virt_type: \"kvm\"\n"));
        assert!(kvm_globals.contains("nova_libvirt_cpu_mode: \"host-passthrough\"\n"));
        assert!(kvm_globals.contains("nova_console: \"none\"\n"));

        let tcg_globals = globals(
            "192.168.127.2",
            "eth0",
            "aarch64",
            false,
            &selected,
            false,
            &crate::kolla::releases::info(OPENSTACK_RELEASE),
        );
        assert!(tcg_globals.contains("enable_nova: true\n"));
        assert!(tcg_globals.contains("nova_compute_virt_type: \"qemu\"\n"));
        assert!(tcg_globals.contains("nova_libvirt_cpu_mode: \"none\"\n"));
        assert!(tcg_globals.contains("nova_console: \"none\"\n"));
    }
}
