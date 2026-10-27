//! One SSH request per process; no listener and no SSH credentials on the target.
use super::protocol::{Frame, Operation, Reply, Request, VERSION};
use crate::{
    manager::{Backend, MicrosandboxBackend},
    model::{Exposure, Plan, PlanRequest, plan, validate_id},
};
use anyhow::{Context, Result, bail, ensure};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::Write,
    net::TcpListener,
    path::{Path, PathBuf},
    sync::Arc,
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

pub struct Limits {
    pub memory_budget_mib: u64,
    pub available_mib: u64,
    pub virtualization: bool,
}
impl Limits {
    pub fn system() -> Self {
        let system = sysinfo::System::new_all();
        Self {
            memory_budget_mib: system.total_memory() / 1024 / 1024 * 80 / 100,
            available_mib: system.available_memory() / 1024 / 1024,
            virtualization: cfg!(target_os = "linux")
                && OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open("/dev/kvm")
                    .is_ok(),
        }
    }
}
pub struct Agent {
    dir: PathBuf,
    backend: Arc<dyn Backend>,
    limits: Limits,
}
impl Agent {
    pub fn local() -> Self {
        let home = std::env::var_os("HOME").unwrap_or_else(|| ".".into());
        Self {
            dir: PathBuf::from(home).join(".local/share/farm/worker-v1"),
            backend: Arc::new(MicrosandboxBackend),
            limits: Limits::system(),
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

    pub async fn execute(&self, request: Request, emit: Emitter) -> Result<Reply> {
        ensure!(
            request.version == VERSION,
            "incompatible farm-agent protocol"
        );
        uuid::Uuid::parse_str(&request.coordinator_id).context("invalid coordinator identity")?;
        if let Some((id, generation)) = request.operation.scope() {
            validate_id(id)?;
            uuid::Uuid::parse_str(generation).context("invalid farm generation")?;
        }
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
        // A host-wide cleanup has no farm id, so it runs before the per-farm scope below.
        if matches!(request.operation, Operation::Purge) {
            // Serialize with creates so a reservation cannot appear mid-purge.
            let _create_lock = lock(&self.dir, "create.lock")?;
            return self.purge(&request).await;
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
                ensure!(
                    self.limits.virtualization,
                    "remote host has no accessible /dev/kvm"
                );
                let canonical = plan(&PlanRequest {
                    components: requested.components.clone(),
                })?;
                ensure!(
                    serde_json::to_value(requested)? == serde_json::to_value(&canonical)?,
                    "farm plan differs from the supported layout"
                );
                ensure!(
                    exposure.port_base >= 15000
                        && exposure.port_base <= 23990
                        && exposure.port_base.is_multiple_of(10)
                        && exposure.bind == std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED),
                    "remote exposure requires 0.0.0.0 and a valid port block"
                );
                // Reuse public-host validation; the worker accepts no shell or filesystem paths.
                crate::model::CreateRequest {
                    id: id.into(),
                    host_id: None,
                    components: requested.components.clone(),
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
                        return Ok(Reply::Nodes { nodes });
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
                    .map(|p| TcpListener::bind((exposure.bind, p)))
                    .collect::<std::io::Result<Vec<_>>>()
                    .context("remote public port block is occupied")?;
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
                emit(Frame::Output {
                    message: "Remote capacity and public ports reserved; creating farm VMs\n"
                        .into(),
                })?;
                Ok(Reply::Nodes {
                    nodes: self
                        .backend
                        .create(id.into(), requested.clone(), exposure.clone())
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
            Operation::Deploy { .. } | Operation::Endpoints { .. } => {
                let registry = LockedRegistry::open(&self.dir)?;
                let entry = registry
                    .owned(&request.coordinator_id, id, generation)?
                    .clone();
                ensure!(!entry.deleted, "remote farm was deleted");
                drop(registry);
                if let Operation::Deploy { from, .. } = &request.operation {
                    ensure!(
                        entry.plan.nodes.len() == 1
                            && entry.plan.nodes[0].role == crate::farm::Role::AllInOne,
                        "legacy multi-node farm cannot be deployed by the single-VM engine; create a new farm"
                    );
                    let _deploy_lock = lock(&self.dir, "deploy.lock")?;
                    let sink = Arc::new(move |event| {
                        emit(match event {
                            crate::kolla::DeployEvent::Phase(phase) => Frame::Phase { phase },
                            crate::kolla::DeployEvent::Step(step) => Frame::Step { step },
                            crate::kolla::DeployEvent::Output(message) => Frame::Output { message },
                        })
                    });
                    self.backend
                        .deploy(
                            id.into(),
                            *from,
                            Some(entry.exposure),
                            entry.plan.components.clone(),
                            sink,
                        )
                        .await?;
                    Ok(Reply::Done)
                } else {
                    Ok(Reply::Endpoints {
                        endpoints: self
                            .backend
                            .endpoints(id.into(), Some(entry.exposure), entry.plan.components)
                            .await?,
                    })
                }
            }
            _ => bail!("unsupported operation"),
        }
    }
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

/// Build id written into an ELF artifact, or `None` when it predates the marker.
pub(crate) fn artifact_build_id(bytes: &[u8]) -> Option<String> {
    let prefix = BUILD_STAMP_PREFIX.as_bytes();
    let start = bytes
        .windows(prefix.len())
        .position(|window| window == prefix)?;
    let rest = &bytes[start + prefix.len()..];
    let end = rest.iter().position(|byte| *byte == 0)?;
    let id = std::str::from_utf8(&rest[..end]).ok()?;
    (!id.is_empty() && id.len() <= 40).then(|| id.to_owned())
}

pub async fn serve_stdio() -> Result<()> {
    use std::io::Read;
    let mut input = String::new();
    std::io::stdin()
        .take(128 * 1024 + 1)
        .read_to_string(&mut input)?;
    ensure!(input.len() <= 128 * 1024, "agent request too large");
    let emit = stdout_emitter();
    let result = match serde_json::from_str::<Request>(&input) {
        Ok(request) => Agent::local().execute(request, emit.clone()).await,
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
            plan: plan(&PlanRequest::default()).unwrap(),
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
            },
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
                plan(&PlanRequest::default()).unwrap(),
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
            },
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
}
