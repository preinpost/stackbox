use crate::{
    farm::{ALL_IN_ONE, FarmNode, NodeSpec},
    kolla::Phase,
};
use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    net::IpAddr,
    time::{SystemTime, UNIX_EPOCH},
};
use utoipa::ToSchema;

pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, ToSchema)]
#[serde(default, deny_unknown_fields)]
pub struct PlanRequest {
    pub components: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, ToSchema)]
pub struct Plan {
    pub components: Vec<String>,
    pub nodes: Vec<NodeSpec>,
    pub memory_mib: u64,
    /// Includes both container disks for each container host. Sparse disks consume less initially.
    pub disk_mib: u64,
    pub warnings: Vec<String>,
}

#[derive(Clone, Debug, Serialize, ToSchema)]
pub struct Component {
    pub id: String,
    pub required: bool,
    pub depends_on: Vec<String>,
    pub requires_data_plane: bool,
    /// False while the service is listed for planning but cannot be deployed yet.
    pub available: bool,
}
#[derive(Clone, Debug, Serialize, ToSchema)]
pub struct Catalog {
    pub components: Vec<Component>,
    pub constraints: Vec<String>,
}

/// A service published on the all-in-one guest at `port_base + offset`.
pub struct PublicPort {
    pub offset: u16,
    pub guest_port: u16,
    pub path: &'static str,
}

/// Host ports reserved per farm: `[port_base, port_base + PUBLIC_PORT_BLOCK)`.
///
/// Twenty leaves room for the twelve optional OpenStack APIs beyond the six published
/// today, and still yields 450 blocks in `15000..24000` — more than a host's memory budget
/// can hold. Every [`PublicPort::offset`] must stay below this, and offsets are append-only
/// so an existing farm's public URLs never move when the catalog grows. Raising the block
/// also changes the grid of port blocks, so `allocate_ports` rejects any overlap, not just
/// an equal base.
pub const PUBLIC_PORT_BLOCK: u16 = 20;

/// A data store installed in every farm and published for integrations.
pub struct InfraPort {
    pub id: &'static str,
    pub label: &'static str,
    pub offset: u16,
    pub guest_port: u16,
}
/// MariaDB and RabbitMQ come with `enable_mariadb`/`enable_rabbitmq`, so every farm owns
/// these ports. Their offsets continue the service catalog's 0..5.
pub const INFRA_PORTS: &[InfraPort] = &[
    InfraPort {
        id: "mariadb",
        label: "MariaDB",
        offset: 6,
        guest_port: 3306,
    },
    InfraPort {
        id: "rabbitmq",
        label: "RabbitMQ (AMQP)",
        offset: 7,
        guest_port: 5672,
    },
    InfraPort {
        id: "rabbitmq-management",
        label: "RabbitMQ 관리 UI",
        offset: 8,
        guest_port: 15672,
    },
];
pub struct ComponentDef {
    pub id: &'static str,
    pub required: bool,
    pub depends_on: &'static [&'static str],
    pub requires_data_plane: bool,
    pub available: bool,
    /// Guest memory added by each optional service process group.
    pub footprint_mib: u32,
    pub public: Option<PublicPort>,
}
/// Catalog order is also deployment and display order; dependencies come first.
pub const COMPONENTS: &[ComponentDef] = &[
    ComponentDef {
        id: "keystone",
        required: true,
        depends_on: &[],
        requires_data_plane: false,
        available: true,
        footprint_mib: 0,
        public: Some(PublicPort {
            offset: 0,
            guest_port: 5000,
            path: "/v3",
        }),
    },
    ComponentDef {
        id: "glance",
        required: true,
        depends_on: &["keystone"],
        requires_data_plane: false,
        available: true,
        footprint_mib: 0,
        public: Some(PublicPort {
            offset: 1,
            guest_port: 9292,
            path: "",
        }),
    },
    ComponentDef {
        id: "placement",
        required: false,
        depends_on: &["keystone"],
        requires_data_plane: false,
        available: true,
        footprint_mib: 256,
        public: Some(PublicPort {
            offset: 2,
            guest_port: 8780,
            path: "",
        }),
    },
    ComponentDef {
        id: "horizon",
        required: false,
        depends_on: &["keystone"],
        requires_data_plane: false,
        available: true,
        footprint_mib: 512,
        public: Some(PublicPort {
            offset: 3,
            guest_port: 80,
            path: "/",
        }),
    },
    ComponentDef {
        id: "neutron",
        required: false,
        depends_on: &["keystone"],
        requires_data_plane: true,
        available: true,
        // Measured: neutron-server, rpc-server and periodic worker use ~780 MiB; the four agents ~800 MiB.
        footprint_mib: 1024 + 1280,
        public: Some(PublicPort {
            offset: 4,
            guest_port: 9696,
            path: "",
        }),
    },
    ComponentDef {
        id: "nova",
        required: false,
        depends_on: &["keystone", "glance", "placement", "neutron"],
        requires_data_plane: true,
        available: true,
        // nova-api, scheduler, conductor, compute and libvirt, plus a small guest.
        footprint_mib: 2048,
        public: Some(PublicPort {
            offset: 5,
            guest_port: 8774,
            path: "/v2.1",
        }),
    },
];
/// Farms recorded before component selection, and imported farms, run the original pair.
pub const BASE_COMPONENTS: &[&str] = &["keystone", "glance"];

pub fn catalog() -> Catalog {
    Catalog {
        components: COMPONENTS
            .iter()
            .map(|c| Component {
                id: c.id.into(),
                required: c.required,
                depends_on: c.depends_on.iter().map(|d| (*d).into()).collect(),
                requires_data_plane: c.requires_data_plane,
                available: c.available,
            })
            .collect(),
        constraints: vec![
            "Each farm runs all selected OpenStack services in one all-in-one VM.".into(),
            "Open vSwitch uses the userspace datapath, so tenant traffic is slower than on a kernel datapath.".into(),
            "Nova uses KVM when microsandbox exposes nested /dev/kvm in the guest, otherwise qemu TCG with slow instance boots.".into(),
        ],
    }
}

/// Required services plus the request and every dependency, in catalog order.
pub fn resolve_components(requested: &[String]) -> Result<Vec<&'static ComponentDef>> {
    let mut wanted: Vec<&str> = COMPONENTS
        .iter()
        .filter(|c| c.required)
        .map(|c| c.id)
        .collect();
    let mut queue: Vec<&str> = Vec::new();
    for id in requested {
        let Some(def) = COMPONENTS.iter().find(|c| c.id == id) else {
            bail!("unsupported component '{id}'");
        };
        queue.push(def.id);
    }
    while let Some(id) = queue.pop() {
        if wanted.contains(&id) {
            continue;
        }
        wanted.push(id);
        let def = COMPONENTS.iter().find(|c| c.id == id).unwrap();
        queue.extend(def.depends_on);
    }
    let resolved: Vec<_> = COMPONENTS
        .iter()
        .filter(|c| wanted.contains(&c.id))
        .collect();
    if let Some(c) = resolved.iter().find(|c| !c.available) {
        bail!("component '{}' is not deployable yet", c.id);
    }
    Ok(resolved)
}

pub fn plan(request: &PlanRequest) -> Result<Plan> {
    let components = resolve_components(&request.components)?;
    let mut node = ALL_IN_ONE;
    node.memory_mib += components.iter().map(|c| c.footprint_mib).sum::<u32>();
    let nodes = vec![node];
    Ok(Plan {
        components: components.iter().map(|c| c.id.to_owned()).collect(),
        memory_mib: nodes.iter().map(|n| u64::from(n.memory_mib)).sum(),
        disk_mib: nodes
            .iter()
            .map(|n| u64::from(n.disk_mib) + 2 * u64::from(n.container_disk_mib.unwrap_or(0)))
            .sum(),
        nodes,
        warnings: catalog().constraints,
    })
}

#[derive(Clone, Debug, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct CreateRequest {
    pub id: String,
    /// Registered remote host. Omit for the local coordinator.
    #[serde(default)]
    pub host_id: Option<String>,
    #[serde(default)]
    pub components: Vec<String>,
    #[serde(default = "default_host")]
    pub external_host: String,
    #[serde(default)]
    pub owner: Option<String>,
    /// UTC Unix seconds. Metadata only in this version; no automatic deletion.
    #[serde(default)]
    pub expires_at: Option<u64>,
    /// Start from this snapshot instead of an empty VM. Its services replace `components`.
    #[serde(default)]
    pub snapshot_id: Option<String>,
}
fn default_host() -> String {
    "localhost".into()
}
impl CreateRequest {
    pub fn validate(&self) -> Result<Plan> {
        validate_id(&self.id)?;
        if let Some(id) = &self.host_id {
            validate_id(id)?;
        }
        // Keep VM hostname + role within Linux's 64 character hostname limit.
        if self.owner.as_ref().is_some_and(|o| o.len() > 256) {
            bail!("owner must be at most 256 bytes");
        }
        if self.expires_at.is_some_and(|t| t <= now()) {
            bail!("expires_at must be in the future");
        }
        if let Some(snapshot) = &self.snapshot_id {
            validate_id(snapshot)?;
            if self.host_id.is_some() {
                bail!("snapshots restore on the local coordinator only");
            }
            if !self.components.is_empty() {
                bail!(
                    "a farm restored from a snapshot runs the snapshot's services; omit components"
                );
            }
        }
        let host = &self.external_host;
        if host.is_empty()
            || host.len() > 253
            || !host
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b".-".contains(&c))
            || host.starts_with('-')
            || host.ends_with('-')
            || host.split('.').any(|s| s.is_empty())
        {
            bail!("external_host must be an IPv4 address or DNS hostname without a scheme or port");
        }
        plan(&PlanRequest {
            components: self.components.clone(),
        })
    }
}
pub fn validate_id(id: &str) -> Result<()> {
    if id.is_empty()
        || id.len() > 48
        || !id
            .bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
        || id.starts_with('-')
        || id.ends_with('-')
    {
        bail!(
            "farm id must be 1-48 lowercase letters, digits or hyphens, starting and ending with a letter or digit"
        );
    }
    Ok(())
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, ToSchema)]
#[serde(default, deny_unknown_fields)]
pub struct DeployRequest {
    pub from: Phase,
}

#[derive(Clone, Debug, Serialize, Deserialize, ToSchema)]
pub struct Exposure {
    pub host: String,
    #[schema(value_type=String)]
    pub bind: IpAddr,
    pub port_base: u16,
}
impl Exposure {
    /// Public URLs of the given components; `None` means a farm without a recorded plan.
    pub fn urls(&self, components: Option<&[String]>) -> BTreeMap<String, String> {
        COMPONENTS
            .iter()
            .filter(|c| match components {
                Some(list) => list.iter().any(|id| id == c.id),
                None => BASE_COMPONENTS.contains(&c.id),
            })
            .filter_map(|c| {
                let port = c.public.as_ref()?;
                Some((
                    c.id.to_owned(),
                    format!(
                        "http://{}:{}{}",
                        self.host,
                        self.port_base + port.offset,
                        port.path
                    ),
                ))
            })
            .collect()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum FarmStatus {
    Creating,
    Created,
    Deploying,
    /// Waiting for a deployed farm's services to answer after its VM started from disk
    Settling,
    Ready,
    Failed,
    Broken,
    Deleting,
    Deleted,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum JobKind {
    Create,
    Deploy,
    Delete,
    /// Freeze the farm's VM into a snapshot
    Snapshot,
    /// Create the farm's VM from a snapshot
    Restore,
    /// Check that a deployed farm's services answer again, without deploying
    Settle,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum JobStatus {
    Queued,
    Running,
    Succeeded,
    Failed,
    Interrupted,
}
impl JobStatus {
    pub fn active(self) -> bool {
        matches!(self, Self::Queued | Self::Running)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, ToSchema)]
pub struct FarmRecord {
    pub id: String,
    #[serde(default)]
    pub host_id: Option<String>,
    #[serde(default)]
    pub remote_generation: Option<String>,
    #[serde(default)]
    pub agent_sha256: Option<String>,
    #[serde(default)]
    pub observation_error: Option<String>,
    #[serde(default)]
    pub last_seen_at: Option<u64>,
    pub status: FarmStatus,
    pub plan: Option<Plan>,
    pub exposure: Option<Exposure>,
    pub owner: Option<String>,
    pub expires_at: Option<u64>,
    pub nodes: Vec<FarmNode>,
    pub phase: Option<Phase>,
    pub error: Option<String>,
    pub active_job: Option<String>,
    pub created_at: u64,
    pub updated_at: u64,
    /// Snapshot this farm was restored from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot_id: Option<String>,
}
impl FarmRecord {
    pub fn memory_mib(&self) -> u64 {
        self.plan
            .as_ref()
            .map(|p| p.memory_mib)
            .unwrap_or_else(|| self.nodes.iter().map(|n| u64::from(n.memory_mib)).sum())
    }
}
#[derive(Clone, Debug, Serialize, Deserialize, ToSchema)]
pub struct Job {
    pub id: String,
    pub farm_id: String,
    pub kind: JobKind,
    pub status: JobStatus,
    pub phase: Option<Phase>,
    pub error: Option<String>,
    pub created_at: u64,
    pub started_at: Option<u64>,
    pub finished_at: Option<u64>,
    /// Snapshot captured by a snapshot job, or restored by a restore job.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot_id: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct SnapshotRequest {
    /// Snapshot ID: 1-48 lowercase letters, digits or hyphens.
    pub id: String,
    #[serde(default)]
    pub note: Option<String>,
}
impl SnapshotRequest {
    pub fn validate(&self) -> Result<()> {
        validate_snapshot_id(&self.id)?;
        if self.note.as_ref().is_some_and(|n| n.len() > 512) {
            bail!("note must be at most 512 bytes");
        }
        Ok(())
    }
}
#[derive(Clone, Debug, Default, Serialize, Deserialize, ToSchema)]
#[serde(default, deny_unknown_fields)]
pub struct ExportRequest {
    /// Absolute archive path on the coordinator; defaults to `<state dir>/exports/<id>.msb`.
    pub path: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ImportRequest {
    /// Absolute path of a `.msb` archive on the coordinator.
    pub path: String,
    /// Snapshot ID to import as; defaults to the ID recorded in the archive.
    #[serde(default)]
    pub id: Option<String>,
}
/// Architecture the running daemon and its golden image are built for.
pub fn current_arch() -> String {
    std::env::consts::ARCH.into()
}

/// Host OS of the running daemon; a farm frozen elsewhere says where it came from.
pub fn current_platform() -> String {
    std::env::consts::OS.into()
}

pub fn validate_snapshot_id(id: &str) -> Result<()> {
    validate_id(id).map_err(|_| {
        anyhow::anyhow!(
            "snapshot id must be 1-48 lowercase letters, digits or hyphens, starting and ending with a letter or digit"
        )
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum SnapshotStatus {
    Creating,
    Ready,
    Exporting,
    Importing,
    Deleting,
    Failed,
}
#[derive(Clone, Debug, Serialize, Deserialize, ToSchema)]
pub struct SnapshotArchive {
    pub path: String,
    pub size_bytes: u64,
    pub exported_at: u64,
}
#[derive(Clone, Debug, Serialize, Deserialize, ToSchema)]
pub struct SnapshotRecord {
    pub id: String,
    pub status: SnapshotStatus,
    /// CPU architecture of the frozen guest; its disks cannot boot on another one.
    #[serde(default = "current_arch")]
    pub arch: String,
    /// Host OS that froze it (`macos`, `linux`); recorded for the operator, not enforced.
    #[serde(default)]
    pub platform: Option<String>,
    /// Farm the snapshot was frozen from; `None` when imported from an archive.
    pub source_farm: Option<String>,
    pub plan: Plan,
    /// Whether OpenStack was deployed when frozen. Restored farms then start `ready`.
    pub deployed: bool,
    /// Public ports the source farm's catalog was registered with; restores rewrite them.
    pub source_exposure: Exposure,
    pub note: Option<String>,
    /// msb snapshot reference; set once captured or imported.
    pub reference: Option<String>,
    pub size_bytes: Option<u64>,
    /// Most recent export.
    pub archive: Option<SnapshotArchive>,
    pub error: Option<String>,
    pub created_at: u64,
    pub updated_at: u64,
}

/// Snapshot metadata kept in msb labels, which travel inside exported archives.
pub mod snapshot_labels {
    pub const ID: &str = "farm.snapshot.id";
    pub const SOURCE: &str = "farm.snapshot.source";
    pub const COMPONENTS: &str = "farm.snapshot.components";
    pub const DEPLOYED: &str = "farm.snapshot.deployed";
    pub const EXPOSURE_HOST: &str = "farm.snapshot.exposure.host";
    pub const EXPOSURE_PORT_BASE: &str = "farm.snapshot.exposure.port-base";
    pub const RELEASE: &str = "farm.snapshot.openstack-release";
    pub const ARCH: &str = "farm.snapshot.arch";
    pub const PLATFORM: &str = "farm.snapshot.platform";
    pub const NOTE: &str = "farm.snapshot.note";
}

#[derive(Clone, Debug, Serialize, Deserialize, ToSchema)]
pub struct LogEntry {
    pub id: u64,
    pub timestamp: u64,
    pub job_id: String,
    pub phase: Option<Phase>,
    pub message: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, ToSchema)]
pub struct Endpoints {
    pub services: BTreeMap<String, String>,
    /// Admin credentials; returned only by the explicit endpoints call, never persisted.
    pub credentials: BTreeMap<String, String>,
    /// Data stores published next to the service APIs, for integrations.
    #[serde(default)]
    pub datastores: Vec<Datastore>,
}

/// One published data store; ports are the host's, not the guest's per-start address.
#[derive(Clone, Debug, Serialize, Deserialize, ToSchema)]
pub struct Datastore {
    pub id: String,
    pub label: String,
    /// Client protocol: `mysql`, `amqp` or `http`.
    pub protocol: String,
    pub host: String,
    pub port: u16,
    pub username: Option<String>,
    pub password: Option<String>,
    /// RabbitMQ vhost; unused by MariaDB.
    pub vhost: Option<String>,
    /// Connection URL without the password.
    pub url: String,
    /// Databases a client can open, excluding MariaDB's own schemas.
    #[serde(default)]
    pub databases: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::farm::Role;
    #[test]
    fn rejects_unsafe_names_and_unknown_components() {
        for id in ["../escape", "a;true", "UPPER", "", "-a"] {
            assert!(validate_id(id).is_err());
        }
        for unsupported in ["swift", "cinder"] {
            assert!(
                plan(&PlanRequest {
                    components: vec![unsupported.into()],
                })
                .is_err()
            );
        }
        let p = plan(&PlanRequest::default()).unwrap();
        assert_eq!(p.memory_mib, 3072);
        assert_eq!(p.nodes.len(), 1);
        assert_eq!(p.components, ["keystone", "glance"]);
        assert!(p.disk_mib > p.nodes.iter().map(|n| u64::from(n.disk_mib)).sum());
    }

    #[test]
    fn optional_services_share_the_single_guest_and_public_ports_stay_stable() {
        let request = |components: &[&str]| PlanRequest {
            components: components.iter().map(|c| (*c).into()).collect(),
        };
        let both = plan(&request(&["placement", "horizon", "keystone"])).unwrap();
        assert_eq!(
            both.components,
            ["keystone", "glance", "placement", "horizon"]
        );
        assert_eq!(both.nodes.len(), 1);
        assert_eq!(both.nodes[0].role, Role::AllInOne);
        assert_eq!(both.memory_mib, 3072 + 256 + 512);
        let all = plan(&request(&["placement", "horizon", "neutron"])).unwrap();
        assert_eq!(all.nodes.len(), 1);
        assert_eq!(all.memory_mib, 6144);
        assert_eq!(all.disk_mib, 20480 + 2 * 32768);
        let exposure = Exposure {
            host: "farm.example".into(),
            bind: [0, 0, 0, 0].into(),
            port_base: 15020,
        };
        let urls = exposure.urls(Some(&all.components));
        assert_eq!(urls["placement"], "http://farm.example:15022");
        assert_eq!(urls["horizon"], "http://farm.example:15023/");
        assert_eq!(urls["neutron"], "http://farm.example:15024");
        let with_nova = plan(&request(&["nova"])).unwrap();
        assert_eq!(
            with_nova.components,
            ["keystone", "glance", "placement", "neutron", "nova"]
        );
        assert_eq!(with_nova.memory_mib, 3072 + 256 + 2304 + 2048);
        assert_eq!(
            exposure.urls(Some(&with_nova.components))["nova"],
            "http://farm.example:15025/v2.1"
        );
        assert_eq!(
            exposure.urls(None).keys().collect::<Vec<_>>(),
            ["glance", "keystone"]
        );
    }

    #[test]
    fn public_port_offsets_stay_inside_one_farm_block() {
        let mut offsets: Vec<u16> = COMPONENTS
            .iter()
            .filter_map(|c| c.public.as_ref().map(|p| p.offset))
            .chain(INFRA_PORTS.iter().map(|p| p.offset))
            .collect();
        let published = offsets.len();
        offsets.sort_unstable();
        offsets.dedup();
        assert_eq!(
            offsets.len(),
            published,
            "each published port needs its own offset"
        );
        let highest = *offsets.last().expect("the catalog publishes keystone");
        assert!(
            highest < PUBLIC_PORT_BLOCK,
            "offset {highest} would spill into the next farm's block of {PUBLIC_PORT_BLOCK} ports; raise PUBLIC_PORT_BLOCK or reuse an offset"
        );
        // Offsets 9..19 are free for optional services such as cinder, swift and heat.
        assert_eq!(
            highest, 8,
            "the published set ends with the RabbitMQ console"
        );
        assert_eq!(PUBLIC_PORT_BLOCK, 20, "the block is capped at twenty ports");
        // The grid still holds more farms than one host's memory can admit.
        let blocks = (24_000 - 15_000) / PUBLIC_PORT_BLOCK;
        assert_eq!(blocks, 450);
        assert!(u16::try_from(published).unwrap() < PUBLIC_PORT_BLOCK);
    }

    #[test]
    fn plans_recorded_with_a_resource_profile_still_load() {
        let mut legacy = serde_json::to_value(plan(&PlanRequest::default()).unwrap()).unwrap();
        legacy["profile"] = "full".into();
        let restored: Plan = serde_json::from_value(legacy).unwrap();
        assert_eq!(restored.components, ["keystone", "glance"]);
    }
}
