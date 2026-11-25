//! The relay's reverse proxy to each stackbox's OpenStack service endpoints. A stackbox exposed
//! with `PUT /api/v1/stackboxes/{id}/relay-exposure` answers at
//! `<relay>/x/<stackbox>/<token>/<service><path>`; the dashboards (`horizon`, `skyline-console`)
//! are entered once there, which sets a cookie that carries their stackbox, token and service for
//! the rest of their pages. Only the public service ports of the stackbox's enabled components
//! are reachable. The infrastructure ports never are as plain HTTP: the RabbitMQ management UI is
//! entered as a dashboard, and MariaDB and RabbitMQ AMQP are reached only by WebSocket, through
//! [`EndpointProxy::resolve_tcp`] and [`super::relay`].
//!
//! The relay's other routes are untouched; [`super::relay::router`] sends everything that is not
//! the WebSocket here.

use std::{collections::BTreeMap, collections::HashMap, sync::Arc, sync::RwLock};

use axum::{
    Router,
    body::Body,
    extract::{
        Path, Request, State,
        ws::{WebSocket, WebSocketUpgrade},
    },
    http::{HeaderMap, HeaderName, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
    routing::get,
};
use bytes::Bytes;
use futures::{StreamExt, stream};
use subtle::ConstantTimeEq;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::model::COMPONENTS;

/// The cookie a dashboard's pages carry: `<stackbox>:<token>:<service>`.
pub const COOKIE: &str = "__stackbox_ui";
/// Web dashboards: entered once, then their own pages use the cookie.
pub const DASHBOARDS: &[&str] = &["horizon", "skyline-console", "rabbitmq-management"];
/// The data stores a WebSocket tunnel reaches (`/x/<stackbox>/<token>/tcp/<service>`).
const TCP_SERVICES: &[&str] = &["mariadb", "rabbitmq"];
/// How many WebSocket tunnels may be open at once across all stackboxes; a request past it is
/// refused with `503 Service Unavailable` until one closes.
pub const MAX_TCP_TUNNELS: usize = 64;
/// JSON bodies up to this size are read and have their URLs rewritten; larger or other bodies
/// stream through untouched.
const REWRITE_LIMIT: u64 = 4 * 1024 * 1024;
/// The `client.mjs` a developer pipes into `node --input-type=module`; `__MYSQL_URL__` and
/// `__AMQP_URL__` are replaced with the stackbox's two tunnel URLs, JSON-encoded.
const TUNNEL_SCRIPT: &str = include_str!("endpoint_proxy/tunnel.mjs");

/// A public service of an exposed stackbox, at `port_base + offset` on its host.
#[derive(Clone, Debug, PartialEq)]
pub struct Service {
    pub id: &'static str,
    pub offset: u16,
    pub path: &'static str,
}

/// One exposed stackbox: its token, its host and port block, and the services it publishes.
#[derive(Clone, Debug)]
pub struct Exposed {
    pub token: String,
    pub host: String,
    pub port_base: u16,
    pub services: Vec<Service>,
}

#[derive(Default)]
struct Table {
    /// The relay's base URL as other hosts and browsers reach it; `None` when the relay is off.
    relay_base: Option<String>,
    stackboxes: HashMap<String, Exposed>,
}

/// The live table of exposed stackboxes, replaced whenever the coordinator's state or the relay's
/// address changes.
pub struct EndpointProxy {
    table: RwLock<Table>,
    client: reqwest::Client,
    /// One permit per open WebSocket tunnel; see [`MAX_TCP_TUNNELS`].
    tcp_slots: Arc<Semaphore>,
}

impl Default for EndpointProxy {
    fn default() -> Self {
        // Redirects pass through to the browser, which must see the relay's own URLs.
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("the endpoint proxy's HTTP client");
        Self {
            table: RwLock::default(),
            client,
            tcp_slots: Arc::new(Semaphore::new(MAX_TCP_TUNNELS)),
        }
    }
}

impl EndpointProxy {
    /// A proxy that allows `limit` WebSocket tunnels at once, for tests of the refusal.
    #[cfg(test)]
    pub fn with_tcp_limit(limit: usize) -> Self {
        Self {
            tcp_slots: Arc::new(Semaphore::new(limit)),
            ..Self::default()
        }
    }

    /// A permit for one more WebSocket tunnel, or `None` when [`MAX_TCP_TUNNELS`] are open. The
    /// permit returns to the pool when it is dropped.
    pub fn try_acquire_tcp(&self) -> Option<OwnedSemaphorePermit> {
        self.tcp_slots.clone().try_acquire_owned().ok()
    }
}

/// The proxy's own routes, for a proxy that serves one stackbox or the relay's table: the
/// `client.mjs` and the data store tunnels are matched first, and everything else goes to
/// [`EndpointProxy::handle`].
pub fn router(proxy: Arc<EndpointProxy>) -> Router {
    Router::new()
        .route(
            "/x/{stackbox}/{token}/tcp/client.mjs",
            get(tcp_client_script),
        )
        .route("/x/{stackbox}/{token}/tcp/{service}", get(tcp_upgrade))
        .fallback(endpoints)
        .with_state(proxy)
}

async fn endpoints(State(proxy): State<Arc<EndpointProxy>>, request: Request) -> Response {
    proxy.handle(request).await
}

/// The `node --input-type=module` script that opens a stackbox's MariaDB and AMQP ports locally.
/// A wrong token, or a relay that is off, is a plain `404`.
async fn tcp_client_script(
    State(proxy): State<Arc<EndpointProxy>>,
    Path((stackbox, token)): Path<(String, String)>,
) -> Response {
    match proxy.client_script(&stackbox, &token) {
        Some(script) => (
            [
                (header::CONTENT_TYPE, "text/javascript; charset=utf-8"),
                (header::CACHE_CONTROL, "no-store"),
            ],
            script,
        )
            .into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

/// A stackbox's MariaDB or RabbitMQ AMQP port, as a WebSocket of binary frames. The token and the
/// service are checked before the upgrade, so a wrong one is a plain `404`.
async fn tcp_upgrade(
    State(proxy): State<Arc<EndpointProxy>>,
    Path((stackbox, token, service)): Path<(String, String, String)>,
    ws: WebSocketUpgrade,
) -> Response {
    let Some((host, port)) = proxy.resolve_tcp(&stackbox, &token, &service) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    // Checked before the upgrade, so a refused client gets a plain answer it can retry.
    let Some(slot) = proxy.try_acquire_tcp() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "too many active TCP tunnels\n",
        )
            .into_response();
    };
    let limit = crate::link::MAX_MESSAGE + 256;
    ws.max_message_size(limit)
        .max_frame_size(limit)
        .on_upgrade(move |socket: WebSocket| crate::link::relay::bridge(socket, host, port, slot))
}

/// The public services of the enabled components, in catalog order. Infrastructure ports are not
/// in the catalog, so they are never among them.
pub fn public_services(components: &[String]) -> Vec<Service> {
    COMPONENTS
        .iter()
        .filter(|c| components.iter().any(|id| id == c.id))
        .flat_map(|c| c.public.iter())
        .map(|port| Service {
            id: port.id,
            offset: port.offset,
            path: port.path,
        })
        .collect()
}

/// The URL a service has on its host's own address, which the proxy rewrites.
pub fn base_url(host: &str, port: u16) -> String {
    format!("http://{host}:{port}")
}

/// The two URLs a service on `host:port` may name itself by, `http://` and `https://`, each mapped
/// to `relay`. An upstream behind a TLS-terminating proxy answers with the `https://` one.
fn address_pairs(host: &str, port: u16, relay: &str) -> Vec<(String, String)> {
    ["http", "https"]
        .into_iter()
        .map(|scheme| (format!("{scheme}://{host}:{port}"), relay.to_owned()))
        .collect()
}

/// Each public service's URL through the relay, by service ID. A dashboard's is its entry point,
/// which sets its cookie; an API's keeps its path (`/v3`, `/v2.1`, ...).
pub fn exposure_urls(
    relay_base: &str,
    stackbox: &str,
    token: &str,
    components: &[String],
) -> BTreeMap<String, String> {
    let base = relay_base.trim_end_matches('/');
    let mut urls: BTreeMap<String, String> = public_services(components)
        .into_iter()
        .map(|service| {
            let entry = format!("{base}/x/{stackbox}/{token}/{}", service.id);
            let url = if DASHBOARDS.contains(&service.id) {
                entry
            } else {
                format!("{entry}{}", service.path)
            };
            (service.id.to_owned(), url)
        })
        .collect();
    let entry = format!("{base}/x/{stackbox}/{token}");
    urls.insert(
        "rabbitmq-management".to_owned(),
        format!("{entry}/rabbitmq-management"),
    );
    for id in TCP_SERVICES {
        urls.insert((*id).to_owned(), format!("{entry}/tcp/{id}"));
    }
    urls.insert(
        "tunnel-script".to_owned(),
        format!("{entry}/tcp/client.mjs"),
    );
    urls
}

/// The RabbitMQ management UI: an infrastructure port, reachable only as a dashboard.
fn management(service: &str) -> Option<Service> {
    let port = crate::model::INFRA_PORTS
        .iter()
        .find(|p| p.id == "rabbitmq-management")?;
    (service == port.id).then_some(Service {
        id: port.id,
        offset: port.offset,
        path: "",
    })
}

/// Where a request goes and how its answer is rewritten.
struct Plan {
    url: String,
    /// The `Host` header the upstream sees.
    host: String,
    /// `Origin` and `Referer` rewritten from the first to the second, for the dashboards' CSRF checks.
    origin: Option<(String, String)>,
    /// Upstream base URL to relay base URL, for `Location` and JSON bodies.
    pairs: Vec<(String, String)>,
    /// Whether JSON bodies up to [`REWRITE_LIMIT`] are rewritten.
    json: bool,
    /// Whether `Domain=` is dropped from the upstream's cookies, so they stay on the relay.
    strip_domain: bool,
}

impl EndpointProxy {
    /// Replace the live table: the relay's base URL and each exposed stackbox by ID.
    pub fn replace(&self, relay_base: Option<String>, stackboxes: HashMap<String, Exposed>) {
        *self.table.write().unwrap_or_else(|p| p.into_inner()) = Table {
            relay_base,
            stackboxes,
        };
    }

    /// Whether `stackbox` is exposed under `token` in the live table right now.
    pub fn is_exposed(&self, stackbox: &str, token: &str) -> bool {
        let table = self.table.read().unwrap_or_else(|p| p.into_inner());
        table
            .stackboxes
            .get(stackbox)
            .is_some_and(|e| bool::from(e.token.as_bytes().ct_eq(token.as_bytes())))
    }

    /// The host and port of a data store a WebSocket tunnel may reach: MariaDB or RabbitMQ AMQP of
    /// an exposed stackbox, under its current token. `None` for anything else, including the
    /// management UI and the plain HTTP paths.
    pub fn resolve_tcp(&self, id: &str, token: &str, service: &str) -> Option<(String, u16)> {
        let table = self.table.read().unwrap_or_else(|p| p.into_inner());
        table.relay_base.as_ref()?;
        let exposed = table.stackboxes.get(id)?;
        if !bool::from(exposed.token.as_bytes().ct_eq(token.as_bytes())) {
            return None;
        }
        if !TCP_SERVICES.contains(&service) {
            return None;
        }
        let port = crate::model::INFRA_PORTS
            .iter()
            .find(|p| p.id == service)?
            .offset;
        let port = exposed.port_base.checked_add(port)?;
        Some((exposed.host.clone(), port))
    }

    /// The `client.mjs` that opens an exposed stackbox's MariaDB and RabbitMQ AMQP ports locally.
    /// It names only the two relay URLs, never a password. `None` unless the relay is on and
    /// `token` is the stackbox's current one (compared in constant time).
    pub fn client_script(&self, id: &str, token: &str) -> Option<String> {
        let table = self.table.read().unwrap_or_else(|p| p.into_inner());
        let relay_base = table.relay_base.as_ref()?;
        let exposed = table.stackboxes.get(id)?;
        if !bool::from(exposed.token.as_bytes().ct_eq(token.as_bytes())) {
            return None;
        }
        let ws = crate::link::tcp_tunnel::to_ws_url(relay_base.trim_end_matches('/')).ok()?;
        let entry = format!("{ws}/x/{id}/{token}/tcp");
        let mysql = serde_json::to_string(&format!("{entry}/mariadb")).ok()?;
        let amqp = serde_json::to_string(&format!("{entry}/rabbitmq")).ok()?;
        Some(
            TUNNEL_SCRIPT
                .replace("__MYSQL_URL__", &mysql)
                .replace("__AMQP_URL__", &amqp),
        )
    }

    /// Serve one request that the relay's WebSocket route does not take.
    pub async fn handle(&self, request: Request) -> Response {
        let rest = request.uri().path().strip_prefix("/x/").map(str::to_owned);
        let Some(rest) = rest else {
            return self.dashboard(request).await;
        };
        let mut parts = rest.splitn(4, '/');
        let (Some(id), Some(token), Some(service)) = (parts.next(), parts.next(), parts.next())
        else {
            return not_found();
        };
        let tail = parts.next().unwrap_or("").to_owned();
        let (id, token, service) = (id.to_owned(), token.to_owned(), service.to_owned());
        self.service(request, &id, &token, &service, &tail).await
    }

    /// The exposed service a request names, or `None` when the relay is off, the stackbox or the
    /// token is unknown (compared in constant time), or the service is not one it publishes.
    fn resolve(&self, id: &str, token: &str, service: &str) -> Option<Target> {
        let table = self.table.read().unwrap_or_else(|p| p.into_inner());
        let relay_base = table.relay_base.clone()?;
        let exposed = table.stackboxes.get(id)?;
        if !bool::from(exposed.token.as_bytes().ct_eq(token.as_bytes())) {
            return None;
        }
        let service = exposed
            .services
            .iter()
            .find(|s| s.id == service)
            .cloned()
            .or_else(|| management(service))?;
        Some(Target {
            exposed: exposed.clone(),
            service,
            relay_base,
        })
    }

    /// `/x/<stackbox>/<token>/<service>[/<rest>]`.
    async fn service(
        &self,
        request: Request,
        id: &str,
        token: &str,
        service: &str,
        tail: &str,
    ) -> Response {
        let Some(target) = self.resolve(id, token, service) else {
            return not_found();
        };
        if DASHBOARDS.contains(&target.service.id) {
            return dashboard_entry(id, token, service, &target.relay_base);
        }
        let port = target.exposed.port_base + target.service.offset;
        let host = &target.exposed.host;
        let query = request
            .uri()
            .query()
            .map(|q| format!("?{q}"))
            .unwrap_or_default();
        let plan = Plan {
            url: format!("{}/{tail}{query}", base_url(host, port)),
            host: format!("{host}:{port}"),
            origin: None,
            pairs: target
                .exposed
                .services
                .iter()
                .flat_map(|s| {
                    address_pairs(
                        host,
                        target.exposed.port_base + s.offset,
                        &format!("{}/x/{id}/{token}/{}", target.relay_base, s.id),
                    )
                })
                .collect(),
            json: true,
            strip_domain: false,
        };
        self.forward(request, plan).await
    }

    /// The dashboard's own pages, which carry the cookie that names it.
    async fn dashboard(&self, request: Request) -> Response {
        let Some((id, token, service)) = cookie_target(request.headers()) else {
            return not_found();
        };
        let Some(target) = self.resolve(&id, &token, &service) else {
            return not_found();
        };
        if !DASHBOARDS.contains(&target.service.id) {
            return not_found();
        }
        let port = target.exposed.port_base + target.service.offset;
        let upstream = base_url(&target.exposed.host, port);
        let path = request
            .uri()
            .path_and_query()
            .map_or("/", |p| p.as_str())
            .to_owned();
        let plan = Plan {
            url: format!("{upstream}{path}"),
            host: format!("{}:{port}", target.exposed.host),
            origin: Some((target.relay_base.clone(), upstream.clone())),
            pairs: address_pairs(&target.exposed.host, port, &target.relay_base),
            json: false,
            strip_domain: true,
        };
        self.forward(request, plan).await
    }

    async fn forward(&self, request: Request, plan: Plan) -> Response {
        let (parts, body) = request.into_parts();
        let mut headers = HeaderMap::new();
        for (name, value) in &parts.headers {
            if hop_by_hop(name) || forwarding_header(name) {
                continue;
            }
            if *name == header::COOKIE {
                if let Some(cookie) = without_relay_cookie(value) {
                    headers.append(name.clone(), cookie);
                }
                continue;
            }
            let rewritten = match &plan.origin {
                Some((from, to)) if *name == header::ORIGIN || *name == header::REFERER => {
                    let text = replace_all(
                        value.to_str().unwrap_or_default(),
                        &[(from.clone(), to.clone())],
                    );
                    HeaderValue::from_str(&text).unwrap_or_else(|_| value.clone())
                }
                _ => value.clone(),
            };
            headers.append(name.clone(), rewritten);
        }
        let Ok(host) = HeaderValue::from_str(&plan.host) else {
            return not_found();
        };
        headers.insert(header::HOST, host);
        headers.insert(
            header::ACCEPT_ENCODING,
            HeaderValue::from_static("identity"),
        );
        let sent = self
            .client
            .request(parts.method, plan.url.as_str())
            .headers(headers)
            .body(reqwest::Body::wrap_stream(body.into_data_stream()))
            .send()
            .await;
        let response = match sent {
            Ok(response) => response,
            Err(error) => {
                eprintln!("endpoint proxy: {}: {error}", plan.url);
                return (StatusCode::BAD_GATEWAY, "the service did not answer\n").into_response();
            }
        };
        // A body without a length (chunked) is read up to the limit too, not skipped.
        let rewrite_json = plan.json
            && is_json(response.headers())
            && response
                .content_length()
                .is_none_or(|length| length <= REWRITE_LIMIT);
        let mut out = HeaderMap::new();
        for (name, value) in response.headers() {
            if hop_by_hop(name) || (rewrite_json && *name == header::CONTENT_LENGTH) {
                continue;
            }
            let value = if *name == header::LOCATION {
                rewrite_header(value, &plan.pairs)
            } else if *name == header::SET_COOKIE && plan.strip_domain {
                strip_cookie_domain(value)
            } else {
                value.clone()
            };
            out.append(name.clone(), value);
        }
        let status = response.status();
        let body = if rewrite_json {
            match rewrite_body(response, &plan.pairs).await {
                Ok(body) => body,
                Err(_) => {
                    return (StatusCode::BAD_GATEWAY, "the service did not answer\n")
                        .into_response();
                }
            }
        } else {
            Body::from_stream(response.bytes_stream())
        };
        let mut out_response = Response::new(body);
        *out_response.status_mut() = status;
        *out_response.headers_mut() = out;
        out_response
    }
}

struct Target {
    exposed: Exposed,
    service: Service,
    relay_base: String,
}

/// The dashboard's cookie as `(stackbox, token, service)`, if one is present.
fn cookie_target(headers: &HeaderMap) -> Option<(String, String, String)> {
    for value in headers.get_all(header::COOKIE) {
        let Ok(text) = value.to_str() else { continue };
        for pair in text.split(';') {
            let Some(cookie) = pair
                .trim()
                .strip_prefix(COOKIE)
                .and_then(|rest| rest.strip_prefix('='))
            else {
                continue;
            };
            let mut parts = cookie.splitn(3, ':');
            if let (Some(id), Some(token), Some(service)) =
                (parts.next(), parts.next(), parts.next())
            {
                return Some((id.to_owned(), token.to_owned(), service.to_owned()));
            }
        }
    }
    None
}

/// Cookies a dashboard's upstream session uses. They are cleared on entry, so switching between
/// the dashboards of one stackbox (Horizon, Skyline, RabbitMQ) does not mix their sessions.
const SESSION_COOKIES: &[&str] = &["sessionid", "csrftoken"];

/// The entry point of a dashboard: its cookie, the other dashboards' session cookies cleared,
/// then the root of its pages.
fn dashboard_entry(id: &str, token: &str, service: &str, relay_base: &str) -> Response {
    let mut cookie = format!("{COOKIE}={id}:{token}:{service}; Path=/; HttpOnly; SameSite=Lax");
    if relay_base.starts_with("https://") {
        cookie.push_str("; Secure");
    }
    let mut response = Response::new(Body::empty());
    *response.status_mut() = StatusCode::FOUND;
    response
        .headers_mut()
        .insert(header::LOCATION, HeaderValue::from_static("/"));
    let cookies = std::iter::once(cookie).chain(
        SESSION_COOKIES
            .iter()
            .map(|name| format!("{name}=; Path=/; Max-Age=0")),
    );
    for value in cookies {
        match HeaderValue::from_str(&value) {
            Ok(value) => {
                response.headers_mut().append(header::SET_COOKIE, value);
            }
            Err(_) => return not_found(),
        }
    }
    response
}

fn not_found() -> Response {
    (StatusCode::NOT_FOUND, "route not found\n").into_response()
}

/// Connection-scoped headers, which a proxy does not pass on.
fn hop_by_hop(name: &HeaderName) -> bool {
    matches!(
        name.as_str(),
        "connection"
            | "keep-alive"
            | "te"
            | "trailer"
            | "transfer-encoding"
            | "upgrade"
            | "host"
            | "proxy-authorization"
            | "proxy-authenticate"
    ) || name.as_str().starts_with("proxy-")
}

/// Headers that a Cloudflare tunnel or another front proxy adds for the relay's own visitor. The
/// upstream speaks plain HTTP and must not see them: it would believe the connection is HTTPS and
/// answer with `https://` URLs.
fn forwarding_header(name: &HeaderName) -> bool {
    let name = name.as_str();
    name.starts_with("x-forwarded-")
        || name.starts_with("cf-")
        || matches!(name, "forwarded" | "x-real-ip" | "cdn-loop")
}

/// A `Cookie` header without the relay's own `__stackbox_ui` cookie, or `None` when nothing else
/// is left in it.
fn without_relay_cookie(value: &HeaderValue) -> Option<HeaderValue> {
    let Ok(text) = value.to_str() else {
        return Some(value.clone());
    };
    let kept: Vec<&str> = text
        .split(';')
        .map(str::trim)
        .filter(|pair| {
            let relay = pair
                .strip_prefix(COOKIE)
                .is_some_and(|rest| rest.starts_with('='));
            !pair.is_empty() && !relay
        })
        .collect();
    if kept.is_empty() {
        return None;
    }
    HeaderValue::from_str(&kept.join("; ")).ok()
}

/// Reads a JSON body of at most [`REWRITE_LIMIT`] bytes and rewrites its URLs. A body that turns
/// out larger is not held in memory: the bytes read so far go on with the rest of the stream,
/// unrewritten.
async fn rewrite_body(
    response: reqwest::Response,
    pairs: &[(String, String)],
) -> Result<Body, reqwest::Error> {
    let mut upstream = response.bytes_stream();
    let mut read: Vec<u8> = Vec::new();
    while let Some(chunk) = upstream.next().await {
        let chunk = chunk?;
        if read.len() as u64 + chunk.len() as u64 > REWRITE_LIMIT {
            let head = stream::once(async move { Ok::<Bytes, reqwest::Error>(Bytes::from(read)) });
            let next = stream::once(async move { Ok::<Bytes, reqwest::Error>(chunk) });
            return Ok(Body::from_stream(head.chain(next).chain(upstream)));
        }
        read.extend_from_slice(&chunk);
    }
    Ok(match std::str::from_utf8(&read) {
        Ok(text) => Body::from(replace_all(text, pairs)),
        Err(_) => Body::from(read),
    })
}

fn is_json(headers: &HeaderMap) -> bool {
    headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.to_ascii_lowercase())
        .is_some_and(|v| v.starts_with("application/json") || v.contains("+json"))
}

/// A `Set-Cookie` without its `Domain=` attribute, so the browser keeps it on the relay's host.
fn strip_cookie_domain(value: &HeaderValue) -> HeaderValue {
    let Ok(text) = value.to_str() else {
        return value.clone();
    };
    let kept: Vec<&str> = text
        .split(';')
        .filter(|part| !part.trim().to_ascii_lowercase().starts_with("domain="))
        .collect();
    HeaderValue::from_str(&kept.join(";")).unwrap_or_else(|_| value.clone())
}

/// Replace each `from` prefix with its `to`. A match whose next character is a digit is not
/// replaced, so `http://h:1500` does not rewrite the start of `http://h:15000`.
pub fn replace_all(text: &str, pairs: &[(String, String)]) -> String {
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    'scan: while i < text.len() {
        for (from, to) in pairs {
            if !from.is_empty() && text[i..].starts_with(from.as_str()) {
                let next = text[i + from.len()..].chars().next();
                if !next.is_some_and(|c| c.is_ascii_digit()) {
                    out.push_str(to);
                    i += from.len();
                    continue 'scan;
                }
            }
        }
        let Some(c) = text[i..].chars().next() else {
            break;
        };
        out.push(c);
        i += c.len_utf8();
    }
    out
}

fn rewrite_header(value: &HeaderValue, pairs: &[(String, String)]) -> HeaderValue {
    let text = replace_all(value.to_str().unwrap_or_default(), pairs);
    HeaderValue::from_str(&text).unwrap_or_else(|_| value.clone())
}

#[cfg(test)]
mod e2e;

#[cfg(test)]
mod tests {
    use super::*;

    fn pair(from: &str, to: &str) -> (String, String) {
        (from.into(), to.into())
    }

    #[test]
    fn replacement_stops_at_a_port_boundary() {
        let pairs = [pair("http://h:1500", "R/a")];
        assert_eq!(replace_all("http://h:1500/v3", &pairs), "R/a/v3");
        assert_eq!(
            replace_all("http://h:15000/v3", &pairs),
            "http://h:15000/v3"
        );
        assert_eq!(replace_all("see http://h:1500", &pairs), "see R/a");
    }

    #[test]
    fn infrastructure_ports_are_never_public() {
        let all: Vec<String> = COMPONENTS.iter().map(|c| c.id.to_owned()).collect();
        let services = public_services(&all);
        for infra in crate::model::INFRA_PORTS {
            assert!(services.iter().all(|s| s.id != infra.id));
            assert!(services.iter().all(|s| s.offset != infra.offset));
        }
        // Plain HTTP never reaches them, but their relay URLs are listed for the portal.
        let urls = exposure_urls("https://relay:10011", "sb", "tok", &all);
        assert_eq!(
            urls["rabbitmq-management"],
            "https://relay:10011/x/sb/tok/rabbitmq-management"
        );
        assert_eq!(urls["mariadb"], "https://relay:10011/x/sb/tok/tcp/mariadb");
        assert_eq!(
            urls["rabbitmq"],
            "https://relay:10011/x/sb/tok/tcp/rabbitmq"
        );
        assert_eq!(
            urls["tunnel-script"],
            "https://relay:10011/x/sb/tok/tcp/client.mjs"
        );
    }

    #[test]
    fn data_store_tunnels_are_only_mariadb_and_amqp() {
        let proxy = EndpointProxy::default();
        proxy.replace(
            Some("https://relay:10011".into()),
            HashMap::from([(
                "sb".to_owned(),
                Exposed {
                    token: "tok".into(),
                    host: "10.0.0.5".into(),
                    port_base: 20_000,
                    services: Vec::new(),
                },
            )]),
        );
        assert_eq!(
            proxy.resolve_tcp("sb", "tok", "mariadb"),
            Some(("10.0.0.5".into(), 20_006))
        );
        assert_eq!(
            proxy.resolve_tcp("sb", "tok", "rabbitmq"),
            Some(("10.0.0.5".into(), 20_007))
        );
        assert_eq!(proxy.resolve_tcp("sb", "tok", "rabbitmq-management"), None);
        assert_eq!(proxy.resolve_tcp("sb", "bad", "mariadb"), None);
        assert_eq!(proxy.resolve_tcp("other", "tok", "mariadb"), None);
        proxy.replace(None, HashMap::new());
        assert_eq!(proxy.resolve_tcp("sb", "tok", "mariadb"), None);
    }

    #[test]
    fn domain_is_dropped_from_cookies() {
        let value = HeaderValue::from_static("csrftoken=x; Domain=example.com; Path=/; HttpOnly");
        assert_eq!(
            strip_cookie_domain(&value).to_str().unwrap(),
            "csrftoken=x; Path=/; HttpOnly"
        );
    }

    #[test]
    fn cookie_names_one_stackbox_token_and_service() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::COOKIE,
            HeaderValue::from_static("a=1; __stackbox_ui=sb:tok:horizon; b=2"),
        );
        assert_eq!(
            cookie_target(&headers),
            Some(("sb".into(), "tok".into(), "horizon".into()))
        );
        let mut other = HeaderMap::new();
        other.insert(
            header::COOKIE,
            HeaderValue::from_static("__stackbox_ui_x=sb:tok:horizon"),
        );
        assert_eq!(cookie_target(&other), None);
    }
}
