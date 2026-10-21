use crate::remote::{HostJob, HostJobKind, HostKey, RegisterHost, RemoteHost, SshTarget};
use crate::{
    manager::{Conflict, FarmManager, NotFound},
    model::*,
};
use axum::{
    Json, Router,
    extract::{Path, Query, Request, State, rejection::JsonRejection},
    http::{HeaderMap, StatusCode, header},
    middleware::{self, Next},
    response::{
        IntoResponse, Response, Sse,
        sse::{Event, KeepAlive},
    },
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use std::{convert::Infallible, net::SocketAddr, sync::Arc, time::Duration};
use utoipa::{OpenApi, ToSchema};

#[derive(Clone)]
struct App {
    manager: FarmManager,
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

pub fn router(manager: FarmManager, token: Option<String>) -> Router {
    let app = App {
        manager,
        token: token.map(Arc::from),
        ssh_discovery: Arc::new(tokio::sync::Semaphore::new(2)),
    };
    Router::new()
        .route("/api/v1/host", get(host))
        .route("/api/v1/hosts", get(remote_hosts).post(register_host))
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
        .route("/api/v1/host-jobs", get(host_jobs))
        .route("/api/v1/host-jobs/{id}", get(host_job))
        .route("/api/v1/catalog", get(catalog_handler))
        .route("/api/v1/plans", post(plan_handler))
        .route("/api/v1/farms", get(list).post(create))
        .route("/api/v1/farms/{id}", get(detail).delete(delete))
        .route("/api/v1/farms/{id}/deploy", post(deploy))
        .route("/api/v1/farms/{id}/endpoints", get(endpoints))
        .route("/api/v1/farms/{id}/logs", get(logs))
        .route("/api/v1/farms/{id}/logs/stream", get(log_stream))
        .route("/api/v1/jobs", get(jobs))
        .route("/api/v1/jobs/{id}", get(job))
        .route("/api/v1/openapi.json", get(openapi))
        .fallback(|| async { ApiError(StatusCode::NOT_FOUND, "route not found".into()) })
        .layer(middleware::from_fn_with_state(app.clone(), auth))
        .with_state(app)
}
async fn auth(State(app): State<App>, request: Request, next: Next) -> Response {
    if let Some(token) = app.token {
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
    pub golden_image: String,
    pub golden_image_available: bool,
    pub memory_total_mib: u64,
    pub memory_available_mib: u64,
    pub memory_budget_mib: u64,
    pub memory_reserved_mib: u64,
    pub disk_available_mib: Option<u64>,
    pub state_dir: String,
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
    command
        .args(["image", "list", "--quiet"])
        .kill_on_drop(true);
    let output = tokio::time::timeout(Duration::from_secs(5), command.output()).await;
    let (msb, golden) = match output {
        Ok(Ok(output)) if output.status.success() => (
            true,
            String::from_utf8_lossy(&output.stdout)
                .lines()
                .any(|l| l.trim() == crate::farm::BASE_IMAGE),
        ),
        _ => (false, false),
    };
    let reserved = app
        .manager
        .list()?
        .iter()
        .filter(|f| f.host_id.is_none())
        .map(FarmRecord::memory_mib)
        .sum();
    Ok(Json(HostInfo {
        agent_artifact: app.manager.agent_artifact(),
        agent_artifacts: app.manager.agent_artifacts(),
        os: std::env::consts::OS.into(),
        arch: std::env::consts::ARCH.into(),
        hypervisor: if cfg!(target_os = "macos") {
            "hvf"
        } else {
            "kvm"
        }
        .into(),
        hypervisor_available: cfg!(target_os = "macos")
            || std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open("/dev/kvm")
                .is_ok(),
        msb_available: msb,
        golden_image: crate::farm::BASE_IMAGE.into(),
        golden_image_available: golden,
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
#[utoipa::path(post, path="/api/v1/hosts", request_body=RegisterHost, responses((status=201, body=RemoteHost), (status=400, body=ErrorBody), (status=409, body=ErrorBody)))]
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
    let host = tokio::task::spawn_blocking(move || manager.register_host(request))
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
        tokio::task::spawn_blocking(move || manager.update_host_credentials(request))
            .await
            .map_err(|_| {
                ApiError(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "credential update failed".into(),
                )
            })??,
    ))
}
#[utoipa::path(delete, path="/api/v1/hosts/{id}", params(("id"=String, Path)), responses((status=200, body=RemovedHost), (status=409, body=ErrorBody)))]
async fn unregister_host(
    State(app): State<App>,
    Path(id): Path<String>,
) -> Result<Json<RemovedHost>, ApiError> {
    app.manager.unregister_host(&id)?;
    Ok(Json(RemovedHost { id }))
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
#[utoipa::path(post, path="/api/v1/hosts/{id}/prepare", params(("id"=String, Path)), responses((status=202, body=HostJob), (status=409, body=ErrorBody)))]
async fn prepare_host(
    State(app): State<App>,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    Ok(accepted_host(
        app.manager.start_host_job(&id, HostJobKind::Prepare)?,
    ))
}
#[utoipa::path(post, path="/api/v1/hosts/{id}/agent", params(("id"=String, Path)), responses((status=202, body=HostJob), (status=409, body=ErrorBody)))]
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
#[utoipa::path(get, path="/api/v1/catalog", responses((status=200, body=Catalog)))]
async fn catalog_handler() -> Json<Catalog> {
    Json(catalog())
}
#[utoipa::path(post, path="/api/v1/plans", request_body=PlanRequest, responses((status=200, body=Plan), (status=400, body=ErrorBody)))]
async fn plan_handler(
    value: Result<Json<PlanRequest>, JsonRejection>,
) -> Result<Json<Plan>, ApiError> {
    Ok(Json(plan(&body(value)?).map_err(bad)?))
}
#[utoipa::path(get, path="/api/v1/farms", responses((status=200, body=Vec<FarmRecord>)))]
async fn list(State(app): State<App>) -> Result<Json<Vec<FarmRecord>>, ApiError> {
    Ok(Json(app.manager.list()?))
}
#[utoipa::path(get, path="/api/v1/farms/{id}", params(("id"=String, Path)), responses((status=200, body=FarmRecord), (status=404, body=ErrorBody)))]
async fn detail(
    State(app): State<App>,
    Path(id): Path<String>,
) -> Result<Json<FarmRecord>, ApiError> {
    Ok(Json(app.manager.get(&id)?))
}
#[utoipa::path(post, path="/api/v1/farms", request_body=CreateRequest, responses((status=202, body=Job), (status=400, body=ErrorBody), (status=409, body=ErrorBody)))]
async fn create(
    State(app): State<App>,
    value: Result<Json<CreateRequest>, JsonRejection>,
) -> Result<Response, ApiError> {
    let request = body(value)?;
    request.validate().map_err(bad)?;
    Ok(accepted(app.manager.create(request)?))
}
fn accepted(job: Job) -> Response {
    (
        StatusCode::ACCEPTED,
        [(header::LOCATION, format!("/api/v1/jobs/{}", job.id))],
        Json(job),
    )
        .into_response()
}
#[utoipa::path(post, path="/api/v1/farms/{id}/deploy", params(("id"=String, Path)), request_body=DeployRequest, responses((status=202, body=Job), (status=409, body=ErrorBody)))]
async fn deploy(
    State(app): State<App>,
    Path(id): Path<String>,
    value: Result<Json<DeployRequest>, JsonRejection>,
) -> Result<Response, ApiError> {
    Ok(accepted(app.manager.deploy(&id, body(value)?)?))
}
#[utoipa::path(delete, path="/api/v1/farms/{id}", params(("id"=String, Path)), responses((status=202, body=Job), (status=409, body=ErrorBody)))]
async fn delete(State(app): State<App>, Path(id): Path<String>) -> Result<Response, ApiError> {
    Ok(accepted(app.manager.delete(&id)?))
}
#[utoipa::path(get, path="/api/v1/jobs", responses((status=200, body=Vec<Job>)))]
async fn jobs(State(app): State<App>) -> Result<Json<Vec<Job>>, ApiError> {
    Ok(Json(app.manager.jobs()?))
}
#[utoipa::path(get, path="/api/v1/jobs/{id}", params(("id"=String, Path)), responses((status=200, body=Job), (status=404, body=ErrorBody)))]
async fn job(State(app): State<App>, Path(id): Path<String>) -> Result<Json<Job>, ApiError> {
    Ok(Json(app.manager.job(&id)?))
}
#[utoipa::path(get, path="/api/v1/farms/{id}/endpoints", params(("id"=String, Path)), responses((status=200, body=Endpoints), (status=409, body=ErrorBody)))]
async fn endpoints(
    State(app): State<App>,
    Path(id): Path<String>,
) -> Result<Json<Endpoints>, ApiError> {
    Ok(Json(app.manager.endpoints(&id).await?))
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
#[utoipa::path(get, path="/api/v1/farms/{id}/logs", params(("id"=String, Path), LogQuery), responses((status=200, body=Vec<LogEntry>)))]
async fn logs(
    State(app): State<App>,
    Path(id): Path<String>,
    Query(query): Query<LogQuery>,
) -> Result<Json<Vec<LogEntry>>, ApiError> {
    Ok(Json(app.manager.logs(&id, query.after, query.limit)?))
}
#[utoipa::path(get, path="/api/v1/farms/{id}/logs/stream", params(("id"=String, Path), LogQuery), responses((status=200, description="SSE: log events with JSON LogEntry data, gap events on expired cursors, error events on read failures", content_type="text/event-stream", body=String)))]
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
                Err(_) => { yield Ok(Event::default().event("error").data("cannot read farm log")); break; }
            }
        }
    };
    Ok(Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(15))))
}

#[derive(OpenApi)]
#[openapi(paths(host, remote_hosts, remote_host, host_fingerprint, test_connection, register_host, update_host_credentials, unregister_host, check_host, prepare_host, install_agent, host_jobs, host_job, catalog_handler, plan_handler, list, detail, create, deploy, delete, endpoints, logs, log_stream, jobs, job), components(schemas(ErrorBody)), modifiers(&Security))]
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
    manager: FarmManager,
    bind: SocketAddr,
    token: Option<String>,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        bind.ip().is_loopback() || token.is_some(),
        "non-loopback API binding requires FARM_API_TOKEN"
    );
    anyhow::ensure!(
        token.as_ref().is_none_or(|t| !t.trim().is_empty()),
        "FARM_API_TOKEN cannot be empty"
    );
    let listener = tokio::net::TcpListener::bind(bind).await?;
    println!(
        "farmd listening on http://{} (state: {})",
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
        }
    });
    let shutdown_manager = manager.clone();
    let result = axum::serve(listener, router(manager.clone(), token)).with_graceful_shutdown(async move {
        shutdown_signal().await;
        println!("farmd draining active jobs; a second signal terminates immediately");
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
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("install SIGTERM handler");
        tokio::select! { _ = tokio::signal::ctrl_c() => {}, _ = term.recv() => {} }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}
