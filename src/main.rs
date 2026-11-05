use anyhow::{Result, bail};
use clap::{Args, Parser, Subcommand};
use farm::remote::{HostJobKind, RegisterLocalHost};
use farm::{
    api,
    kolla::Phase,
    manager::{self, FarmManager, ManagerOptions, default_state_dir},
    model::{
        self, AddComputeRequest, ComputeSize, CreateRequest, DeployRequest, ExportRequest,
        ImportRequest, Job, JobStatus, PlanRequest, SnapshotRecord, SnapshotRequest,
        SnapshotStatus,
    },
};
use std::{
    net::{IpAddr, SocketAddr},
    path::PathBuf,
};

#[derive(Parser)]
#[command(about = "Manage OpenStack farms on microsandbox VMs")]
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

/// Options of `create --release`.
#[derive(Args)]
struct ReleaseArgs {
    /// Start from this OpenStack release's templates: the control VM from the newest
    /// control template, each compute from a compute template of the release when one
    /// boots on its host, otherwise clean. Without a control template the farm is
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
}

#[derive(Subcommand)]
enum Command {
    /// Run the REST API daemon (FARM_API_TOKEN enables bearer authentication)
    Serve {
        #[arg(long, default_value = "127.0.0.1:10010")]
        bind: SocketAddr,
        /// Address on which newly created farms publish OpenStack services
        #[arg(long, default_value = "127.0.0.1")]
        public_bind: IpAddr,
    },
    Catalog {
        #[arg(long)]
        json: bool,
    },
    Plan {
        #[arg(long, value_delimiter = ',')]
        components: Vec<String>,
        /// Compute VMs beside a control VM (0-3); 0 is one all-in-one VM
        #[arg(long, default_value_t = 0)]
        computes: u8,
        #[command(flatten)]
        compute: ComputeArgs,
        #[arg(long)]
        json: bool,
    },
    Create {
        id: String,
        /// Registered host to run the farm on (see the Hosts page or `GET /api/v1/hosts`)
        #[arg(long)]
        host_id: String,
        #[arg(long, value_delimiter = ',')]
        components: Vec<String>,
        /// Compute VMs beside a control VM (0-3); 0 is one all-in-one VM. Adds Nova.
        #[arg(long, default_value_t = 0, conflicts_with = "snapshot")]
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
        /// A control template starts a multi-node farm without computes; add them with add-compute.
        #[arg(long, conflicts_with = "components")]
        snapshot: Option<String>,
        /// Run a compute VM on another host, as `compute-2=<host id>`; repeatable.
        /// Needs FARM_RELAY_URL on the coordinator.
        #[arg(long = "place", value_parser = parse_placement, conflicts_with = "snapshot")]
        placement: Vec<(String, String)>,
        #[command(flatten)]
        by_release: Box<ReleaseArgs>,
    },
    /// OpenStack releases with the templates farms can start from
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
    /// Add the next compute VM to a deployed multi-node farm
    AddCompute {
        id: String,
        /// Host for the new VM; omit to use the farm's primary host. Another host joins
        /// the farm through the coordinator's link relay.
        #[arg(long)]
        host_id: Option<String>,
        /// Boot the new VM from this compute template snapshot instead of installing it clean
        #[arg(long)]
        from_snapshot: Option<String>,
    },
    /// Check that a deployed farm's services answer again after its VM started from disk
    Settle { id: String },
    /// Build this machine's golden image now instead of with the first farm
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
    /// Delay or drop the traffic of a multi-node farm's VM; with neither, heal it
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
    /// Register, check and prepare the hosts farms run on
    #[command(subcommand)]
    Host(HostCommand),
    /// Freeze farms into snapshots, and move snapshots between hosts as archives
    #[command(subcommand)]
    Snapshot(SnapshotCommand),
    /// Print the OpenAPI document without opening the state store
    Openapi,
    /// Run this machine's farm agent. Without a subcommand it serves one request on
    /// stdin/stdout, which is how a coordinator runs its local host's agent.
    Agent {
        #[command(subcommand)]
        mode: Option<AgentMode>,
    },
}
#[derive(Subcommand)]
enum AgentMode {
    /// Keep this host connected to a coordinator, which then runs farms here.
    /// Register the host first (Hosts → 에이전트 연결 등록) to get its token.
    Connect {
        /// Coordinator base URL, such as https://farm.example
        coordinator: String,
        /// Host token; defaults to the FARM_AGENT_TOKEN environment variable
        #[arg(long)]
        token: Option<String>,
    },
    /// Run a deploy job the agent recorded, detached from the request that started it
    #[command(hide = true)]
    Worker { job: PathBuf },
    /// Run the host's link switch; the agent starts it
    #[command(hide = true)]
    Switch { dir: PathBuf },
    /// Carry a guest's link to its host's switch (in farm VMs, under farm-link@.service)
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
    /// Freeze a farm's VM; it stops for the capture and starts again
    Create {
        farm: String,
        id: String,
        #[arg(long)]
        note: Option<String>,
        /// Freeze only this VM of a deployed multi-node farm as a template: `control`, which
        /// `create --snapshot` starts a farm without computes from, or a compute VM
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
    /// automatically when a farm or compute there starts from them
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
async fn wait_snapshot(manager: &FarmManager, id: &str) -> Result<SnapshotRecord> {
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
async fn load_published_agents(manager: &FarmManager) {
    match manager.load_published_agents().await {
        Ok(0) => {}
        Ok(count) => eprintln!(
            "{count} published farm-agent(s) of build {} available",
            farm::BUILD_ID
        ),
        Err(error) => eprintln!("cannot read the published farm-agents: {error:#}"),
    }
}

async fn wait_host(manager: &FarmManager, id: &str, kind: HostJobKind) -> Result<()> {
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
async fn wait(manager: &FarmManager, job: Job) -> Result<()> {
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
    // Planning and schema commands need neither msb nor the daemon's writer lock.
    match &cli.command {
        Command::Catalog { json } => {
            let c = model::catalog();
            if *json {
                return print_json(&c);
            }
            for component in c.components {
                let status = if component.required {
                    "required"
                } else if component.available {
                    "optional"
                } else {
                    "not deployable yet"
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
            json,
        } => {
            let p = model::plan(&PlanRequest {
                components: components.clone(),
                computes: *computes,
                compute: compute.size(),
                release: None,
                layout: None,
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
            let golden = farm::farm::golden::current();
            if *check {
                let built = farm::farm::golden::available(&golden).await?;
                println!("{golden}: {}", if built { "built" } else { "not built" });
                return Ok(());
            }
            farm::farm::golden::ensure(farm::farm::golden::DEFAULT_OS, &farm::farm::ALL_IN_ONE)
                .await?;
            println!("{golden}: built");
            return Ok(());
        }
        Command::Agent {
            mode: Some(AgentMode::Connect { coordinator, token }),
        } => {
            let token = token
                .clone()
                .or_else(|| std::env::var("FARM_AGENT_TOKEN").ok())
                .filter(|t| !t.trim().is_empty())
                .ok_or_else(|| anyhow::anyhow!("pass --token or set FARM_AGENT_TOKEN"))?;
            let agent = std::sync::Arc::new(farm::remote::agent::Agent::local());
            return farm::remote::dial::run(coordinator, &token, agent).await;
        }
        Command::Agent {
            mode: Some(AgentMode::Worker { job }),
        } => {
            if let Err(error) = farm::remote::agent::serve_worker(job).await {
                eprintln!("farm agent worker: {error:#}");
                std::process::exit(1);
            }
            std::process::exit(0);
        }
        Command::Agent {
            mode: Some(AgentMode::Switch { dir }),
        } => return farm::link::host::serve_dir(dir).await,
        Command::Agent {
            mode: Some(AgentMode::Link { setup, conf }),
        } => {
            return if *setup {
                farm::link::guest::setup(conf).await
            } else {
                farm::link::guest::run(conf).await
            };
        }
        Command::Agent { mode: None } => {
            if let Err(error) = farm::remote::agent::serve_stdio().await {
                eprintln!("farm agent: {error}");
                std::process::exit(1);
            }
            // A finished shell can leave tokio's blocking stdin read waiting for input that never comes.
            std::process::exit(0);
        }
        _ => {}
    }
    let mut options = ManagerOptions::local(cli.state_dir.unwrap_or_else(default_state_dir));
    if let Some(limit) = cli.memory_budget_mib {
        options.memory_budget_mib = limit;
    }
    let token = std::env::var("FARM_API_TOKEN").ok();
    if let Command::Serve { bind, public_bind } = &cli.command {
        anyhow::ensure!(
            bind.ip().is_loopback() || token.is_some(),
            "non-loopback API binding requires FARM_API_TOKEN"
        );
        anyhow::ensure!(
            token.as_ref().is_none_or(|t| !t.trim().is_empty()),
            "FARM_API_TOKEN cannot be empty"
        );
        options.public_bind = *public_bind;
    }
    let manager = FarmManager::open(options).await?;
    match cli.command {
        Command::Serve { bind, .. } => {
            load_published_agents(&manager).await;
            for job in manager.upgrade_stale_agents()? {
                eprintln!(
                    "reinstalling the farm-agent on {} (host job {})",
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
            };
            if compute.size().is_some() {
                bail!("a farm made by release takes its compute size from its templates");
            }
            let _ = (snapshot, placement);
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
            ..
        } => {
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
                placement: placement.into_iter().collect(),
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
        } => {
            let job = manager.add_compute(
                &id,
                AddComputeRequest {
                    host_id,
                    from_snapshot,
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
            println!("deleted farm '{id}'");
        }
        Command::Status { id, json } => {
            let farm = manager.get(&id)?;
            if json {
                print_json(&farm)?;
            } else {
                println!("{}: {:?}", farm.id, farm.status);
                if let Some(error) = farm.error {
                    println!("{error}");
                }
                println!("{:<24} {:<10} IP", "NAME", "STATUS");
                for node in farm.nodes {
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
            let farms = manager.list()?;
            if json {
                print_json(&farms)?;
            } else {
                for farm in farms {
                    println!(
                        "{:<24} {:?} ({} nodes)",
                        farm.id,
                        farm.status,
                        farm.nodes.len()
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
            let farm = manager
                .impair(&id, &node, farm::link::Impair { delay_ms, loss })
                .await?;
            match farm.faults.get(&node) {
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
                println!("host '{id}' is ready; create farms with --host-id {id}");
            }
            HostCommand::List { json } => {
                let hosts = manager.hosts()?;
                if json {
                    print_json(&hosts)?;
                } else {
                    for host in hosts {
                        println!("{:<24} {:?} {:?}", host.id, host.kind, host.status);
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
                farm,
                id,
                note,
                node,
            } => {
                wait(
                    &manager,
                    manager.freeze(
                        &farm,
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
                            s.source_farm.as_deref().unwrap_or("archive"),
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
                                    None => "farm snapshot",
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
        | Command::Agent { .. } => {
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
