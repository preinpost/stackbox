use anyhow::{Context, Result};
use std::collections::BTreeMap;

use microsandbox::sandbox::{DestroyOptions, SandboxBuilder, SandboxInspectedSettings};
use microsandbox::{
    MicrosandboxError, MicrosandboxResult, NetworkPolicy, NetworkProfile, Operation, Sandbox,
    UnsupportedReason,
};

use super::golden;
use super::template::{NodeSpec, Role};
use crate::link::guest::LinkConf;
use crate::shell::run;
use std::path::Path;

pub const STACKBOX_LABEL: &str = "stackbox";
pub const ROLE_LABEL: &str = "stackbox.role";
pub const OVERLAY_IP_LABEL: &str = "stackbox.overlay.ip";
/// Guest distribution (`GuestOs::id`) of VMs made from a golden snapshot.
pub const OS_LABEL: &str = "stackbox.os";
/// A multi-node compute VM's position (`NodeSpec::index`).
pub const INDEX_LABEL: &str = "stackbox.node.index";

/// Every all-in-one guest gets the same hostname and service address, so a snapshot
/// restores into another stackbox (and a restart into a new msb slot) unchanged. Multi-node
/// stackboxes' VMs get theirs from `template::hostname` and `template::address` the same way.
pub const GUEST_HOSTNAME: &str = "stackbox-aio";
pub const GUEST_ADDRESS: &str = "10.254.0.2";
pub const GUEST_INTERFACE: &str = "stackbox0";
pub const IDENTITY_CONF: &str = "/etc/stackbox/identity.conf";

/// The guest's fixed hostname and address. A VM that serves the stackbox's APIs (`publish`)
/// DNATs every port the host publishes from eth0 to that address; `peers` are the stackbox's
/// other VMs as (address, hostname), kept in /etc/hosts.
pub fn identity_conf(spec: &NodeSpec, publish: bool, peers: &[(String, String)]) -> String {
    let ports: Vec<String> = crate::model::COMPONENTS
        .iter()
        .flat_map(|c| c.public.iter().map(|p| p.guest_port.to_string()))
        .chain(
            crate::model::INFRA_PORTS
                .iter()
                .map(|p| p.guest_port.to_string()),
        )
        .filter(|_| publish)
        .collect();
    let peers: Vec<String> = peers
        .iter()
        .map(|(address, hostname)| format!("{address}={hostname}"))
        .collect();
    format!(
        "STACKBOX_HOSTNAME={}\nSTACKBOX_ADDRESS={}\nSTACKBOX_PREFIX=24\nSTACKBOX_PORTS=\"{}\"\nSTACKBOX_PEERS=\"{}\"\n",
        spec.hostname(),
        spec.address(),
        ports.join(" "),
        peers.join(" ")
    )
}

/// What a new VM gets besides its spec.
#[derive(Default)]
pub struct NodeSetup<'a> {
    pub exposure: Option<&'a crate::model::Exposure>,
    /// `(port_base offset, guest port)` to publish; only the VM serving the APIs has any.
    pub public_ports: Vec<(u16, u16)>,
    /// The stackbox's other VMs, as (address, hostname).
    pub peers: Vec<(String, String)>,
    pub links: Vec<LinkConf>,
    /// msb snapshot to boot from instead of the golden image, as when restoring a stackbox.
    pub source: Option<String>,
    /// Host PCI devices (`DDDD:BB:DD.F`) passed through to this VM with VFIO; only compute-1
    /// of a stackbox that has them gets any. A VM with devices also gets the `stackbox-vfio` unit.
    pub pci_devices: Vec<String>,
}

// msb's default root is an overlay; Docker's overlayfs cannot nest on it,
// so container storage gets its own ext4 disks.
pub(super) const CONTAINER_DIRS: &[&str] = &["/var/lib/docker", "/var/lib/containerd"];

/// Stackbox files every guest carries, whatever its distribution: (path, contents, mode).
const GUEST_FILES: &[(&str, &str, &str)] = &[
    (
        "/usr/local/sbin/stackbox-identity",
        include_str!("../../guest/stackbox/stackbox-identity"),
        "755",
    ),
    (
        "/etc/systemd/system/stackbox-identity.service",
        include_str!("../../guest/stackbox/stackbox-identity.service"),
        "644",
    ),
    (
        "/usr/local/sbin/stackbox-hostname",
        include_str!("../../guest/stackbox/stackbox-hostname"),
        "755",
    ),
    (
        "/etc/systemd/system/stackbox-hostname.service",
        include_str!("../../guest/stackbox/stackbox-hostname.service"),
        "644",
    ),
    (
        "/usr/local/sbin/stackbox-external",
        include_str!("../../guest/stackbox/stackbox-external"),
        "755",
    ),
    (
        "/etc/systemd/system/stackbox-external.service",
        include_str!("../../guest/stackbox/stackbox-external.service"),
        "644",
    ),
    (
        "/etc/systemd/system/disable-ipv6.service",
        include_str!("../../guest/stackbox/disable-ipv6.service"),
        "644",
    ),
    (
        "/etc/systemd/system/stackbox-link@.service",
        include_str!("../../guest/stackbox/stackbox-link@.service"),
        "644",
    ),
];
const GUEST_UNITS: &str = "disable-ipv6 stackbox-identity stackbox-hostname stackbox-external";

/// Files of a compute VM that has passed-through PCI devices: the helper that binds them to
/// `vfio-pci` before the guest's own drivers claim them, and the unit that runs it at boot.
const VFIO_FILES: &[(&str, &str, &str)] = &[
    (
        "/usr/local/sbin/stackbox-vfio-prepare",
        include_str!("../../guest/stackbox/stackbox-vfio-prepare"),
        "755",
    ),
    (
        "/etc/systemd/system/stackbox-vfio.service",
        include_str!("../../guest/stackbox/stackbox-vfio.service"),
        "644",
    ),
];

/// Write the VFIO helper and enable its unit in a compute VM that has passed-through devices.
/// The unit runs before Docker, so nova-compute's container finds the devices bound to
/// `vfio-pci`; starting it now binds the devices already present in this boot.
pub async fn install_vfio_files(sandbox: &Sandbox) -> Result<()> {
    let fs = sandbox.fs();
    for (path, contents, _) in VFIO_FILES {
        fs.write(path, *contents).await?;
    }
    let modes: String = VFIO_FILES
        .iter()
        .map(|(path, _, mode)| format!("chmod {mode} {path}\n"))
        .collect();
    run(
        sandbox,
        &format!(
            "set -e\n{modes}systemctl daemon-reload\nsystemctl enable stackbox-vfio.service\nsystemctl start stackbox-vfio.service"
        ),
    )
    .await?;
    Ok(())
}

/// Write the stackbox's files and enable their units. Stackbox VMs get them when created and again
/// when deployed, so a redeploy also upgrades a stackbox made by an older build.
pub async fn install_guest_files(sandbox: &Sandbox) -> Result<()> {
    let fs = sandbox.fs();
    for (path, contents, _) in GUEST_FILES {
        fs.write(path, *contents).await?;
    }
    let modes: String = GUEST_FILES
        .iter()
        .map(|(path, _, mode)| format!("chmod {mode} {path}\n"))
        .collect();
    run(
        sandbox,
        &format!(
            "set -e\nmkdir -p /etc/stackbox\n{modes}systemctl daemon-reload\nsystemctl enable {GUEST_UNITS}\nsystemctl start disable-ipv6"
        ),
    )
    .await?;
    Ok(())
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize, utoipa::ToSchema)]
pub struct StackboxNode {
    pub name: String,
    pub role: Option<Role>,
    /// A multi-node compute VM's position, from 1.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub index: Option<u8>,
    pub status: String,
    pub memory_mib: u32,
    /// Live eth0 IPv4 for new all-in-one guests; legacy nodes may retain an overlay address.
    pub ip: Option<String>,
}

/// A stackbox's all-in-one VM is named after the stackbox; other VMs carry their role and index
/// (`<stackbox>-control`, `<stackbox>-compute-2`), as legacy multi-VM nodes carry their role.
pub fn node_name(stackbox_id: &str, role: Role, index: Option<u8>) -> String {
    match super::template::suffix(role, index) {
        None => stackbox_id.to_owned(),
        Some(suffix) => format!("{stackbox_id}-{suffix}"),
    }
}

/// All-in-one VMs created before they were named after their stackbox.
pub const LEGACY_ALL_IN_ONE_SUFFIX: &str = "-all-in-one";

pub fn is_node_name(stackbox_id: &str, role: Role, index: Option<u8>, name: &str) -> bool {
    name == node_name(stackbox_id, role, index)
        || (role == Role::AllInOne
            && name.strip_suffix(LEGACY_ALL_IN_ONE_SUFFIX) == Some(stackbox_id))
}

/// A VM's persisted labels and memory (MiB). msb cannot describe a VM it fails to read, so
/// that reads as no labels and no memory, as it did before the SDK moved to live lookups.
pub async fn settings(sandbox: &Sandbox) -> (BTreeMap<String, String>, u32) {
    match sandbox.config().await.map(|config| config.settings) {
        Ok(SandboxInspectedSettings::Local(spec)) => {
            (spec.labels.clone(), spec.resources.memory_mib)
        }
        _ => (BTreeMap::new(), 0),
    }
}

/// The link binary guests run (see [`crate::link::guest`]).
const GUEST_AGENT: &str = "/usr/local/sbin/stackbox-agent";

/// Give the guest its links: the Linux stackbox-agent at `binary`, each link's configuration and
/// a running `stackbox-link@<dev>` unit. Returns once every device is up with its address.
pub async fn install_links(sandbox: &Sandbox, binary: &Path, links: &[LinkConf]) -> Result<()> {
    if links.is_empty() {
        return Ok(());
    }
    let bytes = tokio::fs::read(binary)
        .await
        .with_context(|| format!("read the guest stackbox-agent {}", binary.display()))?;
    sandbox.fs().write(GUEST_AGENT, bytes).await?;
    for link in links {
        link.validate()?;
        sandbox
            .fs()
            .write(&LinkConf::path(&link.dev), link.render())
            .await?;
    }
    let devs: Vec<String> = links.iter().map(|l| l.dev.clone()).collect();
    let units: Vec<String> = devs.iter().map(|d| format!("stackbox-link@{d}")).collect();
    run(
        sandbox,
        &format!(
            r#"set -e
chmod 755 {GUEST_AGENT}
chmod 600 /etc/stackbox/link-*.conf
systemctl daemon-reload
# A VM booted with its links off (a restored snapshot) may have got stackbox-identity's dummy
# stackbox0, which would keep the link from creating its TAP.
for dev in {devs}; do
  if ip -d link show "$dev" 2>/dev/null | grep -w dummy >/dev/null; then ip link del "$dev"; fi
done
systemctl enable {units}
systemctl restart {units}
for dev in {devs}; do
  for _ in $(seq 100); do ip link show "$dev" >/dev/null 2>&1 && break; sleep 0.1; done
  ip link show "$dev" >/dev/null
done"#,
            units = units.join(" "),
            devs = devs.join(" "),
        ),
    )
    .await
    .context("start the guest's links")?;
    Ok(())
}

pub async fn create_node(
    stackbox_id: &str,
    spec: NodeSpec,
    setup: &NodeSetup<'_>,
) -> Result<StackboxNode> {
    // Fail before creating anything when the guests' link binary is missing.
    let guest_agent = if setup.links.is_empty() {
        None
    } else {
        Some(crate::link::host::guest_agent()?)
    };
    let name = node_name(stackbox_id, spec.role, spec.index);
    let hostname = spec.hostname();
    let os = golden::DEFAULT_OS;
    // The golden snapshot, or the stackbox snapshot restored from, supplies the root and container disks.
    let image = match &setup.source {
        Some(reference) => reference.clone(),
        None => golden::ensure(os, &spec).await?,
    };
    let build = || {
        let builder = Sandbox::builder(&name)
            .override_snapshot(&image)
            .cpus(spec.cpus)
            .memory(spec.memory_mib)
            .init("/sbin/init")
            .hostname(&hostname)
            .network(|n| {
                let mut n = n.policy(NetworkPolicy::from_profiles([
                    NetworkProfile::Public,
                    NetworkProfile::Host,
                ]));
                if let Some(exposure) = setup.exposure {
                    for (offset, guest_port) in &setup.public_ports {
                        n = n.port_bind(exposure.bind, exposure.port_base + offset, *guest_port);
                    }
                }
                n
            })
            .label(STACKBOX_LABEL, stackbox_id)
            .label(ROLE_LABEL, spec.role.as_str())
            .label(OS_LABEL, os.id);
        let builder = match spec.index {
            Some(index) => builder.label(INDEX_LABEL, index.to_string()),
            None => builder,
        };
        // Passed-through devices need the virtio IOMMU so the guest can isolate them by group.
        let builder = if setup.pci_devices.is_empty() {
            builder
        } else {
            builder
                .pci_devices(setup.pci_devices.iter().cloned())
                .virtio_iommu(true)
        };
        builder.detached(true)
    };
    let sandbox = create_nested(build)
        .await
        .with_context(|| format!("create {name}"))?;
    let boot = async {
        wait_until_booted(&sandbox).await?;
        // msb cannot patch files into a VM made from a snapshot, so they follow the boot.
        let identity = identity_conf(&spec, !setup.public_ports.is_empty(), &setup.peers);
        sandbox.fs().write(IDENTITY_CONF, identity).await?;
        install_guest_files(&sandbox).await?;
        if !setup.pci_devices.is_empty() {
            install_vfio_files(&sandbox).await?;
        }
        if let Some(binary) = &guest_agent {
            install_links(&sandbox, binary, &setup.links).await?;
        }
        run(&sandbox, "systemctl restart stackbox-identity stackbox-hostname").await?;
        let ip = run(&sandbox, "ip -4 -o addr show dev eth0 scope global | awk 'NR==1 { split($4, a, \"/\"); print a[1] }'").await?;
        let ip = ip.trim().parse::<std::net::Ipv4Addr>().context("eth0 has no valid IPv4 address")?;
        Ok::<_, anyhow::Error>(ip.to_string())
    }.await;
    detach(&sandbox).await;
    let ip = boot.with_context(|| {
        format!("{name} boot failed; VM is left running for inspection and explicit deletion")
    })?;
    Ok(StackboxNode {
        name,
        role: Some(spec.role),
        index: spec.index,
        status: "running".into(),
        memory_mib: spec.memory_mib,
        ip: Some(ip),
    })
}

/// msb leaves nested virtualization off unless asked. Nova uses the guest's `/dev/kvm`
/// when the host can nest; otherwise the guest boots without it and Nova falls back to TCG.
pub async fn create_nested(build: impl Fn() -> SandboxBuilder) -> MicrosandboxResult<Sandbox> {
    match build().nested_virt(true).create().await {
        // Host support is checked before anything is created, so a plain retry is safe.
        Err(MicrosandboxError::Unsupported {
            op: Operation::SandboxStart,
            reason: UnsupportedReason::NotAvailable(_),
        }) => build().nested_virt(false).create().await,
        result => result,
    }
}

pub async fn describe_node(sandbox: &Sandbox) -> StackboxNode {
    let (labels, memory_mib) = settings(sandbox).await;
    let status = match sandbox.status().await {
        Ok(status) => format!("{status:?}").to_lowercase(),
        Err(_) => "unknown".into(),
    };
    StackboxNode {
        name: sandbox.name().to_owned(),
        role: labels.get(ROLE_LABEL).and_then(|r| r.parse().ok()),
        index: labels.get(INDEX_LABEL).and_then(|i| i.parse().ok()),
        status,
        memory_mib,
        ip: labels.get(OVERLAY_IP_LABEL).cloned(),
    }
}

pub async fn destroy_node(sandbox: &Sandbox) -> Result<()> {
    sandbox
        .destroy_with(DestroyOptions {
            force: true,
            ..Default::default()
        })
        .await
        .with_context(|| format!("destroy {}", sandbox.name()))
}

/// Let go of a VM this process only looked at or created. Its cleanup stays armed until then,
/// so a failure is reported and the VM is left as it is.
pub async fn detach(sandbox: &Sandbox) {
    if let Err(error) = sandbox.detach().await {
        eprintln!("{}: detach: {error}", sandbox.name());
    }
}

/// A VM by name, prepared for guest commands. msb prepares the agent connection on first
/// use; `ping` does it now, so a VM that cannot be reached fails here.
pub async fn connect(name: &str) -> Result<Sandbox> {
    let sandbox = Sandbox::get(name)
        .await
        .with_context(|| format!("find {name}"))?;
    sandbox
        .ping()
        .await
        .with_context(|| format!("connect {name}"))?;
    Ok(sandbox)
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
    eprintln!("{}: systemd {}", sandbox.name(), state.trim());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{GUEST_ADDRESS, GUEST_HOSTNAME, IDENTITY_CONF, identity_conf};

    #[test]
    fn longest_valid_stackbox_id_fits_the_hostname_label_limit() {
        let id = "a".repeat(48);
        crate::model::validate_id(&id).unwrap();
        let name = super::node_name(&id, super::Role::AllInOne, None);
        assert!(name.len() <= 63);
        assert_eq!(name, id);
        assert!(crate::model::validate_id(&"a".repeat(49)).is_err());
    }

    #[test]
    fn all_in_one_vms_are_named_after_their_stackbox_and_keep_legacy_names() {
        use super::{Role, is_node_name, node_name};
        assert_eq!(node_name("dev", Role::AllInOne, None), "dev");
        assert_eq!(node_name("dev", Role::Lb, None), "dev-lb");
        assert_eq!(node_name("dev", Role::Control, None), "dev-control");
        assert_eq!(node_name("dev", Role::Compute, Some(2)), "dev-compute-2");
        assert!(is_node_name("dev", Role::AllInOne, None, "dev"));
        assert!(is_node_name("dev", Role::AllInOne, None, "dev-all-in-one"));
        assert!(!is_node_name("dev", Role::AllInOne, None, "dev2"));
        assert!(!is_node_name("dev", Role::Lb, None, "dev"));
        assert!(is_node_name("dev", Role::Compute, Some(1), "dev-compute-1"));
        assert!(!is_node_name(
            "dev",
            Role::Compute,
            Some(1),
            "dev-compute-2"
        ));
    }

    #[test]
    fn hostname_mapping_follows_eth0_and_survives_msb_hosts_regeneration() {
        use std::io::{Read, Seek, SeekFrom};
        let dir = tempfile::tempdir().unwrap();
        let hosts = dir.path().join("hosts");
        let script = include_str!("../../guest/stackbox/stackbox-hostname")
            .replace("/etc/hosts", "\"$STACKBOX_TEST_HOSTS\"")
            .replace(IDENTITY_CONF, "\"$STACKBOX_TEST_HOSTS.identity\"");
        let initial = "127.0.0.1 localhost demo-all-in-one\n::1 localhost ip6-localhost demo-all-in-one\n192.0.2.1 custom-host\n";
        std::fs::write(&hosts, initial).unwrap();
        let mut observer = std::fs::File::open(&hosts).unwrap();
        for (i, address) in ["192.168.127.2", "192.168.127.2", "192.168.127.6"]
            .iter()
            .enumerate()
        {
            if i == 2 {
                // msb rewrites this same bind-mounted file when restarting the guest.
                std::fs::write(&hosts, initial).unwrap();
            }
            let output = std::process::Command::new("bash")
                .args(["-c", &format!("hostname() {{ echo demo-all-in-one; }}\nip() {{ echo '2: eth0 inet {address}/30 scope global eth0'; }}\n{script}")])
                .env("STACKBOX_TEST_HOSTS", &hosts)
                .output().unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            let text = std::fs::read_to_string(&hosts).unwrap();
            let mappings: Vec<_> = text
                .lines()
                .filter_map(|line| {
                    let fields: Vec<_> = line.split_whitespace().collect();
                    fields
                        .get(1..)
                        .unwrap_or_default()
                        .contains(&"demo-all-in-one")
                        .then(|| fields[0])
                })
                .collect();
            assert_eq!(mappings, [*address]);
            assert!(text.contains("192.0.2.1\tcustom-host"));
            assert!(text.contains("::1\tlocalhost\tip6-localhost"));
            assert_eq!(text.matches("# BEGIN STACKBOX HOSTNAME").count(), 1);
            observer.seek(SeekFrom::Start(0)).unwrap();
            let mut observed = String::new();
            observer.read_to_string(&mut observed).unwrap();
            assert_eq!(observed, text);
        }
        // With a fixed identity the hostname maps to the service address, whatever eth0 holds.
        let peers = [("10.254.0.11".to_string(), "stackbox-compute-1".to_string())];
        std::fs::write(
            dir.path().join("hosts.identity"),
            identity_conf(&super::super::ALL_IN_ONE, true, &peers),
        )
        .unwrap();
        let output = std::process::Command::new("bash")
            .args(["-c", &format!("hostname() {{ echo {GUEST_HOSTNAME}; }}\nip() {{ echo '2: eth0 inet 172.16.0.14/30 scope global eth0'; }}\n{script}")])
            .env("STACKBOX_TEST_HOSTS", &hosts)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let text = std::fs::read_to_string(&hosts).unwrap();
        assert!(text.contains(&format!(
            "{GUEST_ADDRESS} {GUEST_HOSTNAME}\n10.254.0.11 stackbox-compute-1\n"
        )));
        assert!(!text.contains("172.16.0.14"));
    }

    #[test]
    fn identity_dnats_every_published_guest_port() {
        let conf = identity_conf(&super::super::ALL_IN_ONE, true, &[]);
        assert!(conf.contains("STACKBOX_ADDRESS=10.254.0.2\n"));
        assert!(conf.contains("STACKBOX_PORTS=\"5000 9292 8780 80 9696 8774 8004 8000 9311 9001 9511 9998 9999 3306 5672 15672\"\n"));
        // The nft set is rendered from STACKBOX_PORTS by bash substitution.
        let output = std::process::Command::new("bash")
            .args([
                "-c",
                &format!("{conf}\necho \"{{ ${{STACKBOX_PORTS// /, }} }}\""),
            ])
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&output.stdout),
            "{ 5000, 9292, 8780, 80, 9696, 8774, 8004, 8000, 9311, 9001, 9511, 9998, 9999, 3306, 5672, 15672 }\n"
        );
        // A compute VM publishes nothing; stackbox-identity then skips the DNAT table.
        let compute = super::super::NodeSpec {
            index: Some(1),
            ..super::super::COMPUTE
        };
        let conf = identity_conf(&compute, false, &[]);
        assert!(conf.contains("STACKBOX_HOSTNAME=stackbox-compute-1\n"));
        assert!(conf.contains("STACKBOX_ADDRESS=10.254.0.11\n"));
        assert!(conf.contains("STACKBOX_PORTS=\"\"\n"));
        assert!(
            include_str!("../../guest/stackbox/stackbox-identity")
                .contains("[ -n \"${STACKBOX_PORTS:-}\" ] || exit 0")
        );
    }
}
