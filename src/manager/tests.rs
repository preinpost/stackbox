use super::*;
use crate::{
    api,
    farm::FarmNode,
    kolla::{DeployEvent, Phase},
    model,
};
use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use futures::future::BoxFuture;
use std::{collections::BTreeMap, sync::atomic::AtomicUsize, time::Duration};
use tower::ServiceExt;

/// One expansion the fake backend was asked to create.
type Expansion = (String, Vec<String>, Vec<String>);

#[derive(Default)]
pub(crate) struct Fake {
    pub(crate) fail_create: AtomicBool,
    pub(crate) farms: Mutex<BTreeMap<String, Vec<FarmNode>>>,
    /// msb snapshot references and their labels.
    snapshots: Mutex<BTreeMap<String, BTreeMap<String, String>>>,
    /// `(from, to)` of each settle call.
    settled: Mutex<Vec<Option<(Exposure, Exposure)>>>,
    deploys: AtomicUsize,
    max_deploys: AtomicUsize,
    /// Deploys started, finished or not.
    pub(crate) deployed: AtomicUsize,
    pub(crate) fail: AtomicBool,
    pub(crate) deployment_gate: Option<Semaphore>,
    scan_delay: Mutex<Option<(Arc<tokio::sync::Notify>, Arc<tokio::sync::Notify>)>>,
    /// `(farm, node, impair)` of each fault set on a link.
    faults: Mutex<Vec<(String, String, crate::link::Impair)>>,
    /// Shares created here and the remote computes of each spanning deploy.
    shares: Mutex<Vec<crate::remote::protocol::Share>>,
    relinked: Mutex<Vec<(String, String)>>,
    spanning: Mutex<Vec<Vec<u8>>>,
    /// `(farm, nodes on this host, added suffixes)` of each expansion.
    expanded: Mutex<Vec<Expansion>>,
    /// `(farm, index)` of each Kolla compute deployment.
    computed: Mutex<Vec<(String, u8)>>,
    /// `(farm, index)` of each compute booted from a template that Kolla reconfigured.
    reconfigured: Mutex<Vec<(String, u8)>>,
    /// The compute template each expansion that created a VM booted it from.
    expanded_from: Mutex<Vec<Option<String>>>,
    pub(crate) fail_expand: AtomicBool,
    /// Fail the expansion that would create this many VMs (1 is the first), and every
    /// later one; 0 never fails.
    pub(crate) fail_expansion_from: AtomicUsize,
    pub(crate) fail_compute: AtomicBool,
    /// Farms restored from a control template that forgot their source's computes.
    forgotten: Mutex<Vec<String>>,
    pub(crate) fail_forget: AtomicBool,
}
impl Backend for Fake {
    fn relink(&self, id: String, url: String) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            self.relinked.lock().unwrap().push((id, url));
            Ok(())
        })
    }
    fn create_share(
        &self,
        id: String,
        mut plan: Plan,
        exposure: Exposure,
        share: crate::remote::protocol::Share,
    ) -> BoxFuture<'_, Result<Vec<FarmNode>>> {
        Box::pin(async move {
            plan.nodes.retain(|spec| {
                crate::farm::suffix(spec.role, spec.index).is_some_and(|s| share.nodes.contains(&s))
            });
            self.shares.lock().unwrap().push(share);
            self.create(id, plan, exposure).await
        })
    }
    fn expand(
        &self,
        id: String,
        plan: Plan,
        nodes: Vec<String>,
        add: Vec<String>,
        _uplink: Option<crate::link::UplinkConfig>,
        source: Option<String>,
    ) -> BoxFuture<'_, Result<Vec<FarmNode>>> {
        Box::pin(async move {
            if self.fail_expand.load(Ordering::SeqCst) {
                bail!("intentional expand failure");
            }
            let from = self.fail_expansion_from.load(Ordering::SeqCst);
            if from > 0 && !add.is_empty() && self.expanded_from.lock().unwrap().len() + 1 >= from {
                bail!("intentional expand failure");
            }
            if let Some(source) = &source {
                assert!(self.snapshots.lock().unwrap().contains_key(source));
            }
            if !add.is_empty() {
                self.expanded_from.lock().unwrap().push(source);
            }
            self.expanded
                .lock()
                .unwrap()
                .push((id.clone(), nodes, add.clone()));
            let mut farms = self.farms.lock().unwrap();
            let existing = farms.entry(id.clone()).or_default();
            let mut created = Vec::new();
            for suffix in add {
                let spec = plan
                    .nodes
                    .iter()
                    .find(|spec| {
                        crate::farm::suffix(spec.role, spec.index).as_deref()
                            == Some(suffix.as_str())
                    })
                    .with_context(|| format!("no VM {suffix} in the plan"))?;
                let name = crate::farm::node_name(&id, spec.role, spec.index);
                if let Some(node) = existing.iter().find(|node| node.name == name) {
                    created.push(node.clone());
                    continue;
                }
                let node = FarmNode {
                    name,
                    index: spec.index,
                    role: Some(spec.role),
                    status: "running".into(),
                    memory_mib: spec.memory_mib,
                    ip: Some("10.250.0.1".into()),
                };
                existing.push(node.clone());
                created.push(node);
            }
            Ok(created)
        })
    }
    fn ssh_keys(&self, _: String) -> BoxFuture<'_, Result<(String, String)>> {
        Box::pin(async {
            Ok((
                "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIFake".into(),
                "-----BEGIN OPENSSH PRIVATE KEY-----\nfake\n-----END OPENSSH PRIVATE KEY-----"
                    .into(),
            ))
        })
    }
    fn deploy_compute(
        &self,
        id: String,
        _: Plan,
        index: u8,
        _: Vec<String>,
        _: Vec<String>,
        from_template: bool,
        _: EventSink,
    ) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            if self.fail_compute.load(Ordering::SeqCst) {
                bail!("intentional compute deployment failure");
            }
            if from_template {
                self.reconfigured.lock().unwrap().push((id, index));
                return Ok(());
            }
            self.computed.lock().unwrap().push((id, index));
            Ok(())
        })
    }
    fn deploy_spanning(
        &self,
        id: String,
        from: Phase,
        exposure: Option<Exposure>,
        components: Vec<String>,
        remote: Vec<u8>,
        key: String,
        sink: EventSink,
    ) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            assert!(key.starts_with("-----BEGIN OPENSSH PRIVATE KEY-----"));
            self.spanning.lock().unwrap().push(remote);
            self.deploy(id, from, exposure, components, sink).await
        })
    }
    fn impair(
        &self,
        id: String,
        node: String,
        impair: crate::link::Impair,
    ) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            self.faults.lock().unwrap().push((id, node, impair));
            Ok(())
        })
    }
    fn scan(&self) -> BoxFuture<'_, Result<BTreeMap<String, Vec<FarmNode>>>> {
        Box::pin(async {
            let snapshot = self.farms.lock().unwrap().clone();
            let delay = self.scan_delay.lock().unwrap().take();
            if let Some((started, release)) = delay {
                started.notify_one();
                release.notified().await;
            }
            Ok(snapshot)
        })
    }
    fn create(&self, id: String, plan: Plan, _: Exposure) -> BoxFuture<'_, Result<Vec<FarmNode>>> {
        Box::pin(async move {
            if self.fail_create.load(Ordering::SeqCst) {
                bail!("intentional create failure");
            }
            let nodes = plan
                .nodes
                .iter()
                .map(|spec| FarmNode {
                    name: crate::farm::node_name(&id, spec.role, spec.index),
                    index: spec.index,
                    role: Some(spec.role),
                    status: "running".into(),
                    memory_mib: spec.memory_mib,
                    ip: Some("10.250.0.1".into()),
                })
                .collect::<Vec<_>>();
            self.farms.lock().unwrap().insert(id, nodes.clone());
            Ok(nodes)
        })
    }
    fn deploy(
        &self,
        _: String,
        from: Phase,
        _: Option<Exposure>,
        _: Vec<String>,
        sink: EventSink,
    ) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            self.deployed.fetch_add(1, Ordering::SeqCst);
            let active = self.deploys.fetch_add(1, Ordering::SeqCst) + 1;
            self.max_deploys.fetch_max(active, Ordering::SeqCst);
            sink(DeployEvent::Phase(from))?;
            sink(DeployEvent::Output("test deployment output".into()))?;
            sink(DeployEvent::Step("make images".into()))?;
            if let Some(gate) = &self.deployment_gate {
                gate.acquire().await?.forget();
            }
            self.deploys.fetch_sub(1, Ordering::SeqCst);
            if self.fail.load(Ordering::SeqCst) {
                bail!("intentional deploy failure");
            }
            Ok(())
        })
    }
    fn delete(&self, id: String) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            self.farms.lock().unwrap().remove(&id);
            Ok(())
        })
    }
    fn endpoints(
        &self,
        _: String,
        exposure: Option<Exposure>,
        components: Vec<String>,
    ) -> BoxFuture<'_, Result<Endpoints>> {
        Box::pin(async move {
            Ok(Endpoints {
                services: exposure.unwrap().urls(Some(&components)),
                credentials: BTreeMap::from([("OS_PASSWORD".into(), "test-secret".into())]),
                datastores: vec![Datastore {
                    id: "mariadb".into(),
                    label: "MariaDB".into(),
                    protocol: "mysql".into(),
                    host: "localhost".into(),
                    port: 15006,
                    username: Some("root".into()),
                    password: Some("test-secret".into()),
                    vhost: None,
                    url: "mysql://root@localhost:15006/".into(),
                    databases: vec!["keystone".into()],
                }],
            })
        })
    }
    fn freeze(
        &self,
        id: String,
        snapshot_id: String,
        node: Option<String>,
        mut labels: BTreeMap<String, String>,
        sink: EventSink,
    ) -> BoxFuture<'_, Result<crate::farm::FrozenSnapshot>> {
        Box::pin(async move {
            let step = |index, step: &str| {
                sink(DeployEvent::Progress(crate::kolla::StepProgress {
                    step: step.into(),
                    index,
                    total: 3,
                }))
            };
            step(1, "stop the VMs")?;
            step(2, "capture the VMs")?;
            if self.fail.load(Ordering::SeqCst) {
                bail!("intentional freeze failure");
            }
            step(3, "start the VMs")?;
            // A compute template is that one VM's disks.
            if let Some(node) = node {
                assert!(
                    self.farms.lock().unwrap()[&id]
                        .iter()
                        .any(|vm| vm.name == format!("{id}-{node}"))
                );
                labels.insert(crate::model::snapshot_labels::NODE.into(), node);
                let reference = format!("fake/{snapshot_id}");
                self.snapshots
                    .lock()
                    .unwrap()
                    .insert(reference.clone(), labels);
                return Ok(crate::farm::FrozenSnapshot {
                    reference,
                    size_bytes: Some(4096),
                    restart_error: None,
                    nodes: Vec::new(),
                });
            }
            let vms = self.farms.lock().unwrap()[&id].clone();
            // A multi-node farm's VMs are captured one by one, control first.
            let nodes: Vec<_> = vms
                .iter()
                .filter_map(|vm| crate::farm::suffix(vm.role?, vm.index))
                .map(|node| crate::farm::NodeSnapshot {
                    reference: format!("fake/{snapshot_id}-{node}"),
                    node,
                    size_bytes: Some(4096),
                })
                .collect();
            let mut snapshots = self.snapshots.lock().unwrap();
            for node in &nodes {
                snapshots.insert(node.reference.clone(), labels.clone());
            }
            let reference = match nodes.first() {
                Some(control) => control.reference.clone(),
                None => {
                    let reference = format!("fake/{snapshot_id}");
                    snapshots.insert(reference.clone(), labels);
                    reference
                }
            };
            Ok(crate::farm::FrozenSnapshot {
                reference,
                size_bytes: Some(4096 * nodes.len().max(1) as u64),
                restart_error: None,
                nodes,
            })
        })
    }
    fn restore(
        &self,
        id: String,
        reference: String,
        nodes: Vec<crate::farm::NodeSnapshot>,
        plan: Plan,
        exposure: Exposure,
    ) -> BoxFuture<'_, Result<Vec<FarmNode>>> {
        Box::pin(async move {
            assert!(self.snapshots.lock().unwrap().contains_key(&reference));
            for node in &nodes {
                assert!(self.snapshots.lock().unwrap().contains_key(&node.reference));
            }
            self.create(id, plan, exposure).await
        })
    }
    fn settle(
        &self,
        _: String,
        rebind: Option<(Exposure, Exposure)>,
        _: Vec<String>,
        sink: EventSink,
    ) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            if self.fail.load(Ordering::SeqCst) {
                bail!("intentional settle failure");
            }
            sink(DeployEvent::Output("services answer".into()))?;
            self.settled.lock().unwrap().push(rebind);
            Ok(())
        })
    }
    fn forget_computes(
        &self,
        id: String,
        _: Vec<String>,
        sink: EventSink,
    ) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            if self.fail_forget.load(Ordering::SeqCst) {
                bail!("intentional forget failure");
            }
            // Only the restored control VM is left to clean up.
            let farms = self.farms.lock().unwrap();
            assert!(
                farms[&id]
                    .iter()
                    .all(|vm| vm.role == Some(crate::farm::Role::Control))
            );
            sink(DeployEvent::Output(
                "the source farm's computes are forgotten".into(),
            ))?;
            self.forgotten.lock().unwrap().push(id);
            Ok(())
        })
    }
    fn export_snapshot(
        &self,
        reference: String,
        output: std::path::PathBuf,
    ) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            // An archive carries its snapshot's labels, wherever it is moved.
            let labels = self.snapshots.lock().unwrap()[&reference].clone();
            std::fs::write(&output, serde_json::to_vec(&labels)?)?;
            Ok(())
        })
    }
    fn import_snapshot(
        &self,
        archive: std::path::PathBuf,
        snapshot_id: String,
    ) -> BoxFuture<'_, Result<crate::farm::ImportedSnapshot>> {
        Box::pin(async move {
            let labels: BTreeMap<String, String> =
                serde_json::from_slice(&std::fs::read(&archive)?).unwrap_or_default();
            let reference = format!("fake/imported/{snapshot_id}");
            self.snapshots
                .lock()
                .unwrap()
                .insert(reference.clone(), labels.clone());
            Ok(crate::farm::ImportedSnapshot {
                reference,
                labels,
                size_bytes: Some(4096),
            })
        })
    }
    fn remove_snapshot(&self, reference: String) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            self.snapshots
                .lock()
                .unwrap()
                .remove(&reference)
                .context("no such snapshot")?;
            Ok(())
        })
    }
    /// Echoes input and reports resizes until the client closes its side.
    fn terminal(
        &self,
        _id: String,
        _node: Option<String>,
        size: crate::farm::TerminalSize,
        io: crate::farm::TerminalIo,
    ) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            let crate::farm::TerminalIo { mut input, output } = io;
            output
                .send(format!("{}x{}", size.rows, size.cols).into_bytes())
                .await?;
            while let Some(message) = input.recv().await {
                output
                    .send(match message {
                        crate::farm::TerminalInput::Data(data) => data,
                        crate::farm::TerminalInput::Resize(size) => {
                            format!("{}x{}", size.rows, size.cols).into_bytes()
                        }
                    })
                    .await?;
            }
            Ok(())
        })
    }
}
fn options(dir: &Path) -> ManagerOptions {
    ManagerOptions {
        state_dir: dir.into(),
        memory_budget_mib: 50_000,
        agent_binary: None,
        agent_url: None,
        public_bind: "127.0.0.1".parse().unwrap(),
        relay: None,
    }
}
/// Registers this machine as a prepared host, as its check and preparation leave it.
/// Its agent runs in-process on the test's fake backend.
fn local_host(manager: &FarmManager) {
    if manager.host("local").is_err() {
        manager
            .register_local_host(crate::remote::RegisterLocalHost { id: "local".into() })
            .unwrap();
    }
    manager
        .update(|state| {
            let host = &mut state.hosts.get_mut("local").unwrap().host;
            host.status = crate::remote::HostStatus::Ready;
            host.inspection = Some(crate::remote::HostInspection {
                os: "linux".into(),
                arch: std::env::consts::ARCH.into(),
                compatible: true,
                kvm_available: true,
                sudo_available: false,
                memory_total_mib: 65536,
                memory_available_mib: 65536,
                disk_available_mib: 500_000,
                msb_version: Some("0.7.13".into()),
                msb_compatible: true,
                docker_available: true,
                golden_image_available: true,
                vm_boot_verified: true,
                checked_at: now(),
            });
            Ok(())
        })
        .unwrap();
}
async fn open_with(options: ManagerOptions, backend: Arc<Fake>) -> FarmManager {
    let manager = FarmManager::with_backend(options, backend).await.unwrap();
    local_host(&manager);
    manager
}
fn request(id: &str) -> CreateRequest {
    CreateRequest {
        id: id.into(),
        host_id: Some("local".into()),
        components: vec![],
        computes: 0,
        compute: None,
        external_host: "localhost".into(),
        owner: Some("test".into()),
        expires_at: None,
        snapshot_id: None,
        placement: Default::default(),
    }
}
async fn done(manager: &FarmManager, job: &Job) -> Job {
    tokio::time::timeout(Duration::from_secs(5), manager.wait(&job.id))
        .await
        .unwrap()
        .unwrap()
}
async fn settle(manager: &FarmManager) {
    manager.shutdown().await.unwrap();
}

#[tokio::test]
async fn remote_farms_route_over_ssh_with_separate_budgets_ports_streams_and_offline_inventory() {
    use crate::remote::{AgentArtifactInfo, HostJobKind, InstalledAgent, test_server::Fixture};
    let fixture = Fixture::start().await;
    let dir = tempfile::tempdir().unwrap();
    let backend = Arc::new(Fake::default());
    let mut opts = options(dir.path());
    opts.memory_budget_mib = 3072;
    let manager = open_with(opts, backend.clone()).await;
    manager
        .register_host(fixture.registration("target"))
        .unwrap();
    let prep = manager
        .start_host_job("target", HostJobKind::Prepare)
        .unwrap();
    assert_eq!(
        host_done(&manager, &prep.id).await.status,
        JobStatus::Succeeded
    );
    manager
        .update(|state| {
            state.hosts.get_mut("target").unwrap().host.agent = Some(InstalledAgent {
                artifact: AgentArtifactInfo {
                    sha256: "a".repeat(64),
                    os: "linux".into(),
                    arch: std::env::consts::ARCH.into(),
                    protocol: 1,
                    supports_single_vm: true,
                    build_id: crate::BUILD_ID.into(),
                },
                verified_at: now(),
            });
            Ok(())
        })
        .unwrap();
    let local = manager.create(request("local-farm")).unwrap();
    done(&manager, &local).await;
    // Local admission probes real TCP ports, so an actual running farm can make
    // it choose a later block. Pin the fake reservation to the remote pool's
    // first block to test host isolation independently of the machine's ports.
    manager
        .update(|state| {
            state
                .farms
                .get_mut("local-farm")
                .unwrap()
                .exposure
                .as_mut()
                .unwrap()
                .port_base = 15000;
            Ok(())
        })
        .unwrap();
    let mut remote = request("remote-farm");
    remote.host_id = Some("target".into());
    remote.external_host = "target.example".into();
    manager
        .update(|state| {
            state
                .hosts
                .get_mut("target")
                .unwrap()
                .host
                .agent
                .as_mut()
                .unwrap()
                .artifact
                .supports_single_vm = false;
            Ok(())
        })
        .unwrap();
    let old_agent_error = manager.create(remote.clone()).unwrap_err();
    assert!(
        old_agent_error.is::<Conflict>()
            && old_agent_error
                .to_string()
                .contains("updated Farm execution agent")
    );
    assert!(manager.get("remote-farm").is_err());
    manager
        .update(|state| {
            state
                .hosts
                .get_mut("target")
                .unwrap()
                .host
                .agent
                .as_mut()
                .unwrap()
                .artifact
                .supports_single_vm = true;
            Ok(())
        })
        .unwrap();
    let created = manager.create(remote).unwrap();
    assert_eq!(done(&manager, &created).await.status, JobStatus::Succeeded);
    assert!(!backend.farms.lock().unwrap().contains_key("remote-farm"));
    let farm = manager.get("remote-farm").unwrap();
    assert_eq!(farm.nodes.len(), 1);
    assert_eq!(
        farm.exposure.unwrap().port_base,
        manager
            .get("local-farm")
            .unwrap()
            .exposure
            .unwrap()
            .port_base
    );
    assert_eq!(
        manager
            .host("target")
            .unwrap()
            .capacity
            .unwrap()
            .memory_reserved_mib,
        3072
    );
    assert!(
        manager
            .unregister_host("target")
            .unwrap_err()
            .is::<Conflict>()
    );
    // A host with existing farms can recover credentials without being unregistered.
    manager
        .update_host_credentials(fixture.registration("target"))
        .unwrap();
    let mut moved = fixture.registration("target");
    moved.username = "different-user".into();
    assert!(
        manager
            .update_host_credentials(moved)
            .unwrap_err()
            .is::<Conflict>()
    );
    let permit = manager.0.deploy_gate.acquire().await.unwrap();
    let job = manager
        .deploy(
            "remote-farm",
            DeployRequest {
                from: Phase::Verify,
            },
        )
        .unwrap();
    assert!(
        manager
            .start_host_job("target", HostJobKind::Check)
            .unwrap_err()
            .is::<Conflict>()
    );
    drop(permit);
    assert_eq!(done(&manager, &job).await.status, JobStatus::Succeeded);
    assert_eq!(manager.job(&job.id).unwrap().phase, Some(Phase::Verify));
    assert!(
        manager
            .logs("remote-farm", 0, 1000)
            .unwrap()
            .iter()
            .any(|entry| entry.message.contains("remote deploy streamed output"))
    );
    assert_eq!(
        manager.endpoints("remote-farm").await.unwrap().credentials["OS_PASSWORD"],
        "remote-guest-secret"
    );
    assert!(
        !std::fs::read_to_string(dir.path().join("state.json"))
            .unwrap()
            .contains("remote-guest-secret")
    );
    fixture.observed.fail_scan.store(true, Ordering::SeqCst);
    manager.reconcile().await.unwrap();
    let farm = manager.get("remote-farm").unwrap();
    assert_eq!(farm.status, FarmStatus::Ready);
    assert_eq!(farm.nodes.len(), 1);
    assert!(farm.observation_error.is_some());
    assert_eq!(
        manager
            .host("target")
            .unwrap()
            .capacity
            .unwrap()
            .memory_reserved_mib,
        3072
    );
    fixture.observed.fail_scan.store(false, Ordering::SeqCst);
    manager.reconcile().await.unwrap();
    assert!(
        manager
            .get("remote-farm")
            .unwrap()
            .observation_error
            .is_none()
    );
    let deleted = manager.delete("remote-farm").unwrap();
    assert_eq!(done(&manager, &deleted).await.status, JobStatus::Succeeded);
    assert_eq!(
        manager
            .host("target")
            .unwrap()
            .capacity
            .unwrap()
            .memory_reserved_mib,
        0
    );
    assert_eq!(
        manager.get("local-farm").unwrap().status,
        FarmStatus::Created
    );
    manager.unregister_host("target").unwrap();
    settle(&manager).await;
}

async fn host_done(manager: &FarmManager, id: &str) -> crate::remote::HostJob {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let job = manager.host_job(id).unwrap();
            if !job.status.active() {
                return job;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap()
}

#[tokio::test]
async fn shell_websocket_relays_bytes_and_resizes_and_refuses_other_origins() {
    use futures::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::{Message, client::IntoClientRequest};
    let dir = tempfile::tempdir().unwrap();
    let manager = open_with(options(dir.path()), Arc::new(Fake::default())).await;
    let job = manager.create(request("shell")).unwrap();
    done(&manager, &job).await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server =
        tokio::spawn(axum::serve(listener, api::router(manager.clone(), None)).into_future());
    let url = |farm: &str, query: &str| format!("ws://{address}/api/v1/farms/{farm}/shell?{query}");

    let (mut socket, _) = tokio_tungstenite::connect_async(url("shell", "rows=24&cols=80"))
        .await
        .unwrap();
    async fn next<S>(socket: &mut S) -> String
    where
        S: futures::Stream<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin,
    {
        match socket.next().await.unwrap().unwrap() {
            Message::Binary(data) => String::from_utf8(data.to_vec()).unwrap(),
            other => panic!("unexpected {other:?}"),
        }
    }
    assert_eq!(next(&mut socket).await, "24x80");
    socket
        .send(Message::Binary(b"ls\r".to_vec().into()))
        .await
        .unwrap();
    assert_eq!(next(&mut socket).await, "ls\r");
    socket
        .send(Message::Text(
            r#"{"type":"resize","rows":40,"cols":120}"#.into(),
        ))
        .await
        .unwrap();
    assert_eq!(next(&mut socket).await, "40x120");
    socket.close(None).await.unwrap();

    let refused = async |request| match tokio_tungstenite::connect_async(request).await {
        Err(tokio_tungstenite::tungstenite::Error::Http(response)) => response.status(),
        other => panic!("expected an HTTP refusal, got {:?}", other.map(|_| ())),
    };
    let mut foreign = url("shell", "rows=24&cols=80")
        .into_client_request()
        .unwrap();
    foreign
        .headers_mut()
        .insert("origin", "https://evil.example".parse().unwrap());
    assert_eq!(refused(foreign).await, StatusCode::FORBIDDEN);
    assert_eq!(
        refused(
            url("shell", "rows=0&cols=80")
                .into_client_request()
                .unwrap()
        )
        .await,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        refused(
            url("missing", "rows=24&cols=80")
                .into_client_request()
                .unwrap()
        )
        .await,
        StatusCode::NOT_FOUND
    );
    server.abort();
    settle(&manager).await;
}

#[tokio::test]
async fn http_remote_host_registration_validation_job_locations_and_unregistration() {
    use crate::remote::test_server::Fixture;
    let fixture = Fixture::start().await;
    let dir = tempfile::tempdir().unwrap();
    let manager = open_with(options(dir.path()), Arc::new(Fake::default())).await;
    let app = api::router(manager.clone(), None);
    let request = |method: &str, uri: &str, value: serde_json::Value| {
        Request::builder()
            .method(method)
            .uri(uri)
            .header("content-type", "application/json")
            .body(Body::from(value.to_string()))
            .unwrap()
    };
    let payload = serde_json::json!({"id":"remote","address":"127.0.0.1","port":fixture.port,"username":"operator","fingerprint":fixture.fingerprint,"credentials":{"method":"password","password":"fixture-password-only"}});
    for (key, value) in [
        ("address", serde_json::json!("host;touch /tmp/bad")),
        ("username", serde_json::json!("root;id")),
        ("fingerprint", serde_json::json!("SHA256:unverified")),
        (
            "credentials",
            serde_json::json!({"method":"private_key","private_key":"not-a-key"}),
        ),
    ] {
        let mut invalid = payload.clone();
        invalid[key] = value;
        let response = app
            .clone()
            .oneshot(request("POST", "/api/v1/hosts", invalid))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }
    let response = app
        .clone()
        .oneshot(request("POST", "/api/v1/hosts", payload.clone()))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    assert_eq!(response.headers()["location"], "/api/v1/hosts/remote");
    let body = String::from_utf8(
        to_bytes(response.into_body(), 100_000)
            .await
            .unwrap()
            .to_vec(),
    )
    .unwrap();
    assert!(!body.contains("fixture-password-only") && !body.contains("credentials"));
    let updated = app
        .clone()
        .oneshot(request("PUT", "/api/v1/hosts/remote", payload.clone()))
        .await
        .unwrap();
    assert_eq!(updated.status(), StatusCode::OK);
    assert_eq!(
        app.clone()
            .oneshot(request("POST", "/api/v1/hosts", payload))
            .await
            .unwrap()
            .status(),
        StatusCode::CONFLICT
    );
    let permits = manager.0.host_gate.acquire_many(2).await.unwrap();
    let response = app
        .clone()
        .oneshot(request(
            "POST",
            "/api/v1/hosts/remote/check",
            serde_json::Value::Null,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let location = response.headers()["location"].to_str().unwrap().to_string();
    assert!(location.starts_with("/api/v1/host-jobs/"));
    let job: crate::remote::HostJob =
        serde_json::from_slice(&to_bytes(response.into_body(), 100_000).await.unwrap()).unwrap();
    assert_eq!(
        app.clone()
            .oneshot(request(
                "DELETE",
                "/api/v1/hosts/remote",
                serde_json::Value::Null
            ))
            .await
            .unwrap()
            .status(),
        StatusCode::CONFLICT
    );
    drop(permits);
    assert_eq!(
        host_done(&manager, &job.id).await.status,
        JobStatus::Succeeded
    );
    assert_eq!(
        app.clone()
            .oneshot(request("GET", &location, serde_json::Value::Null))
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );
    assert_eq!(
        app.clone()
            .oneshot(request(
                "DELETE",
                "/api/v1/hosts/remote",
                serde_json::Value::Null
            ))
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );
    assert!(manager.hosts().unwrap().iter().all(|h| h.id == "local"));
    settle(&manager).await;
}

#[tokio::test]
async fn remote_host_preparation_is_serialized_idempotent_and_secrets_never_reach_state_or_api() {
    use crate::remote::{HostJobKind, test_server::Fixture};
    let fixture = Fixture::start().await;
    let dir = tempfile::tempdir().unwrap();
    let manager = open_with(options(dir.path()), Arc::new(Fake::default())).await;
    manager
        .register_host(fixture.registration("target"))
        .unwrap();
    assert!(
        manager
            .register_host(fixture.registration("duplicate-address"))
            .unwrap_err()
            .is::<Conflict>()
    );
    let job = manager
        .start_host_job("target", HostJobKind::Prepare)
        .unwrap();
    assert!(
        manager
            .start_host_job("target", HostJobKind::Check)
            .unwrap_err()
            .is::<Conflict>()
    );
    assert!(
        manager
            .unregister_host("target")
            .unwrap_err()
            .is::<Conflict>()
    );
    assert_eq!(
        host_done(&manager, &job.id).await.status,
        JobStatus::Succeeded
    );
    assert!(matches!(
        manager.host("target").unwrap().status,
        crate::remote::HostStatus::Ready
    ));
    let job = manager
        .start_host_job("target", HostJobKind::Prepare)
        .unwrap();
    assert_eq!(
        host_done(&manager, &job.id).await.status,
        JobStatus::Succeeded
    );
    let calls = fixture.observed.calls.lock().unwrap().clone();
    // Preparation leaves the golden image to the first farm; it never builds with Docker.
    assert!(!calls.iter().any(|c| c == "golden"), "{calls:?}");
    assert_eq!(calls.iter().filter(|c| *c == "smoke").count(), 2);
    fixture.observed.fail_smoke.store(true, Ordering::SeqCst);
    let job = manager
        .start_host_job("target", HostJobKind::Prepare)
        .unwrap();
    assert_eq!(host_done(&manager, &job.id).await.status, JobStatus::Failed);
    let state = std::fs::read_to_string(dir.path().join("state.json")).unwrap();
    for secret in ["fixture-password-only", "PRIVATE-KEY-DO-NOT-PERSIST"] {
        assert!(!state.contains(secret));
    }
    let app = api::router(manager.clone(), None);
    for route in ["/api/v1/hosts", "/api/v1/hosts/target", "/api/v1/host-jobs"] {
        let response = app
            .clone()
            .oneshot(Request::builder().uri(route).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()["cache-control"], "no-store");
        let text = String::from_utf8(
            to_bytes(response.into_body(), 1_000_000)
                .await
                .unwrap()
                .to_vec(),
        )
        .unwrap();
        assert!(
            !text.contains("fixture-password-only")
                && !text.contains("ciphertext")
                && !text.contains("credential_ref")
        );
    }
    drop(app);
    settle(&manager).await;
    drop(manager);
    let manager = open_with(options(dir.path()), Arc::new(Fake::default())).await;
    let job = manager
        .start_host_job("target", HostJobKind::Check)
        .unwrap();
    assert_eq!(
        host_done(&manager, &job.id).await.status,
        JobStatus::Succeeded
    );
    manager.unregister_host("target").unwrap();
    assert!(
        manager.hosts().unwrap().iter().all(|h| h.id == "local")
            && manager.host_jobs().unwrap().is_empty()
    );
    settle(&manager).await;
    assert!(
        manager
            .register_host(fixture.registration("after-shutdown"))
            .unwrap_err()
            .is::<Conflict>()
    );
}

#[tokio::test]
async fn preparation_upgrades_an_older_msb_and_refuses_a_newer_one() {
    use crate::remote::{HostJobKind, MSB_VERSION, test_server::Fixture};
    let fixture = Fixture::start().await;
    *fixture.observed.msb.lock().unwrap() = Some("0.7.8".into());
    let dir = tempfile::tempdir().unwrap();
    let manager = open_with(options(dir.path()), Arc::new(Fake::default())).await;
    manager
        .register_host(fixture.registration("target"))
        .unwrap();
    let job = manager
        .start_host_job("target", HostJobKind::Prepare)
        .unwrap();
    let finished = host_done(&manager, &job.id).await;
    assert_eq!(
        finished.status,
        JobStatus::Succeeded,
        "{:?}",
        finished.error
    );
    assert!(finished.events.iter().any(|e| {
        e.message
            .starts_with(&format!("Upgrading msb 0.7.8 to {MSB_VERSION}"))
    }));
    let inspection = manager.host("target").unwrap().inspection.unwrap();
    assert_eq!(inspection.msb_version.as_deref(), Some(MSB_VERSION));
    assert!(inspection.vm_boot_verified);

    *fixture.observed.msb.lock().unwrap() = Some("99.0.0".into());
    fixture.observed.calls.lock().unwrap().clear();
    let job = manager
        .start_host_job("target", HostJobKind::Prepare)
        .unwrap();
    let finished = host_done(&manager, &job.id).await;
    assert_eq!(finished.status, JobStatus::Failed);
    assert!(finished.error.unwrap().contains("does not downgrade msb"));
    assert!(
        !fixture
            .observed
            .calls
            .lock()
            .unwrap()
            .iter()
            .any(|c| c == "install")
    );
    settle(&manager).await;
}

/// A local farm-agent that reports `msb` as its installed version and installs on request.
#[derive(Default)]
struct MsbLink {
    msb: std::sync::Mutex<Option<String>>,
    calls: std::sync::Mutex<Vec<&'static str>>,
}
impl crate::remote::link::AgentLink for MsbLink {
    fn call(
        &self,
        request: crate::remote::protocol::Request,
        _: Option<crate::kolla::EventSink>,
    ) -> BoxFuture<'_, Result<crate::remote::protocol::Reply>> {
        use crate::remote::protocol::{Operation, Reply};
        Box::pin(async move {
            Ok(match request.operation {
                Operation::Hello => Reply::Hello {
                    version: crate::remote::protocol::VERSION,
                    arch: std::env::consts::ARCH.into(),
                    supports_single_vm: true,
                    build_id: crate::BUILD_ID.into(),
                },
                Operation::Inspect => {
                    let msb_version = self.msb.lock().unwrap().clone();
                    Reply::Inspection {
                        inspection: crate::remote::HostInspection {
                            os: "macos".into(),
                            arch: "aarch64".into(),
                            compatible: true,
                            kvm_available: true,
                            sudo_available: false,
                            memory_total_mib: 65536,
                            memory_available_mib: 65536,
                            disk_available_mib: 500_000,
                            msb_compatible: msb_version.as_deref()
                                == Some(crate::remote::MSB_VERSION),
                            msb_version,
                            docker_available: true,
                            golden_image_available: true,
                            vm_boot_verified: false,
                            checked_at: now(),
                        },
                    }
                }
                Operation::InstallMsb => {
                    self.calls.lock().unwrap().push("install");
                    *self.msb.lock().unwrap() = Some(crate::remote::MSB_VERSION.into());
                    Reply::Done
                }
                Operation::VerifyVm => {
                    self.calls.lock().unwrap().push("verify");
                    Reply::Done
                }
                operation => anyhow::bail!("unexpected {operation:?}"),
            })
        })
    }
    fn terminal(
        &self,
        _: crate::remote::protocol::Request,
        _: crate::farm::TerminalIo,
    ) -> BoxFuture<'_, Result<()>> {
        Box::pin(async { anyhow::bail!("no terminal") })
    }
}

#[tokio::test]
async fn agent_host_preparation_upgrades_an_older_msb_and_refuses_a_newer_one() {
    use crate::remote::{HostJobKind, MSB_VERSION};
    let dir = tempfile::tempdir().unwrap();
    let link = Arc::new(MsbLink::default());
    *link.msb.lock().unwrap() = Some("0.7.8".into());
    let manager = FarmManager::with_link(options(dir.path()), link.clone())
        .await
        .unwrap();
    manager
        .register_local_host(crate::remote::RegisterLocalHost { id: "local".into() })
        .unwrap();
    let job = manager
        .start_host_job("local", HostJobKind::Prepare)
        .unwrap();
    let finished = host_done(&manager, &job.id).await;
    assert_eq!(
        finished.status,
        JobStatus::Succeeded,
        "{:?}",
        finished.error
    );
    assert!(finished.events.iter().any(|e| {
        e.message
            .starts_with(&format!("Upgrading msb 0.7.8 to {MSB_VERSION}"))
    }));
    assert_eq!(*link.calls.lock().unwrap(), ["install", "verify"]);
    let inspection = manager.host("local").unwrap().inspection.unwrap();
    assert_eq!(inspection.msb_version.as_deref(), Some(MSB_VERSION));
    assert!(inspection.vm_boot_verified);

    // The current msb is left alone.
    link.calls.lock().unwrap().clear();
    let job = manager
        .start_host_job("local", HostJobKind::Prepare)
        .unwrap();
    assert_eq!(
        host_done(&manager, &job.id).await.status,
        JobStatus::Succeeded
    );
    assert_eq!(*link.calls.lock().unwrap(), ["verify"]);

    link.calls.lock().unwrap().clear();
    *link.msb.lock().unwrap() = Some("99.0.0".into());
    let job = manager
        .start_host_job("local", HostJobKind::Prepare)
        .unwrap();
    let finished = host_done(&manager, &job.id).await;
    assert_eq!(finished.status, JobStatus::Failed);
    assert!(finished.error.unwrap().contains("does not downgrade msb"));
    assert!(link.calls.lock().unwrap().is_empty());
    settle(&manager).await;
}

#[tokio::test]
async fn remote_host_restart_recovers_interrupted_jobs_and_missing_key_never_replaces_secrets() {
    use crate::remote::{HostJobKind, test_server::Fixture};
    let fixture = Fixture::start().await;
    let dir = tempfile::tempdir().unwrap();
    let manager = open_with(options(dir.path()), Arc::new(Fake::default())).await;
    manager
        .register_host(fixture.registration("target"))
        .unwrap();
    manager
        .update(|state| {
            state.hosts.get_mut("target").unwrap().host.active_job =
                Some("interrupted-test".into());
            state.host_jobs.insert(
                "interrupted-test".into(),
                crate::remote::HostJob {
                    id: "interrupted-test".into(),
                    host_id: "target".into(),
                    kind: HostJobKind::Prepare,
                    status: JobStatus::Running,
                    events: vec![],
                    error: None,
                    created_at: now(),
                    finished_at: None,
                    delete_snapshots: false,
                },
            );
            Ok(())
        })
        .unwrap();
    drop(manager);
    let manager = open_with(options(dir.path()), Arc::new(Fake::default())).await;
    assert_eq!(
        manager.host_job("interrupted-test").unwrap().status,
        JobStatus::Interrupted
    );
    assert!(manager.host("target").unwrap().active_job.is_none());
    settle(&manager).await;
    drop(manager);
    std::fs::remove_file(dir.path().join("credentials.key")).unwrap();
    assert!(
        FarmManager::with_backend(options(dir.path()), Arc::new(Fake::default()))
            .await
            .is_err()
    );
    assert!(!dir.path().join("credentials.key").exists());
}

#[tokio::test]
async fn lifecycle_reservations_failures_and_logs_survive_restart() {
    let dir = tempfile::tempdir().unwrap();
    let backend = Arc::new(Fake::default());
    let manager = open_with(options(dir.path()), backend.clone()).await;
    let create = manager.create(request("one")).unwrap();
    assert!(manager.create(request("one")).unwrap_err().is::<Conflict>());
    assert_eq!(done(&manager, &create).await.status, JobStatus::Succeeded);
    let other = manager.create(request("two")).unwrap();
    done(&manager, &other).await;
    assert_ne!(
        manager.get("one").unwrap().exposure.unwrap().port_base,
        manager.get("two").unwrap().exposure.unwrap().port_base
    );
    assert!(manager.endpoints("one").await.unwrap_err().is::<Conflict>());
    backend.fail.store(true, Ordering::SeqCst);
    let failed = manager
        .deploy(
            "one",
            DeployRequest {
                from: Phase::Prechecks,
            },
        )
        .unwrap();
    let failed = done(&manager, &failed).await;
    assert_eq!(failed.status, JobStatus::Failed);
    assert_eq!(failed.phase, Some(Phase::Prechecks));
    assert!(
        manager
            .get("one")
            .unwrap()
            .error
            .unwrap()
            .contains("intentional")
    );
    backend.fail.store(false, Ordering::SeqCst);
    let retry = manager
        .deploy(
            "one",
            DeployRequest {
                from: failed.phase.unwrap(),
            },
        )
        .unwrap();
    done(&manager, &retry).await;
    assert_eq!(manager.get("one").unwrap().status, FarmStatus::Ready);
    assert_eq!(
        manager.endpoints("one").await.unwrap().credentials["OS_PASSWORD"],
        "test-secret"
    );
    let logs = manager.logs("one", 0, 1000).unwrap();
    let last = logs.last().unwrap().id;
    assert!(logs.iter().any(|e| e.message == "test deployment output"));
    assert!(
        !std::fs::read_to_string(dir.path().join("state.json"))
            .unwrap()
            .contains("test-secret")
    );
    settle(&manager).await;
    drop(manager);
    let manager = open_with(options(dir.path()), backend.clone()).await;
    assert_eq!(manager.get("one").unwrap().status, FarmStatus::Ready);
    assert_eq!(
        manager.logs("one", 0, 1000).unwrap().last().unwrap().id,
        last
    );
    backend.farms.lock().unwrap().get_mut("one").unwrap().pop();
    manager.reconcile().await.unwrap();
    assert_eq!(manager.get("one").unwrap().status, FarmStatus::Broken);
    let deleted = manager.delete("one").unwrap();
    assert_eq!(done(&manager, &deleted).await.status, JobStatus::Succeeded);
    assert_eq!(manager.get("one").unwrap().status, FarmStatus::Deleted);
    assert_eq!(manager.list().unwrap().len(), 1);
    let reused = manager.create(request("one")).unwrap();
    done(&manager, &reused).await;
    assert!(
        manager
            .logs("one", last, 1000)
            .unwrap()
            .iter()
            .all(|e| e.id > last)
    );
    settle(&manager).await;
}

#[tokio::test]
async fn multi_node_farms_are_created_deployed_and_faulted_per_vm() {
    let dir = tempfile::tempdir().unwrap();
    let backend = Arc::new(Fake::default());
    let mut opts = options(dir.path());
    opts.memory_budget_mib = 64 * 1024;
    let manager = open_with(opts, backend.clone()).await;
    let multi = CreateRequest {
        computes: 2,
        ..request("mn")
    };
    if !cfg!(target_os = "linux") {
        // Off Linux the local host needs a Linux agent for the guests' links.
        let refused = manager.create(multi.clone()).unwrap_err();
        assert!(
            refused.to_string().contains("task agent-build"),
            "{refused}"
        );
        *manager.0.guest_agent.write().unwrap() = Some(dir.path().join("farm-agent"));
    }
    let job = manager.create(multi).unwrap();
    assert_eq!(done(&manager, &job).await.status, JobStatus::Succeeded);
    let farm = manager.get("mn").unwrap();
    let names: Vec<_> = farm.nodes.iter().map(|n| n.name.as_str()).collect();
    assert_eq!(names, ["mn-control", "mn-compute-1", "mn-compute-2"]);
    assert_eq!(farm.plan.as_ref().unwrap().computes, 2);
    let job = manager.deploy("mn", DeployRequest::default()).unwrap();
    assert_eq!(done(&manager, &job).await.status, JobStatus::Succeeded);
    manager.reconcile().await.unwrap();
    assert_eq!(manager.get("mn").unwrap().status, FarmStatus::Ready);
    manager.check_terminal("mn", None).unwrap();
    manager.check_terminal("mn", Some("compute-2")).unwrap();
    assert!(manager.check_terminal("mn", Some("compute-3")).is_err());

    let cut = crate::link::Impair {
        delay_ms: 0,
        loss: 1.0,
    };
    let farm = manager.impair("mn", "compute-1", cut).await.unwrap();
    assert_eq!(farm.faults["compute-1"], cut);
    // The record keeps the fault across a restart's reload.
    assert_eq!(manager.get("mn").unwrap().faults["compute-1"], cut);
    let farm = manager
        .impair("mn", "compute-1", crate::link::Impair::default())
        .await
        .unwrap();
    assert!(farm.faults.is_empty());
    assert_eq!(
        backend.faults.lock().unwrap().clone(),
        [
            ("mn".into(), "compute-1".into(), cut),
            ("mn".into(), "compute-1".into(), Default::default())
        ]
    );
    assert!(
        manager
            .impair("mn", "compute-3", cut)
            .await
            .unwrap_err()
            .is::<NotFound>()
    );
    let bad = crate::link::Impair {
        delay_ms: 0,
        loss: 2.0,
    };
    assert!(
        manager
            .impair("mn", "control", bad)
            .await
            .unwrap_err()
            .is::<BadRequest>()
    );
    // An all-in-one farm has no links.
    let job = manager.create(request("aio")).unwrap();
    assert_eq!(done(&manager, &job).await.status, JobStatus::Succeeded);
    assert!(
        manager
            .impair("aio", "control", cut)
            .await
            .unwrap_err()
            .is::<Conflict>()
    );
    // A multi-node farm freezes every VM and restores into a farm of the same layout.
    let job = manager
        .freeze(
            "mn",
            SnapshotRequest {
                id: "mn-snap".into(),
                note: None,
                node: None,
            },
        )
        .unwrap();
    assert_eq!(done(&manager, &job).await.status, JobStatus::Succeeded);
    let snapshot = manager.snapshot("mn-snap").unwrap();
    assert_eq!(snapshot.plan.computes, 2);
    let nodes: Vec<_> = snapshot.nodes.iter().map(|n| n.node.as_str()).collect();
    assert_eq!(nodes, ["control", "compute-1", "compute-2"]);
    assert_eq!(snapshot.reference.as_deref(), Some("fake/mn-snap-control"));
    assert_eq!(
        backend.snapshots.lock().unwrap()["fake/mn-snap-compute-2"]
            [crate::model::snapshot_labels::COMPUTES],
        "2"
    );
    assert!(
        manager
            .export_snapshot("mn-snap", ExportRequest::default())
            .unwrap_err()
            .is::<Conflict>()
    );
    let job = manager
        .create(CreateRequest {
            snapshot_id: Some("mn-snap".into()),
            ..request("mn2")
        })
        .unwrap();
    assert_eq!(done(&manager, &job).await.status, JobStatus::Succeeded);
    let restored = manager.get("mn2").unwrap();
    assert_eq!(restored.plan.as_ref().unwrap().computes, 2);
    assert_eq!(restored.status, FarmStatus::Ready);
    assert_eq!(restored.nodes.len(), 3);
    let job = manager.delete("mn2").unwrap();
    assert_eq!(done(&manager, &job).await.status, JobStatus::Succeeded);
    manager.delete_snapshot("mn-snap").await.unwrap();
    assert!(backend.snapshots.lock().unwrap().is_empty());
    // Over HTTP too.
    let app = api::router(manager.clone(), None);
    let response = app
        .oneshot(
            Request::put("/api/v1/farms/mn/nodes/control/faults")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"delay_ms":40}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(manager.get("mn").unwrap().faults["control"].delay_ms, 40);
    settle(&manager).await;
}

#[tokio::test]
async fn deploy_step_is_shown_while_running_and_cleared_after() {
    let dir = tempfile::tempdir().unwrap();
    let backend = Arc::new(Fake {
        deployment_gate: Some(Semaphore::new(0)),
        ..Default::default()
    });
    let manager = open_with(options(dir.path()), backend.clone()).await;
    let create = manager.create(request("step")).unwrap();
    done(&manager, &create).await;
    let job = manager.deploy("step", DeployRequest::default()).unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while manager.get("step").unwrap().step.is_none() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        manager.get("step").unwrap().step.as_deref(),
        Some("make images")
    );
    backend.deployment_gate.as_ref().unwrap().add_permits(1);
    done(&manager, &job).await;
    let farm = manager.get("step").unwrap();
    assert_eq!(farm.status, FarmStatus::Ready);
    assert!(farm.step.is_none(), "a finished job keeps no step text");
    settle(&manager).await;
}

#[tokio::test]
async fn deploy_queue_is_serial_and_farm_mutations_conflict() {
    let dir = tempfile::tempdir().unwrap();
    let backend = Arc::new(Fake {
        deployment_gate: Some(Semaphore::new(0)),
        ..Default::default()
    });
    let manager = open_with(options(dir.path()), backend.clone()).await;
    let a = manager.create(request("a")).unwrap();
    done(&manager, &a).await;
    let b = manager.create(request("b")).unwrap();
    done(&manager, &b).await;
    let first = manager.deploy("a", DeployRequest::default()).unwrap();
    let second = manager.deploy("b", DeployRequest::default()).unwrap();
    assert!(manager.delete("a").unwrap_err().is::<Conflict>());
    assert!(
        manager
            .deploy("a", DeployRequest::default())
            .unwrap_err()
            .is::<Conflict>()
    );
    tokio::time::timeout(Duration::from_secs(5), async {
        while backend.deploys.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(manager.job(&second.id).unwrap().status, JobStatus::Queued);
    backend.deployment_gate.as_ref().unwrap().add_permits(2);
    done(&manager, &first).await;
    done(&manager, &second).await;
    assert_eq!(backend.max_deploys.load(Ordering::SeqCst), 1);
    settle(&manager).await;
}

#[tokio::test]
async fn recovers_interrupted_jobs_and_refuses_second_writer_or_corrupt_state() {
    let dir = tempfile::tempdir().unwrap();
    let backend = Arc::new(Fake::default());
    let manager = open_with(options(dir.path()), backend.clone()).await;
    let create = manager.create(request("resume")).unwrap();
    done(&manager, &create).await;
    assert!(Store::open(dir.path()).is_err());
    manager
        .update(|state| {
            let mut job = new_job("resume", JobKind::Deploy);
            job.status = JobStatus::Running;
            job.phase = Some(Phase::Configure);
            let farm = state.farms.get_mut("resume").unwrap();
            farm.active_job = Some(job.id.clone());
            farm.status = FarmStatus::Deploying;
            farm.phase = job.phase;
            state.jobs.insert(job.id.clone(), job);
            Ok(())
        })
        .unwrap();
    drop(manager);
    let manager = open_with(options(dir.path()), backend).await;
    let farm = manager.get("resume").unwrap();
    assert_eq!(farm.status, FarmStatus::Failed);
    assert_eq!(farm.phase, Some(Phase::Configure));
    assert!(farm.active_job.is_none());
    assert_eq!(
        manager
            .jobs()
            .unwrap()
            .iter()
            .filter(|j| j.status == JobStatus::Interrupted)
            .count(),
        1
    );
    drop(manager);
    std::fs::write(dir.path().join("state.json"), "bad state").unwrap();
    assert!(Store::open(dir.path()).is_err());
    assert_eq!(
        std::fs::read_to_string(dir.path().join("state.json")).unwrap(),
        "bad state"
    );
}

#[tokio::test]
async fn memory_reservations_and_failed_state_writes_prevent_creation() {
    let dir = tempfile::tempdir().unwrap();
    let backend = Arc::new(Fake::default());
    let mut opts = options(dir.path());
    opts.memory_budget_mib = 3072;
    let manager = open_with(opts, backend.clone()).await;
    let created = manager.create(request("first")).unwrap();
    done(&manager, &created).await;
    assert!(
        manager
            .create(request("over-budget"))
            .unwrap_err()
            .is::<Conflict>()
    );
    let deleted = manager.delete("first").unwrap();
    done(&manager, &deleted).await;
    std::fs::create_dir(dir.path().join("state.json.tmp")).unwrap();
    assert!(manager.create(request("cannot-write")).is_err());
    assert!(manager.get("cannot-write").is_err());
    assert!(backend.farms.lock().unwrap().is_empty());
}

#[tokio::test]
async fn http_auth_validation_job_locations_and_schema() {
    let dir = tempfile::tempdir().unwrap();
    let manager = open_with(options(dir.path()), Arc::new(Fake::default())).await;
    let app = api::router(manager.clone(), Some("test-token".into()));
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/v1/catalog")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let authorized = |method: &str, uri: &str, payload: &str| {
        Request::builder()
            .method(method)
            .uri(uri)
            .header("authorization", "Bearer test-token")
            .header("content-type", "application/json")
            .body(Body::from(payload.to_string()))
            .unwrap()
    };
    let response = app
        .clone()
        .oneshot(authorized("POST", "/api/v1/farms", r#"{"id":"../bad"}"#))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let response = app
        .clone()
        .oneshot(authorized(
            "POST",
            "/api/v1/plans",
            r#"{"components":["swift"]}"#,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let response = app
        .clone()
        .oneshot(authorized("POST", "/api/v1/farms", r#"{"id":"http-demo"}"#))
        .await
        .unwrap();
    // Every farm runs on a registered host.
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let response = app
        .clone()
        .oneshot(authorized(
            "POST",
            "/api/v1/farms",
            r#"{"id":"http-demo","host_id":"local"}"#,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    assert!(
        response.headers()["location"]
            .to_str()
            .unwrap()
            .starts_with("/api/v1/jobs/")
    );
    assert_eq!(response.headers()["cache-control"], "no-store");
    let created: Job =
        serde_json::from_slice(&to_bytes(response.into_body(), 1_000_000).await.unwrap()).unwrap();
    done(&manager, &created).await;
    let response = app
        .clone()
        .oneshot(authorized("GET", "/api/v1/farms/http-demo", ""))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let response = app
        .clone()
        .oneshot(authorized("GET", "/api/v1/jobs/missing", ""))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let response = app
        .clone()
        .oneshot(authorized("POST", "/api/v1/farms/http-demo/deploy", "{}"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let deploy: Job =
        serde_json::from_slice(&to_bytes(response.into_body(), 1_000_000).await.unwrap()).unwrap();
    done(&manager, &deploy).await;
    let response = app
        .clone()
        .oneshot(authorized(
            "GET",
            "/api/v1/farms/http-demo/logs/stream?after=1",
            "",
        ))
        .await
        .unwrap();
    assert_eq!(response.headers()["content-type"], "text/event-stream");
    use http_body_util::BodyExt;
    let mut stream = response.into_body();
    let frame = tokio::time::timeout(Duration::from_secs(2), stream.frame())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let data = String::from_utf8_lossy(frame.data_ref().unwrap());
    assert!(data.contains("event: log") && data.contains("id: 2"));
    drop(stream);
    let response = app
        .oneshot(authorized("GET", "/api/v1/openapi.json", ""))
        .await
        .unwrap();
    let spec: serde_json::Value =
        serde_json::from_slice(&to_bytes(response.into_body(), 1_000_000).await.unwrap()).unwrap();
    assert!(spec["paths"]["/api/v1/farms/{id}/logs/stream"].is_object());
    assert!(spec["components"]["schemas"]["CreateRequest"].is_object());
    assert_eq!(model::plan(&PlanRequest::default()).unwrap().nodes.len(), 1);
    settle(&manager).await;
}

#[tokio::test]
async fn stale_inventory_scan_does_not_break_a_newly_created_farm() {
    let dir = tempfile::tempdir().unwrap();
    let backend = Arc::new(Fake::default());
    let manager = open_with(options(dir.path()), backend.clone()).await;
    // Reconciliation scans the hosts that hold farms.
    done(&manager, &manager.create(request("existing")).unwrap()).await;
    let started = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    *backend.scan_delay.lock().unwrap() = Some((started.clone(), release.clone()));
    let scanning = manager.clone();
    let scan = tokio::spawn(async move { scanning.reconcile().await });
    tokio::time::timeout(Duration::from_secs(2), started.notified())
        .await
        .unwrap();
    let job = manager.create(request("racing")).unwrap();
    assert_eq!(done(&manager, &job).await.status, JobStatus::Succeeded);
    release.notify_one();
    scan.await.unwrap().unwrap();
    let farm = manager.get("racing").unwrap();
    assert_eq!(farm.status, FarmStatus::Created);
    assert_eq!(farm.nodes.len(), 1);
    settle(&manager).await;
}

#[tokio::test]
async fn unreserved_vms_are_never_adopted_and_hostless_farms_can_only_be_removed() {
    let dir = tempfile::tempdir().unwrap();
    let backend = Arc::new(Fake::default());
    // A VM with farm labels that no agent reservation of this coordinator covers.
    backend.farms.lock().unwrap().insert(
        "stray".into(),
        vec![FarmNode {
            index: None,
            name: "stray".into(),
            role: Some(Role::AllInOne),
            status: "running".into(),
            memory_mib: 512,
            ip: None,
        }],
    );
    let manager = open_with(options(dir.path()), backend.clone()).await;
    manager.reconcile().await.unwrap();
    assert!(manager.get("stray").unwrap_err().is::<NotFound>());
    // A farm recorded before farms needed a host has no agent to act through.
    manager
        .update(|state| {
            let farm = FarmRecord {
                id: "old".into(),
                host_id: None,
                remote_generation: None,
                agent_sha256: None,
                observation_error: None,
                last_seen_at: None,
                status: FarmStatus::Created,
                plan: None,
                exposure: None,
                owner: None,
                expires_at: None,
                nodes: vec![],
                phase: None,
                step: None,
                error: None,
                active_job: None,
                created_at: now(),
                updated_at: now(),
                snapshot_id: None,
                faults: Default::default(),
                shares: vec![],
                uplink_sha256: None,
            };
            state.farms.insert("old".into(), farm);
            Ok(())
        })
        .unwrap();
    assert!(
        manager
            .deploy("old", DeployRequest::default())
            .unwrap_err()
            .is::<Conflict>()
    );
    let deleted = manager.delete("old").unwrap();
    assert_eq!(done(&manager, &deleted).await.status, JobStatus::Succeeded);
    assert_eq!(manager.get("old").unwrap().status, FarmStatus::Deleted);
    // The stray VM was left alone.
    assert!(backend.farms.lock().unwrap().contains_key("stray"));
    settle(&manager).await;
}

#[tokio::test]
async fn snapshots_freeze_restore_export_import_and_delete() {
    let dir = tempfile::tempdir().unwrap();
    let backend = Arc::new(Fake::default());
    let manager = open_with(options(dir.path()), backend.clone()).await;
    let mut source = request("src");
    source.components = vec!["horizon".into()];
    done(&manager, &manager.create(source).unwrap()).await;
    done(
        &manager,
        &manager.deploy("src", DeployRequest::default()).unwrap(),
    )
    .await;
    let freeze = |id: &str| {
        manager.freeze(
            "src",
            SnapshotRequest {
                id: id.into(),
                note: Some("clean install".into()),
                node: None,
            },
        )
    };
    assert!(freeze("../bad").unwrap_err().is::<BadRequest>());
    let job = freeze("clean").unwrap();
    assert_eq!(job.kind, JobKind::Snapshot);
    assert!(freeze("other").unwrap_err().is::<Conflict>());
    let frozen = done(&manager, &job).await;
    assert_eq!(frozen.status, JobStatus::Succeeded);
    // The backend's steps, numbered against one more for the services check after them.
    let steps: Vec<_> = frozen
        .steps
        .iter()
        .map(|s| (s.index, s.total, s.step.as_str(), s.failed))
        .collect();
    assert_eq!(
        steps,
        [
            (1, 4, "stop the VMs", false),
            (2, 4, "capture the VMs", false),
            (3, 4, "start the VMs", false),
            (4, 4, "check that the services answer", false),
        ]
    );
    assert!(frozen.steps.iter().all(|s| s.finished_at.is_some()));
    assert!(
        frozen
            .steps
            .windows(2)
            .all(|w| w[0].finished_at.unwrap() <= w[1].started_at)
    );
    assert!(manager.get("src").unwrap().step.is_none());
    assert!(freeze("clean").unwrap_err().is::<Conflict>());
    let snapshot = manager.snapshot("clean").unwrap();
    assert_eq!(snapshot.status, SnapshotStatus::Ready);
    assert!(snapshot.deployed);
    assert_eq!(snapshot.plan.components, ["keystone", "glance", "horizon"]);
    assert_eq!(snapshot.reference.as_deref(), Some("fake/clean"));
    let source = manager.get("src").unwrap();
    assert_eq!(source.status, FarmStatus::Ready);
    assert!(source.active_job.is_none());
    // The source's services were awaited after its restart, without rebinding.
    assert!(matches!(backend.settled.lock().unwrap()[..], [None]));
    let labels = backend.snapshots.lock().unwrap()["fake/clean"].clone();
    assert_eq!(
        labels["farm.snapshot.components"],
        "keystone,glance,horizon"
    );
    assert_eq!(labels["farm.snapshot.source"], "src");

    // Restore: the snapshot's plan replaces components, and public endpoints move to new ports.
    let mut copy = request("copy");
    copy.snapshot_id = Some("clean".into());
    copy.components = vec!["nova".into()];
    assert!(manager.create(copy.clone()).is_err());
    copy.components.clear();
    let mut missing = copy.clone();
    missing.id = "missing".into();
    missing.snapshot_id = Some("nope".into());
    assert!(manager.create(missing).unwrap_err().is::<NotFound>());
    let job = manager.create(copy).unwrap();
    assert_eq!(job.kind, JobKind::Restore);
    assert_eq!(job.snapshot_id.as_deref(), Some("clean"));
    assert_eq!(done(&manager, &job).await.status, JobStatus::Succeeded);
    let restored = manager.get("copy").unwrap();
    assert_eq!(restored.status, FarmStatus::Ready);
    assert_eq!(restored.snapshot_id.as_deref(), Some("clean"));
    assert_eq!(
        restored.plan.as_ref().unwrap().components,
        snapshot.plan.components
    );
    let (from, to) = backend.settled.lock().unwrap()[1].clone().unwrap();
    assert_eq!(from.port_base, source.exposure.as_ref().unwrap().port_base);
    assert_eq!(to.port_base, restored.exposure.as_ref().unwrap().port_base);
    assert_ne!(from.port_base, to.port_base);
    assert!(
        manager
            .logs("copy", 0, 1000)
            .unwrap()
            .iter()
            .any(|e| e.message == "services answer")
    );

    // Export writes the archive under its final name only when complete.
    assert!(
        manager
            .export_snapshot(
                "clean",
                ExportRequest {
                    path: Some("relative.msb".into()),
                    ..Default::default()
                }
            )
            .unwrap_err()
            .is::<BadRequest>()
    );
    let exporting = manager
        .export_snapshot("clean", ExportRequest::default())
        .unwrap();
    assert_eq!(exporting.status, SnapshotStatus::Exporting);
    let exported = manager.wait_snapshot("clean").await.unwrap();
    assert_eq!(exported.status, SnapshotStatus::Ready);
    let archive = exported.archive.unwrap();
    let path = dir.path().join("exports/clean.msb");
    assert_eq!(archive.path, path.display().to_string());
    assert_eq!(archive.size_bytes, std::fs::metadata(&path).unwrap().len());
    assert!(!dir.path().join("exports/clean.msb.partial").exists());
    assert!(
        manager
            .export_snapshot("clean", ExportRequest::default())
            .unwrap_err()
            .is::<Conflict>()
    );

    // Import reads the plan back from the archive's labels.
    let import = |path: &std::path::Path, id: Option<&str>| {
        manager.import_snapshot(ImportRequest {
            path: Some(path.display().to_string()),
            id: id.map(Into::into),
            ..Default::default()
        })
    };
    assert!(import(&path, None).unwrap_err().is::<Conflict>());
    assert!(
        import(&dir.path().join("absent.msb"), Some("x"))
            .unwrap_err()
            .is::<BadRequest>()
    );
    assert_eq!(
        import(&path, Some("moved")).unwrap().status,
        SnapshotStatus::Importing
    );
    let moved = manager.wait_snapshot("moved").await.unwrap();
    assert_eq!(moved.status, SnapshotStatus::Ready);
    assert_eq!(moved.plan.components, snapshot.plan.components);
    assert!(moved.deployed);
    assert_eq!(moved.source_farm.as_deref(), Some("src"));
    assert_eq!(moved.note.as_deref(), Some("clean install"));
    assert_eq!(moved.source_exposure.port_base, from.port_base);
    // An archive without farm labels is rejected and not left in msb.
    let plain = dir.path().join("plain.msb");
    std::fs::write(&plain, b"x").unwrap();
    import(&plain, None).unwrap();
    let rejected = manager.wait_snapshot("plain").await.unwrap();
    assert_eq!(rejected.status, SnapshotStatus::Failed);
    assert!(rejected.error.unwrap().contains("not frozen by farm"));
    assert!(
        !backend
            .snapshots
            .lock()
            .unwrap()
            .contains_key("fake/imported/plain")
    );

    // A failed capture leaves the farm as it was and records the error on the snapshot.
    backend.fail.store(true, Ordering::SeqCst);
    let failed = done(&manager, &freeze("broken").unwrap()).await;
    assert_eq!(failed.status, JobStatus::Failed);
    // The step it failed in is marked, and no later one started.
    let last = failed.steps.last().unwrap();
    assert_eq!((last.index, last.step.as_str()), (2, "capture the VMs"));
    assert!(last.failed && last.finished_at.is_some());
    assert!(!failed.steps[0].failed);
    assert_eq!(
        manager.snapshot("broken").unwrap().status,
        SnapshotStatus::Failed
    );
    assert_eq!(manager.get("src").unwrap().status, FarmStatus::Ready);
    backend.fail.store(false, Ordering::SeqCst);

    // A guest frozen elsewhere cannot boot here, and an archive records where it was frozen.
    let mut foreign = manager.snapshot("clean").unwrap();
    foreign.arch = if current_arch() == "aarch64" {
        "x86_64".into()
    } else {
        "aarch64".into()
    };
    manager
        .update(|state| {
            state.snapshots.insert("clean".into(), foreign.clone());
            Ok(())
        })
        .unwrap();
    let mut from_foreign = request("foreign");
    from_foreign.snapshot_id = Some("clean".into());
    let error = manager.create(from_foreign).unwrap_err().to_string();
    assert!(error.contains("cannot boot on this"), "{error}");
    // Freezing records the architecture the daemon runs on.
    assert_eq!(manager.snapshot("clean").unwrap().arch, foreign.arch);
    let local = manager.freeze(
        "src",
        SnapshotRequest {
            id: "local-arch".into(),
            note: None,
            node: None,
        },
    );
    done(&manager, &local.unwrap()).await;
    assert_eq!(manager.snapshot("local-arch").unwrap().arch, current_arch());
    assert_eq!(backend.snapshots.lock().unwrap().len(), 3);
    let arch_label = backend.snapshots.lock().unwrap()["fake/local-arch"]
        .get("farm.snapshot.arch")
        .cloned();
    assert_eq!(arch_label.as_deref(), Some(current_arch().as_str()));
    // The host platform travels with the archive and is shown to the operator.
    let frozen = manager.snapshot("local-arch").unwrap();
    assert_eq!(
        frozen.platform.as_deref(),
        Some(current_platform().as_str())
    );
    let platform_label = backend.snapshots.lock().unwrap()["fake/local-arch"]
        .get("farm.snapshot.platform")
        .cloned();
    assert_eq!(platform_label.as_deref(), Some(current_platform().as_str()));
    manager.delete_snapshot("local-arch").await.unwrap();

    manager.delete_snapshot("moved").await.unwrap();
    assert!(manager.snapshot("moved").unwrap_err().is::<NotFound>());
    assert!(
        !backend
            .snapshots
            .lock()
            .unwrap()
            .contains_key("fake/imported/moved")
    );
    manager.delete_snapshot("broken").await.unwrap();
    assert!(
        manager
            .delete_snapshot("moved")
            .await
            .unwrap_err()
            .is::<NotFound>()
    );

    // Snapshots persist, and interrupted operations recover to an actionable state.
    settle(&manager).await;
    drop(manager);
    let state = dir.path().join("state.json");
    let mut saved: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&state).unwrap()).unwrap();
    saved["snapshots"]["clean"]["status"] = "exporting".into();
    std::fs::write(&state, serde_json::to_vec(&saved).unwrap()).unwrap();
    let manager = open_with(options(dir.path()), backend.clone()).await;
    let clean = manager.snapshot("clean").unwrap();
    assert_eq!(clean.status, SnapshotStatus::Ready);
    assert!(clean.error.unwrap().contains("export again"));
    assert_eq!(
        manager
            .snapshots()
            .unwrap()
            .iter()
            .map(|s| s.id.as_str())
            .collect::<Vec<_>>(),
        ["clean", "plain"]
    );

    let app = api::router(manager.clone(), None);
    let call = |method: &str, uri: &str, payload: &str| {
        Request::builder()
            .method(method)
            .uri(uri)
            .header("content-type", "application/json")
            .body(Body::from(payload.to_string()))
            .unwrap()
    };
    let response = app
        .clone()
        .oneshot(call("GET", "/api/v1/snapshots", ""))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let response = app
        .clone()
        .oneshot(call(
            "POST",
            "/api/v1/farms/src/snapshots",
            r#"{"id":"Bad"}"#,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let response = app
        .clone()
        .oneshot(call(
            "POST",
            "/api/v1/snapshots/import",
            r#"{"path":"rel.msb"}"#,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let response = app
        .clone()
        .oneshot(call("POST", "/api/v1/snapshots/clean/export", "{}"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CONFLICT);
    let response = app
        .clone()
        .oneshot(call("DELETE", "/api/v1/snapshots/plain", ""))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let response = app
        .oneshot(call("GET", "/api/v1/snapshots/plain", ""))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    settle(&manager).await;
}

#[tokio::test]
async fn bucket_archives_carry_metadata_files_and_an_index() {
    use crate::s3::{
        RegisterBucket,
        catalog::{ArchiveMetadata, BucketIndex, TemplateQuery},
        fake::FakeS3,
    };
    use sha2::{Digest, Sha256};
    let s3 = Arc::new(FakeS3::default());
    let endpoint = s3.start().await;
    let dir = tempfile::tempdir().unwrap();
    let backend = Arc::new(Fake::default());
    let manager = open_with(options(dir.path()), backend.clone()).await;
    manager
        .register_bucket(RegisterBucket {
            id: "minio".into(),
            endpoint: Some(endpoint),
            region: None,
            name: "farm".into(),
            prefix: Some("snapshots".into()),
            path_style: None,
            ca_pem: None,
            access_key_id: "admin".into(),
            secret_access_key: Some("secret".into()),
        })
        .unwrap();
    done(&manager, &manager.create(request("src")).unwrap()).await;
    let freeze = manager
        .freeze(
            "src",
            SnapshotRequest {
                id: "clean".into(),
                note: Some("catalogued".into()),
                node: None,
            },
        )
        .unwrap();
    done(&manager, &freeze).await;
    let export = |key: Option<&str>| {
        manager.export_snapshot(
            "clean",
            ExportRequest {
                bucket: Some("minio".into()),
                key: key.map(Into::into),
                ..Default::default()
            },
        )
    };
    // The metadata file would be the catalog itself.
    assert!(
        export(Some("snapshots/index"))
            .unwrap_err()
            .is::<BadRequest>()
    );
    export(None).unwrap();
    let exported = manager.wait_snapshot("clean").await.unwrap();
    assert_eq!(exported.error, None);
    // The metadata file goes up after the archive, and the index after both.
    assert_eq!(
        *s3.writes.lock().unwrap(),
        [
            "snapshots/clean.msb",
            "snapshots/clean.json",
            "snapshots/index.json"
        ]
    );
    let object = |key: &str| s3.objects.lock().unwrap().get(key).cloned();
    let archive = object("snapshots/clean.msb").unwrap();
    let sha256 = format!("{:x}", Sha256::digest(&archive));
    assert_eq!(exported.archive.unwrap().sha256.as_deref(), Some(&*sha256));
    let metadata = ArchiveMetadata::parse(&object("snapshots/clean.json").unwrap()).unwrap();
    assert_eq!(metadata.key, "snapshots/clean.msb");
    assert_eq!(metadata.size_bytes, archive.len() as u64);
    assert_eq!(metadata.sha256, sha256);
    assert_eq!(metadata.snapshot_id, "clean");
    assert_eq!(
        metadata.release.as_deref(),
        Some(crate::kolla::OPENSTACK_RELEASE)
    );
    assert_eq!(metadata.role, None);
    assert_eq!(metadata.source_farm.as_deref(), Some("src"));
    assert_eq!(metadata.note.as_deref(), Some("catalogued"));
    assert_eq!(metadata.arch, current_arch());
    let index = BucketIndex::parse(&object("snapshots/index.json").unwrap()).unwrap();
    assert_eq!(index.entries, std::slice::from_ref(&metadata));

    // Archives from before metadata files are listed without any, and templates written
    // by another coordinator are picked up from their files and indexed.
    let put = |key: &str, body: Vec<u8>| {
        s3.objects.lock().unwrap().insert(key.into(), body);
    };
    put("snapshots/old.msb", archive.clone());
    let template = |key: &str, role, release: &str| {
        let mut template = metadata.clone();
        template.key = key.into();
        template.snapshot_id = key.into();
        template.role = Some(role);
        template.release = Some(release.into());
        template.exported_at += 1;
        put(key, archive.clone());
        put(
            &crate::s3::catalog::metadata_key(key),
            serde_json::to_vec(&template).unwrap(),
        );
        template
    };
    let control = template("snapshots/ctl.msb", TemplateRole::Control, "2025.1");
    template("snapshots/cmp.msb", TemplateRole::Compute, "2024.2");
    let listed = manager.bucket_objects("minio").await.unwrap();
    let about = |key: &str| {
        listed
            .iter()
            .find(|o| o.key == key)
            .unwrap()
            .metadata
            .clone()
    };
    assert_eq!(listed.len(), 4);
    assert_eq!(about("snapshots/old.msb"), None);
    assert_eq!(about("snapshots/ctl.msb"), Some(control.clone()));
    let indexed = || {
        let mut keys: Vec<_> = BucketIndex::parse(&object("snapshots/index.json").unwrap())
            .unwrap()
            .entries
            .into_iter()
            .map(|e| e.key)
            .collect();
        keys.sort();
        keys
    };
    assert_eq!(
        indexed(),
        [
            "snapshots/clean.msb",
            "snapshots/cmp.msb",
            "snapshots/ctl.msb"
        ]
    );
    let found = |query: TemplateQuery| {
        let manager = manager.clone();
        async move { manager.find_templates("minio", &query).await.unwrap() }
    };
    assert_eq!(
        found(TemplateQuery {
            release: Some("2025.1".into()),
            role: Some(TemplateRole::Control),
            arch: None,
            components: Some(metadata.components.clone()),
        })
        .await,
        std::slice::from_ref(&control)
    );
    assert!(
        found(TemplateQuery {
            release: Some("2025.1".into()),
            role: Some(TemplateRole::Compute),
            ..Default::default()
        })
        .await
        .is_empty()
    );
    // Farm snapshots are not templates.
    assert_eq!(found(TemplateQuery::default()).await.len(), 2);

    // A corrupt index is rebuilt by the next listing; a gone archive leaves it.
    put("snapshots/index.json", b"not json".to_vec());
    s3.objects.lock().unwrap().remove("snapshots/cmp.msb");
    manager.bucket_objects("minio").await.unwrap();
    assert_eq!(indexed(), ["snapshots/clean.msb", "snapshots/ctl.msb"]);
    s3.objects.lock().unwrap().remove("snapshots/index.json");
    let rebuilt = manager.rebuild_bucket_index("minio").await.unwrap();
    assert_eq!(rebuilt.entries.len(), 2);
    assert_eq!(indexed(), ["snapshots/clean.msb", "snapshots/ctl.msb"]);

    let import = |key: &str, id: &str| {
        manager
            .import_snapshot(ImportRequest {
                bucket: Some("minio".into()),
                key: Some(key.into()),
                id: Some(id.into()),
                ..Default::default()
            })
            .unwrap();
        let manager = manager.clone();
        let id = id.to_owned();
        async move { manager.wait_snapshot(&id).await.unwrap() }
    };
    let verified = import("snapshots/clean.msb", "verified").await;
    assert_eq!(
        verified.status,
        SnapshotStatus::Ready,
        "{:?}",
        verified.error
    );
    let legacy = import("snapshots/old.msb", "legacy").await;
    assert_eq!(legacy.status, SnapshotStatus::Ready, "{:?}", legacy.error);
    // A replaced archive does not match its metadata file's SHA-256.
    let mut tampered = archive.clone();
    tampered.push(b' ');
    put("snapshots/clean.msb", tampered);
    let refused = import("snapshots/clean.msb", "tampered").await;
    assert_eq!(refused.status, SnapshotStatus::Failed);
    assert!(refused.error.unwrap().contains("SHA-256"));
    assert!(
        !backend
            .snapshots
            .lock()
            .unwrap()
            .contains_key("fake/imported/tampered")
    );
    // The labels inside say farm snapshot, the metadata file control template.
    let contradicted = import("snapshots/ctl.msb", "contradicted").await;
    assert_eq!(contradicted.status, SnapshotStatus::Failed);
    let error = contradicted.error.unwrap();
    assert!(
        error.contains("does not match its metadata file"),
        "{error}"
    );
    assert!(
        !backend
            .snapshots
            .lock()
            .unwrap()
            .contains_key("fake/imported/contradicted")
    );
    settle(&manager).await;
}

#[tokio::test]
async fn snapshots_move_through_registered_buckets_with_sealed_keys() {
    use crate::s3::{RegisterBucket, fake::FakeS3};
    let s3 = Arc::new(FakeS3::default());
    let endpoint = s3.start().await;
    let dir = tempfile::tempdir().unwrap();
    let backend = Arc::new(Fake::default());
    let manager = open_with(options(dir.path()), backend.clone()).await;
    let registration = |secret: Option<&str>| RegisterBucket {
        id: "minio".into(),
        endpoint: Some(endpoint.clone()),
        region: None,
        name: "farm".into(),
        prefix: Some("snapshots".into()),
        path_style: None,
        ca_pem: None,
        access_key_id: "admin".into(),
        secret_access_key: secret.map(Into::into),
    };
    assert!(
        manager
            .register_bucket(registration(None))
            .unwrap_err()
            .is::<BadRequest>()
    );
    assert!(
        manager
            .test_bucket(registration(Some("unique-test-secret")))
            .await
            .unwrap()
            .contains("s3://farm/snapshots/")
    );
    assert!(s3.objects.lock().unwrap().is_empty());
    let bucket = manager
        .register_bucket(registration(Some("unique-test-secret")))
        .unwrap();
    assert_eq!(bucket.prefix, "snapshots/");
    assert!(bucket.path_style);
    assert!(
        manager
            .register_bucket(registration(Some("x")))
            .unwrap_err()
            .is::<Conflict>()
    );
    // The secret is sealed: neither the state file nor the API shows it.
    let state = std::fs::read_to_string(dir.path().join("state.json")).unwrap();
    assert!(!state.contains("unique-test-secret"));
    // An update may keep the secret, but not when it would be sent somewhere else.
    let mut moved = registration(None);
    moved.endpoint = Some("http://elsewhere:9000".into());
    assert!(manager.update_bucket(moved).unwrap_err().is::<BadRequest>());
    let mut renamed = registration(None);
    renamed.prefix = Some("farm/".into());
    assert_eq!(manager.update_bucket(renamed).unwrap().prefix, "farm/");
    manager.check_bucket("minio").await.unwrap();

    done(&manager, &manager.create(request("src")).unwrap()).await;
    let freeze = manager
        .freeze(
            "src",
            SnapshotRequest {
                id: "clean".into(),
                note: Some("via s3".into()),
                node: None,
            },
        )
        .unwrap();
    done(&manager, &freeze).await;
    let export = |bucket: &str, key: Option<&str>| {
        manager.export_snapshot(
            "clean",
            ExportRequest {
                bucket: Some(bucket.into()),
                key: key.map(Into::into),
                ..Default::default()
            },
        )
    };
    assert!(export("nope", None).unwrap_err().is::<BadRequest>());
    assert!(
        export("minio", Some("/abs"))
            .unwrap_err()
            .is::<BadRequest>()
    );
    let exporting = export("minio", None).unwrap();
    let transfer = exporting.transfer.unwrap();
    assert_eq!(transfer.key, "farm/clean.msb");
    // A running transfer keeps its bucket registered.
    assert!(
        manager
            .unregister_bucket("minio")
            .unwrap_err()
            .is::<Conflict>()
    );
    let exported = manager.wait_snapshot("clean").await.unwrap();
    assert_eq!(exported.error, None);
    assert!(exported.transfer.is_none());
    let archive = exported.archive.unwrap();
    assert_eq!(archive.path, "s3://farm/farm/clean.msb");
    assert_eq!(archive.bucket.as_deref(), Some("minio"));
    let uploaded = s3.objects.lock().unwrap()["farm/clean.msb"].clone();
    assert_eq!(archive.size_bytes, uploaded.len() as u64);
    // The local copy is gone once uploaded.
    assert_eq!(
        std::fs::read_dir(dir.path().join("transfers"))
            .unwrap()
            .count(),
        0
    );
    // An existing object is not replaced.
    export("minio", None).unwrap();
    let again = manager.wait_snapshot("clean").await.unwrap();
    assert!(
        again.error.unwrap().contains("already exists"),
        "export over an object"
    );
    assert_eq!(again.archive.unwrap().exported_at, archive.exported_at);
    // Exports keep the extension imports look for.
    export("minio", Some("farm/renamed")).unwrap();
    let renamed = manager.wait_snapshot("clean").await.unwrap();
    assert_eq!(renamed.error, None);
    assert_eq!(
        renamed.archive.unwrap().key.as_deref(),
        Some("farm/renamed.msb")
    );
    // A failed upload is aborted rather than left billed.
    *s3.fail_parts.lock().unwrap() = Some(StatusCode::FORBIDDEN);
    export("minio", Some("farm/failed.msb")).unwrap();
    let failed = manager.wait_snapshot("clean").await.unwrap();
    assert!(failed.error.unwrap().contains("upload part 1"));
    assert!(s3.uploads.lock().unwrap().is_empty());
    assert!(
        manager
            .update(|state| Ok(state.uploads.is_empty()))
            .unwrap()
    );
    *s3.fail_parts.lock().unwrap() = None;

    let objects = manager.bucket_objects("minio").await.unwrap();
    assert_eq!(
        objects.iter().map(|o| o.key.as_str()).collect::<Vec<_>>(),
        ["farm/clean.msb", "farm/renamed.msb"]
    );
    let import = |key: Option<&str>, id: Option<&str>| {
        manager.import_snapshot(ImportRequest {
            bucket: Some("minio".into()),
            key: key.map(Into::into),
            id: id.map(Into::into),
            ..Default::default()
        })
    };
    assert!(import(None, Some("x")).unwrap_err().is::<BadRequest>());
    assert!(
        import(Some("farm/clean.msb"), None)
            .unwrap_err()
            .is::<Conflict>()
    );
    let importing = import(Some("farm/clean.msb"), Some("moved")).unwrap();
    assert_eq!(importing.transfer.unwrap().key, "farm/clean.msb");
    let moved = manager.wait_snapshot("moved").await.unwrap();
    assert_eq!(moved.status, SnapshotStatus::Ready, "{:?}", moved.error);
    assert_eq!(moved.note.as_deref(), Some("via s3"));
    assert_eq!(moved.source_farm.as_deref(), Some("src"));
    assert!(moved.transfer.is_none());
    import(Some("farm/absent.msb"), None).unwrap();
    let absent = manager.wait_snapshot("absent").await.unwrap();
    assert_eq!(absent.status, SnapshotStatus::Failed);
    assert!(absent.error.unwrap().contains("NoSuchKey"));

    // Uploads a stopped daemon left are aborted at the next start; the key still unseals.
    s3.uploads
        .lock()
        .unwrap()
        .insert("left".into(), Default::default());
    manager
        .update(|state| {
            state.uploads.insert(
                "left".into(),
                crate::s3::PendingUpload {
                    bucket: "minio".into(),
                    key: "farm/left.msb".into(),
                },
            );
            Ok(())
        })
        .unwrap();
    settle(&manager).await;
    drop(manager);
    let manager = open_with(options(dir.path()), backend.clone()).await;
    for _ in 0..50 {
        if s3.uploads.lock().unwrap().is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(s3.uploads.lock().unwrap().is_empty());
    manager.check_bucket("minio").await.unwrap();

    let app = api::router(manager.clone(), None);
    let call = |method: &str, uri: &str, payload: &str| {
        Request::builder()
            .method(method)
            .uri(uri)
            .header("content-type", "application/json")
            .body(Body::from(payload.to_string()))
            .unwrap()
    };
    let response = app
        .clone()
        .oneshot(call("GET", "/api/v1/buckets", ""))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), 1_000_000).await.unwrap();
    let text = String::from_utf8_lossy(&body);
    assert!(text.contains("\"access_key_id\":\"admin\"") && !text.contains("unique-test-secret"));
    let response = app
        .clone()
        .oneshot(call("GET", "/api/v1/buckets/minio/objects", ""))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let response = app
        .clone()
        .oneshot(call(
            "PUT",
            "/api/v1/buckets/minio",
            r#"{"id":"other","name":"farm","access_key_id":"a"}"#,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let response = app
        .clone()
        .oneshot(call("DELETE", "/api/v1/buckets/minio", ""))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert!(
        manager
            .export_snapshot(
                "clean",
                ExportRequest {
                    bucket: Some("minio".into()),
                    ..Default::default()
                }
            )
            .unwrap_err()
            .is::<BadRequest>()
    );
    // Objects stay in the bucket.
    assert!(s3.objects.lock().unwrap().contains_key("farm/clean.msb"));
    settle(&manager).await;
}

#[tokio::test]
async fn settle_rechecks_a_restored_farm_without_redeploying() {
    let dir = tempfile::tempdir().unwrap();
    let backend = Arc::new(Fake::default());
    let manager = open_with(options(dir.path()), backend.clone()).await;
    done(&manager, &manager.create(request("src")).unwrap()).await;
    assert!(
        manager
            .settle("src")
            .unwrap_err()
            .to_string()
            .contains("no successful deployment")
    );
    done(
        &manager,
        &manager.deploy("src", DeployRequest::default()).unwrap(),
    )
    .await;
    let freezer = manager
        .freeze(
            "src",
            SnapshotRequest {
                id: "frozen".into(),
                note: None,
                node: None,
            },
        )
        .unwrap();
    done(&manager, &freezer).await;
    let mut copy = request("copy");
    copy.snapshot_id = Some("frozen".into());
    done(&manager, &manager.create(copy).unwrap()).await;
    assert_eq!(manager.get("copy").unwrap().status, FarmStatus::Ready);

    // A restart is observed as broken, then created; the check restores Ready without deploying.
    backend.farms.lock().unwrap().get_mut("copy").unwrap()[0].status = "stopped".into();
    manager.reconcile().await.unwrap();
    assert_eq!(manager.get("copy").unwrap().status, FarmStatus::Broken);
    backend.farms.lock().unwrap().get_mut("copy").unwrap()[0].status = "running".into();
    manager.reconcile().await.unwrap();
    assert_eq!(manager.get("copy").unwrap().status, FarmStatus::Created);
    let deploys = backend.deploys.load(Ordering::SeqCst);
    let job = manager.settle("copy").unwrap();
    assert_eq!(job.kind, JobKind::Settle);
    assert_eq!(job.phase, Some(Phase::Verify));
    assert!(manager.settle("copy").unwrap_err().is::<Conflict>());
    assert_eq!(done(&manager, &job).await.status, JobStatus::Succeeded);
    assert_eq!(manager.get("copy").unwrap().status, FarmStatus::Ready);
    assert_eq!(backend.deploys.load(Ordering::SeqCst), deploys);
    assert!(matches!(backend.settled.lock().unwrap().last(), Some(None)));

    backend.fail.store(true, Ordering::SeqCst);
    let failed = manager.settle("copy").unwrap();
    assert_eq!(done(&manager, &failed).await.status, JobStatus::Failed);
    assert_eq!(manager.get("copy").unwrap().status, FarmStatus::Created);
    settle(&manager).await;
}

#[test]
fn port_blocks_never_overlap_even_when_the_block_width_grows() {
    // The current grid: ten-apart bases do not share ports.
    assert!(!blocks_overlap(15000, 15010, 10));
    assert!(blocks_overlap(15000, 15000, 10));
    // A base inside another farm's range is rejected, so widening the block is a safe change
    // as long as the grid and the overlap test agree.
    assert!(blocks_overlap(15000, 15005, 10));
    assert!(blocks_overlap(15008, 15000, 10));
    // A wider block shrinks the grid; neighbours must still stay clear of each other.
    assert!(blocks_overlap(15000, 15016, 20));
    assert!(!blocks_overlap(15000, 15020, 20));
}

#[tokio::test]
async fn purging_a_host_deletes_its_farms_and_leftover_vms_then_unregisters_it() {
    use crate::remote::{AgentArtifactInfo, HostJobKind, InstalledAgent, test_server::Fixture};
    let fixture = Fixture::start().await;
    let dir = tempfile::tempdir().unwrap();
    let manager = open_with(options(dir.path()), Arc::new(Fake::default())).await;
    manager
        .register_host(fixture.registration("target"))
        .unwrap();
    let prep = manager
        .start_host_job("target", HostJobKind::Prepare)
        .unwrap();
    assert_eq!(
        host_done(&manager, &prep.id).await.status,
        JobStatus::Succeeded
    );
    manager
        .update(|state| {
            state.hosts.get_mut("target").unwrap().host.agent = Some(InstalledAgent {
                artifact: AgentArtifactInfo {
                    sha256: "a".repeat(64),
                    os: "linux".into(),
                    arch: std::env::consts::ARCH.into(),
                    protocol: 1,
                    supports_single_vm: true,
                    build_id: crate::BUILD_ID.into(),
                },
                verified_at: now(),
            });
            Ok(())
        })
        .unwrap();

    let mut remote = request("remote-farm");
    remote.host_id = Some("target".into());
    remote.external_host = "target.example".into();
    done(&manager, &manager.create(remote).unwrap()).await;
    // A local farm must survive a purge of the remote host.
    done(&manager, &manager.create(request("local-farm")).unwrap()).await;

    // Plain unregister still refuses while a farm lives on the host.
    assert!(
        manager
            .unregister_host("target")
            .unwrap_err()
            .to_string()
            .contains("delete all farms")
    );

    let purge = manager
        .start_host_job("target", HostJobKind::Purge)
        .unwrap();
    let finished = host_done(&manager, &purge.id).await;
    assert_eq!(
        finished.status,
        JobStatus::Succeeded,
        "{:?} {:?}",
        finished.error,
        finished.events
    );
    assert!(manager.host("target").is_err());
    // Only the purge job remains, so the caller can still read its result.
    let jobs = manager.host_jobs().unwrap();
    assert_eq!(jobs.len(), 1);
    assert!(matches!(jobs[0].kind, HostJobKind::Purge));
    assert_eq!(
        manager.get("remote-farm").unwrap().status,
        FarmStatus::Deleted
    );
    assert_eq!(
        manager.get("local-farm").unwrap().status,
        FarmStatus::Created
    );
    // The agent was asked to drop what it still reserved for this coordinator.
    assert!(
        jobs[0]
            .events
            .iter()
            .any(|event| event.message.contains("reserved VM(s)")),
        "{:?}",
        jobs[0].events
    );
    assert_eq!(jobs[0].status, JobStatus::Succeeded);
    settle(&manager).await;
}

#[tokio::test]
async fn forgetting_removes_records_without_asking_the_host() {
    let dir = tempfile::tempdir().unwrap();
    let backend = Arc::new(Fake::default());
    let manager = open_with(options(dir.path()), backend.clone()).await;
    done(&manager, &manager.create(request("src")).unwrap()).await;
    done(
        &manager,
        &manager.deploy("src", DeployRequest::default()).unwrap(),
    )
    .await;
    let freeze = manager
        .freeze(
            "src",
            SnapshotRequest {
                id: "clean".into(),
                note: None,
                node: None,
            },
        )
        .unwrap();
    assert_eq!(done(&manager, &freeze).await.status, JobStatus::Succeeded);

    let forget = manager.forget("src").unwrap();
    assert!(forget.forget);
    assert_eq!(done(&manager, &forget).await.status, JobStatus::Succeeded);
    assert_eq!(manager.get("src").unwrap().status, FarmStatus::Deleted);
    // The host was not asked, so its VM is still there.
    assert!(backend.farms.lock().unwrap().contains_key("src"));
    assert!(
        manager
            .logs("src", 0, 1000)
            .unwrap()
            .iter()
            .any(|line| line.message.contains("record only"))
    );

    // Without its host, a snapshot can only be forgotten.
    manager.unregister_host("local").unwrap();
    assert!(
        manager
            .delete_snapshot("clean")
            .await
            .unwrap_err()
            .to_string()
            .contains("register the local host")
    );
    manager.forget_snapshot("clean").await.unwrap();
    assert!(manager.snapshot("clean").unwrap_err().is::<NotFound>());
    assert!(backend.snapshots.lock().unwrap().contains_key("fake/clean"));
    settle(&manager).await;
}

#[tokio::test]
async fn purging_the_local_host_can_also_delete_its_snapshots() {
    use crate::remote::{HostJobKind, test_server::Fixture};
    let fixture = Fixture::start().await;
    let dir = tempfile::tempdir().unwrap();
    let backend = Arc::new(Fake::default());
    let manager = open_with(options(dir.path()), backend.clone()).await;
    done(&manager, &manager.create(request("src")).unwrap()).await;
    done(
        &manager,
        &manager.deploy("src", DeployRequest::default()).unwrap(),
    )
    .await;
    let freeze = manager
        .freeze(
            "src",
            SnapshotRequest {
                id: "clean".into(),
                note: None,
                node: None,
            },
        )
        .unwrap();
    assert_eq!(done(&manager, &freeze).await.status, JobStatus::Succeeded);

    // Only the local host keeps snapshots, so a remote purge cannot take them.
    manager
        .register_host(fixture.registration("target"))
        .unwrap();
    assert!(
        manager
            .start_purge("target", true)
            .unwrap_err()
            .is::<BadRequest>()
    );
    manager.unregister_host("target").unwrap();

    let purge = manager.start_purge("local", true).unwrap();
    assert!(purge.delete_snapshots);
    let finished = host_done(&manager, &purge.id).await;
    assert_eq!(
        finished.status,
        JobStatus::Succeeded,
        "{:?} {:?}",
        finished.error,
        finished.events
    );
    assert!(matches!(finished.kind, HostJobKind::Purge));
    assert!(manager.host("local").is_err());
    assert_eq!(manager.get("src").unwrap().status, FarmStatus::Deleted);
    assert!(manager.snapshot("clean").unwrap_err().is::<NotFound>());
    assert!(!backend.snapshots.lock().unwrap().contains_key("fake/clean"));
    assert!(
        finished
            .events
            .iter()
            .any(|event| event.message == "Deleting 1 snapshot(s)"),
        "{:?}",
        finished.events
    );
    settle(&manager).await;
}

#[tokio::test]
async fn a_restarted_daemon_reinstalls_agents_another_build_installed() {
    use crate::remote::{AgentArtifactInfo, HostJobKind, InstalledAgent, test_server::Fixture};
    let fixtures = [Fixture::start().await, Fixture::start().await];
    let dir = tempfile::tempdir().unwrap();
    // A Linux ELF header for this machine's architecture, stamped with this build.
    let mut elf = vec![0u8; 64];
    elf[..6].copy_from_slice(b"\x7fELF\x02\x01");
    let machine: u16 = if std::env::consts::ARCH == "aarch64" {
        183
    } else {
        62
    };
    elf[18..20].copy_from_slice(&machine.to_le_bytes());
    elf.extend(
        format!(
            "{}{}\0",
            crate::remote::agent::BUILD_STAMP_PREFIX,
            crate::BUILD_ID
        )
        .bytes(),
    );
    let artifact = dir.path().join("farm-agent");
    std::fs::write(&artifact, elf).unwrap();
    let mut options = options(&dir.path().join("state"));
    options.agent_binary = Some(artifact);
    let manager = open_with(options, Arc::new(Fake::default())).await;
    for (fixture, id) in fixtures.iter().zip(["stale", "current"]) {
        manager.register_host(fixture.registration(id)).unwrap();
        let prep = manager.start_host_job(id, HostJobKind::Prepare).unwrap();
        host_done(&manager, &prep.id).await;
    }
    let installed = |build_id: &str| InstalledAgent {
        artifact: AgentArtifactInfo {
            sha256: "a".repeat(64),
            os: "linux".into(),
            arch: std::env::consts::ARCH.into(),
            protocol: 1,
            supports_single_vm: true,
            build_id: build_id.into(),
        },
        verified_at: now(),
    };
    manager
        .update(|state| {
            state.hosts.get_mut("stale").unwrap().host.agent = Some(installed("older-build"));
            state.hosts.get_mut("current").unwrap().host.agent = Some(installed(crate::BUILD_ID));
            Ok(())
        })
        .unwrap();

    let jobs = manager.upgrade_stale_agents().unwrap();
    // Neither the up-to-date host nor the local host, which runs the coordinator itself.
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].host_id, "stale");
    assert!(matches!(jobs[0].kind, HostJobKind::InstallAgent));
    host_done(&manager, &jobs[0].id).await;
    settle(&manager).await;
}

#[tokio::test]
async fn deploying_a_remote_farm_moves_it_to_the_hosts_current_agent() {
    use crate::remote::{AgentArtifactInfo, HostJobKind, InstalledAgent, test_server::Fixture};
    let fixture = Fixture::start().await;
    let dir = tempfile::tempdir().unwrap();
    let manager = open_with(options(dir.path()), Arc::new(Fake::default())).await;
    manager
        .register_host(fixture.registration("target"))
        .unwrap();
    let prep = manager
        .start_host_job("target", HostJobKind::Prepare)
        .unwrap();
    host_done(&manager, &prep.id).await;
    let agent = |sha: &str| InstalledAgent {
        artifact: AgentArtifactInfo {
            sha256: sha.into(),
            os: "linux".into(),
            arch: std::env::consts::ARCH.into(),
            protocol: 1,
            supports_single_vm: true,
            build_id: crate::BUILD_ID.into(),
        },
        verified_at: now(),
    };
    let old = "a".repeat(64);
    manager
        .update(|state| {
            state.hosts.get_mut("target").unwrap().host.agent = Some(agent(&old));
            Ok(())
        })
        .unwrap();
    let mut remote = request("remote-farm");
    remote.host_id = Some("target".into());
    remote.external_host = "target.example".into();
    done(&manager, &manager.create(remote).unwrap()).await;
    assert_eq!(manager.get("remote-farm").unwrap().agent_sha256, Some(old));

    // The operator reinstalls the agent; the next deploy follows the host instead of the pin.
    let new = "b".repeat(64);
    manager
        .update(|state| {
            state.hosts.get_mut("target").unwrap().host.agent = Some(agent(&new));
            Ok(())
        })
        .unwrap();
    let deploy = manager
        .deploy("remote-farm", DeployRequest::default())
        .unwrap();
    assert_eq!(done(&manager, &deploy).await.status, JobStatus::Succeeded);
    assert_eq!(
        manager.get("remote-farm").unwrap().agent_sha256,
        Some(new.clone())
    );
    assert!(
        manager
            .logs("remote-farm", 0, 100)
            .unwrap()
            .iter()
            .any(|entry| entry.message.contains("host's current agent"))
    );

    // An agent built elsewhere is still refused, so a stale install cannot be pinned silently.
    let stale = InstalledAgent {
        artifact: AgentArtifactInfo {
            sha256: "c".repeat(64),
            os: "linux".into(),
            arch: std::env::consts::ARCH.into(),
            protocol: 1,
            supports_single_vm: true,
            build_id: "other-build".into(),
        },
        verified_at: now(),
    };
    manager
        .update(|state| {
            state.hosts.get_mut("target").unwrap().host.agent = Some(stale);
            Ok(())
        })
        .unwrap();
    let error = manager
        .deploy("remote-farm", DeployRequest::default())
        .unwrap_err()
        .to_string();
    assert!(error.contains("reinstall it from Hosts"), "{error}");
    assert_eq!(manager.get("remote-farm").unwrap().agent_sha256, Some(new));
    settle(&manager).await;
}

#[tokio::test]
async fn this_machine_registers_once_without_credentials_and_runs_farms_through_its_agent() {
    use crate::remote::{HostJobKind, HostKind, RegisterLocalHost};
    let dir = tempfile::tempdir().unwrap();
    let backend = Arc::new(Fake::default());
    let manager = FarmManager::with_backend(options(dir.path()), backend.clone())
        .await
        .unwrap();
    // No host yet: a farm has nowhere to run.
    assert!(
        manager
            .create(request("early"))
            .unwrap_err()
            .is::<NotFound>()
    );
    let mut hostless = request("early");
    hostless.host_id = None;
    assert!(manager.create(hostless).unwrap_err().is::<BadRequest>());
    let host = manager
        .register_local_host(RegisterLocalHost { id: "local".into() })
        .unwrap();
    assert_eq!(host.kind, HostKind::Local);
    assert!(host.auth_method.is_none());
    for id in ["local", "other"] {
        assert!(
            manager
                .register_local_host(RegisterLocalHost { id: id.into() })
                .unwrap_err()
                .is::<Conflict>()
        );
    }
    // Registered but not prepared.
    assert!(
        manager
            .create(request("early"))
            .unwrap_err()
            .is::<Conflict>()
    );
    assert!(
        manager
            .start_host_job("local", HostJobKind::InstallAgent)
            .unwrap_err()
            .is::<Conflict>()
    );
    local_host(&manager);
    done(&manager, &manager.create(request("one")).unwrap()).await;
    let farm = manager.get("one").unwrap();
    assert_eq!(farm.host_id.as_deref(), Some("local"));
    assert!(farm.agent_sha256.is_none() && farm.remote_generation.is_some());
    assert_eq!(farm.exposure.unwrap().bind, options(dir.path()).public_bind);
    // The agent keeps its own reservation, so a scan reports the farm back.
    manager.reconcile().await.unwrap();
    assert_eq!(manager.get("one").unwrap().status, FarmStatus::Created);
    assert!(
        manager
            .unregister_host("local")
            .unwrap_err()
            .is::<Conflict>()
    );
    done(&manager, &manager.delete("one").unwrap()).await;
    assert!(backend.farms.lock().unwrap().is_empty());
    manager.unregister_host("local").unwrap();
    settle(&manager).await;
}

#[tokio::test]
async fn a_dial_in_host_runs_farms_and_shells_over_its_own_connection() {
    use crate::remote::{HostKind, RegisterAgentHost, agent::Agent, dial};
    let dir = tempfile::tempdir().unwrap();
    // The coordinator holds no VMs itself; the dialling agent has its own fake runtime.
    let manager = FarmManager::with_backend(options(dir.path()), Arc::new(Fake::default()))
        .await
        .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(
        axum::serve(
            listener,
            api::router(manager.clone(), Some("api-token".into())),
        )
        .into_future(),
    );
    let registered = manager
        .register_agent_host(RegisterAgentHost { id: "edge".into() })
        .unwrap();
    assert_eq!(registered.host.kind, HostKind::Agent);
    // The API token is not an agent token, and neither is anything else.
    let url = dial::connect_url(&format!("http://{address}")).unwrap();
    let remote = Arc::new(Fake::default());
    let agent = Arc::new(Agent::in_process(
        dir.path().join("edge-agent"),
        remote.clone(),
    ));
    for token in ["api-token", "farm_wrong"] {
        assert!(dial::serve(&url, token, agent.clone()).await.is_err());
    }
    let dialling = tokio::spawn({
        let (url, token, agent) = (url.clone(), registered.token.clone(), agent.clone());
        async move { dial::serve(&url, &token, agent).await }
    });
    tokio::time::timeout(Duration::from_secs(5), async {
        while !manager.host("edge").unwrap().connected {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    // A check asks the host's own agent; the coordinator inspects nothing itself.
    let check = manager
        .start_host_job("edge", crate::remote::HostJobKind::Check)
        .unwrap();
    let check = host_done(&manager, &check.id).await;
    assert_eq!(check.status, JobStatus::Succeeded, "{:?}", check.error);
    assert!(manager.host("edge").unwrap().inspection.is_some());
    manager
        .update(|state| {
            let host = &mut state.hosts.get_mut("edge").unwrap().host;
            host.status = crate::remote::HostStatus::Ready;
            host.inspection.as_mut().unwrap().memory_total_mib = 65536;
            Ok(())
        })
        .unwrap();
    let mut on_edge = request("edge-farm");
    on_edge.host_id = Some("edge".into());
    let created = manager.create(on_edge).unwrap();
    assert_eq!(done(&manager, &created).await.status, JobStatus::Succeeded);
    assert!(remote.farms.lock().unwrap().contains_key("edge-farm"));
    let farm = manager.get("edge-farm").unwrap();
    assert_eq!(farm.exposure.unwrap().bind.to_string(), "0.0.0.0");
    // A shell travels over the same socket.
    let (input, input_rx) = tokio::sync::mpsc::channel(4);
    let (output_tx, mut output) = tokio::sync::mpsc::channel(4);
    let shell = manager.terminal(
        "edge-farm",
        None,
        crate::farm::TerminalSize { rows: 24, cols: 80 },
        crate::farm::TerminalIo {
            input: input_rx,
            output: output_tx,
        },
    );
    let client = async {
        assert_eq!(output.recv().await.unwrap(), b"24x80");
        input
            .send(crate::farm::TerminalInput::Data(b"ls\r".to_vec()))
            .await
            .unwrap();
        assert_eq!(output.recv().await.unwrap(), b"ls\r");
        drop(input);
    };
    let (shell, ()) = tokio::join!(shell, client);
    shell.unwrap();
    // Without its connection the host takes no new farms.
    dialling.abort();
    tokio::time::timeout(Duration::from_secs(5), async {
        while manager.host("edge").unwrap().connected {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    let mut later = request("later");
    later.host_id = Some("edge".into());
    assert!(manager.create(later).unwrap_err().is::<Conflict>());
    server.abort();
    settle(&manager).await;
}

#[tokio::test]
async fn a_released_build_installs_agents_it_never_loaded() {
    use sha2::{Digest, Sha256};
    // A Windows PE header stamped with this build: no local artifact covers it.
    let mut agent = b"MZ".to_vec();
    agent.resize(0x40, 0);
    agent[0x3c..0x40].copy_from_slice(&0x40u32.to_le_bytes());
    agent.extend(b"PE\0\0");
    agent.extend(0x8664u16.to_le_bytes());
    agent.extend(
        format!(
            "{}{}\0",
            crate::remote::agent::BUILD_STAMP_PREFIX,
            crate::BUILD_ID
        )
        .bytes(),
    );
    let sha256 = format!("{:x}", Sha256::digest(&agent));
    let path = format!("{}/windows-x86_64/farm-agent.exe", crate::BUILD_ID);
    let manifest = serde_json::json!({
        "build_id": crate::BUILD_ID,
        "version": "0.0.0",
        "agents": {"windows-x86_64": {"path": path, "sha256": sha256, "size": agent.len()}},
    });
    let served = Arc::new(Mutex::new(agent.clone()));
    let downloads = Arc::new(AtomicUsize::new(0));
    let app = axum::Router::new()
        .route(
            &format!("/farm-agent/{}/manifest.json", crate::BUILD_ID),
            axum::routing::get(move || async move { axum::Json(manifest) }),
        )
        .route(
            &format!("/farm-agent/{path}"),
            axum::routing::get({
                let (served, downloads) = (served.clone(), downloads.clone());
                move || async move {
                    downloads.fetch_add(1, Ordering::SeqCst);
                    served.lock().unwrap().clone()
                }
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/farm-agent", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await });

    let dir = tempfile::tempdir().unwrap();
    let mut released = options(dir.path());
    released.agent_url = Some(url.clone());
    // A local agent of another build does not hide the published one.
    let stale = dir.path().join("stale.exe");
    let marker = agent.len() - crate::BUILD_ID.len() - 1;
    let mut stale_agent = agent[..marker].to_vec();
    stale_agent.extend(b"old-build\0");
    std::fs::write(&stale, stale_agent).unwrap();
    let mut with_stale = released.clone();
    with_stale.agent_binary = Some(stale);
    let manager = open_with(with_stale, Arc::new(Fake::default())).await;
    assert!(!manager.has_current_agent("windows-x86_64"));
    assert_eq!(manager.load_published_agents().await.unwrap(), 1);
    assert!(manager.has_current_agent("windows-x86_64"));
    assert!(!manager.has_current_agent("linux-x86_64"));
    assert!(
        manager
            .agent_artifacts()
            .iter()
            .any(|info| info.platform() == "windows-x86_64" && info.sha256 == sha256)
    );
    let fetched = manager.artifact_for("windows-x86_64").await.unwrap();
    assert_eq!(&fetched.bytes[..], &agent[..]);
    manager.artifact_for("windows-x86_64").await.unwrap();
    assert_eq!(downloads.load(Ordering::SeqCst), 1);
    let cached = dir
        .path()
        .join("agents")
        .join(&sha256)
        .join("farm-agent.exe");
    assert_eq!(std::fs::read(&cached).unwrap(), agent);

    settle(&manager).await;
    drop(manager);

    // A restarted coordinator reuses the download; a tampered one is fetched again and refused.
    let restarted = open_with(released.clone(), Arc::new(Fake::default())).await;
    restarted.load_published_agents().await.unwrap();
    restarted.artifact_for("windows-x86_64").await.unwrap();
    assert_eq!(downloads.load(Ordering::SeqCst), 1);
    settle(&restarted).await;
    drop(restarted);
    std::fs::write(&cached, b"tampered").unwrap();
    served.lock().unwrap().push(0);
    let other = open_with(released.clone(), Arc::new(Fake::default())).await;
    other.load_published_agents().await.unwrap();
    let error = other.artifact_for("windows-x86_64").await.err().unwrap();
    assert!(
        format!("{error:#}").contains("published size and sha256"),
        "{error:#}"
    );
    assert_eq!(downloads.load(Ordering::SeqCst), 2);
    assert!(other.artifact_for("macos-aarch64").await.is_err());

    settle(&other).await;
    drop(other);

    // A build that was never released publishes nothing.
    released.agent_url = Some(format!("{url}/missing"));
    let unreleased = open_with(released, Arc::new(Fake::default())).await;
    assert_eq!(unreleased.load_published_agents().await.unwrap(), 0);
    settle(&unreleased).await;
}

/// Records a running deploy of `farm` as a coordinator that stopped mid-deploy leaves it,
/// with the host-side job `remote` of which `frames` progress frames were recorded.
fn running_deploy(manager: &FarmManager, farm: &str, remote: &str, frames: u64) -> Job {
    manager
        .update(|state| {
            let mut job = new_job(farm, JobKind::Deploy);
            job.status = JobStatus::Running;
            job.phase = Some(Phase::Prepare);
            job.started_at = Some(now());
            job.remote_job_id = Some(remote.into());
            job.remote_frames = frames;
            let record = state.farms.get_mut(farm).unwrap();
            record.status = FarmStatus::Deploying;
            record.active_job = Some(job.id.clone());
            record.phase = job.phase;
            state.jobs.insert(job.id.clone(), job.clone());
            Ok(job)
        })
        .unwrap()
}
fn count(manager: &FarmManager, farm: &str, message: &str) -> usize {
    manager
        .logs(farm, 0, 1000)
        .unwrap()
        .iter()
        .filter(|entry| entry.message.contains(message))
        .count()
}

#[tokio::test]
async fn a_restarted_coordinator_reattaches_to_the_deploy_its_host_kept_running() {
    use crate::remote::{agent::Agent, protocol};
    let dir = tempfile::tempdir().unwrap();
    let backend = Arc::new(Fake {
        deployment_gate: Some(Semaphore::new(0)),
        ..Default::default()
    });
    let manager = open_with(options(dir.path()), backend.clone()).await;
    done(&manager, &manager.create(request("resume")).unwrap()).await;
    let coordinator_id = manager.0.store.lock().unwrap().state.coordinator_id.clone();
    let generation = manager.get("resume").unwrap().remote_generation.unwrap();
    // The coordinator recorded the deploy, had applied its first frame and stopped...
    let remote = uuid::Uuid::new_v4().to_string();
    let job = running_deploy(&manager, "resume", &remote, 1);
    // ...while the host's agent went on with it after the request was gone.
    let agent = Arc::new(Agent::in_process(dir.path().join("agent"), backend.clone()));
    let requester = tokio::spawn({
        let request = protocol::Request {
            version: protocol::VERSION,
            coordinator_id,
            operation: protocol::Operation::Deploy {
                farm_id: "resume".into(),
                generation,
                from: Phase::Prepare,
                job_id: remote.clone(),
            },
        };
        async move { agent.execute(request, Arc::new(|_| Ok(()))).await }
    });
    tokio::time::timeout(Duration::from_secs(5), async {
        while backend.deploys.load(Ordering::SeqCst) == 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    requester.abort();
    // Stopping does not wait for a deploy; it goes on without the coordinator.
    tokio::time::timeout(Duration::from_secs(5), manager.shutdown())
        .await
        .unwrap()
        .unwrap();
    drop(manager);

    let manager = open_with(options(dir.path()), backend.clone()).await;
    assert_eq!(manager.job(&job.id).unwrap().status, JobStatus::Running);
    assert_eq!(manager.get("resume").unwrap().status, FarmStatus::Deploying);
    assert_eq!(
        count(&manager, "resume", "re-attaching to the running deploy"),
        1
    );
    backend.deployment_gate.as_ref().unwrap().add_permits(1);
    let finished = done(&manager, &job).await;
    assert_eq!(
        finished.status,
        JobStatus::Succeeded,
        "{:?}",
        finished.error
    );
    assert_eq!(finished.remote_frames, 3);
    assert_eq!(finished.started_at, job.started_at);
    let farm = manager.get("resume").unwrap();
    assert_eq!(farm.status, FarmStatus::Ready);
    assert!(farm.active_job.is_none() && farm.step.is_none() && farm.error.is_none());
    // Only the frames recorded after the restart are applied, each once, from one deploy.
    assert_eq!(count(&manager, "resume", "test deployment output"), 1);
    assert_eq!(count(&manager, "resume", "step: make images"), 1);
    assert_eq!(count(&manager, "resume", "phase prepare"), 0);
    assert_eq!(backend.deployed.load(Ordering::SeqCst), 1);
    assert!(manager.endpoints("resume").await.is_ok());
    settle(&manager).await;
}

#[tokio::test]
async fn a_deploy_whose_host_stays_unreachable_after_a_restart_is_interrupted() {
    let dir = tempfile::tempdir().unwrap();
    let manager = open_with(options(dir.path()), Arc::new(Fake::default())).await;
    manager
        .register_agent_host(crate::remote::RegisterAgentHost { id: "edge".into() })
        .unwrap();
    done(&manager, &manager.create(request("far")).unwrap()).await;
    manager
        .update(|state| {
            state.farms.get_mut("far").unwrap().host_id = Some("edge".into());
            Ok(())
        })
        .unwrap();
    let job = running_deploy(&manager, "far", &uuid::Uuid::new_v4().to_string(), 0);
    settle(&manager).await;
    drop(manager);

    // The dial-in agent never connects again.
    let manager = open_with(options(dir.path()), Arc::new(Fake::default())).await;
    let finished = tokio::time::timeout(Duration::from_secs(15), manager.wait(&job.id))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(finished.status, JobStatus::Interrupted);
    assert!(
        finished
            .error
            .as_deref()
            .unwrap()
            .contains("may still be running on the host"),
        "{:?}",
        finished.error
    );
    let farm = manager.get("far").unwrap();
    assert_eq!(farm.status, FarmStatus::Failed);
    assert!(farm.active_job.is_none());
    settle(&manager).await;
}

#[tokio::test]
async fn a_restarted_coordinator_reattaches_to_a_remote_deploy_over_ssh() {
    use crate::remote::{AgentArtifactInfo, HostJobKind, InstalledAgent, test_server::Fixture};
    let fixture = Fixture::start().await;
    let dir = tempfile::tempdir().unwrap();
    let manager = open_with(options(dir.path()), Arc::new(Fake::default())).await;
    manager
        .register_host(fixture.registration("target"))
        .unwrap();
    let prep = manager
        .start_host_job("target", HostJobKind::Prepare)
        .unwrap();
    host_done(&manager, &prep.id).await;
    manager
        .update(|state| {
            state.hosts.get_mut("target").unwrap().host.agent = Some(InstalledAgent {
                artifact: AgentArtifactInfo {
                    sha256: "a".repeat(64),
                    os: "linux".into(),
                    arch: std::env::consts::ARCH.into(),
                    protocol: 1,
                    supports_single_vm: true,
                    build_id: crate::BUILD_ID.into(),
                },
                verified_at: now(),
            });
            Ok(())
        })
        .unwrap();
    let mut remote = request("remote-farm");
    remote.host_id = Some("target".into());
    remote.external_host = "target.example".into();
    done(&manager, &manager.create(remote).unwrap()).await;
    let remote_job = uuid::Uuid::new_v4().to_string();
    let job = running_deploy(&manager, "remote-farm", &remote_job, 5);
    settle(&manager).await;
    drop(manager);

    let manager = open_with(options(dir.path()), Arc::new(Fake::default())).await;
    let finished = done(&manager, &job).await;
    assert_eq!(
        finished.status,
        JobStatus::Succeeded,
        "{:?}",
        finished.error
    );
    assert_eq!(finished.remote_frames, 6);
    assert!(
        fixture
            .observed
            .calls
            .lock()
            .unwrap()
            .contains(&format!("attach:{remote_job}:5"))
    );
    assert_eq!(
        count(&manager, "remote-farm", "remote deploy re-attached output"),
        1
    );
    assert_eq!(
        manager.get("remote-farm").unwrap().status,
        FarmStatus::Ready
    );
    settle(&manager).await;
}

/// The local agent, whose first deploy request loses its connection after the first frame.
struct DroppingLink {
    inner: crate::remote::link::InProcessLink,
    dropped: AtomicBool,
    attaches: AtomicUsize,
}
impl crate::remote::link::AgentLink for DroppingLink {
    fn call(
        &self,
        request: crate::remote::protocol::Request,
        sink: Option<EventSink>,
    ) -> BoxFuture<'_, Result<crate::remote::protocol::Reply>> {
        use crate::remote::protocol::Operation;
        Box::pin(async move {
            match request.operation {
                Operation::Deploy { .. } if !self.dropped.swap(true, Ordering::SeqCst) => {
                    let sink = sink.unwrap();
                    let (first, applied) = tokio::sync::oneshot::channel();
                    let first = Mutex::new(Some(first));
                    let forward: EventSink = Arc::new(move |event| {
                        sink(event)?;
                        if let Some(first) = first.lock().unwrap().take() {
                            let _ = first.send(());
                        }
                        Ok(())
                    });
                    tokio::select! {
                        _ = self.inner.call(request, Some(forward)) => bail!("the deploy ended early"),
                        _ = applied => bail!("connection reset by peer"),
                    }
                }
                Operation::Attach { .. } => {
                    self.attaches.fetch_add(1, Ordering::SeqCst);
                }
                _ => {}
            }
            self.inner.call(request, sink).await
        })
    }
    fn terminal(
        &self,
        request: crate::remote::protocol::Request,
        io: crate::farm::TerminalIo,
    ) -> BoxFuture<'_, Result<()>> {
        self.inner.terminal(request, io)
    }
}

#[tokio::test]
async fn a_deploy_that_loses_its_connection_follows_the_host_job_again() {
    let dir = tempfile::tempdir().unwrap();
    let backend = Arc::new(Fake {
        deployment_gate: Some(Semaphore::new(0)),
        ..Default::default()
    });
    let link = Arc::new(DroppingLink {
        inner: crate::remote::link::InProcessLink {
            agent: Arc::new(crate::remote::agent::Agent::in_process(
                dir.path().join("agent"),
                backend.clone(),
            )),
        },
        dropped: AtomicBool::new(false),
        attaches: AtomicUsize::new(0),
    });
    let manager = FarmManager::with_link(options(dir.path()), link.clone())
        .await
        .unwrap();
    local_host(&manager);
    done(&manager, &manager.create(request("flaky")).unwrap()).await;
    let job = manager.deploy("flaky", DeployRequest::default()).unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while link.attaches.load(Ordering::SeqCst) == 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(manager.job(&job.id).unwrap().status, JobStatus::Running);
    backend.deployment_gate.as_ref().unwrap().add_permits(1);
    let finished = done(&manager, &job).await;
    assert_eq!(
        finished.status,
        JobStatus::Succeeded,
        "{:?}",
        finished.error
    );
    assert_eq!(finished.remote_frames, 3);
    assert_eq!(count(&manager, "flaky", "lost the connection"), 1);
    assert_eq!(count(&manager, "flaky", "test deployment output"), 1);
    assert_eq!(count(&manager, "flaky", "step: make images"), 1);
    assert_eq!(backend.deployed.load(Ordering::SeqCst), 1);
    assert_eq!(manager.get("flaky").unwrap().status, FarmStatus::Ready);
    settle(&manager).await;
}

#[tokio::test]
async fn a_deployed_farm_can_gain_a_compute_on_the_same_host() {
    let dir = tempfile::tempdir().unwrap();
    let backend = Arc::new(Fake::default());
    let manager = open_with(options(dir.path()), backend.clone()).await;
    if !cfg!(target_os = "linux") {
        *manager.0.guest_agent.write().unwrap() = Some(dir.path().join("farm-agent"));
    }
    let created = manager
        .create(CreateRequest {
            computes: 1,
            ..request("grow")
        })
        .unwrap();
    assert_eq!(done(&manager, &created).await.status, JobStatus::Succeeded);
    let deployed = manager.deploy("grow", DeployRequest::default()).unwrap();
    assert_eq!(done(&manager, &deployed).await.status, JobStatus::Succeeded);
    assert_eq!(manager.get("grow").unwrap().status, FarmStatus::Ready);

    let job = manager
        .add_compute("grow", AddComputeRequest::default())
        .unwrap();
    assert_eq!(job.kind, JobKind::Expand);
    assert_eq!(job.target_host.as_deref(), Some("local"));
    assert_eq!(job.target_node.as_deref(), Some("compute-2"));
    let finished = done(&manager, &job).await;
    assert_eq!(
        finished.status,
        JobStatus::Succeeded,
        "{:?}",
        finished.error
    );
    let farm = manager.get("grow").unwrap();
    assert_eq!(farm.status, FarmStatus::Ready);
    assert_eq!(farm.plan.as_ref().unwrap().computes, 2);
    assert_eq!(farm.nodes.len(), 3);
    assert_eq!(farm.host_of("compute-2"), Some("local"));
    assert_eq!(farm.memory_on("local"), farm.memory_mib());
    assert_eq!(
        backend.expanded.lock().unwrap().as_slice(),
        &[(
            "grow".into(),
            vec!["control".into(), "compute-1".into(), "compute-2".into()],
            vec!["compute-2".into()]
        )]
    );
    assert_eq!(
        backend.computed.lock().unwrap().as_slice(),
        &[("grow".into(), 2)]
    );
    // The next deploy sees the expanded layout instead of refusing it.
    let again = manager.deploy("grow", DeployRequest::default()).unwrap();
    assert_eq!(done(&manager, &again).await.status, JobStatus::Succeeded);
}

#[tokio::test]
async fn only_a_deployed_multi_node_farm_can_gain_a_compute() {
    let dir = tempfile::tempdir().unwrap();
    let backend = Arc::new(Fake::default());
    let manager = open_with(options(dir.path()), backend.clone()).await;
    if !cfg!(target_os = "linux") {
        *manager.0.guest_agent.write().unwrap() = Some(dir.path().join("farm-agent"));
    }
    let created = manager.create(request("aio")).unwrap();
    assert_eq!(done(&manager, &created).await.status, JobStatus::Succeeded);
    let deployed = manager.deploy("aio", DeployRequest::default()).unwrap();
    assert_eq!(done(&manager, &deployed).await.status, JobStatus::Succeeded);
    let refused = manager
        .add_compute("aio", AddComputeRequest::default())
        .unwrap_err();
    assert!(refused.to_string().contains("all-in-one"), "{refused}");

    let created = manager
        .create(CreateRequest {
            computes: 1,
            ..request("waiting")
        })
        .unwrap();
    assert_eq!(done(&manager, &created).await.status, JobStatus::Succeeded);
    let refused = manager
        .add_compute("waiting", AddComputeRequest::default())
        .unwrap_err();
    assert!(refused.to_string().contains("deploy the farm"), "{refused}");
}

#[tokio::test]
async fn a_failed_expansion_keeps_the_old_plan_and_resumes_on_retry() {
    let dir = tempfile::tempdir().unwrap();
    let backend = Arc::new(Fake::default());
    let manager = open_with(options(dir.path()), backend.clone()).await;
    if !cfg!(target_os = "linux") {
        *manager.0.guest_agent.write().unwrap() = Some(dir.path().join("farm-agent"));
    }
    let created = manager
        .create(CreateRequest {
            computes: 1,
            ..request("retry")
        })
        .unwrap();
    assert_eq!(done(&manager, &created).await.status, JobStatus::Succeeded);
    let deployed = manager.deploy("retry", DeployRequest::default()).unwrap();
    assert_eq!(done(&manager, &deployed).await.status, JobStatus::Succeeded);
    backend.fail_compute.store(true, Ordering::SeqCst);
    let failed = manager
        .add_compute("retry", AddComputeRequest::default())
        .unwrap();
    let finished = done(&manager, &failed).await;
    assert_eq!(finished.status, JobStatus::Failed);
    let farm = manager.get("retry").unwrap();
    assert_eq!(farm.status, FarmStatus::Failed);
    // The VM exists, but the plan still describes the deployed layout.
    assert_eq!(farm.plan.as_ref().unwrap().computes, 1);
    assert_eq!(farm.nodes.len(), 3);

    backend.fail_compute.store(false, Ordering::SeqCst);
    let retried = manager
        .add_compute("retry", AddComputeRequest::default())
        .unwrap();
    assert_eq!(retried.target_node.as_deref(), Some("compute-2"));
    let finished = done(&manager, &retried).await;
    assert_eq!(
        finished.status,
        JobStatus::Succeeded,
        "{:?}",
        finished.error
    );
    let farm = manager.get("retry").unwrap();
    assert_eq!(farm.status, FarmStatus::Ready);
    assert_eq!(farm.plan.as_ref().unwrap().computes, 2);
    assert_eq!(farm.nodes.len(), 3);
    assert_eq!(manager.jobs().unwrap().len(), 4);
}

#[tokio::test]
async fn a_compute_template_boots_the_next_compute_and_kolla_only_reconfigures_it() {
    let dir = tempfile::tempdir().unwrap();
    let backend = Arc::new(Fake::default());
    let manager = open_with(options(dir.path()), backend.clone()).await;
    if !cfg!(target_os = "linux") {
        *manager.0.guest_agent.write().unwrap() = Some(dir.path().join("farm-agent"));
    }
    let created = manager
        .create(CreateRequest {
            computes: 1,
            ..request("src")
        })
        .unwrap();
    assert_eq!(done(&manager, &created).await.status, JobStatus::Succeeded);
    let template = |node: &str| SnapshotRequest {
        id: "tpl".into(),
        note: None,
        node: Some(node.into()),
    };
    // Only a deployed farm's control or compute VM can become a template.
    assert!(
        manager
            .freeze("src", template("compute-0"))
            .unwrap_err()
            .is::<BadRequest>()
    );
    assert!(
        manager
            .freeze("src", template("control"))
            .unwrap_err()
            .is::<Conflict>()
    );
    assert!(
        manager
            .freeze("src", template("compute-1"))
            .unwrap_err()
            .is::<Conflict>()
    );
    let deployed = manager.deploy("src", DeployRequest::default()).unwrap();
    assert_eq!(done(&manager, &deployed).await.status, JobStatus::Succeeded);
    assert!(
        manager
            .freeze("src", template("compute-2"))
            .unwrap_err()
            .is::<Conflict>()
    );
    let job = manager.freeze("src", template("compute-1")).unwrap();
    assert_eq!(job.target_node.as_deref(), Some("compute-1"));
    assert_eq!(done(&manager, &job).await.status, JobStatus::Succeeded);
    let snapshot = manager.snapshot("tpl").unwrap();
    assert_eq!(snapshot.template, Some(model::TemplateRole::Compute));
    assert!(snapshot.nodes.is_empty());
    assert_eq!(snapshot.reference.as_deref(), Some("fake/tpl"));
    assert_eq!(
        snapshot.release.as_deref(),
        Some(crate::kolla::OPENSTACK_RELEASE)
    );
    let labels = backend.snapshots.lock().unwrap()["fake/tpl"].clone();
    assert_eq!(labels[crate::model::snapshot_labels::ROLE], "compute");
    assert_eq!(labels[crate::model::snapshot_labels::NODE], "compute-1");
    assert_eq!(
        labels[crate::model::snapshot_labels::RELEASE],
        crate::kolla::OPENSTACK_RELEASE
    );
    // Freezing one compute leaves the farm's control plane alone.
    assert!(backend.settled.lock().unwrap().is_empty());
    assert_eq!(manager.get("src").unwrap().status, FarmStatus::Ready);
    // No farm restores from a template.
    assert!(
        manager
            .create(CreateRequest {
                snapshot_id: Some("tpl".into()),
                ..request("nope")
            })
            .unwrap_err()
            .is::<Conflict>()
    );

    let job = manager
        .add_compute(
            "src",
            AddComputeRequest {
                host_id: None,
                from_snapshot: Some("tpl".into()),
            },
        )
        .unwrap();
    assert_eq!(job.snapshot_id.as_deref(), Some("tpl"));
    // A template in use by an expansion stays.
    assert!(manager.delete_snapshot("tpl").await.is_err());
    let finished = done(&manager, &job).await;
    assert_eq!(
        finished.status,
        JobStatus::Succeeded,
        "{:?}",
        finished.error
    );
    let farm = manager.get("src").unwrap();
    assert_eq!(farm.plan.as_ref().unwrap().computes, 2);
    assert_eq!(farm.nodes.len(), 3);
    assert_eq!(
        backend.expanded_from.lock().unwrap().as_slice(),
        &[Some("fake/tpl".to_owned())]
    );
    assert_eq!(
        backend.reconfigured.lock().unwrap().as_slice(),
        &[("src".into(), 2)]
    );
    assert!(backend.computed.lock().unwrap().is_empty());
    // A template travels as an archive and stays a template.
    let archive = dir.path().join("tpl.msb");
    manager
        .export_snapshot(
            "tpl",
            ExportRequest {
                path: Some(archive.display().to_string()),
                ..Default::default()
            },
        )
        .unwrap();
    manager.wait_snapshot("tpl").await.unwrap();
    manager
        .import_snapshot(ImportRequest {
            path: Some(archive.display().to_string()),
            id: Some("tpl-copy".into()),
            ..Default::default()
        })
        .unwrap();
    let imported = manager.wait_snapshot("tpl-copy").await.unwrap();
    assert_eq!(
        imported.status,
        SnapshotStatus::Ready,
        "{:?}",
        imported.error
    );
    assert_eq!(imported.template, Some(model::TemplateRole::Compute));
    assert_eq!(imported.release, snapshot.release);
    assert_eq!(imported.plan.components, snapshot.plan.components);

    // A template must match the farm's OpenStack release and services.
    manager
        .update(|state| {
            state.snapshots.get_mut("tpl").unwrap().release = Some("2024.1".into());
            Ok(())
        })
        .unwrap();
    let refused = manager
        .add_compute(
            "src",
            AddComputeRequest {
                host_id: None,
                from_snapshot: Some("tpl".into()),
            },
        )
        .unwrap_err();
    assert!(refused.is::<Conflict>());
    assert!(refused.to_string().contains("2024.1"), "{refused}");
    manager
        .update(|state| {
            state.snapshots.get_mut("tpl").unwrap().release =
                Some(crate::kolla::OPENSTACK_RELEASE.into());
            Ok(())
        })
        .unwrap();
    let mut other = request("other");
    other.computes = 1;
    other.components = vec!["horizon".into()];
    assert_eq!(
        done(&manager, &manager.create(other).unwrap()).await.status,
        JobStatus::Succeeded
    );
    let deployed = manager.deploy("other", DeployRequest::default()).unwrap();
    assert_eq!(done(&manager, &deployed).await.status, JobStatus::Succeeded);
    let refused = manager
        .add_compute(
            "other",
            AddComputeRequest {
                host_id: None,
                from_snapshot: Some("tpl".into()),
            },
        )
        .unwrap_err();
    assert!(refused.to_string().contains("horizon"), "{refused}");
    // A farm snapshot is not a template.
    let job = manager
        .freeze(
            "other",
            SnapshotRequest {
                id: "whole".into(),
                note: None,
                node: None,
            },
        )
        .unwrap();
    assert_eq!(done(&manager, &job).await.status, JobStatus::Succeeded);
    let refused = manager
        .add_compute(
            "other",
            AddComputeRequest {
                host_id: None,
                from_snapshot: Some("whole".into()),
            },
        )
        .unwrap_err();
    assert!(
        refused.to_string().contains("not a compute template"),
        "{refused}"
    );
}

#[tokio::test]
async fn a_failed_expansion_from_a_template_is_retried_the_same_way() {
    let dir = tempfile::tempdir().unwrap();
    let backend = Arc::new(Fake::default());
    let manager = open_with(options(dir.path()), backend.clone()).await;
    if !cfg!(target_os = "linux") {
        *manager.0.guest_agent.write().unwrap() = Some(dir.path().join("farm-agent"));
    }
    let created = manager
        .create(CreateRequest {
            computes: 1,
            ..request("retry")
        })
        .unwrap();
    assert_eq!(done(&manager, &created).await.status, JobStatus::Succeeded);
    let deployed = manager.deploy("retry", DeployRequest::default()).unwrap();
    assert_eq!(done(&manager, &deployed).await.status, JobStatus::Succeeded);
    let job = manager
        .freeze(
            "retry",
            SnapshotRequest {
                id: "tpl".into(),
                note: None,
                node: Some("compute-1".into()),
            },
        )
        .unwrap();
    assert_eq!(done(&manager, &job).await.status, JobStatus::Succeeded);
    let from_template = AddComputeRequest {
        host_id: None,
        from_snapshot: Some("tpl".into()),
    };
    backend.fail_compute.store(true, Ordering::SeqCst);
    let failed = manager.add_compute("retry", from_template.clone()).unwrap();
    assert_eq!(done(&manager, &failed).await.status, JobStatus::Failed);
    assert_eq!(manager.get("retry").unwrap().plan.unwrap().computes, 1);

    backend.fail_compute.store(false, Ordering::SeqCst);
    // The VM was booted from the template, so a clean retry would not install it.
    let refused = manager
        .add_compute("retry", AddComputeRequest::default())
        .unwrap_err();
    assert!(refused.to_string().contains("'tpl'"), "{refused}");
    let retried = manager.add_compute("retry", from_template).unwrap();
    let finished = done(&manager, &retried).await;
    assert_eq!(
        finished.status,
        JobStatus::Succeeded,
        "{:?}",
        finished.error
    );
    assert_eq!(manager.get("retry").unwrap().plan.unwrap().computes, 2);
    assert_eq!(
        backend.reconfigured.lock().unwrap().as_slice(),
        &[("retry".into(), 2)]
    );
}

#[tokio::test]
async fn a_control_template_starts_a_farm_without_computes_that_expands_clean_or_from_a_template() {
    let dir = tempfile::tempdir().unwrap();
    let backend = Arc::new(Fake::default());
    let manager = open_with(options(dir.path()), backend.clone()).await;
    if !cfg!(target_os = "linux") {
        *manager.0.guest_agent.write().unwrap() = Some(dir.path().join("farm-agent"));
    }
    let created = manager
        .create(CreateRequest {
            computes: 1,
            ..request("src")
        })
        .unwrap();
    assert_eq!(done(&manager, &created).await.status, JobStatus::Succeeded);
    let template = |id: &str, node: &str| SnapshotRequest {
        id: id.into(),
        note: None,
        node: Some(node.into()),
    };
    let deployed = manager.deploy("src", DeployRequest::default()).unwrap();
    assert_eq!(done(&manager, &deployed).await.status, JobStatus::Succeeded);
    let source_plan = manager.get("src").unwrap().plan.unwrap();

    let job = manager.freeze("src", template("ctl", "control")).unwrap();
    assert_eq!(job.target_node.as_deref(), Some("control"));
    assert_eq!(done(&manager, &job).await.status, JobStatus::Succeeded);
    let snapshot = manager.snapshot("ctl").unwrap();
    assert_eq!(snapshot.template, Some(model::TemplateRole::Control));
    // One VM's disks, like an all-in-one snapshot, so it exports as one archive.
    assert!(snapshot.nodes.is_empty());
    assert_eq!(snapshot.reference.as_deref(), Some("fake/ctl"));
    assert_eq!(snapshot.plan.computes, 0);
    assert_eq!(snapshot.plan.nodes.len(), 1);
    assert_eq!(snapshot.plan.nodes[0].role, crate::farm::Role::Control);
    assert_eq!(
        serde_json::to_value(snapshot.plan.nodes[0]).unwrap(),
        serde_json::to_value(source_plan.nodes[0]).unwrap()
    );
    assert_eq!(snapshot.plan.components, source_plan.components);
    let labels = backend.snapshots.lock().unwrap()["fake/ctl"].clone();
    assert_eq!(labels[crate::model::snapshot_labels::ROLE], "control");
    assert_eq!(labels[crate::model::snapshot_labels::NODE], "control");
    assert_eq!(
        labels[crate::model::snapshot_labels::RELEASE],
        crate::kolla::OPENSTACK_RELEASE
    );
    assert!(!labels.contains_key(crate::model::snapshot_labels::COMPUTES));
    // The source's control plane restarted and was checked; its OpenStack kept its compute.
    assert!(matches!(backend.settled.lock().unwrap().as_slice(), [None]));
    assert!(backend.forgotten.lock().unwrap().is_empty());
    let source = manager.get("src").unwrap();
    assert_eq!(source.status, FarmStatus::Ready);
    assert_eq!(source.plan.unwrap().computes, 1);
    let job = manager.freeze("src", template("cpt", "compute-1")).unwrap();
    assert_eq!(done(&manager, &job).await.status, JobStatus::Succeeded);

    // The whole-farm restore path starts a multi-node farm with the control VM alone.
    let restored = manager
        .create(CreateRequest {
            snapshot_id: Some("ctl".into()),
            ..request("copy")
        })
        .unwrap();
    assert_eq!(restored.kind, JobKind::Restore);
    let finished = done(&manager, &restored).await;
    assert_eq!(
        finished.status,
        JobStatus::Succeeded,
        "{:?}",
        finished.error
    );
    let copy = manager.get("copy").unwrap();
    assert_eq!(copy.status, FarmStatus::Ready);
    assert_eq!(copy.snapshot_id.as_deref(), Some("ctl"));
    let plan = copy.plan.clone().unwrap();
    assert!(plan.multinode());
    assert_eq!(plan.computes, 0);
    assert_eq!(copy.nodes.len(), 1);
    assert_eq!(copy.nodes[0].name, "copy-control");
    assert_eq!(copy.memory_mib(), plan.memory_mib);
    assert_eq!(backend.forgotten.lock().unwrap().as_slice(), &["copy"]);
    let settled = backend.settled.lock().unwrap().last().cloned().unwrap();
    assert_eq!(
        settled.map(|(from, _)| from.port_base),
        Some(snapshot.source_exposure.port_base)
    );
    let logs = manager.logs("copy", 0, 1000).unwrap();
    assert!(
        logs.iter()
            .any(|entry| entry.message.contains("computes are forgotten")),
        "{logs:?}"
    );
    // Kolla came with the template; a farm without a compute VM only grows.
    assert!(
        manager
            .deploy("copy", DeployRequest::default())
            .unwrap_err()
            .to_string()
            .contains("no compute VM yet")
    );
    assert!(
        manager
            .freeze(
                "copy",
                SnapshotRequest {
                    id: "whole".into(),
                    note: None,
                    node: None,
                },
            )
            .unwrap_err()
            .to_string()
            .contains("control template")
    );

    // A check forgets them again, in case the restore stopped before it had.
    let job = manager.settle("copy").unwrap();
    assert_eq!(done(&manager, &job).await.status, JobStatus::Succeeded);
    assert_eq!(backend.forgotten.lock().unwrap().len(), 2);

    // compute-1 is installed clean, then compute-2 boots from the source's compute template.
    let job = manager
        .add_compute("copy", AddComputeRequest::default())
        .unwrap();
    assert_eq!(job.target_node.as_deref(), Some("compute-1"));
    let finished = done(&manager, &job).await;
    assert_eq!(
        finished.status,
        JobStatus::Succeeded,
        "{:?}",
        finished.error
    );
    let job = manager
        .add_compute(
            "copy",
            AddComputeRequest {
                host_id: None,
                from_snapshot: Some("cpt".into()),
            },
        )
        .unwrap();
    assert_eq!(job.target_node.as_deref(), Some("compute-2"));
    let finished = done(&manager, &job).await;
    assert_eq!(
        finished.status,
        JobStatus::Succeeded,
        "{:?}",
        finished.error
    );
    let copy = manager.get("copy").unwrap();
    let plan = copy.plan.unwrap();
    assert_eq!(plan.computes, 2);
    assert_eq!(copy.nodes.len(), 3);
    assert_eq!(
        serde_json::to_value(&plan.nodes[..2]).unwrap(),
        serde_json::to_value(&source_plan.nodes).unwrap()
    );
    assert_eq!(
        backend.computed.lock().unwrap().as_slice(),
        &[("copy".into(), 1)]
    );
    assert_eq!(
        backend.reconfigured.lock().unwrap().as_slice(),
        &[("copy".into(), 2)]
    );

    // A control template travels as an archive and stays one.
    let archive = dir.path().join("ctl.msb");
    manager
        .export_snapshot(
            "ctl",
            ExportRequest {
                path: Some(archive.display().to_string()),
                ..Default::default()
            },
        )
        .unwrap();
    manager.wait_snapshot("ctl").await.unwrap();
    manager
        .import_snapshot(ImportRequest {
            path: Some(archive.display().to_string()),
            id: Some("ctl-copy".into()),
            ..Default::default()
        })
        .unwrap();
    let imported = manager.wait_snapshot("ctl-copy").await.unwrap();
    assert_eq!(
        imported.status,
        SnapshotStatus::Ready,
        "{:?}",
        imported.error
    );
    assert_eq!(imported.template, Some(model::TemplateRole::Control));
    assert_eq!(imported.plan.computes, 0);
    assert_eq!(
        serde_json::to_value(&imported.plan.nodes).unwrap(),
        serde_json::to_value(&snapshot.plan.nodes).unwrap()
    );
    let restored = manager
        .create(CreateRequest {
            snapshot_id: Some("ctl-copy".into()),
            ..request("again")
        })
        .unwrap();
    assert_eq!(done(&manager, &restored).await.status, JobStatus::Succeeded);
    assert_eq!(manager.get("again").unwrap().nodes.len(), 1);
}

#[tokio::test]
async fn a_control_template_needs_a_deployed_multi_node_farm_and_a_clean_copy() {
    let dir = tempfile::tempdir().unwrap();
    let backend = Arc::new(Fake::default());
    let manager = open_with(options(dir.path()), backend.clone()).await;
    if !cfg!(target_os = "linux") {
        *manager.0.guest_agent.write().unwrap() = Some(dir.path().join("farm-agent"));
    }
    let control = |id: &str| SnapshotRequest {
        id: id.into(),
        note: None,
        node: Some("control".into()),
    };
    // An all-in-one farm has no control VM.
    let aio = manager.create(request("aio")).unwrap();
    assert_eq!(done(&manager, &aio).await.status, JobStatus::Succeeded);
    let deployed = manager.deploy("aio", DeployRequest::default()).unwrap();
    assert_eq!(done(&manager, &deployed).await.status, JobStatus::Succeeded);
    let refused = manager.freeze("aio", control("nope")).unwrap_err();
    assert!(refused.is::<Conflict>());
    assert!(refused.to_string().contains("all-in-one"), "{refused}");

    let created = manager
        .create(CreateRequest {
            computes: 1,
            ..request("src")
        })
        .unwrap();
    assert_eq!(done(&manager, &created).await.status, JobStatus::Succeeded);
    let refused = manager.freeze("src", control("nope")).unwrap_err();
    assert!(refused.to_string().contains("deploy the farm"), "{refused}");
    let deployed = manager.deploy("src", DeployRequest::default()).unwrap();
    assert_eq!(done(&manager, &deployed).await.status, JobStatus::Succeeded);
    let job = manager.freeze("src", control("ctl")).unwrap();
    assert_eq!(done(&manager, &job).await.status, JobStatus::Succeeded);
    // Neither a compute template nor a control template adds a compute.
    let refused = manager
        .add_compute(
            "src",
            AddComputeRequest {
                host_id: None,
                from_snapshot: Some("ctl".into()),
            },
        )
        .unwrap_err();
    assert!(
        refused.to_string().contains("not a compute template"),
        "{refused}"
    );

    // A copy that cannot forget its source's computes is not ready.
    backend.fail_forget.store(true, Ordering::SeqCst);
    let restored = manager
        .create(CreateRequest {
            snapshot_id: Some("ctl".into()),
            ..request("copy")
        })
        .unwrap();
    let failed = done(&manager, &restored).await;
    assert_eq!(failed.status, JobStatus::Failed);
    assert!(
        failed
            .error
            .as_deref()
            .is_some_and(|e| e.contains("still registered")),
        "{:?}",
        failed.error
    );
    assert_eq!(manager.get("copy").unwrap().status, FarmStatus::Failed);
    assert!(
        manager
            .add_compute("copy", AddComputeRequest::default())
            .is_err()
    );
}

#[tokio::test]
async fn compute_templates_recorded_before_control_templates_load_as_compute_templates() {
    let dir = tempfile::tempdir().unwrap();
    let manager = open_with(options(dir.path()), Arc::new(Fake::default())).await;
    if !cfg!(target_os = "linux") {
        *manager.0.guest_agent.write().unwrap() = Some(dir.path().join("farm-agent"));
    }
    let created = manager
        .create(CreateRequest {
            computes: 1,
            ..request("src")
        })
        .unwrap();
    assert_eq!(done(&manager, &created).await.status, JobStatus::Succeeded);
    let deployed = manager.deploy("src", DeployRequest::default()).unwrap();
    assert_eq!(done(&manager, &deployed).await.status, JobStatus::Succeeded);
    let job = manager
        .freeze(
            "src",
            SnapshotRequest {
                id: "tpl".into(),
                note: None,
                node: Some("compute-1".into()),
            },
        )
        .unwrap();
    assert_eq!(done(&manager, &job).await.status, JobStatus::Succeeded);
    settle(&manager).await;
    drop(manager);
    let path = dir.path().join("state.json");
    let mut state: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    let record = &mut state["snapshots"]["tpl"];
    assert_eq!(record["template"], "compute");
    assert!(record.get("compute_template").is_none());
    let record = record.as_object_mut().unwrap();
    record.remove("template");
    record.insert("compute_template".into(), true.into());
    std::fs::write(&path, serde_json::to_vec(&state).unwrap()).unwrap();

    let manager = open_with(options(dir.path()), Arc::new(Fake::default())).await;
    let snapshot = manager.snapshot("tpl").unwrap();
    assert_eq!(snapshot.template, Some(model::TemplateRole::Compute));
    assert!(!snapshot.legacy_compute_template);
    let api = serde_json::to_value(&snapshot).unwrap();
    assert_eq!(api["template"], "compute");
    assert!(api.get("compute_template").is_none());
}

#[tokio::test]
async fn a_farm_restored_from_a_snapshot_can_gain_a_compute() {
    let dir = tempfile::tempdir().unwrap();
    let backend = Arc::new(Fake::default());
    let manager = open_with(options(dir.path()), backend.clone()).await;
    if !cfg!(target_os = "linux") {
        *manager.0.guest_agent.write().unwrap() = Some(dir.path().join("farm-agent"));
    }
    let created = manager
        .create(CreateRequest {
            computes: 1,
            ..request("src")
        })
        .unwrap();
    assert_eq!(done(&manager, &created).await.status, JobStatus::Succeeded);
    let deployed = manager.deploy("src", DeployRequest::default()).unwrap();
    assert_eq!(done(&manager, &deployed).await.status, JobStatus::Succeeded);
    let job = manager
        .freeze(
            "src",
            SnapshotRequest {
                id: "snap".into(),
                note: None,
                node: None,
            },
        )
        .unwrap();
    assert_eq!(done(&manager, &job).await.status, JobStatus::Succeeded);
    let restored = manager
        .create(CreateRequest {
            snapshot_id: Some("snap".into()),
            ..request("copy")
        })
        .unwrap();
    assert_eq!(done(&manager, &restored).await.status, JobStatus::Succeeded);
    assert_eq!(manager.get("copy").unwrap().status, FarmStatus::Ready);

    let job = manager
        .add_compute("copy", AddComputeRequest::default())
        .unwrap();
    let finished = done(&manager, &job).await;
    assert_eq!(
        finished.status,
        JobStatus::Succeeded,
        "{:?}",
        finished.error
    );
    let farm = manager.get("copy").unwrap();
    assert_eq!(farm.snapshot_id.as_deref(), Some("snap"));
    assert_eq!(farm.plan.as_ref().unwrap().computes, 2);
    assert_eq!(farm.nodes.len(), 3);
    assert_eq!(
        backend.computed.lock().unwrap().as_slice(),
        &[("copy".into(), 2)]
    );
}

#[tokio::test]
async fn a_farm_can_gain_a_compute_on_a_new_remote_host_and_another_on_its_primary_host() {
    use crate::remote::{AgentArtifactInfo, HostJobKind, InstalledAgent, test_server::Fixture};
    let fixture = Fixture::start().await;
    let dir = tempfile::tempdir().unwrap();
    let backend = Arc::new(Fake::default());
    let mut opts = options(dir.path());
    opts.memory_budget_mib = 64 * 1024;
    let manager = open_with(opts, backend.clone()).await;
    if !cfg!(target_os = "linux") {
        *manager.0.guest_agent.write().unwrap() = Some(dir.path().join("farm-agent"));
    }
    manager.register_host(fixture.registration("edge")).unwrap();
    let prep = manager
        .start_host_job("edge", HostJobKind::Prepare)
        .unwrap();
    assert_eq!(
        host_done(&manager, &prep.id).await.status,
        JobStatus::Succeeded
    );
    manager
        .update(|state| {
            state.hosts.get_mut("edge").unwrap().host.agent = Some(InstalledAgent {
                artifact: AgentArtifactInfo {
                    sha256: "a".repeat(64),
                    os: "linux".into(),
                    arch: std::env::consts::ARCH.into(),
                    protocol: 1,
                    supports_single_vm: true,
                    build_id: crate::BUILD_ID.into(),
                },
                verified_at: now(),
            });
            Ok(())
        })
        .unwrap();
    use crate::link::relay::{RelaySettings, RelayTlsMode};
    manager
        .set_relay(Some(RelaySettings {
            url: "https://coordinator.example:10011".into(),
            mode: RelayTlsMode::Private,
            bind: Some("127.0.0.1:0".parse().unwrap()),
        }))
        .await
        .unwrap();
    manager.start_relay().await;

    let created = manager
        .create(CreateRequest {
            computes: 1,
            ..request("span")
        })
        .unwrap();
    assert_eq!(done(&manager, &created).await.status, JobStatus::Succeeded);
    let deployed = manager.deploy("span", DeployRequest::default()).unwrap();
    assert_eq!(done(&manager, &deployed).await.status, JobStatus::Succeeded);

    let job = manager
        .add_compute(
            "span",
            AddComputeRequest {
                host_id: Some("edge".into()),
                from_snapshot: None,
            },
        )
        .unwrap();
    let finished = done(&manager, &job).await;
    assert_eq!(
        finished.status,
        JobStatus::Succeeded,
        "{:?}",
        finished.error
    );
    let farm = manager.get("span").unwrap();
    assert_eq!(farm.plan.as_ref().unwrap().computes, 2);
    assert_eq!(farm.shares.len(), 1);
    assert_eq!(farm.shares[0].host_id, "edge");
    assert_eq!(farm.shares[0].nodes, ["compute-2"]);
    assert_eq!(farm.host_of("compute-2"), Some("edge"));
    assert!(farm.uplink_sha256.is_some());
    assert_eq!(farm.nodes.len(), 3);
    assert_eq!(manager.read(|s| s.relay_keys["span"].len()).unwrap(), 2);
    assert!(
        !manager
            .read(|s| s.pending_shares.contains_key("span"))
            .unwrap()
    );
    assert!(
        fixture
            .observed
            .calls
            .lock()
            .unwrap()
            .contains(&"share:compute-2".to_string())
    );
    assert!(
        fixture
            .observed
            .calls
            .lock()
            .unwrap()
            .contains(&"prepare:span".to_string())
    );
    assert_eq!(
        backend.computed.lock().unwrap().as_slice(),
        &[("span".into(), 2)]
    );

    // A later compute on the primary host keeps the remote share and the relay as they are.
    let job = manager
        .add_compute("span", AddComputeRequest::default())
        .unwrap();
    let finished = done(&manager, &job).await;
    assert_eq!(
        finished.status,
        JobStatus::Succeeded,
        "{:?}",
        finished.error
    );
    let farm = manager.get("span").unwrap();
    assert_eq!(farm.plan.as_ref().unwrap().computes, 3);
    assert_eq!(farm.shares[0].nodes, ["compute-2"]);
    assert_eq!(farm.host_of("compute-3"), Some("local"));
    assert_eq!(farm.nodes.len(), 4);
    assert_eq!(manager.read(|s| s.relay_keys["span"].len()).unwrap(), 2);
    assert_eq!(
        backend.computed.lock().unwrap().as_slice(),
        &[("span".into(), 2), ("span".into(), 3)]
    );
}

#[tokio::test]
async fn a_farm_can_gain_a_second_compute_on_a_remote_host_it_already_uses() {
    use crate::remote::{AgentArtifactInfo, HostJobKind, InstalledAgent, test_server::Fixture};
    let fixture = Fixture::start().await;
    let dir = tempfile::tempdir().unwrap();
    let backend = Arc::new(Fake::default());
    let mut opts = options(dir.path());
    opts.memory_budget_mib = 64 * 1024;
    let manager = open_with(opts, backend.clone()).await;
    if !cfg!(target_os = "linux") {
        *manager.0.guest_agent.write().unwrap() = Some(dir.path().join("farm-agent"));
    }
    manager.register_host(fixture.registration("edge")).unwrap();
    let prep = manager
        .start_host_job("edge", HostJobKind::Prepare)
        .unwrap();
    assert_eq!(
        host_done(&manager, &prep.id).await.status,
        JobStatus::Succeeded
    );
    manager
        .update(|state| {
            state.hosts.get_mut("edge").unwrap().host.agent = Some(InstalledAgent {
                artifact: AgentArtifactInfo {
                    sha256: "a".repeat(64),
                    os: "linux".into(),
                    arch: std::env::consts::ARCH.into(),
                    protocol: 1,
                    supports_single_vm: true,
                    build_id: crate::BUILD_ID.into(),
                },
                verified_at: now(),
            });
            Ok(())
        })
        .unwrap();
    use crate::link::relay::{RelaySettings, RelayTlsMode};
    manager
        .set_relay(Some(RelaySettings {
            url: "https://coordinator.example:10011".into(),
            mode: RelayTlsMode::Private,
            bind: Some("127.0.0.1:0".parse().unwrap()),
        }))
        .await
        .unwrap();
    manager.start_relay().await;

    let created = manager
        .create(CreateRequest {
            computes: 1,
            ..request("span")
        })
        .unwrap();
    assert_eq!(done(&manager, &created).await.status, JobStatus::Succeeded);
    let deployed = manager.deploy("span", DeployRequest::default()).unwrap();
    assert_eq!(done(&manager, &deployed).await.status, JobStatus::Succeeded);
    for host in ["edge", "edge"] {
        let job = manager
            .add_compute(
                "span",
                AddComputeRequest {
                    host_id: Some(host.into()),
                    from_snapshot: None,
                },
            )
            .unwrap();
        let finished = done(&manager, &job).await;
        assert_eq!(
            finished.status,
            JobStatus::Succeeded,
            "{:?}",
            finished.error
        );
    }
    let farm = manager.get("span").unwrap();
    assert_eq!(farm.plan.as_ref().unwrap().computes, 3);
    assert_eq!(farm.shares[0].nodes, ["compute-2", "compute-3"]);
    assert_eq!(farm.host_of("compute-3"), Some("edge"));
    assert_eq!(farm.nodes.len(), 4);
    assert_eq!(manager.read(|s| s.relay_keys["span"].len()).unwrap(), 2);
    assert!(
        fixture
            .observed
            .calls
            .lock()
            .unwrap()
            .contains(&"expand:span:compute-3".to_string())
    );
    assert_eq!(
        backend.computed.lock().unwrap().as_slice(),
        &[("span".into(), 2), ("span".into(), 3)]
    );
}

#[tokio::test]
async fn a_farm_spans_hosts_with_its_computes_placed_elsewhere() {
    use crate::remote::{AgentArtifactInfo, HostJobKind, InstalledAgent, test_server::Fixture};
    let fixture = Fixture::start().await;
    let dir = tempfile::tempdir().unwrap();
    let backend = Arc::new(Fake::default());
    let mut opts = options(dir.path());
    opts.memory_budget_mib = 64 * 1024;
    let manager = open_with(opts, backend.clone()).await;
    if !cfg!(target_os = "linux") {
        *manager.0.guest_agent.write().unwrap() = Some(dir.path().join("farm-agent"));
    }
    manager.register_host(fixture.registration("edge")).unwrap();
    let prep = manager
        .start_host_job("edge", HostJobKind::Prepare)
        .unwrap();
    assert_eq!(
        host_done(&manager, &prep.id).await.status,
        JobStatus::Succeeded
    );
    manager
        .update(|state| {
            state.hosts.get_mut("edge").unwrap().host.agent = Some(InstalledAgent {
                artifact: AgentArtifactInfo {
                    sha256: "a".repeat(64),
                    os: "linux".into(),
                    arch: std::env::consts::ARCH.into(),
                    protocol: 1,
                    supports_single_vm: true,
                    build_id: crate::BUILD_ID.into(),
                },
                verified_at: now(),
            });
            Ok(())
        })
        .unwrap();
    let spanning = CreateRequest {
        computes: 2,
        placement: [("compute-2".to_string(), "edge".to_string())].into(),
        ..request("mn")
    };
    // Without a relay the hosts cannot reach each other.
    let refused = manager.create(spanning.clone()).unwrap_err();
    assert!(refused.to_string().contains("relay"), "{refused}");
    use crate::link::relay::{RelaySettings, RelayTlsMode};
    let private = RelaySettings {
        url: "https://coordinator.example:10011".into(),
        mode: RelayTlsMode::Private,
        bind: Some("127.0.0.1:0".parse().unwrap()),
    };
    assert!(
        manager
            .set_relay(Some(RelaySettings {
                url: "http://coordinator.example".into(),
                ..private.clone()
            }))
            .await
            .unwrap_err()
            .is::<BadRequest>()
    );
    let status = manager.set_relay(Some(private.clone())).await.unwrap();
    assert!(status.ca_sha256.is_some());
    // The daemon does not serve in tests, so nothing listens yet.
    assert!(status.listening.is_none());
    manager.start_relay().await;
    assert!(manager.relay_status().await.unwrap().listening.is_some());
    assert!(
        manager
            .create(CreateRequest {
                placement: [("compute-3".to_string(), "edge".to_string())].into(),
                ..spanning.clone()
            })
            .is_err()
    );
    let job = manager.create(spanning).unwrap();
    let finished = done(&manager, &job).await;
    assert_eq!(
        finished.status,
        JobStatus::Succeeded,
        "{:?}",
        finished.error
    );
    let farm = manager.get("mn").unwrap();
    assert_eq!(farm.shares.len(), 1);
    assert_eq!(farm.shares[0].host_id, "edge");
    assert_eq!(farm.shares[0].nodes, ["compute-2"]);
    assert_eq!(farm.nodes.len(), 3, "{:?}", farm.nodes);
    assert_eq!(farm.host_of("compute-2"), Some("edge"));
    assert_eq!(farm.host_of("compute-1"), Some("local"));
    // Each host got its part; only the control VM's host holds the private key.
    let local_share = backend.shares.lock().unwrap()[0].clone();
    assert_eq!(local_share.nodes, ["control", "compute-1"]);
    assert!(local_share.ssh_private_key.is_some());
    assert_eq!(
        local_share.uplink.url,
        "wss://coordinator.example:10011/api/v1/link-relay?farm=mn&host=local"
    );
    assert!(
        fixture
            .observed
            .calls
            .lock()
            .unwrap()
            .contains(&"share:compute-2".to_string())
    );
    // Uplink keys are kept sealed: never in the state file, nor in the API's farm record.
    assert_eq!(local_share.uplink.key.len(), 64);
    let state_file = std::fs::read_to_string(dir.path().join("state.json")).unwrap();
    assert!(!state_file.contains(&local_share.uplink.key));
    assert!(
        !serde_json::to_string(&farm)
            .unwrap()
            .contains(&local_share.uplink.key)
    );
    assert_eq!(manager.read(|s| s.relay_keys["mn"].len()).unwrap(), 2);
    // Each host's switch may join the relay with its own token only.
    assert!(
        manager
            .relay()
            .accept("mn", "local", &local_share.uplink.token)
            .is_ok()
    );
    assert!(
        manager
            .relay()
            .accept("mn", "edge", &local_share.uplink.token)
            .is_err()
    );
    // Memory counts on the host that runs each VM.
    let edge_memory = farm.memory_on("edge");
    assert!(edge_memory > 0 && edge_memory + farm.memory_on("local") == farm.memory_mib());

    let deploy = manager.deploy("mn", DeployRequest::default()).unwrap();
    let finished = done(&manager, &deploy).await;
    assert_eq!(
        finished.status,
        JobStatus::Succeeded,
        "{:?}",
        finished.error
    );
    assert!(
        fixture
            .observed
            .calls
            .lock()
            .unwrap()
            .contains(&"prepare:mn".to_string())
    );
    assert_eq!(*backend.spanning.lock().unwrap(), [vec![2u8]]);

    let cut = crate::link::Impair {
        delay_ms: 0,
        loss: 1.0,
    };
    manager.impair("mn", "compute-1", cut).await.unwrap();
    assert_eq!(backend.faults.lock().unwrap().len(), 1);

    // A new relay address, as a quick tunnel gets, reaches each host's switch.
    manager.relink("https://calm-river.trycloudflare.com").await;
    assert_eq!(
        *backend.relinked.lock().unwrap(),
        [(
            "mn".to_string(),
            "wss://calm-river.trycloudflare.com/api/v1/link-relay?farm=mn&host=local".to_string()
        )]
    );
    assert!(
        fixture.observed.calls.lock().unwrap().contains(
            &"relink:mn:wss://calm-river.trycloudflare.com/api/v1/link-relay?farm=mn&host=edge"
                .to_string()
        )
    );
    // A host that missed the new address gets it again from the reconciler. The local
    // host's uplink was accepted above, so only edge, which never joined, is retried.
    assert_eq!(manager.relay().connected("mn"), ["local"]);
    *manager.0.tunnel_url.lock().unwrap() = Some("https://new-river.trycloudflare.com".into());
    manager.relink_missing().await;
    assert_eq!(backend.relinked.lock().unwrap().len(), 1);
    assert!(
        fixture.observed.calls.lock().unwrap().iter().any(|c| c
            == "relink:mn:wss://new-river.trycloudflare.com/api/v1/link-relay?farm=mn&host=edge")
    );
    *manager.0.tunnel_url.lock().unwrap() = None;
    // A quick tunnel's URL comes from Cloudflare, so none may be given.
    assert!(
        manager
            .set_relay(Some(RelaySettings {
                url: "https://x.example".into(),
                mode: RelayTlsMode::QuickTunnel,
                bind: None,
            }))
            .await
            .unwrap_err()
            .is::<BadRequest>()
    );
    // Its switches hold the relay's URL and CA, so only the listen address may change.
    let proxy = RelaySettings {
        url: "https://relay.example".into(),
        mode: RelayTlsMode::Proxy,
        bind: None,
    };
    assert!(
        manager
            .set_relay(Some(proxy.clone()))
            .await
            .unwrap_err()
            .is::<Conflict>()
    );
    manager
        .set_relay(Some(RelaySettings {
            bind: Some("127.0.0.1:0".parse().unwrap()),
            ..private.clone()
        }))
        .await
        .unwrap();

    let delete = manager.delete("mn").unwrap();
    assert_eq!(done(&manager, &delete).await.status, JobStatus::Succeeded);
    assert!(
        fixture
            .observed
            .rpc_farms
            .lock()
            .unwrap()
            .get("mn")
            .is_none()
    );
    assert!(!backend.farms.lock().unwrap().contains_key("mn"));
    assert!(!manager.read(|s| s.relay_keys.contains_key("mn")).unwrap());
    assert!(
        manager
            .relay()
            .accept("mn", "local", &local_share.uplink.token)
            .is_err()
    );
    // Behind a proxy, new farms' switches trust public CAs.
    let status = manager
        .set_relay(Some(RelaySettings {
            bind: Some("127.0.0.1:0".parse().unwrap()),
            ..proxy
        }))
        .await
        .unwrap();
    assert!(status.ca_sha256.is_none());
    let job = manager
        .create(CreateRequest {
            computes: 1,
            placement: [("compute-1".to_string(), "edge".to_string())].into(),
            ..request("px")
        })
        .unwrap();
    assert_eq!(done(&manager, &job).await.status, JobStatus::Succeeded);
    {
        let shares = backend.shares.lock().unwrap();
        let last = shares.last().unwrap();
        assert!(last.uplink.ca.is_empty());
        assert!(
            last.uplink
                .url
                .starts_with("wss://relay.example/api/v1/link-relay?farm=px")
        );
    }
    let delete = manager.delete("px").unwrap();
    assert_eq!(done(&manager, &delete).await.status, JobStatus::Succeeded);
    settle(&manager).await;
}

/// A deployed multi-node farm `src`, frozen into control templates `ctl-old` and `ctl` (the
/// newer) and compute template `cpt`.
async fn release_templates(dir: &Path) -> (FarmManager, Arc<Fake>) {
    let backend = Arc::new(Fake::default());
    let manager = open_with(options(dir), backend.clone()).await;
    if !cfg!(target_os = "linux") {
        *manager.0.guest_agent.write().unwrap() = Some(dir.join("farm-agent"));
    }
    let created = manager
        .create(CreateRequest {
            computes: 1,
            ..request("src")
        })
        .unwrap();
    assert_eq!(done(&manager, &created).await.status, JobStatus::Succeeded);
    let deployed = manager.deploy("src", DeployRequest::default()).unwrap();
    assert_eq!(done(&manager, &deployed).await.status, JobStatus::Succeeded);
    for (id, node) in [
        ("ctl-old", "control"),
        ("ctl", "control"),
        ("cpt", "compute-1"),
    ] {
        let job = manager
            .freeze(
                "src",
                SnapshotRequest {
                    id: id.into(),
                    note: None,
                    node: Some(node.into()),
                },
            )
            .unwrap();
        assert_eq!(done(&manager, &job).await.status, JobStatus::Succeeded);
    }
    manager
        .update(|state| {
            state.snapshots.get_mut("ctl-old").unwrap().created_at = 100;
            state.snapshots.get_mut("ctl").unwrap().created_at = 200;
            Ok(())
        })
        .unwrap();
    (manager, backend)
}
fn by_release(id: &str, computes: u8) -> ReleaseCreateRequest {
    ReleaseCreateRequest {
        id: id.into(),
        release: crate::kolla::OPENSTACK_RELEASE.into(),
        host_id: Some("local".into()),
        computes,
        external_host: "localhost".into(),
        ..Default::default()
    }
}

#[tokio::test]
async fn a_release_plan_picks_the_newest_matching_templates_or_says_why_it_installs_clean() {
    use crate::remote::test_server::Fixture;
    let dir = tempfile::tempdir().unwrap();
    let (manager, _) = release_templates(dir.path()).await;
    let release = crate::kolla::OPENSTACK_RELEASE;

    let plan = manager.preview_release(&by_release("r", 2)).await.unwrap();
    assert_eq!(plan.release, release);
    assert_eq!(plan.arch, current_arch());
    assert_eq!(
        plan.components,
        manager.snapshot("ctl").unwrap().plan.components
    );
    assert_eq!(plan.control.source, NodeSource::Template);
    assert_eq!(plan.control.snapshot_id.as_deref(), Some("ctl"));
    assert!(
        plan.control.reason.contains("newest of 2"),
        "{}",
        plan.control.reason
    );
    assert_eq!(plan.computes.len(), 2);
    for (index, compute) in plan.computes.iter().enumerate() {
        assert_eq!(compute.node, format!("compute-{}", index + 1));
        assert_eq!(compute.host_id, "local");
        assert_eq!(compute.source, NodeSource::Template);
        assert_eq!(compute.snapshot_id.as_deref(), Some("cpt"));
    }
    // Nothing was created.
    assert!(manager.get("r").is_err());
    assert!(
        manager
            .jobs()
            .unwrap()
            .iter()
            .all(|j| j.kind != JobKind::Assemble)
    );

    // A named template wins over the newest.
    let plan = manager
        .preview_release(&ReleaseCreateRequest {
            control_template: Some("ctl-old".into()),
            ..by_release("r", 1)
        })
        .await
        .unwrap();
    assert_eq!(plan.control.snapshot_id.as_deref(), Some("ctl-old"));
    let wrong = manager
        .preview_release(&ReleaseCreateRequest {
            control_template: Some("cpt".into()),
            ..by_release("r", 1)
        })
        .await
        .unwrap_err();
    assert!(wrong.is::<Conflict>(), "{wrong:#}");

    // Other services: the templates do not match, and the coordinator's release installs clean.
    let plan = manager
        .preview_release(&ReleaseCreateRequest {
            components: vec!["horizon".into()],
            ..by_release("r", 2)
        })
        .await
        .unwrap();
    assert_eq!(plan.control.source, NodeSource::Clean);
    assert!(
        plan.control.reason.contains("run other services"),
        "{}",
        plan.control.reason
    );
    assert!(plan.components.contains(&"horizon".to_string()));
    assert!(plan.computes.iter().all(|c| c.source == NodeSource::Clean));

    // A compute on another host is installed clean; templates stay on the local host.
    let fixture = Fixture::start().await;
    manager.register_host(fixture.registration("edge")).unwrap();
    let plan = manager
        .preview_release(&ReleaseCreateRequest {
            compute_hosts: vec!["".into(), "edge".into()],
            ..by_release("r", 2)
        })
        .await
        .unwrap();
    assert_eq!(plan.control.source, NodeSource::Template);
    assert_eq!(plan.computes[0].source, NodeSource::Template);
    assert_eq!(plan.computes[1].host_id, "edge");
    assert_eq!(plan.computes[1].source, NodeSource::Clean);
    assert!(
        plan.computes[1]
            .reason
            .contains("not copied to host 'edge'"),
        "{}",
        plan.computes[1].reason
    );
    // So is a control VM on another host, with its computes.
    let plan = manager
        .preview_release(&ReleaseCreateRequest {
            host_id: Some("edge".into()),
            ..by_release("r", 1)
        })
        .await
        .unwrap();
    assert_eq!(plan.control.source, NodeSource::Clean);
    assert!(
        plan.control.reason.contains("choose the local host"),
        "{}",
        plan.control.reason
    );
    assert!(
        manager
            .preview_release(&ReleaseCreateRequest {
                host_id: Some("edge".into()),
                control_template: Some("ctl".into()),
                ..by_release("r", 1)
            })
            .await
            .unwrap_err()
            .is::<Conflict>()
    );

    // No compute template of the release: the computes are installed clean.
    manager
        .update(|state| {
            state.snapshots.get_mut("cpt").unwrap().release = Some("2025.1".into());
            Ok(())
        })
        .unwrap();
    let plan = manager.preview_release(&by_release("r", 1)).await.unwrap();
    assert_eq!(plan.control.source, NodeSource::Template);
    assert_eq!(plan.computes[0].source, NodeSource::Clean);
    assert!(
        plan.computes[0].reason.contains("no compute template"),
        "{}",
        plan.computes[0].reason
    );
    // A release with no control template that this coordinator does not install is refused.
    let error = manager
        .preview_release(&ReleaseCreateRequest {
            release: "2025.1".into(),
            ..by_release("r", 1)
        })
        .await
        .unwrap_err();
    assert!(error.is::<Conflict>());
    assert!(
        error
            .to_string()
            .contains("no control template of OpenStack 2025.1"),
        "{error:#}"
    );

    // Templates of another architecture never match.
    manager
        .update(|state| {
            for snapshot in state.snapshots.values_mut() {
                snapshot.arch = "riscv64".into();
            }
            Ok(())
        })
        .unwrap();
    let plan = manager.preview_release(&by_release("r", 1)).await.unwrap();
    assert_eq!(plan.control.source, NodeSource::Clean);
    let catalog = manager.release_catalog(None, None).await.unwrap();
    assert_eq!(catalog.releases.len(), 1);
    assert_eq!(catalog.releases[0].local_control, 0);
    assert!(catalog.releases[0].installable);
    let catalog = manager
        .release_catalog(Some("riscv64".into()), None)
        .await
        .unwrap();
    let counts: Vec<_> = catalog
        .releases
        .iter()
        .map(|r| {
            (
                r.release.as_str(),
                r.local_control,
                r.local_compute,
                r.installable,
            )
        })
        .collect();
    assert_eq!(counts, [(release, 2, 0, false), ("2025.1", 0, 1, false)]);

    // A farm ID in use is refused before anything else.
    assert!(
        manager
            .preview_release(&by_release("src", 1))
            .await
            .unwrap_err()
            .is::<Conflict>()
    );
}

#[tokio::test]
async fn a_release_farm_restores_its_control_then_adds_each_compute_in_order() {
    let dir = tempfile::tempdir().unwrap();
    let (manager, backend) = release_templates(dir.path()).await;
    let job = manager.create_release(by_release("r", 2)).await.unwrap();
    assert_eq!(job.kind, JobKind::Assemble);
    let actions: Vec<_> = job.assembly.iter().map(|s| s.action.as_str()).collect();
    assert_eq!(actions, ["restore", "expand", "expand"]);
    let finished = done_slowly(&manager, &job).await;
    assert_eq!(
        finished.status,
        JobStatus::Succeeded,
        "{:?}",
        finished.error
    );
    let kinds: Vec<_> = finished
        .assembly
        .iter()
        .map(|step| {
            assert_eq!(step.status, JobStatus::Succeeded);
            manager.job(step.job_id.as_ref().unwrap()).unwrap()
        })
        .map(|job| (job.kind, job.target_node, job.snapshot_id))
        .collect();
    assert_eq!(
        kinds,
        [
            (JobKind::Restore, None, Some("ctl".into())),
            (
                JobKind::Expand,
                Some("compute-1".into()),
                Some("cpt".into())
            ),
            (
                JobKind::Expand,
                Some("compute-2".into()),
                Some("cpt".into())
            ),
        ]
    );
    let farm = manager.get("r").unwrap();
    assert_eq!(farm.status, FarmStatus::Ready);
    assert_eq!(farm.snapshot_id.as_deref(), Some("ctl"));
    assert_eq!(farm.plan.unwrap().computes, 2);
    assert_eq!(farm.nodes.len(), 3);
    assert_eq!(
        backend.reconfigured.lock().unwrap().as_slice(),
        &[("r".into(), 1), ("r".into(), 2)]
    );
    let logs = manager.logs("r", 0, 1000).unwrap();
    assert!(
        logs.iter()
            .any(|l| l.job_id == job.id && l.message.contains("compute-2 on local: template cpt"))
    );
    // The same farm cannot be started twice.
    assert!(
        manager
            .create_release(by_release("r", 1))
            .await
            .unwrap_err()
            .is::<Conflict>()
    );

    // Without a control template, the coordinator's release is created and deployed clean.
    manager
        .update(|state| {
            state
                .snapshots
                .retain(|_, s| s.template != Some(TemplateRole::Control));
            Ok(())
        })
        .unwrap();
    let job = manager
        .create_release(by_release("clean", 1))
        .await
        .unwrap();
    let actions: Vec<_> = job.assembly.iter().map(|s| s.action.as_str()).collect();
    assert_eq!(actions, ["create", "deploy"]);
    let finished = done_slowly(&manager, &job).await;
    assert_eq!(
        finished.status,
        JobStatus::Succeeded,
        "{:?}",
        finished.error
    );
    let farm = manager.get("clean").unwrap();
    assert_eq!(farm.status, FarmStatus::Ready);
    assert_eq!(farm.snapshot_id, None);
    assert_eq!(farm.plan.unwrap().computes, 1);
}

async fn done_slowly(manager: &FarmManager, job: &Job) -> Job {
    tokio::time::timeout(Duration::from_secs(30), manager.wait(&job.id))
        .await
        .unwrap()
        .unwrap()
}

#[tokio::test]
async fn a_release_farm_that_fails_midway_keeps_its_vms_and_is_retried_by_expansion() {
    let dir = tempfile::tempdir().unwrap();
    let (manager, backend) = release_templates(dir.path()).await;
    backend.fail_expansion_from.store(2, Ordering::SeqCst);
    let job = manager.create_release(by_release("r", 3)).await.unwrap();
    let finished = done_slowly(&manager, &job).await;
    assert_eq!(finished.status, JobStatus::Failed);
    let error = finished.error.unwrap();
    assert!(error.contains("add compute-2"), "{error}");
    assert!(error.contains("from compute template 'cpt'"), "{error}");
    let statuses: Vec<_> = finished.assembly.iter().map(|s| s.status).collect();
    assert_eq!(
        statuses,
        [
            JobStatus::Succeeded,
            JobStatus::Succeeded,
            JobStatus::Failed,
            JobStatus::Interrupted
        ]
    );
    assert!(finished.assembly[3].job_id.is_none());
    // The farm has its control VM and compute-1; the failed expansion committed nothing.
    let farm = manager.get("r").unwrap();
    assert_eq!(farm.status, FarmStatus::Failed);
    assert_eq!(farm.active_job, None);
    assert_eq!(farm.plan.as_ref().unwrap().computes, 1);

    // Retried the way it started, then expanded further.
    backend.fail_expansion_from.store(0, Ordering::SeqCst);
    let request = AddComputeRequest {
        host_id: None,
        from_snapshot: Some("cpt".into()),
    };
    let retry = manager.add_compute("r", request.clone()).unwrap();
    assert_eq!(retry.target_node.as_deref(), Some("compute-2"));
    assert_eq!(done(&manager, &retry).await.status, JobStatus::Succeeded);
    let next = manager.add_compute("r", request).unwrap();
    assert_eq!(next.target_node.as_deref(), Some("compute-3"));
    assert_eq!(done(&manager, &next).await.status, JobStatus::Succeeded);
    let farm = manager.get("r").unwrap();
    assert_eq!(farm.status, FarmStatus::Ready);
    assert_eq!(farm.plan.unwrap().computes, 3);

    // A daemon stop leaves an assemble job interrupted, not its farm.
    manager
        .update(|state| {
            let job = state.jobs.get_mut(&job.id).unwrap();
            job.status = JobStatus::Running;
            job.assembly[3].status = JobStatus::Running;
            Ok(())
        })
        .unwrap();
    settle(&manager).await;
    drop(manager);
    let manager = open_with(options(dir.path()), backend).await;
    let job = manager.job(&job.id).unwrap();
    assert_eq!(job.status, JobStatus::Interrupted);
    assert_eq!(job.assembly[3].status, JobStatus::Interrupted);
    assert_eq!(manager.get("r").unwrap().status, FarmStatus::Ready);
}

#[tokio::test]
async fn a_release_farm_imports_bucket_templates_once_and_reuses_the_import() {
    use crate::s3::{RegisterBucket, fake::FakeS3};
    let s3 = Arc::new(FakeS3::default());
    let endpoint = s3.start().await;
    let dir = tempfile::tempdir().unwrap();
    let (manager, backend) = release_templates(dir.path()).await;
    manager
        .register_bucket(RegisterBucket {
            id: "minio".into(),
            endpoint: Some(endpoint),
            region: None,
            name: "farm".into(),
            prefix: Some("snapshots".into()),
            path_style: None,
            ca_pem: None,
            access_key_id: "admin".into(),
            secret_access_key: Some("secret".into()),
        })
        .unwrap();
    for id in ["ctl", "cpt"] {
        manager
            .export_snapshot(
                id,
                ExportRequest {
                    bucket: Some("minio".into()),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(manager.wait_snapshot(id).await.unwrap().error, None);
    }
    let catalog = manager.release_catalog(None, None).await.unwrap();
    assert_eq!(catalog.releases.len(), 1);
    let counts = &catalog.releases[0];
    assert_eq!(
        (
            counts.local_control,
            counts.local_compute,
            counts.bucket_control,
            counts.bucket_compute
        ),
        (2, 1, 1, 1)
    );
    // Only the bucket keeps them now.
    for id in ["ctl-old", "ctl", "cpt"] {
        manager.forget_snapshot(id).await.unwrap();
    }
    let plan = manager.preview_release(&by_release("r", 2)).await.unwrap();
    assert_eq!(plan.control.source, NodeSource::Bucket);
    assert_eq!(plan.control.bucket.as_deref(), Some("minio"));
    assert_eq!(plan.control.key.as_deref(), Some("snapshots/ctl.msb"));
    assert_eq!(plan.control.snapshot_id.as_deref(), Some("ctl"));
    assert!(
        plan.computes
            .iter()
            .all(|c| c.source == NodeSource::Bucket && c.snapshot_id.as_deref() == Some("cpt"))
    );
    // An unknown bucket named in the request is an error, not a silent miss.
    assert!(
        manager
            .preview_release(&ReleaseCreateRequest {
                bucket: Some("nope".into()),
                ..by_release("r", 1)
            })
            .await
            .unwrap_err()
            .is::<NotFound>()
    );

    let job = manager.create_release(by_release("r", 2)).await.unwrap();
    let actions: Vec<_> = job.assembly.iter().map(|s| s.action.as_str()).collect();
    assert_eq!(actions, ["import", "import", "restore", "expand", "expand"]);
    let finished = done_slowly(&manager, &job).await;
    assert_eq!(
        finished.status,
        JobStatus::Succeeded,
        "{:?}",
        finished.error
    );
    assert_eq!(
        manager.snapshot("ctl").unwrap().template,
        Some(TemplateRole::Control)
    );
    assert_eq!(
        manager.snapshot("cpt").unwrap().status,
        SnapshotStatus::Ready
    );
    assert_eq!(manager.get("r").unwrap().plan.unwrap().computes, 2);
    assert_eq!(backend.reconfigured.lock().unwrap().len(), 2);

    // The imports are local templates now and are used as they are.
    let plan = manager
        .preview_release(&by_release("again", 1))
        .await
        .unwrap();
    assert_eq!(plan.control.source, NodeSource::Template);
    assert_eq!(plan.computes[0].source, NodeSource::Template);

    // An import of the same archive that is still running is waited for, not repeated.
    manager.forget_snapshot("ctl").await.unwrap();
    manager
        .import_snapshot(ImportRequest {
            bucket: Some("minio".into()),
            key: Some("snapshots/ctl.msb".into()),
            id: Some("running".into()),
            ..Default::default()
        })
        .unwrap();
    let plan = manager
        .preview_release(&by_release("again", 1))
        .await
        .unwrap();
    if manager.snapshot("running").unwrap().status == SnapshotStatus::Importing {
        assert_eq!(plan.control.snapshot_id.as_deref(), Some("running"));
    }
    let job = manager
        .create_release(by_release("again", 1))
        .await
        .unwrap();
    let finished = done_slowly(&manager, &job).await;
    assert_eq!(
        finished.status,
        JobStatus::Succeeded,
        "{:?}",
        finished.error
    );
    assert_eq!(
        manager.get("again").unwrap().snapshot_id.as_deref(),
        Some("running")
    );
    assert!(manager.snapshot("ctl").is_err());
}

#[tokio::test]
async fn release_routes_preview_and_start_farms() {
    let dir = tempfile::tempdir().unwrap();
    let (manager, _) = release_templates(dir.path()).await;
    let app = api::router(manager.clone(), None);
    let call = |method: &str, uri: &str, body: Option<serde_json::Value>| {
        let mut builder = Request::builder().method(method).uri(uri);
        if body.is_some() {
            builder = builder.header("content-type", "application/json");
        }
        builder
            .body(body.map_or_else(Body::empty, |b| Body::from(b.to_string())))
            .unwrap()
    };
    let response = app
        .clone()
        .oneshot(call("GET", "/api/v1/releases", None))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let catalog: serde_json::Value =
        serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap();
    assert_eq!(catalog["releases"][0]["local_control"], 2);
    let request = serde_json::json!({
        "id": "r", "release": crate::kolla::OPENSTACK_RELEASE, "host_id": "local", "computes": 1
    });
    let response = app
        .clone()
        .oneshot(call("POST", "/api/v1/releases/plan", Some(request.clone())))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let plan: serde_json::Value =
        serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap();
    assert_eq!(plan["control"]["source"], "template");
    assert_eq!(plan["computes"][0]["snapshot_id"], "cpt");
    let response = app
        .clone()
        .oneshot(call("POST", "/api/v1/releases/farms", Some(request)))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let job: Job =
        serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap();
    assert_eq!(job.kind, JobKind::Assemble);
    assert_eq!(
        done_slowly(&manager, &job).await.status,
        JobStatus::Succeeded
    );
    let response = app
        .oneshot(call(
            "POST",
            "/api/v1/releases/plan",
            Some(serde_json::json!({"id": "x", "release": "1999.1", "host_id": "local"})),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CONFLICT);
}
