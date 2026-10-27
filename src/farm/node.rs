use anyhow::{Context, Result};
use microsandbox::sandbox::{DestroyOptions, SandboxHandle};
use microsandbox::{NetworkPolicy, NetworkProfile, Sandbox};

use super::template::{BASE_IMAGE, NodeSpec, Role};
use crate::shell::run;

pub const FARM_LABEL: &str = "farm";
pub const ROLE_LABEL: &str = "farm.role";
pub const OVERLAY_IP_LABEL: &str = "farm.overlay.ip";

/// Every farm guest gets the same hostname and service address, so a snapshot
/// restores into another farm (and a restart into a new msb slot) unchanged.
pub const GUEST_HOSTNAME: &str = "farm-aio";
pub const GUEST_ADDRESS: &str = "10.254.0.2";
pub const GUEST_INTERFACE: &str = "farm0";
pub const IDENTITY_CONF: &str = "/etc/farm/identity.conf";

/// Guest ports DNATed from eth0 to the service address: every port the host publishes.
pub fn identity_conf() -> String {
    let ports: Vec<String> = crate::model::COMPONENTS
        .iter()
        .filter_map(|c| c.public.as_ref().map(|p| p.guest_port.to_string()))
        .chain(
            crate::model::INFRA_PORTS
                .iter()
                .map(|p| p.guest_port.to_string()),
        )
        .collect();
    format!(
        "FARM_HOSTNAME={GUEST_HOSTNAME}\nFARM_ADDRESS={GUEST_ADDRESS}\nFARM_PREFIX=24\nFARM_PORTS=\"{}\"\n",
        ports.join(" ")
    )
}

// msb's default root is an overlay; Docker's overlayfs cannot nest on it,
// so container storage gets its own ext4 disks.
const CONTAINER_DIRS: &[&str] = &["/var/lib/docker", "/var/lib/containerd"];

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize, utoipa::ToSchema)]
pub struct FarmNode {
    pub name: String,
    pub role: Option<Role>,
    pub status: String,
    pub memory_mib: u32,
    /// Live eth0 IPv4 for new all-in-one guests; legacy nodes may retain an overlay address.
    pub ip: Option<String>,
}

/// A farm's all-in-one VM is named after the farm; legacy multi-VM nodes carry their role.
pub fn node_name(farm_id: &str, role: Role) -> String {
    match role {
        Role::AllInOne => farm_id.to_owned(),
        role => format!("{farm_id}-{role}"),
    }
}

/// All-in-one VMs created before they were named after their farm.
pub const LEGACY_ALL_IN_ONE_SUFFIX: &str = "-all-in-one";

pub fn is_node_name(farm_id: &str, role: Role, name: &str) -> bool {
    name == node_name(farm_id, role)
        || (role == Role::AllInOne && name.strip_suffix(LEGACY_ALL_IN_ONE_SUFFIX) == Some(farm_id))
}

pub fn label(handle: &SandboxHandle, key: &str) -> Option<String> {
    handle.config().ok()?.spec.labels.get(key).cloned()
}

pub async fn create_node(
    farm_id: &str,
    spec: NodeSpec,
    exposure: Option<&crate::model::Exposure>,
    public_ports: &[(u16, u16)],
) -> Result<FarmNode> {
    let name = node_name(farm_id, spec.role);
    let mut builder = Sandbox::builder(&name)
        .image(BASE_IMAGE)
        .cpus(spec.cpus)
        .memory(spec.memory_mib)
        .root_disk(spec.disk_mib)
        .init("/sbin/init")
        .hostname(GUEST_HOSTNAME)
        .network(|n| {
            let mut n = n.policy(NetworkPolicy::from_profiles([
                NetworkProfile::Public,
                NetworkProfile::Host,
            ]));
            if let Some(exposure) = exposure {
                for (offset, guest_port) in public_ports {
                    n = n.port_bind(exposure.bind, exposure.port_base + offset, *guest_port);
                }
            }
            n
        })
        .patch(|p| p.text(IDENTITY_CONF, identity_conf(), Some(0o644), true))
        .patch(|p| {
            p.text(
                "/usr/local/sbin/farm-identity",
                include_str!("../../docker/farm-identity"),
                Some(0o755),
                true,
            )
        })
        .patch(|p| {
            p.text(
                "/etc/systemd/system/farm-identity.service",
                include_str!("../../docker/farm-identity.service"),
                Some(0o644),
                true,
            )
        })
        .patch(|p| {
            p.text(
                "/usr/local/sbin/farm-hostname",
                include_str!("../../docker/farm-hostname"),
                Some(0o755),
                true,
            )
        })
        .patch(|p| {
            p.text(
                "/etc/systemd/system/farm-hostname.service",
                include_str!("../../docker/farm-hostname.service"),
                Some(0o644),
                true,
            )
        })
        .label(FARM_LABEL, farm_id)
        .label(ROLE_LABEL, spec.role.as_str())
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
    let boot = async {
        wait_until_booted(&sandbox).await?;
        // Golden images built before the identity unit lack nftables.
        run(&sandbox, "rpm -q nftables >/dev/null || dnf install -y -q nftables; systemctl daemon-reload && systemctl enable farm-identity farm-hostname && systemctl restart farm-identity farm-hostname").await?;
        let ip = run(&sandbox, "ip -4 -o addr show dev eth0 scope global | awk 'NR==1 { split($4, a, \"/\"); print a[1] }'").await?;
        let ip = ip.trim().parse::<std::net::Ipv4Addr>().context("eth0 has no valid IPv4 address")?;
        Ok::<_, anyhow::Error>(ip.to_string())
    }.await;
    sandbox.detach().await;
    let ip = boot.with_context(|| {
        format!("{name} boot failed; VM is left running for inspection and explicit deletion")
    })?;
    Ok(FarmNode {
        name,
        role: Some(spec.role),
        status: "running".into(),
        memory_mib: spec.memory_mib,
        ip: Some(ip),
    })
}

pub fn describe_node(handle: &SandboxHandle) -> FarmNode {
    FarmNode {
        name: handle.name().to_owned(),
        role: label(handle, ROLE_LABEL).and_then(|r| r.parse().ok()),
        status: format!("{:?}", handle.status_snapshot()).to_lowercase(),
        memory_mib: handle
            .config()
            .map(|c| c.spec.resources.memory_mib)
            .unwrap_or(0),
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
    eprintln!("{}: systemd {}", sandbox.name(), state.trim());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{GUEST_ADDRESS, GUEST_HOSTNAME, IDENTITY_CONF, identity_conf};

    #[test]
    fn longest_valid_farm_id_fits_the_hostname_label_limit() {
        let id = "a".repeat(48);
        crate::model::validate_id(&id).unwrap();
        let name = super::node_name(&id, super::Role::AllInOne);
        assert!(name.len() <= 63);
        assert_eq!(name, id);
        assert!(crate::model::validate_id(&"a".repeat(49)).is_err());
    }

    #[test]
    fn all_in_one_vms_are_named_after_their_farm_and_keep_legacy_names() {
        use super::{Role, is_node_name, node_name};
        assert_eq!(node_name("dev", Role::AllInOne), "dev");
        assert_eq!(node_name("dev", Role::Lb), "dev-lb");
        assert!(is_node_name("dev", Role::AllInOne, "dev"));
        assert!(is_node_name("dev", Role::AllInOne, "dev-all-in-one"));
        assert!(!is_node_name("dev", Role::AllInOne, "dev2"));
        assert!(!is_node_name("dev", Role::Lb, "dev"));
    }

    #[test]
    fn hostname_mapping_follows_eth0_and_survives_msb_hosts_regeneration() {
        use std::io::{Read, Seek, SeekFrom};
        let dir = tempfile::tempdir().unwrap();
        let hosts = dir.path().join("hosts");
        let script = include_str!("../../docker/farm-hostname")
            .replace("/etc/hosts", "\"$FARM_TEST_HOSTS\"")
            .replace(IDENTITY_CONF, "\"$FARM_TEST_HOSTS.identity\"");
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
                .env("FARM_TEST_HOSTS", &hosts)
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
            assert_eq!(text.matches("# BEGIN FARM HOSTNAME").count(), 1);
            observer.seek(SeekFrom::Start(0)).unwrap();
            let mut observed = String::new();
            observer.read_to_string(&mut observed).unwrap();
            assert_eq!(observed, text);
        }
        // With a fixed identity the hostname maps to the service address, whatever eth0 holds.
        std::fs::write(dir.path().join("hosts.identity"), identity_conf()).unwrap();
        let output = std::process::Command::new("bash")
            .args(["-c", &format!("hostname() {{ echo {GUEST_HOSTNAME}; }}\nip() {{ echo '2: eth0 inet 172.16.0.14/30 scope global eth0'; }}\n{script}")])
            .env("FARM_TEST_HOSTS", &hosts)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let text = std::fs::read_to_string(&hosts).unwrap();
        assert!(text.contains(&format!("{GUEST_ADDRESS} {GUEST_HOSTNAME}\n")));
        assert!(!text.contains("172.16.0.14"));
    }

    #[test]
    fn identity_dnats_every_published_guest_port() {
        let conf = identity_conf();
        assert!(conf.contains("FARM_ADDRESS=10.254.0.2\n"));
        assert!(conf.contains("FARM_PORTS=\"5000 9292 8780 80 9696 8774 3306 5672 15672\"\n"));
        // The nft set is rendered from FARM_PORTS by bash substitution.
        let output = std::process::Command::new("bash")
            .args([
                "-c",
                &format!("{conf}\necho \"{{ ${{FARM_PORTS// /, }} }}\""),
            ])
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&output.stdout),
            "{ 5000, 9292, 8780, 80, 9696, 8774, 3306, 5672, 15672 }\n"
        );
    }
}
