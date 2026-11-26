use anyhow::{Context, Result, bail};

use crate::stackbox::{Role, StackboxNode};

pub const OPENSTACK_RELEASE: &str = "2026.2";

/// A multi-node stackbox's compute VM, as Kolla reaches it over the management link.
#[derive(Clone, Debug, PartialEq)]
pub struct Compute {
    /// msb VM name.
    pub name: String,
    pub hostname: String,
    pub address: String,
}

/// The VM that runs Kolla and the control plane (all-in-one, or a multi-node stackbox's control
/// VM), and a multi-node stackbox's computes.
#[derive(Debug)]
pub struct Topology {
    pub name: String,
    pub ip: Option<String>,
    /// Interface holding `ip`: the fixed `stackbox0` service address, or eth0 on older stackboxes.
    pub interface: String,
    pub computes: Vec<Compute>,
    /// A control VM and its computes, if any yet, rather than one all-in-one VM.
    pub multinode: bool,
}

impl Topology {
    pub fn from_nodes(nodes: &[StackboxNode]) -> Result<Self> {
        if crate::stackbox::observed_layout(nodes).is_none() {
            bail!(
                "deployment requires one all-in-one VM, or a control VM and its compute VMs; legacy multi-VM stackboxes cannot be deployed with this version (create a new stackbox)"
            );
        }
        if let Some(node) = nodes.iter().find(|node| node.status != "running") {
            bail!("{} is {}, expected running", node.name, node.status);
        }
        // A control VM without computes yet, restored from a control template, still deploys
        // as a multi-node stackbox.
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
                hostname: crate::stackbox::hostname(Role::Compute, node.index),
                address: crate::stackbox::address(Role::Compute, node.index),
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

/// `multinode`: a control VM and computes joined by the stackbox's links, with tenant networks
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
    let source = image_source(arch, release);
    // globals.yml is passed as extra vars, which beat inventory host variables, so a literal
    // suffix or registry would make every VM pull the control VM's images. A multi-node stackbox's
    // computes may run on hosts of another architecture: each of these reads the host's own
    // `stackbox_*` inventory variable, falling back to the control VM's value.
    let tag_suffix_line = if multinode {
        per_host_line("openstack_tag_suffix", "stackbox_tag_suffix", source.suffix)
    } else {
        format!("openstack_tag_suffix: \"{}\"", source.suffix)
    };
    // Kolla's images are tagged with the series name on the legacy releases and with the
    // year.release name from 2023.1 on.
    let image_tag = &release.series;
    let distro = release.base_distro();
    // The tag's default registry carries no end-of-life images. A series the stackbox builds and
    // publishes itself names its own registry and namespace; the image tag then follows its
    // base distro (`zed-rocky-9`) rather than the tag's default.
    // A series whose images the stackbox builds names its registry in every mode. A maintained
    // series reads each host's registry in a multi-node stackbox, where the control VM's architecture
    // may not be the compute's; a single VM takes the aarch64 build of an arm64 series, if any.
    let images = match (&release.images, multinode) {
        (None, true) => format!(
            "{}\n{}\n",
            per_host_line(
                "docker_registry",
                "stackbox_docker_registry",
                &source.registry
            ),
            per_host_line(
                "docker_namespace",
                "stackbox_docker_namespace",
                &source.namespace
            ),
        ),
        _ => match release.own_origin(arch) {
            Some(origin) => format!(
                "# Kolla images the stackbox built and published for this series.\ndocker_registry: \"{registry}\"\ndocker_namespace: \"{namespace}\"\n",
                registry = origin.registry,
                namespace = origin.namespace,
            ),
            None => String::new(),
        },
    };
    let enabled = ["placement", "horizon", "neutron", "nova"]
        .iter()
        .map(|id| format!("enable_{id}: {}\n", components.iter().any(|c| c == id)))
        .collect::<String>();
    let links = if multinode {
        // Tenant networks are VLANs on each VM's `stackbox1` link (physnet1, br-tenant): VXLAN in
        // the userspace datapath would need the tunnel address on an OVS bridge. The bridges
        // are inventory host variables, which globals.yml (extra vars) would override.
        "tunnel_interface: \"stackbox0\"\nneutron_tenant_network_types: \"vlan\"\nenable_neutron_provider_networks: true\n"
    } else {
        ""
    };
    let external = if components.iter().any(|c| c == "neutron") && !multinode {
        "neutron_external_interface: \"stackbox-ext\"\n"
    } else {
        ""
    };
    let neutron = if components.iter().any(|c| c == "neutron") {
        // The guest kernel has no openvswitch datapath, so OVS forwards in userspace.
        // `stackbox-ext` is a veth the network node routes and masquerades for the external network.
        // The Neutron patch lives in the image's own Python: `python3.9` for the stackbox's Rocky 9
        // build, the release's minor on its other images.
        format!(
            concat!(
                "neutron_plugin_agent: \"openvswitch\"\n",
                "ovs_datapath: \"netdev\"\n",
                "openvswitch_vswitchd_extra_volumes: \"{{{{ default_extra_volumes + ['/etc/stackbox/ovs-extend-start:/usr/local/bin/kolla_openvswitch_extend_start:ro'] }}}}\"\n",
                // The kernel lacks xt_connmark; see neutron_sitecustomize.py.
                "neutron_l3_agent_extra_volumes: \"{{{{ neutron_extra_volumes + ['/etc/stackbox/neutron-sitecustomize.py:/var/lib/kolla/venv/lib64/python{python}/site-packages/sitecustomize.py:ro'] }}}}\"\n",
            ),
            python = release.image_python()
        )
    } else {
        String::new()
    };
    // Releases before 2025.1 predate this guest: their host-OS precheck lists Rocky 9 only,
    // and their baremetal role imports `selinux` from a venv without the binding. The stackbox
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
    // These series derive `enable_heat` from `enable_openstack_core`, which the stackbox turns off
    // below; naming the flag keeps a series whose published images leave heat out from pulling
    // one anyway. A maintained branch keeps the flag implicit, as it deployed before.
    // From 2025.1 on, every service the stackbox can offer gets its flag written out, true when the
    // stackbox selected it, so an unselected one stays off whatever the series defaults to.
    let newer_services = release.release.as_str() >= "2025.1";
    let flags = if newer_services {
        ["heat", "barbican", "designate", "magnum", "skyline"]
            .iter()
            .map(|id| format!("enable_{id}: {}\n", components.iter().any(|c| c == id)))
            .collect::<String>()
    } else {
        "enable_heat: false\n".to_owned()
    };
    // Designate runs its bind9 backend inside the stackbox, with one name server record for its pool.
    // When Neutron runs beside it, Neutron's DNS domain is the same `stackbox.local.` zone.
    let designate = if newer_services && components.iter().any(|c| c == "designate") {
        "designate_backend: \"bind9\"\ndesignate_ns_record:\n  - \"ns1.stackbox.local\"\n"
    } else {
        ""
    };
    let dns_domain = if newer_services
        && components.iter().any(|c| c == "designate")
        && components.iter().any(|c| c == "neutron")
    {
        "neutron_dns_domain: \"stackbox.local.\"\n"
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
{tag_suffix_line}
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
{flags}enable_mariadb: true
enable_memcached: true
enable_rabbitmq: true
enable_keystone: true
enable_glance: true
glance_backend_file: true
{enabled}{links}{external}{neutron}{nova}{designate}{dns_domain}"#
    )
}

/// The mediated-device types nova-compute may bind, in the Kolla file merged into
/// nova-compute's `nova.conf`. `mtty_mtty` is the kernel's virtual parent device, which
/// Nova accepts in place of a PCI address.
pub const MDEV_NOVA_COMPUTE_CONF: &str =
    "[devices]\nenabled_mdev_types = mtty-1\n\n[mdev_mtty-1]\ndevice_addresses = mtty_mtty\n";

/// The nova-compute.conf of every stackbox. Without `resume_guests_state_on_host_boot`, Nova
/// leaves each instance SHUTOFF when its compute VM comes back from a snapshot.
pub const NOVA_COMPUTE_BASE_CONF: &str = "[DEFAULT]\nresume_guests_state_on_host_boot = True\n";

/// A PCI device the compute-1 guest shows after `stackbox-vfio-prepare` bound it to `vfio-pci`.
/// `bdf` is the guest's address, which is what Nova's `device_spec` names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GuestPciDevice {
    pub bdf: String,
    pub vendor_id: String,
    pub product_id: String,
    pub iommu_group: u32,
}

/// One `[pci] alias`: a flavor asks for it by name, and Nova matches it to a device by ids.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PciAlias {
    pub name: String,
    pub vendor_id: String,
    pub product_id: String,
}

/// The filters the scheduler runs. OpenStack 2025.1 to 2026.2 leave `PciPassthroughFilter`
/// out of their default list, so stackbox names the whole list.
pub const NOVA_ENABLED_FILTERS: &str = "ComputeFilter, ComputeCapabilitiesFilter, ImagePropertiesFilter, ServerGroupAntiAffinityFilter, ServerGroupAffinityFilter, PciPassthroughFilter";

/// One alias per distinct `(vendor_id, product_id)`, in the order the devices are listed: the
/// first pair is `stackbox-pci`, the next `stackbox-pci-2`, and so on.
pub fn pci_aliases(devices: &[GuestPciDevice]) -> Vec<PciAlias> {
    let mut pairs: Vec<(String, String)> = Vec::new();
    for device in devices {
        let pair = (device.vendor_id.clone(), device.product_id.clone());
        if !pairs.contains(&pair) {
            pairs.push(pair);
        }
    }
    pairs
        .into_iter()
        .enumerate()
        .map(|(index, (vendor_id, product_id))| PciAlias {
            name: pci_alias_name(index),
            vendor_id,
            product_id,
        })
        .collect()
}

/// The alias of the `index`th distinct device pair (zero-based).
pub fn pci_alias_name(index: usize) -> String {
    if index == 0 {
        "stackbox-pci".to_owned()
    } else {
        format!("stackbox-pci-{}", index + 1)
    }
}

/// The first line of every `nova.conf` stackbox writes. Removal checks for it, so a file an operator
/// wrote by hand is never deleted.
pub const NOVA_GLOBAL_MARKER: &str = "# Managed by stackbox";

/// The nova.conf that every Nova service merges (`/etc/kolla/config/nova.conf`): the scheduler's
/// filters and the `[pci]` aliases. `None` when the stackbox has no passthrough device.
pub fn nova_global_conf(pci_devices: &[GuestPciDevice]) -> Option<String> {
    if pci_devices.is_empty() {
        return None;
    }
    let mut conf = format!(
        "{NOVA_GLOBAL_MARKER}\n[filter_scheduler]\nenabled_filters = {NOVA_ENABLED_FILTERS}\n\n[pci]\n"
    );
    for alias in pci_aliases(pci_devices) {
        conf.push_str(&format!(
            "alias = {{\"name\": \"{}\", \"vendor_id\": \"{}\", \"product_id\": \"{}\", \"device_type\": \"type-PCI\"}}\n",
            alias.name, alias.vendor_id, alias.product_id
        ));
    }
    Some(conf)
}

/// The whole nova-compute.conf: the base section, then the mtty sections when `mdev`, then one
/// `device_spec` per passthrough device (named by its guest address).
pub fn nova_compute_conf(mdev: bool, pci_devices: &[GuestPciDevice]) -> String {
    let mut conf = NOVA_COMPUTE_BASE_CONF.to_owned();
    if mdev {
        conf.push('\n');
        conf.push_str(MDEV_NOVA_COMPUTE_CONF);
    }
    if !pci_devices.is_empty() {
        conf.push_str("\n[pci]\n");
        for device in pci_devices {
            conf.push_str(&format!(
                "device_spec = {{\"vendor_id\": \"{}\", \"product_id\": \"{}\", \"address\": \"{}\"}}\n",
                device.vendor_id, device.product_id, device.bdf
            ));
        }
    }
    conf
}

/// Where each host keeps the Nova files stackbox's mtty backport mounts into nova_compute, as
/// `<release>/<module>` (see [`NOVA_MTTY_MODULES`]).
pub const NOVA_MTTY_DIR: &str = "/etc/stackbox/nova-mtty";

/// Where a host keeps the script that builds those files from the image.
pub const NOVA_MTTY_PREPARE: &str = "/etc/stackbox/prepare-nova-mtty.sh";

/// The two Nova modules the backport changes, relative to the Nova package. Each one is
/// mounted over the image's own copy.
pub const NOVA_MTTY_MODULES: [&str; 2] =
    ["nova/virt/libvirt/config.py", "nova/virt/libvirt/driver.py"];

/// stackbox's backport of Nova's `mtty` support for a release whose Nova has none: upstream
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
/// It extends Kolla's list instead of replacing it: every release stackbox supports defaults
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
# Written by stackbox: builds the Nova files nova_compute mounts for stackbox's mtty backport.
set -o errexit
set -o pipefail
set -o nounset

image='{image}'
out='{NOVA_MTTY_DIR}/{release}'
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
cat > "$work/apply.py" <<'STACKBOX_APPLY_PY'
{apply}STACKBOX_APPLY_PY
cat > "$work/nova-mtty.patch" <<'STACKBOX_PATCH'
{patch}STACKBOX_PATCH
docker pull "$image" >/dev/null
docker run --rm --user root --entrypoint bash -v "$work:/stackbox" "$image" -c '
set -o errexit
site=$(python3 -c "import os, nova; print(os.path.dirname(os.path.realpath(nova.__path__[0])))")
[ "$site" = "$1" ] || {{ echo "nova is in $site, not $1" >&2; exit 1; }}
mkdir -p /stackbox/src/nova/virt/libvirt
cp "$site/nova/virt/libvirt/config.py" "$site/nova/virt/libvirt/driver.py" /stackbox/src/nova/virt/libvirt/
python3 /stackbox/apply.py /stackbox/nova-mtty.patch /stackbox/src
' stackbox '{site}' < /dev/null
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

/// Neutron on a multi-node stackbox: tenant VLANs on physnet1 (every VM's `stackbox1` link, bridged
/// as br-tenant) and the external flat network on physnet2 (br-ex on the control VM).
pub const MULTINODE_ML2: &str = "[ml2_type_vlan]\nnetwork_vlan_ranges = physnet1:100:199\n[ml2_type_flat]\nflat_networks = physnet2\n";
/// The Open vSwitch agents of a multi-node stackbox run no tunnels. Kolla's default hybrid
/// iptables firewall needs physdev, which the guest kernel lacks; the OVS firewall uses the
/// userspace datapath's own conntrack.
pub const MULTINODE_OVS_AGENT: &str = "[agent]\ntunnel_types =\nl2_population = false\n[securitygroup]\nfirewall_driver = openvswitch\n";

/// The file Kolla merges into Skyline's `skyline.yaml` (`{{ node_custom_config }}/skyline/skyline.yaml`,
/// `node_custom_config` = `/etc/kolla/config`) in 2025.1, 2025.2 and 2026.2. Skyline's login asks
/// Keystone for the catalog's `public` endpoint by default (`openstack.interface_type`). On the
/// control VM that address is a host DNAT the VM cannot reach, so the login fails with 401 while the
/// password is right. The internal endpoint is the one the VM serves.
pub const SKYLINE_OVERRIDE: &str = "# Written by stackbox: the control VM cannot reach the public endpoint (host DNAT), so log in through the internal one.\nopenstack:\n  interface_type: internal\n";

/// The Skyline override for a stackbox, or `None` when Skyline is not selected.
pub fn skyline_override(components: &[String]) -> Option<&'static str> {
    components
        .iter()
        .any(|c| c == "skyline")
        .then_some(SKYLINE_OVERRIDE)
}

/// The Kolla image tag suffix and registry of a VM of `arch`: the suffix is `-aarch64` where the
/// release publishes aarch64 images; the registry is the one [`ReleaseInfo::own_origin`] names,
/// kolla-ansible's default otherwise.
pub fn image_source(arch: &str, release: &crate::kolla::releases::ReleaseInfo) -> ImageSource {
    let origin = release
        .own_origin(arch)
        .unwrap_or_else(|| crate::kolla::releases::Origin {
            registry: crate::kolla::releases::KOLLA_REGISTRY.into(),
            namespace: crate::kolla::releases::KOLLA_NAMESPACE.into(),
        });
    ImageSource {
        suffix: tag_suffix(arch, release),
        registry: origin.registry,
        namespace: origin.namespace,
    }
}

/// Where one VM pulls its Kolla images from: the tag suffix and the registry path.
#[derive(Clone, Debug, PartialEq)]
pub struct ImageSource {
    /// `-aarch64` or empty, appended to the image tag.
    pub suffix: &'static str,
    /// The registry, `ghcr.io` or `quay.io`.
    pub registry: String,
    /// Below-the-registry name, `preinpost/kolla` or `openstack.kolla`.
    pub namespace: String,
}

/// The Kolla image tag suffix of a VM of `arch`. Only the releases whose Rocky images are
/// published for aarch64 carry it; the amd64-only series the stackbox builds itself and the older
/// maintained ones have none.
pub fn tag_suffix(arch: &str, release: &crate::kolla::releases::ReleaseInfo) -> &'static str {
    if arch.trim() == "aarch64" && release.arm64 {
        "-aarch64"
    } else {
        ""
    }
}

/// One `globals.yml` line whose value is the host's own inventory variable `var`, falling back
/// to `control` (the control VM's value) on a host that names none. Ansible templates extra vars
/// per host, so the line reads the variable of the host it runs on.
fn per_host_line(key: &str, var: &str, control: &str) -> String {
    format!("{key}: \"{{{{ {var} | default('{control}') }}}}\"")
}

/// Rewrites the `globals.yml` lines that pin one image source for every host, as stackboxes
/// deployed before computes could differ in architecture wrote them, into the per-host form:
/// `openstack_tag_suffix`, `docker_registry`, `docker_namespace`. A registry or namespace the
/// file does not name is added with kolla-ansible's default, which is what it pulled from before.
/// Leaves the file alone when it already reads each host's values.
pub fn per_host_images(globals: &str) -> Option<String> {
    // (key in globals.yml, the inventory variable that overrides it, the control VM's default)
    let keys = [
        ("openstack_tag_suffix", "stackbox_tag_suffix", None),
        (
            "docker_registry",
            "stackbox_docker_registry",
            Some(crate::kolla::releases::KOLLA_REGISTRY),
        ),
        (
            "docker_namespace",
            "stackbox_docker_namespace",
            Some(crate::kolla::releases::KOLLA_NAMESPACE),
        ),
    ];
    let present = |key: &str| {
        globals
            .lines()
            .any(|line| line.starts_with(&format!("{key}:")))
    };
    let missing: Vec<String> = keys
        .iter()
        .filter_map(|(key, var, default)| {
            let default = (*default)?;
            (!present(key)).then(|| per_host_line(key, var, default))
        })
        .collect();
    let mut changed = false;
    let mut out = String::with_capacity(globals.len() + 128);
    for line in globals.split_inclusive('\n') {
        let body = line.trim_end_matches('\n');
        let rewritten = keys.iter().find_map(|(key, var, _)| {
            let value = body
                .strip_prefix(&format!("{key}: \""))?
                .strip_suffix('"')?;
            // A value that already reads a variable is the per-host form.
            (!value.contains(['{', '"', '\''])).then(|| per_host_line(key, var, value))
        });
        match rewritten {
            Some(line_text) => {
                out.push_str(&line_text);
                out.push_str(&line[body.len()..]);
                changed = true;
            }
            None => out.push_str(line),
        }
        // The registry lines join the file after the release, where kolla-ansible reads them.
        if body.starts_with("openstack_release:") && !missing.is_empty() {
            for extra in &missing {
                out.push_str(extra);
                out.push('\n');
            }
            changed = true;
        }
    }
    changed.then_some(out)
}

/// Kolla's multinode inventory with the groups that place services filled in: the control
/// VM runs the control plane, network node and Kolla itself; Nova runs on the computes.
/// `sources` holds the image source (tag suffix and registry) of each compute, by hostname,
/// whose architecture may differ from the control VM's; a compute missing from it uses the
/// control VM's.
pub fn multinode_inventory(
    template: &str,
    control: &str,
    computes: &[Compute],
    sources: &std::collections::BTreeMap<String, ImageSource>,
) -> Result<String> {
    let compute_hosts: Vec<String> = computes
        .iter()
        .map(|c| {
            let suffix = sources
                .get(&c.hostname)
                .map(|source| {
                    format!(
                        " stackbox_tag_suffix=\"{}\" stackbox_docker_registry=\"{}\" stackbox_docker_namespace=\"{}\"",
                        source.suffix, source.registry, source.namespace
                    )
                })
                .unwrap_or_default();
            format!(
                "{} ansible_host={} neutron_external_interface=stackbox1 neutron_bridge_name=br-tenant{suffix}",
                c.hostname, c.address
            )
        })
        .collect();
    let groups = [
        (
            "control",
            format!(
                "{control} ansible_connection=local neutron_external_interface=stackbox1,stackbox-ext neutron_bridge_name=br-tenant,br-ex"
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
        bail!("unexpected Kolla multinode inventory; review the stackbox's inventory rewrite");
    }
    Ok(out)
}

/// Kolla merges this override after its dynamic 40%-of-host-RAM default.
pub const GALERA_CONFIG: &str = "[mysqld]\ninnodb_buffer_pool_size = 512M\nmax_connections = 256\nkey_buffer_size = 16M\nmax_heap_table_size = 16M\ntmp_table_size = 16M\nwsrep_slave_threads = 1\n";

#[cfg(test)]
mod tests {
    use super::*;

    fn node(role: Role) -> StackboxNode {
        StackboxNode {
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
    fn multi_node_stackboxes_deploy_from_their_control_vm_to_numbered_computes() {
        let compute = |index| StackboxNode {
            name: format!("demo-compute-{index}"),
            index: Some(index),
            ..node(Role::Compute)
        };
        let control = StackboxNode {
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
                    hostname: "stackbox-compute-1".into(),
                    address: "10.254.0.11".into(),
                },
                Compute {
                    name: "demo-compute-2".into(),
                    hostname: "stackbox-compute-2".into(),
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
            "stackbox0",
            "aarch64",
            false,
            &selected,
            true,
            &crate::kolla::releases::info(OPENSTACK_RELEASE),
        );
        assert!(globals.contains("tunnel_interface: \"stackbox0\"\n"));
        assert!(globals.contains("neutron_tenant_network_types: \"vlan\"\n"));
        assert!(globals.contains("enable_neutron_provider_networks: true\n"));
        // Extra vars would override the inventory's per-host bridges.
        assert!(!globals.contains("neutron_external_interface"));
        // ...and its per-host image suffix: each host reads its own, the control VM's by default.
        assert!(globals.contains(
            "openstack_tag_suffix: \"{{ stackbox_tag_suffix | default('-aarch64') }}\"\n"
        ));
        let single = globals_single(&selected);
        assert!(single.contains("neutron_external_interface: \"stackbox-ext\"\n"));
        assert!(!single.contains("tunnel_interface"));
        assert!(MULTINODE_ML2.contains("network_vlan_ranges = physnet1:100:199"));
        assert!(MULTINODE_ML2.contains("flat_networks = physnet2"));
        assert!(MULTINODE_OVS_AGENT.contains("tunnel_types =\n"));

        // The head of Kolla's inventory/multinode, which the rewrite keys on.
        let template = "# These initial groups are the only groups required to be modified.\n[control]\n# These hostname must be resolvable from your deployment host\ncontrol01\ncontrol02\ncontrol03\n\n# The above can also be specified as follows:\n#control[01:03]     ansible_user=kolla\n\n[network]\nnetwork01\nnetwork02\n\n[compute]\ncompute01\n\n[monitoring]\nmonitoring01\n\n[storage]\nstorage01\n\n[deployment]\nlocalhost       ansible_connection=local\n\n[baremetal:children]\ncontrol\n";
        let no_suffixes = std::collections::BTreeMap::new();
        let inventory = multinode_inventory(
            template,
            "stackbox-control",
            &topology_computes(),
            &no_suffixes,
        )
        .unwrap();
        assert!(inventory.contains("[control]\nstackbox-control ansible_connection=local neutron_external_interface=stackbox1,stackbox-ext neutron_bridge_name=br-tenant,br-ex\n\n"));
        assert!(inventory.contains("[network]\nstackbox-control\n\n"));
        assert!(inventory.contains("[compute]\nstackbox-compute-1 ansible_host=10.254.0.11 neutron_external_interface=stackbox1 neutron_bridge_name=br-tenant\n\n"));
        assert!(inventory.contains("[storage]\nstackbox-control\n\n[deployment]\nlocalhost"));
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
        assert!(
            multinode_inventory("[control]\nx\n", "stackbox-control", &[], &no_suffixes).is_err()
        );
        let alone = multinode_inventory(template, "stackbox-control", &[], &no_suffixes).unwrap();
        assert!(alone.contains("[compute]\n\n[monitoring]\nstackbox-control\n"));
    }

    #[test]
    fn newer_services_get_explicit_flags_and_designate_its_backend() {
        let selected = ["keystone", "glance", "heat", "designate", "neutron"].map(String::from);
        let rendered = globals_at(&selected, "2025.2");
        assert!(rendered.contains("enable_heat: true\n"));
        assert!(rendered.contains("enable_designate: true\n"));
        // Unselected services are named off rather than left to the series' defaults.
        for id in ["barbican", "magnum", "skyline"] {
            assert!(rendered.contains(&format!("enable_{id}: false\n")), "{id}");
        }
        assert!(rendered.contains("designate_backend: \"bind9\"\n"));
        // kolla's pools.yaml.j2 appends the trailing dot itself; a dotted value would render `ns1.stackbox.local..`.
        assert!(rendered.contains("designate_ns_record:\n  - \"ns1.stackbox.local\"\n"));
        assert!(!rendered.contains("ns1.stackbox.local.\""));
        assert!(rendered.contains("neutron_dns_domain: \"stackbox.local.\"\n"));

        // Without Designate, Neutron keeps its own DNS domain and no backend is named.
        let without = ["keystone", "glance", "heat", "neutron"].map(String::from);
        let rendered = globals_at(&without, "2025.2");
        assert!(rendered.contains("enable_designate: false\n"));
        assert!(!rendered.contains("designate_backend"));
        assert!(!rendered.contains("designate_ns_record"));
        assert!(!rendered.contains("neutron_dns_domain"));

        // Before 2025.1 the newer flags are not written; heat keeps its explicit false.
        let old = globals_at(&["keystone", "glance"].map(String::from), "2024.2");
        assert!(old.contains("enable_heat: false\n"));
        assert!(!old.contains("enable_barbican"));
        assert!(!old.contains("enable_skyline"));
    }

    fn globals_at(selected: &[String], release: &str) -> String {
        globals(
            "10.254.0.2",
            "stackbox0",
            "aarch64",
            false,
            selected,
            false,
            &crate::kolla::releases::info(release),
        )
    }

    fn globals_single(selected: &[String]) -> String {
        globals(
            "10.254.0.2",
            "stackbox0",
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
            hostname: "stackbox-compute-1".into(),
            address: "10.254.0.11".into(),
        }]
    }

    #[test]
    fn computes_of_another_architecture_pull_their_own_images() {
        let current = crate::kolla::releases::info(OPENSTACK_RELEASE);
        assert!(current.arm64);
        assert_eq!(tag_suffix("aarch64", &current), "-aarch64");
        assert_eq!(tag_suffix("aarch64\n", &current), "-aarch64");
        assert_eq!(tag_suffix("x86_64", &current), "");
        let amd64_only = crate::kolla::releases::info("2024.2");
        assert_eq!(tag_suffix("aarch64", &amd64_only), "");

        let template = "[control]\ncontrol01\n\n[network]\nnetwork01\n\n[compute]\ncompute01\n\n[monitoring]\nmonitoring01\n\n[storage]\nstorage01\n";
        let mut computes = topology_computes();
        computes.push(Compute {
            name: "demo-compute-2".into(),
            hostname: "stackbox-compute-2".into(),
            address: "10.254.0.12".into(),
        });
        let sources = std::collections::BTreeMap::from([
            (
                "stackbox-compute-1".to_owned(),
                image_source("x86_64", &current),
            ),
            (
                "stackbox-compute-2".to_owned(),
                image_source("aarch64", &current),
            ),
        ]);
        let inventory =
            multinode_inventory(template, "stackbox-control", &computes, &sources).unwrap();
        assert!(inventory.contains("stackbox-compute-1 ansible_host=10.254.0.11 neutron_external_interface=stackbox1 neutron_bridge_name=br-tenant stackbox_tag_suffix=\"\" stackbox_docker_registry=\"quay.io\" stackbox_docker_namespace=\"openstack.kolla\"\n"));
        assert!(inventory.contains("stackbox-compute-2 ansible_host=10.254.0.12 neutron_external_interface=stackbox1 neutron_bridge_name=br-tenant stackbox_tag_suffix=\"-aarch64\" stackbox_docker_registry=\"quay.io\" stackbox_docker_namespace=\"openstack.kolla\"\n"));
        // The control VM keeps the globals' default.
        assert!(inventory.contains("[control]\nstackbox-control ansible_connection=local neutron_external_interface=stackbox1,stackbox-ext neutron_bridge_name=br-tenant,br-ex\n"));

        // A stackbox deployed before this pinned one suffix for every host in globals.yml.
        let old = "---\nopenstack_release: \"2026.2\"\nopenstack_tag_suffix: \"-aarch64\"\nnetwork_interface: \"stackbox0\"\n";
        // The registry and namespace were never written for quay's default, so the rewrite adds
        // them with that default.
        let rewritten = per_host_images(old).unwrap();
        assert_eq!(
            rewritten,
            "---\nopenstack_release: \"2026.2\"\ndocker_registry: \"{{ stackbox_docker_registry | default('quay.io') }}\"\ndocker_namespace: \"{{ stackbox_docker_namespace | default('openstack.kolla') }}\"\nopenstack_tag_suffix: \"{{ stackbox_tag_suffix | default('-aarch64') }}\"\nnetwork_interface: \"stackbox0\"\n"
        );
        assert_eq!(per_host_images(&rewritten), None);
        assert!(
            per_host_images("openstack_tag_suffix: \"\"").unwrap()
                == "openstack_tag_suffix: \"{{ stackbox_tag_suffix | default('') }}\""
        );
        assert_eq!(per_host_images("kolla_base_distro: \"rocky\"\n"), None);

        // A stackbox on a registry of its own (2026.1's aarch64 build on ghcr, single VM) gives
        // each computes the same per-host form, the control VM's registry by default.
        let ghcr = "---\nopenstack_release: \"2026.1\"\ndocker_registry: \"ghcr.io\"\ndocker_namespace: \"preinpost/kolla\"\nopenstack_tag_suffix: \"-aarch64\"\n";
        assert_eq!(
            per_host_images(ghcr).unwrap(),
            "---\nopenstack_release: \"2026.1\"\ndocker_registry: \"{{ stackbox_docker_registry | default('ghcr.io') }}\"\ndocker_namespace: \"{{ stackbox_docker_namespace | default('preinpost/kolla') }}\"\nopenstack_tag_suffix: \"{{ stackbox_tag_suffix | default('-aarch64') }}\"\n"
        );
    }

    #[test]
    fn the_2026_1_aarch64_images_come_from_the_stackbox_registry() {
        // (a) All-in-one aarch64 2026.1: ghcr registry and the `-aarch64` suffix.
        let release = crate::kolla::releases::info("2026.1");
        assert!(release.arm64);
        let selected = ["keystone", "glance", "placement", "neutron", "nova"].map(String::from);
        let single = globals(
            "10.0.0.2",
            "stackbox0",
            "aarch64",
            false,
            &selected,
            false,
            &release,
        );
        assert!(
            single.contains("docker_registry: \"ghcr.io\"\n"),
            "{single}"
        );
        assert!(
            single.contains("docker_namespace: \"preinpost/kolla\"\n"),
            "{single}"
        );
        assert!(
            single.contains("openstack_tag_suffix: \"-aarch64\"\n"),
            "{single}"
        );
        assert!(single.contains("openstack_release: \"2026.1\"\n"));
        // The amd64 all-in-one 2026.1 keeps quay's default: no registry line at all.
        let amd64 = globals(
            "10.0.0.2",
            "stackbox0",
            "x86_64",
            false,
            &selected,
            false,
            &release,
        );
        assert!(!amd64.contains("docker_registry"), "{amd64}");
        assert!(amd64.contains("openstack_tag_suffix: \"\"\n"));
        // The stackbox's own 2026.1 release keeps quay for 2026.2.
        let v2026_2 = crate::kolla::releases::info("2026.2");
        let quay = globals(
            "10.0.0.2",
            "stackbox0",
            "aarch64",
            false,
            &selected,
            false,
            &v2026_2,
        );
        assert!(!quay.contains("docker_registry"), "{quay}");

        // (b) Multi-node, x86 control and aarch64 compute: the control's default is quay, and the
        // compute's inventory line names ghcr.
        let multi = globals(
            "10.0.0.2",
            "stackbox0",
            "x86_64",
            false,
            &selected,
            true,
            &release,
        );
        assert!(
            multi.contains(
                "docker_registry: \"{{ stackbox_docker_registry | default('quay.io') }}\"\n"
            ),
            "{multi}"
        );
        assert!(multi.contains("docker_namespace: \"{{ stackbox_docker_namespace | default('openstack.kolla') }}\"\n"), "{multi}");
        assert!(
            multi.contains("openstack_tag_suffix: \"{{ stackbox_tag_suffix | default('') }}\"\n"),
            "{multi}"
        );
        let template = "[control]\ncontrol01\n\n[network]\nnetwork01\n\n[compute]\ncompute01\n\n[monitoring]\nmonitoring01\n\n[storage]\nstorage01\n";
        let compute = Compute {
            name: "demo-compute-1".into(),
            hostname: "stackbox-compute-1".into(),
            address: "10.254.0.11".into(),
        };
        let sources = std::collections::BTreeMap::from([(
            "stackbox-compute-1".to_owned(),
            image_source("aarch64", &release),
        )]);
        let inventory = multinode_inventory(
            template,
            "stackbox-control",
            std::slice::from_ref(&compute),
            &sources,
        )
        .unwrap();
        assert!(inventory.contains("stackbox-compute-1 ansible_host=10.254.0.11 neutron_external_interface=stackbox1 neutron_bridge_name=br-tenant stackbox_tag_suffix=\"-aarch64\" stackbox_docker_registry=\"ghcr.io\" stackbox_docker_namespace=\"preinpost/kolla\"\n"), "{inventory}");

        // (c) Multi-node, aarch64 control and x86 compute: the control's default is ghcr, and the
        // x86 compute reads quay's registry and no suffix from its own line.
        let multi_arm = globals(
            "10.0.0.2",
            "stackbox0",
            "aarch64",
            false,
            &selected,
            true,
            &release,
        );
        assert!(
            multi_arm.contains(
                "docker_registry: \"{{ stackbox_docker_registry | default('ghcr.io') }}\"\n"
            ),
            "{multi_arm}"
        );
        assert!(multi_arm.contains("docker_namespace: \"{{ stackbox_docker_namespace | default('preinpost/kolla') }}\"\n"), "{multi_arm}");
        assert!(
            multi_arm.contains(
                "openstack_tag_suffix: \"{{ stackbox_tag_suffix | default('-aarch64') }}\"\n"
            ),
            "{multi_arm}"
        );
        let x86 = std::collections::BTreeMap::from([(
            "stackbox-compute-1".to_owned(),
            image_source("x86_64", &release),
        )]);
        let inventory =
            multinode_inventory(template, "stackbox-control", &[compute], &x86).unwrap();
        assert!(inventory.contains("stackbox_tag_suffix=\"\" stackbox_docker_registry=\"quay.io\" stackbox_docker_namespace=\"openstack.kolla\"\n"), "{inventory}");

        // (d) 2026.2 aarch64 stays on quay; (e) 2024.2 keeps no aarch64 suffix or registry (the
        // manager refuses an aarch64 host for it through `available_on`).
        assert_eq!(
            image_source("aarch64", &v2026_2),
            ImageSource {
                suffix: "-aarch64",
                registry: "quay.io".into(),
                namespace: "openstack.kolla".into(),
            }
        );
        let amd64_only = crate::kolla::releases::info("2024.2");
        assert_eq!(image_source("aarch64", &amd64_only).suffix, "");
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
            "stackbox0",
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
            assert!(
                globals.contains(&format!("{key}: \"stackbox0\"\n")),
                "{key}"
            );
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
            "docker_sdk_",
        ] {
            assert!(!globals.contains(absent), "{absent}");
        }
        // From 2025.1 every newer service is named, and the unselected ones are off.
        assert!(globals.contains("enable_heat: false\n"));
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
        // No kmod in the guest, and Neutron's default is ip6_tables before 2026.1: stackbox
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
        // deployed with: only a series the stackbox builds itself names a registry. The fixture
        // is this function's output at the commit before that work plus the Neutron module
        // default stackbox now writes for every release, blank line and all, so an equality
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
            "stackbox0",
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
    fn the_2024_2_globals_are_unchanged_from_the_fixture() {
        // The fixture is this function's output for 2024.2 at HEAD (41caea6), with the same
        // inputs as the 2026.2 golden above. An equality failure means the 2024.2 globals a
        // deployed stackbox would get have changed.
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
            "stackbox0",
            "x86_64",
            false,
            &selected,
            false,
            &crate::kolla::releases::info("2024.2"),
        );
        assert_eq!(
            text,
            include_str!("../../tests/fixtures/kolla/globals-2024.2.yml")
        );
    }

    #[test]
    fn a_series_the_stackbox_builds_pulls_from_its_registry_on_its_base_distro() {
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
        // The guest has no kmod, and Neutron's default is ip6_tables until 2026.1; stackbox
        // empties it for every release, since this kernel cannot load modules at all.
        assert_eq!(zed.matches("neutron_modules_default: []").count(), 1);
        // The series carries no heat image; `enable_openstack_core: false` alone would leave
        // the flag to kolla-ansible's indirection.
        assert_eq!(zed.matches("enable_heat: false").count(), 1);

        // 2024.1/2024.2 are supported, but quay no longer carries them: the stackbox builds
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

        // A series the stackbox has not built keeps exactly what it pulled before: the tag's own
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
        // stackbox writes a simulated GPU's providers through Placement, and reads the mediated
        // device's from Nova: neither is a globals entry. Kolla's provider-config file only
        // takes CUSTOM_* classes, which is exactly what the standard VGPU contract is not.
        let selected = ["keystone", "glance", "placement", "neutron", "nova"].map(String::from);
        let text = globals(
            "10.254.0.10",
            "stackbox0",
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
        let plain = nova_compute_conf(false, &[]);
        assert_eq!(
            plain,
            "[DEFAULT]\nresume_guests_state_on_host_boot = True\n"
        );
        let mdev = nova_compute_conf(true, &[]);
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
        // Upstream merged `mtty` support on 2026.2 and nothing else; stackbox carries a patch
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

spec = importlib.util.spec_from_file_location("stackbox_apply", sys.argv[1])
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
    /// for (sparse, no blobs outside `nova/virt/libvirt`) and apply stackbox's file to the branch
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
    /// the host, so it has to parse and pass `shellcheck`. `STACKBOX_DUMP_NOVA_PREPARE=<path>`
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
            assert!(script.contains("STACKBOX_APPLY_PY"), "{release}");
            assert!(
                script.contains("/stackbox/apply.py /stackbox/nova-mtty.patch"),
                "{release}"
            );
            assert!(
                script.contains(&format!(
                    "' stackbox '{}' < /dev/null",
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
        let body = script.split_once("<<'STACKBOX_APPLY_PY'\n").unwrap().1;
        let (applier, rest) = body.split_once("STACKBOX_APPLY_PY\n").unwrap();
        assert!(applier.contains("def apply_hunk("));
        let (diff, _) = rest
            .split_once("<<'STACKBOX_PATCH'\n")
            .unwrap()
            .1
            .split_once("STACKBOX_PATCH\n")
            .unwrap();
        assert!(diff.starts_with("# stackbox: OpenStack Nova"));
        assert!(nova_mtty_prepare_script("2026.2").is_none());

        if let Ok(path) = std::env::var("STACKBOX_DUMP_NOVA_PREPARE") {
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
            "nova_compute_extra_volumes: \"{{ default_extra_volumes + ['/etc/stackbox/nova-mtty/2025.1/nova/virt/libvirt/config.py:/var/lib/kolla/venv/lib/python3.9/site-packages/nova/virt/libvirt/config.py:ro', '/etc/stackbox/nova-mtty/2025.1/nova/virt/libvirt/driver.py:/var/lib/kolla/venv/lib/python3.9/site-packages/nova/virt/libvirt/driver.py:ro'] }}\"\n"
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
            "stackbox0",
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

    #[test]
    fn skyline_logs_in_through_the_internal_endpoint_only_when_selected() {
        let selected = skyline_override(&["keystone".into(), "skyline".into()]).unwrap();
        // The file is merged into Skyline's skyline.yaml: the key sits under `openstack:`.
        assert_eq!(
            selected
                .lines()
                .filter(|l| !l.starts_with('#'))
                .collect::<Vec<_>>(),
            ["openstack:", "  interface_type: internal"]
        );
        assert!(selected.starts_with("# Written by stackbox"));
        assert!(skyline_override(&["keystone".into(), "horizon".into()]).is_none());
        assert!(skyline_override(&[]).is_none());
    }

    fn guest_pci(bdf: &str, vendor: &str, product: &str, group: u32) -> GuestPciDevice {
        GuestPciDevice {
            bdf: bdf.to_owned(),
            vendor_id: vendor.to_owned(),
            product_id: product.to_owned(),
            iommu_group: group,
        }
    }

    /// The `key = value` lines of a nova.conf section, with the JSON of each `alias`/`device_spec`.
    fn json_values(conf: &str, key: &str) -> Vec<serde_json::Value> {
        conf.lines()
            .filter_map(|line| line.strip_prefix(&format!("{key} = ")))
            .map(|value| serde_json::from_str(value).unwrap_or_else(|e| panic!("{value}: {e}")))
            .collect()
    }

    #[test]
    fn nova_global_conf_is_absent_without_passthrough() {
        assert_eq!(nova_global_conf(&[]), None);
    }

    #[test]
    fn nova_global_conf_starts_with_the_stackbox_marker() {
        let devices = [guest_pci("0000:00:05.0", "10de", "1eb8", 3)];
        let conf = nova_global_conf(&devices).unwrap();
        assert_eq!(conf.lines().next(), Some(NOVA_GLOBAL_MARKER));
    }

    #[test]
    fn nova_global_conf_names_every_default_filter_and_the_pci_filter() {
        let devices = [guest_pci("0000:00:05.0", "10de", "1eb8", 3)];
        let conf = nova_global_conf(&devices).unwrap();
        let filters = conf
            .lines()
            .find_map(|line| line.strip_prefix("enabled_filters = "))
            .unwrap();
        let names: Vec<&str> = filters.split(", ").collect();
        assert_eq!(
            names,
            [
                "ComputeFilter",
                "ComputeCapabilitiesFilter",
                "ImagePropertiesFilter",
                "ServerGroupAntiAffinityFilter",
                "ServerGroupAffinityFilter",
                "PciPassthroughFilter",
            ]
        );
        assert!(
            conf.starts_with(&format!("{NOVA_GLOBAL_MARKER}\n[filter_scheduler]\n")),
            "{conf}"
        );
        assert!(conf.contains("\n[pci]\n"), "{conf}");
    }

    #[test]
    fn nova_global_conf_aliases_parse_and_dedup_repeated_pairs() {
        let devices = [
            guest_pci("0000:00:05.0", "10de", "1eb8", 3),
            guest_pci("0000:00:06.0", "10de", "1eb8", 4),
            guest_pci("0000:00:07.0", "8086", "1521", 5),
        ];
        let conf = nova_global_conf(&devices).unwrap();
        let aliases = json_values(&conf, "alias");
        assert_eq!(aliases.len(), 2, "{conf}");
        assert_eq!(aliases[0]["name"], "stackbox-pci");
        assert_eq!(aliases[0]["vendor_id"], "10de");
        assert_eq!(aliases[0]["product_id"], "1eb8");
        assert_eq!(aliases[0]["device_type"], "type-PCI");
        assert_eq!(aliases[1]["name"], "stackbox-pci-2");
        assert_eq!(aliases[1]["vendor_id"], "8086");
        assert_eq!(aliases[1]["product_id"], "1521");
    }

    #[test]
    fn pci_aliases_keep_the_order_of_first_appearance() {
        let devices = [
            guest_pci("0000:00:05.0", "8086", "1521", 1),
            guest_pci("0000:00:06.0", "10de", "1eb8", 2),
            guest_pci("0000:00:07.0", "8086", "1521", 3),
            guest_pci("0000:00:08.0", "1002", "73bf", 4),
        ];
        let names: Vec<(String, String, String)> = pci_aliases(&devices)
            .into_iter()
            .map(|a| (a.name, a.vendor_id, a.product_id))
            .collect();
        assert_eq!(
            names,
            [
                ("stackbox-pci".into(), "8086".into(), "1521".into()),
                ("stackbox-pci-2".into(), "10de".into(), "1eb8".into()),
                ("stackbox-pci-3".into(), "1002".into(), "73bf".into()),
            ]
        );
    }

    #[test]
    fn nova_compute_conf_declares_each_device_by_its_guest_address() {
        let devices = [
            guest_pci("0000:00:05.0", "10de", "1eb8", 3),
            guest_pci("0000:00:06.0", "10de", "1eb8", 4),
        ];
        let conf = nova_compute_conf(false, &devices);
        assert!(conf.starts_with(NOVA_COMPUTE_BASE_CONF), "{conf}");
        assert!(!conf.contains("[devices]"), "{conf}");
        let specs = json_values(&conf, "device_spec");
        assert_eq!(specs.len(), 2, "{conf}");
        assert_eq!(specs[0]["vendor_id"], "10de");
        assert_eq!(specs[0]["product_id"], "1eb8");
        assert_eq!(specs[0]["address"], "0000:00:05.0");
        assert_eq!(specs[1]["address"], "0000:00:06.0");
        assert_eq!(conf.matches("[pci]").count(), 1, "{conf}");
    }

    #[test]
    fn nova_compute_conf_keeps_the_mtty_sections_beside_the_pci_ones() {
        let devices = [guest_pci("0000:00:05.0", "10de", "1eb8", 3)];
        let conf = nova_compute_conf(true, &devices);
        assert!(conf.starts_with(NOVA_COMPUTE_BASE_CONF), "{conf}");
        assert!(conf.contains(MDEV_NOVA_COMPUTE_CONF), "{conf}");
        assert_eq!(conf.matches("[DEFAULT]").count(), 1, "{conf}");
        assert_eq!(conf.matches("[devices]").count(), 1, "{conf}");
        assert_eq!(json_values(&conf, "device_spec").len(), 1, "{conf}");
        assert!(
            conf.find("[devices]").unwrap() < conf.find("[pci]").unwrap(),
            "{conf}"
        );
    }
}
