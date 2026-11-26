use crate::remote::{
    AgentHostToken, HostJob, HostJobKind, HostKey, HostKind, RegisterAgentHost, RegisterHost,
    RegisterLocalHost, RemoteHost, SshTarget,
};
use crate::{
    manager::{BadRequest, Conflict, NotFound, StackboxManager},
    model::*,
    s3::{Bucket, BucketCheck, BucketObject, RegisterBucket, catalog::BucketIndex},
};
use axum::{
    Json, Router,
    extract::{
        Path, Query, Request, State,
        rejection::JsonRejection,
        ws::{Message, WebSocket, WebSocketUpgrade},
    },
    http::{HeaderMap, StatusCode, header},
    middleware::{self, Next},
    response::{
        IntoResponse, Response, Sse,
        sse::{Event, KeepAlive},
    },
    routing::{get, post, put},
};
use serde::{Deserialize, Serialize};
use std::{convert::Infallible, io::Write, net::SocketAddr, sync::Arc, time::Duration};
use utoipa::{OpenApi, ToSchema};

#[derive(Clone)]
struct App {
    manager: StackboxManager,
    token: Option<Arc<str>>,
    ssh_discovery: Arc<tokio::sync::Semaphore>,
}
#[derive(Debug, Serialize, ToSchema)]
pub struct ErrorBody {
    pub error: String,
}
struct ApiError(StatusCode, String);
impl From<anyhow::Error> for ApiError {
    fn from(e: anyhow::Error) -> Self {
        let status = if e.is::<NotFound>() {
            StatusCode::NOT_FOUND
        } else if e.is::<BadRequest>() {
            StatusCode::BAD_REQUEST
        } else if e.is::<Conflict>() {
            StatusCode::CONFLICT
        } else {
            StatusCode::INTERNAL_SERVER_ERROR
        };
        if status == StatusCode::INTERNAL_SERVER_ERROR {
            eprintln!("API error: {e:#}");
        }
        Self(
            status,
            if status == StatusCode::INTERNAL_SERVER_ERROR {
                "internal server error; inspect daemon logs".into()
            } else {
                e.to_string()
            },
        )
    }
}
impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(ErrorBody { error: self.1 })).into_response()
    }
}
fn body<T>(value: Result<Json<T>, JsonRejection>) -> Result<T, ApiError> {
    value
        .map(|v| v.0)
        .map_err(|e| ApiError(e.status(), e.body_text()))
}
fn bad(e: anyhow::Error) -> ApiError {
    ApiError(StatusCode::BAD_REQUEST, e.to_string())
}

pub fn router(manager: StackboxManager, token: Option<String>) -> Router {
    let app = App {
        manager,
        token: token.map(Arc::from),
        ssh_discovery: Arc::new(tokio::sync::Semaphore::new(2)),
    };
    // The agent-only endpoint is kept for older callers; the router still mounts it.
    #[allow(deprecated)]
    Router::new()
        .route("/api/v1/host", get(host))
        .route("/api/v1/hosts", get(remote_hosts).post(register_host))
        .route("/api/v1/local-host", post(register_local_host))
        .route("/api/v1/agent-hosts", post(register_agent_host))
        .route(crate::remote::dial::CONNECT_PATH, get(agent_connect))
        .route("/api/v1/hosts/fingerprint", post(host_fingerprint))
        .route("/api/v1/hosts/test-connection", post(test_connection))
        .route(
            "/api/v1/hosts/{id}",
            get(remote_host)
                .delete(unregister_host)
                .put(update_host_credentials),
        )
        .route("/api/v1/hosts/{id}/check", post(check_host))
        .route("/api/v1/hosts/{id}/prepare", post(prepare_host))
        .route("/api/v1/hosts/{id}/agent", post(install_agent))
        .route("/api/v1/hosts/{id}/pci/bind", post(bind_host_pci))
        .route("/api/v1/hosts/{id}/pci/unbind", post(unbind_host_pci))
        .route(
            "/api/v1/hosts/{id}/pci/helper-install",
            get(host_pci_helper_install),
        )
        .route("/api/v1/host-jobs", get(host_jobs))
        .route("/api/v1/host-jobs/{id}", get(host_job))
        .route("/api/v1/catalog", get(catalog_handler))
        .route("/api/v1/plans", post(plan_handler))
        .route("/api/v1/stackboxes", get(list).post(create))
        .route("/api/v1/releases", get(releases))
        .route("/api/v1/releases/plan", post(release_plan))
        .route("/api/v1/releases/stackboxes", post(create_release))
        .route("/api/v1/stackboxes/{id}", get(detail).delete(delete))
        .route("/api/v1/stackboxes/{id}/deploy", post(deploy))
        .route("/api/v1/stackboxes/{id}/computes", post(add_compute))
        .route("/api/v1/stackboxes/{id}/instances", get(instances))
        .route(
            "/api/v1/stackboxes/{id}/instances/{server}/gpu-load",
            get(gpu_load).put(set_gpu_load).delete(clear_gpu_load),
        )
        .route(
            "/api/v1/stackboxes/{id}/gpu/guest-script",
            get(gpu_guest_script),
        )
        .route("/api/v1/stackboxes/{id}/gpu/user-data", get(gpu_user_data))
        .route("/api/v1/stackboxes/{id}/settle", post(settle))
        .route("/api/v1/stackboxes/{id}/endpoints", get(endpoints))
        .route(
            "/api/v1/stackboxes/{id}/relay-exposure",
            put(set_relay_exposure).delete(remove_relay_exposure),
        )
        .route(
            "/api/v1/stackboxes/{id}/nodes/{node}/faults",
            put(set_faults),
        )
        .route("/api/v1/stackboxes/{id}/logs", get(logs))
        .route("/api/v1/stackboxes/{id}/logs/stream", get(log_stream))
        .route("/api/v1/stackboxes/{id}/shell", get(shell))
        .route("/api/v1/stackboxes/{id}/snapshots", post(freeze))
        .route("/api/v1/snapshots", get(snapshots))
        .route("/api/v1/snapshots/import", post(import_snapshot))
        .route(
            "/api/v1/snapshots/{id}",
            get(snapshot).delete(delete_snapshot),
        )
        .route("/api/v1/snapshots/{id}/export", post(export_snapshot))
        .route(
            "/api/v1/snapshots/{id}/copies/{host_id}",
            axum::routing::delete(delete_snapshot_copy),
        )
        .route("/api/v1/buckets", get(buckets).post(register_bucket))
        .route("/api/v1/buckets/test-connection", post(test_bucket))
        .route(
            "/api/v1/buckets/{id}",
            get(bucket).put(update_bucket).delete(unregister_bucket),
        )
        .route("/api/v1/buckets/{id}/check", post(check_bucket))
        .route("/api/v1/buckets/{id}/objects", get(bucket_objects))
        .route("/api/v1/buckets/{id}/index", post(rebuild_bucket_index))
        .route(
            "/api/v1/relay",
            get(relay_status).put(set_relay).delete(remove_relay),
        )
        .route("/api/v1/relay/cloudflared/update", post(update_cloudflared))
        .route("/api/v1/jobs", get(jobs))
        .route("/api/v1/jobs/{id}", get(job))
        .route("/api/v1/openapi.json", get(openapi))
        .fallback(|| async { ApiError(StatusCode::NOT_FOUND, "route not found".into()) })
        .layer(middleware::from_fn_with_state(app.clone(), auth))
        .with_state(app)
}
async fn auth(State(app): State<App>, request: Request, next: Next) -> Response {
    // A dial-in agent authenticates with its own host token, never the API token.
    let agent = request.uri().path() == crate::remote::dial::CONNECT_PATH;
    if let Some(token) = app.token.filter(|_| !agent) {
        let expected = format!("Bearer {token}");
        let actual = request
            .headers()
            .get(header::AUTHORIZATION)
            .map(|v| v.as_bytes())
            .unwrap_or_default();
        // Avoid early byte-by-byte comparisons of secrets.
        use subtle::ConstantTimeEq;
        if !bool::from(expected.as_bytes().ct_eq(actual)) {
            return ApiError(StatusCode::UNAUTHORIZED, "bearer token required".into())
                .into_response();
        }
    }
    let mut response = next.run(request).await;
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
    response
}

#[derive(Serialize, ToSchema)]
pub struct HostInfo {
    pub agent_artifact: Option<crate::remote::AgentArtifactInfo>,
    #[serde(default)]
    pub agent_artifacts: Vec<crate::remote::AgentArtifactInfo>,
    pub os: String,
    pub arch: String,
    pub hypervisor: String,
    pub hypervisor_available: bool,
    pub msb_available: bool,
    /// msb snapshot new stackboxes start from; the first stackbox on the host builds it.
    pub golden_image: String,
    pub golden_image_available: bool,
    /// Revision this daemon was built from; a host agent must report the same one.
    pub build_id: String,
    pub memory_total_mib: u64,
    pub memory_available_mib: u64,
    pub memory_budget_mib: u64,
    pub memory_reserved_mib: u64,
    pub disk_available_mib: Option<u64>,
    pub state_dir: String,
    /// The relay stackboxes that span hosts join through (`STACKBOX_RELAY_URL`); none disables them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relay_url: Option<String>,
}
#[utoipa::path(get, path="/api/v1/host", responses((status=200, body=HostInfo)))]
async fn host(State(app): State<App>) -> Result<Json<HostInfo>, ApiError> {
    let dir = app.manager.state_dir().to_owned();
    let (total, available, disk) = tokio::task::spawn_blocking(move || {
        let sys = sysinfo::System::new_all();
        let disks = sysinfo::Disks::new_with_refreshed_list();
        let disk = disks
            .iter()
            .filter(|d| dir.starts_with(d.mount_point()))
            .max_by_key(|d| d.mount_point().as_os_str().len())
            .map(|d| d.available_space() / 1024 / 1024);
        (
            sys.total_memory() / 1024 / 1024,
            sys.available_memory() / 1024 / 1024,
            disk,
        )
    })
    .await
    .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let binary = std::env::var_os("MSB_PATH").unwrap_or_else(|| "msb".into());
    let mut command = tokio::process::Command::new(binary);
    command.arg("--version").kill_on_drop(true);
    let output = tokio::time::timeout(Duration::from_secs(5), command.output()).await;
    let msb = matches!(output, Ok(Ok(output)) if output.status.success());
    let golden_image = crate::stackbox::golden::current();
    let golden = msb
        && tokio::time::timeout(
            Duration::from_secs(5),
            crate::stackbox::golden::available(&golden_image),
        )
        .await
        .is_ok_and(|available| available.unwrap_or(false));
    let local = app
        .manager
        .hosts()?
        .into_iter()
        .find(|h| h.kind == HostKind::Local)
        .map(|h| h.id);
    let reserved = app
        .manager
        .list()?
        .iter()
        .filter(|f| local.is_some() && f.host_id == local)
        .map(StackboxRecord::memory_mib)
        .sum();
    Ok(Json(HostInfo {
        relay_url: app.manager.relay_url(),
        agent_artifact: app.manager.agent_artifact(),
        agent_artifacts: app.manager.agent_artifacts(),
        os: std::env::consts::OS.into(),
        arch: std::env::consts::ARCH.into(),
        hypervisor: crate::remote::host::hypervisor().into(),
        hypervisor_available: crate::remote::host::hypervisor_available(),
        msb_available: msb,
        golden_image,
        golden_image_available: golden,
        build_id: crate::BUILD_ID.into(),
        memory_total_mib: total,
        memory_available_mib: available,
        memory_budget_mib: app.manager.options().memory_budget_mib,
        memory_reserved_mib: reserved,
        disk_available_mib: disk,
        state_dir: app.manager.state_dir().display().to_string(),
    }))
}
#[utoipa::path(get, path="/api/v1/hosts", responses((status=200, body=Vec<RemoteHost>)))]
async fn remote_hosts(State(app): State<App>) -> Result<Json<Vec<RemoteHost>>, ApiError> {
    Ok(Json(app.manager.hosts()?))
}
#[utoipa::path(get, path="/api/v1/hosts/{id}", params(("id"=String, Path)), responses((status=200, body=RemoteHost), (status=404, body=ErrorBody)))]
async fn remote_host(
    State(app): State<App>,
    Path(id): Path<String>,
) -> Result<Json<RemoteHost>, ApiError> {
    Ok(Json(app.manager.host(&id)?))
}
#[utoipa::path(post, path="/api/v1/hosts/fingerprint", request_body=SshTarget, responses((status=200, body=HostKey), (status=400, body=ErrorBody), (status=429, body=ErrorBody)))]
async fn host_fingerprint(
    State(app): State<App>,
    value: Result<Json<SshTarget>, JsonRejection>,
) -> Result<Json<HostKey>, ApiError> {
    let target = body(value)?;
    target.validate().map_err(bad)?;
    let _permit = app.ssh_discovery.try_acquire().map_err(|_| {
        ApiError(
            StatusCode::TOO_MANY_REQUESTS,
            "too many SSH fingerprint checks; retry shortly".into(),
        )
    })?;
    Ok(Json(
        crate::remote::ssh::fingerprint(&target)
            .await
            .map_err(bad)?,
    ))
}
#[utoipa::path(post, path="/api/v1/hosts/test-connection", request_body=crate::remote::ConnectionTestRequest, responses((status=200, body=crate::remote::ConnectionTest), (status=400, body=ErrorBody), (status=429, body=ErrorBody)))]
async fn test_connection(
    State(app): State<App>,
    value: Result<Json<crate::remote::ConnectionTestRequest>, JsonRejection>,
) -> Result<Json<crate::remote::ConnectionTest>, ApiError> {
    let request = body(value)?;
    request.validate().map_err(bad)?;
    let request =
        tokio::task::spawn_blocking(move || request.credentials.validate().map(|()| request))
            .await
            .map_err(|_| {
                ApiError(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "credential validation failed".into(),
                )
            })?
            .map_err(bad)?;
    let _permit = app.ssh_discovery.try_acquire().map_err(|_| {
        ApiError(
            StatusCode::TOO_MANY_REQUESTS,
            "too many SSH connection tests; retry shortly".into(),
        )
    })?;
    let identity = crate::remote::ssh::Session::test_connection(&request)
        .await
        .map_err(bad)?;
    Ok(Json(crate::remote::ConnectionTest { identity }))
}
#[utoipa::path(post, path="/api/v1/hosts", request_body=RegisterHost, responses((status=201, description="The host; its active_job is the preparation job the registration started, unless another job already runs on it", body=RemoteHost), (status=400, body=ErrorBody), (status=409, body=ErrorBody)))]
async fn register_host(
    State(app): State<App>,
    value: Result<Json<RegisterHost>, JsonRejection>,
) -> Result<Response, ApiError> {
    let request = body(value)?;
    request.validate().map_err(bad)?;
    // bcrypt-encrypted private keys may take time to decode. Validate outside the async executor.
    let request =
        tokio::task::spawn_blocking(move || request.credentials.validate().map(|()| request))
            .await
            .map_err(|_| {
                ApiError(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "credential validation failed".into(),
                )
            })?
            .map_err(bad)?;
    let manager = app.manager;
    let host = tokio::task::spawn_blocking(move || manager.register_and_prepare_host(request))
        .await
        .map_err(|_| {
            ApiError(
                StatusCode::INTERNAL_SERVER_ERROR,
                "host registration failed".into(),
            )
        })??;
    Ok((
        StatusCode::CREATED,
        [(header::LOCATION, format!("/api/v1/hosts/{}", host.id))],
        Json(host),
    )
        .into_response())
}
#[utoipa::path(post, path="/api/v1/local-host", request_body=RegisterLocalHost, responses((status=201, description="The host; its active_job is the preparation job the registration started", body=RemoteHost), (status=400, body=ErrorBody), (status=409, body=ErrorBody)))]
async fn register_local_host(
    State(app): State<App>,
    value: Result<Json<RegisterLocalHost>, JsonRejection>,
) -> Result<Response, ApiError> {
    let host = app.manager.register_and_prepare_local_host(body(value)?)?;
    Ok((
        StatusCode::CREATED,
        [(header::LOCATION, format!("/api/v1/hosts/{}", host.id))],
        Json(host),
    )
        .into_response())
}
#[utoipa::path(post, path="/api/v1/agent-hosts", request_body=RegisterAgentHost, responses((status=201, description="The host and the token its agent connects with; shown once", body=AgentHostToken), (status=400, body=ErrorBody), (status=409, body=ErrorBody)))]
async fn register_agent_host(
    State(app): State<App>,
    value: Result<Json<RegisterAgentHost>, JsonRejection>,
) -> Result<Response, ApiError> {
    let registered = app.manager.register_agent_host(body(value)?)?;
    Ok((
        StatusCode::CREATED,
        [(
            header::LOCATION,
            format!("/api/v1/hosts/{}", registered.host.id),
        )],
        Json(registered),
    )
        .into_response())
}
/// A dial-in host's agent holds this socket open; the coordinator sends it requests.
async fn agent_connect(
    State(app): State<App>,
    headers: HeaderMap,
    upgrade: WebSocketUpgrade,
) -> Result<Response, ApiError> {
    same_origin(&headers)?;
    let unauthorized = || ApiError(StatusCode::UNAUTHORIZED, "agent token required".into());
    let token = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .ok_or_else(unauthorized)?;
    let host = app
        .manager
        .authenticate_agent(token)
        .map_err(|_| unauthorized())?;
    Ok(upgrade.on_upgrade(move |socket| relay_agent(app.manager, host, socket)))
}
async fn relay_agent(manager: StackboxManager, host: String, socket: WebSocket) {
    use futures::{SinkExt, StreamExt};
    let mut attached = manager.attach_agent(&host);
    let (mut sink, mut stream) = socket.split();
    // Proxies close idle sockets; pings keep a quiet agent connected.
    let mut ping = tokio::time::interval(Duration::from_secs(30));
    loop {
        tokio::select! {
            envelope = attached.outgoing.recv() => {
                let Some(envelope) = envelope else { break };
                let Ok(text) = serde_json::to_string(&envelope) else { break };
                if sink.send(Message::Text(text.into())).await.is_err() {
                    break;
                }
            }
            message = stream.next() => match message {
                Some(Ok(Message::Text(text))) => {
                    match serde_json::from_str(&text) {
                        Ok(crate::remote::dial::Envelope::Frame { session, frame }) => {
                            attached.deliver(session, frame)
                        }
                        _ => {
                            eprintln!("agent of host {host} sent an invalid message; disconnecting");
                            break;
                        }
                    }
                }
                Some(Ok(Message::Close(_)) | Err(_)) | None => break,
                Some(Ok(_)) => {}
            },
            _ = ping.tick() => {
                if sink.send(Message::Ping(Default::default())).await.is_err() {
                    break;
                }
            }
        }
    }
}
#[derive(Serialize, ToSchema)]
struct RemovedHost {
    id: String,
}
#[utoipa::path(put, path="/api/v1/hosts/{id}", params(("id"=String, Path)), request_body=RegisterHost, responses((status=200, body=RemoteHost), (status=400, body=ErrorBody), (status=409, body=ErrorBody)))]
async fn update_host_credentials(
    State(app): State<App>,
    Path(id): Path<String>,
    value: Result<Json<RegisterHost>, JsonRejection>,
) -> Result<Json<RemoteHost>, ApiError> {
    let request = body(value)?;
    request.validate().map_err(bad)?;
    if id != request.id {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            "host id cannot be changed".into(),
        ));
    }
    let request =
        tokio::task::spawn_blocking(move || request.credentials.validate().map(|()| request))
            .await
            .map_err(|_| {
                ApiError(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "credential validation failed".into(),
                )
            })?
            .map_err(bad)?;
    let manager = app.manager;
    Ok(Json(
        tokio::task::spawn_blocking(move || manager.update_host_credentials_and_prepare(request))
            .await
            .map_err(|_| {
                ApiError(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "credential update failed".into(),
                )
            })??,
    ))
}
#[derive(Deserialize, utoipa::IntoParams)]
struct UnregisterQuery {
    /// Also delete this coordinator's stackboxes and leftover VMs on the host, then unregister it.
    #[serde(default)]
    purge: bool,
    /// With purge, also delete the snapshots frozen on the host, whose originals it keeps,
    /// with their copies on other hosts. The local host also keeps imported snapshots.
    #[serde(default)]
    snapshots: bool,
}
#[utoipa::path(delete, path="/api/v1/hosts/{id}", params(("id"=String, Path), UnregisterQuery), responses((status=200, description="Unregistered; records and credentials only", body=RemovedHost), (status=202, description="purge=true: host job that deletes the stackboxes, their VMs, with snapshots=true the snapshots, and then the host", body=HostJob), (status=400, body=ErrorBody), (status=409, body=ErrorBody)))]
async fn unregister_host(
    State(app): State<App>,
    Path(id): Path<String>,
    Query(query): Query<UnregisterQuery>,
) -> Result<Response, ApiError> {
    if query.purge {
        return Ok(accepted_host(
            app.manager.start_purge(&id, query.snapshots)?,
        ));
    }
    if query.snapshots {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            "snapshots=true needs purge=true".into(),
        ));
    }
    app.manager.unregister_host(&id)?;
    Ok(Json(RemovedHost { id }).into_response())
}
fn accepted_host(job: HostJob) -> Response {
    (
        StatusCode::ACCEPTED,
        [(header::LOCATION, format!("/api/v1/host-jobs/{}", job.id))],
        Json(job),
    )
        .into_response()
}
#[utoipa::path(post, path="/api/v1/hosts/{id}/check", params(("id"=String, Path)), responses((status=202, body=HostJob), (status=409, body=ErrorBody)))]
async fn check_host(State(app): State<App>, Path(id): Path<String>) -> Result<Response, ApiError> {
    Ok(accepted_host(
        app.manager.start_host_job(&id, HostJobKind::Check)?,
    ))
}
/// Binds a PCI device of the host to `vfio-pci` through its stackbox-vfio-bind helper, so a
/// stackbox can pass it through. The job's inspection replaces the host's.
#[utoipa::path(post, path="/api/v1/hosts/{id}/pci/bind", params(("id"=String, Path)), request_body=crate::remote::pci::PciBindRequest, responses((status=202, body=HostJob), (status=400, body=ErrorBody), (status=404, body=ErrorBody), (status=409, body=ErrorBody)))]
async fn bind_host_pci(
    State(app): State<App>,
    Path(id): Path<String>,
    Json(request): Json<crate::remote::pci::PciBindRequest>,
) -> Result<Response, ApiError> {
    Ok(accepted_host(app.manager.bind_host_pci(&id, &request.bdf)?))
}
/// Gives a PCI device of the host back to its host driver through the helper.
#[utoipa::path(post, path="/api/v1/hosts/{id}/pci/unbind", params(("id"=String, Path)), request_body=crate::remote::pci::PciBindRequest, responses((status=202, body=HostJob), (status=400, body=ErrorBody), (status=404, body=ErrorBody), (status=409, body=ErrorBody)))]
async fn unbind_host_pci(
    State(app): State<App>,
    Path(id): Path<String>,
    Json(request): Json<crate::remote::pci::PciBindRequest>,
) -> Result<Response, ApiError> {
    Ok(accepted_host(
        app.manager.unbind_host_pci(&id, &request.bdf)?,
    ))
}
#[derive(Deserialize)]
struct HelperInstallQuery {
    /// The account the host's stackbox-agent runs `sudo -n` as. An SSH host defaults to its login
    /// user; any other host must name it.
    #[serde(default)]
    user: Option<String>,
}
/// The self-contained script that installs the stackbox-vfio-bind helper and its sudoers rule on
/// the host, for the portal to copy and the operator to run there.
#[utoipa::path(get, path="/api/v1/hosts/{id}/pci/helper-install", params(("id"=String, Path)), responses((status=200, body=crate::remote::pci::PciHelperInstall), (status=400, body=ErrorBody), (status=404, body=ErrorBody)))]
async fn host_pci_helper_install(
    State(app): State<App>,
    Path(id): Path<String>,
    Query(query): Query<HelperInstallQuery>,
) -> Result<Json<crate::remote::pci::PciHelperInstall>, ApiError> {
    Ok(Json(
        app.manager.pci_helper_install(&id, query.user.as_deref())?,
    ))
}
#[utoipa::path(post, path="/api/v1/hosts/{id}/prepare", params(("id"=String, Path)), responses((status=202, body=HostJob), (status=409, body=ErrorBody)))]
async fn prepare_host(
    State(app): State<App>,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    Ok(accepted_host(
        app.manager.start_host_job(&id, HostJobKind::Prepare)?,
    ))
}
/// Install only the agent, without msb, the stackbox kernel or a verification boot.
///
/// Deprecated: `POST /hosts/{id}/prepare` installs the agent too, after the kernel and the
/// verification VM, so the portal offers one button. This endpoint stays for older callers.
#[utoipa::path(post, path="/api/v1/hosts/{id}/agent", params(("id"=String, Path)), responses((status=202, body=HostJob), (status=409, body=ErrorBody)))]
#[deprecated(note = "prepare installs the agent too; use POST /api/v1/hosts/{id}/prepare")]
async fn install_agent(
    State(app): State<App>,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    Ok(accepted_host(
        app.manager.start_host_job(&id, HostJobKind::InstallAgent)?,
    ))
}
#[utoipa::path(get, path="/api/v1/host-jobs", responses((status=200, body=Vec<HostJob>)))]
async fn host_jobs(State(app): State<App>) -> Result<Json<Vec<HostJob>>, ApiError> {
    Ok(Json(app.manager.host_jobs()?))
}
#[utoipa::path(get, path="/api/v1/host-jobs/{id}", params(("id"=String, Path)), responses((status=200, body=HostJob), (status=404, body=ErrorBody)))]
async fn host_job(
    State(app): State<App>,
    Path(id): Path<String>,
) -> Result<Json<HostJob>, ApiError> {
    Ok(Json(app.manager.host_job(&id)?))
}
#[derive(Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
struct CatalogQuery {
    /// OpenStack release to check each service against, such as 2025.2; omitted lists every
    /// service with its static availability.
    release: Option<String>,
}
#[utoipa::path(get, path="/api/v1/catalog", params(CatalogQuery), responses((status=200, body=Catalog)))]
async fn catalog_handler(Query(query): Query<CatalogQuery>) -> Json<Catalog> {
    Json(catalog(query.release.as_deref()))
}
#[utoipa::path(post, path="/api/v1/plans", request_body=PlanRequest, responses((status=200, body=Plan), (status=400, body=ErrorBody)))]
async fn plan_handler(
    value: Result<Json<PlanRequest>, JsonRejection>,
) -> Result<Json<Plan>, ApiError> {
    let request = body(value)?;
    require_multi_node(request.computes, request.layout).map_err(bad)?;
    request.check_release().map_err(bad)?;
    Ok(Json(plan(&request).map_err(bad)?))
}
#[utoipa::path(get, path="/api/v1/stackboxes", responses((status=200, body=Vec<StackboxView>)))]
async fn list(State(app): State<App>) -> Result<Json<Vec<StackboxView>>, ApiError> {
    Ok(Json(
        app.manager
            .list()?
            .into_iter()
            .map(StackboxView::of)
            .collect(),
    ))
}
#[utoipa::path(get, path="/api/v1/stackboxes/{id}", params(("id"=String, Path)), responses((status=200, body=StackboxView), (status=404, body=ErrorBody)))]
async fn detail(
    State(app): State<App>,
    Path(id): Path<String>,
) -> Result<Json<StackboxView>, ApiError> {
    Ok(Json(StackboxView::of(app.manager.get(&id)?)))
}
#[utoipa::path(post, path="/api/v1/stackboxes", request_body=CreateRequest, responses((status=202, body=Job), (status=400, body=ErrorBody), (status=409, body=ErrorBody)))]
async fn create(
    State(app): State<App>,
    value: Result<Json<CreateRequest>, JsonRejection>,
) -> Result<Response, ApiError> {
    let request = body(value)?;
    // A snapshot or bucket restore keeps the layout it was frozen with, including legacy ones.
    if request.snapshot_id.is_none() && request.bucket.is_none() {
        require_multi_node(request.computes, request.layout).map_err(bad)?;
    }
    request.validate().map_err(bad)?;
    let job = if request.bucket.is_some() {
        app.manager.create_from_bucket(request).await?
    } else {
        app.manager.create(request)?
    };
    Ok(accepted(job))
}
#[derive(Deserialize, utoipa::IntoParams)]
struct ReleaseQuery {
    /// CPU architecture; defaults to this coordinator's.
    arch: Option<String>,
    /// Registered bucket to count templates in; omitted counts every bucket.
    bucket: Option<String>,
}
#[utoipa::path(get, path="/api/v1/releases", params(ReleaseQuery), responses((status=200, description="OpenStack releases with the control and compute templates kept here and in buckets, newest release first. The release this coordinator installs clean is always listed", body=ReleaseCatalog), (status=404, body=ErrorBody)))]
async fn releases(
    State(app): State<App>,
    Query(query): Query<ReleaseQuery>,
) -> Result<Json<ReleaseCatalog>, ApiError> {
    Ok(Json(
        app.manager
            .release_catalog(query.arch, query.bucket)
            .await?,
    ))
}
#[utoipa::path(post, path="/api/v1/releases/plan", request_body=ReleaseCreateRequest, responses((status=200, description="Dry run: which template or clean install would make each VM, and why. Nothing is created", body=ReleasePlan), (status=400, body=ErrorBody), (status=404, body=ErrorBody), (status=409, description="No control template of the release, and this coordinator installs another release clean", body=ErrorBody)))]
async fn release_plan(
    State(app): State<App>,
    value: Result<Json<ReleaseCreateRequest>, JsonRejection>,
) -> Result<Json<ReleasePlan>, ApiError> {
    let request = body(value)?;
    require_multi_node(request.computes, request.layout).map_err(bad)?;
    Ok(Json(app.manager.preview_release(&request).await?))
}
#[utoipa::path(post, path="/api/v1/releases/stackboxes", request_body=ReleaseCreateRequest, responses((status=202, description="Assemble job: the plan's imports, the control VM, then each compute, each step a job of its own listed in `assembly`", body=Job), (status=400, body=ErrorBody), (status=404, body=ErrorBody), (status=409, body=ErrorBody)))]
async fn create_release(
    State(app): State<App>,
    value: Result<Json<ReleaseCreateRequest>, JsonRejection>,
) -> Result<Response, ApiError> {
    let request = body(value)?;
    require_multi_node(request.computes, request.layout).map_err(bad)?;
    Ok(accepted(app.manager.create_release(request).await?))
}
fn accepted(job: Job) -> Response {
    (
        StatusCode::ACCEPTED,
        [(header::LOCATION, format!("/api/v1/jobs/{}", job.id))],
        Json(job),
    )
        .into_response()
}
#[utoipa::path(post, path="/api/v1/stackboxes/{id}/deploy", params(("id"=String, Path)), request_body=DeployRequest, responses((status=202, body=Job), (status=409, body=ErrorBody)))]
async fn deploy(
    State(app): State<App>,
    Path(id): Path<String>,
    value: Result<Json<DeployRequest>, JsonRejection>,
) -> Result<Response, ApiError> {
    Ok(accepted(app.manager.deploy(&id, body(value)?)?))
}
#[utoipa::path(post, path="/api/v1/stackboxes/{id}/computes", params(("id"=String, Path)), request_body=AddComputeRequest, responses((status=202, description="Add the next compute VM to a deployed multi-node stackbox, on the given host or the stackbox's primary host", body=Job), (status=400, body=ErrorBody), (status=404, body=ErrorBody), (status=409, body=ErrorBody)))]
async fn add_compute(
    State(app): State<App>,
    Path(id): Path<String>,
    value: Result<Json<AddComputeRequest>, JsonRejection>,
) -> Result<Response, ApiError> {
    Ok(accepted(app.manager.add_compute(&id, body(value)?)?))
}
#[utoipa::path(get, path="/api/v1/stackboxes/{id}/instances", params(("id"=String, Path)), responses((status=200, description="Nova servers in a GPU stackbox, with each flavor's GPU resource classes and the load stackbox applied", body=Vec<StackboxInstance>), (status=409, description="Not a deployed GPU stackbox, or Nova is not part of it", body=ErrorBody)))]
async fn instances(
    State(app): State<App>,
    Path(id): Path<String>,
) -> Result<Json<Vec<StackboxInstance>>, ApiError> {
    Ok(Json(app.manager.instances(&id).await?))
}

#[utoipa::path(get, path="/api/v1/stackboxes/{id}/instances/{server}/gpu-load", params(("id"=String, Path), ("server"=String, Path, description="Nova server UUID, or a unique instance name")), responses((status=200, description="The load for the instance; a server without one shows the default with applied=false", body=GpuLoadView), (status=404, body=ErrorBody), (status=409, body=ErrorBody)))]
async fn gpu_load(
    State(app): State<App>,
    Path((id, server)): Path<(String, String)>,
) -> Result<Json<GpuLoadView>, ApiError> {
    Ok(Json(app.manager.gpu_load(&id, &server).await?))
}

#[utoipa::path(put, path="/api/v1/stackboxes/{id}/instances/{server}/gpu-load", params(("id"=String, Path), ("server"=String, Path, description="Nova server UUID, or a unique instance name")), request_body=GpuLoad, responses((status=200, description="The load was written to the instance's metadata; the guest sees it within about 15 seconds", body=GpuLoadView), (status=400, body=ErrorBody), (status=404, body=ErrorBody), (status=409, body=ErrorBody)))]
async fn set_gpu_load(
    State(app): State<App>,
    Path((id, server)): Path<(String, String)>,
    value: Result<Json<GpuLoad>, JsonRejection>,
) -> Result<Json<GpuLoadView>, ApiError> {
    Ok(Json(
        app.manager.set_gpu_load(&id, &server, body(value)?).await?,
    ))
}

#[utoipa::path(delete, path="/api/v1/stackboxes/{id}/instances/{server}/gpu-load", params(("id"=String, Path), ("server"=String, Path, description="Nova server UUID, or a unique instance name")), responses((status=200, description="The metadata is removed; the guest returns to its default at the next metadata refresh", body=GpuLoadView), (status=404, body=ErrorBody), (status=409, body=ErrorBody)))]
async fn clear_gpu_load(
    State(app): State<App>,
    Path((id, server)): Path<(String, String)>,
) -> Result<Json<GpuLoadView>, ApiError> {
    Ok(Json(app.manager.clear_gpu_load(&id, &server).await?))
}

#[utoipa::path(get, path="/api/v1/stackboxes/{id}/gpu/guest-script", params(("id"=String, Path)), responses((status=200, description="The fake nvidia-smi script, ready to install as /usr/bin/nvidia-smi", content_type="text/x-shellscript", body=String), (status=409, body=ErrorBody)))]
async fn gpu_guest_script(
    State(app): State<App>,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    let script = app.manager.gpu_guest_script(&id)?;
    Ok((
        [
            (header::CONTENT_TYPE, "text/x-shellscript; charset=utf-8"),
            (
                header::CONTENT_DISPOSITION,
                "attachment; filename=\"nvidia-smi.sh\"",
            ),
        ],
        script,
    )
        .into_response())
}

#[utoipa::path(get, path="/api/v1/stackboxes/{id}/gpu/user-data", params(("id"=String, Path)), responses((status=200, description="A user-data installer for images that do not apply Nova vendor data (CirrOS): it writes the fake nvidia-smi and prints its XML to the console", content_type="text/x-shellscript", body=String), (status=409, body=ErrorBody)))]
async fn gpu_user_data(
    State(app): State<App>,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    let script = app.manager.gpu_user_data(&id)?;
    Ok((
        [
            (header::CONTENT_TYPE, "text/x-shellscript; charset=utf-8"),
            (
                header::CONTENT_DISPOSITION,
                "attachment; filename=\"stackbox-gpu-user-data.sh\"",
            ),
        ],
        script,
    )
        .into_response())
}

#[utoipa::path(post, path="/api/v1/stackboxes/{id}/settle", params(("id"=String, Path)), responses((status=202, description="Wait for the stackbox's services to answer again after its VM started from disk; no deployment runs", body=Job), (status=409, body=ErrorBody)))]
async fn settle(State(app): State<App>, Path(id): Path<String>) -> Result<Response, ApiError> {
    Ok(accepted(app.manager.settle(&id)?))
}
#[derive(Deserialize, utoipa::IntoParams)]
struct ForgetQuery {
    /// Remove only the coordinator's record, without asking the host. Whatever the host
    /// still keeps stays there; use it when the host is gone or unreachable.
    #[serde(default)]
    forget: bool,
}
#[utoipa::path(delete, path="/api/v1/stackboxes/{id}", params(("id"=String, Path), ForgetQuery), responses((status=202, body=Job), (status=409, body=ErrorBody)))]
async fn delete(
    State(app): State<App>,
    Path(id): Path<String>,
    Query(query): Query<ForgetQuery>,
) -> Result<Response, ApiError> {
    Ok(accepted(if query.forget {
        app.manager.forget(&id)?
    } else {
        app.manager.delete(&id)?
    }))
}
#[utoipa::path(get, path="/api/v1/relay", responses((status=200, description="Where stackboxes that span hosts join", body=crate::manager::RelayStatus)))]
async fn relay_status(
    State(app): State<App>,
) -> Result<Json<crate::manager::RelayStatus>, ApiError> {
    Ok(Json(app.manager.relay_status().await?))
}
#[utoipa::path(put, path="/api/v1/relay", request_body=crate::link::relay::RelaySettings, responses((status=200, description="Saved; the relay listens anew at once", body=crate::manager::RelayStatus), (status=400, body=ErrorBody), (status=409, description="Stackboxes that span hosts hold the current URL and CA", body=ErrorBody)))]
async fn set_relay(
    State(app): State<App>,
    value: Result<Json<crate::link::relay::RelaySettings>, JsonRejection>,
) -> Result<Json<crate::manager::RelayStatus>, ApiError> {
    Ok(Json(app.manager.set_relay(Some(body(value)?)).await?))
}
#[utoipa::path(post, path="/api/v1/relay/cloudflared/update", responses((status=200, description="Checked for a new cloudflared; the tunnel restarts on one", body=crate::manager::RelayStatus), (status=409, body=ErrorBody)))]
async fn update_cloudflared(
    State(app): State<App>,
) -> Result<Json<crate::manager::RelayStatus>, ApiError> {
    Ok(Json(app.manager.update_cloudflared().await?))
}
#[utoipa::path(delete, path="/api/v1/relay", responses((status=200, description="The relay is off; stackboxes cannot span hosts", body=crate::manager::RelayStatus), (status=409, body=ErrorBody)))]
async fn remove_relay(
    State(app): State<App>,
) -> Result<Json<crate::manager::RelayStatus>, ApiError> {
    Ok(Json(app.manager.set_relay(None).await?))
}
#[utoipa::path(get, path="/api/v1/jobs", responses((status=200, body=Vec<Job>)))]
async fn jobs(State(app): State<App>) -> Result<Json<Vec<Job>>, ApiError> {
    Ok(Json(app.manager.jobs()?))
}
#[utoipa::path(get, path="/api/v1/jobs/{id}", params(("id"=String, Path)), responses((status=200, body=Job), (status=404, body=ErrorBody)))]
async fn job(State(app): State<App>, Path(id): Path<String>) -> Result<Json<Job>, ApiError> {
    Ok(Json(app.manager.job(&id)?))
}
#[utoipa::path(get, path="/api/v1/stackboxes/{id}/endpoints", params(("id"=String, Path)), responses((status=200, body=Endpoints), (status=409, body=ErrorBody)))]
async fn endpoints(
    State(app): State<App>,
    Path(id): Path<String>,
) -> Result<Json<Endpoints>, ApiError> {
    Ok(Json(app.manager.endpoints(&id).await?))
}
#[utoipa::path(put, path="/api/v1/stackboxes/{id}/relay-exposure", params(("id"=String, Path)), request_body=crate::model::RelayExposureRequest, responses((status=200, description="The stackbox's services are reachable through its own Cloudflare quick tunnel; `state` is `starting` until the tunnel has its address, then `ready`. `rotate` makes a new token without restarting the tunnel", body=crate::model::RelayExposureStatus), (status=400, body=ErrorBody), (status=404, body=ErrorBody), (status=409, description="Not deployed or a job is running, or a dial-in agent host", body=ErrorBody)))]
async fn set_relay_exposure(
    State(app): State<App>,
    Path(id): Path<String>,
    value: Result<Json<crate::model::RelayExposureRequest>, JsonRejection>,
) -> Result<Json<crate::model::RelayExposureStatus>, ApiError> {
    Ok(Json(
        app.manager.set_relay_exposure(&id, body(value)?).await?,
    ))
}
#[utoipa::path(delete, path="/api/v1/stackboxes/{id}/relay-exposure", params(("id"=String, Path)), responses((status=200, description="The stackbox's tunnel is stopped; old URLs answer nothing", body=crate::model::RelayExposureStatus), (status=404, body=ErrorBody)))]
async fn remove_relay_exposure(
    State(app): State<App>,
    Path(id): Path<String>,
) -> Result<Json<crate::model::RelayExposureStatus>, ApiError> {
    Ok(Json(app.manager.remove_relay_exposure(&id)?))
}
#[utoipa::path(put, path="/api/v1/stackboxes/{id}/nodes/{node}/faults", params(("id"=String, Path), ("node"=String, Path, description="VM of a multi-node stackbox: control, compute-1, ...")), request_body=crate::link::Impair, responses((status=200, description="The stackbox with the VM's faults applied; no delay and no loss heals it", body=StackboxView), (status=400, body=ErrorBody), (status=404, body=ErrorBody), (status=409, body=ErrorBody)))]
async fn set_faults(
    State(app): State<App>,
    Path((id, node)): Path<(String, String)>,
    value: Result<Json<crate::link::Impair>, JsonRejection>,
) -> Result<Json<StackboxView>, ApiError> {
    Ok(Json(StackboxView::of(
        app.manager.impair(&id, &node, body(value)?).await?,
    )))
}
#[utoipa::path(post, path="/api/v1/stackboxes/{id}/snapshots", params(("id"=String, Path)), request_body=SnapshotRequest, responses((status=202, description="Snapshot job; the stackbox's VM stops for the capture and starts again. With `node: control` every VM stops, computes first, but only the control VM is captured, as a control template", body=Job), (status=400, body=ErrorBody), (status=409, body=ErrorBody)))]
async fn freeze(
    State(app): State<App>,
    Path(id): Path<String>,
    value: Result<Json<SnapshotRequest>, JsonRejection>,
) -> Result<Response, ApiError> {
    Ok(accepted(app.manager.freeze(&id, body(value)?)?))
}
#[utoipa::path(get, path="/api/v1/snapshots", responses((status=200, body=Vec<SnapshotRecord>)))]
async fn snapshots(State(app): State<App>) -> Result<Json<Vec<SnapshotRecord>>, ApiError> {
    Ok(Json(app.manager.snapshots()?))
}
#[utoipa::path(get, path="/api/v1/snapshots/{id}", params(("id"=String, Path)), responses((status=200, body=SnapshotRecord), (status=404, body=ErrorBody)))]
async fn snapshot(
    State(app): State<App>,
    Path(id): Path<String>,
) -> Result<Json<SnapshotRecord>, ApiError> {
    Ok(Json(app.manager.snapshot(&id)?))
}
#[derive(Serialize, ToSchema)]
struct RemovedSnapshot {
    id: String,
    /// Copies on other hosts that could not be removed, as `<host>: <error>`; they stay
    /// on those hosts until removed there.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    copies_left: Vec<String>,
}
#[utoipa::path(delete, path="/api/v1/snapshots/{id}", params(("id"=String, Path), ForgetQuery), responses((status=200, body=RemovedSnapshot), (status=404, body=ErrorBody), (status=409, body=ErrorBody)))]
async fn delete_snapshot(
    State(app): State<App>,
    Path(id): Path<String>,
    Query(query): Query<ForgetQuery>,
) -> Result<Json<RemovedSnapshot>, ApiError> {
    let copies_left = if query.forget {
        app.manager.forget_snapshot(&id).await?;
        Vec::new()
    } else {
        app.manager.delete_snapshot(&id).await?
    };
    Ok(Json(RemovedSnapshot { id, copies_left }))
}
#[utoipa::path(delete, path="/api/v1/snapshots/{id}/copies/{host_id}", params(("id"=String, Path), ("host_id"=String, Path)), responses((status=200, description="The copy on that host was removed; the snapshot itself stays", body=SnapshotRecord), (status=404, body=ErrorBody), (status=409, body=ErrorBody)))]
async fn delete_snapshot_copy(
    State(app): State<App>,
    Path((id, host_id)): Path<(String, String)>,
) -> Result<Json<SnapshotRecord>, ApiError> {
    Ok(Json(app.manager.remove_snapshot_copy(&id, &host_id).await?))
}
fn accepted_snapshot(snapshot: SnapshotRecord) -> Response {
    (
        StatusCode::ACCEPTED,
        [(
            header::LOCATION,
            format!("/api/v1/snapshots/{}", snapshot.id),
        )],
        Json(snapshot),
    )
        .into_response()
}
#[utoipa::path(post, path="/api/v1/snapshots/{id}/export", params(("id"=String, Path)), request_body=ExportRequest, responses((status=202, description="Export started; poll the snapshot until it leaves `exporting`", body=SnapshotRecord), (status=400, body=ErrorBody), (status=409, body=ErrorBody)))]
async fn export_snapshot(
    State(app): State<App>,
    Path(id): Path<String>,
    value: Result<Json<ExportRequest>, JsonRejection>,
) -> Result<Response, ApiError> {
    Ok(accepted_snapshot(
        app.manager.export_snapshot(&id, body(value)?)?,
    ))
}
#[utoipa::path(post, path="/api/v1/snapshots/import", request_body=ImportRequest, responses((status=202, description="Import started; poll the snapshot until it leaves `importing`", body=SnapshotRecord), (status=400, body=ErrorBody), (status=409, body=ErrorBody)))]
async fn import_snapshot(
    State(app): State<App>,
    value: Result<Json<ImportRequest>, JsonRejection>,
) -> Result<Response, ApiError> {
    Ok(accepted_snapshot(
        app.manager.import_snapshot(body(value)?)?,
    ))
}
#[utoipa::path(get, path="/api/v1/buckets", responses((status=200, body=Vec<Bucket>)))]
async fn buckets(State(app): State<App>) -> Result<Json<Vec<Bucket>>, ApiError> {
    Ok(Json(app.manager.buckets()?))
}
#[utoipa::path(get, path="/api/v1/buckets/{id}", params(("id"=String, Path)), responses((status=200, body=Bucket), (status=404, body=ErrorBody)))]
async fn bucket(State(app): State<App>, Path(id): Path<String>) -> Result<Json<Bucket>, ApiError> {
    Ok(Json(app.manager.bucket(&id)?))
}
#[utoipa::path(post, path="/api/v1/buckets", request_body=RegisterBucket, responses((status=201, description="Registered; the secret key is stored encrypted and not returned", body=Bucket), (status=400, body=ErrorBody), (status=409, body=ErrorBody)))]
async fn register_bucket(
    State(app): State<App>,
    value: Result<Json<RegisterBucket>, JsonRejection>,
) -> Result<Response, ApiError> {
    let bucket = app.manager.register_bucket(body(value)?)?;
    Ok((
        StatusCode::CREATED,
        [(header::LOCATION, format!("/api/v1/buckets/{}", bucket.id))],
        Json(bucket),
    )
        .into_response())
}
#[utoipa::path(put, path="/api/v1/buckets/{id}", params(("id"=String, Path)), request_body=RegisterBucket, responses((status=200, body=Bucket), (status=400, body=ErrorBody), (status=404, body=ErrorBody), (status=409, body=ErrorBody)))]
async fn update_bucket(
    State(app): State<App>,
    Path(id): Path<String>,
    value: Result<Json<RegisterBucket>, JsonRejection>,
) -> Result<Json<Bucket>, ApiError> {
    let request = body(value)?;
    if id != request.id {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            "bucket id cannot be changed".into(),
        ));
    }
    Ok(Json(app.manager.update_bucket(request)?))
}
#[derive(Serialize, ToSchema)]
struct RemovedBucket {
    id: String,
}
#[utoipa::path(delete, path="/api/v1/buckets/{id}", params(("id"=String, Path)), responses((status=200, description="Unregistered; objects in the bucket are kept", body=RemovedBucket), (status=404, body=ErrorBody), (status=409, body=ErrorBody)))]
async fn unregister_bucket(
    State(app): State<App>,
    Path(id): Path<String>,
) -> Result<Json<RemovedBucket>, ApiError> {
    app.manager.unregister_bucket(&id)?;
    Ok(Json(RemovedBucket { id }))
}
#[utoipa::path(post, path="/api/v1/buckets/test-connection", request_body=RegisterBucket, responses((status=200, description="Lists, writes and deletes a test object under the prefix; nothing is saved. Without a secret key, the registered bucket's is used", body=BucketCheck), (status=400, body=ErrorBody)))]
async fn test_bucket(
    State(app): State<App>,
    value: Result<Json<RegisterBucket>, JsonRejection>,
) -> Result<Json<BucketCheck>, ApiError> {
    let message = app.manager.test_bucket(body(value)?).await?;
    Ok(Json(BucketCheck { message }))
}
#[utoipa::path(post, path="/api/v1/buckets/{id}/check", params(("id"=String, Path)), responses((status=200, description="Lists, writes and deletes a test object under the prefix", body=BucketCheck), (status=400, body=ErrorBody), (status=404, body=ErrorBody)))]
async fn check_bucket(
    State(app): State<App>,
    Path(id): Path<String>,
) -> Result<Json<BucketCheck>, ApiError> {
    let message = app.manager.check_bucket(&id).await?;
    Ok(Json(BucketCheck { message }))
}
#[utoipa::path(get, path="/api/v1/buckets/{id}/objects", params(("id"=String, Path)), responses((status=200, description="`.msb` objects under the bucket's prefix, newest first, with the metadata their exports wrote next to them. A missing or outdated `index.json` is written again", body=Vec<BucketObject>), (status=400, body=ErrorBody), (status=404, body=ErrorBody)))]
async fn bucket_objects(
    State(app): State<App>,
    Path(id): Path<String>,
) -> Result<Json<Vec<BucketObject>>, ApiError> {
    Ok(Json(app.manager.bucket_objects(&id).await?))
}
#[utoipa::path(post, path="/api/v1/buckets/{id}/index", params(("id"=String, Path)), responses((status=200, description="`index.json` under the bucket's prefix, written again from the archives' metadata files", body=BucketIndex), (status=400, body=ErrorBody), (status=404, body=ErrorBody)))]
async fn rebuild_bucket_index(
    State(app): State<App>,
    Path(id): Path<String>,
) -> Result<Json<BucketIndex>, ApiError> {
    Ok(Json(app.manager.rebuild_bucket_index(&id).await?))
}

#[derive(Deserialize, utoipa::IntoParams)]
struct LogQuery {
    #[serde(default)]
    after: u64,
    #[serde(default = "default_limit")]
    limit: usize,
}
fn default_limit() -> usize {
    1000
}
#[utoipa::path(get, path="/api/v1/stackboxes/{id}/logs", params(("id"=String, Path), LogQuery), responses((status=200, body=Vec<LogEntry>)))]
async fn logs(
    State(app): State<App>,
    Path(id): Path<String>,
    Query(query): Query<LogQuery>,
) -> Result<Json<Vec<LogEntry>>, ApiError> {
    Ok(Json(app.manager.logs(&id, query.after, query.limit)?))
}
#[utoipa::path(get, path="/api/v1/stackboxes/{id}/logs/stream", params(("id"=String, Path), LogQuery), responses((status=200, description="SSE: log events with JSON LogEntry data, gap events on expired cursors, error events on read failures", content_type="text/event-stream", body=String)))]
async fn log_stream(
    State(app): State<App>,
    Path(id): Path<String>,
    Query(query): Query<LogQuery>,
    headers: HeaderMap,
) -> Result<Sse<impl futures::Stream<Item = Result<Event, Infallible>>>, ApiError> {
    app.manager.get(&id)?;
    let mut after = headers
        .get("last-event-id")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse().ok())
        .unwrap_or(query.after);
    let stream = async_stream::stream! {
        let mut interval = tokio::time::interval(Duration::from_millis(500));
        loop {
            interval.tick().await;
            if !app.manager.is_accepting() { break; }
            match app.manager.logs(&id, after, 1000) {
                Ok(entries) => {
                    if let Some(first) = entries.first() && after > 0 && first.id > after + 1 { yield Ok(Event::default().event("gap").data("requested cursor expired; replaying retained tail")); }
                    for entry in entries { after = entry.id; yield Ok(Event::default().event("log").id(entry.id.to_string()).json_data(entry).unwrap()); }
                }
                Err(_) => { yield Ok(Event::default().event("error").data("cannot read stackbox log")); break; }
            }
        }
    };
    Ok(Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(15))))
}

#[derive(Deserialize)]
struct ShellQuery {
    rows: u16,
    cols: u16,
    /// A multi-node VM's suffix (`control`, `compute-1`); omitted for the default VM.
    #[serde(default)]
    node: Option<String>,
}
/// Control messages share the socket with terminal bytes, which travel as binary frames.
#[derive(Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum ShellControl {
    Resize { rows: u16, cols: u16 },
    Exit,
    Error { message: String },
}
/// A browser always names the page that opened a WebSocket, and CORS does not apply to it.
/// Only the portal's same-origin proxy (which sends no Origin) or a same-host page may attach.
fn same_origin(headers: &HeaderMap) -> Result<(), ApiError> {
    let Some(origin) = headers.get(header::ORIGIN) else {
        return Ok(());
    };
    let host = headers.get(header::HOST).and_then(|v| v.to_str().ok());
    let origin_host = origin
        .to_str()
        .ok()
        .and_then(|o| o.split_once("://"))
        .map(|(_, rest)| rest);
    if host.is_some() && origin_host == host {
        Ok(())
    } else {
        Err(ApiError(
            StatusCode::FORBIDDEN,
            "cross-origin shell sessions are not allowed".into(),
        ))
    }
}
/// Interactive login shell in the stackbox's VM over a WebSocket: binary frames carry terminal
/// bytes both ways; text frames carry `ShellControl` messages.
async fn shell(
    State(app): State<App>,
    Path(id): Path<String>,
    Query(query): Query<ShellQuery>,
    headers: HeaderMap,
    upgrade: WebSocketUpgrade,
) -> Result<Response, ApiError> {
    same_origin(&headers)?;
    let size = crate::stackbox::TerminalSize {
        rows: query.rows,
        cols: query.cols,
    }
    .validate()
    .map_err(bad)?;
    app.manager.check_terminal(&id, query.node.as_deref())?;
    let node = query.node;
    Ok(upgrade.on_upgrade(move |socket| relay_shell(app.manager, id, node, size, socket)))
}
async fn relay_shell(
    manager: StackboxManager,
    id: String,
    node: Option<String>,
    size: crate::stackbox::TerminalSize,
    socket: WebSocket,
) {
    use crate::stackbox::{TerminalInput, TerminalIo, TerminalSize};
    use futures::{SinkExt, StreamExt};
    let (mut sink, mut stream) = socket.split();
    let (input_tx, input_rx) = tokio::sync::mpsc::channel(64);
    let (output_tx, output_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
    let result = {
        let session = manager.terminal(
            &id,
            node.as_deref(),
            size,
            TerminalIo {
                input: input_rx,
                output: output_tx,
            },
        );
        // Owns the receiver, so a dead socket unblocks the shell's output instead of stalling it.
        let send = async {
            let mut output_rx = output_rx;
            while let Some(data) = output_rx.recv().await {
                if sink.send(Message::Binary(data.into())).await.is_err() {
                    break;
                }
            }
        };
        let pump = async { tokio::join!(session, send).0 };
        // Ends when the browser leaves, which drops `input_tx` and hangs the shell up.
        let receive = async move {
            while let Some(Ok(message)) = stream.next().await {
                let input = match message {
                    Message::Binary(data) => TerminalInput::Data(data.to_vec()),
                    Message::Text(text) => match serde_json::from_str(&text) {
                        Ok(ShellControl::Resize { rows, cols }) => {
                            match (TerminalSize { rows, cols }).validate() {
                                Ok(size) => TerminalInput::Resize(size),
                                Err(_) => continue,
                            }
                        }
                        _ => continue,
                    },
                    Message::Close(_) => break,
                    _ => continue,
                };
                if input_tx.send(input).await.is_err() {
                    break;
                }
            }
        };
        tokio::pin!(pump, receive);
        tokio::select! {
            result = &mut pump => result,
            _ = &mut receive => pump.await,
        }
    };
    let notice = match result {
        Ok(()) => ShellControl::Exit,
        Err(error) => {
            eprintln!("shell for stackbox {id} ended: {error:#}");
            ShellControl::Error {
                message: format!("{error:#}"),
            }
        }
    };
    if let Ok(text) = serde_json::to_string(&notice) {
        let _ = sink.send(Message::Text(text.into())).await;
    }
    let _ = sink.close().await;
}

#[derive(OpenApi)]
#[openapi(paths(host, remote_hosts, remote_host, host_fingerprint, test_connection, register_host, register_local_host, register_agent_host, update_host_credentials, unregister_host, check_host, prepare_host, install_agent, bind_host_pci, unbind_host_pci, host_pci_helper_install, host_jobs, host_job, catalog_handler, plan_handler, list, detail, create, releases, release_plan, create_release, deploy, add_compute, instances, gpu_load, set_gpu_load, clear_gpu_load, gpu_guest_script, gpu_user_data, settle, delete, endpoints, set_relay_exposure, remove_relay_exposure, set_faults, logs, log_stream, freeze, snapshots, snapshot, delete_snapshot, delete_snapshot_copy, export_snapshot, import_snapshot, buckets, bucket, register_bucket, update_bucket, unregister_bucket, test_bucket, check_bucket, bucket_objects, rebuild_bucket_index, relay_status, set_relay, remove_relay, update_cloudflared, jobs, job), components(schemas(ErrorBody)), modifiers(&Security))]
struct ApiDoc;
struct Security;
impl utoipa::Modify for Security {
    fn modify(&self, doc: &mut utoipa::openapi::OpenApi) {
        use utoipa::openapi::security::{
            Http, HttpAuthScheme, SecurityRequirement, SecurityScheme,
        };
        doc.components.as_mut().unwrap().add_security_scheme(
            "bearer",
            SecurityScheme::Http(Http::new(HttpAuthScheme::Bearer)),
        );
        doc.security = Some(vec![
            SecurityRequirement::new("bearer", Vec::<String>::new()),
            SecurityRequirement::default(),
        ]);
    }
}
pub fn openapi_document() -> utoipa::openapi::OpenApi {
    ApiDoc::openapi()
}
async fn openapi() -> Json<utoipa::openapi::OpenApi> {
    Json(openapi_document())
}

pub async fn serve(
    manager: StackboxManager,
    bind: SocketAddr,
    token: Option<String>,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        bind.ip().is_loopback() || token.is_some(),
        "non-loopback API binding requires STACKBOX_API_TOKEN"
    );
    anyhow::ensure!(
        token.as_ref().is_none_or(|t| !t.trim().is_empty()),
        "STACKBOX_API_TOKEN cannot be empty"
    );
    let listener = tokio::net::TcpListener::bind(bind).await?;
    println!(
        "stackboxd listening on http://{} (state: {})",
        listener.local_addr()?,
        manager.state_dir().display()
    );
    let reconcile_manager = manager.clone();
    let reconciler = tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(30));
        interval.tick().await;
        loop {
            interval.tick().await;
            if let Err(e) = reconcile_manager.reconcile().await {
                eprintln!("reconcile failed: {e:#}");
            }
            // A host that missed a quick tunnel's new address gets it again.
            reconcile_manager.relink_missing().await;
        }
    });
    manager.start_relay().await;
    let shutdown_manager = manager.clone();
    let result = axum::serve(listener, router(manager.clone(), token)).with_graceful_shutdown(async move {
        shutdown_signal().await;
        // A closed terminal sends SIGHUP and then fails writes; that must not abort the drain.
        let _ = writeln!(
            std::io::stdout(),
            "stackboxd draining active jobs (running deploys continue on their hosts); a second signal terminates immediately"
        );
        let graceful = async { let result = shutdown_manager.shutdown().await; if let Err(e) = result { eprintln!("shutdown failed: {e:#}"); } };
        tokio::select! { _ = graceful => {}, _ = shutdown_signal() => { std::process::exit(130); } }
    }).await;
    reconciler.abort();
    result?;
    Ok(())
}
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut term = signal(SignalKind::terminate()).expect("install SIGTERM handler");
        // The terminal the daemon runs in was closed.
        let mut hangup = signal(SignalKind::hangup()).expect("install SIGHUP handler");
        tokio::select! { _ = tokio::signal::ctrl_c() => {}, _ = term.recv() => {}, _ = hangup.recv() => {} }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_sockets_accept_the_proxy_or_the_same_host_only() {
        let headers = |pairs: &[(header::HeaderName, &str)]| {
            let mut map = HeaderMap::new();
            for (name, value) in pairs {
                map.insert(name.clone(), value.parse().unwrap());
            }
            map
        };
        assert!(same_origin(&headers(&[(header::HOST, "127.0.0.1:8080")])).is_ok());
        assert!(
            same_origin(&headers(&[
                (header::HOST, "127.0.0.1:8080"),
                (header::ORIGIN, "http://127.0.0.1:8080"),
            ]))
            .is_ok()
        );
        for origin in ["https://evil.example", "null", "http://127.0.0.1:8081"] {
            assert!(
                same_origin(&headers(&[
                    (header::HOST, "127.0.0.1:8080"),
                    (header::ORIGIN, origin),
                ]))
                .is_err()
            );
        }
    }
}
