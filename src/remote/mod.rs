pub mod agent;
pub mod artifact;
pub mod backend;
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
pub const MSB_VERSION: &str = "0.7.13";

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
    pub vm_boot_verified: bool,
    pub checked_at: u64,
}
impl HostInspection {
    /// The farm-agent platform this host needs, as in [`AgentArtifactInfo::platform`].
    pub fn platform(&self) -> String {
        format!("{}-{}", self.os, self.arch)
    }
    pub fn runtime_ready(&self) -> bool {
        self.compatible && self.kvm_available && self.msb_compatible
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
    pub created_at: u64,
    pub updated_at: u64,
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
mod msb_version_tests {
    use super::*;

    #[test]
    fn only_older_releases_are_outdated() {
        assert!(msb_outdated("0.7.8"));
        assert!(msb_outdated("0.6.99"));
        assert!(!msb_outdated(MSB_VERSION));
        assert!(!msb_outdated("0.7.14"));
        assert!(!msb_outdated("1.0.0"));
        assert!(!msb_outdated("0.7.x"));
    }

    #[test]
    fn hosts_run_the_msb_the_crate_is_built_against() {
        assert!(
            include_str!("../../Cargo.toml")
                .contains(&format!("microsandbox = {{ version = \"={MSB_VERSION}\""))
        );
    }
}
