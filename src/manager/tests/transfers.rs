//! Snapshots copied to the hosts that boot templates.
use super::*;
use crate::{
    model::{CopySource, snapshot_labels as label},
    remote::{RegisterAgentHost, agent::Agent, dial},
    s3::{RegisterBucket, fake::FakeS3},
};

/// A snapshot the local fake keeps at `fake/<id>`, recorded as ready.
fn keep_snapshot(manager: &FarmManager, backend: &Fake, id: &str, template: Option<TemplateRole>) {
    let mut labels = BTreeMap::from([
        (label::ID.to_string(), id.to_string()),
        (label::ARCH.to_string(), current_arch()),
    ]);
    if let Some(role) = template {
        labels.insert(label::ROLE.into(), role.as_str().into());
    }
    backend
        .snapshots
        .lock()
        .unwrap()
        .insert(format!("fake/{id}"), labels);
    let plan = match template {
        Some(TemplateRole::Compute) => layout(&PlanRequest {
            components: vec![],
            computes: 1,
            compute: None,
            layout: None,
            release: None,
            gpu: None,
        })
        .unwrap(),
        _ => plan(&PlanRequest::default()).unwrap(),
    };
    manager
        .update(|state| {
            state.snapshots.insert(
                id.into(),
                SnapshotRecord {
                    id: id.into(),
                    status: SnapshotStatus::Ready,
                    arch: current_arch(),
                    platform: None,
                    host_id: None,
                    source_farm: None,
                    plan,
                    deployed: false,
                    source_exposure: Exposure {
                        host: "localhost".into(),
                        bind: "127.0.0.1".parse().unwrap(),
                        port_base: 15000,
                    },
                    note: None,
                    reference: Some(format!("fake/{id}")),
                    nodes: Vec::new(),
                    template,
                    node: template.map(|role| match role {
                        TemplateRole::Control => "control".into(),
                        TemplateRole::Compute => "compute-1".into(),
                    }),
                    legacy_compute_template: false,
                    release: Some(crate::kolla::OPENSTACK_RELEASE.into()),
                    size_bytes: Some(4096),
                    archive: None,
                    transfer: None,
                    copies: Vec::new(),
                    error: None,
                    created_at: now(),
                    updated_at: now(),
                },
            );
            Ok(())
        })
        .unwrap();
}

/// What the fake exports for a snapshot: its labels.
fn archive_of(backend: &Fake, id: &str) -> Vec<u8> {
    serde_json::to_vec(&backend.snapshots.lock().unwrap()[&format!("fake/{id}")]).unwrap()
}
fn sha256(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    format!("{:x}", Sha256::digest(bytes))
}

struct DialIn {
    remote: Arc<Fake>,
    server: tokio::task::JoinHandle<std::io::Result<()>>,
    dialling: tokio::task::JoinHandle<Result<()>>,
}
impl Drop for DialIn {
    fn drop(&mut self) {
        self.dialling.abort();
        self.server.abort();
    }
}
/// Host `edge`, whose real agent dials in and runs on its own fake runtime.
async fn dial_in(manager: &FarmManager, dir: &Path) -> DialIn {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server =
        tokio::spawn(axum::serve(listener, api::router(manager.clone(), None)).into_future());
    let registered = manager
        .register_agent_host(RegisterAgentHost { id: "edge".into() })
        .unwrap();
    let url = dial::connect_url(&format!("http://{address}")).unwrap();
    let remote = Arc::new(Fake::default());
    let agent = Arc::new(Agent::in_process(dir.join("edge-agent"), remote.clone()));
    let dialling = tokio::spawn(async move { dial::serve(&url, &registered.token, agent).await });
    tokio::time::timeout(Duration::from_secs(5), async {
        while !manager.host("edge").unwrap().connected {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    let check = manager
        .start_host_job("edge", crate::remote::HostJobKind::Check)
        .unwrap();
    assert_eq!(
        host_done(manager, &check.id).await.status,
        JobStatus::Succeeded
    );
    manager
        .update(|state| {
            let host = &mut state.hosts.get_mut("edge").unwrap().host;
            host.status = crate::remote::HostStatus::Ready;
            let inspection = host.inspection.as_mut().unwrap();
            inspection.memory_total_mib = 65536;
            inspection.os = "linux".into();
            Ok(())
        })
        .unwrap();
    DialIn {
        remote,
        server,
        dialling,
    }
}

async fn bucket(manager: &FarmManager) -> Arc<FakeS3> {
    let s3 = Arc::new(FakeS3::default());
    let endpoint = s3.start().await;
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
            secret_access_key: Some("bucket-secret-never-sent".into()),
        })
        .unwrap();
    s3
}
/// Put `bytes` in the bucket as the snapshot's last export, said to hash to `sha256`.
fn exported(manager: &FarmManager, s3: &FakeS3, id: &str, bytes: Vec<u8>, sha256: String) {
    let key = format!("snapshots/{id}.msb");
    let size_bytes = bytes.len() as u64;
    s3.objects.lock().unwrap().insert(key.clone(), bytes);
    manager
        .update(|state| {
            state.snapshots.get_mut(id).unwrap().archive = Some(SnapshotArchive {
                path: format!("s3://farm/{key}"),
                size_bytes,
                exported_at: now(),
                bucket: Some("minio".into()),
                key: Some(key),
                sha256: Some(sha256),
            });
            Ok(())
        })
        .unwrap();
}

#[tokio::test]
async fn a_snapshot_is_streamed_to_a_host_once_reused_and_removed_with_it() {
    let dir = tempfile::tempdir().unwrap();
    let backend = Arc::new(Fake::default());
    let manager = open_with(options(dir.path()), backend.clone()).await;
    let edge = dial_in(&manager, dir.path()).await;
    keep_snapshot(&manager, &backend, "clean", None);

    // The local host keeps it already.
    assert_eq!(
        manager
            .ensure_snapshot_on_host("clean", "local", None)
            .await
            .unwrap(),
        "fake/clean"
    );
    let lines = Arc::new(Mutex::new(Vec::new()));
    let sink: EventSink = {
        let lines = lines.clone();
        Arc::new(move |event| {
            if let DeployEvent::Output(line) = event {
                lines.lock().unwrap().push(line);
            }
            Ok(())
        })
    };
    let reference = manager
        .ensure_snapshot_on_host("clean", "edge", Some(sink))
        .await
        .unwrap();
    assert_eq!(reference, "fake/imported/clean");
    assert!(
        edge.remote
            .snapshots
            .lock()
            .unwrap()
            .contains_key(&reference)
    );
    let lines = lines.lock().unwrap().join("");
    assert!(lines.contains("streaming 0 MiB to host edge"), "{lines}");
    assert!(lines.contains("snapshot clean is on host edge"), "{lines}");
    let copies = manager.snapshot("clean").unwrap().copies;
    assert_eq!(copies.len(), 1);
    assert_eq!(
        (copies[0].host_id.as_str(), copies[0].via),
        ("edge", CopySource::Stream)
    );
    // The staged archive is gone once loaded.
    let staged = dir.path().join("edge-agent/transfers");
    assert_eq!(std::fs::read_dir(&staged).unwrap().count(), 0);
    assert!(
        !dir.path()
            .join("transfers")
            .read_dir()
            .unwrap()
            .any(|_| true)
    );

    // The host's copy is used again; a lost one is copied again.
    edge.remote.snapshots.lock().unwrap().insert(
        "fake/imported/clean".into(),
        BTreeMap::from([(label::ID.into(), "marked".into())]),
    );
    manager
        .ensure_snapshot_on_host("clean", "edge", None)
        .await
        .unwrap();
    assert_eq!(
        edge.remote.snapshots.lock().unwrap()["fake/imported/clean"][label::ID],
        "marked"
    );
    assert_eq!(
        manager.snapshot("clean").unwrap().copies[0].via,
        CopySource::Stream
    );
    edge.remote.snapshots.lock().unwrap().clear();
    manager
        .ensure_snapshot_on_host("clean", "edge", None)
        .await
        .unwrap();
    assert_eq!(
        edge.remote.snapshots.lock().unwrap()["fake/imported/clean"][label::ID],
        "clean"
    );

    // A farm on that host starts from its copy.
    let mut on_edge = request("copy");
    on_edge.host_id = Some("edge".into());
    on_edge.snapshot_id = Some("clean".into());
    let restored = done(&manager, &manager.create(on_edge).unwrap()).await;
    assert_eq!(
        restored.status,
        JobStatus::Succeeded,
        "{:?}",
        restored.error
    );
    assert!(edge.remote.farms.lock().unwrap().contains_key("copy"));
    let deleted = manager.delete("copy").unwrap();
    assert_eq!(done(&manager, &deleted).await.status, JobStatus::Succeeded);

    // Removing the copy leaves the snapshot; deleting the snapshot removes a new copy.
    let snapshot = manager.remove_snapshot_copy("clean", "edge").await.unwrap();
    assert!(snapshot.copies.is_empty());
    assert!(edge.remote.snapshots.lock().unwrap().is_empty());
    assert!(
        manager
            .remove_snapshot_copy("clean", "edge")
            .await
            .unwrap_err()
            .is::<NotFound>()
    );
    manager
        .ensure_snapshot_on_host("clean", "edge", None)
        .await
        .unwrap();
    assert!(manager.delete_snapshot("clean").await.unwrap().is_empty());
    assert!(edge.remote.snapshots.lock().unwrap().is_empty());
    assert!(!backend.snapshots.lock().unwrap().contains_key("fake/clean"));
    drop(edge);
    settle(&manager).await;
}

#[tokio::test]
async fn a_host_downloads_a_bucket_archive_itself_and_falls_back_to_streaming_on_a_mismatch() {
    let dir = tempfile::tempdir().unwrap();
    let backend = Arc::new(Fake::default());
    let manager = open_with(options(dir.path()), backend.clone()).await;
    let edge = dial_in(&manager, dir.path()).await;
    let s3 = bucket(&manager).await;
    keep_snapshot(&manager, &backend, "tmpl", Some(TemplateRole::Compute));
    let archive = archive_of(&backend, "tmpl");
    exported(&manager, &s3, "tmpl", archive.clone(), sha256(&archive));
    // The local copy is not read: a host that downloads needs no export.
    backend.snapshots.lock().unwrap().remove("fake/tmpl");
    let reference = manager
        .ensure_snapshot_on_host("tmpl", "edge", None)
        .await
        .unwrap();
    assert_eq!(
        manager.snapshot("tmpl").unwrap().copies[0].via,
        CopySource::Bucket
    );
    assert_eq!(
        edge.remote.snapshots.lock().unwrap()[&reference][label::ID],
        "tmpl"
    );
    manager.remove_snapshot_copy("tmpl", "edge").await.unwrap();

    // Other bytes in the bucket are refused; the coordinator streams its own copy instead.
    backend.snapshots.lock().unwrap().insert(
        "fake/tmpl".into(),
        serde_json::from_slice(&archive).unwrap(),
    );
    s3.objects
        .lock()
        .unwrap()
        .insert("snapshots/tmpl.msb".into(), b"replaced".to_vec());
    let lines = Arc::new(Mutex::new(String::new()));
    let sink: EventSink = {
        let lines = lines.clone();
        Arc::new(move |event| {
            if let DeployEvent::Output(line) = event {
                lines.lock().unwrap().push_str(&line);
            }
            Ok(())
        })
    };
    manager
        .ensure_snapshot_on_host("tmpl", "edge", Some(sink))
        .await
        .unwrap();
    let lines = lines.lock().unwrap().clone();
    assert!(
        lines.contains("could not download it from the bucket"),
        "{lines}"
    );
    assert!(lines.contains("SHA-256"), "{lines}");
    assert!(!lines.contains("bucket-secret-never-sent"));
    assert_eq!(
        manager.snapshot("tmpl").unwrap().copies[0].via,
        CopySource::Stream
    );
    drop(edge);
    settle(&manager).await;
}

#[tokio::test]
async fn an_archive_with_other_bytes_than_announced_is_never_loaded() {
    let dir = tempfile::tempdir().unwrap();
    let remote = Arc::new(Fake::default());
    let agent = Agent::in_process(dir.path().join("agent"), remote.clone());
    let coordinator = uuid::Uuid::new_v4().to_string();
    let request = |operation| crate::remote::protocol::Request {
        version: crate::remote::protocol::VERSION,
        coordinator_id: coordinator.clone(),
        operation,
    };
    let emit: crate::remote::agent::Emitter = Arc::new(|_| Ok(()));
    let transfer_id = uuid::Uuid::new_v4().to_string();
    let body = b"{\"farm.snapshot.id\":\"t\"}".to_vec();
    let line = |bytes: &[u8]| {
        use base64::{Engine, engine::general_purpose::STANDARD};
        let mut line = serde_json::to_vec(&crate::remote::protocol::TerminalFrame::Input {
            data: STANDARD.encode(bytes),
        })
        .unwrap();
        line.push(b'\n');
        line
    };
    let receive = |sha256: String| {
        request(crate::remote::protocol::Operation::ReceiveArchive {
            transfer_id: transfer_id.clone(),
            sha256,
            size_bytes: body.len() as u64,
        })
    };
    let error = agent
        .terminal(&receive("0".repeat(64)), &line(&body)[..], emit.clone())
        .await
        .unwrap_err();
    assert!(
        format!("{error:#}").contains("it is not imported"),
        "{error:#}"
    );
    let import = request(crate::remote::protocol::Operation::ImportArchive {
        transfer_id: transfer_id.clone(),
        snapshot_id: "t".into(),
    });
    let error = agent
        .execute(import.clone(), emit.clone())
        .await
        .unwrap_err();
    assert!(
        format!("{error:#}").contains("no verified archive"),
        "{error:#}"
    );
    assert!(remote.snapshots.lock().unwrap().is_empty());
    // A longer stream than announced stops early.
    let mut longer = line(&body);
    longer.extend(line(b"more"));
    assert!(
        agent
            .terminal(&receive(sha256(&body)), &longer[..], emit.clone())
            .await
            .is_err()
    );
    agent
        .terminal(&receive(sha256(&body)), &line(&body)[..], emit.clone())
        .await
        .unwrap();
    let crate::remote::protocol::Reply::Imported { snapshot } =
        agent.execute(import, emit.clone()).await.unwrap()
    else {
        panic!("not imported");
    };
    let found = agent
        .execute(
            request(crate::remote::protocol::Operation::FindSnapshot {
                snapshot_id: "t".into(),
            }),
            emit.clone(),
        )
        .await
        .unwrap();
    assert!(matches!(
        found,
        crate::remote::protocol::Reply::Found { reference: Some(ref r), .. } if *r == snapshot.reference
    ));
    // Another coordinator does not see it.
    let other = crate::remote::protocol::Request {
        coordinator_id: uuid::Uuid::new_v4().to_string(),
        ..request(crate::remote::protocol::Operation::FindSnapshot {
            snapshot_id: "t".into(),
        })
    };
    assert!(matches!(
        agent.execute(other, emit).await.unwrap(),
        crate::remote::protocol::Reply::Found {
            reference: None,
            ..
        }
    ));
}

/// An SSH host prepared with the fixture's agent.
async fn ssh_host(manager: &FarmManager, fixture: &crate::remote::test_server::Fixture) {
    use crate::remote::{AgentArtifactInfo, HostJobKind, InstalledAgent};
    manager.register_host(fixture.registration("edge")).unwrap();
    let prep = manager
        .start_host_job("edge", HostJobKind::Prepare)
        .unwrap();
    assert_eq!(
        host_done(manager, &prep.id).await.status,
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
}

#[tokio::test]
async fn an_agent_from_before_template_transfers_is_named_and_left_alone() {
    let fixture = crate::remote::test_server::Fixture::start().await;
    fixture.observed.legacy_agent.store(true, Ordering::SeqCst);
    let dir = tempfile::tempdir().unwrap();
    let backend = Arc::new(Fake::default());
    let manager = open_with(options(dir.path()), backend.clone()).await;
    ssh_host(&manager, &fixture).await;
    keep_snapshot(&manager, &backend, "tmpl", Some(TemplateRole::Compute));
    let error = manager
        .ensure_snapshot_on_host("tmpl", "edge", None)
        .await
        .unwrap_err();
    assert!(error.is::<Conflict>());
    assert!(
        error
            .to_string()
            .contains("the farm-agent on host 'edge' predates template transfers"),
        "{error}"
    );
    let calls = fixture.observed.calls.lock().unwrap().clone();
    assert!(
        !calls.iter().any(|call| call.contains("FindSnapshot")),
        "{calls:?}"
    );
    assert!(manager.snapshot("tmpl").unwrap().copies.is_empty());
    settle(&manager).await;
}

#[tokio::test]
async fn a_compute_on_a_remote_host_boots_from_a_template_the_host_downloads() {
    let fixture = crate::remote::test_server::Fixture::start().await;
    let dir = tempfile::tempdir().unwrap();
    let backend = Arc::new(Fake::default());
    let mut opts = options(dir.path());
    opts.memory_budget_mib = 64 * 1024;
    let manager = open_with(opts, backend.clone()).await;
    if !cfg!(target_os = "linux") {
        *manager.0.guest_agent.write().unwrap() = Some(dir.path().join("farm-agent"));
    }
    ssh_host(&manager, &fixture).await;
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
    let s3 = bucket(&manager).await;

    let created = manager
        .create(CreateRequest {
            computes: 1,
            ..request("span")
        })
        .unwrap();
    assert_eq!(done(&manager, &created).await.status, JobStatus::Succeeded);
    let deployed = manager.deploy("span", DeployRequest::default()).unwrap();
    assert_eq!(done(&manager, &deployed).await.status, JobStatus::Succeeded);
    keep_snapshot(&manager, &backend, "tmpl", Some(TemplateRole::Compute));
    let components = manager.get("span").unwrap().plan.unwrap().components;
    manager
        .update(|state| {
            state.snapshots.get_mut("tmpl").unwrap().plan.components = components;
            Ok(())
        })
        .unwrap();
    let archive = archive_of(&backend, "tmpl");
    exported(&manager, &s3, "tmpl", archive.clone(), sha256(&archive));

    let job = manager
        .add_compute(
            "span",
            AddComputeRequest {
                host_id: Some("edge".into()),
                from_snapshot: Some("tmpl".into()),
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
    let calls = fixture.observed.calls.lock().unwrap().clone();
    let fetched = calls
        .iter()
        .find(|call| call.starts_with("fetch:"))
        .expect("the host downloads the archive");
    assert!(fetched.ends_with("/farm/snapshots/tmpl.msb"), "{fetched}");
    assert!(
        calls.contains(&"create-from:ssh/tmpl".to_string()),
        "{calls:?}"
    );
    assert!(calls.contains(&"share:compute-2".to_string()));
    let copies = manager.snapshot("tmpl").unwrap().copies;
    assert_eq!(
        (
            copies[0].host_id.as_str(),
            copies[0].reference.as_str(),
            copies[0].via
        ),
        ("edge", "ssh/tmpl", CopySource::Bucket)
    );
    // Kolla only reconfigures it, from the control VM's host.
    assert_eq!(
        backend.reconfigured.lock().unwrap().as_slice(),
        &[("span".into(), 2)]
    );
    let log: String = manager
        .logs("span", 0, 1000)
        .unwrap()
        .into_iter()
        .map(|entry| entry.message)
        .collect();
    assert!(
        log.contains("host edge downloads snapshots/tmpl.msb from bucket minio"),
        "{log}"
    );
    assert!(!log.contains("bucket-secret-never-sent"));

    // Deleting the snapshot removes the copy too, while the farm keeps its VM.
    assert!(manager.delete_snapshot("tmpl").await.unwrap().is_empty());
    assert!(fixture.observed.cached.lock().unwrap().is_empty());
    assert!(
        fixture
            .observed
            .calls
            .lock()
            .unwrap()
            .contains(&"remove-snapshot:ssh/tmpl".to_string())
    );
    settle(&manager).await;
}

/// The other architecture than this machine's, for a host that cannot boot its guests.
fn foreign_arch() -> String {
    if current_arch() == "aarch64" {
        "x86_64".into()
    } else {
        "aarch64".into()
    }
}
fn set_arch(manager: &FarmManager, host: &str, arch: &str) {
    manager
        .update(|state| {
            let host = &mut state.hosts.get_mut(host).unwrap().host;
            host.inspection.as_mut().unwrap().arch = arch.into();
            Ok(())
        })
        .unwrap();
}
/// Farm `id` on host `edge`, frozen whole into snapshot `snapshot`.
async fn freeze_on_edge(manager: &FarmManager, id: &str, snapshot: &str) {
    if manager.get(id).is_err() {
        let mut on_edge = request(id);
        on_edge.host_id = Some("edge".into());
        let created = done(manager, &manager.create(on_edge).unwrap()).await;
        assert_eq!(created.status, JobStatus::Succeeded, "{:?}", created.error);
    }
    let frozen = manager
        .freeze(
            id,
            SnapshotRequest {
                id: snapshot.into(),
                note: None,
                node: None,
            },
        )
        .unwrap();
    let frozen = done(manager, &frozen).await;
    assert_eq!(frozen.status, JobStatus::Succeeded, "{:?}", frozen.error);
}

#[tokio::test]
async fn a_snapshot_frozen_on_a_remote_host_stays_there_and_is_copied_and_exported_from_it() {
    let dir = tempfile::tempdir().unwrap();
    let backend = Arc::new(Fake::default());
    let manager = open_with(options(dir.path()), backend.clone()).await;
    let edge = dial_in(&manager, dir.path()).await;
    // The host's architecture is recorded, not this coordinator's.
    set_arch(&manager, "edge", &foreign_arch());
    freeze_on_edge(&manager, "far", "remote").await;
    let snapshot = manager.snapshot("remote").unwrap();
    assert_eq!(snapshot.host_id.as_deref(), Some("edge"));
    assert_eq!(snapshot.arch, foreign_arch());
    assert_eq!(snapshot.platform.as_deref(), Some("linux"));
    assert_eq!(snapshot.reference.as_deref(), Some("fake/remote"));
    assert_eq!(
        edge.remote.snapshots.lock().unwrap()["fake/remote"][label::ARCH],
        foreign_arch()
    );
    assert!(backend.snapshots.lock().unwrap().is_empty());

    // Its own host keeps the original; this machine cannot boot it.
    assert_eq!(
        manager
            .ensure_snapshot_on_host("remote", "edge", None)
            .await
            .unwrap(),
        "fake/remote"
    );
    let error = manager
        .ensure_snapshot_on_host("remote", "local", None)
        .await
        .unwrap_err();
    assert!(error.is::<Conflict>(), "{error:#}");
    let mut here = request("here");
    here.snapshot_id = Some("remote".into());
    let error = manager.create(here.clone()).unwrap_err();
    assert!(
        error.to_string().contains("cannot boot on host 'local'"),
        "{error}"
    );

    // On a host of the same architecture, it is streamed from the host that froze it.
    set_arch(&manager, "edge", &current_arch());
    manager
        .update(|state| {
            state.snapshots.get_mut("remote").unwrap().arch = current_arch();
            Ok(())
        })
        .unwrap();
    let reference = manager
        .ensure_snapshot_on_host("remote", "local", None)
        .await
        .unwrap();
    assert_eq!(reference, "fake/imported/remote");
    assert_eq!(
        backend.snapshots.lock().unwrap()[&reference],
        edge.remote.snapshots.lock().unwrap()["fake/remote"]
    );
    let copies = manager.snapshot("remote").unwrap().copies;
    assert_eq!(copies.len(), 1);
    assert_eq!(
        (copies[0].host_id.as_str(), copies[0].via),
        ("local", CopySource::Stream)
    );
    let restored = done(&manager, &manager.create(here).unwrap()).await;
    assert_eq!(
        restored.status,
        JobStatus::Succeeded,
        "{:?}",
        restored.error
    );
    assert!(backend.farms.lock().unwrap().contains_key("here"));
    done(&manager, &manager.delete("here").unwrap()).await;

    // An export reads the original on its host.
    let output = dir.path().join("remote.msb");
    manager
        .export_snapshot(
            "remote",
            ExportRequest {
                path: Some(output.display().to_string()),
                bucket: None,
                key: None,
            },
        )
        .unwrap();
    let exported = manager.wait_snapshot("remote").await.unwrap();
    assert_eq!(exported.error, None);
    assert_eq!(
        std::fs::read(&output).unwrap(),
        archive_of(&edge.remote, "remote")
    );
    assert_eq!(
        std::fs::read_dir(dir.path().join("edge-agent/transfers"))
            .unwrap()
            .count(),
        0
    );

    // Deleting it removes the original there and the copy here.
    assert!(manager.delete_snapshot("remote").await.unwrap().is_empty());
    assert!(
        !edge
            .remote
            .snapshots
            .lock()
            .unwrap()
            .contains_key("fake/remote")
    );
    assert!(backend.snapshots.lock().unwrap().is_empty());
    drop(edge);
    settle(&manager).await;
}

#[tokio::test]
async fn a_snapshot_recorded_before_snapshots_had_a_host_is_the_local_hosts() {
    let dir = tempfile::tempdir().unwrap();
    let backend = Arc::new(Fake::default());
    let manager = open_with(options(dir.path()), backend.clone()).await;
    keep_snapshot(&manager, &backend, "old", None);
    let mut record = serde_json::to_value(manager.snapshot("old").unwrap()).unwrap();
    assert!(record.get("host_id").is_none());
    record.as_object_mut().unwrap().remove("host_id");
    let loaded: SnapshotRecord = serde_json::from_value(record).unwrap();
    assert_eq!(loaded.host_id, None);
    assert_eq!(
        manager
            .ensure_snapshot_on_host("old", "local", None)
            .await
            .unwrap(),
        "fake/old"
    );
    let mut restore = request("again");
    restore.snapshot_id = Some("old".into());
    let restored = done(&manager, &manager.create(restore).unwrap()).await;
    assert_eq!(
        restored.status,
        JobStatus::Succeeded,
        "{:?}",
        restored.error
    );
    done(&manager, &manager.delete("again").unwrap()).await;
    manager.delete_snapshot("old").await.unwrap();
    assert!(backend.snapshots.lock().unwrap().is_empty());
    settle(&manager).await;
}

#[tokio::test]
async fn purging_a_remote_host_keeps_its_snapshots_unless_asked_and_forgets_copies_there() {
    let dir = tempfile::tempdir().unwrap();
    let backend = Arc::new(Fake::default());
    let manager = open_with(options(dir.path()), backend.clone()).await;
    let edge = dial_in(&manager, dir.path()).await;
    keep_snapshot(&manager, &backend, "clean", None);
    manager
        .ensure_snapshot_on_host("clean", "edge", None)
        .await
        .unwrap();
    freeze_on_edge(&manager, "far", "kept").await;
    manager
        .ensure_snapshot_on_host("kept", "local", None)
        .await
        .unwrap();

    let purge = manager.start_purge("edge", false).unwrap();
    let finished = host_done(&manager, &purge.id).await;
    assert_eq!(
        finished.status,
        JobStatus::Succeeded,
        "{:?}",
        finished.error
    );
    assert!(manager.host("edge").is_err());
    // The copy there went with the host; the snapshot frozen there stays recorded.
    assert!(manager.snapshot("clean").unwrap().copies.is_empty());
    assert_eq!(
        edge.remote
            .snapshots
            .lock()
            .unwrap()
            .keys()
            .collect::<Vec<_>>(),
        vec!["fake/kept"]
    );
    assert!(backend.snapshots.lock().unwrap().contains_key("fake/clean"));
    let kept = manager.snapshot("kept").unwrap();
    assert_eq!(kept.host_id.as_deref(), Some("edge"));
    // Its copy here still boots farms; the original cannot be read until the host returns.
    assert_eq!(
        manager
            .ensure_snapshot_on_host("kept", "local", None)
            .await
            .unwrap(),
        "fake/imported/kept"
    );
    let error = manager
        .export_snapshot(
            "kept",
            ExportRequest {
                path: Some(dir.path().join("kept.msb").display().to_string()),
                bucket: None,
                key: None,
            },
        )
        .unwrap_err();
    assert!(error.is::<Conflict>());
    assert!(
        error
            .to_string()
            .contains("is kept on host 'edge', which is not registered"),
        "{error}"
    );
    // Deleting it removes the copy here and the record.
    assert!(manager.delete_snapshot("kept").await.unwrap().is_empty());
    assert!(manager.snapshot("kept").unwrap_err().is::<NotFound>());
    assert!(
        !backend
            .snapshots
            .lock()
            .unwrap()
            .contains_key("fake/imported/kept")
    );
    drop(edge);
    settle(&manager).await;
}

#[tokio::test]
async fn purging_a_remote_host_with_its_snapshots_deletes_only_the_ones_frozen_there() {
    let dir = tempfile::tempdir().unwrap();
    let backend = Arc::new(Fake::default());
    let manager = open_with(options(dir.path()), backend.clone()).await;
    let edge = dial_in(&manager, dir.path()).await;
    keep_snapshot(&manager, &backend, "clean", None);
    manager
        .ensure_snapshot_on_host("clean", "edge", None)
        .await
        .unwrap();
    freeze_on_edge(&manager, "far", "gone").await;
    manager
        .ensure_snapshot_on_host("gone", "local", None)
        .await
        .unwrap();

    let purge = manager.start_purge("edge", true).unwrap();
    assert!(purge.delete_snapshots);
    let finished = host_done(&manager, &purge.id).await;
    assert_eq!(
        finished.status,
        JobStatus::Succeeded,
        "{:?}",
        finished.error
    );
    assert!(
        finished
            .events
            .iter()
            .any(|event| event.message == "Deleting 1 snapshot(s)"),
        "{:?}",
        finished.events
    );
    assert!(manager.snapshot("gone").unwrap_err().is::<NotFound>());
    assert!(edge.remote.snapshots.lock().unwrap().is_empty());
    // The local host's snapshot stays, without the copy the host had.
    let clean = manager.snapshot("clean").unwrap();
    assert!(clean.copies.is_empty());
    assert_eq!(
        backend.snapshots.lock().unwrap().keys().collect::<Vec<_>>(),
        vec!["fake/clean"]
    );
    drop(edge);
    settle(&manager).await;
}

#[tokio::test]
async fn a_release_plan_follows_the_architecture_of_the_control_vms_host() {
    let dir = tempfile::tempdir().unwrap();
    let manager = open_with(options(dir.path()), Arc::new(Fake::default())).await;
    let _edge = dial_in(&manager, dir.path()).await;
    let request = |host: &str, release: &str| ReleaseCreateRequest {
        id: "eol".into(),
        release: release.into(),
        host_id: Some(host.into()),
        computes: 1,
        external_host: "localhost".into(),
        ..Default::default()
    };
    // The 2024.x images the farm publishes are amd64 only, whichever host coordinates.
    set_arch(&manager, "edge", "x86_64");
    let plan = manager
        .preview_release(&request("edge", "2024.1"))
        .await
        .unwrap();
    assert_eq!(plan.arch, "x86_64");
    assert_eq!(plan.control.source, NodeSource::Clean);
    set_arch(&manager, "edge", "aarch64");
    let error = manager
        .preview_release(&request("edge", "2024.1"))
        .await
        .unwrap_err();
    assert!(error.is::<Conflict>(), "{error:#}");
    assert!(
        error.to_string().contains("host 'edge' is aarch64"),
        "{error:#}"
    );
    // Its computes share the control VM's architecture.
    let mut mixed = request("local", crate::kolla::OPENSTACK_RELEASE);
    mixed.compute_hosts = vec!["edge".into()];
    set_arch(&manager, "edge", &foreign_arch());
    let error = manager.preview_release(&mixed).await.unwrap_err();
    assert!(
        error.to_string().contains("cannot mix CPU architectures"),
        "{error:#}"
    );
}

#[tokio::test]
async fn a_farm_on_a_host_another_coordinator_publishes_on_takes_a_free_port_block() {
    let fixture = crate::remote::test_server::Fixture::start().await;
    fixture.observed.reserved_ports.lock().unwrap().push(15000);
    let dir = tempfile::tempdir().unwrap();
    let backend = Arc::new(Fake::default());
    let manager = open_with(options(dir.path()), backend.clone()).await;
    ssh_host(&manager, &fixture).await;
    let created = manager
        .create(CreateRequest {
            host_id: Some("edge".into()),
            external_host: "edge.example".into(),
            ..request("remote-farm")
        })
        .unwrap();
    assert_eq!(done(&manager, &created).await.status, JobStatus::Succeeded);
    let base = manager
        .get("remote-farm")
        .unwrap()
        .exposure
        .unwrap()
        .port_base;
    assert!(
        base.abs_diff(15000) >= crate::model::PUBLIC_PORT_BLOCK,
        "{base}"
    );
    settle(&manager).await;
}

#[tokio::test]
async fn a_snapshot_restored_on_a_host_another_coordinator_publishes_on_takes_a_free_port_block() {
    let fixture = crate::remote::test_server::Fixture::start().await;
    let dir = tempfile::tempdir().unwrap();
    let backend = Arc::new(Fake::default());
    let manager = open_with(options(dir.path()), backend.clone()).await;
    ssh_host(&manager, &fixture).await;
    keep_snapshot(&manager, &backend, "snap", None);
    manager
        .update(|state| {
            state.snapshots.get_mut("snap").unwrap().host_id = Some("edge".into());
            Ok(())
        })
        .unwrap();
    fixture.observed.reserved_ports.lock().unwrap().push(15000);
    let restored = manager
        .create(CreateRequest {
            host_id: Some("edge".into()),
            external_host: "edge.example".into(),
            snapshot_id: Some("snap".into()),
            ..request("restored")
        })
        .unwrap();
    let restored = done(&manager, &restored).await;
    assert_eq!(
        restored.status,
        JobStatus::Succeeded,
        "{:?}",
        restored.error
    );
    let base = manager.get("restored").unwrap().exposure.unwrap().port_base;
    assert!(
        base.abs_diff(15000) >= crate::model::PUBLIC_PORT_BLOCK,
        "{base}"
    );
    settle(&manager).await;
}

#[tokio::test]
async fn multi_node_snapshot_on_remote_host_exports_to_s3_and_imports_back() {
    let dir = tempfile::tempdir().unwrap();
    let backend = Arc::new(Fake::default());
    let manager = open_with(options(dir.path()), backend.clone()).await;
    let edge = dial_in(&manager, dir.path()).await;
    let s3 = Arc::new(crate::s3::fake::FakeS3::default());
    let endpoint = s3.start().await;
    manager
        .register_bucket(crate::s3::RegisterBucket {
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

    let mut create_req = request("mn-edge");
    create_req.host_id = Some("edge".into());
    create_req.components = vec!["keystone".into(), "nova".into()];
    create_req.computes = 1;
    create_req.compute = Some(ComputeSize {
        cpus: 4,
        memory_mib: 4096,
        disk_mib: 32768,
    });
    create_req.gpu = Some(GpuConfig {
        mode: GpuMode::Simulated,
        count: 2,
        traits: None,
        devices: None,
        resource_class: None,
    });
    let created = done(&manager, &manager.create(create_req).unwrap()).await;
    assert_eq!(created.status, JobStatus::Succeeded, "{:?}", created.error);
    let deployed = done(
        &manager,
        &manager.deploy("mn-edge", DeployRequest::default()).unwrap(),
    )
    .await;
    assert_eq!(
        deployed.status,
        JobStatus::Succeeded,
        "{:?}",
        deployed.error
    );

    let freeze = manager
        .freeze(
            "mn-edge",
            SnapshotRequest {
                id: "mn-remote".into(),
                note: Some("multi-node s3".into()),
                node: None,
            },
        )
        .unwrap();
    assert_eq!(done(&manager, &freeze).await.status, JobStatus::Succeeded);
    let original = manager.snapshot("mn-remote").unwrap();
    assert_eq!(original.host_id.as_deref(), Some("edge"));
    assert_eq!(original.nodes.len(), 2);

    // Export the remote multi-node snapshot to S3.
    manager
        .export_snapshot(
            "mn-remote",
            ExportRequest {
                bucket: Some("minio".into()),
                ..Default::default()
            },
        )
        .unwrap();
    let exported = manager.wait_snapshot("mn-remote").await.unwrap();
    assert_eq!(exported.error, None);
    let archive = exported.archive.unwrap();
    assert_eq!(archive.bucket.as_deref(), Some("minio"));
    assert_eq!(archive.key.as_deref(), Some("snapshots/mn-remote.msb"));
    assert!(archive.sha256.is_some());

    // Metadata file and bucket listing record computes > 0.
    let objects = manager.bucket_objects("minio").await.unwrap();
    assert_eq!(objects.len(), 1);
    let metadata = objects[0].metadata.as_ref().unwrap();
    assert_eq!(metadata.computes, 1);
    assert_eq!(metadata.role, None);

    // Staging directories are cleaned up on both coordinator and agent.
    assert_eq!(
        std::fs::read_dir(dir.path().join("transfers"))
            .unwrap()
            .count(),
        0
    );
    assert_eq!(
        std::fs::read_dir(dir.path().join("edge-agent/transfers"))
            .unwrap()
            .count(),
        0
    );

    // Import back from S3 onto the local host and check compute size, GPU config and VM list.
    manager
        .import_snapshot(ImportRequest {
            bucket: Some("minio".into()),
            key: Some("snapshots/mn-remote.msb".into()),
            id: Some("mn-from-s3".into()),
            ..Default::default()
        })
        .unwrap();
    let imported = manager.wait_snapshot("mn-from-s3").await.unwrap();
    assert_eq!(
        imported.status,
        SnapshotStatus::Ready,
        "{:?}",
        imported.error
    );
    assert_eq!(imported.plan.computes, 1);
    assert_eq!(imported.plan.compute, original.plan.compute);
    assert_eq!(imported.plan.gpu, original.plan.gpu);
    assert_eq!(imported.note.as_deref(), Some("multi-node s3"));
    assert_eq!(
        imported
            .nodes
            .iter()
            .map(|n| n.node.as_str())
            .collect::<Vec<_>>(),
        ["control", "compute-1"]
    );
    assert_eq!(
        imported.archive.as_ref().and_then(|a| a.sha256.as_deref()),
        archive.sha256.as_deref()
    );

    // Restore the multi-node snapshot directly onto a second remote host ("edge-2") without
    // manual import: the coordinator copies the bundle there from the bucket, caches it, and
    // boots both VMs.
    let listener2 = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address2 = listener2.local_addr().unwrap();
    let server2 =
        tokio::spawn(axum::serve(listener2, api::router(manager.clone(), None)).into_future());
    let registered2 = manager
        .register_agent_host(RegisterAgentHost {
            id: "edge-2".into(),
        })
        .unwrap();
    let url2 = crate::remote::dial::connect_url(&format!("http://{address2}")).unwrap();
    let remote2 = Arc::new(Fake::default());
    let agent2 = Arc::new(crate::remote::agent::Agent::in_process(
        dir.path().join("edge2-agent"),
        remote2.clone(),
    ));
    let dialling2 =
        tokio::spawn(
            async move { crate::remote::dial::serve(&url2, &registered2.token, agent2).await },
        );
    tokio::time::timeout(Duration::from_secs(5), async {
        while !manager.host("edge-2").unwrap().connected {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    let check2 = manager
        .start_host_job("edge-2", crate::remote::HostJobKind::Check)
        .unwrap();
    assert_eq!(
        host_done(&manager, &check2.id).await.status,
        JobStatus::Succeeded
    );
    manager
        .update(|state| {
            let host = &mut state.hosts.get_mut("edge-2").unwrap().host;
            host.status = crate::remote::HostStatus::Ready;
            let inspection = host.inspection.as_mut().unwrap();
            inspection.memory_total_mib = 65536;
            inspection.os = "linux".into();
            Ok(())
        })
        .unwrap();

    let restored_job = manager
        .create(CreateRequest {
            host_id: Some("edge-2".into()),
            snapshot_id: Some("mn-remote".into()),
            ..request("mn-copied")
        })
        .unwrap();
    let restored = done(&manager, &restored_job).await;
    assert_eq!(
        restored.status,
        JobStatus::Succeeded,
        "{:?}",
        restored.error
    );
    let copied_farm = manager.get("mn-copied").unwrap();
    assert_eq!(copied_farm.host_id.as_deref(), Some("edge-2"));
    assert_eq!(copied_farm.nodes.len(), 2);
    let copies = manager.snapshot("mn-remote").unwrap().copies;
    assert_eq!(copies.len(), 1);
    assert_eq!(copies[0].host_id, "edge-2");
    assert_eq!(copies[0].via, CopySource::Bucket);
    assert_eq!(copies[0].nodes.len(), 2);

    // A second restore on the same host reuses the cached multi-node copy.
    let again_job = manager
        .create(CreateRequest {
            host_id: Some("edge-2".into()),
            snapshot_id: Some("mn-remote".into()),
            ..request("mn-copied-2")
        })
        .unwrap();
    let again = done(&manager, &again_job).await;
    assert_eq!(again.status, JobStatus::Succeeded, "{:?}", again.error);
    done(&manager, &manager.delete("mn-copied").unwrap()).await;
    done(&manager, &manager.delete("mn-copied-2").unwrap()).await;

    // Removing the copy removes both VMs' msb snapshots from the target host.
    manager
        .remove_snapshot_copy("mn-remote", "edge-2")
        .await
        .unwrap();
    assert!(manager.snapshot("mn-remote").unwrap().copies.is_empty());
    assert!(
        !remote2
            .snapshots
            .lock()
            .unwrap()
            .keys()
            .any(|k| k.contains("mn-remote"))
    );

    // Delete both existing snapshot records on the coordinator so only the S3 bucket holds it,
    // then start a farm directly from the bucket onto edge-2 without importing first.
    manager.forget_snapshot("mn-remote").await.unwrap();
    manager.forget_snapshot("mn-from-s3").await.unwrap();
    assert!(manager.snapshot("mn-remote").is_err());

    let direct_job = manager
        .create_from_bucket(CreateRequest {
            host_id: Some("edge-2".into()),
            bucket: Some("minio".into()),
            key: Some("snapshots/mn-remote.msb".into()),
            ..request("mn-direct")
        })
        .await
        .unwrap();
    let direct = done(&manager, &direct_job).await;
    assert_eq!(direct.status, JobStatus::Succeeded, "{:?}", direct.error);
    let direct_farm = manager.get("mn-direct").unwrap();
    assert_eq!(direct_farm.host_id.as_deref(), Some("edge-2"));
    assert_eq!(direct_farm.nodes.len(), 2);
    assert_eq!(direct_farm.status, FarmStatus::Ready);
    assert_eq!(
        direct_farm.plan.as_ref().unwrap().compute,
        original.plan.compute
    );
    assert_eq!(direct_farm.plan.as_ref().unwrap().gpu, original.plan.gpu);
    // The hydrated snapshot record stays registered with edge-2 as its origin.
    let hydrated = manager
        .snapshot(direct_farm.snapshot_id.as_deref().unwrap())
        .unwrap();
    assert_eq!(hydrated.host_id.as_deref(), Some("edge-2"));
    assert_eq!(hydrated.nodes.len(), 2);

    done(&manager, &manager.delete("mn-direct").unwrap()).await;
    dialling2.abort();
    server2.abort();
    drop(edge);
    settle(&manager).await;
}
