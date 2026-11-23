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
    /// Compute VMs beside a control VM, 1-3. A farm always runs Nova and Neutron; a
    /// request with none is refused, as are the legacy layouts below.
    pub computes: u8,
    /// Size of every compute VM; omitted for the default. Only for farms with compute VMs.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compute: Option<ComputeSize>,
    /// OpenStack release to install; omitted installs this coordinator's own release.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub release: Option<String>,
    /// Layout of a farm without computes: read only, for farms and snapshots made before
    /// every farm gained computes. New farms leave it out; [`require_multi_node`] refuses it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub layout: Option<FarmLayout>,
    /// A virtual GPU on every compute VM; omitted for none. Not hardware: see [`GpuConfig`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gpu: Option<GpuConfig>,
    /// Host PCI devices passed through to compute-1 (`DDDD:BB:DD.F`). Each must already be
    /// bound to `vfio-pci` on the farm's host; see [`normalize_pci_devices`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pci_devices: Vec<String>,
}

/// Normalizes one PCI address to `DDDD:BB:DD.F` in lowercase. The device number must be at
/// most `0x1f` and the function at most `7`, as the PCI configuration space allows.
pub fn normalize_pci_bdf(bdf: &str) -> Result<String> {
    let text = bdf.trim().to_ascii_lowercase();
    let bytes = text.as_bytes();
    let shape_ok = bytes.len() == 12
        && bytes[4] == b':'
        && bytes[7] == b':'
        && bytes[10] == b'.'
        && bytes
            .iter()
            .enumerate()
            .all(|(i, b)| matches!(i, 4 | 7 | 10) || b.is_ascii_hexdigit());
    if !shape_ok {
        bail!("PCI address '{bdf}' must look like 0000:04:00.3 (DDDD:BB:DD.F in hex)");
    }
    let device = u8::from_str_radix(&text[8..10], 16)?;
    let function = u8::from_str_radix(&text[11..12], 16)?;
    if device > 0x1f || function > 7 {
        bail!("PCI address '{bdf}' has a device above 0x1f or a function above 7");
    }
    Ok(text)
}

/// Normalizes a list of PCI addresses, refusing a duplicate. The order is the request's.
pub fn normalize_pci_devices(devices: &[String]) -> Result<Vec<String>> {
    let mut seen = std::collections::BTreeSet::new();
    let mut out = Vec::with_capacity(devices.len());
    for device in devices {
        let bdf = normalize_pci_bdf(device)?;
        if !seen.insert(bdf.clone()) {
            bail!("PCI device '{bdf}' is listed more than once");
        }
        out.push(bdf);
    }
    Ok(out)
}

impl PlanRequest {
    /// The release the farm installs: the request's, or this coordinator's own.
    pub fn release_or_default(&self) -> &str {
        self.release
            .as_deref()
            .unwrap_or(crate::kolla::OPENSTACK_RELEASE)
    }
}

/// The layout of a farm without computes, kept readable for farms and snapshots made
/// before every new farm ran a control VM with computes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "kebab-case")]
pub enum FarmLayout {
    /// Every selected service in one VM; it cannot gain compute VMs.
    AllInOne,
    /// A control VM alone, ready for computes to be added one at a time.
    Control,
}

/// A new farm always runs a control VM and at least one compute VM, so a compute can be
/// added to it from a template at any time. The layouts in [`FarmLayout`] stay readable on
/// existing farms and snapshots but no request makes one; a snapshot restore keeps the
/// layout it was frozen with.
pub fn require_multi_node(computes: u8, layout: Option<FarmLayout>) -> Result<()> {
    if layout.is_some() {
        bail!(
            "a new farm runs computes beside a control VM; omit layout (all-in-one and control-only farms can no longer be created)"
        );
    }
    if computes == 0 {
        bail!("a new farm has at least one compute VM; set computes to 1-3");
    }
    Ok(())
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
/// How a farm gets a GPU when the host has none. Both modes are virtual devices: no guest
/// sees a real GPU, and nothing computes on one.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum GpuMode {
    /// farm advertises the standard `VGPU` inventory on a child resource provider of every
    /// compute, under the name a vGPU deployment's Nova gives one: `<host>_pci_<address>`.
    /// The flavor asks for that class plus the traits the request names, so Placement
    /// schedules and allocates a virtual GPU, and the instance boots without one: Nova's
    /// libvirt driver has no mediated-device type configured, so it neither creates nor
    /// consumes one (OpenStack 2025.1 and newer).
    #[default]
    Simulated,
    /// Nova binds the guest kernel's `mtty` mediated device as a `VGPU` (OpenStack 2025.1 and
    /// newer, on a compute VM booted with a VFIO/mdev libkrunfw). Releases before 2026.2 get
    /// Nova's own `mtty` support from farm's backport, which the container applies at start
    /// (see [`crate::kolla::config::nova_mtty_patch`]). The guest sees a `4348:3253` serial
    /// controller through VFIO.
    Mtty,
}

/// Virtual GPUs a compute VM offers at most.
pub const MAX_GPU_COUNT: u32 = 24;
/// Fake physical GPUs a compute VM splits its virtual GPUs over at most.
pub const MAX_GPU_DEVICES: u32 = 24;
/// The trait `simulated` requires when the request names none.
pub const DEFAULT_VGPU_TRAIT: &str = "CUSTOM_FARM_VGPU";
/// The resource class both modes advertise unless the request picks another one: the standard
/// class Nova inventories a mediated device's virtual GPU under.
pub const DEFAULT_RESOURCE_CLASS: &str = "VGPU";
/// The PCI bus number of the first fake GPU on a compute. Any bus would do: the address is
/// farm's, not read from a device, and exists to give the provider a vGPU host's name.
const VGPU_BUS_FIRST: u32 = 0x84;

/// A farm's GPU: `count` virtual devices on every compute VM.
///
/// The `simulated` mode's provider layout is farm's own, so a request can describe the
/// contract another deployment's flavor expects: the resource class, the traits it requires
/// and how many cards the host has. `mtty` advertises what Nova finds and takes none of them.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct GpuConfig {
    #[serde(default)]
    pub mode: GpuMode,
    /// Devices per compute VM, 1-[`MAX_GPU_COUNT`].
    #[serde(default = "one_virtual_gpu")]
    #[schema(minimum = 1, maximum = 24)]
    pub count: u32,
    /// Traits the flavor requires besides the resource class. Omitted, `simulated` requires
    /// [`DEFAULT_VGPU_TRAIT`]; `mtty` requires none, because Nova builds that provider.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub traits: Option<Vec<String>>,
    /// Fake physical GPUs each compute advertises its virtual GPUs on, one child resource
    /// provider each, as a vGPU host has one per card. Two devices make a flavor's request
    /// land on a card rather than on a host. Omitted: one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(minimum = 1, maximum = 24)]
    pub devices: Option<u32>,
    /// The resource class the flavor asks for. Omitted: [`DEFAULT_RESOURCE_CLASS`]. `PGPU`
    /// and `CUSTOM_*` build another deployment's contract; the inventory is farm's either
    /// way, so no driver checks it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<String>, example = "VGPU")]
    pub resource_class: Option<String>,
}

/// The serde default of [`GpuConfig::count`]: a body that names only a mode.
fn one_virtual_gpu() -> u32 {
    1
}
impl Default for GpuConfig {
    fn default() -> Self {
        Self {
            mode: GpuMode::Simulated,
            count: 1,
            traits: None,
            devices: None,
            resource_class: None,
        }
    }
}
impl GpuConfig {
    /// The fake GPU `device` of `host`, named as Nova names the provider of a card at a PCI
    /// address: `<hypervisor_hostname>_pci_<domain>_<bus>_<slot>_<func>`.
    pub fn provider_name(host: &str, device: u32) -> String {
        format!("{host}_pci_0000_{:02x}_00_0", VGPU_BUS_FIRST + device)
    }
    /// The `count` virtual GPUs split over the fake GPUs, so every card offers at least one
    /// and the first ones take the remainder.
    pub fn device_totals(&self) -> Vec<u32> {
        let devices = self.device_count().min(self.count.max(1));
        (0..devices)
            .map(|device| self.count / devices + u32::from(device < self.count % devices))
            .collect()
    }
    /// The fake GPUs each compute advertises the virtual GPUs on.
    pub fn device_count(&self) -> u32 {
        self.devices.unwrap_or(1)
    }
    /// The resource class each compute inventories and the flavor asks for.
    pub fn resource_class(&self) -> &str {
        match self.mode {
            GpuMode::Simulated => self
                .resource_class
                .as_deref()
                .unwrap_or(DEFAULT_RESOURCE_CLASS),
            GpuMode::Mtty => DEFAULT_RESOURCE_CLASS,
        }
    }
    /// The traits the flavor requires. The kernel's mediated device carries none of farm's:
    /// Nova sets that provider's traits itself.
    pub fn traits(&self) -> Vec<String> {
        match (self.mode, &self.traits) {
            (GpuMode::Mtty, _) => Vec::new(),
            (GpuMode::Simulated, Some(traits)) => traits.clone(),
            (GpuMode::Simulated, None) => vec![DEFAULT_VGPU_TRAIT.to_owned()],
        }
    }
    /// The flavor farm creates for both modes: one virtual GPU, whatever class it is.
    pub fn flavor(&self) -> &'static str {
        "farm.vgpu"
    }
    /// The oldest OpenStack release that can offer this mode.
    pub fn min_release(&self) -> &'static str {
        match self.mode {
            // `simulated` needs the Placement microversion that gives a provider a parent
            // (1.14), and `mtty` needs Nova's mediated-device support, which 2025.1's Nova
            // gains from the backport farm installs at container start.
            GpuMode::Simulated | GpuMode::Mtty => "2025.1",
        }
    }
    /// The mode as the API and the portal name it.
    pub fn mode_name(&self) -> &'static str {
        match self.mode {
            GpuMode::Simulated => "simulated",
            GpuMode::Mtty => "mtty",
        }
    }
    /// Refuses a release that cannot express this mode. `release` is trimmed and may be a
    /// series name; it is compared after [`crate::kolla::releases::info`] resolves it. Like
    /// [`ComponentDef::unavailable_in`], only a numbered `YYYY.N` release can be ordered, so a
    /// branch name such as `master` is refused.
    pub fn check_release(&self, release: &str) -> Result<()> {
        let release = crate::kolla::releases::info(release.trim()).release;
        if !crate::kolla::releases::year_release(&release) {
            bail!(
                "OpenStack {release} is not a numbered release; a '{}' GPU needs {} or newer",
                self.mode_name(),
                self.min_release()
            );
        }
        if release.as_str() < self.min_release() {
            bail!(
                "OpenStack {release} cannot offer a '{}' GPU; it needs {} or newer",
                self.mode_name(),
                self.min_release()
            );
        }
        Ok(())
    }
    /// Refuses a plan whose release, services or count cannot express the mode. `release` may
    /// be a series name; it is compared after [`crate::kolla::releases::info`] resolves it.
    ///
    /// What the device binds is the *VM host's* kernel, which a plan does not know: every
    /// `mtty` farm is judged by each compute host's inspection when it is created or expanded
    /// (see `ensure_mtty_host` in `src/manager/mod.rs`). This coordinator's own kernel says
    /// nothing about a remote host, so it is deliberately not consulted here.
    pub fn validate(&self, release: &str, components: &[String]) -> Result<()> {
        if !(1..=MAX_GPU_COUNT).contains(&self.count) {
            bail!("a compute VM can offer 1-{MAX_GPU_COUNT} virtual GPUs");
        }
        if !components.iter().any(|c| c == "nova") {
            bail!("the GPU option needs Nova in the farm's components");
        }
        self.check_release(release)?;
        if self.mode == GpuMode::Mtty {
            // Nova owns that provider, so its inventory, traits and class are not farm's to
            // shape: nothing would carry them.
            if self.traits.is_some()
                || self.devices.is_some()
                || self
                    .resource_class
                    .as_deref()
                    .is_some_and(|rc| rc != DEFAULT_RESOURCE_CLASS)
            {
                bail!(
                    "'gpu.traits', 'gpu.devices' and 'gpu.resource_class' describe the 'simulated' mode's own provider; 'mtty' advertises the VGPU inventory Nova builds"
                );
            }
            return Ok(());
        }
        let devices = self.device_count();
        if !(1..=MAX_GPU_DEVICES).contains(&devices) {
            bail!("a compute VM can advertise its virtual GPUs on 1-{MAX_GPU_DEVICES} devices");
        }
        if devices > self.count {
            bail!(
                "{} virtual GPU(s) cannot fill {devices} devices; every device offers at least one",
                self.count
            );
        }
        for trait_name in self.traits() {
            custom_placement_name("trait", &trait_name)?;
        }
        if let Some(class) = &self.resource_class
            && !matches!(class.as_str(), "VGPU" | "PGPU")
        {
            custom_placement_name("resource class", class)?;
        }
        Ok(())
    }
}

/// The memory, power and clock envelope a virtual GPU reports. The guest derives
/// utilization, temperature, fan speed and the remaining clocks from its load sample
/// with integer arithmetic, so one profile name is all the metadata carries.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub enum GpuProfile {
    /// 8 GiB, 150 W.
    #[default]
    #[serde(rename = "sim-8g")]
    Sim8g,
    /// 16 GiB, 250 W.
    #[serde(rename = "sim-16g")]
    Sim16g,
    /// 40 GiB, 350 W.
    #[serde(rename = "sim-40g")]
    Sim40g,
    /// 80 GiB, 400 W.
    #[serde(rename = "sim-80g")]
    Sim80g,
}
impl GpuProfile {
    /// The API and metadata name, `sim-8g`.
    pub fn id(self) -> &'static str {
        match self {
            Self::Sim8g => "sim-8g",
            Self::Sim16g => "sim-16g",
            Self::Sim40g => "sim-40g",
            Self::Sim80g => "sim-80g",
        }
    }
    pub fn from_id(id: &str) -> Option<Self> {
        Some(match id {
            "sim-8g" => Self::Sim8g,
            "sim-16g" => Self::Sim16g,
            "sim-40g" => Self::Sim40g,
            "sim-80g" => Self::Sim80g,
            _ => return None,
        })
    }
    /// Framebuffer memory the guest reports, in MiB.
    pub fn memory_mib(self) -> u32 {
        match self {
            Self::Sim8g => 8192,
            Self::Sim16g => 16384,
            Self::Sim40g => 40960,
            Self::Sim80g => 81920,
        }
    }
    /// Board power limit the guest reports, in whole watts.
    pub fn power_limit_w(self) -> u32 {
        match self {
            Self::Sim8g => 150,
            Self::Sim16g => 250,
            Self::Sim40g => 350,
            Self::Sim80g => 400,
        }
    }
    /// Idle and boost SM clock, in MHz.
    pub fn clocks_mhz(self) -> (u32, u32) {
        match self {
            Self::Sim8g => (300, 1500),
            Self::Sim16g => (300, 1800),
            Self::Sim40g => (300, 1950),
            Self::Sim80g => (300, 2000),
        }
    }
    /// Memory clock the guest reports, in MHz.
    pub fn memory_clock_mhz(self) -> u32 {
        match self {
            Self::Sim8g => 7000,
            Self::Sim16g => 8000,
            Self::Sim40g => 9000,
            Self::Sim80g => 10000,
        }
    }
    pub const ALL: [Self; 4] = [Self::Sim8g, Self::Sim16g, Self::Sim40g, Self::Sim80g];
}

/// How a virtual GPU's utilization moves through one period. farm samples the
/// pattern into a small table; the guest picks a sample from the current second, so
/// the guest needs no trigonometry and the portal's preview draws the same table.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum GpuPattern {
    /// The middle of `min` and `max`.
    #[default]
    Constant,
    Sine,
    Triangle,
    /// `max` for the first half of the period, `min` for the second.
    Square,
    /// Rises from `min` to `max`, then starts over.
    Sawtooth,
    /// Deterministic pseudo-random samples, so the same load draws the same table.
    Random,
    /// `min` except for one sample at `max` in the middle of the period.
    Spike,
}
impl GpuPattern {
    pub fn id(self) -> &'static str {
        match self {
            Self::Constant => "constant",
            Self::Sine => "sine",
            Self::Triangle => "triangle",
            Self::Square => "square",
            Self::Sawtooth => "sawtooth",
            Self::Random => "random",
            Self::Spike => "spike",
        }
    }
    pub fn from_id(id: &str) -> Option<Self> {
        Some(match id {
            "constant" => Self::Constant,
            "sine" => Self::Sine,
            "triangle" => Self::Triangle,
            "square" => Self::Square,
            "sawtooth" => Self::Sawtooth,
            "random" => Self::Random,
            "spike" => Self::Spike,
            _ => return None,
        })
    }
    pub const ALL: [Self; 7] = [
        Self::Constant,
        Self::Sine,
        Self::Triangle,
        Self::Square,
        Self::Sawtooth,
        Self::Random,
        Self::Spike,
    ];
}

/// The load a portal sets on one instance. farm turns it into a sample table and
/// writes it to the instance's Nova metadata; the guest script reads that table.
/// Nothing computes on the values: this exists to exercise monitoring paths.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(default, deny_unknown_fields)]
pub struct GpuLoad {
    pub profile: GpuProfile,
    pub pattern: GpuPattern,
    /// Lowest utilization, percent.
    #[schema(minimum = 0, maximum = 100)]
    pub min: u8,
    /// Highest utilization, percent.
    #[schema(minimum = 0, maximum = 100)]
    pub max: u8,
    /// Seconds in one full pattern.
    #[schema(minimum = 1, maximum = 3600)]
    pub period_s: u32,
    /// Share of framebuffer memory reported as used, percent.
    #[schema(minimum = 0, maximum = 100)]
    pub memory_ratio: u8,
}
impl Default for GpuLoad {
    fn default() -> Self {
        Self {
            profile: GpuProfile::Sim8g,
            pattern: GpuPattern::Sine,
            min: 10,
            max: 90,
            period_s: 60,
            memory_ratio: 30,
        }
    }
}

/// A virtual GPU instance as the portal's load editor lists it. `gpu_count` comes
/// from the flavor's resource class (or 1 when it asks for none, because the guest
/// script defaults to one virtual GPU).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct FarmInstance {
    pub id: String,
    pub name: String,
    pub status: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub project: String,
    pub flavor: String,
    /// Virtual GPUs the flavor asks for, 0 when it asks for no GPU resource.
    pub gpu_count: u32,
    /// Resource classes the flavor requires, `VGPU: 1`.
    pub resources: BTreeMap<String, u32>,
    /// The load farm applied, or one decoded from the instance's metadata after a
    /// coordinator lost its state.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub load: Option<GpuLoad>,
    /// Whether the instance's metadata currently carries `farm_gpu_*`.
    pub applied: bool,
    /// The `farm_gpu_*` keys currently set on the instance.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub properties: Vec<String>,
}

/// One instance's load plus the metadata keys farm found on it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct GpuLoadView {
    pub server: String,
    pub load: GpuLoad,
    pub applied: bool,
    /// `farm_gpu_*` keys currently set on the instance.
    pub properties: Vec<String>,
}

/// Refuses a name Placement would not take as a custom trait or resource class:
/// `CUSTOM_` and upper-case letters, digits or `_`.
fn custom_placement_name(kind: &str, name: &str) -> Result<()> {
    let valid = name.strip_prefix("CUSTOM_").is_some_and(|rest| {
        !rest.is_empty()
            && rest
                .chars()
                .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
    }) && name.len() <= 255;
    if !valid {
        bail!("a {kind} is 'CUSTOM_' and upper-case letters, digits or '_' (got '{name}')");
    }
    Ok(())
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
    /// The virtual GPU every compute VM offers; absent on farms without one, and on plans
    /// recorded before the option existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gpu: Option<GpuConfig>,
    /// Host PCI devices compute-1 receives through VFIO; absent on plans recorded before
    /// the option existed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pci_devices: Vec<String>,
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
    /// False while the service is listed for planning but cannot be deployed yet, or, when a
    /// release was asked for, cannot run in that release (see `reason`).
    pub available: bool,
    /// The oldest OpenStack release that runs the service; `None` when every release does.
    pub min_release: Option<String>,
    /// Guest memory the service adds to the VM that runs it, in MiB.
    pub footprint_mib: u32,
    /// True while `footprint_mib` is a planning estimate that no farm has measured yet.
    pub estimated: bool,
    /// Why the service cannot run in the requested release; `None` when it can, or when the
    /// catalog was asked without a release.
    pub reason: Option<String>,
}
#[derive(Clone, Debug, Serialize, ToSchema)]
pub struct Catalog {
    pub components: Vec<Component>,
    pub constraints: Vec<String>,
}

/// A service published on the all-in-one guest at `port_base + offset`.
pub struct PublicPort {
    /// The key the URL is listed under in a farm's `services`. Kolla names the service's
    /// public URL variable after it.
    pub id: &'static str,
    pub offset: u16,
    pub guest_port: u16,
    pub path: &'static str,
}

/// Host ports reserved per farm: `[port_base, port_base + PUBLIC_PORT_BLOCK)`.
///
/// Twenty holds the published offsets: 0 through 5 are the six original services, 6 through 8
/// are the infrastructure ports (MariaDB, RabbitMQ, and the RabbitMQ management console), and
/// 9 through 15 are the 2025.1 services (heat, barbican, designate, magnum, skyline). Offsets
/// 16 through 19 are spare. It still yields 450 blocks in `15000..24000` —
/// more than a host's memory budget can hold. Every [`PublicPort::offset`] must stay below
/// this, and offsets are append-only so an existing farm's public URLs never move when the
/// catalog grows. Raising the block also changes the grid of port blocks, so `allocate_ports`
/// rejects any overlap, not just an equal base.
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
    /// The oldest release that can run the service; `None` runs on every release.
    pub since: Option<&'static str>,
    /// Guest memory added by each optional service process group.
    pub footprint_mib: u32,
    /// Whether `footprint_mib` is an estimate rather than a measurement.
    pub estimated: bool,
    /// Public ports in offset order; a service may publish several, or none.
    pub public: &'static [PublicPort],
}
impl ComponentDef {
    /// Why `release` cannot run this service, or `None` when it can. `release` may be a series
    /// name; it is compared after [`crate::kolla::releases::info`] resolves it.
    fn unavailable_in(&self, release: &str) -> Option<String> {
        let since = self.since?;
        // Trimmed like `PlanRequest::check_release`. Only a numbered `YYYY.N` release can be
        // ordered against `since`; a branch name such as `master` cannot, so it is refused.
        let release = crate::kolla::releases::info(release.trim()).release;
        if !crate::kolla::releases::year_release(&release) {
            return Some(format!(
                "OpenStack {release} is not a numbered release; '{}' needs {since} or newer",
                self.id
            ));
        }
        (release.as_str() < since).then(|| {
            format!(
                "OpenStack {release} cannot run '{}'; it needs {since} or newer",
                self.id
            )
        })
    }
}
/// Catalog order is also deployment and display order; dependencies come first.
pub const COMPONENTS: &[ComponentDef] = &[
    ComponentDef {
        id: "keystone",
        required: true,
        depends_on: &[],
        requires_data_plane: false,
        available: true,
        since: None,
        footprint_mib: 0,
        estimated: false,
        public: &[PublicPort {
            id: "keystone",
            offset: 0,
            guest_port: 5000,
            path: "/v3",
        }],
    },
    ComponentDef {
        id: "glance",
        required: true,
        depends_on: &["keystone"],
        requires_data_plane: false,
        available: true,
        since: None,
        footprint_mib: 0,
        estimated: false,
        public: &[PublicPort {
            id: "glance",
            offset: 1,
            guest_port: 9292,
            path: "",
        }],
    },
    ComponentDef {
        id: "placement",
        required: false,
        depends_on: &["keystone"],
        requires_data_plane: false,
        available: true,
        since: None,
        footprint_mib: 256,
        estimated: false,
        public: &[PublicPort {
            id: "placement",
            offset: 2,
            guest_port: 8780,
            path: "",
        }],
    },
    ComponentDef {
        id: "horizon",
        required: false,
        depends_on: &["keystone"],
        requires_data_plane: false,
        available: true,
        since: None,
        footprint_mib: 512,
        estimated: false,
        public: &[PublicPort {
            id: "horizon",
            offset: 3,
            guest_port: 80,
            path: "/",
        }],
    },
    ComponentDef {
        id: "neutron",
        required: false,
        depends_on: &["keystone"],
        requires_data_plane: true,
        available: true,
        since: None,
        // Measured: neutron-server, rpc-server and periodic worker use ~780 MiB; the four agents ~800 MiB.
        footprint_mib: 1024 + 1280,
        estimated: false,
        public: &[PublicPort {
            id: "neutron",
            offset: 4,
            guest_port: 9696,
            path: "",
        }],
    },
    ComponentDef {
        id: "nova",
        required: false,
        depends_on: &["keystone", "glance", "placement", "neutron"],
        requires_data_plane: true,
        available: true,
        since: None,
        // nova-api, scheduler, conductor, compute and libvirt, plus a small guest.
        footprint_mib: 2048,
        estimated: false,
        public: &[PublicPort {
            id: "nova",
            offset: 5,
            guest_port: 8774,
            path: "/v2.1",
        }],
    },
    // The services below need OpenStack 2025.1 or newer: their Kolla images and
    // `enable_<service>` flags exist from that release. Their footprints are estimates.
    ComponentDef {
        id: "heat",
        required: false,
        depends_on: &["keystone"],
        requires_data_plane: false,
        available: true,
        since: Some("2025.1"),
        // Measured 2026-11-18 on 2026.2: ~416 MiB idle (heat_engine 240.6 + heat_api 88.9 +
        // heat_api_cfn 86.6), with peak headroom included.
        footprint_mib: 512,
        estimated: false,
        public: &[
            PublicPort {
                id: "heat",
                offset: 9,
                guest_port: 8004,
                path: "",
            },
            PublicPort {
                id: "heat-cfn",
                offset: 10,
                guest_port: 8000,
                path: "",
            },
        ],
    },
    ComponentDef {
        id: "barbican",
        required: false,
        depends_on: &["keystone"],
        requires_data_plane: false,
        available: true,
        since: Some("2025.1"),
        footprint_mib: 320,
        estimated: true,
        public: &[PublicPort {
            id: "barbican",
            offset: 11,
            guest_port: 9311,
            path: "",
        }],
    },
    ComponentDef {
        id: "designate",
        required: false,
        depends_on: &["keystone", "neutron"],
        requires_data_plane: false,
        available: true,
        since: Some("2025.1"),
        // Includes the bind9 backend's name server.
        footprint_mib: 576,
        estimated: true,
        public: &[PublicPort {
            id: "designate",
            offset: 12,
            guest_port: 9001,
            path: "",
        }],
    },
    ComponentDef {
        id: "magnum",
        required: false,
        depends_on: &["keystone", "glance", "heat", "nova", "neutron"],
        requires_data_plane: false,
        available: true,
        since: Some("2025.1"),
        footprint_mib: 512,
        estimated: true,
        public: &[PublicPort {
            id: "magnum",
            offset: 13,
            guest_port: 9511,
            path: "",
        }],
    },
    ComponentDef {
        id: "skyline",
        required: false,
        depends_on: &["keystone"],
        requires_data_plane: false,
        available: true,
        since: Some("2025.1"),
        footprint_mib: 384,
        estimated: true,
        public: &[
            PublicPort {
                id: "skyline",
                offset: 14,
                guest_port: 9998,
                path: "",
            },
            PublicPort {
                id: "skyline-console",
                offset: 15,
                guest_port: 9999,
                path: "",
            },
        ],
    },
];
/// Farms recorded before component selection, and imported farms, run the original pair.
pub const BASE_COMPONENTS: &[&str] = &["keystone", "glance"];

/// The service catalog. With a `release`, each service says whether that release can run it;
/// without one, every service reports only its static availability.
pub fn catalog(release: Option<&str>) -> Catalog {
    Catalog {
        components: COMPONENTS
            .iter()
            .map(|c| {
                let reason = release.and_then(|release| c.unavailable_in(release));
                Component {
                    id: c.id.into(),
                    required: c.required,
                    depends_on: c.depends_on.iter().map(|d| (*d).into()).collect(),
                    requires_data_plane: c.requires_data_plane,
                    available: c.available && reason.is_none(),
                    min_release: c.since.map(Into::into),
                    footprint_mib: c.footprint_mib,
                    estimated: c.estimated,
                    reason,
                }
            })
            .collect(),
        constraints: vec![
            "Each farm runs all selected OpenStack services in one all-in-one VM.".into(),
            "Open vSwitch uses the userspace datapath, so tenant traffic is slower than on a kernel datapath.".into(),
            "Nova uses KVM when microsandbox exposes nested /dev/kvm in the guest, otherwise qemu TCG with slow instance boots.".into(),
            "A GPU farm gets a virtual device, not a real GPU. Both modes advertise the standard VGPU scheduling contract: 'simulated' puts a VGPU inventory and the requested traits on one child resource provider per fake GPU of each compute (OpenStack 2025.1 and newer), 'mtty' binds the guest kernel's mtty mediated device (2025.1 and newer, on a host prepared with farm's kernel; farm patches Nova's driver itself up to 2026.1).".into(),
            "Guest software in a GPU instance sees a device it cannot compute on: the device is a contract for scheduling tests, not hardware.".into(),
        ],
    }
}

/// Required services plus the request and every dependency, in catalog order, for a farm that
/// installs `release` (a series name is resolved as [`catalog`] does).
pub fn resolve_components(
    requested: &[String],
    release: &str,
) -> Result<Vec<&'static ComponentDef>> {
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
    if let Some(reason) = resolved.iter().find_map(|c| c.unavailable_in(release)) {
        bail!("{reason}");
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
        && resolve_components(&request.components, request.release_or_default())?
            .iter()
            .any(|c| c.id == "nova")
    {
        request.computes = 1;
    }
    layout(&request)
}

impl PlanRequest {
    /// Refuses a release older than this coordinator supports, unless it opted in. A request
    /// without a release installs this build's own; a snapshot restore keeps the snapshot's.
    pub fn check_release(&self) -> Result<()> {
        if let Some(release) = &self.release
            && let Some(why) = crate::kolla::releases::unsupported(release.trim())
        {
            bail!("{why}");
        }
        Ok(())
    }
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
        pci_devices: Vec::new(),
        components: components.to_vec(),
        computes: 0,
        compute,
        release: release.map(str::to_owned),
        layout: Some(FarmLayout::Control),
        gpu: None,
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
        && resolve_components(&request.components, request.release_or_default())?
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
    let components = resolve_components(&requested, request.release_or_default())?;
    let computes = request.computes;
    // The default size is stored as none, so a farm planned before sizes keeps its plan.
    let compute = match request.compute {
        Some(_) if computes == 0 && !control_only => bail!("compute size needs compute VMs"),
        Some(size) => Some(size.validate()?).filter(|size| *size != ComputeSize::default()),
        None => None,
    };
    // Every device is offered by a compute VM, so a GPU needs one to advertise it.
    let gpu = match &request.gpu {
        Some(gpu) => {
            if computes == 0 {
                bail!("a GPU is offered by compute VMs; add one or leave gpu out");
            }
            gpu.validate(
                request
                    .release
                    .as_deref()
                    .unwrap_or(crate::kolla::OPENSTACK_RELEASE),
                &components
                    .iter()
                    .map(|c| c.id.to_owned())
                    .collect::<Vec<_>>(),
            )?;
            Some(gpu.clone())
        }
        None => None,
    };
    // Passed-through devices belong to compute-1, so they need a compute VM too.
    let pci_devices = normalize_pci_devices(&request.pci_devices)?;
    if !pci_devices.is_empty() && computes == 0 {
        bail!("PCI passthrough is offered by compute VMs; add one or leave pci_devices out");
    }
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
    let mut warnings = catalog(None).constraints;
    if computes > 0 {
        warnings[0] = format!(
            "The farm runs its control plane, network node and Kolla on one control VM and Nova instances on {} compute VM(s), joined by the host's link switch.",
            computes
        );
    } else if control_only {
        warnings[0] = "The farm runs its control plane, network node and Kolla on one control VM; Nova instances need a compute VM added to it.".into();
    }
    if let Some(gpu) = &gpu {
        let what = match gpu.mode {
            GpuMode::Simulated => {
                let traits = gpu.traits();
                format!(
                    "each compute VM advertises {} virtual GPU(s) over {} fake GPU device(s) through resource class {} and trait(s) {}",
                    gpu.count,
                    gpu.device_count(),
                    gpu.resource_class(),
                    if traits.is_empty() {
                        "none".to_owned()
                    } else {
                        traits.join(", ")
                    }
                )
            }
            GpuMode::Mtty => format!(
                "each compute VM binds {} mediated device(s) as virtual GPUs",
                gpu.count
            ),
        };
        warnings.push(format!(
            "GPU '{}': {what}; this is a simulated device, not a real GPU, and guest software cannot compute on it.",
            gpu.mode_name()
        ));
    }
    Ok(Plan {
        components: components.iter().map(|c| c.id.to_owned()).collect(),
        release: request.release.clone(),
        computes,
        compute,
        gpu,
        pci_devices,
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
    /// A virtual GPU on every compute VM (see [`GpuConfig`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gpu: Option<GpuConfig>,
    /// Host PCI devices passed through to compute-1 (see [`PlanRequest::pci_devices`]).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pci_devices: Vec<String>,
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
    /// Start directly from an archive in a registered S3 bucket (requires `key`; mutually
    /// exclusive with `snapshot_id`). The archive must have a metadata file and cannot be a
    /// compute template.
    #[serde(default)]
    pub bucket: Option<String>,
    /// Object key in `bucket` to restore from.
    #[serde(default)]
    pub key: Option<String>,
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
        if self.bucket.is_some() != self.key.is_some() {
            bail!("restoring from a bucket requires both bucket and key");
        }
        if self.snapshot_id.is_some() && self.bucket.is_some() {
            bail!("restore from either snapshot_id or bucket/key, not both");
        }
        if let Some(bucket) = &self.bucket {
            validate_id(bucket)?;
        }
        if let Some(key) = &self.key {
            crate::s3::validate_key(key)?;
        }
        if let Some(snapshot) = &self.snapshot_id {
            validate_id(snapshot)?;
        }
        if self.snapshot_id.is_some() || self.bucket.is_some() {
            if !self.placement.is_empty() {
                bail!("a farm restored from a snapshot runs on one host; omit placement");
            }
            if !self.components.is_empty()
                || self.computes > 0
                || self.compute.is_some()
                || self.layout.is_some()
                || self.gpu.is_some()
                || !self.pci_devices.is_empty()
            {
                bail!(
                    "a farm restored from a snapshot runs the snapshot's services and layout; omit components, computes, compute, layout, gpu and pci_devices"
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
        let request = PlanRequest {
            components: self.components.clone(),
            computes: self.computes,
            compute: self.compute,
            release: self.release.clone(),
            layout: self.layout,
            gpu: self.gpu.clone(),
            pci_devices: self.pci_devices.clone(),
        };
        request.check_release()?;
        plan(&request)
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
            .flat_map(|c| c.public.iter())
            .map(|port| {
                (
                    port.id.to_owned(),
                    format!(
                        "http://{}:{}{}",
                        self.host,
                        self.port_base + port.offset,
                        port.path
                    ),
                )
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
    /// Its VM is stopping for a snapshot; [`FarmRecord::resume`] says what it was, so the
    /// freeze leaves the farm as ready (or as failed) as it found it.
    Snapshotting,
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
    /// The status the farm had before a snapshot job stopped its VM. Set only while the
    /// farm is `snapshotting`; the daemon keeps it with itself, not in the API.
    #[serde(skip)]
    pub resume: Option<FarmStatus>,
    /// Faults set on a multi-node farm's links, by VM (`control`, `compute-1`, ...).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub faults: BTreeMap<String, crate::link::Impair>,
    /// Virtual GPU loads the portal applied, by Nova server UUID. The guest reads the
    /// same values from instance metadata; this copy keeps the pattern the portal shows
    /// after a coordinator restart. A farm restored from a snapshot recovers the load
    /// from the instance's metadata instead.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub gpu_loads: BTreeMap<String, GpuLoad>,
    /// Other hosts that run some of a multi-node farm's compute VMs. `host_id` runs the rest.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub shares: Vec<HostShare>,
    /// SHA-256 of the token `host_id`'s switch joins the relay with, for a farm with shares.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uplink_sha256: Option<String>,
}

/// The OpenStack release a farm installs, named as the images and documents name it: the code
/// name beside the number (`2022.2` is `zed`), and whether this coordinator still supports it.
#[derive(Clone, Debug, PartialEq, Serialize, ToSchema)]
pub struct FarmOpenStack {
    /// What the farm records, `2022.2`.
    pub release: String,
    /// Its series name, `zed`; the same as `release` from 2023.1 on, where the names became
    /// `year.release`.
    pub series: String,
    /// The series' code name for display, `Zed` for 2022.2 and `Caracal` for 2024.1; empty
    /// when it has no published name. Display only: `series` keeps the name the images and
    /// the upper-constraints path use.
    pub codename: String,
    /// kolla-ansible keeps no branch for it any more, so it installs from the series' last tag.
    pub eol: bool,
    /// False below the oldest supported release, whatever the host's
    /// `FARM_ALLOW_UNSUPPORTED_RELEASES` says: such a farm keeps working but is out of support.
    pub supported: bool,
}

/// A farm as the API returns it: the stored record plus the release facts the portal labels the
/// farm with. Only the record is kept; the release comes from the release tables, so the names
/// live in one place and a farm's state stays what it was created with.
#[derive(Clone, Debug, Serialize, ToSchema)]
pub struct FarmView {
    #[serde(flatten)]
    pub farm: FarmRecord,
    /// Absent on a farm created before plans kept their release.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub openstack: Option<FarmOpenStack>,
}

impl FarmView {
    pub fn of(farm: FarmRecord) -> Self {
        let openstack = farm
            .plan
            .as_ref()
            .and_then(|plan| plan.release.as_deref())
            .map(|release| {
                let info = crate::kolla::releases::info(release);
                FarmOpenStack {
                    release: info.release,
                    series: info.series,
                    codename: info.codename,
                    eol: info.eol,
                    supported: crate::kolla::releases::in_supported_range(release),
                }
            });
        Self { farm, openstack }
    }
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
    /// A virtual GPU on every compute VM (see [`GpuConfig`]); only a clean install can take
    /// one, because a template's VMs boot with the plan they were frozen with.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gpu: Option<GpuConfig>,
    /// PCI devices passed through to compute-1 (see [`PlanRequest::pci_devices`]); like a GPU,
    /// only a clean install can take them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pci_devices: Vec<String>,
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
    /// The series name the images and the upper-constraints path use: `wallaby`, or the same
    /// `year.release` as `release` from 2023.1 on.
    #[serde(default)]
    pub series: String,
    /// Code name of the series for display, `Caracal`; empty when it has no published name.
    /// Display only: `series` keeps the name the images and constraints use.
    #[serde(default)]
    pub codename: String,
    pub local_control: usize,
    pub local_compute: usize,
    pub bucket_control: usize,
    pub bucket_compute: usize,
    /// Whether this coordinator can also install the release clean.
    pub installable: bool,
    /// End of life: kolla-ansible keeps no branch, so the release installs from its last tag
    /// and only where its older images run.
    #[serde(default)]
    pub eol: bool,
    /// CPU architectures its images are published for (`x86_64`, `aarch64`).
    #[serde(default)]
    pub arches: Vec<String>,
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
    /// msb snapshot reference on that host. A multi-node copy's is its control VM's, and
    /// `nodes` holds every VM's.
    pub reference: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub nodes: Vec<crate::farm::NodeSnapshot>,
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
            pci_devices: Vec::new(),
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
            bucket: None,
            key: None,
            placement: Default::default(),
            gpu: None,
        };
        assert!(legacy.validate().is_err());
        for unsupported in ["swift", "cinder"] {
            assert!(
                plan(&PlanRequest {
                    pci_devices: Vec::new(),
                    components: vec![unsupported.into()],
                    computes: 0,
                    compute: None,
                    layout: None,
                    release: None,
                    gpu: None,
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
            pci_devices: Vec::new(),
            components: components.iter().map(|c| (*c).into()).collect(),
            computes: 0,
            compute: None,
            layout: None,
            release: None,
            gpu: None,
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
            pci_devices: Vec::new(),
            components: components.iter().map(|c| (*c).into()).collect(),
            computes,
            compute: None,
            layout: None,
            release: None,
            gpu: None,
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
            pci_devices: Vec::new(),
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
            bucket: None,
            key: None,
            placement: Default::default(),
            gpu: None,
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
            pci_devices: Vec::new(),
            components: vec!["horizon".into()],
            computes: 2,
            compute: Some(size),
            layout: None,
            release: None,
            gpu: None,
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
            pci_devices: Vec::new(),
            components: alone.components.clone(),
            computes: 1,
            compute: alone.compute,
            layout: None,
            release: None,
            gpu: None,
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
    fn a_new_farm_needs_a_control_vm_and_a_compute() {
        // Any count a form offers makes the same shape: a control VM and computes.
        for computes in 1..=3 {
            assert!(require_multi_node(computes, None).is_ok());
        }
        let none = require_multi_node(0, None).unwrap_err();
        assert!(none.to_string().contains("at least one compute VM"));
        // The legacy layouts have no count to give them, and no request makes one.
        for layout in [FarmLayout::AllInOne, FarmLayout::Control] {
            let error = require_multi_node(0, Some(layout)).unwrap_err();
            assert!(error.to_string().contains("omit layout"));
            assert!(require_multi_node(1, Some(layout)).is_err());
        }
    }

    #[test]
    fn a_control_layout_is_not_an_all_in_one_farm() {
        let alone = |form: FarmLayout| {
            crate::model::layout(&PlanRequest {
                pci_devices: Vec::new(),
                components: vec![],
                computes: 0,
                compute: None,
                layout: Some(form),
                release: None,
                gpu: None,
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
            pci_devices: Vec::new(),
            components: vec![],
            computes: 1,
            compute: None,
            layout: None,
            release: None,
            gpu: None,
        })
        .unwrap();
        assert_eq!(
            control.memory_mib,
            u64::from(with_compute.nodes[0].memory_mib)
        );
        // Its first compute joins without changing the control VM.
        let next = layout(&PlanRequest {
            pci_devices: Vec::new(),
            components: control.components.clone(),
            computes: 1,
            compute: None,
            layout: None,
            release: None,
            gpu: None,
        })
        .unwrap();
        assert_eq!(
            serde_json::to_value(&next.nodes[..1]).unwrap(),
            serde_json::to_value(&control.nodes).unwrap()
        );
        // A layout never rides along with computes.
        assert!(
            layout(&PlanRequest {
                pci_devices: Vec::new(),
                components: vec![],
                computes: 1,
                compute: None,
                layout: Some(FarmLayout::Control),
                release: None,
                gpu: None,
            })
            .is_err()
        );
        // Nova cannot run on one all-in-one VM.
        assert!(
            layout(&PlanRequest {
                pci_devices: Vec::new(),
                components: vec!["nova".into()],
                computes: 0,
                compute: None,
                layout: Some(FarmLayout::AllInOne),
                release: None,
                gpu: None,
            })
            .is_err()
        );
    }

    #[test]
    fn compute_vms_take_the_requested_size() {
        let sized = |computes, compute| {
            plan(&PlanRequest {
                pci_devices: Vec::new(),
                components: vec![],
                computes,
                compute,
                layout: None,
                release: None,
                gpu: None,
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
                pci_devices: Vec::new(),
                components: vec![],
                computes: 0,
                compute: Some(size),
                layout: None,
                release: None,
                gpu: None,
            })
            .is_err()
        );
    }

    #[test]
    fn public_port_offsets_stay_inside_one_farm_block() {
        let mut offsets: Vec<u16> = COMPONENTS
            .iter()
            .flat_map(|c| c.public.iter().map(|p| p.offset))
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
        // Offsets 16..19 are free for optional services such as cinder and swift.
        assert_eq!(
            highest, 15,
            "the published set ends with the skyline console"
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

    #[test]
    fn the_api_view_names_the_release_a_farm_installs() {
        let farm = |release: Option<&str>| FarmRecord {
            id: "demo".into(),
            host_id: Some("local".into()),
            remote_generation: None,
            agent_sha256: None,
            observation_error: None,
            last_seen_at: None,
            status: FarmStatus::Created,
            plan: Some(Plan {
                pci_devices: Vec::new(),
                components: vec!["keystone".into()],
                release: release.map(str::to_owned),
                computes: 0,
                compute: None,
                gpu: None,
                nodes: vec![],
                memory_mib: 3072,
                disk_mib: 20480,
                warnings: vec![],
            }),
            exposure: None,
            owner: None,
            expires_at: None,
            nodes: vec![],
            phase: None,
            step: None,
            error: None,
            active_job: None,
            created_at: 0,
            updated_at: 0,
            snapshot_id: None,
            resume: None,
            faults: Default::default(),
            gpu_loads: Default::default(),
            shares: vec![],
            uplink_sha256: None,
        };
        // The series carries the name the images and release notes use, and an end-of-life
        // series below the supported minimum is flagged as both.
        let openstack = FarmView::of(farm(Some("2022.2")))
            .openstack
            .expect("the plan kept its release");
        assert_eq!(openstack.release, "2022.2");
        assert_eq!(openstack.series, "zed");
        assert!(openstack.eol && !openstack.supported);
        // A `year.release` names its own series, and the supported range starts at Caracal.
        let openstack = FarmView::of(farm(Some("2026.2"))).openstack.unwrap();
        assert_eq!(openstack.series, "2026.2");
        assert!(!openstack.eol && openstack.supported);
        // 2023.x has neither images nor a branch: outside support, but not end of life.
        let openstack = FarmView::of(farm(Some("2023.1"))).openstack.unwrap();
        assert!(!openstack.eol && !openstack.supported);
        // A plan recorded before releases were kept says nothing rather than guessing one.
        assert!(FarmView::of(farm(None)).openstack.is_none());
        // The recorded farm is still flat in the response, so clients read it as a farm.
        let value = serde_json::to_value(FarmView::of(farm(Some("2024.1")))).unwrap();
        assert_eq!(value["id"], "demo");
        assert_eq!(value["plan"]["release"], "2024.1");
        assert_eq!(value["openstack"]["release"], "2024.1");
        assert_eq!(value["openstack"]["supported"], true);
    }
    fn gpu(mode: GpuMode, count: u32) -> GpuConfig {
        GpuConfig {
            mode,
            count,
            ..GpuConfig::default()
        }
    }
    fn traits(list: &[&str]) -> Option<Vec<String>> {
        Some(list.iter().map(|t| (*t).to_owned()).collect())
    }
    fn nova_components() -> Vec<String> {
        ["keystone", "glance", "placement", "nova"]
            .map(String::from)
            .to_vec()
    }

    #[test]
    fn a_gpu_mode_is_offered_only_by_the_releases_that_can_express_it() {
        let with_nova = nova_components();
        // simulated writes the placement providers itself, so it needs the microversion
        // that gives a provider a parent (1.14, well before 2025.1) and nothing else.
        for release in ["2025.1", "2026.1", "2026.2"] {
            assert!(
                gpu(GpuMode::Simulated, 1)
                    .validate(release, &with_nova)
                    .is_ok(),
                "{release}"
            );
        }
        let why = gpu(GpuMode::Simulated, 1)
            .validate("2024.2", &with_nova)
            .unwrap_err()
            .to_string();
        assert!(why.contains("2025.1"), "{why}");
        // mtty's Nova support comes from farm's backport on 2025.1–2026.1 and from Nova itself
        // on 2026.2; a series name resolves to its release number.
        for release in ["2025.1", "2025.2", "2026.1", "2026.2"] {
            assert!(
                gpu(GpuMode::Mtty, 1).validate(release, &with_nova).is_ok(),
                "{release}"
            );
        }
        let why = gpu(GpuMode::Mtty, 1)
            .validate("2024.2", &with_nova)
            .unwrap_err()
            .to_string();
        assert!(why.contains("2025.1"), "{why}");
    }

    #[test]
    fn a_gpu_release_is_trimmed_before_it_is_compared() {
        let with_nova = nova_components();
        // A padded numbered release names the same release, as the service minimum does.
        assert!(
            gpu(GpuMode::Simulated, 1)
                .validate(" 2025.2", &with_nova)
                .is_ok()
        );
    }

    #[test]
    fn a_gpu_refuses_a_branch_name_as_a_release() {
        let with_nova = nova_components();
        // `master` is not a numbered release, so it cannot prove the GPU's minimum.
        let why = gpu(GpuMode::Simulated, 1)
            .validate("master", &with_nova)
            .unwrap_err()
            .to_string();
        assert!(why.contains("not a numbered release"), "{why}");
        let why = gpu(GpuMode::Mtty, 1)
            .validate(" master ", &with_nova)
            .unwrap_err()
            .to_string();
        assert!(why.contains("not a numbered release"), "{why}");
    }

    #[test]
    fn a_gpu_needs_nova_a_compute_and_at_most_the_kernel_s_devices() {
        let with_nova = nova_components();
        let no_nova = ["keystone", "glance"].map(String::from).to_vec();
        let why = gpu(GpuMode::Simulated, 1)
            .validate("2026.2", &no_nova)
            .unwrap_err()
            .to_string();
        assert!(why.contains("Nova"), "{why}");
        for count in [0, MAX_GPU_COUNT + 1] {
            let why = gpu(GpuMode::Simulated, count)
                .validate("2026.2", &with_nova)
                .unwrap_err()
                .to_string();
            assert!(why.contains("1-24"), "{why}");
        }
        assert!(
            gpu(GpuMode::Mtty, MAX_GPU_COUNT)
                .validate("2026.2", &with_nova)
                .is_ok()
        );

        // A plan without a compute VM has no resource provider to hold the device.
        let why = plan(&PlanRequest {
            pci_devices: Vec::new(),
            components: vec![],
            computes: 0,
            compute: None,
            release: None,
            layout: Some(FarmLayout::Control),
            gpu: Some(gpu(GpuMode::Simulated, 1)),
        })
        .unwrap_err()
        .to_string();
        assert!(why.contains("compute VMs"), "{why}");
    }

    #[test]
    fn the_simulated_modes_traits_devices_and_class_are_placement_names() {
        let with_nova = nova_components();
        let check = |gpu: GpuConfig| gpu.validate("2026.2", &with_nova).map(|_| ());

        // Every placement name farm writes is one Placement accepts: CUSTOM_ and capitals.
        for name in ["CUSTOM_FARM_VGPU", "CUSTOM_A", "CUSTOM_A1_B2", "CUSTOM__"] {
            assert!(
                check(GpuConfig {
                    traits: traits(&[name]),
                    ..gpu(GpuMode::Simulated, 2)
                })
                .is_ok(),
                "{name}"
            );
        }
        for name in ["CUSTOM_", "farm_vgpu", "CUSTOM-lower", "CUSTOM_bad", ""] {
            let why = check(GpuConfig {
                traits: traits(&[name]),
                ..gpu(GpuMode::Simulated, 2)
            })
            .unwrap_err()
            .to_string();
            assert!(why.contains("CUSTOM_"), "{name}: {why}");
        }
        // The class is VGPU or PGPU, or a custom one of the same shape.
        for class in ["VGPU", "PGPU", "CUSTOM_VGPU"] {
            assert!(
                check(GpuConfig {
                    resource_class: Some(class.into()),
                    ..gpu(GpuMode::Simulated, 1)
                })
                .is_ok(),
                "{class}"
            );
        }
        let why = check(GpuConfig {
            resource_class: Some("GPU".into()),
            ..gpu(GpuMode::Simulated, 1)
        })
        .unwrap_err()
        .to_string();
        assert!(why.contains("resource class"), "{why}");

        // Devices are fake cards: each must be fillable, and there are at most 24.
        assert!(
            check(GpuConfig {
                devices: Some(4),
                ..gpu(GpuMode::Simulated, 4)
            })
            .is_ok()
        );
        let why = check(GpuConfig {
            devices: Some(3),
            ..gpu(GpuMode::Simulated, 2)
        })
        .unwrap_err()
        .to_string();
        assert!(why.contains("cannot fill"), "{why}");
        let why = check(GpuConfig {
            devices: Some(0),
            ..gpu(GpuMode::Simulated, 2)
        })
        .unwrap_err()
        .to_string();
        assert!(why.contains("1-24"), "{why}");

        // mtty's provider is Nova's, so none of the three can shape it.
        for gpu in [
            GpuConfig {
                traits: traits(&["CUSTOM_X"]),
                ..gpu(GpuMode::Mtty, 1)
            },
            GpuConfig {
                devices: Some(2),
                ..gpu(GpuMode::Mtty, 1)
            },
            GpuConfig {
                resource_class: Some("CUSTOM_X".into()),
                ..gpu(GpuMode::Mtty, 1)
            },
        ] {
            let why = gpu.validate("2026.2", &with_nova).unwrap_err().to_string();
            assert!(why.contains("simulated"), "{why}");
        }
        // Naming VGPU outright is not a change.
        assert!(
            check(GpuConfig {
                resource_class: Some("VGPU".into()),
                ..gpu(GpuMode::Mtty, 1)
            })
            .is_ok()
        );
    }

    #[test]
    fn a_simulated_gpu_is_advertised_on_one_provider_per_fake_card() {
        // Nova names a card's provider <host>_pci_<domain>_<bus>_<slot>_<func>.
        assert_eq!(
            GpuConfig::provider_name("farm-compute-1", 0),
            "farm-compute-1_pci_0000_84_00_0"
        );
        assert_eq!(
            GpuConfig::provider_name("farm-compute-2", 1),
            "farm-compute-2_pci_0000_85_00_0"
        );
        // The count is split over the cards, the first ones taking the remainder.
        let totals = |count, devices| {
            GpuConfig {
                devices,
                count,
                ..gpu(GpuMode::Simulated, count)
            }
            .device_totals()
        };
        assert_eq!(totals(1, None), [1]);
        assert_eq!(totals(4, Some(1)), [4]);
        assert_eq!(totals(4, Some(2)), [2, 2]);
        assert_eq!(totals(5, Some(2)), [3, 2]);
        assert_eq!(totals(7, Some(3)), [3, 2, 2]);
        assert_eq!(totals(2, Some(2)), [1, 1]);
    }

    #[test]
    fn a_simulated_gpu_flavor_takes_the_class_and_the_traits() {
        let plain = gpu(GpuMode::Simulated, 2);
        assert_eq!(plain.resource_class(), "VGPU");
        assert_eq!(plain.traits(), [DEFAULT_VGPU_TRAIT]);
        assert_eq!(plain.flavor(), "farm.vgpu");
        assert_eq!(plain.device_count(), 1);
        let shaped = GpuConfig {
            traits: traits(&["CUSTOM_A", "CUSTOM_B"]),
            devices: Some(2),
            resource_class: Some("PGPU".into()),
            ..gpu(GpuMode::Simulated, 2)
        };
        assert_eq!(shaped.resource_class(), "PGPU");
        assert_eq!(shaped.traits(), ["CUSTOM_A", "CUSTOM_B"]);
        assert_eq!(shaped.device_count(), 2);
        // An empty list is how a request asks for no trait at all.
        assert!(
            GpuConfig {
                traits: traits(&[]),
                ..gpu(GpuMode::Simulated, 1)
            }
            .traits()
            .is_empty()
        );
        // The mediated device carries no farm trait and always advertises VGPU.
        let mtty = gpu(GpuMode::Mtty, 24);
        assert_eq!(mtty.resource_class(), "VGPU");
        assert!(mtty.traits().is_empty());
        assert_eq!(mtty.flavor(), "farm.vgpu");
    }

    #[test]
    fn a_plan_offers_mtty_without_asking_the_coordinators_kernel() {
        // The device is the VM host's kernel's, and planning does not know the host. Each
        // target host's inspection decides when the farm is created or expanded
        // (`ensure_mtty_host` in src/manager/mod.rs); this process's kernel never does.
        let planned = plan(&PlanRequest {
            pci_devices: Vec::new(),
            components: vec!["nova".into()],
            computes: 1,
            compute: None,
            release: Some("2026.2".into()),
            layout: None,
            gpu: Some(gpu(GpuMode::Mtty, 2)),
        })
        .unwrap();
        assert_eq!(planned.gpu, Some(gpu(GpuMode::Mtty, 2)));
        assert!(
            gpu(GpuMode::Simulated, 2)
                .validate("2026.2", &nova_components())
                .is_ok()
        );
        // The release window is still a plan-level refusal; the kernel is not.
        let why = gpu(GpuMode::Mtty, 1)
            .validate("2024.2", &nova_components())
            .unwrap_err()
            .to_string();
        assert!(why.contains("2025.1"), "{why}");
    }

    #[test]
    fn a_plan_keeps_the_gpu_it_validated() {
        let planned = plan(&PlanRequest {
            pci_devices: Vec::new(),
            components: vec!["nova".into()],
            computes: 1,
            compute: None,
            release: Some("2026.2".into()),
            layout: None,
            gpu: Some(gpu(GpuMode::Simulated, 4)),
        })
        .unwrap();
        assert_eq!(planned.gpu, Some(gpu(GpuMode::Simulated, 4)));
        assert_eq!(planned.computes, 1);
        let warnings = planned.warnings.join("\n");
        assert!(warnings.contains(DEFAULT_VGPU_TRAIT), "{warnings}");
        assert!(warnings.contains("not a real GPU"), "{warnings}");
        assert!(warnings.contains("fake GPU device"), "{warnings}");
        // The count is part of the request; a farm without one keeps no field at all.
        assert_eq!(
            serde_json::to_value(planned.gpu).unwrap(),
            serde_json::json!({"mode": "simulated", "count": 4})
        );
        let plain = plan(&PlanRequest {
            pci_devices: Vec::new(),
            components: vec!["nova".into()],
            computes: 1,
            compute: None,
            release: None,
            layout: None,
            gpu: None,
        })
        .unwrap();
        assert!(plain.gpu.is_none());
        assert!(
            !serde_json::to_value(&plain)
                .unwrap()
                .as_object()
                .unwrap()
                .contains_key("gpu")
        );
    }

    #[test]
    fn a_gpu_reads_with_a_default_count_and_old_state_reads_without_one() {
        // `gpu: {mode: simulated}` takes the default count of one and farm's own contract.
        let parsed: GpuConfig = serde_json::from_str(r#"{"mode":"simulated"}"#).unwrap();
        assert_eq!(parsed, gpu(GpuMode::Simulated, 1));
        assert_eq!(parsed.resource_class(), "VGPU");
        assert_eq!(parsed.traits(), [DEFAULT_VGPU_TRAIT]);
        assert_eq!(parsed.device_count(), 1);
        let parsed: GpuConfig = serde_json::from_str(r#"{"count":3}"#).unwrap();
        assert_eq!(parsed, gpu(GpuMode::Simulated, 3));
        // A farm recorded before the knobs existed keeps the defaults.
        let parsed: GpuConfig = serde_json::from_str(r#"{"mode":"simulated","count":2}"#).unwrap();
        assert_eq!(
            (parsed.traits, parsed.devices, parsed.resource_class),
            (None, None, None)
        );
        // The knobs round-trip, and the mode without them serializes as it always did.
        let shaped = GpuConfig {
            traits: traits(&["CUSTOM_A"]),
            devices: Some(2),
            resource_class: Some("PGPU".into()),
            ..gpu(GpuMode::Simulated, 4)
        };
        let value = serde_json::to_value(&shaped).unwrap();
        assert_eq!(
            value,
            serde_json::json!({
                "mode": "simulated",
                "count": 4,
                "traits": ["CUSTOM_A"],
                "devices": 2,
                "resource_class": "PGPU",
            })
        );
        assert_eq!(serde_json::from_value::<GpuConfig>(value).unwrap(), shaped);
        assert_eq!(
            serde_json::to_value(gpu(GpuMode::Mtty, 1)).unwrap(),
            serde_json::json!({"mode": "mtty", "count": 1})
        );

        // A plan and a farm written before the option existed deserialize with no GPU.
        let plan: Plan = serde_json::from_str(
            r#"{"components":["keystone"],"computes":0,"nodes":[],"memory_mib":0,"disk_mib":0,"warnings":[]}"#,
        )
        .unwrap();
        assert!(plan.gpu.is_none());
        let farm: FarmRecord = serde_json::from_str(
            r#"{"id":"old","status":"created","plan":{"components":["keystone"],"computes":1,"nodes":[],"memory_mib":0,"disk_mib":0,"warnings":[]},"exposure":null,"owner":null,"expires_at":null,"nodes":[],"phase":null,"error":null,"active_job":null,"created_at":0,"updated_at":0}"#,
        )
        .unwrap();
        assert!(farm.plan.unwrap().gpu.is_none());
    }

    fn names(components: &[&str]) -> Vec<String> {
        components.iter().map(|c| (*c).to_owned()).collect()
    }

    #[test]
    fn the_2025_services_are_refused_before_2025_1() {
        let heat = names(&["heat"]);
        let refused = resolve_components(&heat, "2024.2")
            .err()
            .expect("heat needs 2025.1")
            .to_string();
        assert_eq!(
            refused,
            "OpenStack 2024.2 cannot run 'heat'; it needs 2025.1 or newer"
        );
        assert!(resolve_components(&heat, "2025.1").is_ok());
        assert!(resolve_components(&heat, "2025.2").is_ok());
        // A padded year is the same release the planner trims to; a branch name is not a
        // numbered release, so it cannot prove the service's minimum and is refused.
        assert!(resolve_components(&heat, " 2025.2").is_ok());
        let branch = resolve_components(&heat, "master")
            .err()
            .expect("master is not a numbered release")
            .to_string();
        assert_eq!(
            branch,
            "OpenStack master is not a numbered release; 'heat' needs 2025.1 or newer"
        );
        // The planner refuses it too, so the API and the CLI agree.
        let before = PlanRequest {
            components: heat.clone(),
            release: Some("2024.2".into()),
            ..PlanRequest::default()
        };
        assert!(plan(&before).is_err());
        let after = PlanRequest {
            release: Some("2025.2".into()),
            ..before
        };
        let planned = plan(&after).unwrap();
        assert!(planned.components.iter().any(|c| c == "heat"));
    }

    #[test]
    fn magnum_brings_heat_and_its_other_dependencies() {
        let request = PlanRequest {
            components: names(&["magnum"]),
            release: Some("2025.2".into()),
            ..PlanRequest::default()
        };
        let planned = plan(&request).unwrap();
        for id in [
            "keystone",
            "glance",
            "placement",
            "neutron",
            "nova",
            "heat",
            "magnum",
        ] {
            assert!(
                planned.components.iter().any(|c| c == id),
                "magnum should bring {id}: {:?}",
                planned.components
            );
        }
        assert!(!planned.components.iter().any(|c| c == "barbican"));
        // Nova needs a compute VM, so the planner adds one beside the control VM.
        assert_eq!(planned.computes, 1);
    }

    #[test]
    fn selected_footprints_count_against_the_control_vm() {
        let release = Some("2025.2".to_owned());
        let bare = plan(&PlanRequest {
            release: release.clone(),
            ..PlanRequest::default()
        })
        .unwrap();
        let with = plan(&PlanRequest {
            components: names(&["heat", "barbican"]),
            release: release.clone(),
            ..PlanRequest::default()
        })
        .unwrap();
        // One all-in-one VM carries every footprint.
        assert_eq!(with.nodes.len(), 1);
        assert_eq!(with.memory_mib - bare.memory_mib, 512 + 320);

        // With computes, the control VM carries heat and magnum on top of Nova's set.
        let nova = plan(&PlanRequest {
            components: names(&["nova"]),
            computes: 1,
            release: release.clone(),
            ..PlanRequest::default()
        })
        .unwrap();
        let magnum = plan(&PlanRequest {
            components: names(&["magnum"]),
            computes: 1,
            release,
            ..PlanRequest::default()
        })
        .unwrap();
        assert_eq!(
            magnum.nodes[0].memory_mib - nova.nodes[0].memory_mib,
            512 + 512
        );
    }

    #[test]
    fn the_catalog_says_whether_a_release_can_run_each_service() {
        let find = |catalog: &Catalog, id: &str| {
            catalog
                .components
                .iter()
                .find(|c| c.id == id)
                .unwrap()
                .clone()
        };
        let old = catalog(Some("2024.2"));
        let heat = find(&old, "heat");
        assert!(!heat.available);
        assert_eq!(
            heat.reason.as_deref(),
            Some("OpenStack 2024.2 cannot run 'heat'; it needs 2025.1 or newer")
        );
        assert!(find(&old, "keystone").available);
        assert!(find(&old, "keystone").reason.is_none());

        let new = catalog(Some("2025.2"));
        assert!(find(&new, "heat").available);
        assert!(find(&new, "heat").reason.is_none());

        // Without a release every service is listed with its static availability.
        let any = catalog(None);
        let heat = find(&any, "heat");
        assert!(heat.available);
        assert_eq!(heat.min_release.as_deref(), Some("2025.1"));
        assert!(!heat.estimated);
        assert_eq!(heat.footprint_mib, 512);
        let nova = find(&any, "nova");
        assert!(nova.min_release.is_none());
        assert!(!nova.estimated);
    }

    #[test]
    fn every_published_port_has_its_own_id_and_offset() {
        let mut ids: Vec<&str> = COMPONENTS
            .iter()
            .flat_map(|c| c.public.iter().map(|p| p.id))
            .collect();
        let published = ids.len();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), published, "port ids must be unique");
        // Heat publishes two ports; the new services add seven ports in all.
        assert_eq!(published, 6 + 7);
    }
}

#[cfg(test)]
mod pci_tests {
    use super::*;

    #[test]
    fn a_pci_address_is_normalized_to_lowercase_and_bounded() {
        assert_eq!(normalize_pci_bdf("0000:04:00.3").unwrap(), "0000:04:00.3");
        assert_eq!(normalize_pci_bdf(" 0000:0A:1F.7 ").unwrap(), "0000:0a:1f.7");
        for bad in [
            "04:00.3",      // no domain
            "0000:04:00:3", // wrong separator
            "0000:04:20.0", // device above 0x1f
            "0000:04:00.8", // function above 7
            "0000:0g:00.0", // not hex
        ] {
            assert!(normalize_pci_bdf(bad).is_err(), "{bad} was accepted");
        }
    }

    #[test]
    fn a_duplicate_pci_device_is_refused_whatever_its_case() {
        let why =
            normalize_pci_devices(&["0000:04:00.3".into(), "0000:04:00.3".into()]).unwrap_err();
        assert!(why.to_string().contains("more than once"), "{why:#}");
        let why =
            normalize_pci_devices(&["0000:0A:00.0".into(), "0000:0a:00.0".into()]).unwrap_err();
        assert!(why.to_string().contains("more than once"), "{why:#}");
    }

    fn with_pci(computes: u8, devices: &[&str]) -> PlanRequest {
        PlanRequest {
            components: vec!["keystone".into(), "glance".into()],
            computes,
            pci_devices: devices.iter().map(|d| (*d).to_owned()).collect(),
            ..Default::default()
        }
    }

    #[test]
    fn passthrough_needs_a_compute_and_lands_in_the_plan_normalized() {
        let why = layout(&with_pci(0, &["0000:04:00.3"])).unwrap_err();
        assert!(
            why.to_string().contains("offered by compute VMs"),
            "{why:#}"
        );
        let planned = layout(&with_pci(1, &["0000:04:00.3"])).unwrap();
        assert_eq!(planned.pci_devices, vec!["0000:04:00.3".to_owned()]);
        let planned = layout(&with_pci(1, &["0000:0A:00.3"])).unwrap();
        assert_eq!(planned.pci_devices, vec!["0000:0a:00.3".to_owned()]);
        assert!(layout(&with_pci(1, &[])).unwrap().pci_devices.is_empty());
    }

    #[test]
    fn a_restored_farm_refuses_passthrough() {
        let request: CreateRequest =
            serde_json::from_str(r#"{"id":"restored","host_id":"local","snapshot_id":"snap-1","pci_devices":["0000:04:00.3"]}"#)
                .unwrap();
        let why = request.validate().unwrap_err();
        assert!(why.to_string().contains("pci_devices"), "{why:#}");
    }

    #[test]
    fn a_state_file_without_pci_fields_still_reads_and_stays_unchanged() {
        // A plan recorded before passthrough existed: no `pci_devices` key at all.
        let planned = plan(&PlanRequest {
            components: vec!["keystone".into(), "glance".into()],
            computes: 1,
            ..Default::default()
        })
        .unwrap();
        let mut value = serde_json::to_value(&planned).unwrap();
        assert!(
            value.get("pci_devices").is_none(),
            "an empty list must not be written: {value}"
        );
        value.as_object_mut().unwrap().remove("pci_devices");
        let read: Plan = serde_json::from_value(value).unwrap();
        assert!(read.pci_devices.is_empty());
        // And a plan that does carry devices round-trips them.
        let with = layout(&with_pci(1, &["0000:04:00.3"])).unwrap();
        let back: Plan = serde_json::from_value(serde_json::to_value(&with).unwrap()).unwrap();
        assert_eq!(back.pci_devices, with.pci_devices);
    }
}
