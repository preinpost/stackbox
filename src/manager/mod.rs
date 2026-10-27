mod backend;
pub(crate) mod hosts;
mod logs;
mod snapshots;
mod store;

use crate::{
    farm::Role,
    kolla::{DeployEvent, EventSink},
    model::*,
};
use anyhow::{Context, Result, anyhow, bail};
pub use backend::{Backend, MicrosandboxBackend};
use std::{
    net::{IpAddr, Ipv4Addr, TcpListener},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};
use store::{State, Store};
use tokio::sync::Semaphore;

#[derive(Clone)]
pub struct ManagerOptions {
    pub state_dir: PathBuf,
    pub public_bind: IpAddr,
    pub memory_budget_mib: u64,
    pub agent_binary: Option<PathBuf>,
}
impl ManagerOptions {
    pub fn local(state_dir: PathBuf) -> Self {
        let sys = sysinfo::System::new_all();
        Self {
            state_dir,
            public_bind: IpAddr::V4(Ipv4Addr::LOCALHOST),
            memory_budget_mib: sys.total_memory() / (1024 * 1024) * 80 / 100,
            agent_binary: std::env::var_os("FARM_AGENT_PATH").map(PathBuf::from),
        }
    }
}
pub fn default_state_dir() -> PathBuf {
    if let Some(path) = std::env::var_os("XDG_DATA_HOME") {
        PathBuf::from(path).join("farm")
    } else {
        PathBuf::from(std::env::var_os("HOME").unwrap_or_else(|| ".".into()))
            .join(".local/share/farm")
    }
}

#[derive(Clone)]
pub struct FarmManager(Arc<Inner>);
struct Inner {
    store: Mutex<Store>,
    logs: Mutex<logs::Logs>,
    backend: Arc<dyn Backend>,
    options: ManagerOptions,
    create_gate: Semaphore,
    deploy_gate: Semaphore,
    /// Serializes capture, export and import, which each move the whole VM disk.
    snapshot_gate: Semaphore,
    accepting: AtomicBool,
    vault: crate::remote::Vault,
    host_gate: Semaphore,
    artifacts: std::collections::BTreeMap<String, Arc<crate::remote::artifact::Artifact>>,
}
#[derive(Debug)]
pub struct NotFound(pub String);
impl std::fmt::Display for NotFound {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} not found", self.0)
    }
}
impl std::error::Error for NotFound {}
#[derive(Debug)]
pub struct Conflict(pub String);
impl std::fmt::Display for Conflict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for Conflict {}
fn conflict(message: impl Into<String>) -> anyhow::Error {
    Conflict(message.into()).into()
}
#[derive(Debug)]
pub struct BadRequest(pub String);
impl std::fmt::Display for BadRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for BadRequest {}
fn bad_request(message: impl Into<String>) -> anyhow::Error {
    BadRequest(message.into()).into()
}

impl FarmManager {
    pub async fn open(options: ManagerOptions) -> Result<Self> {
        Self::with_backend(options, backend::real()).await
    }
    pub async fn with_backend(options: ManagerOptions, backend: Arc<dyn Backend>) -> Result<Self> {
        let mut agent_paths = Vec::new();
        if let Some(path) = &options.agent_binary {
            agent_paths.push(path.clone());
        }
        // Unit tests must not pick up developer binaries from the working tree.
        let agent_dir = std::env::var_os("FARM_AGENT_DIR")
            .map(PathBuf::from)
            .or_else(|| {
                if cfg!(test) {
                    None
                } else {
                    Some(PathBuf::from("dist/agent"))
                }
            });
        if let Some(agent_dir) = agent_dir {
            for arch in ["aarch64", "x86_64"] {
                agent_paths.push(agent_dir.join(arch).join("farm-agent"));
            }
        }
        let artifacts = crate::remote::artifact::load_all(&agent_paths)?
            .into_iter()
            .map(|(arch, artifact)| (arch, Arc::new(artifact)))
            .collect();
        let store = Store::open(&options.state_dir)?;
        let vault = crate::remote::Vault::open(&options.state_dir, !store.state.hosts.is_empty())?;
        for host in store.state.hosts.values() {
            // A missing/wrong key must never silently discard or replace existing credentials.
            vault.unseal(&host.credentials, &host.aad())?;
        }
        let logs = logs::Logs::new(&options.state_dir)?;
        let manager = Self(Arc::new(Inner {
            store: Mutex::new(store),
            logs: Mutex::new(logs),
            backend,
            options,
            create_gate: Semaphore::new(1),
            deploy_gate: Semaphore::new(1),
            snapshot_gate: Semaphore::new(1),
            accepting: AtomicBool::new(true),
            vault,
            host_gate: Semaphore::new(2),
            artifacts,
        }));
        manager.update(|state| {
            for job in state.host_jobs.values_mut().filter(|j| j.status.active()) {
                job.status = JobStatus::Interrupted;
                job.error = Some(
                    "daemon stopped during this job; inspect the remote host before retrying"
                        .into(),
                );
                job.finished_at = Some(now());
                if let Some(stored) = state.hosts.get_mut(&job.host_id) {
                    stored.host.status = crate::remote::HostStatus::Failed;
                    stored.host.active_job = None;
                    stored.host.error = job.error.clone();
                    stored.host.updated_at = now();
                }
            }
            for job in state.jobs.values_mut().filter(|j| j.status.active()) {
                job.status = JobStatus::Interrupted;
                job.error = Some(
                    "daemon stopped during this job; inspect the VMs and retry explicitly".into(),
                );
                job.finished_at = Some(now());
                if let Some(farm) = state.farms.get_mut(&job.farm_id) {
                    farm.active_job = None;
                    farm.status = FarmStatus::Failed;
                    farm.error = job.error.clone();
                    farm.updated_at = now();
                }
            }
            snapshots::recover_interrupted(state);
            Ok(())
        })?;
        manager.reconcile().await?;
        Ok(manager)
    }
    fn update<T>(&self, f: impl FnOnce(&mut State) -> Result<T>) -> Result<T> {
        self.0
            .store
            .lock()
            .map_err(|_| anyhow!("state mutex poisoned"))?
            .update(f)
    }
    pub fn list(&self) -> Result<Vec<FarmRecord>> {
        Ok(self
            .0
            .store
            .lock()
            .map_err(|_| anyhow!("state mutex poisoned"))?
            .state
            .farms
            .values()
            .filter(|f| f.status != FarmStatus::Deleted)
            .cloned()
            .collect())
    }
    pub fn get(&self, id: &str) -> Result<FarmRecord> {
        self.0
            .store
            .lock()
            .map_err(|_| anyhow!("state mutex poisoned"))?
            .state
            .farms
            .get(id)
            .cloned()
            .ok_or_else(|| NotFound(format!("farm '{id}'")).into())
    }
    pub fn job(&self, id: &str) -> Result<Job> {
        self.0
            .store
            .lock()
            .map_err(|_| anyhow!("state mutex poisoned"))?
            .state
            .jobs
            .get(id)
            .cloned()
            .ok_or_else(|| NotFound(format!("job '{id}'")).into())
    }
    pub fn jobs(&self) -> Result<Vec<Job>> {
        Ok(self
            .0
            .store
            .lock()
            .map_err(|_| anyhow!("state mutex poisoned"))?
            .state
            .jobs
            .values()
            .cloned()
            .collect())
    }
    pub fn logs(&self, farm: &str, after: u64, limit: usize) -> Result<Vec<LogEntry>> {
        self.get(farm)?;
        self.0
            .logs
            .lock()
            .map_err(|_| anyhow!("log mutex poisoned"))?
            .tail(farm, after, limit)
    }
    fn log(
        &self,
        farm: &str,
        job: &str,
        phase: Option<crate::kolla::Phase>,
        message: String,
    ) -> Result<()> {
        self.0
            .logs
            .lock()
            .map_err(|_| anyhow!("log mutex poisoned"))?
            .append(farm, job, phase, message)
    }
    pub async fn endpoints(&self, id: &str) -> Result<Endpoints> {
        let farm = self.get(id)?;
        if farm.status != FarmStatus::Ready || farm.active_job.is_some() {
            return Err(conflict(
                "endpoints are available after a successful deployment",
            ));
        }
        let components = farm_components(&farm);
        self.backend_for(&farm)?
            .endpoints(id.into(), farm.exposure, components)
            .await
    }
    pub fn options(&self) -> &ManagerOptions {
        &self.0.options
    }
    pub fn state_dir(&self) -> &Path {
        &self.0.options.state_dir
    }
    pub fn agent_artifact(&self) -> Option<crate::remote::AgentArtifactInfo> {
        self.agent_artifacts().into_iter().next()
    }
    pub fn agent_artifacts(&self) -> Vec<crate::remote::AgentArtifactInfo> {
        self.0
            .artifacts
            .values()
            .map(|artifact| artifact.info.clone())
            .collect()
    }
    pub fn artifact_for(&self, arch: &str) -> Result<Arc<crate::remote::artifact::Artifact>> {
        self.0.artifacts.get(arch).cloned().ok_or_else(|| {
            anyhow!(
                "no {arch} farm-agent is loaded; build it with scripts/build-agent.sh {arch} and restart the coordinator"
            )
        })
    }
    fn backend_for(&self, farm: &FarmRecord) -> Result<Arc<dyn Backend>> {
        match &farm.host_id {
            None => Ok(self.0.backend.clone()),
            Some(host_id) => self.remote_backend(
                host_id,
                farm.agent_sha256
                    .as_deref()
                    .context("remote farm has no pinned agent")?,
                farm.remote_generation
                    .as_deref()
                    .context("remote farm has no generation")?,
            ),
        }
    }
    fn remote_backend(
        &self,
        host_id: &str,
        sha256: &str,
        generation: &str,
    ) -> Result<Arc<dyn Backend>> {
        let store = self
            .0
            .store
            .lock()
            .map_err(|_| anyhow!("state mutex poisoned"))?;
        let stored = store
            .state
            .hosts
            .get(host_id)
            .ok_or_else(|| NotFound(format!("host '{host_id}'")))?;
        Ok(Arc::new(crate::remote::backend::RemoteBackend {
            host: stored.host.clone(),
            credentials: self.0.vault.unseal(&stored.credentials, &stored.aad())?,
            sha256: sha256.into(),
            coordinator_id: store.state.coordinator_id.clone(),
            generation: generation.into(),
        }))
    }

    pub fn create(&self, request: CreateRequest) -> Result<Job> {
        let mut plan = request.validate()?;
        let id = request.id.clone();
        let job = self.update(|state| {
            self.ensure_accepting()?;
            if state
                .farms
                .get(&id)
                .is_some_and(|f| f.status != FarmStatus::Deleted)
            {
                return Err(conflict(format!("farm '{id}' already exists")));
            }
            if let Some(snapshot_id) = &request.snapshot_id {
                plan = snapshots::restorable(state, snapshot_id)?.plan.clone();
            }
            let reserved: u64 = state
                .farms
                .values()
                .filter(|f| f.status != FarmStatus::Deleted && f.host_id == request.host_id)
                .map(FarmRecord::memory_mib)
                .sum();
            let (budget, agent_sha256, remote_generation) = if let Some(host_id) = &request.host_id {
                let host = &state.hosts.get(host_id).ok_or_else(|| NotFound(format!("host '{host_id}'")))?.host;
                if !matches!(host.status, crate::remote::HostStatus::Ready) || host.active_job.is_some() { return Err(conflict("remote host must finish environment preparation before creating farms")); }
                let inspection = host.inspection.as_ref().context("ready host has no inspection")?;
                if now().saturating_sub(inspection.checked_at) > 300 { return Err(conflict("remote inspection is older than 5 minutes; check the host again before creating a farm")); }
                let agent = host.agent.as_ref().ok_or_else(|| conflict("install the Farm execution agent on this host first"))?;
                if !agent.artifact.supports_single_vm { return Err(conflict("install the updated Farm execution agent to create single-VM farms")); }
                if agent.artifact.build_id != crate::BUILD_ID {
                    return Err(conflict(format!(
                        "the agent installed on this host was built from {} but this coordinator is {};
rebuild it with `task agent-build` and install the agent again",
                        agent.artifact.build_id, crate::BUILD_ID
                    )));
                }
                (inspection.memory_total_mib * 80 / 100, Some(agent.artifact.sha256.clone()), Some(uuid::Uuid::new_v4().to_string()))
            } else { (self.0.options.memory_budget_mib, None, None) };
            if reserved.saturating_add(plan.memory_mib) > budget {
                return Err(conflict(format!(
                    "memory budget exceeded: {reserved} MiB reserved + {} MiB requested > {} MiB",
                    plan.memory_mib, budget
                )));
            }
            let exposure =
                allocate_ports(state, request.external_host, if request.host_id.is_some() { IpAddr::V4(Ipv4Addr::UNSPECIFIED) } else { self.0.options.public_bind }, request.host_id.as_deref())?;
            let mut job = new_job(&id, if request.snapshot_id.is_some() { JobKind::Restore } else { JobKind::Create });
            job.snapshot_id = request.snapshot_id.clone();
            state.farms.insert(
                id.clone(),
                FarmRecord {
                    id: id.clone(),
                    host_id: request.host_id,
                    agent_sha256,
                    remote_generation,
                    observation_error: None,
                    last_seen_at: None,
                    status: FarmStatus::Creating,
                    plan: Some(plan),
                    exposure: Some(exposure),
                    owner: request.owner,
                    expires_at: request.expires_at,
                    nodes: vec![],
                    phase: None,
                    step: None,
                    error: None,
                    active_job: Some(job.id.clone()),
                    created_at: now(),
                    updated_at: now(),
                    snapshot_id: request.snapshot_id,
                },
            );
            state.jobs.insert(job.id.clone(), job.clone());
            Ok(job)
        })?;
        self.spawn(job.clone(), None);
        Ok(job)
    }
    pub fn deploy(&self, id: &str, request: DeployRequest) -> Result<Job> {
        let job = self.update(|state| {
            self.ensure_accepting()?;
            let farm = state
                .farms
                .get_mut(id)
                .ok_or_else(|| NotFound(format!("farm '{id}'")))?;
            ensure_idle(farm)?;
            if farm
                .host_id
                .as_ref()
                .and_then(|id| state.hosts.get(id))
                .is_some_and(|h| h.host.active_job.is_some())
            {
                return Err(conflict("host preparation is running"));
            }
            if !matches!(
                farm.status,
                FarmStatus::Created | FarmStatus::Ready | FarmStatus::Failed
            ) {
                return Err(conflict("farm must be created before deploying"));
            }
            if farm.plan.as_ref().is_some_and(|plan| plan.nodes.len() != 1 || plan.nodes[0].role != Role::AllInOne)
                || farm.nodes.iter().any(|node| node.role != Some(Role::AllInOne))
            {
                return Err(conflict("legacy multi-node farms cannot be deployed by the single-VM engine; keep or delete this farm and create a new farm"));
            }
            let expected = 1;
            if farm.nodes.len() != expected || farm.nodes.iter().any(|n| n.status != "running") {
                return Err(conflict(format!(
                    "all {expected} farm nodes must be running before deploying"
                )));
            }
            // A remote farm keeps the agent it was created with. Move it to the host's current
            // agent once that agent matches this coordinator, so reinstalling the agent also
            // repairs existing farms.
            let mut repinned = None;
            if let Some(host_id) = farm.host_id.clone()
                && let Some(stored) = state.hosts.get(&host_id)
                && let Some(agent) = &stored.host.agent
                && farm.agent_sha256.as_deref() != Some(agent.artifact.sha256.as_str())
            {
                if agent.artifact.build_id != crate::BUILD_ID {
                    return Err(conflict(format!(
                        "the agent installed on {host_id} was built from {} but this coordinator is {}; reinstall it from Hosts first",
                        agent.artifact.build_id, crate::BUILD_ID
                    )));
                }
                repinned = Some(agent.artifact.sha256.clone());
                farm.agent_sha256 = repinned.clone();
            }
            let mut job = new_job(id, JobKind::Deploy);
            job.phase = Some(request.from);
            farm.status = FarmStatus::Deploying;
            farm.active_job = Some(job.id.clone());
            farm.error = None;
            farm.phase = Some(request.from);
            farm.updated_at = now();
            state.jobs.insert(job.id.clone(), job.clone());
            Ok((job, repinned))
        })?;
        let (job, repinned) = job;
        if let Some(digest) = repinned {
            self.log(
                &job.farm_id,
                &job.id,
                None,
                format!(
                    "agent: using the host's current agent {}
",
                    &digest[..12]
                ),
            )?;
        }
        self.spawn(job.clone(), Some(request));
        Ok(job)
    }
    /// Re-check a farm whose VM started from disk (or was restarted) without deploying again.
    pub fn settle(&self, id: &str) -> Result<Job> {
        let job = self.update(|state| {
            self.ensure_accepting()?;
            let farm = state
                .farms
                .get_mut(id)
                .ok_or_else(|| NotFound(format!("farm '{id}'")))?;
            ensure_idle(farm)?;
            if farm
                .host_id
                .as_ref()
                .and_then(|id| state.hosts.get(id))
                .is_some_and(|h| h.host.active_job.is_some())
            {
                return Err(conflict("host preparation is running"));
            }
            // Only a farm that was deployed has services to wait for.
            if !matches!(
                farm.status,
                FarmStatus::Ready | FarmStatus::Created | FarmStatus::Broken | FarmStatus::Settling
            ) {
                return Err(conflict(
                    "farm must be deployed (or restored from a deployed snapshot) to check its services",
                ));
            }
            let deployed = state.jobs.values().any(|j| {
                j.farm_id == id
                    && j.kind == JobKind::Deploy
                    && j.status == JobStatus::Succeeded
            }) || farm
                .snapshot_id
                .as_ref()
                .and_then(|snapshot| state.snapshots.get(snapshot))
                .is_some_and(|snapshot| snapshot.deployed);
            if !deployed {
                return Err(conflict(
                    "farm has no successful deployment; deploy it before checking its services",
                ));
            }
            if farm
                .plan
                .as_ref()
                .is_none_or(|plan| plan.nodes.len() != 1)
            {
                return Err(conflict("farms imported from older versions cannot be checked"));
            }
            if farm.nodes.len() != 1 || farm.nodes[0].status != "running" {
                return Err(conflict("the farm's VM must be running to check its services"));
            }
            let mut job = new_job(id, JobKind::Settle);
            job.phase = Some(crate::kolla::Phase::Verify);
            farm.status = FarmStatus::Settling;
            farm.active_job = Some(job.id.clone());
            farm.error = None;
            farm.phase = Some(crate::kolla::Phase::Verify);
            farm.updated_at = now();
            state.jobs.insert(job.id.clone(), job.clone());
            Ok(job)
        })?;
        self.spawn(job.clone(), None);
        Ok(job)
    }
    pub fn delete(&self, id: &str) -> Result<Job> {
        self.delete_farm(id, false)
    }

    /// `own_host_job` lets a purge delete the farms of the host whose job it already holds.
    pub(crate) fn delete_farm(&self, id: &str, own_host_job: bool) -> Result<Job> {
        let job = self.update(|state| {
            self.ensure_accepting()?;
            let farm = state
                .farms
                .get_mut(id)
                .ok_or_else(|| NotFound(format!("farm '{id}'")))?;
            ensure_idle(farm)?;
            if !own_host_job
                && farm
                    .host_id
                    .as_ref()
                    .and_then(|id| state.hosts.get(id))
                    .is_some_and(|h| h.host.active_job.is_some())
            {
                return Err(conflict("host preparation is running"));
            }
            if farm.status == FarmStatus::Deleted {
                return Err(conflict("farm is already deleted"));
            }
            let job = new_job(id, JobKind::Delete);
            farm.status = FarmStatus::Deleting;
            farm.active_job = Some(job.id.clone());
            farm.error = None;
            farm.updated_at = now();
            state.jobs.insert(job.id.clone(), job.clone());
            Ok(job)
        })?;
        self.spawn(job.clone(), None);
        Ok(job)
    }
    fn ensure_accepting(&self) -> Result<()> {
        if !self.0.accepting.load(Ordering::SeqCst) {
            bail!(Conflict("daemon is shutting down".into()));
        }
        Ok(())
    }
    fn spawn(&self, job: Job, request: Option<DeployRequest>) {
        let manager = self.clone();
        tokio::spawn(async move {
            // Catch task panics too, so the durable job never remains Running indefinitely.
            use futures::FutureExt;
            let result = std::panic::AssertUnwindSafe(manager.execute(&job, request))
                .catch_unwind()
                .await
                .unwrap_or_else(|_| Err(anyhow!("job panicked")));
            // Refresh partial VM results even if creation or deletion failed.
            if let Ok(backend) = manager
                .get(&job.farm_id)
                .and_then(|farm| manager.backend_for(&farm))
                && let Ok(mut observed) = backend.scan().await
            {
                let nodes = observed.remove(&job.farm_id).unwrap_or_default();
                if let Err(error) = manager.update(|state| {
                    state
                        .farms
                        .get_mut(&job.farm_id)
                        .context("farm missing")?
                        .nodes = nodes;
                    Ok(())
                }) {
                    eprintln!("cannot refresh job inventory: {error:#}");
                }
            }
            if let Err(error) = manager.finish(&job, result) {
                eprintln!("cannot persist completion of {}: {error:#}", job.id);
            }
        });
    }
    async fn execute(&self, job: &Job, request: Option<DeployRequest>) -> Result<()> {
        let _permit = match job.kind {
            JobKind::Create | JobKind::Restore => Some(self.0.create_gate.acquire().await?),
            JobKind::Deploy => Some(self.0.deploy_gate.acquire().await?),
            JobKind::Snapshot => Some(self.0.snapshot_gate.acquire().await?),
            JobKind::Settle => Some(self.0.deploy_gate.acquire().await?),
            JobKind::Delete => None,
        };
        self.update(|state| {
            let j = state.jobs.get_mut(&job.id).context("job missing")?;
            j.status = JobStatus::Running;
            j.started_at = Some(now());
            Ok(())
        })?;
        self.log(
            &job.farm_id,
            &job.id,
            None,
            format!("{:?} started", job.kind),
        )?;
        let farm = self.get(&job.farm_id)?;
        match job.kind {
            JobKind::Create => {
                let nodes = self
                    .backend_for(&farm)?
                    .create(
                        farm.id.clone(),
                        farm.plan.context("farm has no plan")?,
                        farm.exposure.context("farm has no exposure")?,
                    )
                    .await?;
                self.update(|state| {
                    state
                        .farms
                        .get_mut(&job.farm_id)
                        .context("farm missing")?
                        .nodes = nodes;
                    Ok(())
                })?;
            }
            JobKind::Deploy => {
                let components = farm_components(&farm);
                self.backend_for(&farm)?
                    .deploy(
                        farm.id,
                        request.unwrap_or_default().from,
                        farm.exposure,
                        components,
                        self.event_sink(job),
                    )
                    .await?;
            }
            JobKind::Delete => self.backend_for(&farm)?.delete(farm.id).await?,
            JobKind::Settle => {
                let components = farm_components(&farm);
                self.backend_for(&farm)?
                    .settle(farm.id, None, components, self.event_sink(job))
                    .await?;
            }
            JobKind::Snapshot => self.freeze_job(job, farm).await?,
            JobKind::Restore => self.restore_job(job, farm).await?,
        }
        Ok(())
    }
    /// Records deployment phases on the farm and job, and output in the farm log.
    fn event_sink(&self, job: &Job) -> EventSink {
        let manager = self.clone();
        let job = job.clone();
        Arc::new(move |event| {
            let phase = match event {
                DeployEvent::Step(step) => {
                    manager.update(|state| {
                        let farm = state.farms.get_mut(&job.farm_id).context("farm missing")?;
                        farm.step = Some(step.clone());
                        farm.updated_at = now();
                        Ok(())
                    })?;
                    manager.log(&job.farm_id, &job.id, None, format!("step: {step}"))?;
                    return Ok(());
                }
                DeployEvent::Phase(phase) => {
                    manager.update(|state| {
                        let farm = state.farms.get_mut(&job.farm_id).context("farm missing")?;
                        farm.phase = Some(phase);
                        farm.step = None;
                        farm.updated_at = now();
                        state.jobs.get_mut(&job.id).context("job missing")?.phase = Some(phase);
                        Ok(())
                    })?;
                    manager.log(&job.farm_id, &job.id, Some(phase), format!("phase {phase}"))?;
                    return Ok(());
                }
                DeployEvent::Output(message) => {
                    let phase = manager.job(&job.id)?.phase;
                    (phase, message)
                }
            };
            manager.log(&job.farm_id, &job.id, phase.0, phase.1)
        })
    }
    fn finish(&self, job: &Job, result: Result<()>) -> Result<()> {
        let error = result.err().map(|e| format!("{e:#}"));
        // A terminal state must still be persisted if writing the final log fails.
        let log_result = self.log(
            &job.farm_id,
            &job.id,
            self.job(&job.id)?.phase,
            error.clone().unwrap_or_else(|| "job succeeded".into()),
        );
        self.update(|state| {
            let current = state.jobs.get_mut(&job.id).context("job missing")?;
            current.status = if error.is_some() {
                JobStatus::Failed
            } else {
                JobStatus::Succeeded
            };
            current.error = error.clone();
            current.finished_at = Some(now());
            // A farm restored from a deployed snapshot is ready as soon as its services answer.
            let deployed = job
                .snapshot_id
                .as_ref()
                .and_then(|id| state.snapshots.get(id))
                .is_some_and(|s| s.deployed);
            let farm = state.farms.get_mut(&job.farm_id).context("farm missing")?;
            farm.status = match (job.kind, error.is_some()) {
                // Freezing leaves the farm as it was; a failure is reported on the job and snapshot.
                (JobKind::Snapshot, _) => farm.status,
                // A check that fails leaves the VM as it was; the user can run it again.
                (JobKind::Settle, true) => FarmStatus::Created,
                (JobKind::Settle, false) => FarmStatus::Ready,
                (_, true) => FarmStatus::Failed,
                (JobKind::Create, false) => FarmStatus::Created,
                (JobKind::Deploy, false) => FarmStatus::Ready,
                (JobKind::Delete, false) => FarmStatus::Deleted,
                (JobKind::Restore, false) if deployed => FarmStatus::Ready,
                (JobKind::Restore, false) => FarmStatus::Created,
            };
            farm.error = error;
            farm.active_job = None;
            farm.updated_at = now();
            if farm.status == FarmStatus::Deleted {
                farm.nodes.clear();
            }
            Ok(())
        })?;
        log_result
    }
    pub async fn wait(&self, id: &str) -> Result<Job> {
        loop {
            let job = self.job(id)?;
            if !job.status.active() {
                return Ok(job);
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    }
    pub async fn shutdown(&self) -> Result<()> {
        // Serialize admission with create/deploy/delete's state transaction.
        {
            let _lock = self
                .0
                .store
                .lock()
                .map_err(|_| anyhow!("state mutex poisoned"))?;
            self.0.accepting.store(false, Ordering::SeqCst);
        }
        loop {
            if self.jobs()?.iter().all(|j| !j.status.active())
                && self.host_jobs()?.iter().all(|j| !j.status.active())
                && self.snapshots()?.iter().all(|s| !snapshots::busy(s.status))
            {
                return Ok(());
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    }
    pub fn is_accepting(&self) -> bool {
        self.0.accepting.load(Ordering::SeqCst)
    }
    pub async fn reconcile(&self) -> Result<()> {
        use futures::StreamExt;
        let (revision, targets) = {
            let store = self
                .0
                .store
                .lock()
                .map_err(|_| anyhow!("state mutex poisoned"))?;
            let targets: std::collections::BTreeMap<_, _> = store
                .state
                .farms
                .values()
                .filter(|f| f.status != FarmStatus::Deleted)
                .filter_map(|f| Some((f.host_id.clone()?, f.agent_sha256.clone()?)))
                .collect();
            (store.revision, targets)
        };
        let local = self.0.backend.scan().await;
        let remote = futures::stream::iter(targets.into_iter().map(|(id, digest)| async move {
            let result = match self.remote_backend(&id, &digest, "") {
                Ok(backend) => backend.scan().await,
                Err(error) => Err(error),
            };
            (Some(id), result)
        }))
        .buffer_unordered(4)
        .collect::<Vec<_>>()
        .await;
        let mut observations = vec![(None, local)];
        observations.extend(remote);
        let mut store = self
            .0
            .store
            .lock()
            .map_err(|_| anyhow!("state mutex poisoned"))?;
        if store.revision != revision {
            return Ok(());
        }
        store.update(|state| {
            for (host_id, observation) in observations {
                match observation {
                    Ok(observed) => apply_inventory(state, observed, host_id.as_deref())?,
                    Err(error) => {
                        for farm in state.farms.values_mut().filter(|f| {
                            f.host_id == host_id
                                && f.status != FarmStatus::Deleted
                                && f.active_job.is_none()
                        }) {
                            farm.observation_error =
                                Some(format!("host inventory unavailable: {error}"));
                        }
                    }
                }
            }
            Ok(())
        })
    }
}
fn apply_inventory(
    state: &mut State,
    mut observed: std::collections::BTreeMap<String, Vec<crate::farm::FarmNode>>,
    host_id: Option<&str>,
) -> Result<()> {
    for farm in state
        .farms
        .values_mut()
        .filter(|f| f.host_id.as_deref() == host_id)
    {
        if farm.active_job.is_some() {
            observed.remove(&farm.id);
            continue;
        }
        let nodes = observed.remove(&farm.id).unwrap_or_default();
        if farm.status == FarmStatus::Deleted && nodes.is_empty() {
            continue;
        }
        let expected: Vec<String> = farm
            .plan
            .as_ref()
            .map(|p| {
                p.nodes
                    .iter()
                    .map(|n| format!("{}-{}", farm.id, n.role))
                    .collect()
            })
            .unwrap_or_else(|| {
                if nodes.iter().any(|node| node.role == Some(Role::AllInOne)) {
                    return vec![format!("{}-{}", farm.id, Role::AllInOne)];
                }
                // Imported legacy farms have no recorded plan. Preserve their
                // known role layout, including optional control/compute nodes.
                let mut roles = vec![
                    Role::Deployer,
                    Role::Lb,
                    Role::Infra,
                    Role::Keystone,
                    Role::Glance,
                ];
                for role in [Role::Control, Role::Compute] {
                    if nodes.iter().any(|node| node.role == Some(role)) {
                        roles.push(role);
                    }
                }
                roles
                    .iter()
                    .map(|role| format!("{}-{role}", farm.id))
                    .collect()
            });
        let intact = nodes.len() == expected.len()
            && expected.iter().all(|name| {
                nodes
                    .iter()
                    .any(|n| &n.name == name && n.status == "running")
            });
        if !intact || farm.status == FarmStatus::Deleted {
            farm.status = FarmStatus::Broken;
            farm.error = Some("VM inventory differs from the expected running nodes".into());
        } else if farm.status == FarmStatus::Broken {
            farm.status = FarmStatus::Created;
            farm.error = None;
        }
        farm.observation_error = None;
        farm.last_seen_at = Some(now());
        farm.nodes = nodes;
        farm.updated_at = now();
    }
    if host_id.is_some() {
        return Ok(());
    }
    for (id, nodes) in observed {
        // Foreign labels are not valid local paths and cannot be safely managed.
        if validate_id(&id).is_err() || state.farms.contains_key(&id) {
            continue;
        }
        let single = nodes.len() == 1
            && nodes[0].role == Some(Role::AllInOne)
            && nodes[0].name == format!("{id}-all-in-one");
        let legacy = (5..=7).contains(&nodes.len())
            && [
                Role::Deployer,
                Role::Lb,
                Role::Infra,
                Role::Keystone,
                Role::Glance,
            ]
            .iter()
            .all(|r| {
                nodes
                    .iter()
                    .any(|n| n.role == Some(*r) && n.status == "running")
            })
            && nodes
                .iter()
                .all(|node| node.role.is_some_and(|role| role != Role::AllInOne));
        let intact = (single || legacy) && nodes.iter().all(|node| node.status == "running");
        state.farms.insert(
            id.clone(),
            FarmRecord {
                id,
                host_id: None,
                remote_generation: None,
                agent_sha256: None,
                observation_error: None,
                last_seen_at: Some(now()),
                status: if intact {
                    FarmStatus::Created
                } else {
                    FarmStatus::Broken
                },
                plan: None,
                exposure: None,
                owner: None,
                expires_at: None,
                nodes,
                phase: None,
                step: None,
                error: None,
                active_job: None,
                created_at: now(),
                updated_at: now(),
                snapshot_id: None,
            },
        );
    }
    Ok(())
}

fn ensure_idle(farm: &FarmRecord) -> Result<()> {
    if farm.active_job.is_some() {
        Err(conflict("farm already has an active job"))
    } else {
        Ok(())
    }
}
fn new_job(farm: &str, kind: JobKind) -> Job {
    Job {
        id: uuid::Uuid::new_v4().to_string(),
        farm_id: farm.into(),
        kind,
        status: JobStatus::Queued,
        phase: None,
        error: None,
        created_at: now(),
        started_at: None,
        finished_at: None,
        snapshot_id: None,
    }
}
fn farm_components(farm: &FarmRecord) -> Vec<String> {
    farm.plan.as_ref().map_or_else(
        || {
            crate::model::BASE_COMPONENTS
                .iter()
                .map(|c| (*c).into())
                .collect()
        },
        |plan| plan.components.clone(),
    )
}
/// Whether two `block`-wide port ranges around `a` and `b` share any port.
fn blocks_overlap(a: u16, b: u16, block: u16) -> bool {
    a.abs_diff(b) < block
}

fn allocate_ports(
    state: &State,
    host: String,
    bind: IpAddr,
    host_id: Option<&str>,
) -> Result<Exposure> {
    let block = PUBLIC_PORT_BLOCK;
    for base in (15000u16..24000).step_by(usize::from(block)) {
        // Compare blocks, not just bases, so a widened block cannot overlap an older farm's ports.
        if state.farms.values().any(|f| {
            f.status != FarmStatus::Deleted
                && f.host_id.as_deref() == host_id
                && f.exposure
                    .as_ref()
                    .is_some_and(|e| blocks_overlap(e.port_base, base, block))
        }) {
            continue;
        }
        // Check host sockets as well as durable reservations; actual binding remains the SDK's job.
        let listeners: std::io::Result<Vec<_>> = (base..if host_id.is_some() {
            base
        } else {
            base + block
        })
            .map(|p| TcpListener::bind((bind, p)))
            .collect();
        if host_id.is_some() || listeners.is_ok() {
            return Ok(Exposure {
                host,
                bind,
                port_base: base,
            });
        }
    }
    Err(conflict("no free public TCP port blocks"))
}

#[cfg(test)]
pub(crate) mod tests;
