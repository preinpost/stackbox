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

#[derive(Default)]
pub(crate) struct Fake {
    pub(crate) fail_create: AtomicBool,
    farms: Mutex<BTreeMap<String, Vec<FarmNode>>>,
    /// msb snapshot references and their labels.
    snapshots: Mutex<BTreeMap<String, BTreeMap<String, String>>>,
    /// Exported archive contents: the labels of the snapshot written there.
    archives: Mutex<BTreeMap<std::path::PathBuf, BTreeMap<String, String>>>,
    /// `(from, to)` of each settle call.
    settled: Mutex<Vec<Option<(Exposure, Exposure)>>>,
    deploys: AtomicUsize,
    max_deploys: AtomicUsize,
    fail: AtomicBool,
    deployment_gate: Option<Semaphore>,
    scan_delay: Mutex<Option<(Arc<tokio::sync::Notify>, Arc<tokio::sync::Notify>)>>,
}
impl Backend for Fake {
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
                    name: format!("{id}-{}", spec.role),
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
            let active = self.deploys.fetch_add(1, Ordering::SeqCst) + 1;
            self.max_deploys.fetch_max(active, Ordering::SeqCst);
            sink(DeployEvent::Phase(from))?;
            sink(DeployEvent::Output("test deployment output".into()))?;
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
        labels: BTreeMap<String, String>,
    ) -> BoxFuture<'_, Result<crate::farm::FrozenSnapshot>> {
        Box::pin(async move {
            if self.fail.load(Ordering::SeqCst) {
                bail!("intentional freeze failure");
            }
            assert!(self.farms.lock().unwrap().contains_key(&id));
            let reference = format!("fake/{snapshot_id}");
            self.snapshots
                .lock()
                .unwrap()
                .insert(reference.clone(), labels);
            Ok(crate::farm::FrozenSnapshot {
                reference,
                size_bytes: Some(4096),
                restart_error: None,
            })
        })
    }
    fn restore(
        &self,
        id: String,
        reference: String,
        plan: Plan,
        exposure: Exposure,
    ) -> BoxFuture<'_, Result<Vec<FarmNode>>> {
        Box::pin(async move {
            assert!(self.snapshots.lock().unwrap().contains_key(&reference));
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
    fn export_snapshot(
        &self,
        reference: String,
        output: std::path::PathBuf,
    ) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            let labels = self.snapshots.lock().unwrap()[&reference].clone();
            std::fs::write(&output, b"archive")?;
            // The manager renames the partial file; the labels travel with it.
            let published = output.display().to_string();
            let published = published.strip_suffix(".partial").unwrap_or(&published);
            self.archives
                .lock()
                .unwrap()
                .insert(published.into(), labels);
            Ok(())
        })
    }
    fn import_snapshot(
        &self,
        archive: std::path::PathBuf,
        snapshot_id: String,
    ) -> BoxFuture<'_, Result<crate::farm::ImportedSnapshot>> {
        Box::pin(async move {
            let labels = self
                .archives
                .lock()
                .unwrap()
                .get(&archive)
                .cloned()
                .unwrap_or_default();
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
}
fn options(dir: &Path) -> ManagerOptions {
    ManagerOptions {
        state_dir: dir.into(),
        memory_budget_mib: 50_000,
        agent_binary: None,
        public_bind: "127.0.0.1".parse().unwrap(),
    }
}
fn request(id: &str) -> CreateRequest {
    CreateRequest {
        id: id.into(),
        host_id: None,
        components: vec![],
        external_host: "localhost".into(),
        owner: Some("test".into()),
        expires_at: None,
        snapshot_id: None,
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
    let manager = FarmManager::with_backend(opts, backend.clone())
        .await
        .unwrap();
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
async fn http_remote_host_registration_validation_job_locations_and_unregistration() {
    use crate::remote::test_server::Fixture;
    let fixture = Fixture::start().await;
    let dir = tempfile::tempdir().unwrap();
    let manager = FarmManager::with_backend(options(dir.path()), Arc::new(Fake::default()))
        .await
        .unwrap();
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
    assert!(manager.hosts().unwrap().is_empty());
    settle(&manager).await;
}

#[tokio::test]
async fn remote_host_preparation_is_serialized_idempotent_and_secrets_never_reach_state_or_api() {
    use crate::remote::{HostJobKind, test_server::Fixture};
    let fixture = Fixture::start().await;
    let dir = tempfile::tempdir().unwrap();
    let manager = FarmManager::with_backend(options(dir.path()), Arc::new(Fake::default()))
        .await
        .unwrap();
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
    assert_eq!(calls.iter().filter(|c| *c == "golden").count(), 1);
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
    let manager = FarmManager::with_backend(options(dir.path()), Arc::new(Fake::default()))
        .await
        .unwrap();
    let job = manager
        .start_host_job("target", HostJobKind::Check)
        .unwrap();
    assert_eq!(
        host_done(&manager, &job.id).await.status,
        JobStatus::Succeeded
    );
    manager.unregister_host("target").unwrap();
    assert!(manager.hosts().unwrap().is_empty() && manager.host_jobs().unwrap().is_empty());
    settle(&manager).await;
    assert!(
        manager
            .register_host(fixture.registration("after-shutdown"))
            .unwrap_err()
            .is::<Conflict>()
    );
}

#[tokio::test]
async fn remote_host_restart_recovers_interrupted_jobs_and_missing_key_never_replaces_secrets() {
    use crate::remote::{HostJobKind, test_server::Fixture};
    let fixture = Fixture::start().await;
    let dir = tempfile::tempdir().unwrap();
    let manager = FarmManager::with_backend(options(dir.path()), Arc::new(Fake::default()))
        .await
        .unwrap();
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
                },
            );
            Ok(())
        })
        .unwrap();
    drop(manager);
    let manager = FarmManager::with_backend(options(dir.path()), Arc::new(Fake::default()))
        .await
        .unwrap();
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
    let manager = FarmManager::with_backend(options(dir.path()), backend.clone())
        .await
        .unwrap();
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
    let manager = FarmManager::with_backend(options(dir.path()), backend.clone())
        .await
        .unwrap();
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
async fn deploy_queue_is_serial_and_farm_mutations_conflict() {
    let dir = tempfile::tempdir().unwrap();
    let backend = Arc::new(Fake {
        deployment_gate: Some(Semaphore::new(0)),
        ..Default::default()
    });
    let manager = FarmManager::with_backend(options(dir.path()), backend.clone())
        .await
        .unwrap();
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
    let manager = FarmManager::with_backend(options(dir.path()), backend.clone())
        .await
        .unwrap();
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
    let manager = FarmManager::with_backend(options(dir.path()), backend)
        .await
        .unwrap();
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
    let manager = FarmManager::with_backend(opts, backend.clone())
        .await
        .unwrap();
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
    let manager = FarmManager::with_backend(options(dir.path()), Arc::new(Fake::default()))
        .await
        .unwrap();
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
    let manager = FarmManager::with_backend(options(dir.path()), backend.clone())
        .await
        .unwrap();
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
async fn legacy_inventory_is_preserved_but_only_single_vm_farms_can_deploy() {
    let dir = tempfile::tempdir().unwrap();
    let backend = Arc::new(Fake::default());
    let roles = [
        Role::Deployer,
        Role::Lb,
        Role::Infra,
        Role::Keystone,
        Role::Glance,
        Role::Control,
        Role::Compute,
    ];
    let legacy: Vec<_> = roles
        .iter()
        .map(|role| FarmNode {
            name: format!("legacy-{role}"),
            role: Some(*role),
            status: "running".into(),
            memory_mib: 512,
            ip: Some("10.250.0.1".into()),
        })
        .collect();
    backend
        .farms
        .lock()
        .unwrap()
        .insert("legacy".into(), legacy.clone());
    let manager = FarmManager::with_backend(options(dir.path()), backend.clone())
        .await
        .unwrap();
    let farm = manager.get("legacy").unwrap();
    assert_eq!(farm.status, FarmStatus::Created);
    assert_eq!(farm.nodes.len(), 7);
    assert_eq!(farm.memory_mib(), 3584);
    let error = manager
        .deploy("legacy", DeployRequest::default())
        .unwrap_err();
    assert!(error.is::<Conflict>() && error.to_string().contains("legacy multi-node"));
    assert_eq!(backend.farms.lock().unwrap()["legacy"].len(), 7);
    let created = manager.create(request("single")).unwrap();
    assert_eq!(done(&manager, &created).await.status, JobStatus::Succeeded);
    let deployed = manager.deploy("single", DeployRequest::default()).unwrap();
    assert_eq!(done(&manager, &deployed).await.status, JobStatus::Succeeded);
    let deleted = manager.delete("legacy").unwrap();
    assert_eq!(done(&manager, &deleted).await.status, JobStatus::Succeeded);
    assert!(!backend.farms.lock().unwrap().contains_key("legacy"));
    assert_eq!(manager.get("single").unwrap().nodes.len(), 1);
    settle(&manager).await;
}

#[tokio::test]
async fn snapshots_freeze_restore_export_import_and_delete() {
    let dir = tempfile::tempdir().unwrap();
    let backend = Arc::new(Fake::default());
    let manager = FarmManager::with_backend(options(dir.path()), backend.clone())
        .await
        .unwrap();
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
            },
        )
    };
    assert!(freeze("../bad").unwrap_err().is::<BadRequest>());
    let job = freeze("clean").unwrap();
    assert_eq!(job.kind, JobKind::Snapshot);
    assert!(freeze("other").unwrap_err().is::<Conflict>());
    assert_eq!(done(&manager, &job).await.status, JobStatus::Succeeded);
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
                    path: Some("relative.msb".into())
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
    assert_eq!(archive.size_bytes, 7);
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
            path: path.display().to_string(),
            id: id.map(Into::into),
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
    let manager = FarmManager::with_backend(options(dir.path()), backend.clone())
        .await
        .unwrap();
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
async fn settle_rechecks_a_restored_farm_without_redeploying() {
    let dir = tempfile::tempdir().unwrap();
    let backend = Arc::new(Fake::default());
    let manager = FarmManager::with_backend(options(dir.path()), backend.clone())
        .await
        .unwrap();
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
    let manager = FarmManager::with_backend(options(dir.path()), Arc::new(Fake::default()))
        .await
        .unwrap();
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
