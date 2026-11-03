//! One SSH request per process; no listener and no SSH credentials on the target.
use super::{
    job::Job,
    protocol::{Frame, MAX_FRAME, Operation, Reply, Request, TerminalFrame, VERSION},
};
use crate::{
    farm::{TerminalInput, TerminalIo, TerminalSize},
    manager::{Backend, MicrosandboxBackend},
    model::{Exposure, Plan, PlanRequest, layout, validate_id},
};
use anyhow::{Context, Result, bail, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    ffi::OsString,
    fs::{self, File, OpenOptions},
    io::Write,
    net::TcpListener,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

pub type Emitter = Arc<dyn Fn(Frame) -> Result<()> + Send + Sync>;
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Reservation {
    coordinator: String,
    generation: String,
    plan: Plan,
    exposure: Exposure,
    deleted: bool,
}
#[derive(Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Registry {
    farms: BTreeMap<String, Reservation>,
}
struct LockedRegistry {
    dir: PathBuf,
    state: Registry,
    _lock: AgentLock,
}
struct AgentLock(File);
impl Drop for AgentLock {
    fn drop(&mut self) {
        // A concurrent fork can briefly inherit this file before exec closes it.
        // Unlock explicitly so that child descriptor cannot prolong our lock.
        let _ = FileExt::unlock(&self.0);
    }
}
impl LockedRegistry {
    fn open(dir: &Path) -> Result<Self> {
        let lock = lock(dir, "registry.lock")?;
        let state = match fs::read(dir.join("registry.json")) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .context("invalid remote agent registry; refusing to overwrite it")?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Registry::default(),
            Err(e) => return Err(e.into()),
        };
        Ok(Self {
            dir: dir.into(),
            state,
            _lock: lock,
        })
    }
    fn save(&self) -> Result<()> {
        let path = self.dir.join("registry.json.tmp");
        let mut options = OpenOptions::new();
        options.create(true).truncate(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&path)?;
        file.write_all(&serde_json::to_vec(&self.state)?)?;
        file.sync_all()?;
        fs::rename(path, self.dir.join("registry.json"))?;
        File::open(&self.dir)?.sync_all()?;
        Ok(())
    }
    fn owned(&self, coordinator: &str, id: &str, generation: &str) -> Result<&Reservation> {
        let entry = self
            .state
            .farms
            .get(id)
            .context("remote farm reservation not found")?;
        ensure!(
            entry.coordinator == coordinator && entry.generation == generation,
            "remote farm belongs to another coordinator or generation"
        );
        Ok(entry)
    }
}
fn lock(dir: &Path, name: &str) -> Result<AgentLock> {
    fs::create_dir_all(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
    }
    let mut options = OpenOptions::new();
    options.create(true).truncate(false).read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(dir.join(name))?;
    file.try_lock_exclusive()
        .context("remote agent operation is already running; inspect it before retrying")?;
    Ok(AgentLock(file))
}

#[derive(Clone)]
pub struct Limits {
    pub memory_budget_mib: u64,
    pub available_mib: u64,
    pub virtualization: bool,
    /// Bind a new farm's public port block before reserving it.
    pub probe_ports: bool,
}
impl Limits {
    pub fn system() -> Self {
        let system = sysinfo::System::new_all();
        Self {
            memory_budget_mib: system.total_memory() / 1024 / 1024 * 80 / 100,
            // macOS counts reclaimable inactive pages as used, so its "available" memory
            // refuses VMs that would fit; there the reservation budget alone applies.
            available_mib: if cfg!(target_os = "macos") {
                u64::MAX
            } else {
                system.available_memory() / 1024 / 1024
            },
            virtualization: crate::remote::host::hypervisor_available(),
            probe_ports: true,
        }
    }
}
/// Where a deploy job's worker runs. Either way it records the job in the same files.
#[derive(Clone)]
enum Runner {
    /// A task of this process, which the job does not outlive.
    Inline,
    /// A process of its own: this binary as `agent worker <job dir>`.
    Detached,
    /// A process of its own from this command, with the job directory appended.
    #[cfg(test)]
    Command(Vec<OsString>),
}
#[derive(Clone)]
pub struct Agent {
    dir: PathBuf,
    backend: Arc<dyn Backend>,
    limits: Limits,
    runner: Runner,
}
impl Agent {
    /// An agent that runs inside the coordinator's process, for tests. Its deploys run as
    /// tasks of that process.
    pub fn in_process(dir: PathBuf, backend: Arc<dyn Backend>) -> Self {
        Self {
            dir,
            backend,
            limits: Limits {
                memory_budget_mib: u64::MAX,
                available_mib: u64::MAX,
                virtualization: true,
                // Parallel tests would contend for the same real ports.
                probe_ports: false,
            },
            runner: Runner::Inline,
        }
    }
    pub fn local() -> Self {
        let dir = local_dir();
        Self {
            backend: Arc::new(MicrosandboxBackend::new(&dir)),
            dir,
            limits: Limits::system(),
            runner: Runner::Detached,
        }
    }
    /// Destroys every farm VM this host's agent created, whatever coordinator reserved it,
    /// and forgets them. Used when the coordinator removes the host.
    async fn purge(&self, request: &Request) -> Result<Reply> {
        let reserved: Vec<String> = {
            let registry = LockedRegistry::open(&self.dir)?;
            registry.state.farms.keys().cloned().collect()
        };
        let mut purged = 0;
        let mut failed = Vec::new();
        for id in &reserved {
            match self.backend.delete(id.clone()).await {
                Ok(()) => purged += 1,
                Err(error) => failed.push(format!("{id}: {error:#}")),
            }
        }
        // Forget only what is gone, so a retry still knows what to clean up.
        let mut registry = LockedRegistry::open(&self.dir)?;
        for id in &reserved {
            if !failed
                .iter()
                .any(|message| message.starts_with(&format!("{id}:")))
            {
                registry.state.farms.remove(id);
            }
        }
        registry.save()?;
        let _ = request;
        ensure!(
            failed.is_empty(),
            "failed to remove {} reserved farm(s): {}",
            failed.len(),
            failed.join("; ")
        );
        Ok(Reply::Purged { farms: purged })
    }

    /// Reserve capacity and public ports for a new farm on this host. A replay of the same
    /// request returns the farm's existing nodes instead.
    async fn reserve(
        &self,
        request: &Request,
        id: &str,
        generation: &str,
        requested: &Plan,
        exposure: &Exposure,
    ) -> Result<Option<Vec<crate::farm::FarmNode>>> {
        ensure!(
            self.limits.virtualization,
            "this host has no usable hypervisor (/dev/kvm on Linux)"
        );
        // The coordinator applies plan()'s policy; restored snapshots keep an older layout.
        let canonical = layout(&PlanRequest {
            components: requested.components.clone(),
            computes: requested.computes,
        })?;
        ensure!(
            serde_json::to_value(requested)? == serde_json::to_value(&canonical)?,
            "farm plan differs from the supported layout"
        );
        // The coordinator picks the bind address (0.0.0.0 for remote hosts, its public bind
        // locally); binding the whole block below proves the ports are free.
        ensure!(
            exposure.port_base >= 15000
                && exposure.port_base <= 23990
                && exposure.port_base.is_multiple_of(10),
            "exposure requires a valid port block"
        );
        // Reuse public-host validation.
        crate::model::CreateRequest {
            id: id.into(),
            host_id: None,
            components: requested.components.clone(),
            computes: requested.computes,
            external_host: exposure.host.clone(),
            owner: None,
            expires_at: None,
            snapshot_id: None,
        }
        .validate()?;
        let _create_lock = lock(&self.dir, "create.lock")?;
        let mut registry = LockedRegistry::open(&self.dir)?;
        let observed = self.backend.scan().await?;
        if let Some(entry) = registry.state.farms.get(id) {
            ensure!(
                entry.coordinator == request.coordinator_id,
                "farm id is owned by another coordinator"
            );
            if !entry.deleted {
                ensure!(
                    entry.generation == generation,
                    "farm id is reserved by an earlier generation"
                );
                ensure!(
                    serde_json::to_value(&entry.plan)? == serde_json::to_value(requested)?
                        && serde_json::to_value(&entry.exposure)?
                            == serde_json::to_value(exposure)?,
                    "replayed create differs from the original reservation"
                );
                let nodes = observed.get(id).cloned().unwrap_or_default();
                ensure!(
                    nodes.len() == requested.nodes.len()
                        && nodes.iter().all(|n| n.status == "running"),
                    "earlier create is incomplete; inspect and delete it explicitly before recreating"
                );
                return Ok(Some(nodes));
            }
            ensure!(
                entry.generation != generation,
                "this farm generation was already deleted"
            );
        }
        ensure!(
            observed.get(id).is_none_or(Vec::is_empty),
            "existing VM labels conflict with this farm; refusing to adopt or remove them"
        );
        let reserved: u64 = registry
            .state
            .farms
            .values()
            .filter(|r| !r.deleted)
            .map(|r| r.plan.memory_mib)
            .sum();
        let foreign: u64 = observed
            .iter()
            .filter(|(id, _)| !registry.state.farms.get(*id).is_some_and(|r| !r.deleted))
            .flat_map(|(_, nodes)| nodes)
            .map(|n| u64::from(n.memory_mib))
            .sum();
        ensure!(
            reserved
                .saturating_add(foreign)
                .saturating_add(requested.memory_mib)
                <= self.limits.memory_budget_mib,
            "remote host memory reservation budget exceeded"
        );
        ensure!(
            requested.memory_mib <= self.limits.available_mib,
            "remote host available memory is insufficient"
        );
        ensure!(
            !registry
                .state
                .farms
                .values()
                .any(|r| !r.deleted && r.exposure.port_base == exposure.port_base),
            "remote public ports are reserved by another farm"
        );
        let sockets = (exposure.port_base..exposure.port_base + 10)
            .filter(|_| self.limits.probe_ports)
            .map(|p| TcpListener::bind((exposure.bind, p)))
            .collect::<std::io::Result<Vec<_>>>()
            .context("the farm's public port block is occupied on this host")?;
        registry.state.farms.insert(
            id.into(),
            Reservation {
                coordinator: request.coordinator_id.clone(),
                generation: generation.into(),
                plan: requested.clone(),
                exposure: exposure.clone(),
                deleted: false,
            },
        );
        registry.save()?;
        drop(registry);
        drop(sockets);
        Ok(None)
    }

    fn admit(request: &Request) -> Result<()> {
        ensure!(
            request.version == VERSION,
            "incompatible farm-agent protocol"
        );
        uuid::Uuid::parse_str(&request.coordinator_id).context("invalid coordinator identity")?;
        if let Some((id, generation)) = request.operation.scope() {
            validate_id(id)?;
            uuid::Uuid::parse_str(generation).context("invalid farm generation")?;
        }
        if let Operation::Deploy { job_id, .. } | Operation::Attach { job_id, .. } =
            &request.operation
        {
            uuid::Uuid::parse_str(job_id).context("invalid job id")?;
        }
        Ok(())
    }

    /// Relay a shell in the farm's VM: `TerminalFrame` lines in, `Frame::Terminal` out.
    /// It takes no farm lock, so a shell can watch a running deployment.
    pub async fn terminal<R>(&self, request: &Request, input: R, emit: Emitter) -> Result<Reply>
    where
        R: tokio::io::AsyncBufRead + Unpin,
    {
        use tokio::io::{AsyncBufReadExt, AsyncReadExt};
        Self::admit(request)?;
        let Operation::Shell {
            farm_id: id,
            generation,
            size,
        } = &request.operation
        else {
            bail!("not a shell request");
        };
        let size = size.validate()?;
        {
            let registry = LockedRegistry::open(&self.dir)?;
            let entry = registry.owned(&request.coordinator_id, id, generation)?;
            ensure!(!entry.deleted, "remote farm was deleted");
        }
        emit(Frame::Attached)?;
        let (input_tx, input_rx) = tokio::sync::mpsc::channel(64);
        let (output_tx, output_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
        let session = self.backend.terminal(
            id.clone(),
            size,
            TerminalIo {
                input: input_rx,
                output: output_tx,
            },
        );
        // Owns the receiver, so a lost coordinator unblocks the shell's output instead of stalling it.
        let relay = async {
            let mut output_rx = output_rx;
            while let Some(data) = output_rx.recv().await {
                emit(Frame::Terminal {
                    data: STANDARD.encode(data),
                })?;
            }
            Ok::<_, anyhow::Error>(())
        };
        let pump = async { tokio::join!(session, relay) };
        // Ends at EOF, which drops `input_tx` and hangs the shell up.
        let read = async move {
            let mut input = input;
            let mut line = String::new();
            loop {
                line.clear();
                let read = (&mut input)
                    .take(MAX_FRAME as u64 + 1)
                    .read_line(&mut line)
                    .await?;
                if read == 0 {
                    return Ok::<_, anyhow::Error>(());
                }
                ensure!(line.len() <= MAX_FRAME, "terminal frame exceeds limit");
                let message = match serde_json::from_str(&line).context("invalid terminal frame")? {
                    TerminalFrame::Input { data } => TerminalInput::Data(
                        STANDARD.decode(data).context("invalid terminal input")?,
                    ),
                    TerminalFrame::Resize { rows, cols } => {
                        TerminalInput::Resize(TerminalSize { rows, cols }.validate()?)
                    }
                };
                if input_tx.send(message).await.is_err() {
                    return Ok(());
                }
            }
        };
        tokio::pin!(pump, read);
        let (session, relay) = tokio::select! {
            outcome = &mut pump => outcome,
            read = &mut read => {
                let outcome = pump.await;
                read?;
                outcome
            }
        };
        session?;
        relay?;
        Ok(Reply::Done)
    }

    /// The live reservation `request`'s coordinator holds for this farm generation.
    fn owned(&self, request: &Request, id: &str, generation: &str) -> Result<Reservation> {
        let registry = LockedRegistry::open(&self.dir)?;
        let entry = registry
            .owned(&request.coordinator_id, id, generation)?
            .clone();
        ensure!(!entry.deleted, "farm was deleted");
        Ok(entry)
    }

    /// The reservation a deploy of this farm generation runs on.
    fn deployable(&self, request: &Request, id: &str, generation: &str) -> Result<Reservation> {
        let entry = self.owned(request, id, generation)?;
        ensure!(
            crate::farm::layout(&entry.plan.nodes).is_ok(),
            "legacy multi-node farm cannot be deployed; create a new farm"
        );
        Ok(entry)
    }

    /// Start the deploy as job `job_id`, or join the job already started under that id, and
    /// follow it to its result. Ending this request leaves the job running.
    async fn deploy(&self, request: &Request, job_id: &str, emit: Emitter) -> Result<Reply> {
        let (id, generation) = request
            .operation
            .scope()
            .context("deploy without a farm id")?;
        // Refuse a foreign or impossible deploy before it leaves a job behind.
        self.deployable(request, id, generation)?;
        let (job, created) = Job::create(&self.dir, job_id, request)?;
        if created {
            if let Err(error) = self.launch(&job).await {
                job.finish(Err(anyhow::anyhow!("{error:#}")))?;
                return Err(error);
            }
        } else {
            ensure!(
                job.request.coordinator_id == request.coordinator_id
                    && serde_json::to_value(&job.request.operation)?
                        == serde_json::to_value(&request.operation)?,
                "job {job_id} is another deploy"
            );
        }
        job.follow(0, &emit).await
    }

    async fn launch(&self, job: &Job) -> Result<()> {
        let command: Vec<OsString> = match &self.runner {
            Runner::Inline => {
                let claim = job.claim()?;
                let (agent, job) = (self.clone(), job.clone());
                tokio::spawn(async move {
                    let _claim = claim;
                    if let Err(error) = agent.run_job(&job).await {
                        eprintln!("farm agent: cannot record the deploy job: {error:#}");
                    }
                });
                return Ok(());
            }
            Runner::Detached => vec![
                std::env::current_exe()
                    .context("cannot locate the agent binary")?
                    .into(),
                "agent".into(),
                "worker".into(),
            ],
            #[cfg(test)]
            Runner::Command(command) => command.clone(),
        };
        job.spawn(&command).await
    }

    /// Run a claimed job and record its result. Bounded like the request it replaced.
    async fn run_job(&self, job: &Job) -> Result<()> {
        let seconds = job.request.operation.seconds();
        let outcome = tokio::time::timeout(
            Duration::from_secs(seconds),
            self.run_deploy(&job.request, job.recorder()?),
        )
        .await
        .unwrap_or_else(|_| {
            Err(anyhow::anyhow!(
                "deploy timed out after {seconds} s; inspect the farm's VM before retrying"
            ))
        });
        job.finish(outcome)
    }

    async fn run_deploy(&self, request: &Request, emit: Emitter) -> Result<Reply> {
        Self::admit(request)?;
        let Operation::Deploy {
            farm_id: id,
            generation,
            from,
            ..
        } = &request.operation
        else {
            bail!("not a deploy job");
        };
        let _farm_lock = lock(&self.dir, &format!("{id}.lock"))?;
        let entry = self.deployable(request, id, generation)?;
        let _deploy_lock = lock(&self.dir, "deploy.lock")?;
        self.backend
            .deploy(
                id.into(),
                *from,
                Some(entry.exposure),
                entry.plan.components.clone(),
                progress(emit),
            )
            .await?;
        Ok(Reply::Done)
    }

    /// Snapshot storage on this host. Paths name files on this host, which the coordinator
    /// only sends to the agent on its own machine.
    async fn snapshot_operation(&self, operation: &Operation) -> Result<Reply> {
        let path = |value: &str| {
            let path = PathBuf::from(value);
            ensure!(
                path.is_absolute(),
                "snapshot archive paths must be absolute"
            );
            Ok(path)
        };
        match operation {
            Operation::ExportSnapshot { reference, output } => {
                self.backend
                    .export_snapshot(reference.clone(), path(output)?)
                    .await?;
                Ok(Reply::Done)
            }
            Operation::ImportSnapshot {
                archive,
                snapshot_id,
            } => {
                crate::model::validate_snapshot_id(snapshot_id)?;
                Ok(Reply::Imported {
                    snapshot: self
                        .backend
                        .import_snapshot(path(archive)?, snapshot_id.clone())
                        .await?,
                })
            }
            Operation::RemoveSnapshot { reference } => {
                self.backend.remove_snapshot(reference.clone()).await?;
                Ok(Reply::Done)
            }
            _ => bail!("not a snapshot storage operation"),
        }
    }

    pub async fn execute(&self, request: Request, emit: Emitter) -> Result<Reply> {
        Self::admit(&request)?;
        ensure!(
            !matches!(request.operation, Operation::Shell { .. }),
            "shell sessions need a terminal stream"
        );
        if matches!(request.operation, Operation::Hello) {
            return Ok(Reply::Hello {
                version: VERSION,
                arch: std::env::consts::ARCH.into(),
                supports_single_vm: true,
                build_id: crate::BUILD_ID.into(),
            });
        }
        if matches!(request.operation, Operation::Scan) {
            let registry = LockedRegistry::open(&self.dir)?;
            let owned: Vec<_> = registry
                .state
                .farms
                .iter()
                .filter(|(_, r)| r.coordinator == request.coordinator_id && !r.deleted)
                .map(|(id, _)| id.clone())
                .collect();
            drop(registry);
            let mut inventory = self.backend.scan().await?;
            let farms = owned
                .into_iter()
                .map(|id| {
                    let nodes = inventory.remove(&id).unwrap_or_default();
                    (id, nodes)
                })
                .collect();
            return Ok(Reply::Inventory { farms });
        }
        // Host-wide checks have no farm id either.
        if matches!(request.operation, Operation::Inspect) {
            return Ok(Reply::Inspection {
                inspection: crate::remote::host::inspect().await,
            });
        }
        if matches!(request.operation, Operation::VerifyVm) {
            crate::remote::host::smoke(&uuid::Uuid::new_v4().to_string()).await?;
            return Ok(Reply::Done);
        }
        if matches!(request.operation, Operation::InstallMsb) {
            // Stands in for flock(1), which macOS lacks; Linux installs also take it in the script.
            let _install_lock = lock(&self.dir, "msb-install.lock")?;
            crate::remote::host::install_msb().await?;
            return Ok(Reply::Done);
        }
        // A host-wide cleanup has no farm id, so it runs before the per-farm scope below.
        if matches!(request.operation, Operation::Purge) {
            // Serialize with creates so a reservation cannot appear mid-purge.
            let _create_lock = lock(&self.dir, "create.lock")?;
            return self.purge(&request).await;
        }
        if matches!(
            request.operation,
            Operation::ExportSnapshot { .. }
                | Operation::ImportSnapshot { .. }
                | Operation::RemoveSnapshot { .. }
        ) {
            return self.snapshot_operation(&request.operation).await;
        }
        // A deploy outlives this request; its worker takes the farm's locks instead.
        if let Operation::Deploy { job_id, .. } = &request.operation {
            return self.deploy(&request, job_id, emit).await;
        }
        if let Operation::Attach { job_id, offset } = &request.operation {
            let job = Job::open(&self.dir, job_id)?;
            // Its reservation was checked when the job started; the job names its owner.
            ensure!(
                job.request.coordinator_id == request.coordinator_id,
                "job {job_id} belongs to another coordinator"
            );
            return job.follow(*offset, &emit).await;
        }
        let (id, generation) = request
            .operation
            .scope()
            .context("farm operation without a farm id")?;
        let _farm_lock = lock(&self.dir, &format!("{id}.lock"))?;
        match &request.operation {
            Operation::Create {
                plan: requested,
                exposure,
                ..
            } => {
                if let Some(nodes) = self
                    .reserve(&request, id, generation, requested, exposure)
                    .await?
                {
                    return Ok(Reply::Nodes { nodes });
                }
                emit(Frame::Output {
                    message: "Host capacity and public ports reserved; creating the farm VM\n"
                        .into(),
                })?;
                Ok(Reply::Nodes {
                    nodes: self
                        .backend
                        .create(id.into(), requested.clone(), exposure.clone())
                        .await?,
                })
            }
            Operation::Restore {
                reference,
                plan: requested,
                exposure,
                ..
            } => {
                if let Some(nodes) = self
                    .reserve(&request, id, generation, requested, exposure)
                    .await?
                {
                    return Ok(Reply::Nodes { nodes });
                }
                Ok(Reply::Nodes {
                    nodes: self
                        .backend
                        .restore(
                            id.into(),
                            reference.clone(),
                            requested.clone(),
                            exposure.clone(),
                        )
                        .await?,
                })
            }
            Operation::Delete { .. } => {
                let _create_lock = lock(&self.dir, "create.lock")?;
                let mut registry = LockedRegistry::open(&self.dir)?;
                if !registry.state.farms.contains_key(id) {
                    // A failed preflight may not have created a reservation. Never delete foreign labels.
                    ensure!(
                        self.backend.scan().await?.get(id).is_none_or(Vec::is_empty),
                        "unowned remote VMs exist; refusing deletion"
                    );
                    return Ok(Reply::Done);
                }
                registry.owned(&request.coordinator_id, id, generation)?;
                drop(registry);
                self.backend.delete(id.into()).await?;
                ensure!(
                    self.backend.scan().await?.get(id).is_none_or(Vec::is_empty),
                    "remote VMs remain after deletion"
                );
                registry = LockedRegistry::open(&self.dir)?;
                registry.owned(&request.coordinator_id, id, generation)?;
                registry.state.farms.get_mut(id).unwrap().deleted = true;
                registry.save()?;
                Ok(Reply::Done)
            }
            Operation::Endpoints { .. } => {
                let entry = self.owned(&request, id, generation)?;
                Ok(Reply::Endpoints {
                    endpoints: self
                        .backend
                        .endpoints(id.into(), Some(entry.exposure), entry.plan.components)
                        .await?,
                })
            }
            Operation::Settle { rebind, .. } => {
                let entry = self.owned(&request, id, generation)?;
                self.backend
                    .settle(
                        id.into(),
                        rebind.clone(),
                        entry.plan.components,
                        progress(emit),
                    )
                    .await?;
                Ok(Reply::Done)
            }
            Operation::Freeze {
                snapshot_id,
                labels,
                ..
            } => {
                self.owned(&request, id, generation)?;
                crate::model::validate_snapshot_id(snapshot_id)?;
                Ok(Reply::Frozen {
                    snapshot: self
                        .backend
                        .freeze(id.into(), snapshot_id.clone(), labels.clone())
                        .await?,
                })
            }
            Operation::Impair { node, impair, .. } => {
                let entry = self.owned(&request, id, generation)?;
                ensure!(
                    entry.plan.nodes.iter().any(|spec| {
                        spec.role != crate::farm::Role::AllInOne
                            && crate::farm::suffix(spec.role, spec.index).as_deref()
                                == Some(node.as_str())
                    }),
                    "farm '{id}' has no linked VM '{node}'"
                );
                impair.validate()?;
                self.backend
                    .impair(id.into(), node.clone(), *impair)
                    .await?;
                Ok(Reply::Done)
            }
            _ => bail!("unsupported operation"),
        }
    }
}

fn local_dir() -> PathBuf {
    let home = std::env::var_os("HOME").unwrap_or_else(|| ".".into());
    PathBuf::from(home).join(".local/share/farm/worker-v1")
}

/// `agent worker <job dir>`: run a job recorded under this agent's directory to its result.
/// The agent starts it detached; it is not meant to be run by hand.
pub async fn serve_worker(dir: &Path) -> Result<()> {
    let agent_dir = dir
        .parent()
        .and_then(Path::parent)
        .context("not a job directory")?;
    work(dir, Arc::new(MicrosandboxBackend::new(agent_dir))).await
}
async fn work(dir: &Path, backend: Arc<dyn Backend>) -> Result<()> {
    let id = dir
        .file_name()
        .and_then(|name| name.to_str())
        .context("not a job directory")?;
    let agent_dir = dir
        .parent()
        .and_then(Path::parent)
        .context("not a job directory")?;
    let job = Job::open(agent_dir, id)?;
    let _claim = job.claim()?;
    ensure!(!job.finished(), "job {id} already finished");
    Agent {
        dir: agent_dir.into(),
        backend,
        // A worker only runs its job; it reserves nothing.
        limits: Limits {
            memory_budget_mib: 0,
            available_mib: 0,
            virtualization: false,
            probe_ports: false,
        },
        runner: Runner::Inline,
    }
    .run_job(&job)
    .await
}

/// Forwards deployment progress to the coordinator as frames.
fn progress(emit: Emitter) -> crate::kolla::EventSink {
    Arc::new(move |event| {
        emit(match event {
            crate::kolla::DeployEvent::Phase(phase) => Frame::Phase { phase },
            crate::kolla::DeployEvent::Step(step) => Frame::Step { step },
            crate::kolla::DeployEvent::Output(message) => Frame::Output { message },
        })
    })
}

pub fn stdout_emitter() -> Emitter {
    Arc::new(|frame| {
        let mut output = std::io::stdout().lock();
        serde_json::to_writer(&mut output, &frame)?;
        output.write_all(b"\n")?;
        output.flush()?;
        Ok(())
    })
}
/// Marker the coordinator reads out of the artifact file instead of executing it.
///
/// The artifact is a Linux binary that cannot run on every coordinator host, so the
/// build id has to be greppable bytes rather than a runtime report.
#[used]
#[unsafe(no_mangle)]
pub static FARM_AGENT_BUILD_STAMP: [u8; BUILD_STAMP_LEN] = build_stamp();

pub const BUILD_STAMP_PREFIX: &str = "farm-agent-build:";
const BUILD_STAMP_LEN: usize = 64;

const fn build_stamp() -> [u8; BUILD_STAMP_LEN] {
    let prefix = BUILD_STAMP_PREFIX.as_bytes();
    let id = crate::BUILD_ID.as_bytes();
    let mut stamp = [0u8; BUILD_STAMP_LEN];
    let mut at = 0;
    while at < prefix.len() {
        stamp[at] = prefix[at];
        at += 1;
    }
    let mut index = 0;
    while index < id.len() && at < BUILD_STAMP_LEN - 1 {
        stamp[at] = id[index];
        at += 1;
        index += 1;
    }
    stamp
}

/// Build id written into an artifact, or `None` when it predates the marker. The prefix can
/// also sit in the binary on its own, as the PE linker leaves it, so the first occurrence
/// followed by an id counts.
pub(crate) fn artifact_build_id(bytes: &[u8]) -> Option<String> {
    let prefix = BUILD_STAMP_PREFIX.as_bytes();
    bytes
        .windows(prefix.len())
        .enumerate()
        .filter(|(_, window)| *window == prefix)
        .find_map(|(start, _)| {
            let rest = &bytes[start + prefix.len()..];
            let end = rest.iter().position(|byte| *byte == 0)?;
            let id = std::str::from_utf8(&rest[..end]).ok()?;
            (!id.is_empty()
                && id.len() <= 40
                && id
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'))
            .then(|| id.to_owned())
        })
}

/// The request is the first line of stdin, or all of it. Only a shell keeps reading after it.
pub async fn serve_stdio() -> Result<()> {
    use tokio::io::{AsyncBufReadExt, AsyncReadExt};
    const LIMIT: usize = 128 * 1024;
    let mut stdin = tokio::io::BufReader::new(tokio::io::stdin());
    let mut input = Vec::new();
    (&mut stdin)
        .take(LIMIT as u64 + 1)
        .read_until(b'\n', &mut input)
        .await?;
    ensure!(input.len() <= LIMIT, "agent request too large");
    let emit = stdout_emitter();
    let agent = Agent::local();
    let result = match serde_json::from_slice::<Request>(&input) {
        Ok(request) if matches!(request.operation, Operation::Shell { .. }) => {
            agent.terminal(&request, stdin, emit.clone()).await
        }
        Ok(request) => agent.execute(request, emit.clone()).await,
        Err(_) => Err(anyhow::anyhow!("invalid farm-agent request")),
    };
    match result {
        Ok(result) => emit(Frame::Result { result }),
        Err(error) => {
            emit(Frame::Error {
                message: format!("{error:#}"),
            })?;
            Err(anyhow::anyhow!("remote operation failed"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{manager::tests::Fake, model::PlanRequest};
    use std::sync::atomic::Ordering;
    fn create(id: &str, generation: &str, base: u16) -> Operation {
        Operation::Create {
            farm_id: id.into(),
            generation: generation.into(),
            plan: layout(&PlanRequest::default()).unwrap(),
            exposure: Exposure {
                host: "target.example".into(),
                bind: "0.0.0.0".parse().unwrap(),
                port_base: base,
            },
        }
    }
    fn request(owner: &str, operation: Operation) -> Request {
        Request {
            version: VERSION,
            coordinator_id: owner.into(),
            operation,
        }
    }
    #[tokio::test]
    async fn durable_ownership_replay_budgets_failure_reservations_and_foreign_vms() {
        let dir = tempfile::tempdir().unwrap();
        let backend = Arc::new(Fake::default());
        let agent = Agent {
            dir: dir.path().into(),
            backend: backend.clone(),
            limits: Limits {
                // Enough for one default plan; the second must be refused.
                memory_budget_mib: 5000,
                available_mib: 10000,
                virtualization: true,
                probe_ports: true,
            },
            runner: Runner::Inline,
        };
        let owner = uuid::Uuid::new_v4().to_string();
        let other = uuid::Uuid::new_v4().to_string();
        let generation = uuid::Uuid::new_v4().to_string();
        let next = uuid::Uuid::new_v4().to_string();
        let emit: Emitter = Arc::new(|_| Ok(()));
        let first = request(&owner, create("one", &generation, 23500));
        assert!(
            matches!(agent.execute(first.clone(), emit.clone()).await.unwrap(), Reply::Nodes { nodes } if nodes.len() == 1)
        );
        agent.execute(first.clone(), emit.clone()).await.unwrap();
        assert!(
            agent
                .execute(request(&owner, create("two", &next, 23510)), emit.clone())
                .await
                .unwrap_err()
                .to_string()
                .contains("budget")
        );
        for (who, token) in [(&other, &generation), (&owner, &next)] {
            assert!(
                agent
                    .execute(
                        request(
                            who,
                            Operation::Delete {
                                farm_id: "one".into(),
                                generation: token.into()
                            }
                        ),
                        emit.clone()
                    )
                    .await
                    .is_err()
            );
        }
        assert!(
            matches!(agent.execute(request(&other, Operation::Scan), emit.clone()).await.unwrap(), Reply::Inventory { farms } if farms.is_empty())
        );
        agent
            .execute(
                request(
                    &owner,
                    Operation::Delete {
                        farm_id: "one".into(),
                        generation: generation.clone(),
                    },
                ),
                emit.clone(),
            )
            .await
            .unwrap();
        let replay_error = agent.execute(first, emit.clone()).await.unwrap_err();
        assert!(
            replay_error.to_string().contains("already deleted"),
            "{replay_error:#}"
        );
        backend.fail_create.store(true, Ordering::SeqCst);
        assert!(
            agent
                .execute(request(&owner, create("one", &next, 23500)), emit.clone())
                .await
                .is_err()
        );
        assert!(!LockedRegistry::open(dir.path()).unwrap().state.farms["one"].deleted);
        agent
            .execute(
                request(
                    &owner,
                    Operation::Delete {
                        farm_id: "one".into(),
                        generation: next.clone(),
                    },
                ),
                emit.clone(),
            )
            .await
            .unwrap();
        backend.fail_create.store(false, Ordering::SeqCst);
        backend
            .create(
                "foreign".into(),
                layout(&PlanRequest::default()).unwrap(),
                Exposure {
                    host: "localhost".into(),
                    bind: "127.0.0.1".parse().unwrap(),
                    port_base: 15000,
                },
            )
            .await
            .unwrap();
        assert!(
            agent
                .execute(
                    request(
                        &owner,
                        Operation::Delete {
                            farm_id: "foreign".into(),
                            generation: next
                        }
                    ),
                    emit.clone()
                )
                .await
                .unwrap_err()
                .to_string()
                .contains("unowned")
        );
        assert!(backend.scan().await.unwrap().contains_key("foreign"));
        let original = fs::read(dir.path().join("registry.json")).unwrap();
        let held = lock(dir.path(), "one.lock").unwrap();
        assert!(
            agent
                .execute(
                    request(
                        &owner,
                        create("one", &uuid::Uuid::new_v4().to_string(), 23500)
                    ),
                    emit.clone()
                )
                .await
                .is_err()
        );
        drop(held);
        fs::write(dir.path().join("registry.json"), b"corrupt").unwrap();
        assert!(
            agent
                .execute(request(&owner, Operation::Scan), emit)
                .await
                .is_err()
        );
        assert_eq!(
            fs::read(dir.path().join("registry.json")).unwrap(),
            b"corrupt"
        );
        assert!(!original.is_empty());
    }

    type Frames = Arc<std::sync::Mutex<Vec<Frame>>>;
    fn collector() -> (Frames, Emitter) {
        let frames = Frames::default();
        let collect: Emitter = {
            let frames = frames.clone();
            Arc::new(move |frame| {
                frames.lock().unwrap().push(frame);
                Ok(())
            })
        };
        (frames, collect)
    }
    fn deploy(owner: &str, generation: &str, job_id: &str) -> Request {
        request(
            owner,
            Operation::Deploy {
                farm_id: "one".into(),
                generation: generation.into(),
                from: crate::kolla::Phase::Prepare,
                job_id: job_id.into(),
            },
        )
    }
    fn attach(owner: &str, job_id: &str, offset: u64) -> Request {
        request(
            owner,
            Operation::Attach {
                job_id: job_id.into(),
                offset,
            },
        )
    }

    #[tokio::test]
    async fn deploy_jobs_are_recorded_attached_to_and_owned_by_their_coordinator() {
        let dir = tempfile::tempdir().unwrap();
        let backend = Arc::new(Fake::default());
        let agent = Agent::in_process(dir.path().into(), backend.clone());
        let owner = uuid::Uuid::new_v4().to_string();
        let other = uuid::Uuid::new_v4().to_string();
        let generation = uuid::Uuid::new_v4().to_string();
        let (_, silent) = collector();
        agent
            .execute(
                request(&owner, create("one", &generation, 23700)),
                silent.clone(),
            )
            .await
            .unwrap();
        let job_id = uuid::Uuid::new_v4().to_string();
        let (frames, collect) = collector();
        let reply = agent
            .execute(deploy(&owner, &generation, &job_id), collect)
            .await
            .unwrap();
        assert!(matches!(reply, Reply::Done));
        assert_eq!(frames.lock().unwrap().len(), 3);
        let job_dir = dir.path().join("jobs").join(&job_id);
        assert_eq!(
            fs::read_to_string(job_dir.join("frames.jsonl"))
                .unwrap()
                .lines()
                .count(),
            3
        );
        assert!(job_dir.join("result.json").exists());

        // The same deploy again joins the recorded job instead of deploying twice.
        let (again, collect) = collector();
        agent
            .execute(deploy(&owner, &generation, &job_id), collect)
            .await
            .unwrap();
        assert_eq!(again.lock().unwrap().len(), 3);
        assert_eq!(backend.deployed.load(Ordering::SeqCst), 1);
        let (rest, collect) = collector();
        agent
            .execute(attach(&owner, &job_id, 2), collect)
            .await
            .unwrap();
        assert!(matches!(
            &rest.lock().unwrap()[..],
            [Frame::Step { step }] if step == "make images"
        ));

        let foreign = agent
            .execute(attach(&other, &job_id, 0), silent.clone())
            .await
            .unwrap_err();
        assert!(
            foreign.to_string().contains("another coordinator"),
            "{foreign:#}"
        );
        assert!(
            agent
                .execute(deploy(&other, &generation, &job_id), silent.clone())
                .await
                .is_err()
        );
        let unknown = agent
            .execute(
                attach(&owner, &uuid::Uuid::new_v4().to_string(), 0),
                silent.clone(),
            )
            .await
            .unwrap_err();
        assert!(unknown.to_string().contains("unknown"), "{unknown:#}");

        // A job whose worker is gone without a result reports that, not a hang.
        let orphan = uuid::Uuid::new_v4().to_string();
        Job::create(dir.path(), &orphan, &deploy(&owner, &generation, &orphan)).unwrap();
        let lost = agent
            .execute(attach(&owner, &orphan, 0), silent.clone())
            .await
            .unwrap_err();
        assert!(
            lost.to_string()
                .contains("deploy worker exited without a result"),
            "{lost:#}"
        );

        // A failed deploy keeps its error for a later attach.
        backend.fail.store(true, Ordering::SeqCst);
        let failing = uuid::Uuid::new_v4().to_string();
        for operation in [
            deploy(&owner, &generation, &failing),
            attach(&owner, &failing, 0),
        ] {
            let error = agent.execute(operation, silent.clone()).await.unwrap_err();
            assert!(error.to_string().contains("intentional deploy failure"));
        }
        assert_eq!(backend.deployed.load(Ordering::SeqCst), 2);
    }

    /// Holds a deploy until its job directory has a `release` file.
    struct Held(PathBuf);
    impl Backend for Held {
        fn scan(
            &self,
        ) -> futures::future::BoxFuture<'_, Result<BTreeMap<String, Vec<crate::farm::FarmNode>>>>
        {
            Box::pin(async { bail!("not in this test") })
        }
        fn create(
            &self,
            _: String,
            _: Plan,
            _: Exposure,
        ) -> futures::future::BoxFuture<'_, Result<Vec<crate::farm::FarmNode>>> {
            Box::pin(async { bail!("not in this test") })
        }
        fn deploy(
            &self,
            _: String,
            from: crate::kolla::Phase,
            _: Option<Exposure>,
            _: Vec<String>,
            sink: crate::kolla::EventSink,
        ) -> futures::future::BoxFuture<'_, Result<()>> {
            use crate::kolla::DeployEvent;
            Box::pin(async move {
                sink(DeployEvent::Phase(from))?;
                sink(DeployEvent::Output("waiting for release".into()))?;
                for _ in 0..600 {
                    if self.0.exists() {
                        sink(DeployEvent::Output("released".into()))?;
                        return Ok(());
                    }
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
                bail!("never released")
            })
        }
        fn delete(&self, _: String) -> futures::future::BoxFuture<'_, Result<()>> {
            Box::pin(async { bail!("not in this test") })
        }
        fn endpoints(
            &self,
            _: String,
            _: Option<Exposure>,
            _: Vec<String>,
        ) -> futures::future::BoxFuture<'_, Result<crate::model::Endpoints>> {
            Box::pin(async { bail!("not in this test") })
        }
    }

    /// Not a test of its own: the worker process `a_detached_worker_outlives_its_request`
    /// starts from this test binary runs it with the job directory as the last argument.
    #[tokio::test]
    async fn detached_worker() {
        let Some(dir) = std::env::args()
            .next_back()
            .map(PathBuf::from)
            .filter(|dir| dir.join("spec.json").exists())
        else {
            return;
        };
        work(&dir, Arc::new(Held(dir.join("release"))))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn a_detached_worker_outlives_its_request() {
        let dir = tempfile::tempdir().unwrap();
        let mut agent = Agent::in_process(dir.path().into(), Arc::new(Fake::default()));
        agent.runner = Runner::Command(vec![
            std::env::current_exe().unwrap().into(),
            "--exact".into(),
            "remote::agent::tests::detached_worker".into(),
            "--nocapture".into(),
        ]);
        let agent = Arc::new(agent);
        let owner = uuid::Uuid::new_v4().to_string();
        let generation = uuid::Uuid::new_v4().to_string();
        let (_, silent) = collector();
        agent
            .execute(request(&owner, create("one", &generation, 23710)), silent)
            .await
            .unwrap();
        let job_id = uuid::Uuid::new_v4().to_string();
        let (frames, collect) = collector();
        let requester = tokio::spawn({
            let (agent, request) = (agent.clone(), deploy(&owner, &generation, &job_id));
            async move { agent.execute(request, collect).await }
        });
        tokio::time::timeout(Duration::from_secs(30), async {
            while frames.lock().unwrap().len() < 2 {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        // The coordinator's connection ends; the worker does not.
        requester.abort();
        let _ = requester.await;
        let job = Job::open(dir.path(), &job_id).unwrap();
        assert!(job.running() && !job.finished());
        #[cfg(unix)]
        {
            let pid: i32 = fs::read_to_string(dir.path().join("jobs").join(&job_id).join("pid"))
                .unwrap()
                .parse()
                .unwrap();
            // SAFETY: getsid only reads the session ids of two processes.
            let (worker, ours) = unsafe { (libc::getsid(pid), libc::getsid(0)) };
            assert!(
                worker == pid && worker != ours,
                "the worker has its own session"
            );
        }
        fs::write(dir.path().join("jobs").join(&job_id).join("release"), "").unwrap();
        let (rest, collect) = collector();
        let reply = tokio::time::timeout(
            Duration::from_secs(60),
            agent.execute(attach(&owner, &job_id, 2), collect),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(matches!(reply, Reply::Done));
        assert!(matches!(
            &rest.lock().unwrap()[..],
            [Frame::Output { message }] if message == "released"
        ));
    }

    #[tokio::test]
    async fn purge_removes_every_reserved_farm_and_forgets_them() {
        let dir = tempfile::tempdir().unwrap();
        let backend = Arc::new(Fake::default());
        let agent = Agent {
            dir: dir.path().to_path_buf(),
            backend: backend.clone(),
            limits: Limits {
                // Two default plans are 3 GiB each.
                memory_budget_mib: 20_000,
                available_mib: 32_000,
                virtualization: true,
                probe_ports: true,
            },
            runner: Runner::Inline,
        };
        let owner = uuid::Uuid::new_v4().to_string();
        let other = uuid::Uuid::new_v4().to_string();
        let emit: Emitter = Arc::new(|_| Ok(()));
        for (who, id, base) in [(&owner, "one", 15000), (&other, "two", 15010)] {
            agent
                .execute(
                    request(who, create(id, &uuid::Uuid::new_v4().to_string(), base)),
                    emit.clone(),
                )
                .await
                .unwrap();
        }
        // Both coordinators' VMs are this host's farms, so a purge removes all of them.
        let reply = agent
            .execute(request(&owner, Operation::Purge), emit.clone())
            .await
            .unwrap();
        assert!(matches!(reply, Reply::Purged { farms: 2 }), "{reply:?}");
        assert!(backend.farms.lock().unwrap().is_empty());
        assert!(matches!(
            agent
                .execute(request(&owner, Operation::Scan), emit.clone())
                .await
                .unwrap(),
            Reply::Inventory { farms } if farms.is_empty()
        ));
        // A second purge has nothing left and still succeeds.
        assert!(matches!(
            agent
                .execute(request(&owner, Operation::Purge), emit)
                .await
                .unwrap(),
            Reply::Purged { farms: 0 }
        ));
    }

    #[tokio::test]
    async fn shell_relays_frames_for_the_owner_only_and_ends_at_eof() {
        let dir = tempfile::tempdir().unwrap();
        let agent = Agent {
            dir: dir.path().into(),
            backend: Arc::new(Fake::default()),
            limits: Limits {
                memory_budget_mib: 10000,
                available_mib: 10000,
                virtualization: true,
                probe_ports: true,
            },
            runner: Runner::Inline,
        };
        let owner = uuid::Uuid::new_v4().to_string();
        let generation = uuid::Uuid::new_v4().to_string();
        let silent: Emitter = Arc::new(|_| Ok(()));
        agent
            .execute(
                request(&owner, create("one", &generation, 23900)),
                silent.clone(),
            )
            .await
            .unwrap();
        let shell = |who: &str| {
            request(
                who,
                Operation::Shell {
                    farm_id: "one".into(),
                    generation: generation.clone(),
                    size: TerminalSize { rows: 24, cols: 80 },
                },
            )
        };
        let frames = Arc::new(std::sync::Mutex::new(Vec::new()));
        let collect: Emitter = {
            let frames = frames.clone();
            Arc::new(move |frame| {
                frames.lock().unwrap().push(frame);
                Ok(())
            })
        };
        let input = format!(
            "{}\n{}\n",
            serde_json::to_string(&TerminalFrame::Input {
                data: STANDARD.encode("ls\r")
            })
            .unwrap(),
            serde_json::to_string(&TerminalFrame::Resize {
                rows: 40,
                cols: 120
            })
            .unwrap()
        );
        let reply = agent
            .terminal(&shell(&owner), input.as_bytes(), collect)
            .await
            .unwrap();
        assert!(matches!(reply, Reply::Done));
        let output: Vec<String> = frames
            .lock()
            .unwrap()
            .iter()
            .filter_map(|frame| match frame {
                Frame::Terminal { data } => {
                    Some(String::from_utf8(STANDARD.decode(data).unwrap()).unwrap())
                }
                _ => None,
            })
            .collect();
        assert!(matches!(frames.lock().unwrap()[0], Frame::Attached));
        assert_eq!(output, ["24x80", "ls\r", "40x120"]);

        let other = uuid::Uuid::new_v4().to_string();
        assert!(
            agent
                .terminal(&shell(&other), &b""[..], silent.clone())
                .await
                .is_err()
        );
        assert!(
            agent
                .terminal(&shell(&owner), &b"not json\n"[..], silent.clone())
                .await
                .is_err()
        );
        // A shell is a stream; the one-shot entry point refuses it.
        assert!(agent.execute(shell(&owner), silent).await.is_err());
    }
}
