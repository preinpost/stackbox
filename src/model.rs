use crate::{
    farm::{ALL_IN_ONE, COMPUTE, CONTROL, FarmNode, MAX_COMPUTES, NodeSpec},
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
    /// Compute VMs beside a control VM, up to 3; 0 runs everything in one all-in-one VM.
    /// A multi-node farm always runs Nova and Neutron.
    pub computes: u8,
    /// Size of every compute VM; omitted for the default. Only for farms with compute VMs.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compute: Option<ComputeSize>,
    /// OpenStack release to install; omitted installs this coordinator's own release.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub release: Option<String>,
    /// VMs of a farm without computes; omitted keeps the all-in-one default.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub layout: Option<FarmLayout>,
}

/// How a farm without compute VMs lays its service out, so that "one VM" and "a control
/// VM that gains computes later" are never the same choice.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "kebab-case")]
pub enum FarmLayout {
    /// Every selected service in one VM; it cannot gain compute VMs.
    AllInOne,
    /// A control VM alone, ready for computes to be added one at a time.
    Control,
}

/// A compute VM's size. Omitted fields keep the default compute VM's value.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(default, deny_unknown_fields)]
pub struct ComputeSize {
    /// vCPUs, 1-16.
    pub cpus: u8,
    /// Guest memory, 3072-65536 MiB; nova-compute, libvirt and OVS take about 0.8 GiB of it.
    pub memory_mib: u32,
    /// Each container disk, 16384-524288 MiB; Nova keeps instance disks on Docker's.
    pub disk_mib: u32,
}
impl Default for ComputeSize {
    fn default() -> Self {
        Self {
            cpus: COMPUTE.cpus,
            memory_mib: COMPUTE.memory_mib,
            disk_mib: COMPUTE.container_disk_mib.unwrap_or(0),
        }
    }
}
impl ComputeSize {
    fn validate(self) -> Result<Self> {
        if !(1..=16).contains(&self.cpus) {
            bail!("a compute VM has 1-16 vCPUs");
        }
        if !(3072..=65536).contains(&self.memory_mib) {
            bail!("a compute VM has 3072-65536 MiB of memory");
        }
        if !(16384..=524288).contains(&self.disk_mib) {
            bail!("a compute VM's container disk is 16384-524288 MiB");
        }
        Ok(self)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, ToSchema)]
pub struct Plan {
    pub components: Vec<String>,
    /// OpenStack release the VMs install. Absent on plans recorded before it was kept, which
    /// read as [`crate::kolla::OPENSTACK_RELEASE`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub release: Option<String>,
    /// Compute VMs beside the control VM; 0 for an all-in-one farm.
    #[serde(default)]
    pub computes: u8,
    /// The compute VMs' size when it is not the default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compute: Option<ComputeSize>,
    pub nodes: Vec<NodeSpec>,
    pub memory_mib: u64,
    /// Includes both container disks for each container host. Sparse disks consume less initially.
    pub disk_mib: u64,
    pub warnings: Vec<String>,
}
impl Plan {
    /// Whether the farm runs a control VM, with or without computes yet, rather than one
    /// all-in-one VM.
    pub fn multinode(&self) -> bool {
        self.nodes
            .first()
            .is_some_and(|spec| spec.role == crate::farm::Role::Control)
    }
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

/// Of Nova's footprint, what nova-compute, libvirt and a small guest take; on a multi-node
/// farm the compute VMs carry it.
const NOVA_COMPUTE_MIB: u32 = 1024;

/// Plans a new farm. Nova always gets a compute VM apart from the network node: on one VM,
/// Neutron's userspace firewall drops instance TCP (see the OVS patch in kolla).
pub fn plan(request: &PlanRequest) -> Result<Plan> {
    let mut request = request.clone();
    // An explicit layout owns the VM count: an all-in-one farm stays one VM even with Nova,
    // and a control VM stays alone until computes are added.
    if request.computes == 0
        && request.layout.is_none()
        && resolve_components(&request.components)?
            .iter()
            .any(|c| c.id == "nova")
    {
        request.computes = 1;
    }
    layout(&request)
}

/// A multi-node farm's control VM alone, as a control template restores it: its farm's
/// plan without the compute VMs, which are added one at a time afterwards. The compute size
/// is kept for them.
pub fn control_layout(
    components: &[String],
    compute: Option<ComputeSize>,
    release: Option<&str>,
) -> Result<Plan> {
    layout(&PlanRequest {
        components: components.to_vec(),
        computes: 0,
        compute,
        release: release.map(str::to_owned),
        layout: Some(FarmLayout::Control),
    })
}

/// Lays out the VMs exactly as requested, so farms and snapshots made before [`plan`] moved
/// Nova off the all-in-one VM keep their layout.
pub fn layout(request: &PlanRequest) -> Result<Plan> {
    if request.computes > MAX_COMPUTES {
        bail!("a farm has at most {MAX_COMPUTES} compute VMs");
    }
    if request.layout.is_some() && request.computes > 0 {
        bail!(
            "a farm layout chooses the VMs when it has no compute VMs; leave layout out to run computes beside a control VM"
        );
    }
    let control_only = request.layout == Some(FarmLayout::Control);
    if request.layout == Some(FarmLayout::AllInOne)
        && resolve_components(&request.components)?
            .iter()
            .any(|c| c.id == "nova")
    {
        bail!(
            "Nova needs a compute VM, so an all-in-one farm cannot run it; create a control VM that gains computes instead"
        );
    }
    let mut requested = request.components.clone();
    if (request.computes > 0 || control_only) && !requested.iter().any(|c| c == "nova") {
        // Compute VMs exist to run instances; Nova brings Neutron and Placement along.
        requested.push("nova".into());
    }
    let components = resolve_components(&requested)?;
    let computes = request.computes;
    // The default size is stored as none, so a farm planned before sizes keeps its plan.
    let compute = match request.compute {
        Some(_) if computes == 0 && !control_only => bail!("compute size needs compute VMs"),
        Some(size) => Some(size.validate()?).filter(|size| *size != ComputeSize::default()),
        None => None,
    };
    let size = compute.unwrap_or_default();
    let footprint = components.iter().map(|c| c.footprint_mib).sum::<u32>();
    let nodes = if computes > 0 {
        let mut control = CONTROL;
        control.memory_mib += footprint - NOVA_COMPUTE_MIB;
        std::iter::once(control)
            .chain((1..=computes).map(|index| NodeSpec {
                index: Some(index),
                cpus: size.cpus,
                memory_mib: size.memory_mib,
                container_disk_mib: Some(size.disk_mib),
                ..COMPUTE
            }))
            .collect()
    } else if control_only {
        // Nova's control plane stays on the control VM; nova-compute waits for a compute.
        let mut control = CONTROL;
        control.memory_mib += footprint - NOVA_COMPUTE_MIB;
        vec![control]
    } else {
        let mut node = ALL_IN_ONE;
        node.memory_mib += footprint;
        vec![node]
    };
    let mut warnings = catalog().constraints;
    if computes > 0 {
        warnings[0] = format!(
            "The farm runs its control plane, network node and Kolla on one control VM and Nova instances on {} compute VM(s), joined by the host's link switch.",
            computes
        );
    } else if control_only {
        warnings[0] = "The farm runs its control plane, network node and Kolla on one control VM; Nova instances need a compute VM added to it.".into();
    }
    Ok(Plan {
        components: components.iter().map(|c| c.id.to_owned()).collect(),
        release: request.release.clone(),
        computes,
        compute,
        memory_mib: nodes.iter().map(|n| u64::from(n.memory_mib)).sum(),
        disk_mib: nodes
            .iter()
            .map(|n| u64::from(n.disk_mib) + 2 * u64::from(n.container_disk_mib.unwrap_or(0)))
            .sum(),
        nodes,
        warnings,
    })
}

#[derive(Clone, Debug, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct CreateRequest {
    pub id: String,
    /// Registered host that runs the farm: this machine's local host or a remote one.
    /// Required; it is optional only so the error can say how to register one.
    #[serde(default)]
    pub host_id: Option<String>,
    #[serde(default)]
    pub components: Vec<String>,
    /// Compute VMs beside a control VM (see [`PlanRequest::computes`]).
    #[serde(default)]
    pub computes: u8,
    /// Size of every compute VM (see [`PlanRequest::compute`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compute: Option<ComputeSize>,
    /// OpenStack release to install (see [`PlanRequest::release`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub release: Option<String>,
    /// VMs of a farm without computes (see [`PlanRequest::layout`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub layout: Option<FarmLayout>,
    #[serde(default = "default_host")]
    pub external_host: String,
    #[serde(default)]
    pub owner: Option<String>,
    /// UTC Unix seconds. Metadata only in this version; no automatic deletion.
    #[serde(default)]
    pub expires_at: Option<u64>,
    /// Start from this snapshot instead of an empty VM. Its services replace `components`.
    /// A control template starts a multi-node farm with its control VM alone, ready for
    /// computes to be added; a compute template cannot start a farm.
    #[serde(default)]
    pub snapshot_id: Option<String>,
    /// Compute VMs that run on another host than `host_id`, by VM (`compute-2` → host id).
    /// Their links reach the control VM's host through the coordinator's relay.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub placement: BTreeMap<String, String>,
}
fn default_host() -> String {
    "localhost".into()
}
impl CreateRequest {
    pub fn validate(&self) -> Result<Plan> {
        validate_id(&self.id)?;
        // Older all-in-one VMs are named `<farm>-all-in-one`; such an id would read as one of them.
        if self.id.ends_with(crate::farm::LEGACY_ALL_IN_ONE_SUFFIX) {
            bail!(
                "farm id must not end with '{}'",
                crate::farm::LEGACY_ALL_IN_ONE_SUFFIX
            );
        }
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
        for (node, host) in &self.placement {
            validate_id(host)?;
            let index = node
                .strip_prefix("compute-")
                .and_then(|i| i.parse::<u8>().ok())
                .filter(|i| *i >= 1 && *i <= self.computes);
            if index.is_none() || node != &format!("compute-{}", index.unwrap_or(0)) {
                bail!(
                    "placement names compute VMs, compute-1 to compute-{}; got '{node}'",
                    self.computes
                );
            }
        }
        if let Some(snapshot) = &self.snapshot_id {
            validate_id(snapshot)?;
            if !self.placement.is_empty() {
                bail!("a farm restored from a snapshot runs on one host; omit placement");
            }
            if !self.components.is_empty()
                || self.computes > 0
                || self.compute.is_some()
                || self.layout.is_some()
            {
                bail!(
                    "a farm restored from a snapshot runs the snapshot's services and layout; omit components, computes, compute and layout"
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
            computes: self.computes,
            compute: self.compute,
            release: self.release.clone(),
            layout: self.layout,
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

#[derive(Clone, Debug, Default, Serialize, Deserialize, ToSchema)]
#[serde(default, deny_unknown_fields)]
pub struct AddComputeRequest {
    /// Host for the new compute VM; omitted for the farm's primary host. A compute on
    /// another host joins the farm through its link relay.
    pub host_id: Option<String>,
    /// Compute template snapshot to boot the new VM from instead of installing it clean.
    /// Kolla then only reconfigures it; the template must match the farm's OpenStack
    /// release, components and CPU architecture.
    pub from_snapshot: Option<String>,
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
    /// Add the next compute VM to an existing multi-node farm
    Expand,
    Delete,
    /// Freeze the farm's VM into a snapshot
    Snapshot,
    /// Create the farm's VM from a snapshot
    Restore,
    /// Check that a deployed farm's services answer again, without deploying
    Settle,
    /// Start a farm of an OpenStack release from its templates: restore the control VM,
    /// then add each compute, each step a job of its own
    Assemble,
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
    /// Finer progress inside `phase`: our marker or the running Ansible task.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub step: Option<String>,
    pub created_at: u64,
    pub updated_at: u64,
    /// Snapshot this farm was restored from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot_id: Option<String>,
    /// Faults set on a multi-node farm's links, by VM (`control`, `compute-1`, ...).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub faults: BTreeMap<String, crate::link::Impair>,
    /// Other hosts that run some of a multi-node farm's compute VMs. `host_id` runs the rest.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub shares: Vec<HostShare>,
    /// SHA-256 of the token `host_id`'s switch joins the relay with, for a farm with shares.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uplink_sha256: Option<String>,
}

/// The compute VMs of a farm that one other host runs.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct HostShare {
    pub host_id: String,
    /// The agent this host's part was created with, for a remote host.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_sha256: Option<String>,
    /// VM suffixes (`compute-2`, ...).
    pub nodes: Vec<String>,
    /// SHA-256 of the token this host's switch joins the relay with.
    pub uplink_sha256: String,
}

impl FarmRecord {
    pub fn memory_mib(&self) -> u64 {
        self.plan
            .as_ref()
            .map(|p| p.memory_mib)
            .unwrap_or_else(|| self.nodes.iter().map(|n| u64::from(n.memory_mib)).sum())
    }
    /// The host that runs the VM with `suffix`.
    pub fn host_of(&self, suffix: &str) -> Option<&str> {
        self.shares
            .iter()
            .find(|share| share.nodes.iter().any(|n| n == suffix))
            .map(|share| share.host_id.as_str())
            .or(self.host_id.as_deref())
    }
    /// Every host the farm has VMs on: `host_id` first.
    pub fn hosts(&self) -> Vec<&str> {
        self.host_id
            .as_deref()
            .into_iter()
            .chain(self.shares.iter().map(|share| share.host_id.as_str()))
            .collect()
    }
    /// Memory the farm reserves on `host`.
    pub fn memory_on(&self, host: &str) -> u64 {
        if self.shares.is_empty() {
            return if self.host_id.as_deref() == Some(host) {
                self.memory_mib()
            } else {
                0
            };
        }
        self.plan.as_ref().map_or(0, |plan| {
            memory_on(plan, self.host_id.as_deref(), &self.shares, host)
        })
    }
}

/// Memory of the VMs of `plan` that run on `host`, with `shares` on other hosts than `primary`.
pub fn memory_on(plan: &Plan, primary: Option<&str>, shares: &[HostShare], host: &str) -> u64 {
    plan.nodes
        .iter()
        .filter(|spec| {
            let suffix = crate::farm::suffix(spec.role, spec.index).unwrap_or_default();
            shares
                .iter()
                .find(|share| share.nodes.contains(&suffix))
                .map(|share| share.host_id.as_str())
                .or(primary)
                == Some(host)
        })
        .map(|spec| u64::from(spec.memory_mib))
        .sum()
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
    /// A delete that removes only the coordinator's record and leaves any VM on the host.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub forget: bool,
    /// The host-side job a deploy runs as. It outlives the coordinator, which re-attaches
    /// to it after a restart.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_job_id: Option<String>,
    /// Progress events of the host-side job already recorded here.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub remote_frames: u64,
    /// Host that receives the VM of a compute-expansion job.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_host: Option<String>,
    /// Suffix of the compute VM a compute-expansion job adds (`compute-2`, ...).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_node: Option<String>,
    /// Numbered steps of a job whose steps are known up front, such as a freeze, in the
    /// order they started; the last one is current while the job runs.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub steps: Vec<JobStep>,
    /// The steps of an assemble job, in order, each with the job that ran it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub assembly: Vec<AssemblyStep>,
}
/// One numbered step of a job.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct JobStep {
    pub step: String,
    /// Position from 1.
    pub index: u32,
    pub total: u32,
    pub started_at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<u64>,
    /// The job failed during this step.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub failed: bool,
}
impl Job {
    /// Start a step, ending the one before it.
    pub fn start_step(&mut self, step: String, index: u32, total: u32, at: u64) {
        self.end_step(false, at);
        self.steps.push(JobStep {
            step,
            index,
            total,
            started_at: at,
            finished_at: None,
            failed: false,
        });
    }
    /// End the current step, if one is running.
    pub fn end_step(&mut self, failed: bool, at: u64) {
        if let Some(current) = self.steps.last_mut().filter(|s| s.finished_at.is_none()) {
            current.finished_at = Some(at);
            current.failed = failed;
        }
    }
}
fn is_zero(value: &u64) -> bool {
    *value == 0
}

/// One step of an assemble job.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct AssemblyStep {
    /// What the step does: `import`, `create`, `deploy`, `restore` or `expand`.
    pub action: String,
    /// The VM it makes (`control`, `compute-1`, ...); none for an import.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node: Option<String>,
    /// The template it imports or boots from; none for a clean install.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot_id: Option<String>,
    /// Host the VM runs on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_id: Option<String>,
    pub description: String,
    pub status: JobStatus,
    /// The farm job that ran it; none for an import, which the snapshot shows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub job_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Create a farm of an OpenStack release from saved templates: the control VM from a
/// control template, each compute from a compute template of the same release when one
/// can boot on its host, otherwise installed clean.
#[derive(Clone, Debug, Default, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ReleaseCreateRequest {
    pub id: String,
    /// OpenStack release, such as `2026.2`.
    pub release: String,
    /// Registered host of the control VM. Required; optional only for the error message.
    #[serde(default)]
    pub host_id: Option<String>,
    /// Services the farm runs; empty takes the control template's.
    #[serde(default)]
    pub components: Vec<String>,
    /// Compute VMs, up to 3.
    #[serde(default)]
    pub computes: u8,
    /// VMs of a farm without computes: `all-in-one` for one VM that runs everything, or
    /// `control` for a control VM alone, ready for computes from compute templates to join
    /// it later. Omitted keeps the previous behaviour (a control template starts its farm
    /// with the control VM alone, a clean install runs all-in-one).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub layout: Option<FarmLayout>,
    /// Host of each compute VM in order (`compute-1` first); a missing or empty entry is
    /// the control VM's host.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub compute_hosts: Vec<String>,
    /// Size of the compute VMs this farm installs clean, and of the ones added to it later.
    /// A compute that boots from a compute template keeps the template's size.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compute: Option<ComputeSize>,
    /// Control template to use instead of the newest matching one.
    #[serde(default)]
    pub control_template: Option<String>,
    /// Compute template to use instead of the newest matching one.
    #[serde(default)]
    pub compute_template: Option<String>,
    /// Registered bucket to also search for templates; omitted searches every bucket.
    #[serde(default)]
    pub bucket: Option<String>,
    #[serde(default = "default_host")]
    pub external_host: String,
    #[serde(default)]
    pub owner: Option<String>,
    #[serde(default)]
    pub expires_at: Option<u64>,
}
impl ReleaseCreateRequest {
    /// The VM's host: `compute_hosts` for a compute, else the control VM's.
    pub fn host_of(&self, index: u8) -> Option<String> {
        self.compute_hosts
            .get(usize::from(index).wrapping_sub(1))
            .filter(|host| !host.trim().is_empty())
            .cloned()
            .or_else(|| self.host_id.clone())
    }
}

/// Where a planned VM comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum NodeSource {
    /// A template snapshot this coordinator keeps.
    Template,
    /// A template archive in a bucket, imported first.
    Bucket,
    /// Installed from the golden image.
    Clean,
}

/// How one VM of a release farm will be made, and why.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct PlannedNode {
    /// `control`, `compute-1`, ... or `all-in-one` for a clean farm without computes.
    pub node: String,
    pub host_id: String,
    pub source: NodeSource,
    /// The local template it boots from; for a bucket template, the ID it is imported as.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bucket: Option<String>,
    /// Object key of a bucket template's archive.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    pub reason: String,
}

/// The resolved plan of a [`ReleaseCreateRequest`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct ReleasePlan {
    pub farm_id: String,
    pub release: String,
    pub arch: String,
    pub components: Vec<String>,
    pub control: PlannedNode,
    pub computes: Vec<PlannedNode>,
    /// Buckets that could not be searched, and other things worth knowing.
    #[serde(default)]
    pub notes: Vec<String>,
}

/// Templates of one OpenStack release.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct ReleaseTemplates {
    pub release: String,
    pub local_control: usize,
    pub local_compute: usize,
    pub bucket_control: usize,
    pub bucket_compute: usize,
    /// Whether this coordinator can also install the release clean.
    pub installable: bool,
}

/// Releases a farm can be created with on one architecture, newest first.
#[derive(Clone, Debug, Serialize, Deserialize, ToSchema)]
pub struct ReleaseCatalog {
    pub arch: String,
    pub releases: Vec<ReleaseTemplates>,
    /// Buckets that could not be listed, with the error.
    #[serde(default)]
    pub unreachable: Vec<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct SnapshotRequest {
    /// Snapshot ID: 1-48 lowercase letters, digits or hyphens.
    pub id: String,
    #[serde(default)]
    pub note: Option<String>,
    /// Freeze only this VM of a deployed multi-node farm as a template: a compute VM
    /// (`compute-1`, ...) that later expansions boot new computes from, or `control`, which
    /// new farms start from without computes.
    #[serde(default)]
    pub node: Option<String>,
}
impl SnapshotRequest {
    pub fn validate(&self) -> Result<()> {
        validate_snapshot_id(&self.id)?;
        if let Some(node) = &self.node {
            let index = node
                .strip_prefix("compute-")
                .and_then(|index| index.parse::<u8>().ok());
            if node != "control"
                && !index.is_some_and(|index| (1..=crate::farm::MAX_COMPUTES).contains(&index))
            {
                bail!(
                    "only the control VM or a compute VM (compute-1, ...) can be frozen as a template"
                );
            }
        }
        if self.note.as_ref().is_some_and(|n| n.len() > 512) {
            bail!("note must be at most 512 bytes");
        }
        Ok(())
    }
}
#[derive(Clone, Debug, Default, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ExportRequest {
    /// Absolute archive path on the coordinator; defaults to `<state dir>/exports/<id>.msb`.
    #[serde(default)]
    pub path: Option<String>,
    /// Registered bucket to upload the archive to instead of keeping it on the coordinator.
    #[serde(default)]
    pub bucket: Option<String>,
    /// Object key in `bucket`, with `.msb` appended when missing; defaults to `<prefix><id>.msb`.
    /// An existing object is not replaced.
    #[serde(default)]
    pub key: Option<String>,
}
/// Import from an archive file, or from an object in a registered bucket.
#[derive(Clone, Debug, Default, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ImportRequest {
    /// Absolute path of a `.msb` archive on the coordinator.
    #[serde(default)]
    pub path: Option<String>,
    /// Registered bucket holding the archive; requires `key`.
    #[serde(default)]
    pub bucket: Option<String>,
    #[serde(default)]
    pub key: Option<String>,
    /// Snapshot ID to import as; defaults to the archive's file name.
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
    /// File path on the coordinator, or `s3://<bucket name>/<key>`.
    pub path: String,
    pub size_bytes: u64,
    pub exported_at: u64,
    /// Registered bucket it was uploaded to.
    #[serde(default)]
    pub bucket: Option<String>,
    #[serde(default)]
    pub key: Option<String>,
    /// Hex SHA-256 of an uploaded archive, as its metadata file in the bucket records.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
}
/// An export to, or import from, a bucket in progress.
#[derive(Clone, Debug, Serialize, Deserialize, ToSchema)]
pub struct SnapshotTransfer {
    pub bucket: String,
    pub key: String,
    /// Bytes uploaded or downloaded so far.
    pub done_bytes: u64,
    /// Archive size; `None` while an export is still writing the archive.
    pub total_bytes: Option<u64>,
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
    /// Remote host that keeps the original, which it was frozen on; `None` for the local
    /// host, which keeps imported snapshots and every snapshot recorded before this field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_id: Option<String>,
    /// Farm the snapshot was frozen from; `None` when imported from an archive.
    pub source_farm: Option<String>,
    pub plan: Plan,
    /// Whether OpenStack was deployed when frozen. Restored farms then start `ready`.
    pub deployed: bool,
    /// Public ports the source farm's catalog was registered with; restores rewrite them.
    pub source_exposure: Exposure,
    pub note: Option<String>,
    /// msb snapshot reference; set once captured or imported. A multi-node snapshot's is
    /// its control VM's, and `nodes` holds every VM's.
    pub reference: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub nodes: Vec<crate::farm::NodeSnapshot>,
    /// The one VM a template holds: `control`, which new farms start from without
    /// computes, or `compute`, which expansions boot new computes from. None for a snapshot
    /// of a whole farm.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub template: Option<TemplateRole>,
    /// The VM a template was frozen from (`control`, `compute-1`, ...).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node: Option<String>,
    /// Records from before control templates marked compute templates this way; read once
    /// into `template` when the state is loaded.
    #[serde(default, rename = "compute_template", skip_serializing)]
    pub legacy_compute_template: bool,
    /// OpenStack release the guest was deployed with; unknown for older snapshots.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub release: Option<String>,
    pub size_bytes: Option<u64>,
    /// Most recent export.
    pub archive: Option<SnapshotArchive>,
    #[serde(default)]
    pub transfer: Option<SnapshotTransfer>,
    /// Copies kept on hosts other than the original's, which farms and computes there boot
    /// from.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub copies: Vec<SnapshotCopy>,
    pub error: Option<String>,
    pub created_at: u64,
    pub updated_at: u64,
}

/// A snapshot copied to another host, where its agent keeps it as an msb snapshot.
#[derive(Clone, Debug, Serialize, Deserialize, ToSchema)]
pub struct SnapshotCopy {
    pub host_id: String,
    /// msb snapshot reference on that host.
    pub reference: String,
    /// How the archive got there.
    pub via: CopySource,
    pub size_bytes: Option<u64>,
    pub copied_at: u64,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum CopySource {
    /// The host downloaded the archive from a registered bucket itself.
    Bucket,
    /// The coordinator streamed the archive over the agent's connection.
    Stream,
    /// The host already had it when the coordinator looked.
    Found,
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
    /// Compute VMs of a multi-node snapshot's farm; absent on all-in-one snapshots.
    pub const COMPUTES: &str = "farm.snapshot.computes";
    /// The VM a multi-node snapshot's disk was taken from (`control`, `compute-1`, ...).
    pub const NODE: &str = "farm.snapshot.node";
    /// `control` or `compute` on a template; absent on farm snapshots.
    pub const ROLE: &str = "farm.snapshot.role";
}

/// The VM a template snapshot was frozen from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum TemplateRole {
    Control,
    Compute,
}
impl TemplateRole {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Control => "control",
            Self::Compute => "compute",
        }
    }
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
        let legacy = CreateRequest {
            id: "dev-all-in-one".into(),
            host_id: None,
            components: vec![],
            computes: 0,
            compute: None,
            layout: None,
            release: None,
            external_host: default_host(),
            owner: None,
            expires_at: None,
            snapshot_id: None,
            placement: Default::default(),
        };
        assert!(legacy.validate().is_err());
        for unsupported in ["swift", "cinder"] {
            assert!(
                plan(&PlanRequest {
                    components: vec![unsupported.into()],
                    computes: 0,
                    compute: None,
                    layout: None,
                    release: None,
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
            computes: 0,
            compute: None,
            layout: None,
            release: None,
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
        // Nova gets a compute VM; snapshots from before that keep their all-in-one layout.
        let with_nova = plan(&request(&["nova"])).unwrap();
        assert_eq!(
            with_nova.components,
            ["keystone", "glance", "placement", "neutron", "nova"]
        );
        assert_eq!(with_nova.computes, 1);
        assert_eq!(with_nova.memory_mib, 6656 + 3072);
        let old_nova = layout(&request(&["nova"])).unwrap();
        assert_eq!(old_nova.nodes.len(), 1);
        assert_eq!(old_nova.memory_mib, 3072 + 256 + 2304 + 2048);
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
    fn multi_node_farms_split_nova_between_control_and_computes() {
        let request = |components: &[&str], computes| PlanRequest {
            components: components.iter().map(|c| (*c).into()).collect(),
            computes,
            compute: None,
            layout: None,
            release: None,
        };
        // Compute VMs bring Nova, and Nova its dependencies.
        let two = plan(&request(&[], 2)).unwrap();
        assert_eq!(two.computes, 2);
        assert_eq!(
            two.components,
            ["keystone", "glance", "placement", "neutron", "nova"]
        );
        let roles: Vec<_> = two.nodes.iter().map(|n| (n.role, n.index)).collect();
        assert_eq!(
            roles,
            [
                (Role::Control, None),
                (Role::Compute, Some(1)),
                (Role::Compute, Some(2))
            ]
        );
        assert_eq!(two.nodes[0].memory_mib, 3072 + 256 + 2304 + 2048 - 1024);
        assert_eq!(two.nodes[1].memory_mib, 3072);
        assert_eq!(two.memory_mib, 6656 + 2 * 3072);
        assert_eq!(two.disk_mib, 3 * (20480 + 2 * 32768));
        assert!(two.warnings.iter().any(|w| w.contains("2 compute VM")));
        crate::farm::layout(&two.nodes).unwrap();
        let horizon = plan(&request(&["horizon"], 1)).unwrap();
        assert_eq!(horizon.nodes[0].memory_mib, 6656 + 512);
        assert!(plan(&request(&[], 4)).is_err());
        // Old plans have no layout and read as all-in-one.
        let mut old = serde_json::to_value(plan(&PlanRequest::default()).unwrap()).unwrap();
        old.as_object_mut().unwrap().remove("computes");
        assert_eq!(serde_json::from_value::<Plan>(old).unwrap().computes, 0);
        let restore = CreateRequest {
            id: "dev".into(),
            host_id: None,
            components: vec![],
            computes: 1,
            compute: None,
            layout: None,
            release: None,
            external_host: default_host(),
            owner: None,
            expires_at: None,
            snapshot_id: Some("snap".into()),
            placement: Default::default(),
        };
        assert!(restore.validate().is_err());
    }

    #[test]
    fn a_control_template_keeps_its_farms_control_vm_and_compute_size() {
        let size = ComputeSize {
            cpus: 4,
            memory_mib: 8192,
            disk_mib: 32768,
        };
        let farm = layout(&PlanRequest {
            components: vec!["horizon".into()],
            computes: 2,
            compute: Some(size),
            layout: None,
            release: None,
        })
        .unwrap();
        let alone = control_layout(&farm.components, farm.compute, None).unwrap();
        assert!(alone.multinode() && !layout(&PlanRequest::default()).unwrap().multinode());
        assert_eq!(alone.computes, 0);
        assert_eq!(alone.compute, Some(size));
        assert_eq!(alone.components, farm.components);
        assert_eq!(
            serde_json::to_value(&alone.nodes).unwrap(),
            serde_json::to_value(&farm.nodes[..1]).unwrap()
        );
        assert_eq!(alone.memory_mib, u64::from(farm.nodes[0].memory_mib));
        crate::farm::layout(&alone.nodes).unwrap();
        // Its first compute comes back as the farm's had it.
        let next = layout(&PlanRequest {
            components: alone.components.clone(),
            computes: 1,
            compute: alone.compute,
            layout: None,
            release: None,
        })
        .unwrap();
        assert_eq!(
            serde_json::to_value(&next.nodes[..1]).unwrap(),
            serde_json::to_value(&alone.nodes).unwrap()
        );
        assert_eq!(
            serde_json::to_value(next.nodes[1]).unwrap(),
            serde_json::to_value(farm.nodes[1]).unwrap()
        );

        let freeze = |node: &str| SnapshotRequest {
            id: "tpl".into(),
            note: None,
            node: Some(node.into()),
        };
        freeze("control").validate().unwrap();
        freeze("compute-3").validate().unwrap();
        for node in ["compute-0", "compute-4", "all-in-one", "controller"] {
            assert!(freeze(node).validate().is_err(), "{node}");
        }
    }

    #[test]
    fn a_control_layout_is_not_an_all_in_one_farm() {
        let alone = |form: FarmLayout| {
            crate::model::layout(&PlanRequest {
                components: vec![],
                computes: 0,
                compute: None,
                layout: Some(form),
                release: None,
            })
            .unwrap()
        };
        // All-in-one is one VM that runs everything and cannot gain computes.
        let one = alone(FarmLayout::AllInOne);
        assert_eq!(one.nodes.len(), 1);
        assert_eq!(one.nodes[0].role, Role::AllInOne);
        assert!(!one.multinode());
        // A control VM alone is a multi-node farm that waits for its computes.
        let control = alone(FarmLayout::Control);
        assert_eq!(control.nodes.len(), 1);
        assert_eq!(control.nodes[0].role, Role::Control);
        assert_eq!(control.computes, 0);
        assert!(control.multinode());
        // Nova's control plane is installed now; nova-compute arrives with a compute VM.
        assert!(control.components.contains(&"nova".to_string()));
        assert!(control.memory_mib > one.memory_mib);
        let with_compute = layout(&PlanRequest {
            components: vec![],
            computes: 1,
            compute: None,
            layout: None,
            release: None,
        })
        .unwrap();
        assert_eq!(
            control.memory_mib,
            u64::from(with_compute.nodes[0].memory_mib)
        );
        // Its first compute joins without changing the control VM.
        let next = layout(&PlanRequest {
            components: control.components.clone(),
            computes: 1,
            compute: None,
            layout: None,
            release: None,
        })
        .unwrap();
        assert_eq!(
            serde_json::to_value(&next.nodes[..1]).unwrap(),
            serde_json::to_value(&control.nodes).unwrap()
        );
        // A layout never rides along with computes.
        assert!(
            layout(&PlanRequest {
                components: vec![],
                computes: 1,
                compute: None,
                layout: Some(FarmLayout::Control),
                release: None,
            })
            .is_err()
        );
        // Nova cannot run on one all-in-one VM.
        assert!(
            layout(&PlanRequest {
                components: vec!["nova".into()],
                computes: 0,
                compute: None,
                layout: Some(FarmLayout::AllInOne),
                release: None,
            })
            .is_err()
        );
    }

    #[test]
    fn compute_vms_take_the_requested_size() {
        let sized = |computes, compute| {
            plan(&PlanRequest {
                components: vec![],
                computes,
                compute,
                layout: None,
                release: None,
            })
        };
        let size = ComputeSize {
            cpus: 4,
            memory_mib: 8192,
            disk_mib: 65536,
        };
        let big = sized(2, Some(size)).unwrap();
        assert_eq!(big.compute, Some(size));
        for node in &big.nodes[1..] {
            assert_eq!(
                (node.cpus, node.memory_mib, node.container_disk_mib),
                (4, 8192, Some(65536))
            );
        }
        assert_eq!(big.nodes[0].memory_mib, 6656);
        assert_eq!(big.memory_mib, 6656 + 2 * 8192);
        assert_eq!(big.disk_mib, (20480 + 2 * 32768) + 2 * (20480 + 2 * 65536));
        // The default size plans like an unsized farm, so its plan stays as before.
        let default = sized(1, Some(ComputeSize::default())).unwrap();
        assert_eq!(default.compute, None);
        assert_eq!(
            serde_json::to_value(&default).unwrap(),
            serde_json::to_value(sized(1, None).unwrap()).unwrap()
        );
        // Partial sizes keep the default for what they omit.
        let partial: ComputeSize = serde_json::from_str(r#"{"memory_mib":6144}"#).unwrap();
        assert_eq!((partial.cpus, partial.disk_mib), (2, 32768));
        for bad in [
            ComputeSize { cpus: 0, ..size },
            ComputeSize {
                memory_mib: 2048,
                ..size
            },
            ComputeSize {
                disk_mib: 8192,
                ..size
            },
        ] {
            assert!(sized(1, Some(bad)).is_err());
        }
        // Only compute VMs have a compute size.
        assert!(
            layout(&PlanRequest {
                components: vec![],
                computes: 0,
                compute: Some(size),
                layout: None,
                release: None,
            })
            .is_err()
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
