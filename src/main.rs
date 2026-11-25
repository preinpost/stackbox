#![recursion_limit = "256"]

use anyhow::{Result, bail};
use clap::{Args, Parser, Subcommand};
use stackbox::remote::{HostJobKind, RegisterLocalHost};
use stackbox::{
    api,
    kolla::Phase,
    manager::{self, ManagerOptions, StackboxManager, default_state_dir},
    model::{
        self, AddComputeRequest, ComputeSize, CreateRequest, DeployRequest, ExportRequest,
        GpuConfig, ImportRequest, Job, JobStatus, PlanRequest, SnapshotRecord, SnapshotRequest,
        SnapshotStatus,
    },
};
use std::{
    net::{IpAddr, SocketAddr},
    path::PathBuf,
};

#[derive(Parser)]
#[command(about = "Manage OpenStack stackboxes on microsandbox VMs")]
struct Cli {
    #[arg(long, global = true)]
    state_dir: Option<PathBuf>,
    /// Total guest-memory reservation budget; defaults to 80% of host RAM
    #[arg(long, global = true)]
    memory_budget_mib: Option<u64>,
    #[command(subcommand)]
    command: Command,
}
/// Size of every compute VM; an omitted flag keeps the default compute VM's value.
#[derive(Args)]
struct ComputeArgs {
    /// vCPUs of each compute VM (1-16)
    #[arg(long)]
    compute_cpus: Option<u8>,
    /// Memory of each compute VM in MiB (3072-65536)
    #[arg(long)]
    compute_memory_mib: Option<u32>,
    /// Each container disk of a compute VM in MiB (16384-524288); holds Nova's instance disks
    #[arg(long)]
    compute_disk_mib: Option<u32>,
}
impl ComputeArgs {
    fn size(&self) -> Option<ComputeSize> {
        if self.compute_cpus.is_none()
            && self.compute_memory_mib.is_none()
            && self.compute_disk_mib.is_none()
        {
            return None;
        }
        let default = ComputeSize::default();
        Some(ComputeSize {
            cpus: self.compute_cpus.unwrap_or(default.cpus),
            memory_mib: self.compute_memory_mib.unwrap_or(default.memory_mib),
            disk_mib: self.compute_disk_mib.unwrap_or(default.disk_mib),
        })
    }
}

/// A virtual GPU on each compute VM: no hardware GPU is involved.
#[derive(Clone, Copy, clap::ValueEnum)]
enum GpuModeArg {
    /// Advertise the standard VGPU inventory on a child provider per fake GPU (OpenStack
    /// 2025.1 and newer); the guest gets no device
    Simulated,
    /// Nova binds stackbox kernel's mtty mediated device (2025.1 and newer; stackbox patches
    /// Nova's driver itself before 2026.2)
    Mtty,
}

#[derive(Args)]
struct GpuArgs {
    /// Virtual GPU on every compute VM: 'simulated' (OpenStack 2025.1 and newer) or 'mtty'
    /// (2025.1 and newer, on hosts prepared with stackbox's kernel); not real hardware
    #[arg(long, value_enum)]
    gpu: Option<GpuModeArg>,
    /// Virtual GPUs per compute VM (1-24)
    #[arg(long, requires = "gpu", default_value_t = 1)]
    gpu_count: u32,
    /// Fake GPU devices each compute splits its virtual GPUs over (simulated only, 1-24);
    /// one resource provider each, as a vGPU host has one per card
    #[arg(long = "gpu-devices", requires = "gpu")]
    gpu_devices: Option<u32>,
    /// Trait the flavor requires besides the resource class (simulated only, repeatable;
    /// 'CUSTOM_*'; default CUSTOM_STACKBOX_VGPU)
    #[arg(long = "gpu-trait", requires = "gpu")]
    gpu_traits: Vec<String>,
    /// Resource class the flavor asks for (simulated only): VGPU, PGPU or CUSTOM_*
    #[arg(long = "gpu-resource-class", requires = "gpu")]
    gpu_resource_class: Option<String>,
}
impl GpuArgs {
    fn config(&self) -> Option<GpuConfig> {
        self.gpu.map(|mode| GpuConfig {
            mode: match mode {
                GpuModeArg::Simulated => model::GpuMode::Simulated,
                GpuModeArg::Mtty => model::GpuMode::Mtty,
            },
            count: self.gpu_count,
            traits: (!self.gpu_traits.is_empty()).then(|| self.gpu_traits.clone()),
            devices: self.gpu_devices,
            resource_class: self.gpu_resource_class.clone(),
        })
    }
}

/// PCI passthrough, shared by `plan` and `create`: host devices given to compute-1.
#[derive(Args)]
struct PciArgs {
    /// Host PCI device to pass through to compute-1, as DDDD:BB:DD.F (repeatable). It must
    /// already be bound to vfio-pci on that host, in an IOMMU group with no other driver; stackbox
    /// never unbinds a host driver. Needs a compute VM: a clean install, a snapshot restore of a
    /// multi-node snapshot, or the add-compute of compute-1 can take them.
    #[arg(long = "pci", value_name = "BDF")]
    pci: Vec<String>,
}

/// Options of `create --release`.
#[derive(Args)]
struct ReleaseArgs {
    /// Start from this OpenStack release's templates: the control VM from the newest
    /// control template, each compute from a compute template of the release when one
    /// boots on its host, otherwise clean. Without a control template the stackbox is
    /// installed clean, which only this coordinator's release can be.
    #[arg(long, conflicts_with_all = ["snapshot", "placement"])]
    release: Option<String>,
    /// Host of the next compute VM with --release (compute-1 first); repeatable.
    /// Omitted ones run on --host-id.
    #[arg(long = "compute-host", requires = "release")]
    compute_hosts: Vec<String>,
    /// Control template to use with --release instead of the newest one
    #[arg(long, requires = "release")]
    control_template: Option<String>,
    /// Compute template to use with --release instead of the newest one
    #[arg(long, requires = "release")]
    compute_template: Option<String>,
    /// Bucket to search for templates with --release; omitted searches every bucket
    #[arg(long, requires = "release")]
    bucket: Option<String>,
    /// With --release, print which template or clean install makes each VM and stop
    #[arg(long, requires = "release")]
    dry_run: bool,
    /// These GPU flags are shared, not release-only: `plan` takes them, and so does a plain
    /// `create`, which passes them to the stackbox it makes. A GPU stackbox ignores templates and
    /// installs clean, so with --release they mean a clean install of every VM.
    #[command(flatten)]
    gpu: GpuArgs,
    #[command(flatten)]
    pci: PciArgs,
}

#[derive(Subcommand)]
enum Command {
    /// Forward a local TCP port to a stackbox data store (MariaDB / RabbitMQ) over its Cloudflare relay URL
    Tunnel {
        /// The stackbox's relay URL for the service, such as https://relay/x/<stackbox>/<token>/tcp/mariadb
        url: String,
        /// Local address to listen on; defaults to 127.0.0.1:3306 (MariaDB) or 127.0.0.1:5672 (RabbitMQ)
        #[arg(short, long)]
        listen: Option<String>,
    },
    /// Run the REST API daemon (STACKBOX_API_TOKEN enables bearer authentication)
    Serve {
        #[arg(long, default_value = "127.0.0.1:10010")]
        bind: SocketAddr,
        /// Address on which newly created stackboxes publish OpenStack services
        #[arg(long, default_value = "127.0.0.1")]
        public_bind: IpAddr,
    },
    Catalog {
        /// Check each service against this OpenStack release, such as 2025.2.
        #[arg(long)]
        release: Option<String>,
        #[arg(long)]
        json: bool,
    },
    Plan {
        #[arg(long, value_delimiter = ',')]
        components: Vec<String>,
        /// Compute VMs beside a control VM (1-3); a new stackbox always has one
        #[arg(long, default_value_t = 1)]
        computes: u8,
        #[command(flatten)]
        compute: ComputeArgs,
        #[command(flatten)]
        gpu: GpuArgs,
        #[command(flatten)]
        pci: PciArgs,
        #[arg(long)]
        json: bool,
    },
    Create {
        id: String,
        /// Registered host to run the stackbox on (see the Hosts page or `GET /api/v1/hosts`)
        #[arg(long)]
        host_id: String,
        #[arg(long, value_delimiter = ',')]
        components: Vec<String>,
        /// Compute VMs beside a control VM (1-3); a new stackbox always has one. Adds Nova.
        #[arg(long, default_value_t = 1, conflicts_with = "snapshot")]
        computes: u8,
        #[command(flatten)]
        compute: ComputeArgs,
        #[arg(long, default_value = "localhost")]
        external_host: String,
        #[arg(long)]
        owner: Option<String>,
        /// UTC Unix seconds; metadata only
        #[arg(long)]
        expires_at: Option<u64>,
        /// Start from a snapshot instead of an empty VM (this machine's host only; replaces --components).
        /// A control template starts a legacy multi-node stackbox without computes; add them with add-compute.
        #[arg(long, conflicts_with = "components")]
        snapshot: Option<String>,
        /// Run a compute VM on another host, as `compute-2=<host id>`; repeatable.
        /// Needs STACKBOX_RELAY_URL on the coordinator.
        #[arg(long = "place", value_parser = parse_placement, conflicts_with = "snapshot")]
        placement: Vec<(String, String)>,
        #[command(flatten)]
        by_release: Box<ReleaseArgs>,
    },
    /// OpenStack releases with the templates stackboxes can start from
    Releases {
        /// CPU architecture; defaults to this host's
        #[arg(long)]
        arch: Option<String>,
        /// Count only this bucket's templates
        #[arg(long)]
        bucket: Option<String>,
        #[arg(long)]
        json: bool,
    },
    Deploy {
        id: String,
        #[arg(long, value_enum, default_value_t=Phase::Prepare)]
        from: Phase,
    },
    /// Add the next compute VM to a deployed multi-node stackbox
    AddCompute {
        id: String,
        /// Host for the new VM; omit to use the stackbox's primary host. Another host joins
        /// the stackbox through the coordinator's link relay.
        #[arg(long)]
        host_id: Option<String>,
        /// Boot the new VM from this compute template snapshot instead of installing it clean
        #[arg(long)]
        from_snapshot: Option<String>,
        #[command(flatten)]
        pci: PciArgs,
    },
    /// Check that a deployed stackbox's services answer again after its VM started from disk
    Settle { id: String },
    /// Build this machine's golden image now instead of with the first stackbox
    Golden {
        /// Only report whether it is built
        #[arg(long)]
        check: bool,
    },
    Status {
        id: String,
        #[arg(long)]
        json: bool,
    },
    List {
        #[arg(long)]
        json: bool,
    },
    Delete {
        id: String,
        /// Remove only the record, without asking the host; a VM it left there stays
        #[arg(long)]
        forget: bool,
    },
    Endpoints {
        id: String,
        #[arg(long)]
        json: bool,
    },
    /// Delay or drop the traffic of a multi-node stackbox's VM; with neither, heal it
    Fault {
        id: String,
        /// control, compute-1, ...
        node: String,
        /// Added to frames in each direction, so a round trip gains twice this
        #[arg(long, default_value_t = 0)]
        delay_ms: u64,
        /// Share of frames dropped in each direction; 1 cuts the VM off
        #[arg(long, default_value_t = 0.0)]
        loss: f64,
    },
    Logs {
        id: String,
        #[arg(long, default_value_t = 0)]
        after: u64,
    },
    /// Register, check and prepare the hosts stackboxes run on
    #[command(subcommand)]
    Host(HostCommand),
    /// Freeze stackboxes into snapshots, and move snapshots between hosts as archives
    #[command(subcommand)]
    Snapshot(SnapshotCommand),
    /// Print the OpenAPI document without opening the state store
    Openapi,
    /// Run this machine's stackbox agent. Without a subcommand it serves one request on
    /// stdin/stdout, which is how a coordinator runs its local host's agent.
    Agent {
        #[command(subcommand)]
        mode: Option<AgentMode>,
    },
}
#[derive(Subcommand)]
enum AgentMode {
    /// Keep this host connected to a coordinator, which then runs stackboxes here.
    /// Register the host first (Hosts → 에이전트 연결 등록) to get its token.
    Connect {
        /// Coordinator base URL, such as https://stackbox.example
        coordinator: String,
        /// Host token; defaults to the STACKBOX_AGENT_TOKEN environment variable
        #[arg(long)]
        token: Option<String>,
    },
    /// Run a deploy job the agent recorded, detached from the request that started it
    #[command(hide = true)]
    Worker { job: PathBuf },
    /// Run the host's link switch; the agent starts it
    #[command(hide = true)]
    Switch { dir: PathBuf },
    /// Carry a guest's link to its host's switch (in stackbox VMs, under stackbox-link@.service)
    #[command(hide = true)]
    Link {
        /// Only create the TAP device and give it its MTU and address
        #[arg(long)]
        setup: bool,
        conf: PathBuf,
    },
}
#[derive(Subcommand)]
enum HostCommand {
    /// Register this machine as a host (no SSH) and prepare it
    Local {
        #[arg(long, default_value = "local")]
        id: String,
    },
    List {
        #[arg(long)]
        json: bool,
    },
    /// Inspect a registered host
    Check { id: String },
    /// Prepare a registered host and boot a test VM on it
    Prepare { id: String },
}
#[derive(Subcommand)]
enum SnapshotCommand {
    /// Freeze a stackbox's VM; it stops for the capture and starts again
    Create {
        stackbox: String,
        id: String,
        #[arg(long)]
        note: Option<String>,
        /// Freeze only this VM of a deployed multi-node stackbox as a template: `control`, which
        /// `create --snapshot` starts a stackbox without computes from, or a compute VM
        /// (compute-1, ...) for add-compute --from-snapshot
        #[arg(long)]
        node: Option<String>,
    },
    List {
        #[arg(long)]
        json: bool,
    },
    /// Write a self-contained .msb archive (default: <state dir>/exports/<id>.msb),
    /// or upload it to a bucket registered in the portal or API
    Export {
        id: String,
        /// Archive file or existing directory
        #[arg(long, short, conflicts_with = "bucket")]
        output: Option<PathBuf>,
        /// Registered bucket to upload to
        #[arg(long)]
        bucket: Option<String>,
        /// Object key, with .msb appended when missing; defaults to <bucket prefix><id>.msb
        #[arg(long, requires = "bucket")]
        key: Option<String>,
    },
    /// Import an archive file, or an object of a registered bucket (--bucket and --key)
    Import {
        #[arg(required_unless_present = "bucket", conflicts_with = "bucket")]
        archive: Option<PathBuf>,
        #[arg(long, requires = "key")]
        bucket: Option<String>,
        #[arg(long, requires = "bucket")]
        key: Option<String>,
        /// Defaults to the archive's file name
        #[arg(long)]
        id: Option<String>,
    },
    /// List a registered bucket's archives with what their metadata files say
    Bucket {
        bucket: String,
        /// Write the bucket's index.json again from the metadata files first
        #[arg(long)]
        rebuild_index: bool,
        #[arg(long)]
        json: bool,
    },
    Delete {
        id: String,
        /// Remove only the record, without asking the host; a disk left there stays
        #[arg(long)]
        forget: bool,
    },
    /// Remove the copy of a snapshot another host keeps; templates are copied to hosts
    /// automatically when a stackbox or compute there starts from them
    Uncache {
        id: String,
        #[arg(long)]
        host: String,
    },
}
fn print_json(value: &impl serde::Serialize) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}
/// The kernel and GPU facts a host's last verification VM reported, for `stackbox host list`.
fn kernel_note(inspection: &stackbox::remote::HostInspection) -> String {
    let mut parts = Vec::new();
    if let Some(path) = &inspection.custom_kernel {
        parts.push(format!("custom kernel {path}"));
    } else if let Some(reason) = &inspection.custom_kernel_reason {
        parts.push(format!("stock kernel: {reason}"));
    }
    if let Some(gpu) = &inspection.gpu_kernel {
        if !gpu.kernel_release.is_empty() {
            parts.push(format!("kernel {}", gpu.kernel_release));
        }
        if gpu.vfio {
            parts.push("VFIO".into());
        }
        if gpu.vfio_mdev {
            parts.push("mdev".into());
        }
        for mdev_type in &gpu.mdev_types {
            parts.push(format!(
                "{}×{}",
                mdev_type.name, mdev_type.available_instances
            ));
        }
    } else if inspection.mtty_available {
        // A preparation from before the structured probe recorded only this.
        parts.push("mtty".into());
    }
    if parts.is_empty() {
        String::new()
    } else {
        format!(" · {}", parts.join(" · "))
    }
}
async fn wait_snapshot(manager: &StackboxManager, id: &str) -> Result<SnapshotRecord> {
    let snapshot = manager.wait_snapshot(id).await?;
    if snapshot.status == SnapshotStatus::Failed {
        bail!(
            "{}",
            snapshot.error.unwrap_or_else(|| "snapshot failed".into())
        );
    }
    if let Some(error) = &snapshot.error {
        bail!("{error}");
    }
    Ok(snapshot)
}
fn absolute(path: PathBuf) -> Result<String> {
    Ok(std::path::absolute(path)?.display().to_string())
}
/// A coordinator without Docker still installs agents when its build was released.
async fn load_published_agents(manager: &StackboxManager) {
    match manager.load_published_agents().await {
        Ok(0) => {}
        Ok(count) => eprintln!(
            "{count} published stackbox-agent(s) of build {} available",
            stackbox::BUILD_ID
        ),
        Err(error) => eprintln!("cannot read the published stackbox-agents: {error:#}"),
    }
}

async fn wait_host(manager: &StackboxManager, id: &str, kind: HostJobKind) -> Result<()> {
    let job = manager.start_host_job(id, kind)?;
    loop {
        let job = manager.host_job(&job.id)?;
        if !job.status.active() {
            for event in &job.events {
                println!("{}", event.message);
            }
            if job.status != JobStatus::Succeeded {
                bail!(
                    "{}",
                    job.error.unwrap_or_else(|| format!("{:?}", job.status))
                );
            }
            return Ok(());
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
}
async fn wait(manager: &StackboxManager, job: Job) -> Result<()> {
    println!("job {}: {:?}", job.id, job.kind);
    let job = manager.wait(&job.id).await?;
    if job.status != JobStatus::Succeeded {
        bail!(
            "{}",
            job.error.unwrap_or_else(|| format!("job {:?}", job.status))
        );
    }
    Ok(())
}
#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    if let Command::Tunnel { url, listen } = &cli.command {
        return stackbox::link::tcp_tunnel::run(url, listen.as_deref()).await;
    }
    // Planning and schema commands need neither msb nor the daemon's writer lock, nor a
    // usable stackbox kernel: computing a plan never boots a VM, and whether a host can bind the
    // `mtty` device is decided by that host's inspection when the stackbox is created.
    match &cli.command {
        Command::Catalog { release, json } => {
            let c = model::catalog(release.as_deref());
            if *json {
                return print_json(&c);
            }
            for component in c.components {
                let status = if component.required {
                    "required".to_owned()
                } else if component.available {
                    match &component.min_release {
                        Some(min) => format!("optional, {min} or newer"),
                        None => "optional".to_owned(),
                    }
                } else if let Some(reason) = &component.reason {
                    format!("unavailable: {reason}")
                } else {
                    "not deployable yet".to_owned()
                };
                let depends = if component.depends_on.is_empty() {
                    String::new()
                } else {
                    format!(", needs {}", component.depends_on.join(", "))
                };
                println!("{} ({status}{depends})", component.id);
            }
            for constraint in c.constraints {
                println!("{constraint}");
            }
            return Ok(());
        }
        Command::Plan {
            components,
            computes,
            compute,
            gpu,
            pci,
            json,
        } => {
            model::require_multi_node(*computes, None)?;
            let p = model::plan(&PlanRequest {
                components: components.clone(),
                computes: *computes,
                compute: compute.size(),
                release: None,
                layout: None,
                gpu: gpu.config(),
                pci_devices: pci.pci.clone(),
            })?;
            if *json {
                return print_json(&p);
            }
            println!(
                "{} nodes, {} MiB guest memory, {} MiB provisioned disk",
                p.nodes.len(),
                p.memory_mib,
                p.disk_mib
            );
            for node in p.nodes {
                println!(
                    "{:<12} {} MiB, {} vCPUs",
                    node.hostname(),
                    node.memory_mib,
                    node.cpus
                );
            }
            for warning in p.warnings {
                println!("{warning}");
            }
            return Ok(());
        }
        Command::Openapi => return print_json(&api::openapi_document()),
        Command::Golden { check } => {
            // Building a golden image boots a VM, so this is where the custom kernel of this
            // process has to reach the SDK first.
            stackbox::kernel::configure_sdk()?;
            let golden = stackbox::stackbox::golden::current();
            if *check {
                let built = stackbox::stackbox::golden::available(&golden).await?;
                println!("{golden}: {}", if built { "built" } else { "not built" });
                return Ok(());
            }
            stackbox::stackbox::golden::ensure(
                stackbox::stackbox::golden::DEFAULT_OS,
                &stackbox::stackbox::ALL_IN_ONE,
            )
            .await?;
            println!("{golden}: built");
            return Ok(());
        }
        Command::Agent {
            mode: Some(AgentMode::Connect { coordinator, token }),
        } => {
            stackbox::kernel::configure_sdk()?;
            let token = token
                .clone()
                .or_else(|| std::env::var("STACKBOX_AGENT_TOKEN").ok())
                .filter(|t| !t.trim().is_empty())
                .ok_or_else(|| anyhow::anyhow!("pass --token or set STACKBOX_AGENT_TOKEN"))?;
            let agent = std::sync::Arc::new(stackbox::remote::agent::Agent::local());
            return stackbox::remote::dial::run(coordinator, &token, agent).await;
        }
        Command::Agent {
            mode: Some(AgentMode::Worker { job }),
        } => {
            stackbox::kernel::configure_sdk()?;
            if let Err(error) = stackbox::remote::agent::serve_worker(job).await {
                eprintln!("stackbox agent worker: {error:#}");
                std::process::exit(1);
            }
            std::process::exit(0);
        }
        Command::Agent {
            mode: Some(AgentMode::Switch { dir }),
        } => return stackbox::link::host::serve_dir(dir).await,
        Command::Agent {
            mode: Some(AgentMode::Link { setup, conf }),
        } => {
            return if *setup {
                stackbox::link::guest::setup(conf).await
            } else {
                stackbox::link::guest::run(conf).await
            };
        }
        Command::Agent { mode: None } => {
            stackbox::kernel::configure_sdk()?;
            if let Err(error) = stackbox::remote::agent::serve_stdio().await {
                eprintln!("stackbox agent: {error}");
                std::process::exit(1);
            }
            // A finished shell can leave tokio's blocking stdin read waiting for input that never comes.
            std::process::exit(0);
        }
        _ => {}
    }
    // Every command below reaches microsandbox: it creates, boots or deletes VMs on this
    // machine or on a host whose agent it starts. Point the SDK at the custom kernel of
    // this process once, before the runtime is resolved.
    stackbox::kernel::configure_sdk()?;
    let mut options = ManagerOptions::local(cli.state_dir.unwrap_or_else(default_state_dir));
    if let Some(limit) = cli.memory_budget_mib {
        options.memory_budget_mib = limit;
    }
    let token = std::env::var("STACKBOX_API_TOKEN").ok();
    if let Command::Serve { bind, public_bind } = &cli.command {
        anyhow::ensure!(
            bind.ip().is_loopback() || token.is_some(),
            "non-loopback API binding requires STACKBOX_API_TOKEN"
        );
        anyhow::ensure!(
            token.as_ref().is_none_or(|t| !t.trim().is_empty()),
            "STACKBOX_API_TOKEN cannot be empty"
        );
        options.public_bind = *public_bind;
    }
    let manager = StackboxManager::open(options).await?;
    match cli.command {
        Command::Serve { bind, .. } => {
            load_published_agents(&manager).await;
            for job in manager.upgrade_stale_agents()? {
                eprintln!(
                    "reinstalling the stackbox-agent on {} (host job {})",
                    job.host_id, job.id
                );
            }
            api::serve(manager, bind, token).await?
        }
        Command::Create {
            id,
            host_id,
            components,
            computes,
            compute,
            external_host,
            owner,
            expires_at,
            snapshot,
            placement,
            by_release,
        } if by_release.release.is_some() => {
            let ReleaseArgs {
                release,
                compute_hosts,
                control_template,
                compute_template,
                bucket,
                dry_run,
                gpu,
                pci,
            } = *by_release;
            let request = model::ReleaseCreateRequest {
                id: id.clone(),
                release: release.unwrap_or_default(),
                host_id: Some(host_id),
                components,
                computes,
                compute_hosts,
                compute: compute.size(),
                control_template,
                compute_template,
                bucket,
                layout: None,
                external_host,
                owner,
                expires_at,
                gpu: gpu.config(),
                pci_devices: pci.pci,
            };
            if compute.size().is_some() {
                bail!("a stackbox made by release takes its compute size from its templates");
            }
            let _ = (snapshot, placement);
            model::require_multi_node(request.computes, request.layout)?;
            let plan = manager.preview_release(&request).await?;
            for line in manager::describe_release_plan(&plan) {
                println!("{line}");
            }
            if dry_run {
                return Ok(());
            }
            let job = manager.create_release(request).await?;
            wait(&manager, job).await?;
            print_json(&manager.get(&id)?)?;
        }
        Command::Create {
            id,
            host_id,
            components,
            computes,
            compute,
            external_host,
            owner,
            expires_at,
            snapshot,
            placement,
            by_release,
            ..
        } => {
            // A snapshot brings its own services and layout; a new stackbox has its computes.
            let computes = if snapshot.is_some() { 0 } else { computes };
            if snapshot.is_none() {
                model::require_multi_node(computes, None)?;
            }
            let job = manager.create(CreateRequest {
                id: id.clone(),
                host_id: Some(host_id),
                components,
                computes,
                compute: compute.size(),
                release: None,
                layout: None,
                external_host,
                owner,
                expires_at,
                snapshot_id: snapshot,
                bucket: None,
                key: None,
                placement: placement.into_iter().collect(),
                // The GPU and PCI flags ride in the release options, which plain creates share.
                gpu: by_release.gpu.config(),
                pci_devices: by_release.pci.pci.clone(),
            })?;
            wait(&manager, job).await?;
            print_json(&manager.get(&id)?)?;
        }
        Command::Releases { arch, bucket, json } => {
            let catalog = manager.release_catalog(arch, bucket).await?;
            if json {
                return print_json(&catalog);
            }
            for r in &catalog.releases {
                println!(
                    "{:<10} control {} here, {} in buckets · compute {} here, {} in buckets · clean install: {}",
                    r.release,
                    r.local_control,
                    r.bucket_control,
                    r.local_compute,
                    r.bucket_compute,
                    if r.installable { "yes" } else { "no" }
                );
            }
            for error in &catalog.unreachable {
                eprintln!("bucket not searched: {error}");
            }
        }
        Command::Deploy { id, from } => {
            wait(&manager, manager.deploy(&id, DeployRequest { from })?).await?;
        }
        Command::AddCompute {
            id,
            host_id,
            from_snapshot,
            pci,
        } => {
            let job = manager.add_compute(
                &id,
                AddComputeRequest {
                    host_id,
                    from_snapshot,
                    pci_devices: pci.pci,
                },
            )?;
            wait(&manager, job).await?;
            print_json(&manager.get(&id)?)?;
        }
        Command::Settle { id } => {
            wait(&manager, manager.settle(&id)?).await?;
            println!("{id}: services answer again");
        }
        Command::Delete { id, forget } => {
            let job = if forget {
                manager.forget(&id)?
            } else {
                manager.delete(&id)?
            };
            wait(&manager, job).await?;
            println!("deleted stackbox '{id}'");
        }
        Command::Status { id, json } => {
            let stackbox = manager.get(&id)?;
            if json {
                print_json(&stackbox)?;
            } else {
                println!("{}: {:?}", stackbox.id, stackbox.status);
                if let Some(error) = stackbox.error {
                    println!("{error}");
                }
                println!("{:<24} {:<10} IP", "NAME", "STATUS");
                for node in stackbox.nodes {
                    println!(
                        "{:<24} {:<10} {}",
                        node.name,
                        node.status,
                        node.ip.as_deref().unwrap_or("-")
                    );
                }
            }
        }
        Command::List { json } => {
            let stackboxes = manager.list()?;
            if json {
                print_json(&stackboxes)?;
            } else {
                for stackbox in stackboxes {
                    println!(
                        "{:<24} {:?} ({} nodes)",
                        stackbox.id,
                        stackbox.status,
                        stackbox.nodes.len()
                    );
                }
            }
        }
        Command::Endpoints { id, json: _ } => print_json(&manager.endpoints(&id).await?)?,
        Command::Fault {
            id,
            node,
            delay_ms,
            loss,
        } => {
            let stackbox = manager
                .impair(&id, &node, stackbox::link::Impair { delay_ms, loss })
                .await?;
            match stackbox.faults.get(&node) {
                Some(f) => println!("{id}/{node}: delay {} ms, loss {}", f.delay_ms, f.loss),
                None => println!("{id}/{node}: healed"),
            }
        }
        Command::Logs { id, after } => {
            for entry in manager.logs(&id, after, 1000)? {
                println!("[{}] {}", entry.id, entry.message);
            }
        }
        Command::Host(command) => match command {
            HostCommand::Local { id } => {
                if manager.host(&id).is_err() {
                    manager.register_local_host(RegisterLocalHost { id: id.clone() })?;
                }
                wait_host(&manager, &id, HostJobKind::Prepare).await?;
                println!("host '{id}' is ready; create stackboxes with --host-id {id}");
            }
            HostCommand::List { json } => {
                let hosts = manager.hosts()?;
                if json {
                    print_json(&hosts)?;
                } else {
                    for host in hosts {
                        let note = host
                            .inspection
                            .as_ref()
                            .map(kernel_note)
                            .unwrap_or_default();
                        println!("{:<24} {:?} {:?}{note}", host.id, host.kind, host.status);
                    }
                }
            }
            HostCommand::Check { id } => wait_host(&manager, &id, HostJobKind::Check).await?,
            HostCommand::Prepare { id } => {
                load_published_agents(&manager).await;
                wait_host(&manager, &id, HostJobKind::Prepare).await?
            }
        },
        Command::Snapshot(command) => match command {
            SnapshotCommand::Create {
                stackbox,
                id,
                note,
                node,
            } => {
                wait(
                    &manager,
                    manager.freeze(
                        &stackbox,
                        SnapshotRequest {
                            id: id.clone(),
                            note,
                            node,
                        },
                    )?,
                )
                .await?;
                print_json(&manager.snapshot(&id)?)?;
            }
            SnapshotCommand::List { json } => {
                let snapshots = manager.snapshots()?;
                if json {
                    print_json(&snapshots)?;
                } else {
                    for s in snapshots {
                        println!(
                            "{:<24} {:?} {:<8} from {} ({}{}{}){}",
                            s.id,
                            s.status,
                            s.arch,
                            s.source_stackbox.as_deref().unwrap_or("archive"),
                            s.plan.components.join(","),
                            if s.deployed { ", deployed" } else { "" },
                            match s.template {
                                Some(model::TemplateRole::Control) => ", control template",
                                Some(model::TemplateRole::Compute) => ", compute template",
                                None => "",
                            },
                            if s.copies.is_empty() {
                                String::new()
                            } else {
                                format!(
                                    " copied to {}",
                                    s.copies
                                        .iter()
                                        .map(|c| c.host_id.as_str())
                                        .collect::<Vec<_>>()
                                        .join(",")
                                )
                            }
                        );
                    }
                }
            }
            SnapshotCommand::Export {
                id,
                output,
                bucket,
                key,
            } => {
                manager.export_snapshot(
                    &id,
                    ExportRequest {
                        path: output.map(absolute).transpose()?,
                        bucket,
                        key,
                    },
                )?;
                let snapshot = wait_snapshot(&manager, &id).await?;
                let archive = snapshot.archive.expect("export records its archive");
                println!("{} ({} bytes)", archive.path, archive.size_bytes);
            }
            SnapshotCommand::Import {
                archive,
                bucket,
                key,
                id,
            } => {
                let record = manager.import_snapshot(ImportRequest {
                    path: archive.map(absolute).transpose()?,
                    bucket,
                    key,
                    id,
                })?;
                print_json(&wait_snapshot(&manager, &record.id).await?)?;
            }
            SnapshotCommand::Bucket {
                bucket,
                rebuild_index,
                json,
            } => {
                if rebuild_index {
                    let index = manager.rebuild_bucket_index(&bucket).await?;
                    eprintln!("indexed {} archives", index.entries.len());
                }
                let objects = manager.bucket_objects(&bucket).await?;
                if json {
                    print_json(&objects)?;
                } else {
                    for o in objects {
                        let about = match &o.metadata {
                            None => "no metadata".into(),
                            Some(m) => format!(
                                "{} {} {} ({})",
                                m.release.as_deref().unwrap_or("unknown release"),
                                match m.role {
                                    Some(model::TemplateRole::Control) => "control template",
                                    Some(model::TemplateRole::Compute) => "compute template",
                                    None => "stackbox snapshot",
                                },
                                m.arch,
                                m.components.join(",")
                            ),
                        };
                        println!("{:<40} {:>14} {about}", o.key, o.size_bytes);
                    }
                }
            }
            SnapshotCommand::Delete { id, forget } => {
                if forget {
                    manager.forget_snapshot(&id).await?;
                } else {
                    for copy in manager.delete_snapshot(&id).await? {
                        eprintln!("copy left on {copy}");
                    }
                }
                println!("deleted snapshot '{id}'");
            }
            SnapshotCommand::Uncache { id, host } => {
                print_json(&manager.remove_snapshot_copy(&id, &host).await?)?;
            }
        },
        Command::Catalog { .. }
        | Command::Plan { .. }
        | Command::Openapi
        | Command::Golden { .. }
        | Command::Agent { .. }
        | Command::Tunnel { .. } => {
            unreachable!()
        }
    }
    Ok(())
}

fn parse_placement(value: &str) -> Result<(String, String), String> {
    value
        .split_once('=')
        .map(|(node, host)| (node.to_owned(), host.to_owned()))
        .ok_or_else(|| "expected compute-<n>=<host id>".into())
}
