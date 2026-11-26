//! One SSH request per process; no listener and no SSH credentials on the target.
use super::{
    job::Job,
    protocol::{Frame, MAX_FRAME, Operation, Reply, Request, TerminalFrame, VERSION},
};
use crate::{
    manager::{Backend, MicrosandboxBackend},
    model::{Exposure, Plan, PlanRequest, layout, validate_id},
    stackbox::{TerminalInput, TerminalIo, TerminalSize},
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
    /// The part of a stackbox that spans hosts this host runs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    share: Option<super::protocol::Share>,
}
impl Reservation {
    /// Memory of the VMs this host runs.
    fn memory_mib(&self) -> u64 {
        share_memory(&self.plan, self.share.as_ref())
    }
    /// Whether this host publishes the stackbox's ports: it runs its all-in-one or control VM.
    fn publishes(&self) -> bool {
        self.share.as_ref().is_none_or(|share| share.has_control())
    }
    /// Whether this host runs the VM with `suffix`.
    fn runs(&self, suffix: &str) -> bool {
        self.share
            .as_ref()
            .is_none_or(|share| share.nodes.iter().any(|n| n == suffix))
    }
}
fn share_memory(plan: &Plan, share: Option<&super::protocol::Share>) -> u64 {
    plan.nodes
        .iter()
        .filter(|spec| {
            share.is_none_or(|share| {
                crate::stackbox::suffix(spec.role, spec.index)
                    .is_some_and(|s| share.nodes.contains(&s))
            })
        })
        .map(|spec| u64::from(spec.memory_mib))
        .sum()
}
#[derive(Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Registry {
    stackboxes: BTreeMap<String, Reservation>,
}
struct LockedRegistry {
    dir: PathBuf,
    state: Registry,
    _lock: AgentLock,
}
pub(super) struct AgentLock(File);
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
            .stackboxes
            .get(id)
            .context("remote stackbox reservation not found")?;
        ensure!(
            entry.coordinator == coordinator && entry.generation == generation,
            "remote stackbox belongs to another coordinator or generation"
        );
        Ok(entry)
    }
}
pub(super) fn lock(dir: &Path, name: &str) -> Result<AgentLock> {
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
    /// Bind a new stackbox's public port block before reserving it.
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
    /// The command a PCI bind or unbind runs before the helper's verb and device: the real
    /// `sudo -n` helper, or a stand-in in tests.
    pci_helper: Vec<String>,
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
            pci_helper: crate::remote::pci::sudo_command(),
        }
    }
    pub fn local() -> Self {
        let dir = local_dir();
        Self {
            backend: Arc::new(MicrosandboxBackend::new(&dir)),
            dir,
            limits: Limits::system(),
            runner: Runner::Detached,
            pci_helper: crate::remote::pci::sudo_command(),
        }
    }
    /// Destroys the stackbox VMs this host's agent holds and forgets them: every coordinator's
    /// when `coordinator` is `None` (`Purge`), otherwise only that coordinator's (`PurgeOwn`).
    /// Used when the coordinator removes the host. Runs under the create lock, so a
    /// reservation cannot appear mid-purge. A retry finds what is already gone and succeeds.
    async fn purge(&self, coordinator: Option<&str>) -> Result<Reply> {
        let reserved: Vec<String> = {
            let registry = LockedRegistry::open(&self.dir)?;
            registry
                .state
                .stackboxes
                .iter()
                .filter(|(_, r)| coordinator.is_none_or(|c| r.coordinator == c))
                .map(|(id, _)| id.clone())
                .collect()
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
                registry.state.stackboxes.remove(id);
            }
        }
        // Counted after the removal: what other coordinators still hold on the host.
        let kept_for_others = coordinator.map_or(0, |c| {
            registry
                .state
                .stackboxes
                .values()
                .filter(|r| !r.deleted && r.coordinator != c)
                .count()
        });
        registry.save()?;
        ensure!(
            failed.is_empty(),
            "failed to remove {} reserved stackbox(s): {}",
            failed.len(),
            failed.join("; ")
        );
        Ok(Reply::Purged {
            stackboxes: purged,
            kept_for_others: Some(kept_for_others),
        })
    }

    /// Reserve capacity and public ports for a new stackbox on this host. A replay of the same
    /// request returns the stackbox's existing nodes instead.
    async fn reserve(
        &self,
        request: &Request,
        id: &str,
        generation: &str,
        requested: &Plan,
        exposure: &Exposure,
        share: Option<&super::protocol::Share>,
    ) -> Result<Option<Vec<crate::stackbox::StackboxNode>>> {
        ensure!(
            self.limits.virtualization,
            "this host has no usable hypervisor (/dev/kvm on Linux)"
        );
        // The coordinator applies plan()'s policy; restored snapshots keep an older layout,
        // and a control template's stackbox starts with its control VM alone.
        let canonical = if requested.multinode() && requested.computes == 0 {
            crate::model::control_layout(
                &requested.components,
                requested.compute,
                requested.release.as_deref(),
            )?
        } else {
            layout(&PlanRequest {
                pci_devices: requested.pci_devices.clone(),
                components: requested.components.clone(),
                computes: requested.computes,
                compute: requested.compute,
                layout: None,
                release: requested.release.clone(),
                gpu: requested.gpu.clone(),
            })?
        };
        ensure!(
            serde_json::to_value(requested)? == serde_json::to_value(&canonical)?,
            "stackbox plan differs from the supported layout"
        );
        // The coordinator picks the bind address (0.0.0.0 for remote hosts, its public bind
        // locally); binding the whole block below proves the ports are free.
        ensure!(
            exposure.port_base >= 15000
                && exposure.port_base <= 23990
                && exposure.port_base % 10 == 0,
            "exposure requires a valid port block"
        );
        // Reuse public-host validation.
        crate::model::CreateRequest {
            pci_devices: Vec::new(),
            id: id.into(),
            host_id: None,
            components: requested.components.clone(),
            computes: requested.computes,
            // A control template's stackbox keeps its computes' size for later expansions.
            compute: requested.compute.filter(|_| requested.computes > 0),
            layout: None,
            release: None,
            external_host: exposure.host.clone(),
            owner: None,
            expires_at: None,
            snapshot_id: None,
            bucket: None,
            key: None,
            placement: Default::default(),
            gpu: None,
        }
        .validate()?;
        if let Some(share) = share {
            share.validate(id, requested)?;
        }
        let publishes = share.is_none_or(|share| share.has_control());
        let memory_mib = share_memory(requested, share);
        let _create_lock = lock(&self.dir, "create.lock")?;
        let mut registry = LockedRegistry::open(&self.dir)?;
        let observed = self.backend.scan().await?;
        if let Some(entry) = registry.state.stackboxes.get(id) {
            ensure!(
                entry.coordinator == request.coordinator_id,
                "stackbox id is owned by another coordinator"
            );
            if !entry.deleted {
                ensure!(
                    entry.generation == generation,
                    "stackbox id is reserved by an earlier generation"
                );
                ensure!(
                    serde_json::to_value(&entry.plan)? == serde_json::to_value(requested)?
                        && serde_json::to_value(&entry.exposure)?
                            == serde_json::to_value(exposure)?
                        && entry.share.as_ref() == share,
                    "replayed create differs from the original reservation"
                );
                let nodes = observed.get(id).cloned().unwrap_or_default();
                let expected = share.map_or(requested.nodes.len(), |share| share.nodes.len());
                ensure!(
                    nodes.len() == expected && nodes.iter().all(|n| n.status == "running"),
                    "earlier create is incomplete; inspect and delete it explicitly before recreating"
                );
                return Ok(Some(nodes));
            }
            ensure!(
                entry.generation != generation,
                "this stackbox generation was already deleted"
            );
        }
        ensure!(
            observed.get(id).is_none_or(Vec::is_empty),
            "existing VM labels conflict with this stackbox; refusing to adopt or remove them"
        );
        let reserved: u64 = registry
            .state
            .stackboxes
            .values()
            .filter(|r| !r.deleted)
            .map(Reservation::memory_mib)
            .sum();
        let foreign: u64 = observed
            .iter()
            .filter(|(id, _)| {
                !registry
                    .state
                    .stackboxes
                    .get(*id)
                    .is_some_and(|r| !r.deleted)
            })
            .flat_map(|(_, nodes)| nodes)
            .map(|n| u64::from(n.memory_mib))
            .sum();
        ensure!(
            reserved.saturating_add(foreign).saturating_add(memory_mib)
                <= self.limits.memory_budget_mib,
            "remote host memory reservation budget exceeded"
        );
        ensure!(
            memory_mib <= self.limits.available_mib,
            "remote host available memory is insufficient"
        );
        // Only the host of the control VM publishes the stackbox's ports.
        ensure!(
            !publishes
                || !registry.state.stackboxes.values().any(|r| {
                    !r.deleted && r.publishes() && r.exposure.port_base == exposure.port_base
                }),
            "remote public ports are reserved by another stackbox"
        );
        let sockets = (exposure.port_base..exposure.port_base + 10)
            .filter(|_| self.limits.probe_ports && publishes)
            .map(|p| TcpListener::bind((exposure.bind, p)))
            .collect::<std::io::Result<Vec<_>>>()
            .context("the stackbox's public port block is occupied on this host")?;
        registry.state.stackboxes.insert(
            id.into(),
            Reservation {
                coordinator: request.coordinator_id.clone(),
                generation: generation.into(),
                plan: requested.clone(),
                exposure: exposure.clone(),
                deleted: false,
                share: share.cloned(),
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
            "incompatible stackbox-agent protocol"
        );
        uuid::Uuid::parse_str(&request.coordinator_id).context("invalid coordinator identity")?;
        if let Some((id, generation)) = request.operation.scope() {
            validate_id(id)?;
            uuid::Uuid::parse_str(generation).context("invalid stackbox generation")?;
        }
        if let Operation::Deploy { job_id, .. } | Operation::Attach { job_id, .. } =
            &request.operation
        {
            uuid::Uuid::parse_str(job_id).context("invalid job id")?;
        }
        Ok(())
    }

    /// Relay a shell in the stackbox's VM: `TerminalFrame` lines in, `Frame::Terminal` out.
    /// It takes no stackbox lock, so a shell can watch a running deployment. An archive the
    /// coordinator streams arrives the same way.
    pub async fn terminal<R>(&self, request: &Request, input: R, emit: Emitter) -> Result<Reply>
    where
        R: tokio::io::AsyncBufRead + Unpin,
    {
        use tokio::io::{AsyncBufReadExt, AsyncReadExt};
        Self::admit(request)?;
        if let Operation::ReceiveArchive {
            transfer_id,
            sha256,
            size_bytes,
        } = &request.operation
        {
            // Links wait for this before streaming, as for a shell.
            emit(Frame::Attached)?;
            super::cache::receive(&self.dir, transfer_id, (sha256, *size_bytes), input).await?;
            return Ok(Reply::Done);
        }
        if let Operation::SendArchive { transfer_id } = &request.operation {
            emit(Frame::Attached)?;
            super::cache::send(&self.dir, transfer_id, &emit).await?;
            return Ok(Reply::Done);
        }
        let Operation::Shell {
            stackbox_id: id,
            generation,
            node,
            size,
        } = &request.operation
        else {
            bail!("not a shell request");
        };
        let size = size.validate()?;
        {
            let registry = LockedRegistry::open(&self.dir)?;
            let entry = registry.owned(&request.coordinator_id, id, generation)?;
            ensure!(!entry.deleted, "remote stackbox was deleted");
            let wanted = node.clone().unwrap_or_else(|| "control".into());
            ensure!(
                entry.share.is_none() || entry.runs(&wanted),
                "VM '{wanted}' of stackbox '{id}' runs on another host"
            );
        }
        emit(Frame::Attached)?;
        let (input_tx, input_rx) = tokio::sync::mpsc::channel(64);
        let (output_tx, output_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
        let session = self.backend.terminal(
            id.clone(),
            node.clone(),
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
                    TerminalFrame::Eof => TerminalInput::Close,
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

    /// The live reservation `request`'s coordinator holds for this stackbox generation.
    fn owned(&self, request: &Request, id: &str, generation: &str) -> Result<Reservation> {
        let registry = LockedRegistry::open(&self.dir)?;
        let entry = registry
            .owned(&request.coordinator_id, id, generation)?
            .clone();
        ensure!(!entry.deleted, "stackbox was deleted");
        Ok(entry)
    }

    /// The reservation a deploy of this stackbox generation runs on.
    fn deployable(&self, request: &Request, id: &str, generation: &str) -> Result<Reservation> {
        let entry = self.owned(request, id, generation)?;
        ensure!(
            crate::stackbox::layout(&entry.plan.nodes).is_ok(),
            "legacy multi-node stackbox cannot be deployed; create a new stackbox"
        );
        ensure!(
            entry.publishes(),
            "this host runs only compute VMs of stackbox '{id}'; deploy it from the control VM's host"
        );
        Ok(entry)
    }

    /// Start the deploy as job `job_id`, or join the job already started under that id, and
    /// follow it to its result. Ending this request leaves the job running.
    async fn deploy(&self, request: &Request, job_id: &str, emit: Emitter) -> Result<Reply> {
        let (id, generation) = request
            .operation
            .scope()
            .context("deploy without a stackbox id")?;
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
                        eprintln!("stackbox agent: cannot record the deploy job: {error:#}");
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
                "deploy timed out after {seconds} s; inspect the stackbox's VM before retrying"
            ))
        });
        job.finish(outcome)
    }

    async fn run_deploy(&self, request: &Request, emit: Emitter) -> Result<Reply> {
        Self::admit(request)?;
        let Operation::Deploy {
            stackbox_id: id,
            generation,
            from,
            reference,
            ..
        } = &request.operation
        else {
            bail!("not a deploy job");
        };
        // The coordinator resolved this ref against the catalog; this process has no cache
        // of its own. An older coordinator sends none, and the install keeps `info`'s ref.
        let reference = reference
            .clone()
            .filter(|reference| crate::kolla::releases::reference_ok(reference));
        let _stackbox_lock = lock(&self.dir, &format!("{id}.lock"))?;
        let entry = self.deployable(request, id, generation)?;
        let _deploy_lock = lock(&self.dir, "deploy.lock")?;
        if let Some(share) = &entry.share {
            // Computes on other hosts; the coordinator prepared them before this deploy.
            let remote: Vec<u8> = entry
                .plan
                .nodes
                .iter()
                .filter(|spec| {
                    crate::stackbox::suffix(spec.role, spec.index)
                        .is_some_and(|s| !share.nodes.contains(&s))
                })
                .filter_map(|spec| spec.index)
                .collect();
            // A plan recorded before releases were kept installs this build's release.
            let release = entry
                .plan
                .release
                .clone()
                .unwrap_or_else(|| crate::kolla::OPENSTACK_RELEASE.to_owned());
            self.backend
                .deploy_spanning(
                    id.into(),
                    *from,
                    Some(entry.exposure.clone()),
                    entry.plan.components.clone(),
                    release,
                    reference.clone(),
                    entry.plan.gpu,
                    entry.plan.pci_devices.clone(),
                    remote,
                    share
                        .ssh_private_key
                        .clone()
                        .context("deploy the stackbox from the host of its control VM")?,
                    progress(emit),
                )
                .await?;
            return Ok(Reply::Done);
        }
        self.backend
            .deploy(
                id.into(),
                *from,
                Some(entry.exposure),
                entry.plan.components.clone(),
                entry
                    .plan
                    .release
                    .clone()
                    .unwrap_or_else(|| crate::kolla::OPENSTACK_RELEASE.to_owned()),
                reference,
                entry.plan.gpu,
                entry.plan.pci_devices.clone(),
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
                super::cache::forget(&self.dir, reference).await?;
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
                supports_template_transfer: true,
            });
        }
        if matches!(request.operation, Operation::Scan) {
            let registry = LockedRegistry::open(&self.dir)?;
            let owned: Vec<_> = registry
                .state
                .stackboxes
                .iter()
                .filter(|(_, r)| r.coordinator == request.coordinator_id && !r.deleted)
                .map(|(id, _)| id.clone())
                .collect();
            drop(registry);
            let mut inventory = self.backend.scan().await?;
            let stackboxes = owned
                .into_iter()
                .map(|id| {
                    let nodes = inventory.remove(&id).unwrap_or_default();
                    (id, nodes)
                })
                .collect();
            return Ok(Reply::Inventory { stackboxes });
        }
        if matches!(request.operation, Operation::PublicPorts) {
            let registry = LockedRegistry::open(&self.dir)?;
            let bases = registry
                .state
                .stackboxes
                .values()
                .filter(|r| !r.deleted && r.publishes())
                .map(|r| r.exposure.port_base)
                .collect();
            return Ok(Reply::PublicPorts { bases });
        }
        // A PCI bind or unbind changes the host's drivers, so one runs at a time; the reply carries
        // the fresh inspection the coordinator stores.
        if let Some((verb, bdf)) = request.operation.pci_request() {
            let _pci_lock = lock(&self.dir, "pci.lock")?;
            let bdf = crate::model::normalize_pci_bdf(bdf)?;
            crate::remote::host::run_pci_helper(&self.pci_helper, verb, &bdf).await?;
            return Ok(Reply::Inspection {
                inspection: Box::new(crate::remote::host::inspect().await),
            });
        }
        // Host-wide checks have no stackbox id either.
        if matches!(request.operation, Operation::Inspect) {
            return Ok(Reply::Inspection {
                inspection: Box::new(crate::remote::host::inspect().await),
            });
        }
        if matches!(request.operation, Operation::VerifyVm) {
            let gpu = crate::remote::host::smoke(&uuid::Uuid::new_v4().to_string()).await?;
            // The coordinator stores the inspection it gets here, so the boot's GPU probe
            // survives this one-request process. A plain `Done` would leave the host looking
            // unverified until something else happened to inspect it in the same process.
            let mut inspection = crate::remote::host::inspect().await;
            inspection.mtty_available = gpu.mtty_available();
            inspection.gpu_kernel = Some(gpu);
            return Ok(Reply::Inspection {
                inspection: Box::new(inspection),
            });
        }
        if matches!(request.operation, Operation::InstallMsb) {
            // Stands in for flock(1), which macOS lacks; Linux installs also take it in the script.
            let _install_lock = lock(&self.dir, "msb-install.lock")?;
            crate::remote::host::install_msb().await?;
            return Ok(Reply::Done);
        }
        // A host-wide cleanup has no stackbox id, so it runs before the per-stackbox scope below.
        if matches!(request.operation, Operation::Purge | Operation::PurgeOwn) {
            // Serialize with creates so a reservation cannot appear mid-purge. A purge that
            // finds another one running fails here rather than waiting, and nothing is deleted.
            let _create_lock = lock(&self.dir, "create.lock")?;
            let scope = match request.operation {
                Operation::PurgeOwn => Some(request.coordinator_id.as_str()),
                _ => None,
            };
            return self.purge(scope).await;
        }
        if matches!(request.operation, Operation::RemoveGolden) {
            // Under the golden build lock, so a build in progress on this host finishes first.
            let groups = self.backend.remove_golden().await?;
            return Ok(Reply::GoldenRemoved { groups });
        }
        if matches!(
            request.operation,
            Operation::ExportSnapshot { .. }
                | Operation::ImportSnapshot { .. }
                | Operation::RemoveSnapshot { .. }
        ) {
            return self.snapshot_operation(&request.operation).await;
        }
        match &request.operation {
            Operation::FindSnapshot { snapshot_id } => {
                let found = super::cache::find(
                    &self.dir,
                    self.backend.as_ref(),
                    &request.coordinator_id,
                    snapshot_id,
                )
                .await?;
                let (reference, nodes) = match found {
                    Some((reference, nodes)) => (Some(reference), nodes),
                    None => (None, Vec::new()),
                };
                return Ok(Reply::Found { reference, nodes });
            }
            Operation::FetchArchive {
                transfer_id,
                url,
                ca_pem,
                sha256,
                size_bytes,
            } => {
                super::cache::fetch(
                    &self.dir,
                    transfer_id,
                    url,
                    ca_pem.as_deref(),
                    (sha256, *size_bytes),
                    &emit,
                )
                .await?;
                return Ok(Reply::Done);
            }
            Operation::ImportArchive {
                transfer_id,
                snapshot_id,
            } => {
                return Ok(Reply::Imported {
                    snapshot: super::cache::import(
                        &self.dir,
                        self.backend.as_ref(),
                        &request.coordinator_id,
                        transfer_id,
                        snapshot_id,
                    )
                    .await?,
                });
            }
            Operation::ExportArchive {
                transfer_id,
                reference,
            } => {
                let (sha256, size_bytes) =
                    super::cache::export(&self.dir, self.backend.as_ref(), transfer_id, reference)
                        .await?;
                return Ok(Reply::Archived { sha256, size_bytes });
            }
            Operation::ReceiveArchive { .. } => bail!("an archive needs an input stream"),
            Operation::SendArchive { .. } => bail!("an archive goes out as a stream"),
            _ => {}
        }
        // A deploy outlives this request; its worker takes the stackbox's locks instead.
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
            .context("stackbox operation without a stackbox id")?;
        let _stackbox_lock = lock(&self.dir, &format!("{id}.lock"))?;
        match &request.operation {
            Operation::Create {
                plan: requested,
                exposure,
                share,
                source,
                ..
            } => {
                ensure!(
                    source.is_none() || share.is_some(),
                    "only a share of a stackbox that spans hosts boots from a template on create"
                );
                if let Some(nodes) = self
                    .reserve(
                        &request,
                        id,
                        generation,
                        requested,
                        exposure,
                        share.as_deref(),
                    )
                    .await?
                {
                    return Ok(Reply::Nodes { nodes });
                }
                emit(Frame::Output {
                    message: "Host capacity and public ports reserved; creating the stackbox VM\n"
                        .into(),
                })?;
                let nodes = match (share, source) {
                    (Some(share), Some(source)) => {
                        self.backend
                            .create_share_from(
                                id.into(),
                                requested.clone(),
                                exposure.clone(),
                                (**share).clone(),
                                source.clone(),
                            )
                            .await?
                    }
                    (Some(share), None) => {
                        self.backend
                            .create_share(
                                id.into(),
                                requested.clone(),
                                exposure.clone(),
                                (**share).clone(),
                            )
                            .await?
                    }
                    (None, _) => {
                        self.backend
                            .create(id.into(), requested.clone(), exposure.clone())
                            .await?
                    }
                };
                Ok(Reply::Nodes { nodes })
            }
            Operation::Relink { url, .. } => {
                let entry = self.owned(&request, id, generation)?;
                ensure!(
                    entry.share.is_some(),
                    "stackbox '{id}' runs on this host alone"
                );
                ensure!(
                    url.starts_with("wss://") && url.contains(&format!("?stackbox={id}&host=")),
                    "invalid relay URL for stackbox '{id}'"
                );
                self.backend.relink(id.into(), url.clone()).await?;
                Ok(Reply::Done)
            }
            Operation::Prepare { .. } => {
                let entry = self.owned(&request, id, generation)?;
                let share = entry.share.as_ref().context(
                    "only a stackbox that spans hosts is prepared apart from its deploy",
                )?;
                ensure!(
                    !share.has_control(),
                    "the control VM's host prepares its VMs as it deploys"
                );
                // The release is already fixed by what this host created.
                let release = entry
                    .plan
                    .release
                    .clone()
                    .unwrap_or_else(|| crate::kolla::OPENSTACK_RELEASE.to_owned());
                self.backend
                    .prepare(
                        id.into(),
                        entry.plan.components.clone(),
                        release,
                        share.ssh_public_key.clone(),
                    )
                    .await?;
                Ok(Reply::Done)
            }
            Operation::Expand {
                plan: requested,
                nodes,
                add,
                uplink,
                source,
                ..
            } => {
                let entry = self.owned(&request, id, generation)?;
                ensure!(
                    source.is_none() || add.len() == 1,
                    "a compute template boots exactly one new VM"
                );
                // The coordinator applies plan()'s policy; expansion must keep every
                // existing VM's spec and only append compute VMs.
                let canonical = layout(&PlanRequest {
                    pci_devices: requested.pci_devices.clone(),
                    components: requested.components.clone(),
                    computes: requested.computes,
                    compute: requested.compute,
                    layout: None,
                    release: requested.release.clone(),
                    gpu: requested.gpu.clone(),
                })?;
                ensure!(
                    serde_json::to_value(requested)? == serde_json::to_value(&canonical)?,
                    "stackbox plan differs from the supported layout"
                );
                ensure!(
                    requested.computes >= entry.plan.computes,
                    "an expansion adds compute VMs"
                );
                ensure!(
                    requested.nodes.len() >= entry.plan.nodes.len()
                        && serde_json::to_value(&requested.nodes[..entry.plan.nodes.len()])?
                            == serde_json::to_value(&entry.plan.nodes)?,
                    "an expansion cannot change the stackbox's existing VMs"
                );
                let suffixes: Vec<String> = requested
                    .nodes
                    .iter()
                    .filter_map(|spec| crate::stackbox::suffix(spec.role, spec.index))
                    .collect();
                let ordered: Vec<String> = suffixes
                    .iter()
                    .filter(|suffix| nodes.contains(suffix))
                    .cloned()
                    .collect();
                ensure!(
                    ordered.len() == nodes.len()
                        && nodes.iter().all(|node| suffixes.contains(node))
                        && add.iter().all(|node| ordered.contains(node)),
                    "the expansion names VMs the stackbox does not have"
                );
                let mut share = entry.share.clone();
                if let Some(uplink) = uplink {
                    // The control VM's host joins the relay for the first time. Keep the
                    // stackbox's existing Kolla key so computes prepared earlier still trust it.
                    share = Some(match share {
                        Some(mut share) => {
                            share.uplink = uplink.clone();
                            share.nodes = ordered.clone();
                            share
                        }
                        None => {
                            let (public_key, private_key) =
                                self.backend.ssh_keys(id.into()).await?;
                            crate::remote::protocol::Share {
                                nodes: ordered.clone(),
                                uplink: uplink.clone(),
                                ssh_public_key: public_key,
                                ssh_private_key: Some(private_key),
                            }
                        }
                    });
                } else if let Some(share) = &mut share {
                    share.nodes = ordered.clone();
                }
                if let Some(share) = &share {
                    share.validate(id, requested)?;
                }
                let new_memory = share_memory(requested, share.as_ref());
                {
                    let registry = LockedRegistry::open(&self.dir)?;
                    let reserved: u64 = registry
                        .state
                        .stackboxes
                        .values()
                        .filter(|r| !r.deleted && r.generation != generation)
                        .map(Reservation::memory_mib)
                        .sum();
                    let foreign: u64 = self
                        .backend
                        .scan()
                        .await?
                        .iter()
                        .filter(|(stackbox, _)| {
                            !registry
                                .state
                                .stackboxes
                                .get(*stackbox)
                                .is_some_and(|r| !r.deleted)
                        })
                        .flat_map(|(_, nodes)| nodes)
                        .map(|n| u64::from(n.memory_mib))
                        .sum();
                    ensure!(
                        reserved.saturating_add(foreign).saturating_add(new_memory)
                            <= self.limits.memory_budget_mib,
                        "remote host memory reservation budget exceeded"
                    );
                    ensure!(
                        new_memory <= self.limits.available_mib,
                        "remote host available memory is insufficient"
                    );
                    drop(registry);
                }
                let created = self
                    .backend
                    .expand(
                        id.into(),
                        requested.clone(),
                        ordered.clone(),
                        add.clone(),
                        uplink.clone(),
                        source.clone(),
                    )
                    .await?;
                let mut registry = LockedRegistry::open(&self.dir)?;
                let entry = registry
                    .state
                    .stackboxes
                    .get_mut(id)
                    .context("remote stackbox reservation disappeared")?;
                ensure!(
                    entry.coordinator == request.coordinator_id
                        && entry.generation == generation
                        && !entry.deleted,
                    "stackbox reservation changed while expanding"
                );
                entry.plan = requested.clone();
                entry.share = share;
                registry.save()?;
                Ok(Reply::Nodes { nodes: created })
            }
            Operation::SshKeys { .. } => {
                let entry = self.owned(&request, id, generation)?;
                ensure!(
                    entry.publishes(),
                    "stackbox '{id}' is served from another host"
                );
                let (public_key, private_key) = match &entry.share {
                    Some(share) => (
                        share.ssh_public_key.clone(),
                        share
                            .ssh_private_key
                            .clone()
                            .context("the control VM's SSH key is missing")?,
                    ),
                    None => self.backend.ssh_keys(id.into()).await?,
                };
                Ok(Reply::SshKeys {
                    public_key,
                    private_key,
                })
            }
            Operation::DeployCompute {
                index,
                from_template,
                ..
            } => {
                let entry = self.owned(&request, id, generation)?;
                ensure!(
                    entry.publishes(),
                    "stackbox '{id}' is served from another host"
                );
                ensure!(
                    entry.plan.nodes.iter().any(|spec| {
                        spec.role == crate::stackbox::Role::Compute && spec.index == Some(*index)
                    }),
                    "stackbox '{id}' has no compute-{index}"
                );
                let nodes = entry.share.as_ref().map_or_else(
                    || {
                        entry
                            .plan
                            .nodes
                            .iter()
                            .filter_map(|spec| crate::stackbox::suffix(spec.role, spec.index))
                            .collect()
                    },
                    |share| share.nodes.clone(),
                );
                self.backend
                    .deploy_compute(
                        id.into(),
                        entry.plan.clone(),
                        *index,
                        nodes,
                        entry.plan.components.clone(),
                        *from_template,
                        progress(emit),
                    )
                    .await?;
                Ok(Reply::Done)
            }
            Operation::Restore {
                reference,
                nodes,
                plan: requested,
                exposure,
                ..
            } => {
                if let Some(nodes) = self
                    .reserve(&request, id, generation, requested, exposure, None)
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
                            nodes.clone(),
                            requested.clone(),
                            exposure.clone(),
                        )
                        .await?,
                })
            }
            Operation::Delete { .. } => {
                let _create_lock = lock(&self.dir, "create.lock")?;
                let mut registry = LockedRegistry::open(&self.dir)?;
                if !registry.state.stackboxes.contains_key(id) {
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
                registry.state.stackboxes.get_mut(id).unwrap().deleted = true;
                registry.save()?;
                Ok(Reply::Done)
            }
            Operation::Endpoints { .. } => {
                let entry = self.owned(&request, id, generation)?;
                ensure!(
                    entry.publishes(),
                    "stackbox '{id}' is served from another host"
                );
                Ok(Reply::Endpoints {
                    endpoints: self
                        .backend
                        .endpoints(id.into(), Some(entry.exposure), entry.plan.components)
                        .await?,
                })
            }
            Operation::Settle { rebind, .. } => {
                let entry = self.owned(&request, id, generation)?;
                ensure!(
                    entry.publishes(),
                    "stackbox '{id}' is served from another host"
                );
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
            Operation::ForgetComputes { .. } => {
                let entry = self.owned(&request, id, generation)?;
                ensure!(
                    entry.publishes(),
                    "stackbox '{id}' is served from another host"
                );
                ensure!(
                    entry.plan.multinode(),
                    "stackbox '{id}' has no control VM to forget computes on"
                );
                self.backend
                    .forget_computes(id.into(), entry.plan.components, progress(emit))
                    .await?;
                Ok(Reply::Done)
            }
            Operation::AttachPci { .. } => {
                let entry = self.owned(&request, id, generation)?;
                ensure!(
                    entry.publishes(),
                    "stackbox '{id}' is served from another host"
                );
                // The devices are the ones this host's reservation recorded, not a request's.
                ensure!(
                    !entry.plan.pci_devices.is_empty(),
                    "stackbox '{id}' has no PCI devices to attach"
                );
                let mtty = entry
                    .plan
                    .gpu
                    .as_ref()
                    .is_some_and(|gpu| gpu.mode == crate::model::GpuMode::Mtty);
                let release = entry
                    .plan
                    .release
                    .clone()
                    .unwrap_or_else(|| crate::kolla::OPENSTACK_RELEASE.to_owned());
                self.backend
                    .attach_pci(
                        id.into(),
                        release,
                        entry.plan.pci_devices.clone(),
                        mtty,
                        progress(emit),
                    )
                    .await?;
                Ok(Reply::Done)
            }
            Operation::Freeze {
                snapshot_id,
                node,
                labels,
                ..
            } => {
                let entry = self.owned(&request, id, generation)?;
                crate::model::validate_snapshot_id(snapshot_id)?;
                if let Some(node) = node {
                    ensure!(
                        entry.plan.nodes.iter().any(|spec| {
                            matches!(
                                spec.role,
                                crate::stackbox::Role::Control | crate::stackbox::Role::Compute
                            ) && crate::stackbox::suffix(spec.role, spec.index).as_deref()
                                == Some(node.as_str())
                        }),
                        "stackbox '{id}' has no control or compute VM '{node}'"
                    );
                    ensure!(
                        entry.runs(node),
                        "VM '{node}' of stackbox '{id}' runs on another host"
                    );
                }
                Ok(Reply::Frozen {
                    snapshot: self
                        .backend
                        .freeze(
                            id.into(),
                            snapshot_id.clone(),
                            node.clone(),
                            labels.clone(),
                            progress(emit),
                        )
                        .await?,
                })
            }
            Operation::Impair { node, impair, .. } => {
                let entry = self.owned(&request, id, generation)?;
                ensure!(
                    entry.plan.nodes.iter().any(|spec| {
                        spec.role != crate::stackbox::Role::AllInOne
                            && crate::stackbox::suffix(spec.role, spec.index).as_deref()
                                == Some(node.as_str())
                    }),
                    "stackbox '{id}' has no linked VM '{node}'"
                );
                ensure!(
                    entry.runs(node),
                    "VM '{node}' of stackbox '{id}' runs on another host"
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
    PathBuf::from(home).join(".local/share/stackbox/worker-v1")
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
        pci_helper: crate::remote::pci::sudo_command(),
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
            crate::kolla::DeployEvent::Progress(crate::kolla::StepProgress {
                step,
                index,
                total,
            }) => Frame::Progress { step, index, total },
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
pub static STACKBOX_AGENT_BUILD_STAMP: [u8; BUILD_STAMP_LEN] = build_stamp();

pub const BUILD_STAMP_PREFIX: &str = "stackbox-agent-build:";
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
        Ok(request) if request.operation.streams_input() => {
            agent.terminal(&request, stdin, emit.clone()).await
        }
        Ok(request) => agent.execute(request, emit.clone()).await,
        Err(_) => Err(anyhow::anyhow!("invalid stackbox-agent request")),
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
            stackbox_id: id.into(),
            generation: generation.into(),
            plan: layout(&PlanRequest::default()).unwrap(),
            exposure: Exposure {
                host: "target.example".into(),
                bind: "0.0.0.0".parse().unwrap(),
                port_base: base,
            },
            share: None,
            source: None,
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
    async fn public_ports_list_the_publishing_blocks_of_every_coordinator() {
        let dir = tempfile::tempdir().unwrap();
        let agent = Agent {
            dir: dir.path().into(),
            backend: Arc::new(Fake::default()),
            limits: Limits {
                memory_budget_mib: 50000,
                available_mib: 100000,
                virtualization: true,
                probe_ports: false,
            },
            runner: Runner::Inline,
            pci_helper: crate::remote::pci::sudo_command(),
        };
        let other = uuid::Uuid::new_v4().to_string();
        let generation = uuid::Uuid::new_v4().to_string();
        let emit: Emitter = Arc::new(|_| Ok(()));
        agent
            .execute(
                request(&other, create("held", &generation, 15000)),
                emit.clone(),
            )
            .await
            .unwrap();
        let asker = uuid::Uuid::new_v4().to_string();
        assert!(matches!(
            agent.execute(request(&asker, Operation::PublicPorts), emit.clone()).await.unwrap(),
            Reply::PublicPorts { bases } if bases == vec![15000]
        ));
        assert!(
            agent
                .execute(
                    request(
                        &asker,
                        create("new", &uuid::Uuid::new_v4().to_string(), 15000)
                    ),
                    emit
                )
                .await
                .unwrap_err()
                .to_string()
                .contains("reserved by another stackbox")
        );
    }
    /// A PCI bind through the agent runs the installed helper's script against the fixture's
    /// sysfs, refuses a device the helper protects without touching it, and replies with the
    /// host's inspection. The test helper runs as this user, so the seams are honoured.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_pci_bind_runs_the_helper_and_replies_with_the_inspection() {
        use crate::remote::pci::fixture;
        if fixture::running_as_root() {
            eprintln!("skipping: root ignores the helper's test seams");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let sysfs = dir.path().join("sys");
        let dev = dir.path().join("dev");
        fixture::build(&sysfs);
        std::fs::create_dir_all(dev.join("vfio")).unwrap();
        std::fs::write(dev.join("vfio/18"), "").unwrap();
        std::fs::write(dev.join("vfio/17"), "").unwrap();
        let group = std::process::Command::new("id")
            .arg("-gn")
            .output()
            .unwrap();
        let conf = dir.path().join("vfio.conf");
        std::fs::write(
            &conf,
            format!("GROUP={}\n", String::from_utf8_lossy(&group.stdout).trim()),
        )
        .unwrap();
        let script = dir.path().join("stackbox-vfio-bind");
        std::fs::write(&script, crate::remote::pci::helper_script()).unwrap();
        let mut agent = Agent::in_process(dir.path().join("agent"), Arc::new(Fake::default()));
        let mut command = vec!["env".to_owned()];
        command.extend(fixture::helper_env(&sysfs, &dev, &conf, "8:1"));
        command.push("sh".into());
        command.push(script.display().to_string());
        agent.pci_helper = command;
        let owner = uuid::Uuid::new_v4().to_string();
        let emit: Emitter = Arc::new(|_| Ok(()));

        let reply = agent
            .execute(
                request(
                    &owner,
                    Operation::PciBind {
                        bdf: "0000:0B:00.0".into(),
                    },
                ),
                emit.clone(),
            )
            .await
            .unwrap();
        assert!(matches!(reply, Reply::Inspection { .. }));
        let bound = sysfs.join("devices/pci0000:00/0000:0b:00.0");
        assert_eq!(
            std::fs::read_to_string(bound.join("driver_override")).unwrap(),
            "vfio-pci\n"
        );

        let refused = agent
            .execute(
                request(
                    &owner,
                    Operation::PciBind {
                        bdf: "0000:05:00.0".into(),
                    },
                ),
                emit.clone(),
            )
            .await
            .unwrap_err();
        assert!(refused.to_string().contains("root_disk"), "{refused:#}");
        assert_eq!(
            std::fs::read_to_string(sysfs.join("devices/pci0000:00/0000:05:00.0/driver_override"))
                .unwrap(),
            ""
        );

        let back = agent
            .execute(
                request(
                    &owner,
                    Operation::PciUnbind {
                        bdf: "0000:09:00.0".into(),
                    },
                ),
                emit,
            )
            .await
            .unwrap();
        assert!(matches!(back, Reply::Inspection { .. }));
        assert_eq!(
            std::fs::read_to_string(sysfs.join("bus/pci/drivers_probe")).unwrap(),
            "0000:09:00.0\n"
        );
    }

    /// A helper that refuses a device says why on stderr; the agent's error carries that reason,
    /// not the generic SSH wording, so the coordinator's job shows what the host refused.
    #[tokio::test]
    async fn a_refused_helper_reports_its_stderr_reason_through_execute() {
        let dir = tempfile::tempdir().unwrap();
        let mut agent = Agent::in_process(dir.path().join("agent"), Arc::new(Fake::default()));
        agent.pci_helper = vec![
            "sh".into(),
            "-c".into(),
            "printf 'sudo: noise\\nstackbox-vfio-bind: 0000:02:00.0 is protected (root_disk) and cannot be passed through\\n' >&2; exit 1".into(),
        ];
        let owner = uuid::Uuid::new_v4().to_string();
        let emit: Emitter = Arc::new(|_| Ok(()));
        let error = agent
            .execute(
                request(
                    &owner,
                    Operation::PciBind {
                        bdf: "0000:02:00.0".into(),
                    },
                ),
                emit,
            )
            .await
            .unwrap_err();
        let text = error.to_string();
        assert!(
            text.contains("stackbox-vfio-bind: 0000:02:00.0 is protected (root_disk) and cannot be passed through"),
            "{text}"
        );
        assert!(!text.contains("check remote prerequisites"), "{text}");
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
            pci_helper: crate::remote::pci::sudo_command(),
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
                                stackbox_id: "one".into(),
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
            matches!(agent.execute(request(&other, Operation::Scan), emit.clone()).await.unwrap(), Reply::Inventory { stackboxes } if stackboxes.is_empty())
        );
        agent
            .execute(
                request(
                    &owner,
                    Operation::Delete {
                        stackbox_id: "one".into(),
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
        assert!(!LockedRegistry::open(dir.path()).unwrap().state.stackboxes["one"].deleted);
        agent
            .execute(
                request(
                    &owner,
                    Operation::Delete {
                        stackbox_id: "one".into(),
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
                            stackbox_id: "foreign".into(),
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
    /// A restore and a later attach carry the devices across `Agent::execute`: the reservation
    /// records them on the plan, and `AttachPci` hands the backend the devices that reservation
    /// recorded, not any the request could name.
    #[tokio::test]
    async fn restore_and_attach_pci_keep_the_devices_across_the_agent_boundary() {
        let dir = tempfile::tempdir().unwrap();
        let backend = Arc::new(Fake::default());
        backend
            .snapshots
            .lock()
            .unwrap()
            .insert("snap-ref".into(), Default::default());
        let agent = Agent {
            dir: dir.path().into(),
            backend: backend.clone(),
            limits: Limits {
                memory_budget_mib: 50000,
                available_mib: 100000,
                virtualization: true,
                probe_ports: false,
            },
            runner: Runner::Inline,
            pci_helper: crate::remote::pci::sudo_command(),
        };
        let owner = uuid::Uuid::new_v4().to_string();
        let generation = uuid::Uuid::new_v4().to_string();
        let emit: Emitter = Arc::new(|_| Ok(()));
        let plan = layout(&PlanRequest {
            computes: 1,
            pci_devices: vec!["0000:04:00.3".into()],
            ..Default::default()
        })
        .unwrap();
        agent
            .execute(
                request(
                    &owner,
                    Operation::Restore {
                        stackbox_id: "restored-pci".into(),
                        generation: generation.clone(),
                        reference: "snap-ref".into(),
                        nodes: Vec::new(),
                        plan,
                        exposure: crate::model::Exposure {
                            host: "target.example".into(),
                            bind: "0.0.0.0".parse().unwrap(),
                            port_base: 15040,
                        },
                    },
                ),
                emit.clone(),
            )
            .await
            .expect("a restore with a PCI plan must reserve and start");
        let stored = LockedRegistry::open(dir.path()).unwrap();
        assert_eq!(
            stored.state.stackboxes["restored-pci"].plan.pci_devices,
            vec!["0000:04:00.3".to_owned()]
        );
        drop(stored);

        agent
            .execute(
                request(
                    &owner,
                    Operation::AttachPci {
                        stackbox_id: "restored-pci".into(),
                        generation: generation.clone(),
                    },
                ),
                emit.clone(),
            )
            .await
            .expect("the attach runs for the devices its reservation recorded");
        assert_eq!(
            backend.attached.lock().unwrap().as_slice(),
            &[(
                "restored-pci".into(),
                vec!["0000:04:00.3".to_owned()],
                false
            )]
        );

        // A stackbox whose reservation has no devices has nothing to attach.
        let plain = agent
            .execute(
                request(
                    &owner,
                    Operation::AttachPci {
                        stackbox_id: "no-such-stackbox".into(),
                        generation,
                    },
                ),
                emit,
            )
            .await;
        assert!(plain.is_err());
    }

    /// Runs a reserve and an expansion through the agent's `execute` boundary, the path the
    /// coordinator's link takes, and checks the stored plan keeps its passthrough devices.
    #[tokio::test]
    async fn reserve_and_expand_keep_the_pci_devices_across_the_agent_boundary() {
        let dir = tempfile::tempdir().unwrap();
        let agent = Agent {
            dir: dir.path().into(),
            backend: Arc::new(Fake::default()),
            limits: Limits {
                memory_budget_mib: 50000,
                available_mib: 100000,
                virtualization: true,
                probe_ports: false,
            },
            runner: Runner::Inline,
            pci_helper: crate::remote::pci::sudo_command(),
        };
        let owner = uuid::Uuid::new_v4().to_string();
        let generation = uuid::Uuid::new_v4().to_string();
        let emit: Emitter = Arc::new(|_| Ok(()));
        let with_pci = |computes: u8, devices: &[&str]| PlanRequest {
            computes,
            pci_devices: devices.iter().map(|d| (*d).to_owned()).collect(),
            ..Default::default()
        };
        let plan = layout(&with_pci(1, &["0000:04:00.3"])).unwrap();
        assert_eq!(plan.pci_devices, vec!["0000:04:00.3".to_owned()]);
        agent
            .execute(
                request(
                    &owner,
                    Operation::Create {
                        stackbox_id: "pci-stackbox".into(),
                        generation: generation.clone(),
                        plan,
                        exposure: Exposure {
                            host: "target.example".into(),
                            bind: "0.0.0.0".parse().unwrap(),
                            port_base: 15000,
                        },
                        share: None,
                        source: None,
                    },
                ),
                emit.clone(),
            )
            .await
            .expect("a PCI plan the layout accepts must reserve");
        let stored = LockedRegistry::open(dir.path()).unwrap();
        assert_eq!(
            stored.state.stackboxes["pci-stackbox"].plan.pci_devices,
            vec!["0000:04:00.3".to_owned()]
        );
        drop(stored);

        // Expansion to two computes keeps the device on compute-1 and adds compute-2.
        let grown = layout(&with_pci(2, &["0000:04:00.3"])).unwrap();
        let suffixes: Vec<String> = grown
            .nodes
            .iter()
            .filter_map(|spec| crate::stackbox::suffix(spec.role, spec.index))
            .collect();
        let added: Vec<String> = grown
            .nodes
            .iter()
            .filter(|spec| spec.index == Some(2))
            .filter_map(|spec| crate::stackbox::suffix(spec.role, spec.index))
            .collect();
        assert!(!added.is_empty());
        agent
            .execute(
                request(
                    &owner,
                    Operation::Expand {
                        stackbox_id: "pci-stackbox".into(),
                        generation: generation.clone(),
                        plan: grown,
                        nodes: suffixes,
                        add: added,
                        uplink: None,
                        source: None,
                    },
                ),
                emit.clone(),
            )
            .await
            .expect("an expansion keeps the PCI plan it was reserved with");
        let stored = LockedRegistry::open(dir.path()).unwrap();
        assert_eq!(
            stored.state.stackboxes["pci-stackbox"].plan.pci_devices,
            vec!["0000:04:00.3".to_owned()]
        );
        assert_eq!(stored.state.stackboxes["pci-stackbox"].plan.computes, 2);
        drop(stored);

        // A malformed address is refused at the agent too, not only by the coordinator.
        let mut forged = layout(&with_pci(1, &[])).unwrap();
        forged.pci_devices = vec!["0000:04:20.3".into()];
        let refused = agent
            .execute(
                request(
                    &owner,
                    Operation::Create {
                        stackbox_id: "bad-pci-2".into(),
                        generation: generation.clone(),
                        plan: forged,
                        exposure: Exposure {
                            host: "target.example".into(),
                            bind: "0.0.0.0".parse().unwrap(),
                            port_base: 15020,
                        },
                        share: None,
                        source: None,
                    },
                ),
                emit,
            )
            .await
            .unwrap_err();
        assert!(
            refused.to_string().contains("0000:04:20.3")
                || refused.to_string().contains("supported layout"),
            "{refused:#}"
        );
    }
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
                stackbox_id: "one".into(),
                generation: generation.into(),
                from: crate::kolla::Phase::Prepare,
                job_id: job_id.into(),
                reference: None,
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
        // A request without the field, as an older coordinator sends it, deploys as before.
        assert_eq!(backend.references.lock().unwrap().as_slice(), [None]);
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
        fn remove_golden(&self) -> futures::future::BoxFuture<'_, Result<usize>> {
            Box::pin(async { bail!("not in this test") })
        }
        fn scan(
            &self,
        ) -> futures::future::BoxFuture<
            '_,
            Result<BTreeMap<String, Vec<crate::stackbox::StackboxNode>>>,
        > {
            Box::pin(async { bail!("not in this test") })
        }
        fn create(
            &self,
            _: String,
            _: Plan,
            _: Exposure,
        ) -> futures::future::BoxFuture<'_, Result<Vec<crate::stackbox::StackboxNode>>> {
            Box::pin(async { bail!("not in this test") })
        }
        #[allow(clippy::too_many_arguments)]
        fn deploy(
            &self,
            _: String,
            from: crate::kolla::Phase,
            _: Option<Exposure>,
            _: Vec<String>,
            _: String,
            _: Option<String>,
            _: Option<crate::model::GpuConfig>,
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
    async fn purge_all_removes_every_reserved_stackbox_and_forgets_them() {
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
                probe_ports: false,
            },
            runner: Runner::Inline,
            pci_helper: crate::remote::pci::sudo_command(),
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
        // Both coordinators' VMs are this host's stackboxes, so a purge removes all of them.
        let reply = agent
            .execute(request(&owner, Operation::Purge), emit.clone())
            .await
            .unwrap();
        assert!(
            matches!(reply, Reply::Purged { stackboxes: 2, .. }),
            "{reply:?}"
        );
        assert!(backend.stackboxes.lock().unwrap().is_empty());
        assert!(matches!(
            agent
                .execute(request(&owner, Operation::Scan), emit.clone())
                .await
                .unwrap(),
            Reply::Inventory { stackboxes } if stackboxes.is_empty()
        ));
        // A second purge has nothing left and still succeeds.
        assert!(matches!(
            agent
                .execute(request(&owner, Operation::Purge), emit)
                .await
                .unwrap(),
            Reply::Purged { stackboxes: 0, .. }
        ));
    }

    /// An agent over `dir` whose backend holds the VMs of every coordinator it serves. Its tests
    /// run in parallel and share the fixed port bases below, so nothing here binds those ports.
    fn agent_over(dir: &std::path::Path, backend: Arc<Fake>) -> Agent {
        Agent {
            dir: dir.to_path_buf(),
            backend,
            limits: Limits {
                memory_budget_mib: 40_000,
                available_mib: 64_000,
                virtualization: true,
                probe_ports: false,
            },
            runner: Runner::Inline,
            pci_helper: crate::remote::pci::sudo_command(),
        }
    }

    #[tokio::test]
    async fn own_purge_removes_only_this_coordinators_reservations() {
        let dir = tempfile::tempdir().unwrap();
        let backend = Arc::new(Fake::default());
        let agent = agent_over(dir.path(), backend.clone());
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
        let reply = agent
            .execute(request(&owner, Operation::PurgeOwn), emit.clone())
            .await
            .unwrap();
        assert!(
            matches!(
                reply,
                Reply::Purged {
                    stackboxes: 1,
                    kept_for_others: Some(1)
                }
            ),
            "{reply:?}"
        );
        // The other coordinator's VM and its reservation stay, and it still sees its own.
        assert_eq!(
            backend
                .stackboxes
                .lock()
                .unwrap()
                .keys()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            ["two"]
        );
        assert!(matches!(
            agent
                .execute(request(&other, Operation::Scan), emit.clone())
                .await
                .unwrap(),
            Reply::Inventory { stackboxes } if stackboxes.keys().map(String::as_str).collect::<Vec<_>>() == ["two"]
        ));
        // A retry after a lost reply finds nothing of its own left and succeeds.
        let retry = agent
            .execute(request(&owner, Operation::PurgeOwn), emit)
            .await
            .unwrap();
        assert!(
            matches!(
                retry,
                Reply::Purged {
                    stackboxes: 0,
                    kept_for_others: Some(1)
                }
            ),
            "{retry:?}"
        );
        assert_eq!(backend.stackboxes.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn own_purge_keeps_a_reservation_whose_vm_cannot_be_deleted() {
        let dir = tempfile::tempdir().unwrap();
        let backend = Arc::new(Fake::default());
        let agent = agent_over(dir.path(), backend.clone());
        let owner = uuid::Uuid::new_v4().to_string();
        let emit: Emitter = Arc::new(|_| Ok(()));
        agent
            .execute(
                request(
                    &owner,
                    create("one", &uuid::Uuid::new_v4().to_string(), 15000),
                ),
                emit.clone(),
            )
            .await
            .unwrap();
        backend.fail_delete.store(true, Ordering::SeqCst);
        let error = agent
            .execute(request(&owner, Operation::PurgeOwn), emit.clone())
            .await
            .unwrap_err();
        assert!(
            error.to_string().contains("failed to remove 1"),
            "{error:#}"
        );
        // The reservation stays, so the coordinator's retry still knows what to clean up.
        backend.fail_delete.store(false, Ordering::SeqCst);
        assert!(matches!(
            agent
                .execute(request(&owner, Operation::PurgeOwn), emit)
                .await
                .unwrap(),
            Reply::Purged {
                stackboxes: 1,
                kept_for_others: Some(0)
            }
        ));
    }

    #[tokio::test]
    async fn own_purge_refuses_to_run_while_another_create_holds_the_lock() {
        let dir = tempfile::tempdir().unwrap();
        let backend = Arc::new(Fake::default());
        let agent = agent_over(dir.path(), backend.clone());
        let owner = uuid::Uuid::new_v4().to_string();
        let emit: Emitter = Arc::new(|_| Ok(()));
        agent
            .execute(
                request(
                    &owner,
                    create("one", &uuid::Uuid::new_v4().to_string(), 15000),
                ),
                emit.clone(),
            )
            .await
            .unwrap();
        // A second coordinator's purge (or a create) holds the lock: this one deletes nothing.
        let held = lock(dir.path(), "create.lock").unwrap();
        let error = agent
            .execute(request(&owner, Operation::PurgeOwn), emit.clone())
            .await
            .unwrap_err();
        assert!(error.to_string().contains("already running"), "{error:#}");
        assert_eq!(backend.stackboxes.lock().unwrap().len(), 1);
        drop(held);
        assert!(matches!(
            agent
                .execute(request(&owner, Operation::PurgeOwn), emit)
                .await
                .unwrap(),
            Reply::Purged {
                stackboxes: 1,
                kept_for_others: Some(0)
            }
        ));
    }

    /// The golden groups are shared by every stackbox on the host, so `RemoveGolden` deletes them
    /// only when the agent itself finds no reservation: a coordinator's check before the call can
    /// be overtaken by another coordinator's create.
    #[tokio::test]
    async fn remove_golden_is_refused_while_any_reservation_is_held() {
        let dir = tempfile::tempdir().unwrap();
        let backend = Arc::new(Fake::default());
        let agent = agent_over(dir.path(), backend.clone());
        let owner = uuid::Uuid::new_v4().to_string();
        let other = uuid::Uuid::new_v4().to_string();
        let emit: Emitter = Arc::new(|_| Ok(()));
        agent
            .execute(
                request(&other, create("two", &uuid::Uuid::new_v4().to_string(), 15010)),
                emit.clone(),
            )
            .await
            .unwrap();
        let error = agent
            .execute(request(&owner, Operation::RemoveGolden), emit.clone())
            .await
            .unwrap_err();
        assert!(error.to_string().contains("in use"), "{error:#}");
        assert_eq!(backend.golden_removals.load(Ordering::SeqCst), 0);
        // With no reservation left on the host, the same request removes the groups.
        agent
            .execute(request(&owner, Operation::Purge), emit.clone())
            .await
            .unwrap();
        assert!(matches!(
            agent
                .execute(request(&owner, Operation::RemoveGolden), emit)
                .await
                .unwrap(),
            Reply::GoldenRemoved { groups: 1 }
        ));
        assert_eq!(backend.golden_removals.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn remove_golden_fails_safe_while_a_create_or_purge_holds_the_lock() {
        let dir = tempfile::tempdir().unwrap();
        let backend = Arc::new(Fake::default());
        let agent = agent_over(dir.path(), backend.clone());
        let owner = uuid::Uuid::new_v4().to_string();
        let emit: Emitter = Arc::new(|_| Ok(()));
        let held = lock(dir.path(), "create.lock").unwrap();
        let error = agent
            .execute(request(&owner, Operation::RemoveGolden), emit.clone())
            .await
            .unwrap_err();
        assert!(error.to_string().contains("already running"), "{error:#}");
        assert_eq!(backend.golden_removals.load(Ordering::SeqCst), 0);
        drop(held);
    }

    /// What a coordinator built before `PurgeOwn` reads as a purge reply: `Reply` denies unknown
    /// fields, so any extra field makes it refuse the reply after the VMs are already gone.
    #[derive(serde::Deserialize)]
    #[serde(tag = "result", rename_all = "snake_case", deny_unknown_fields)]
    #[allow(dead_code)]
    enum OldReply {
        Purged { stackboxes: usize },
    }

    #[tokio::test]
    async fn purge_replies_keep_the_shape_an_older_coordinator_reads() {
        let dir = tempfile::tempdir().unwrap();
        let backend = Arc::new(Fake::default());
        let agent = agent_over(dir.path(), backend.clone());
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
        let reply = agent
            .execute(request(&owner, Operation::Purge), emit)
            .await
            .unwrap();
        let json = serde_json::to_string(&reply).unwrap();
        assert_eq!(json, r#"{"result":"purged","stackboxes":2}"#);
        let old: OldReply = serde_json::from_str(&json).unwrap();
        assert!(matches!(old, OldReply::Purged { stackboxes: 2 }));
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
            pci_helper: crate::remote::pci::sudo_command(),
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
                    stackbox_id: "one".into(),
                    generation: generation.clone(),
                    node: None,
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

    /// A script session over the agent link: the coordinator's EOF travels as a frame that
    /// closes the shell's stdin, and the session stays up until the real `bash -s` behind the
    /// fake backend finishes. A dropped connection would mean a client that left, and the
    /// session would hang the shell up.
    #[tokio::test]
    async fn a_script_session_over_the_link_runs_to_its_end_after_eof() {
        let dir = tempfile::tempdir().unwrap();
        let backend = Arc::new(Fake::default());
        backend.run_scripts.store(true, Ordering::SeqCst);
        let agent = Arc::new(Agent {
            dir: dir.path().into(),
            backend,
            limits: Limits {
                memory_budget_mib: 10000,
                available_mib: 10000,
                virtualization: true,
                probe_ports: true,
            },
            runner: Runner::Inline,
            pci_helper: crate::remote::pci::sudo_command(),
        });
        let owner = uuid::Uuid::new_v4().to_string();
        let generation = uuid::Uuid::new_v4().to_string();
        let silent: Emitter = Arc::new(|_| Ok(()));
        agent
            .execute(request(&owner, create("one", &generation, 23950)), silent)
            .await
            .unwrap();
        let request = request(
            &owner,
            Operation::Shell {
                stackbox_id: "one".into(),
                generation,
                node: None,
                size: TerminalSize {
                    rows: 40,
                    cols: 240,
                },
            },
        );
        let link = std::sync::Arc::new(crate::remote::link::InProcessLink { agent });
        use crate::remote::link::AgentLink;
        let script = "printf 'a\\n'; sleep 0.3\nprintf 'b\\n'";
        let session = crate::stackbox::ScriptSession::new({
            let link = link.clone();
            move |io| Box::pin(async move { link.terminal(request, io).await })
        });
        let out = session.run(script).await.unwrap();
        assert_eq!(out, "a\nb");
    }
}
