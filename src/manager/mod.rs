mod backend;
mod buckets;
pub(crate) mod hosts;
mod logs;
mod snapshots;
mod store;

use crate::{
    farm::Role,
    kolla::{DeployEvent, EventSink},
    model::*,
    remote::HostKind,
};
use anyhow::{Context, Result, anyhow, bail};
pub use backend::{Backend, MicrosandboxBackend};
use std::{
    net::{IpAddr, Ipv4Addr, TcpListener},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};
use store::{State, Store};
use tokio::sync::Semaphore;

#[derive(Clone)]
pub struct ManagerOptions {
    pub state_dir: PathBuf,
    pub public_bind: IpAddr,
    pub memory_budget_mib: u64,
    pub agent_binary: Option<PathBuf>,
    /// Release bucket to fetch this build's agents from, for platforms no local artifact covers.
    pub agent_url: Option<String>,
}
impl ManagerOptions {
    pub fn local(state_dir: PathBuf) -> Self {
        let sys = sysinfo::System::new_all();
        Self {
            state_dir,
            public_bind: IpAddr::V4(Ipv4Addr::LOCALHOST),
            memory_budget_mib: sys.total_memory() / (1024 * 1024) * 80 / 100,
            agent_binary: std::env::var_os("FARM_AGENT_PATH").map(PathBuf::from),
            // An empty FARM_AGENT_URL turns published agents off.
            agent_url: match std::env::var("FARM_AGENT_URL") {
                Ok(url) => Some(url).filter(|url| !url.trim().is_empty()),
                Err(_) => Some(crate::remote::artifact::RELEASE_URL.into()),
            },
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
    /// This machine's farm-agent, for a host registered as local.
    local_link: Arc<dyn crate::remote::link::AgentLink>,
    /// Sockets of hosts whose agents dial in.
    hub: Arc<crate::remote::hub::Hub>,
    options: ManagerOptions,
    create_gate: Semaphore,
    deploy_gate: Semaphore,
    /// Serializes capture, export and import, which each move the whole VM disk.
    snapshot_gate: Semaphore,
    accepting: AtomicBool,
    vault: crate::remote::Vault,
    host_gate: Semaphore,
    /// The Linux farm-agent the local host's multi-node guests run, where this binary is not one.
    guest_agent: std::sync::RwLock<Option<PathBuf>>,
    /// Agent binaries loaded at startup, by platform.
    artifacts: std::collections::BTreeMap<String, Arc<crate::remote::artifact::Artifact>>,
    /// This build's released agents for the other platforms, by platform.
    published: std::sync::RwLock<
        std::collections::BTreeMap<String, Arc<crate::remote::artifact::Published>>,
    >,
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
/// A deploy whose host-side job this coordinator lost; it may still be running there.
#[derive(Debug)]
struct Detached(String);
impl std::fmt::Display for Detached {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for Detached {}
/// How long a deploy keeps trying to reach its host-side job again before giving it up.
const REATTACH_FOR: Duration = Duration::from_secs(if cfg!(test) { 2 } else { 600 });
const REATTACHING: &str = "Coordinator restarted; re-attaching to the running deploy";

impl FarmManager {
    pub async fn open(options: ManagerOptions) -> Result<Self> {
        let link = crate::remote::link::ProcessLink::current()?;
        let guest_agent = link.guest_agent.clone();
        let manager = Self::with_link(options, Arc::new(link)).await?;
        if let Some(path) = manager.write_guest_agent()? {
            let _ = guest_agent.set(path);
        }
        Ok(manager)
    }
    /// Off Linux, the local agent cannot hand its own binary to the guests of multi-node
    /// farms; give it the loaded Linux agent for this architecture instead.
    fn write_guest_agent(&self) -> Result<Option<PathBuf>> {
        if cfg!(target_os = "linux") {
            return Ok(None);
        }
        let platform = format!("linux-{}", crate::model::current_arch());
        let Some(artifact) = self.0.artifacts.get(&platform) else {
            return Ok(None);
        };
        let dir = self.0.options.state_dir.join("guest");
        std::fs::create_dir_all(&dir)?;
        let path = dir.join(format!("farm-agent-{platform}"));
        if std::fs::read(&path).ok().as_deref() != Some(&artifact.bytes[..]) {
            let staging = dir.join(format!(".farm-agent-{}", uuid::Uuid::new_v4()));
            std::fs::write(&staging, &artifact.bytes[..])?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&staging, std::fs::Permissions::from_mode(0o755))?;
            }
            std::fs::rename(&staging, &path)?;
        }
        *self
            .0
            .guest_agent
            .write()
            .map_err(|_| anyhow!("guest agent lock poisoned"))? = Some(path.clone());
        Ok(Some(path))
    }
    /// Runs the local host's agent in-process on `backend`, so tests need no VMs.
    pub async fn with_backend(options: ManagerOptions, backend: Arc<dyn Backend>) -> Result<Self> {
        let agent =
            crate::remote::agent::Agent::in_process(options.state_dir.join("agent"), backend);
        Self::with_link(
            options,
            Arc::new(crate::remote::link::InProcessLink {
                agent: Arc::new(agent),
            }),
        )
        .await
    }
    async fn with_link(
        options: ManagerOptions,
        local_link: Arc<dyn crate::remote::link::AgentLink>,
    ) -> Result<Self> {
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
            agent_paths.extend(crate::remote::artifact::binaries_in(&agent_dir));
        }
        let artifacts = crate::remote::artifact::load_all(&agent_paths)?
            .into_iter()
            .map(|(platform, artifact)| (platform, Arc::new(artifact)))
            .collect();
        let store = Store::open(&options.state_dir)?;
        let vault = crate::remote::Vault::open(
            &options.state_dir,
            store.state.hosts.values().any(|h| h.credentials.is_some())
                || !store.state.buckets.is_empty(),
        )?;
        for host in store.state.hosts.values() {
            // A missing/wrong key must never silently discard or replace existing credentials.
            if let Some(credentials) = &host.credentials {
                vault.unseal::<crate::remote::Credentials>(credentials, &host.aad())?;
            }
        }
        for stored in store.state.buckets.values() {
            vault.unseal::<crate::s3::BucketSecret>(&stored.secret, &stored.aad())?;
        }
        // Archives of transfers a stopped daemon left; holding the state lock, none is running.
        let _ = std::fs::remove_dir_all(options.state_dir.join("transfers"));
        let logs = logs::Logs::new(&options.state_dir)?;
        let manager = Self(Arc::new(Inner {
            store: Mutex::new(store),
            logs: Mutex::new(logs),
            local_link,
            hub: Arc::default(),
            options,
            create_gate: Semaphore::new(1),
            deploy_gate: Semaphore::new(1),
            snapshot_gate: Semaphore::new(1),
            accepting: AtomicBool::new(true),
            vault,
            host_gate: Semaphore::new(2),
            guest_agent: Default::default(),
            artifacts,
            published: Default::default(),
        }));
        let mut resumed = Vec::new();
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
                // A deploy runs on as its host-side job; follow it again instead of giving it up.
                if job.kind == JobKind::Deploy && job.remote_job_id.is_some() {
                    job.status = JobStatus::Running;
                    if let Some(farm) = state.farms.get_mut(&job.farm_id) {
                        farm.step = Some(REATTACHING.into());
                        farm.updated_at = now();
                    }
                    resumed.push(job.clone());
                    continue;
                }
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
        for job in resumed {
            manager.log(&job.farm_id, &job.id, job.phase, format!("{REATTACHING}\n"))?;
            manager.spawn(job, None);
        }
        manager.abort_pending_uploads();
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
    /// Set the faults of a multi-node farm VM's links (`node` is `control`, `compute-1`, ...),
    /// such as loss 1.0 to cut it off; `Impair::default()` heals it. It applies at once,
    /// whatever job the farm runs.
    pub async fn impair(
        &self,
        id: &str,
        node: &str,
        impair: crate::link::Impair,
    ) -> Result<FarmRecord> {
        impair.validate().map_err(|e| bad_request(e.to_string()))?;
        let farm = self.get(id)?;
        if matches!(
            farm.status,
            FarmStatus::Creating | FarmStatus::Deleting | FarmStatus::Deleted
        ) {
            return Err(conflict("faults apply to a created farm"));
        }
        let plan = farm
            .plan
            .as_ref()
            .filter(|plan| plan.computes > 0)
            .ok_or_else(|| conflict("only multi-node farms have links to fault"))?;
        if !plan
            .nodes
            .iter()
            .any(|spec| crate::farm::suffix(spec.role, spec.index).as_deref() == Some(node))
        {
            return Err(NotFound(format!("VM '{node}' of farm '{id}'")).into());
        }
        self.backend_for(&farm)?
            .impair(id.into(), node.into(), impair)
            .await?;
        self.update(|state| {
            let farm = state
                .farms
                .get_mut(id)
                .ok_or_else(|| NotFound(format!("farm '{id}'")))?;
            if impair == crate::link::Impair::default() {
                farm.faults.remove(node);
            } else {
                farm.faults.insert(node.into(), impair);
            }
            farm.updated_at = now();
            Ok(farm.clone())
        })
    }
    /// Fails unless the farm's VM can take a shell session. `node` picks a multi-node VM by
    /// suffix (`control`, `compute-1`); without it the shell opens on the all-in-one or control VM.
    pub fn check_terminal(&self, id: &str, node: Option<&str>) -> Result<FarmRecord> {
        let farm = self.get(id)?;
        let vm = match node {
            None => farm
                .nodes
                .iter()
                .find(|vm| matches!(vm.role, Some(Role::AllInOne | Role::Control))),
            Some(node) => Some(
                farm.nodes
                    .iter()
                    .find(|vm| {
                        vm.role
                            .and_then(|role| crate::farm::suffix(role, vm.index))
                            .as_deref()
                            == Some(node)
                    })
                    .ok_or_else(|| NotFound(format!("VM '{node}' of farm '{id}'")))?,
            ),
        };
        if !vm.is_some_and(|vm| vm.status == "running") {
            return Err(conflict("the shell needs the farm's VM to be running"));
        }
        Ok(farm)
    }
    /// Interactive shell in the farm's VM; it does not wait for or block the farm's jobs.
    pub async fn terminal(
        &self,
        id: &str,
        node: Option<&str>,
        size: crate::farm::TerminalSize,
        io: crate::farm::TerminalIo,
    ) -> Result<()> {
        let farm = self.check_terminal(id, node)?;
        // A shell needs only the host's reservation, not the agent the farm was built
        // with, so a remote farm uses the host's current agent once that matches this coordinator.
        let current = farm.host_id.as_ref().and_then(|host_id| {
            self.0
                .store
                .lock()
                .ok()?
                .state
                .hosts
                .get(host_id)?
                .host
                .agent
                .as_ref()
                .filter(|agent| agent.artifact.build_id == crate::BUILD_ID)
                .map(|agent| agent.artifact.sha256.clone())
        });
        let backend = match current {
            Some(digest) => self.agent_backend(
                farm.host_id.as_deref().context("farm has no host")?,
                Some(&digest),
                farm.remote_generation
                    .as_deref()
                    .context("farm has no generation")?,
            )?,
            None => self.backend_for(&farm)?,
        };
        backend
            .terminal(id.into(), node.map(Into::into), size, io)
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
    /// Loaded agents first, then the published ones for the remaining platforms.
    pub fn agent_artifacts(&self) -> Vec<crate::remote::AgentArtifactInfo> {
        let mut infos: Vec<_> = self
            .0
            .artifacts
            .values()
            .map(|artifact| artifact.info.clone())
            .collect();
        if let Ok(published) = self.0.published.read() {
            infos.extend(published.values().map(|agent| agent.info.clone()));
        }
        infos
    }
    /// Looks up this build's released agents in the release bucket, for every platform no
    /// loaded artifact of this build covers, and returns how many it found. A build that was never
    /// released has none. Hosts that need one download it on first use.
    pub async fn load_published_agents(&self) -> Result<usize> {
        let Some(url) = &self.0.options.agent_url else {
            return Ok(0);
        };
        let found: std::collections::BTreeMap<_, _> = crate::remote::artifact::fetch_published(url)
            .await?
            .into_iter()
            .filter(|(platform, _)| self.loaded_current(platform).is_none())
            .map(|(platform, agent)| (platform, Arc::new(agent)))
            .collect();
        let count = found.len();
        *self
            .0
            .published
            .write()
            .map_err(|_| anyhow!("agent mutex poisoned"))? = found;
        Ok(count)
    }
    /// Whether an agent of this build can be installed on a `platform` host.
    pub fn has_current_agent(&self, platform: &str) -> bool {
        self.loaded_current(platform).is_some() || self.published_agent(platform).is_some()
    }
    fn loaded_current(&self, platform: &str) -> Option<&Arc<crate::remote::artifact::Artifact>> {
        self.0
            .artifacts
            .get(platform)
            .filter(|artifact| artifact.info.build_id == crate::BUILD_ID)
    }
    fn published_agent(&self, platform: &str) -> Option<Arc<crate::remote::artifact::Published>> {
        self.0.published.read().ok()?.get(platform).cloned()
    }
    /// The agent for a `platform` host: a loaded one of this build, else this build's
    /// published one, downloaded into the state directory the first time. A loaded agent of
    /// another build is returned last, for the caller to refuse with the rebuild command.
    pub async fn artifact_for(
        &self,
        platform: &str,
    ) -> Result<Arc<crate::remote::artifact::Artifact>> {
        if let Some(artifact) = self.loaded_current(platform) {
            return Ok(artifact.clone());
        }
        if let Some(published) = self.published_agent(platform) {
            return published.get(&self.state_dir().join("agents")).await;
        }
        self.0.artifacts.get(platform).cloned().ok_or_else(|| {
            anyhow!(
                "no {platform} farm-agent is loaded and build {} published none; build it with scripts/build-agent.sh and restart the coordinator",
                crate::BUILD_ID
            )
        })
    }
    fn backend_for(&self, farm: &FarmRecord) -> Result<Arc<dyn Backend>> {
        Ok(Arc::new(self.agent_for(farm)?))
    }
    fn agent_for(&self, farm: &FarmRecord) -> Result<crate::remote::backend::AgentBackend> {
        let host_id = farm.host_id.as_deref().ok_or_else(|| {
            conflict("this farm predates host registration; it has no host to run on")
        })?;
        self.agent(
            host_id,
            farm.agent_sha256.as_deref(),
            farm.remote_generation
                .as_deref()
                .context("farm has no generation")?,
        )
    }
    fn agent_backend(
        &self,
        host_id: &str,
        digest: Option<&str>,
        generation: &str,
    ) -> Result<Arc<dyn Backend>> {
        Ok(Arc::new(self.agent(host_id, digest, generation)?))
    }
    /// The host's farm-agent, acting for one farm generation. A remote host runs the agent
    /// artifact `digest`; the local host runs this coordinator's own binary.
    fn agent(
        &self,
        host_id: &str,
        digest: Option<&str>,
        generation: &str,
    ) -> Result<crate::remote::backend::AgentBackend> {
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
        let link: Arc<dyn crate::remote::link::AgentLink> = match stored.host.kind {
            HostKind::Local => self.0.local_link.clone(),
            HostKind::Agent => Arc::new(crate::remote::hub::HubLink {
                hub: self.0.hub.clone(),
                host: host_id.into(),
            }),
            HostKind::Ssh => Arc::new(crate::remote::link::SshLink {
                host: stored.host.clone(),
                credentials: self.0.vault.unseal(
                    stored
                        .credentials
                        .as_ref()
                        .context("remote host has no credentials")?,
                    &stored.aad(),
                )?,
                digest: digest.context("remote farm has no pinned agent")?.into(),
            }),
        };
        Ok(crate::remote::backend::AgentBackend {
            link,
            coordinator_id: store.state.coordinator_id.clone(),
            generation: generation.into(),
        })
    }
    /// This machine's agent, which keeps the snapshots. Requires a registered local host.
    fn local_backend(&self) -> Result<Arc<dyn Backend>> {
        let local = self
            .0
            .store
            .lock()
            .map_err(|_| anyhow!("state mutex poisoned"))?
            .state
            .hosts
            .values()
            .find(|stored| stored.host.kind == HostKind::Local)
            .map(|stored| stored.host.id.clone())
            .ok_or_else(|| {
                conflict("register the local host under Hosts to keep snapshots on it")
            })?;
        self.agent_backend(&local, None, "")
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
            let host_id = request.host_id.clone().ok_or_else(|| {
                bad_request("choose a host for the farm; register the local host or a server under Hosts first")
            })?;
            let host = &state
                .hosts
                .get(&host_id)
                .ok_or_else(|| NotFound(format!("host '{host_id}'")))?
                .host;
            if !matches!(host.status, crate::remote::HostStatus::Ready) || host.active_job.is_some() {
                return Err(conflict("the host must finish preparation before creating farms"));
            }
            if request.snapshot_id.is_some() && host.kind != HostKind::Local {
                return Err(conflict("snapshots restore on the local host only"));
            }
            let inspection = host.inspection.as_ref().context("ready host has no inspection")?;
            // A multi-node farm's guests run a Linux farm-agent for their links. Linux agents
            // give their own; this machine gives the one loaded at startup.
            if plan.computes > 0 {
                let ready = if host.kind == HostKind::Local {
                    cfg!(target_os = "linux")
                        || std::env::var_os("FARM_GUEST_AGENT").is_some()
                        || self.0.guest_agent.read().is_ok_and(|g| g.is_some())
                } else {
                    inspection.os == "linux"
                };
                if !ready {
                    return Err(conflict(if host.kind == HostKind::Local {
                        format!("multi-node farms need a linux-{} farm-agent for their guests; build it with `task agent-build` and restart the coordinator", crate::model::current_arch())
                    } else {
                        "multi-node farms run on Linux hosts and this machine's local host only".to_string()
                    }));
                }
            }
            let (budget, agent_sha256, bind) = match host.kind {
                HostKind::Local => (self.0.options.memory_budget_mib, None, self.0.options.public_bind),
                HostKind::Agent => {
                    if !self.0.hub.connected(&host_id) {
                        return Err(conflict(format!("the agent of host '{host_id}' is not connected; run `farm agent connect` there")));
                    }
                    (inspection.memory_total_mib * 80 / 100, None, IpAddr::V4(Ipv4Addr::UNSPECIFIED))
                }
                HostKind::Ssh => {
                    let agent = host.agent.as_ref().ok_or_else(|| conflict("install the Farm execution agent on this host first"))?;
                    if !agent.artifact.supports_single_vm { return Err(conflict("install the updated Farm execution agent to create single-VM farms")); }
                    if agent.artifact.build_id != crate::BUILD_ID {
                        return Err(conflict(format!(
                            "the agent installed on this host was built from {} but this coordinator is {}; rebuild it with `task agent-build` and install the agent again",
                            agent.artifact.build_id, crate::BUILD_ID
                        )));
                    }
                    (inspection.memory_total_mib * 80 / 100, Some(agent.artifact.sha256.clone()), IpAddr::V4(Ipv4Addr::UNSPECIFIED))
                }
            };
            let local = host.kind == HostKind::Local;
            let reserved: u64 = state
                .farms
                .values()
                .filter(|f| f.status != FarmStatus::Deleted && f.host_id.as_deref() == Some(host_id.as_str()))
                .map(FarmRecord::memory_mib)
                .sum();
            if reserved.saturating_add(plan.memory_mib) > budget {
                return Err(conflict(format!(
                    "memory budget exceeded: {reserved} MiB reserved + {} MiB requested > {} MiB",
                    plan.memory_mib, budget
                )));
            }
            let exposure = allocate_ports(state, request.external_host, bind, &host_id, local)?;
            let remote_generation = Some(uuid::Uuid::new_v4().to_string());
            let mut job = new_job(&id, if request.snapshot_id.is_some() { JobKind::Restore } else { JobKind::Create });
            job.snapshot_id = request.snapshot_id.clone();
            state.farms.insert(
                id.clone(),
                FarmRecord {
                    id: id.clone(),
                    host_id: Some(host_id),
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
                    faults: Default::default(),
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
            let layout = farm.plan.as_ref().map(|plan| crate::farm::layout(&plan.nodes));
            if !matches!(layout, Some(Ok(_))) {
                return Err(conflict("legacy multi-node farms cannot be deployed; keep or delete this farm and create a new farm"));
            }
            let expected = farm.plan.as_ref().map_or(1, |plan| plan.nodes.len());
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
        self.delete_farm(id, false, false)
    }
    /// Removes the farm's record without asking its host, for a host that is gone or
    /// unreachable. A VM it left on the host stays until the host is purged.
    pub fn forget(&self, id: &str) -> Result<Job> {
        self.delete_farm(id, false, true)
    }

    /// `own_host_job` lets a purge delete the farms of the host whose job it already holds.
    pub(crate) fn delete_farm(&self, id: &str, own_host_job: bool, forget: bool) -> Result<Job> {
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
            let mut job = new_job(id, JobKind::Delete);
            job.forget = forget;
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
            // Refresh partial VM results even if creation or deletion failed. A forgotten
            // farm's host is not asked; it may be the reason the farm is being forgotten.
            if !job.forget
                && let Ok(backend) = manager
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
            j.started_at.get_or_insert_with(now);
            Ok(())
        })?;
        if job.remote_job_id.is_none() {
            self.log(
                &job.farm_id,
                &job.id,
                None,
                format!("{:?} started", job.kind),
            )?;
        }
        let farm = self.get(&job.farm_id)?;
        match job.kind {
            JobKind::Create => {
                // An SSH host's inspection may be old by now; look at the host again first.
                if let Some(host_id) = &farm.host_id {
                    if self.refresh_ssh_inspection(host_id, 300).await? {
                        self.log(&farm.id, &job.id, None, format!("re-inspected host {host_id} over SSH\n"))?;
                    }
                }
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
                self.deploy_job(job, &farm, request.unwrap_or_default().from)
                    .await?
            }
            JobKind::Delete if job.forget => self.log(
                &farm.id,
                &job.id,
                None,
                format!(
                    "removing this farm's record only; a VM it left on host {} stays until the host is purged\n",
                    farm.host_id.as_deref().unwrap_or("(none)")
                ),
            )?,
            // A farm from before host registration has no agent to remove its VM; drop the record.
            JobKind::Delete if farm.host_id.is_none() => self.log(
                &farm.id,
                &job.id,
                None,
                "this farm has no host; removing its record only (delete any leftover VM with msb)\n"
                    .into(),
            )?,
            JobKind::Delete => self.backend_for(&farm)?.delete(farm.id).await?,
            JobKind::Settle => {
                let components = farm_components(&farm);
                self.backend_for(&farm)?
                    .settle(farm.id, None, components, self.event_sink(job, None))
                    .await?;
            }
            JobKind::Snapshot => self.freeze_job(job, farm).await?,
            JobKind::Restore => self.restore_job(job, farm).await?,
        }
        Ok(())
    }
    /// Runs a deploy as a host-side job, which outlives this coordinator and its connection
    /// to the host. A lost connection, or a restart for a job recorded with its host-side
    /// id, follows that job again from the frames already recorded.
    async fn deploy_job(
        &self,
        job: &Job,
        farm: &FarmRecord,
        from: crate::kolla::Phase,
    ) -> Result<()> {
        use crate::remote::protocol::AgentError;
        let backend = self.agent_for(farm)?;
        let mut attach = job.remote_job_id.is_some();
        let remote = match &job.remote_job_id {
            Some(remote) => remote.clone(),
            None => {
                let remote = uuid::Uuid::new_v4().to_string();
                // Recorded before the host hears of it, so a restart knows what to follow.
                self.update(|state| {
                    state
                        .jobs
                        .get_mut(&job.id)
                        .context("job missing")?
                        .remote_job_id = Some(remote.clone());
                    Ok(())
                })?;
                remote
            }
        };
        let frames = Arc::new(AtomicU64::new(job.remote_frames));
        let sink = self.event_sink(job, Some(frames.clone()));
        let mut request_failed = None;
        let mut lost: Option<Instant> = None;
        let mut delay = Duration::from_secs(1);
        let outcome = loop {
            let before = frames.load(Ordering::SeqCst);
            let outcome = if attach {
                backend.attach(remote.clone(), before, sink.clone()).await
            } else {
                backend
                    .deploy_job(farm.id.clone(), from, remote.clone(), sink.clone())
                    .await
            };
            let error = match outcome {
                Ok(()) => break Ok(()),
                // The deploy request may never have arrived; say why it was sent again.
                Err(error) if error.is::<AgentError>() => {
                    break Err(match request_failed {
                        Some(cause) if frames.load(Ordering::SeqCst) == 0 => {
                            anyhow!("{error:#} (the deploy request failed: {cause})")
                        }
                        _ => error,
                    });
                }
                Err(error) => error,
            };
            if !attach {
                request_failed = Some(format!("{error:#}"));
            }
            attach = true;
            if lost.is_none() || frames.load(Ordering::SeqCst) != before {
                self.log(
                    &job.farm_id,
                    &job.id,
                    self.job(&job.id)?.phase,
                    format!("lost the connection to the host's deploy ({error:#}); re-attaching\n"),
                )?;
                lost = Some(Instant::now());
                delay = Duration::from_secs(1);
            }
            if lost.is_some_and(|since| since.elapsed() >= REATTACH_FOR) {
                break Err(Detached(format!(
                    "cannot reach the host's deploy job {remote} ({error:#}); it may still be running on the host, so check the farm there before retrying"
                ))
                .into());
            }
            tokio::time::sleep(delay).await;
            delay = (delay * 2).min(Duration::from_secs(30));
        };
        // Output since the last save counts too, so the finished job tells it all.
        let saved = self.update(|state| {
            state
                .jobs
                .get_mut(&job.id)
                .context("job missing")?
                .remote_frames = frames.load(Ordering::SeqCst);
            Ok(())
        });
        outcome.and(saved)
    }
    /// Records deployment phases on the farm and job, and output in the farm log. With
    /// `frames`, it also counts the host-side job's frames applied, saved with the job.
    fn event_sink(&self, job: &Job, frames: Option<Arc<AtomicU64>>) -> EventSink {
        let manager = self.clone();
        let job = job.clone();
        let saved = Mutex::new(Instant::now());
        Arc::new(move |event| {
            let count = frames
                .as_ref()
                .map(|frames| frames.fetch_add(1, Ordering::SeqCst) + 1);
            let save = |state: &mut State| {
                if let Some(count) = count {
                    state
                        .jobs
                        .get_mut(&job.id)
                        .context("job missing")?
                        .remote_frames = count;
                }
                Ok::<_, anyhow::Error>(())
            };
            let phase = match event {
                DeployEvent::Step(step) => {
                    manager.update(|state| {
                        let farm = state.farms.get_mut(&job.farm_id).context("farm missing")?;
                        farm.step = Some(step.clone());
                        farm.updated_at = now();
                        save(state)
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
                        save(state)
                    })?;
                    manager.log(&job.farm_id, &job.id, Some(phase), format!("phase {phase}"))?;
                    return Ok(());
                }
                DeployEvent::Output(message) => {
                    let phase = manager.job(&job.id)?.phase;
                    (phase, message)
                }
            };
            manager.log(&job.farm_id, &job.id, phase.0, phase.1)?;
            // Output is too frequent to save each count; a restart within this window
            // records those lines again.
            let mut saved = saved.lock().map_err(|_| anyhow!("sink mutex poisoned"))?;
            if count.is_some() && saved.elapsed() >= Duration::from_secs(2) {
                manager.update(save)?;
                *saved = Instant::now();
            }
            Ok(())
        })
    }
    fn finish(&self, job: &Job, result: Result<()>) -> Result<()> {
        let detached = result.as_ref().is_err_and(|e| e.is::<Detached>());
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
            current.status = if detached {
                JobStatus::Interrupted
            } else if error.is_some() {
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
            // The step describes work in flight; once the job ends it is stale.
            farm.step = None;
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
            // A deploy runs on as its host-side job, and the next start re-attaches to it.
            if self
                .jobs()?
                .iter()
                .all(|j| !j.status.active() || j.kind == JobKind::Deploy)
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
            // Each host is scanned through its own agent, with a farm's pinned agent on remote hosts.
            let targets: std::collections::BTreeMap<String, Option<String>> = store
                .state
                .farms
                .values()
                .filter(|f| f.status != FarmStatus::Deleted)
                .filter_map(|f| Some((f.host_id.clone()?, f.agent_sha256.clone())))
                .collect();
            (store.revision, targets)
        };
        let observations =
            futures::stream::iter(targets.into_iter().map(|(id, digest)| async move {
                let result = match self.agent_backend(&id, digest.as_deref(), "") {
                    Ok(backend) => backend.scan().await,
                    Err(error) => Err(error),
                };
                (id, result)
            }))
            .buffer_unordered(4)
            .collect::<Vec<_>>()
            .await;
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
                    Ok(observed) => apply_inventory(state, observed, &host_id),
                    Err(error) => {
                        for farm in state.farms.values_mut().filter(|f| {
                            f.host_id.as_deref() == Some(host_id.as_str())
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
/// A host's agent reports only the farms this coordinator reserved there.
fn apply_inventory(
    state: &mut State,
    mut observed: std::collections::BTreeMap<String, Vec<crate::farm::FarmNode>>,
    host_id: &str,
) {
    for farm in state
        .farms
        .values_mut()
        .filter(|f| f.host_id.as_deref() == Some(host_id))
    {
        if farm.active_job.is_some() {
            observed.remove(&farm.id);
            continue;
        }
        let nodes = observed.remove(&farm.id).unwrap_or_default();
        if farm.status == FarmStatus::Deleted && nodes.is_empty() {
            continue;
        }
        let expected: Vec<(Role, Option<u8>)> = farm
            .plan
            .as_ref()
            .map(|p| p.nodes.iter().map(|n| (n.role, n.index)).collect())
            .unwrap_or_else(|| {
                if nodes.iter().any(|node| node.role == Some(Role::AllInOne)) {
                    return vec![(Role::AllInOne, None)];
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
                roles.into_iter().map(|role| (role, None)).collect()
            });
        let intact = nodes.len() == expected.len()
            && expected.iter().all(|(role, index)| {
                nodes.iter().any(|n| {
                    crate::farm::is_node_name(&farm.id, *role, *index, &n.name)
                        && n.status == "running"
                })
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
        forget: false,
        remote_job_id: None,
        remote_frames: 0,
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

/// `probe` checks the block against this machine's sockets too, for farms published here.
fn allocate_ports(
    state: &State,
    host: String,
    bind: IpAddr,
    host_id: &str,
    probe: bool,
) -> Result<Exposure> {
    let block = PUBLIC_PORT_BLOCK;
    for base in (15000u16..24000).step_by(usize::from(block)) {
        // Compare blocks, not just bases, so a widened block cannot overlap an older farm's ports.
        if state.farms.values().any(|f| {
            f.status != FarmStatus::Deleted
                && f.host_id.as_deref() == Some(host_id)
                && f.exposure
                    .as_ref()
                    .is_some_and(|e| blocks_overlap(e.port_base, base, block))
        }) {
            continue;
        }
        // Check host sockets as well as durable reservations; actual binding remains the SDK's job.
        if !probe || (base..base + block).all(|p| TcpListener::bind((bind, p)).is_ok()) {
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
