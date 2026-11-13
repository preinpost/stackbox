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
#[allow(clippy::too_many_arguments)]
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
    // and their baremetal role imports `selinux` from a venv without the binding. The farm
    // chooses the guest, and every other precheck still runs.
    let older_guest_globals = if release.release.as_str() < "2025.1" {
        "# This release's list of supported host distributions predates this guest.\nprechecks_enable_host_os_checks: false\n# This guest has no SELinux, and the venv's Python has no `selinux` binding; the role's\n# no-op would fail importing it.\nchange_selinux: false\n"
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
# The guest kernel has no module loading (/proc/modules is absent) and ip6_tables is built
# in. 2025.1 and 2025.2 would otherwise `modprobe` Neutron's default modules and fail the
# deploy's last play; every other service that loads one is off or patched.
neutron_modules_default: []
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

/// The mediated-device types nova-compute may bind, in the Kolla file merged into
/// nova-compute's `nova.conf`. `mtty_mtty` is the kernel's virtual parent device, which
/// Nova accepts in place of a PCI address.
pub const MDEV_NOVA_COMPUTE_CONF: &str =
    "[devices]\nenabled_mdev_types = mtty-1\n\n[mdev_mtty-1]\ndevice_addresses = mtty_mtty\n";

/// The nova-compute.conf of every farm. Without `resume_guests_state_on_host_boot`, Nova
/// leaves each instance SHUTOFF when its compute VM comes back from a snapshot.
pub const NOVA_COMPUTE_BASE_CONF: &str = "[DEFAULT]\nresume_guests_state_on_host_boot = True\n";

/// The whole nova-compute.conf: the base section, then the mtty sections when `mdev`.
pub fn nova_compute_conf(mdev: bool) -> String {
    if mdev {
        format!("{NOVA_COMPUTE_BASE_CONF}\n{MDEV_NOVA_COMPUTE_CONF}")
    } else {
        NOVA_COMPUTE_BASE_CONF.to_owned()
    }
}

/// Where each host keeps the Nova files farm's mtty backport mounts into nova_compute, as
/// `<release>/<module>` (see [`NOVA_MTTY_MODULES`]).
pub const NOVA_MTTY_DIR: &str = "/etc/farm/nova-mtty";

/// Where a host keeps the script that builds those files from the image.
pub const NOVA_MTTY_PREPARE: &str = "/etc/farm/prepare-nova-mtty.sh";

/// The two Nova modules the backport changes, relative to the Nova package. Each one is
/// mounted over the image's own copy.
pub const NOVA_MTTY_MODULES: [&str; 2] =
    ["nova/virt/libvirt/config.py", "nova/virt/libvirt/driver.py"];

/// farm's backport of Nova's `mtty` support for a release whose Nova has none: upstream
/// merged it on 2026.2 only, so the two older maintained series and 2026.1 get the patch
/// file for their own source. `None` means the release needs no patch.
///
/// Each file carries what it changes from the upstream commit (Python version, the privsep
/// branch 2026.2 removed) and is applied by [`nova_mtty_prepare_script`], which fails rather
/// than skipping a hunk that no longer matches.
pub fn nova_mtty_patch(release: &str) -> Option<&'static str> {
    match release {
        "2025.1" => Some(include_str!("patches/nova-mtty-2025.1.diff")),
        "2025.2" => Some(include_str!("patches/nova-mtty-2025.2.diff")),
        "2026.1" => Some(include_str!("patches/nova-mtty-2026.1.diff")),
        _ => None,
    }
}

/// The nova-compute image that carries the Nova the backport patches. The host builds the
/// patched files from this image, so the tag is the one Kolla deploys for the release.
pub fn nova_mtty_image(release: &str) -> Option<&'static str> {
    match release {
        "2025.1" => Some("quay.io/openstack.kolla/nova-compute:2025.1-rocky-9"),
        "2025.2" => Some("quay.io/openstack.kolla/nova-compute:2025.2-rocky-10"),
        "2026.1" => Some("quay.io/openstack.kolla/nova-compute:2026.1-rocky-10"),
        _ => None,
    }
}

/// Nova's package directory in that image. The venv's `lib64` links to `lib`, so the mounts
/// name the real directory.
pub fn nova_mtty_site(release: &str) -> Option<&'static str> {
    match release {
        "2025.1" => Some("/var/lib/kolla/venv/lib/python3.9/site-packages"),
        "2025.2" | "2026.1" => Some("/var/lib/kolla/venv/lib/python3.12/site-packages"),
        _ => None,
    }
}

/// Each backported module's copy on the host and its path in the container, for `release`.
pub fn nova_mtty_mounts(release: &str) -> Option<Vec<(String, String)>> {
    let site = nova_mtty_site(release)?;
    Some(
        NOVA_MTTY_MODULES
            .iter()
            .map(|module| {
                (
                    format!("{NOVA_MTTY_DIR}/{release}/{module}"),
                    format!("{site}/{module}"),
                )
            })
            .collect(),
    )
}

/// The `globals.yml` entry that mounts those copies read-only over nova_compute's modules.
///
/// It extends Kolla's list instead of replacing it: every release farm supports defaults
/// `nova_compute_extra_volumes` to `{{ nova_extra_volumes }}`, which is itself
/// `{{ default_extra_volumes }}` (`ansible/roles/nova-cell/defaults/main.yml`), so naming
/// `default_extra_volumes` keeps whatever Kolla or an operator put there.
pub fn nova_mtty_extra_volumes(release: &str) -> Option<String> {
    let mounts: Vec<String> = nova_mtty_mounts(release)?
        .into_iter()
        .map(|(host, container)| format!("'{host}:{container}:ro'"))
        .collect();
    Some(format!(
        "nova_compute_extra_volumes: \"{{{{ default_extra_volumes + [{}] }}}}\"\n",
        mounts.join(", ")
    ))
}

/// The script a host runs as root to build the Nova files of [`nova_mtty_mounts`]. The image's
/// tree is root-owned and nova_compute runs as `nova`, so the patch is applied by a root
/// container to copies on the host, before nova_compute exists. A hunk that does not match
/// stops the script before any host file changes.
pub fn nova_mtty_prepare_script(release: &str) -> Option<String> {
    let patch = nova_mtty_patch(release)?;
    let image = nova_mtty_image(release)?;
    let site = nova_mtty_site(release)?;
    let modules = NOVA_MTTY_MODULES.join(" ");
    Some(format!(
        r#"#!/bin/bash
# Written by farm: builds the Nova files nova_compute mounts for farm's mtty backport.
set -o errexit
set -o pipefail
set -o nounset

image='{image}'
out='{NOVA_MTTY_DIR}/{release}'
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
cat > "$work/apply.py" <<'FARM_APPLY_PY'
{apply}FARM_APPLY_PY
cat > "$work/nova-mtty.patch" <<'FARM_PATCH'
{patch}FARM_PATCH
docker pull "$image" >/dev/null
docker run --rm --user root --entrypoint bash -v "$work:/farm" "$image" -c '
set -o errexit
site=$(python3 -c "import os, nova; print(os.path.dirname(os.path.realpath(nova.__path__[0])))")
[ "$site" = "$1" ] || {{ echo "nova is in $site, not $1" >&2; exit 1; }}
mkdir -p /farm/src/nova/virt/libvirt
cp "$site/nova/virt/libvirt/config.py" "$site/nova/virt/libvirt/driver.py" /farm/src/nova/virt/libvirt/
python3 /farm/apply.py /farm/nova-mtty.patch /farm/src
' farm '{site}' < /dev/null
mkdir -p "$out/nova/virt/libvirt"
for module in {modules}; do
  install -m 0644 "$work/src/$module" "$out/$module.new"
  mv -f "$out/$module.new" "$out/$module"
done
echo "nova-mtty files for {release} in $out"
"#,
        apply = include_str!("patches/apply.py"),
    ))
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
        // end-of-life tags) need their host checks relaxed. Neutron's module default is
        // emptied for every release: this kernel cannot load modules at all.
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
        // No kmod in the guest, and Neutron's default is ip6_tables before 2026.1: farm
        // empties it for every release, since this kernel cannot load modules at all.
        assert_eq!(legacy.matches("neutron_modules_default: []").count(), 1);
        for release in ["2024.1", "2025.1", "2025.2", "2026.1", OPENSTACK_RELEASE] {
            let text = self::globals(
                "192.168.127.2",
                "eth0",
                "x86_64",
                false,
                &selected,
                false,
                &crate::kolla::releases::info(release),
            );
            assert_eq!(
                text.matches("neutron_modules_default: []").count(),
                1,
                "{release}"
            );
        }
    }

    #[test]
    fn a_maintained_release_writes_the_globals_it_had() {
        // The end-of-life image work must not change the file a maintained release already
        // deployed with: only a series the farm builds itself names a registry. The fixture
        // is this function's output at the commit before that work plus the Neutron module
        // default farm now writes for every release, blank line and all, so an equality
        // failure means the shipped file changed rather than the fixture following the code.
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
        // The guest has no kmod, and Neutron's default is ip6_tables until 2026.1; farm
        // empties it for every release, since this kernel cannot load modules at all.
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
            // It predates this guest (Rocky 10), so the host-OS and SELinux overrides apply;
            // the collection already names Docker's /linux/centos.
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
    #[test]
    fn a_gpu_takes_no_kolla_globals() {
        // farm writes a simulated GPU's providers through Placement, and reads the mediated
        // device's from Nova: neither is a globals entry. Kolla's provider-config file only
        // takes CUSTOM_* classes, which is exactly what the standard VGPU contract is not.
        let selected = ["keystone", "glance", "placement", "neutron", "nova"].map(String::from);
        let text = globals(
            "10.254.0.10",
            "farm0",
            "x86_64",
            false,
            &selected,
            true,
            &crate::kolla::releases::info(OPENSTACK_RELEASE),
        );
        assert!(!text.contains("provider_config"), "{text}");
        assert!(!text.contains("enabled_mdev_types"), "{text}");
    }

    #[test]
    fn the_mdev_config_names_the_kernel_type_and_its_parent() {
        // Copied from the PoC: nova merges /etc/kolla/config/nova/nova-compute.conf into
        // nova-compute's nova.conf, and the virtual parent device needs no PCI address.
        assert_eq!(
            MDEV_NOVA_COMPUTE_CONF,
            "[devices]\nenabled_mdev_types = mtty-1\n\n[mdev_mtty-1]\ndevice_addresses = mtty_mtty\n"
        );
    }

    #[test]
    fn nova_compute_resumes_its_guests_on_host_boot_with_or_without_mtty() {
        let plain = nova_compute_conf(false);
        assert_eq!(
            plain,
            "[DEFAULT]\nresume_guests_state_on_host_boot = True\n"
        );
        let mdev = nova_compute_conf(true);
        assert!(
            mdev.starts_with("[DEFAULT]\nresume_guests_state_on_host_boot = True\n\n"),
            "{mdev}"
        );
        assert!(mdev.contains(MDEV_NOVA_COMPUTE_CONF), "{mdev}");
        assert_eq!(mdev.matches("[DEFAULT]").count(), 1, "{mdev}");
        assert_eq!(mdev.matches("[devices]").count(), 1, "{mdev}");
    }

    #[test]
    fn the_mtty_backport_covers_the_releases_that_lack_it() {
        // Upstream merged `mtty` support on 2026.2 and nothing else; farm carries a patch
        // for every older maintained series and lets a newer one use Nova's own code.
        for release in ["2025.1", "2025.2", "2026.1"] {
            let patch = nova_mtty_patch(release).unwrap_or_else(|| panic!("{release}"));
            // Provenance travels with the patch: a public file that copies Apache-2.0 code
            // names the upstream commit and its license.
            assert!(
                patch.contains("f412fac1ae1e78e91b8b7c2c4b0cf2be4f63ceb2"),
                "{release}"
            );
            assert!(patch.contains("Apache License 2.0"), "{release}");
            assert!(
                patch.contains("--- a/nova/virt/libvirt/driver.py"),
                "{release}"
            );
            assert!(
                patch.contains("--- a/nova/virt/libvirt/config.py"),
                "{release}"
            );
            // Only the two files the running service needs; tests do not ship in the image.
            assert!(!patch.contains("tests/unit"), "{release}");
            assert_eq!(patch.matches("--- a/nova/").count(), 2, "{release}");
        }
        for release in ["2026.2", "2024.2", "2024.1", "2022.2"] {
            assert!(nova_mtty_patch(release).is_none(), "{release}");
        }
        // 2025.1 and 2025.2 support Python 3.9, where `str | None` is a runtime type error
        // in an annotation; their patches use the file's own `ty.Optional` spelling. The
        // headers name the upstream spelling, so only hunk lines are checked.
        for release in ["2025.1", "2025.2"] {
            let patch = nova_mtty_patch(release).unwrap();
            assert!(patch.contains(") -> ty.Optional[str]:"), "{release}");
            assert!(
                !patch.lines().any(|line| {
                    (line.starts_with('+') || line.starts_with('-')) && line.contains("str | None")
                }),
                "{release}"
            );
        }
        assert!(
            nova_mtty_patch("2026.1")
                .unwrap()
                .contains(") -> str | None:")
        );
    }

    /// The offline half of the same check: rebuild each patched file from its hunks' own
    /// pre-patch context and let apply.py's matcher place them. An anchor the reconstruction
    /// presents twice (or a hunk another hunk covers) makes the container's placement a guess,
    /// and this fails here instead of on an image built from a drifted Nova.
    #[test]
    fn the_mtty_backport_hunks_match_their_own_context_uniquely() {
        let dir = tempfile::tempdir().unwrap();
        let applier = dir.path().join("apply.py");
        std::fs::write(&applier, include_str!("patches/apply.py")).unwrap();
        let driver = dir.path().join("check.py");
        std::fs::write(
            &driver,
            r#"import importlib.util
import sys

spec = importlib.util.spec_from_file_location("farm_apply", sys.argv[1])
applier = importlib.util.module_from_spec(spec)
spec.loader.exec_module(applier)
with open(sys.argv[2]) as handle:
    hunks = applier.parse(handle.readlines())

# All the patcher can match against is its own context: join each file's hunks with a
# blank line so a block cannot be completed by its neighbour.
reconstructed = {path: "\n".join(old for old, _ in blocks) for path, blocks in hunks.items()}
for path in sorted(hunks):
    text = reconstructed[path]
    for number, (old, new) in enumerate(hunks[path], 1):
        text, state = applier.apply_hunk(text, old, new, path, number)
        assert state == "applied", (path, number, state)
    # A second start of the container finds the new text, as `--check` does.
    for number, (old, new) in enumerate(hunks[path], 1):
        text, state = applier.apply_hunk(text, old, new, path, number)
        assert state == "already", (path, number, state)
print("hunks matched")
"#,
        )
        .unwrap();
        for release in ["2025.1", "2025.2", "2026.1"] {
            let patch = dir.path().join(format!("{release}.diff"));
            std::fs::write(&patch, nova_mtty_patch(release).unwrap()).unwrap();
            let output = match std::process::Command::new("python3")
                .arg(&driver)
                .arg(&applier)
                .arg(&patch)
                .output()
            {
                Ok(output) => output,
                Err(error) => {
                    eprintln!("python3 unavailable, skipping the mtty anchor test: {error}");
                    return;
                }
            };
            assert!(
                output.status.success(),
                "{release}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(
                String::from_utf8_lossy(&output.stdout).contains("hunks matched"),
                "{release}: {}",
                String::from_utf8_lossy(&output.stdout)
            );
        }
    }

    /// The same check against the real thing: clone each Nova branch the patches are written
    /// for (sparse, no blobs outside `nova/virt/libvirt`) and apply farm's file to the branch
    /// tip and to every release tag on it, which is what an image built at any point of the
    /// series' life carries. Needs the network:
    ///
    /// ```console
    /// cargo test the_mtty_backport_applies_to_the_upstream_branches -- --ignored --nocapture
    /// ```
    #[test]
    #[ignore = "clones OpenStack Nova; see the doc comment"]
    fn the_mtty_backport_applies_to_the_upstream_branches() {
        // The major versions `releases.openstack.org` publishes per series; the .0 tag is the
        // first release, and each later tag is a bugfix the image may have been built from.
        let series = [
            (
                "2025.1",
                "unmaintained/2025.1",
                ["31.0.0", "31.1.0", "31.2.0", "31.3.0"],
            ),
            (
                "2025.2",
                "stable/2025.2",
                ["32.0.0", "32.1.0", "32.2.0", ""],
            ),
            (
                "2026.1",
                "stable/2026.1",
                ["33.0.0", "33.0.1", "33.0.2", ""],
            ),
        ];
        let applier = tempfile::tempdir().unwrap();
        let applier = applier.path().join("apply.py");
        std::fs::write(&applier, include_str!("patches/apply.py")).unwrap();
        let patch_path = tempfile::tempdir().unwrap();
        // Applies the patch to `checkout` and proves the result: the patcher's own check,
        // then the import the container would do. Returns what the patcher printed.
        let check = |checkout: &std::path::Path, patch: &std::path::Path| -> String {
            let apply = std::process::Command::new("python3")
                .arg(&applier)
                .arg(patch)
                .arg(checkout)
                .output()
                .expect("python3");
            assert!(
                apply.status.success(),
                "{}",
                String::from_utf8_lossy(&apply.stderr)
            );
            let checked = std::process::Command::new("python3")
                .arg(&applier)
                .arg("--check")
                .arg(patch)
                .arg(checkout)
                .output()
                .expect("python3");
            assert!(
                checked.status.success(),
                "{}",
                String::from_utf8_lossy(&checked.stderr)
            );
            let compile = std::process::Command::new("python3")
                .args(["-m", "py_compile"])
                .arg(checkout.join("nova/virt/libvirt/driver.py"))
                .arg(checkout.join("nova/virt/libvirt/config.py"))
                .output()
                .expect("python3");
            assert!(
                compile.status.success(),
                "{}",
                String::from_utf8_lossy(&compile.stderr)
            );
            String::from_utf8_lossy(&apply.stdout).into_owned()
        };
        for (release, branch, tags) in series {
            let dir = tempfile::tempdir().unwrap();
            let checkout = dir.path().join("nova");
            // Only the package tree the patch touches; Nova's history and tests stay away.
            let clone = std::process::Command::new("git")
                .args([
                    "clone",
                    "--depth",
                    "1",
                    "--filter=blob:none",
                    "--sparse",
                    "--branch",
                    branch,
                    "https://opendev.org/openstack/nova.git",
                ])
                .arg(&checkout)
                .output()
                .expect("git");
            assert!(
                clone.status.success(),
                "{branch}: {}",
                String::from_utf8_lossy(&clone.stderr)
            );
            let sparse = std::process::Command::new("git")
                .arg("-C")
                .arg(&checkout)
                .args(["sparse-checkout", "set", "nova/virt/libvirt"])
                .output()
                .expect("git");
            assert!(sparse.status.success());
            let patch = patch_path.path().join("backport.diff");
            std::fs::write(&patch, nova_mtty_patch(release).unwrap()).unwrap();

            let head = std::process::Command::new("git")
                .arg("-C")
                .arg(&checkout)
                .args(["log", "--oneline", "-1"])
                .output()
                .unwrap();
            println!(
                "{release} ({}) tip: {}py_compile OK",
                String::from_utf8_lossy(&head.stdout).trim(),
                check(&checkout, &patch),
            );
            for tag in tags.iter().filter(|tag| !tag.is_empty()) {
                let fetch = std::process::Command::new("git")
                    .arg("-C")
                    .arg(&checkout)
                    .args(["fetch", "--depth", "1", "origin"])
                    .arg(format!("refs/tags/{tag}"))
                    .output()
                    .expect("git");
                assert!(
                    fetch.status.success(),
                    "{release} {tag}: {}",
                    String::from_utf8_lossy(&fetch.stderr)
                );
                let reset = std::process::Command::new("git")
                    .arg("-C")
                    .arg(&checkout)
                    .args(["checkout", "--force", "FETCH_HEAD"])
                    .output()
                    .expect("git");
                assert!(
                    reset.status.success(),
                    "{release} {tag}: {}",
                    String::from_utf8_lossy(&reset.stderr)
                );
                println!(
                    "{release} tag {tag}: {}py_compile OK",
                    check(&checkout, &patch)
                );
            }
        }
    }

    /// The prepare script is a host shell program that ends up in a root container and on
    /// the host, so it has to parse and pass `shellcheck`. `FARM_DUMP_NOVA_PREPARE=<path>`
    /// writes it out for a manual look.
    #[test]
    fn the_mtty_prepare_script_patches_a_copy_of_the_image_nova() {
        for release in ["2025.1", "2025.2", "2026.1"] {
            let script = nova_mtty_prepare_script(release).unwrap();
            assert!(
                script.contains(nova_mtty_image(release).unwrap()),
                "{release}"
            );
            assert!(script.contains("--user root"), "{release}");
            assert!(script.contains("FARM_APPLY_PY"), "{release}");
            assert!(
                script.contains("/farm/apply.py /farm/nova-mtty.patch"),
                "{release}"
            );
            assert!(
                script.contains(&format!(
                    "' farm '{}' < /dev/null",
                    nova_mtty_site(release).unwrap()
                )),
                "{release}"
            );
            assert!(
                script.contains(&format!("out='{NOVA_MTTY_DIR}/{release}'")),
                "{release}"
            );
            assert!(!script.contains("{{"), "{release}");
        }
        let script = nova_mtty_prepare_script("2025.1").unwrap();
        // The heredocs hold a program and a diff: a stray delimiter would end one early.
        let body = script.split_once("<<'FARM_APPLY_PY'\n").unwrap().1;
        let (applier, rest) = body.split_once("FARM_APPLY_PY\n").unwrap();
        assert!(applier.contains("def apply_hunk("));
        let (diff, _) = rest
            .split_once("<<'FARM_PATCH'\n")
            .unwrap()
            .1
            .split_once("FARM_PATCH\n")
            .unwrap();
        assert!(diff.starts_with("# farm: OpenStack Nova"));
        assert!(nova_mtty_prepare_script("2026.2").is_none());

        if let Ok(path) = std::env::var("FARM_DUMP_NOVA_PREPARE") {
            std::fs::write(path, &script).unwrap();
        }
        let output = match std::process::Command::new("bash")
            .args(["-n", "-c", &script])
            .output()
        {
            Ok(output) => output,
            Err(error) => {
                eprintln!("bash unavailable, skipping the mtty prepare test: {error}");
                return;
            }
        };
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        use std::io::Write;
        let mut child = match std::process::Command::new("shellcheck")
            .args(["-s", "bash", "-"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
        {
            Ok(child) => child,
            Err(error) => {
                eprintln!("shellcheck unavailable, skipping the mtty prepare lint: {error}");
                return;
            }
        };
        child
            .stdin
            .take()
            .unwrap()
            .write_all(script.as_bytes())
            .unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stdout)
        );
    }

    #[test]
    fn the_mtty_volumes_mount_the_host_copies_over_the_image_modules() {
        assert_eq!(
            nova_mtty_extra_volumes("2025.1").unwrap(),
            "nova_compute_extra_volumes: \"{{ default_extra_volumes + ['/etc/farm/nova-mtty/2025.1/nova/virt/libvirt/config.py:/var/lib/kolla/venv/lib/python3.9/site-packages/nova/virt/libvirt/config.py:ro', '/etc/farm/nova-mtty/2025.1/nova/virt/libvirt/driver.py:/var/lib/kolla/venv/lib/python3.9/site-packages/nova/virt/libvirt/driver.py:ro'] }}\"\n"
        );
        let volumes = nova_mtty_extra_volumes("2026.1").unwrap();
        assert!(
            volumes.contains(
                "/var/lib/kolla/venv/lib/python3.12/site-packages/nova/virt/libvirt/driver.py:ro"
            ),
            "{volumes}"
        );
        assert!(nova_mtty_extra_volumes("2026.2").is_none());
        let selected = ["keystone", "glance", "nova"].map(String::from);
        let text = globals(
            "10.254.0.10",
            "farm0",
            "x86_64",
            false,
            &selected,
            false,
            &crate::kolla::releases::info(OPENSTACK_RELEASE),
        );
        assert!(!text.contains("nova_compute_extra_volumes"), "{text}");
    }

    /// The patcher inside the hook is what stands between a drifted Nova source and a
    /// half-patched driver.py, so exercise it on a fixture: apply, re-apply, check, and
    /// fail on a source that matches neither side.
    #[test]
    fn the_mtty_patcher_applies_once_and_fails_loudly_on_drift() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("site-packages");
        std::fs::create_dir_all(root.join("pkg")).unwrap();
        let source = root.join("pkg/mod.py");
        std::fs::write(&source, "value = 0\ndef g():\n    return 2\n").unwrap();
        let patch = dir.path().join("backport.diff");
        std::fs::write(
            &patch,
            "# provenance stays out of the hunks\n--- a/pkg/mod.py\n+++ b/pkg/mod.py\n@@ -1,3 +1,3 @@\n-value = 0\n+value = 1\n def g():\n     return 2\n",
        )
        .unwrap();
        let applier = dir.path().join("apply.py");
        std::fs::write(&applier, include_str!("patches/apply.py")).unwrap();
        let run = |args: &[&str]| {
            std::process::Command::new("python3")
                .arg(&applier)
                .args(args)
                .output()
        };
        let patch = patch.to_str().unwrap();
        let root_text = root.to_str().unwrap();

        let applied = match run(&[patch, root_text]) {
            Ok(output) => output,
            Err(error) => {
                eprintln!("python3 unavailable, skipping the mtty patcher test: {error}");
                return;
            }
        };
        assert!(
            applied.status.success(),
            "{}",
            String::from_utf8_lossy(&applied.stderr)
        );
        assert_eq!(
            std::fs::read_to_string(&source).unwrap(),
            "value = 1\ndef g():\n    return 2\n"
        );

        // Re-running is a no-op, and the check the verify phase runs passes.
        let again = run(&[patch, root_text]).unwrap();
        assert!(again.status.success());
        assert!(String::from_utf8_lossy(&again.stdout).contains("already applied"));
        let checked = run(&["--check", patch, root_text]).unwrap();
        assert!(checked.status.success());
        assert!(String::from_utf8_lossy(&checked.stdout).contains("backport in place"));

        // A context line at the end of the hunk may drift with a Nova bugfix; the changed
        // line still identifies the hunk.
        std::fs::write(&source, "value = 0\ndef h():\n    return 2\n").unwrap();
        let drifted = run(&[patch, root_text]).unwrap();
        assert!(
            drifted.status.success(),
            "{}",
            String::from_utf8_lossy(&drifted.stderr)
        );
        assert_eq!(
            std::fs::read_to_string(&source).unwrap(),
            "value = 1\ndef h():\n    return 2\n"
        );

        // A source that matches nothing is a failure, in either mode.
        std::fs::write(&source, "value = 7\n").unwrap();
        let unmatched = run(&[patch, root_text]).unwrap();
        assert!(!unmatched.status.success());
        assert!(
            String::from_utf8_lossy(&unmatched.stderr).contains("cannot patch pkg/mod.py"),
            "{}",
            String::from_utf8_lossy(&unmatched.stderr)
        );
        let unchecked = run(&["--check", patch, root_text]).unwrap();
        assert!(!unchecked.status.success());
    }
}
