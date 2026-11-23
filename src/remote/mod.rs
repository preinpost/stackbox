pub mod agent;
pub mod artifact;
pub mod backend;
mod cache;
pub mod dial;
pub mod host;
pub mod hub;
pub mod job;
pub mod link;
pub mod protocol;
mod secrets;
pub mod ssh;
#[cfg(test)]
pub(crate) mod test_server;
pub use secrets::{SealedCredentials, Vault};

use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use zeroize::{Zeroize, ZeroizeOnDrop};

/// The msb release hosts run. The `microsandbox` crate in Cargo.toml is pinned to the same version.
pub const MSB_VERSION: &str = "0.8.1";

/// Whether an installed msb predates [`MSB_VERSION`], so preparation upgrades it in place.
pub fn msb_outdated(version: &str) -> bool {
    let parse = |v: &str| {
        v.split('.')
            .map(|part| part.parse::<u64>().ok())
            .collect::<Option<Vec<_>>>()
    };
    matches!((parse(version), parse(MSB_VERSION)), (Some(have), Some(want)) if have < want)
}

#[derive(Clone, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct SshTarget {
    pub address: String,
    #[serde(default = "ssh_port")]
    pub port: u16,
}
fn ssh_port() -> u16 {
    22
}
impl SshTarget {
    pub fn validate(&self) -> Result<()> {
        let address = &self.address;
        ensure!(self.port != 0, "SSH port must be 1-65535");
        let ip = address.parse::<std::net::IpAddr>().is_ok();
        let dns = !address.is_empty()
            && address.len() <= 253
            && address.split('.').all(|label| {
                !label.is_empty()
                    && label.len() <= 63
                    && !label.starts_with('-')
                    && !label.ends_with('-')
                    && label
                        .bytes()
                        .all(|c| c.is_ascii_alphanumeric() || c == b'-')
            });
        ensure!(
            ip || dns,
            "address must be an IP address or DNS hostname without a scheme or port"
        );
        Ok(())
    }
}

/// Write-only input. Never include this type in a host/job response or derive Debug.
#[derive(Clone, Serialize, Deserialize, ToSchema, Zeroize, ZeroizeOnDrop)]
#[serde(tag = "method", rename_all = "snake_case", deny_unknown_fields)]
pub enum Credentials {
    Password {
        #[schema(write_only)]
        password: String,
    },
    PrivateKey {
        #[schema(write_only)]
        private_key: String,
        #[serde(default)]
        #[schema(write_only)]
        passphrase: Option<String>,
    },
}
impl Credentials {
    pub fn method(&self) -> AuthMethod {
        match self {
            Self::Password { .. } => AuthMethod::Password,
            Self::PrivateKey { .. } => AuthMethod::PrivateKey,
        }
    }
    pub fn validate(&self) -> Result<()> {
        match self {
            Self::Password { password } => ensure!(
                !password.is_empty() && password.len() <= 4096,
                "password must be 1-4096 bytes"
            ),
            Self::PrivateKey {
                private_key,
                passphrase,
            } => {
                ensure!(
                    !private_key.is_empty() && private_key.len() <= 65536,
                    "private key must be 1-65536 bytes"
                );
                ensure!(
                    passphrase.as_ref().is_none_or(|p| p.len() <= 4096),
                    "key passphrase must be at most 4096 bytes"
                );
                russh::keys::decode_secret_key(private_key, passphrase.as_deref()).map_err(
                    |_| anyhow::anyhow!("invalid or unsupported private key / passphrase"),
                )?;
            }
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum AuthMethod {
    Password,
    PrivateKey,
}

/// Register the machine the coordinator runs on as a host.
#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct RegisterLocalHost {
    #[serde(default = "local_host_id")]
    pub id: String,
}
fn local_host_id() -> String {
    "local".into()
}

#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct RegisterHost {
    pub id: String,
    pub address: String,
    #[serde(default = "ssh_port")]
    pub port: u16,
    pub username: String,
    /// SHA256 fingerprint verified by the operator using an independent trusted source.
    pub fingerprint: String,
    pub credentials: Credentials,
}
/// One authenticated SSH check. Credentials are not stored.
#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ConnectionTestRequest {
    pub address: String,
    #[serde(default = "ssh_port")]
    pub port: u16,
    pub username: String,
    pub fingerprint: String,
    pub credentials: Credentials,
}
impl ConnectionTestRequest {
    pub fn validate(&self) -> Result<()> {
        RegisterHost {
            id: "probe".into(),
            address: self.address.clone(),
            port: self.port,
            username: self.username.clone(),
            fingerprint: self.fingerprint.clone(),
            credentials: self.credentials.clone(),
        }
        .validate()
    }
}
#[derive(Serialize, ToSchema)]
pub struct ConnectionTest {
    /// `uname -srm` from the remote host. No credential material.
    pub identity: String,
}
impl RegisterHost {
    pub fn validate(&self) -> Result<()> {
        crate::model::validate_id(&self.id)?;
        ensure!(
            self.id != "fingerprint",
            "host id 'fingerprint' is reserved"
        );
        SshTarget {
            address: self.address.clone(),
            port: self.port,
        }
        .validate()?;
        ensure!(
            !self.username.is_empty()
                && self.username.len() <= 64
                && self
                    .username
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b)),
            "invalid SSH username"
        );
        use base64::Engine;
        let value = self.fingerprint.strip_prefix("SHA256:").unwrap_or_default();
        ensure!(
            base64::engine::general_purpose::STANDARD_NO_PAD
                .decode(value)
                .is_ok_and(|v| v.len() == 32),
            "expected an OpenSSH SHA256 host fingerprint"
        );
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, ToSchema)]
pub struct HostKey {
    pub fingerprint: String,
    pub algorithm: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum HostStatus {
    Registered,
    Checking,
    Preparing,
    NeedsSetup,
    RuntimeReady,
    Ready,
    Failed,
}

#[derive(Clone, Debug, Serialize, Deserialize, ToSchema)]
pub struct HostInspection {
    pub os: String,
    pub arch: String,
    pub compatible: bool,
    pub kvm_available: bool,
    pub sudo_available: bool,
    pub memory_total_mib: u64,
    pub memory_available_mib: u64,
    pub disk_available_mib: u64,
    pub msb_version: Option<String>,
    pub msb_compatible: bool,
    pub docker_available: bool,
    /// This build's golden image is built; otherwise the first farm on the host builds it.
    pub golden_image_available: bool,
    /// `FARM_LIBKRUNFW_PATH` the host's agent boots farm VMs from, or the kernel host
    /// preparation installed under `~/.local/share/farm/libkrunfw`; absent for the stock kernel.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custom_kernel: Option<String>,
    /// Why an installed kernel was found but is not used, such as one built for another msb.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custom_kernel_reason: Option<String>,
    /// The farm kernel ran the host's verification VM and exposed the `mtty` mediated
    /// device type, which an OpenStack 2025.1 or newer `mtty` GPU farm binds as a vGPU.
    #[serde(default)]
    pub mtty_available: bool,
    /// What the verification VM read from its own kernel about the GPU stack. Our kernel
    /// carries VFIO, mdev and the `mtty` sample driver built in, so there are no modules to
    /// load; these fields say whether those built-in features came up in the booted guest.
    /// Absent until a preparation from a coordinator that probes them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gpu_kernel: Option<GpuKernel>,
    /// PCI devices on this host bound to `vfio-pci`: the only ones a farm may pass through.
    /// Only devices in such a state are listed, so a host's chipset devices stay out of its
    /// inspection and `state.json`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pci_devices: Vec<HostPciDevice>,
    /// Whether this host's `msb` takes `--pci` and `--iommu` on `msb run`. The upstream and
    /// fork builds share one version string, so the version cannot tell them apart; the
    /// inspection asks the binary's own help. Absent from older inspections, which read false.
    #[serde(default)]
    pub msb_pci_capable: bool,
    /// Whether this host can take a PCI passthrough farm now: the farm kernel is installed,
    /// `msb` supports `--pci`/`--iommu` (`msb_pci_capable`), `/dev/vfio/vfio` exists and at
    /// least one of `pci_devices` is `vfio_ready`. The guest kernel is the farm's, so the
    /// stock kernel answers no.
    #[serde(default)]
    pub pci_passthrough_available: bool,
    pub vm_boot_verified: bool,
    pub checked_at: u64,
}

/// A host PCI device a farm may pass through. Only `vfio-pci`-bound devices are reported.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct HostPciDevice {
    /// Lowercase `DDDD:BB:DD.F`, such as `0000:04:00.3`.
    pub bdf: String,
    /// Four lowercase hex digits.
    pub vendor_id: String,
    /// Four lowercase hex digits.
    pub product_id: String,
    /// Six lowercase hex digits: base class, subclass and programming interface.
    pub class: String,
    /// The IOMMU group, or absent when the device has none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub iommu_group: Option<u32>,
    /// The bound driver; `vfio-pci` for every listed device.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub driver: Option<String>,
    /// Every device in the IOMMU group is either unbound or bound to `vfio-pci`, so the group
    /// can be handed to a guest without taking a device another driver uses.
    #[serde(default)]
    pub group_exclusive: bool,
    /// `driver` is `vfio-pci`, the group is exclusive and `/dev/vfio/<group>` is readable and
    /// writable by the agent. Only such a device is accepted for passthrough.
    #[serde(default)]
    pub vfio_ready: bool,
}

/// Whether an IOMMU group can be handed to a guest: every member is unbound or bound to
/// `vfio-pci`. `members` holds each member's bound driver, `None` when unbound.
pub fn group_exclusive(members: &[Option<&str>]) -> bool {
    members
        .iter()
        .all(|driver| driver.is_none() || *driver == Some("vfio-pci"))
}

/// Whether a host can take a PCI passthrough farm now (`pci_passthrough_available`): the farm
/// kernel is installed (its guest kernel carries `vfio-pci`), the host's `msb` takes
/// `--pci`/`--iommu`, `/dev/vfio/vfio` exists, and at least one `vfio-pci` device is
/// `vfio_ready`. The stock kernel answers no.
pub fn pci_passthrough_ready(
    custom_kernel: bool,
    msb_pci_capable: bool,
    vfio_device: bool,
    devices: &[HostPciDevice],
) -> bool {
    custom_kernel
        && msb_pci_capable
        && vfio_device
        && devices.iter().any(|device| device.vfio_ready)
}

/// Whether `msb run --help` mentions both `--pci` and `--iommu`, the flags a PCI farm's VMs
/// need. `inspect.sh` makes the same check on the same text.
pub fn msb_help_supports_pci(help: &str) -> bool {
    help.contains("--pci") && help.contains("--iommu")
}
/// The kernel a verification VM booted: farm's installed kernel, or microsandbox's stock one.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum GpuKernelKind {
    Custom,
    #[default]
    Stock,
}
/// What the guest kernel of a verification VM reported about the GPU stack. Every flag is
/// read inside that VM, so it says the kernel came up with the feature, not that the host
/// has a module for it: farm's kernel is built with VFIO and mdev included.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct GpuKernel {
    #[serde(default)]
    pub kernel: GpuKernelKind,
    /// Installed kernel's id: the directory name under `~/.local/share/farm/libkrunfw`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kernel_id: Option<String>,
    /// Why the stock kernel booted although a kernel is installed, such as one built for
    /// another msb.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// `/dev/vfio/vfio` exists in the guest.
    #[serde(default)]
    pub vfio: bool,
    /// `/sys/class/mdev_bus` exists in the guest, so the mdev bus registered.
    #[serde(default)]
    pub vfio_mdev: bool,
    /// `/sys/class/mdev_bus/mtty` exists in the guest: the sample parent device registered.
    #[serde(default)]
    pub mtty_registered: bool,
    /// Every mediated device type the guest kernel offers, as in
    /// `/sys/class/mdev_bus/*/mdev_supported_types/*`.
    #[serde(default)]
    pub mdev_types: Vec<MdevType>,
    /// `uname -r` in the guest.
    #[serde(default)]
    pub kernel_release: String,
}
impl GpuKernel {
    /// Name the kernel the probe booted: farm's installed one, or the stock kernel. `reason`
    /// explains a stock boot although an installation exists.
    pub fn for_kernel(
        mut self,
        custom: bool,
        kernel_id: Option<String>,
        reason: Option<String>,
    ) -> Self {
        self.kernel = if custom {
            GpuKernelKind::Custom
        } else {
            GpuKernelKind::Stock
        };
        self.kernel_id = kernel_id;
        self.reason = reason;
        self
    }
    /// Whether the guest can bind an `mtty-1` vGPU: the parent device and that type exist.
    /// The instance count is reported separately, as an exhausted device is still present.
    pub fn mtty_available(&self) -> bool {
        self.mtty_registered && self.mdev_types.iter().any(|t| t.name == "mtty-1")
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct MdevType {
    pub name: String,
    pub available_instances: u64,
}
impl HostInspection {
    /// The farm-agent platform this host needs, as in [`AgentArtifactInfo::platform`].
    pub fn platform(&self) -> String {
        format!("{}-{}", self.os, self.arch)
    }
    pub fn runtime_ready(&self) -> bool {
        self.compatible && self.kvm_available && self.msb_compatible
    }
    /// Carry what the last verification boot learned about the booted kernel into an
    /// inspection that only read the host: a plain check cannot boot a guest. `same_kernel`
    /// says that boot's kernel and msb are still the host's.
    pub fn keep_verification(&mut self, previous: &Self, same_kernel: bool) {
        if !same_kernel {
            return;
        }
        self.mtty_available |= previous.mtty_available;
        if self.gpu_kernel.is_none() {
            self.gpu_kernel.clone_from(&previous.gpu_kernel);
        }
    }
    pub fn status(&self) -> HostStatus {
        if self.runtime_ready() && self.vm_boot_verified {
            HostStatus::Ready
        } else if self.runtime_ready() {
            HostStatus::RuntimeReady
        } else {
            HostStatus::NeedsSetup
        }
    }
}

/// How the coordinator reaches a host's farm-agent.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum HostKind {
    /// A registered server, reached over SSH with a pinned host key.
    #[default]
    Ssh,
    /// The machine the coordinator runs on; its agent is the coordinator's own binary.
    Local,
    /// A host whose agent dials in to the coordinator (`farm agent connect`).
    Agent,
}

#[derive(Clone, Debug, Serialize, Deserialize, ToSchema)]
pub struct RemoteHost {
    pub id: String,
    #[serde(default)]
    pub kind: HostKind,
    /// SSH address; `localhost` for the local host.
    pub address: String,
    /// SSH port, user and host key; unused for the local host.
    pub port: u16,
    pub username: String,
    pub fingerprint: String,
    /// `None` for the local host, which needs no credential.
    pub auth_method: Option<AuthMethod>,
    pub status: HostStatus,
    pub inspection: Option<HostInspection>,
    #[serde(default)]
    pub agent: Option<InstalledAgent>,
    /// Derived from coordinator reservations, not a live hypervisor limit.
    #[serde(default)]
    pub capacity: Option<HostCapacity>,
    pub active_job: Option<String>,
    pub error: Option<String>,
    /// Whether a dial-in host's agent holds its connection now. Not stored.
    #[serde(default)]
    pub connected: bool,
    /// The coordinator's last attempt to reach an SSH host; absent until one ran.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reachability: Option<HostReachability>,
    pub created_at: u64,
    pub updated_at: u64,
}

/// What the coordinator's last attempts to reach an SSH host found: its inventory scan, or a
/// plain connection check for a host that runs no farm.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct HostReachability {
    /// Whether the host is considered reachable. One failed attempt is not enough to say it is
    /// gone; it takes two in a row.
    pub reachable: bool,
    /// When the coordinator last tried the host, in Unix seconds; 0 if never.
    #[serde(default)]
    pub checked_at: u64,
    /// When the host last answered, in Unix seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_ok_at: Option<u64>,
    /// Why the last attempt failed; absent after a successful one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default)]
    pub consecutive_failures: u32,
}

impl Default for HostReachability {
    fn default() -> Self {
        Self {
            reachable: true,
            checked_at: 0,
            last_ok_at: None,
            error: None,
            consecutive_failures: 0,
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub struct StoredHost {
    pub host: RemoteHost,
    pub credential_ref: String,
    /// `None` for the local host.
    pub credentials: Option<SealedCredentials>,
    /// SHA-256 of a dial-in host's agent token; the token itself is shown once.
    #[serde(default)]
    pub token_sha256: Option<String>,
}

/// Register a host whose agent will dial in.
#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct RegisterAgentHost {
    pub id: String,
}
/// The new host and the token its agent connects with. The token is not shown again.
#[derive(Serialize, ToSchema)]
pub struct AgentHostToken {
    pub host: RemoteHost,
    pub token: String,
}
impl StoredHost {
    pub fn aad(&self) -> String {
        serde_json::to_string(&(
            &self.credential_ref,
            &self.host.id,
            &self.host.address,
            self.host.port,
            &self.host.username,
            &self.host.fingerprint,
        ))
        .unwrap()
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum HostJobKind {
    Check,
    Prepare,
    InstallAgent,
    /// Delete this coordinator's farms and leftover VMs on the host, then unregister it.
    Purge,
}

#[derive(Clone, Debug, Serialize, Deserialize, ToSchema)]
pub struct AgentArtifactInfo {
    pub sha256: String,
    /// `linux`, `macos` or `windows`; agents recorded before this field are Linux ones.
    #[serde(default = "linux")]
    pub os: String,
    pub arch: String,
    pub protocol: u32,
    /// Old installed agents remain usable for legacy inventory and deletion.
    #[serde(default)]
    pub supports_single_vm: bool,
    /// Revision the artifact was built from; empty for artifacts older than this field.
    #[serde(default)]
    pub build_id: String,
}
fn linux() -> String {
    "linux".into()
}
impl AgentArtifactInfo {
    /// `<os>-<arch>`, such as `linux-x86_64` or `macos-aarch64`.
    pub fn platform(&self) -> String {
        format!("{}-{}", self.os, self.arch)
    }
}
#[derive(Clone, Debug, Serialize, Deserialize, ToSchema)]
pub struct InstalledAgent {
    pub artifact: AgentArtifactInfo,
    pub verified_at: u64,
}
#[derive(Clone, Debug, Serialize, Deserialize, ToSchema)]
pub struct HostCapacity {
    pub memory_budget_mib: u64,
    pub memory_reserved_mib: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, ToSchema)]
pub struct HostJobEvent {
    pub at: u64,
    pub message: String,
}
#[derive(Clone, Debug, Serialize, Deserialize, ToSchema)]
pub struct HostJob {
    pub id: String,
    pub host_id: String,
    pub kind: HostJobKind,
    pub status: crate::model::JobStatus,
    pub events: Vec<HostJobEvent>,
    pub error: Option<String>,
    pub created_at: u64,
    pub finished_at: Option<u64>,
    /// A purge that also deletes the snapshots this host keeps.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub delete_snapshots: bool,
}

#[cfg(test)]
mod msb_pci_tests {
    use super::msb_help_supports_pci;

    #[test]
    fn only_a_run_help_with_both_flags_counts_as_pci_capable() {
        let fork = "Usage: msb run [OPTIONS]\n  --pci <BDF>\n  --iommu\n";
        assert!(msb_help_supports_pci(fork));
        assert!(
            !msb_help_supports_pci("Usage: msb run\n  --pci <BDF>\n"),
            "no --iommu"
        );
        assert!(
            !msb_help_supports_pci("Usage: msb run\n  --iommu\n"),
            "no --pci"
        );
        assert!(
            !msb_help_supports_pci(""),
            "a failed help run reads as upstream"
        );
    }
}

#[cfg(test)]
mod msb_version_tests {
    use super::*;

    #[test]
    fn only_older_releases_are_outdated() {
        assert!(msb_outdated("0.7.8"));
        assert!(msb_outdated("0.6.99"));
        assert!(!msb_outdated(MSB_VERSION));
        assert!(msb_outdated("0.7.14"));
        assert!(!msb_outdated("0.8.2"));
        assert!(!msb_outdated("1.0.0"));
        assert!(!msb_outdated("0.7.x"));
    }

    #[test]
    fn an_inspection_from_before_the_custom_kernel_still_reads() {
        // A state file written before farm knew about custom kernels has neither field.
        let older = serde_json::json!({
            "os": "linux",
            "arch": "aarch64",
            "compatible": true,
            "kvm_available": true,
            "sudo_available": false,
            "memory_total_mib": 65536,
            "memory_available_mib": 32768,
            "disk_available_mib": 500000,
            "msb_version": "0.7.13",
            "msb_compatible": true,
            "docker_available": true,
            "golden_image_available": true,
            "vm_boot_verified": true,
            "checked_at": 1,
        });
        let inspection: HostInspection = serde_json::from_value(older).unwrap();
        assert!(inspection.runtime_ready() && inspection.vm_boot_verified);
        assert_eq!(inspection.custom_kernel, None);
        assert!(!inspection.mtty_available);
        assert!(inspection.gpu_kernel.is_none());
        // An unknown field from a newer coordinator does not sink the whole inspection.
        let newer = serde_json::json!({
            "os": "linux", "arch": "aarch64", "compatible": true, "kvm_available": true,
            "sudo_available": false, "memory_total_mib": 1, "memory_available_mib": 1,
            "disk_available_mib": 1, "msb_version": null, "msb_compatible": false,
            "docker_available": false, "golden_image_available": false,
            "custom_kernel": "/srv/kernels/libkrunfw.so.5.6.1",
            "mtty_available": true, "vm_boot_verified": false, "checked_at": 2,
            "gpu_kernel": {
                "kernel": "custom", "kernel_id": "0123456789abcdef",
                "vfio": true, "vfio_mdev": true, "mtty_registered": true,
                "mdev_types": [{"name": "mtty-1", "available_instances": 24}],
                "kernel_release": "6.12.113"
            },
            "a_field_from_later": 1,
        });
        let inspection: HostInspection = serde_json::from_value(newer).unwrap();
        assert_eq!(
            inspection.custom_kernel.as_deref(),
            Some("/srv/kernels/libkrunfw.so.5.6.1")
        );
        assert!(inspection.mtty_available);
        let gpu = inspection.gpu_kernel.unwrap();
        assert_eq!(gpu.kernel, GpuKernelKind::Custom);
        assert_eq!(gpu.kernel_id.as_deref(), Some("0123456789abcdef"));
        assert!(gpu.mtty_available());
        assert_eq!(gpu.mdev_types[0].available_instances, 24);
        assert_eq!(gpu.kernel_release, "6.12.113");
    }

    #[test]
    fn a_check_keeps_the_gpu_probe_only_for_the_kernel_it_read() {
        let probed = GpuKernel {
            kernel: GpuKernelKind::Custom,
            kernel_id: Some("0123456789abcdef".into()),
            vfio: true,
            vfio_mdev: true,
            mtty_registered: true,
            mdev_types: vec![MdevType {
                name: "mtty-1".into(),
                available_instances: 24,
            }],
            kernel_release: "6.12.113".into(),
            ..Default::default()
        };
        let mut previous = HostInspection {
            custom_kernel: Some("/k/0123456789abcdef/libkrunfw.so.5.6.1".into()),
            mtty_available: true,
            gpu_kernel: Some(probed.clone()),
            ..serde_json::from_value(serde_json::json!({
                "os": "linux", "arch": "x86_64", "compatible": true,
                "kvm_available": true, "sudo_available": false,
                "memory_total_mib": 1, "memory_available_mib": 1,
                "disk_available_mib": 1, "msb_version": "0.7.13",
                "msb_compatible": true, "docker_available": false,
                "golden_image_available": false, "vm_boot_verified": true,
                "checked_at": 1
            }))
            .unwrap()
        };
        let mut checked = previous.clone();
        checked.mtty_available = false;
        checked.gpu_kernel = None;
        checked.keep_verification(&previous, true);
        assert!(checked.mtty_available);
        assert_eq!(checked.gpu_kernel.as_ref(), Some(&probed));
        // Another kernel was never booted here, so its GPU state is unknown.
        previous.custom_kernel = Some("/k/fedcba9876543210/libkrunfw.so.5.6.1".into());
        let mut checked = previous.clone();
        checked.mtty_available = false;
        checked.gpu_kernel = None;
        checked.keep_verification(&previous, false);
        assert!(!checked.mtty_available);
        assert!(checked.gpu_kernel.is_none());
    }

    #[test]
    fn hosts_run_the_msb_the_crate_is_built_against() {
        // The crate is the fork's pinned git revision; its msb is the fork's build of MSB_VERSION.
        let cargo = include_str!("../../Cargo.toml");
        assert!(cargo.contains("microsandbox = { git = \"https://github.com/preinpost/microsandbox.git\", rev = \"da3aa7956d34bd29bcea69140be642b2d00156a2\""));
        assert!(cargo.contains("msb_krun = { git = \"https://github.com/preinpost/libkrun.git\", rev = \"97f7accd2f6cf78d04b34f1c8badf6624ef83e5d\""));
        assert!(cargo.contains("msb_krun_utils = { git = \"https://github.com/preinpost/libkrun.git\", rev = \"97f7accd2f6cf78d04b34f1c8badf6624ef83e5d\""));
        assert_eq!(MSB_VERSION, "0.8.1");
    }
}
