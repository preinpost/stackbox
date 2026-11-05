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
        crate::remote::protocol::Reply::Found { reference: Some(ref r) } if *r == snapshot.reference
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
        crate::remote::protocol::Reply::Found { reference: None }
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
