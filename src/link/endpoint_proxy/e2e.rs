//! End to end: the real relay listener in front of mock OpenStack services on a block of
//! consecutive local ports, so `port_base + offset` addresses them as a stackbox's do.

use super::*;
use crate::link::relay::{self, Relay};
use axum::{Router, body::Body, extract::Request, response::IntoResponse};
use std::process::Stdio;
use std::{net::SocketAddr, sync::Arc, time::Duration};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const TOKEN: &str = "t0k3n";

/// Services of a stackbox with Keystone, Glance, Nova and Horizon enabled.
fn services() -> Vec<Service> {
    public_services(&["keystone", "glance", "nova", "horizon"].map(String::from))
}

/// A mock upstream for the service at `offset` of the block at `base`. It answers the paths the
/// tests use; anything else is `200 ok`.
fn mock(base: u16, offset: u16) -> Router {
    Router::new().fallback(move |request: Request| async move {
        let path = request.uri().path().to_owned();
        let header = |name: &str| {
            request
                .headers()
                .get(name)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("-")
                .to_owned()
        };
        let echo = format!(
            "host={} origin={} referer={}",
            header("host"),
            header("origin"),
            header("referer")
        );
        match (offset, path.as_str()) {
            (0, "/v3/auth/tokens") => {
                let catalog = format!(
                    r#"{{"token":{{"catalog":[{{"type":"compute","endpoints":[{{"url":"http://127.0.0.1:{}/v2.1"}}]}}]}}}}"#,
                    base + 5
                );
                ([(header::CONTENT_TYPE, "application/json")], catalog).into_response()
            }
            (0, "/v3/redirect") => (
                StatusCode::FOUND,
                [(header::LOCATION, format!("http://127.0.0.1:{base}/v3/auth/tokens"))],
            )
                .into_response(),
            (0, "/v3/chunked") => {
                let body = format!(
                    r#"{{"version":{{"links":[{{"href":"https://127.0.0.1:{base}/v3/"}}]}},"compute":"http://127.0.0.1:{}/v2.1"}}"#,
                    base + 5
                );
                // A stream without a length goes out `Transfer-Encoding: chunked`.
                let stream = futures::stream::iter([Ok::<_, std::io::Error>(bytes::Bytes::from(body))]);
                let mut response = Body::from_stream(stream).into_response();
                response.headers_mut().insert(
                    header::CONTENT_TYPE,
                    HeaderValue::from_static("application/json"),
                );
                response.headers_mut().insert(
                    "x-seen-xfp",
                    HeaderValue::from_str(&header("x-forwarded-proto")).unwrap(),
                );
                response
            }
            (3, "/auth/probe/") => (
                StatusCode::FOUND,
                [
                    ("location", format!("https://127.0.0.1:{}/auth/login/?next=/", base + 3)),
                    ("x-seen-xfp", header("x-forwarded-proto")),
                    ("x-seen-forwarded", header("forwarded")),
                    ("x-seen-cf-visitor", header("cf-visitor")),
                    ("x-seen-cookie", header("cookie")),
                ],
            )
                .into_response(),
            (8, "/") => "rabbitmq management ui\n".into_response(),
            (1, "/v2/images/file") => (
                [(header::CONTENT_TYPE, "application/octet-stream")],
                image_bytes(),
            )
                .into_response(),
            (3, "/auth/login/") => (
                StatusCode::FOUND,
                [
                    (
                        header::LOCATION,
                        format!("http://127.0.0.1:{}/project/", base + 3),
                    ),
                    (
                        header::SET_COOKIE,
                        "csrftoken=abc; Domain=127.0.0.1; Path=/".to_owned(),
                    ),
                ],
                echo,
            )
                .into_response(),
            _ => "ok".into_response(),
        }
    })
}

fn image_bytes() -> Vec<u8> {
    (0..300_000u32).map(|i| (i % 251) as u8).collect()
}

/// Serve the mocks on nine consecutive ports (offsets 0 through 8: Keystone, Glance, Horizon,
/// Nova, MariaDB, RabbitMQ AMQP and its management UI), and return the first one. Offsets 6 and 7
/// are TCP echo servers (MariaDB, AMQP), the rest are HTTP mocks.
async fn upstream_block() -> u16 {
    loop {
        let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = probe.local_addr().unwrap().port();
        drop(probe);
        if base > 65_000 {
            continue;
        }
        let mut listeners = Vec::new();
        for offset in 0..=8 {
            match std::net::TcpListener::bind(("127.0.0.1", base + offset)) {
                Ok(listener) => listeners.push(listener),
                Err(_) => break,
            }
        }
        if listeners.len() < 9 {
            continue;
        }
        for (offset, listener) in listeners.into_iter().enumerate() {
            listener.set_nonblocking(true).unwrap();
            let listener = tokio::net::TcpListener::from_std(listener).unwrap();
            if offset == 6 || offset == 7 {
                tokio::spawn(echo(listener));
                continue;
            }
            let app = mock(base, offset as u16);
            tokio::spawn(async move { axum::serve(listener, app).await });
        }
        return base;
    }
}

/// A TCP echo server: each connection's bytes go back to it, then it closes.
async fn echo(listener: tokio::net::TcpListener) {
    while let Ok((socket, _)) = listener.accept().await {
        tokio::spawn(async move {
            let (mut reader, mut writer) = socket.into_split();
            let _ = tokio::io::copy(&mut reader, &mut writer).await;
            let _ = writer.shutdown().await;
        });
    }
}

fn exposed(token: &str, base: u16) -> Exposed {
    Exposed {
        token: token.into(),
        host: "127.0.0.1".into(),
        port_base: base,
        services: services(),
    }
}

/// A relay listening on a free port, with `sb` exposed at `base` under `token` when given.
async fn relay_for(base: u16, token: Option<&str>) -> (Arc<Relay>, String) {
    let relay = Arc::new(Relay::default());
    let bind: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let (addr, _task) = relay::listen(relay.clone(), bind, None).await.unwrap();
    let relay_base = format!("http://{addr}");
    let stackboxes = token
        .map(|token| HashMap::from([("sb".to_owned(), exposed(token, base))]))
        .unwrap_or_default();
    relay
        .endpoints
        .replace(Some(relay_base.clone()), stackboxes);
    (relay, relay_base)
}

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap()
}

#[tokio::test]
async fn routes_outside_an_exposed_service_answer_404_and_the_switch_socket_is_guarded() {
    let base = upstream_block().await;
    let (_relay, relay_base) = relay_for(base, Some(TOKEN)).await;
    let client = client();
    let not_found = |url: String| {
        let client = client.clone();
        async move {
            let response = client.get(url).send().await.unwrap();
            assert_eq!(response.status(), StatusCode::NOT_FOUND);
        }
    };
    not_found(format!("{relay_base}/")).await;
    not_found(format!("{relay_base}/x/sb/bad/keystone/v3")).await;
    not_found(format!("{relay_base}/x/sb/{TOKEN}/mariadb/")).await;
    not_found(format!("{relay_base}/x/sb/{TOKEN}/mariadb")).await;
    not_found(format!("{relay_base}/x/sb/{TOKEN}/rabbitmq")).await;
    not_found(format!("{relay_base}/x/sb/{TOKEN}/rabbitmq/")).await;
    not_found(format!("{relay_base}/x/nope/{TOKEN}/keystone/v3")).await;
    // The switches' WebSocket route is still there, and it still wants an uplink token.
    let upgrade = client
        .get(format!(
            "{relay_base}/api/v1/link-relay?stackbox=sb&host=h1"
        ))
        .header("connection", "Upgrade")
        .header("upgrade", "websocket")
        .header("sec-websocket-version", "13")
        .header("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ==")
        .send()
        .await
        .unwrap();
    assert_eq!(upgrade.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn with_the_relay_off_every_endpoint_is_404() {
    let base = upstream_block().await;
    let (relay, relay_base) = relay_for(base, Some(TOKEN)).await;
    relay
        .endpoints
        .replace(None, HashMap::from([("sb".into(), exposed(TOKEN, base))]));
    let response = client()
        .get(format!("{relay_base}/x/sb/{TOKEN}/keystone/v3/auth/tokens"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn keystone_catalog_and_redirects_point_at_the_relay() {
    let base = upstream_block().await;
    let (_relay, relay_base) = relay_for(base, Some(TOKEN)).await;
    let client = client();
    let response = client
        .get(format!("{relay_base}/x/sb/{TOKEN}/keystone/v3/auth/tokens"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = response.text().await.unwrap();
    let nova = format!("{relay_base}/x/sb/{TOKEN}/nova/v2.1");
    assert!(body.contains(&nova), "catalog: {body}");
    assert!(
        !body.contains(&format!("127.0.0.1:{}", base + 5)),
        "catalog: {body}"
    );

    let response = client
        .get(format!("{relay_base}/x/sb/{TOKEN}/keystone/v3/redirect"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FOUND);
    assert_eq!(
        response.headers()[header::LOCATION],
        format!("{relay_base}/x/sb/{TOKEN}/keystone/v3/auth/tokens").as_str()
    );
}

#[tokio::test]
async fn binary_bodies_stream_through_untouched() {
    let base = upstream_block().await;
    let (_relay, relay_base) = relay_for(base, Some(TOKEN)).await;
    let response = client()
        .get(format!("{relay_base}/x/sb/{TOKEN}/glance/v2/images/file"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.bytes().await.unwrap().as_ref(),
        image_bytes().as_slice()
    );
}

#[tokio::test]
async fn the_dashboard_cookie_gates_its_pages_and_rewrites_their_origins() {
    let base = upstream_block().await;
    let (_relay, relay_base) = relay_for(base, Some(TOKEN)).await;
    let client = client();
    let entry = client
        .get(format!("{relay_base}/x/sb/{TOKEN}/horizon"))
        .send()
        .await
        .unwrap();
    assert_eq!(entry.status(), StatusCode::FOUND);
    assert_eq!(entry.headers()[header::LOCATION], "/");
    let cookie = entry.headers()[header::SET_COOKIE]
        .to_str()
        .unwrap()
        .to_owned();
    assert!(
        cookie.starts_with(&format!(
            "__stackbox_ui=sb:{TOKEN}:horizon; Path=/; HttpOnly"
        )),
        "cookie: {cookie}"
    );

    let jar = format!("__stackbox_ui=sb:{TOKEN}:horizon");
    let login = client
        .post(format!("{relay_base}/auth/login/"))
        .header(header::COOKIE, &jar)
        .header(header::ORIGIN, &relay_base)
        .header(header::REFERER, format!("{relay_base}/auth/login/"))
        .body("username=admin")
        .send()
        .await
        .unwrap();
    assert_eq!(login.status(), StatusCode::FOUND);
    assert_eq!(
        login.headers()[header::LOCATION],
        format!("{relay_base}/project/").as_str()
    );
    let set_cookie = login.headers()[header::SET_COOKIE]
        .to_str()
        .unwrap()
        .to_owned();
    assert!(
        !set_cookie.to_ascii_lowercase().contains("domain"),
        "{set_cookie}"
    );
    let echoed = login.text().await.unwrap();
    let upstream = format!("127.0.0.1:{}", base + 3);
    assert_eq!(
        echoed,
        format!("host={upstream} origin=http://{upstream} referer=http://{upstream}/auth/login/")
    );

    // The dashboard's pages need the cookie; another service's cookie does not open them.
    let no_cookie = client
        .get(format!("{relay_base}/project/"))
        .send()
        .await
        .unwrap();
    assert_eq!(no_cookie.status(), StatusCode::NOT_FOUND);
    let keystone_cookie = client
        .get(format!("{relay_base}/project/"))
        .header(header::COOKIE, format!("__stackbox_ui=sb:{TOKEN}:keystone"))
        .send()
        .await
        .unwrap();
    assert_eq!(keystone_cookie.status(), StatusCode::NOT_FOUND);
    let wrong_token = client
        .get(format!("{relay_base}/project/"))
        .header(header::COOKIE, "__stackbox_ui=sb:bad:horizon")
        .send()
        .await
        .unwrap();
    assert_eq!(wrong_token.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn cloudflare_forwarding_headers_stay_off_the_upstream_and_the_internal_cookie() {
    let base = upstream_block().await;
    let (_relay, relay_base) = relay_for(base, Some(TOKEN)).await;
    let response = client()
        .get(format!("{relay_base}/auth/probe/"))
        .header(
            header::COOKIE,
            format!("__stackbox_ui=sb:{TOKEN}:horizon; sessionid=abc"),
        )
        .header("x-forwarded-proto", "https")
        .header("forwarded", "proto=https")
        .header("cf-visitor", r#"{"scheme":"https"}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FOUND);
    assert_eq!(
        response.headers()["x-seen-xfp"],
        "-",
        "x-forwarded-proto reached the upstream"
    );
    assert_eq!(
        response.headers()["x-seen-forwarded"],
        "-",
        "forwarded reached the upstream"
    );
    assert_eq!(
        response.headers()["x-seen-cf-visitor"],
        "-",
        "cf-visitor reached the upstream"
    );
    assert_eq!(
        response.headers()["x-seen-cookie"],
        "sessionid=abc",
        "the internal cookie reached the upstream"
    );
    assert_eq!(
        response.headers()[header::LOCATION],
        format!("{relay_base}/auth/login/?next=/").as_str(),
        "an https Location was not rewritten to the relay"
    );
}

#[tokio::test]
async fn chunked_json_is_rewritten_and_forwarding_headers_are_dropped() {
    let base = upstream_block().await;
    let (_relay, relay_base) = relay_for(base, Some(TOKEN)).await;
    let response = client()
        .get(format!("{relay_base}/x/sb/{TOKEN}/keystone/v3/chunked"))
        .header("x-forwarded-proto", "https")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["x-seen-xfp"], "-");
    let body = response.text().await.unwrap();
    let href = format!("{relay_base}/x/sb/{TOKEN}/keystone/v3/");
    let compute = format!("{relay_base}/x/sb/{TOKEN}/nova/v2.1");
    assert!(body.contains(&href), "keystone href not rewritten: {body}");
    assert!(body.contains(&compute), "nova url not rewritten: {body}");
    assert!(
        !body.contains(&format!("127.0.0.1:{base}"))
            && !body.contains(&format!("127.0.0.1:{}", base + 5)),
        "upstream address leaked: {body}"
    );
}

#[tokio::test]
async fn rotating_or_removing_the_exposure_revokes_old_urls_and_cookies_at_once() {
    let base = upstream_block().await;
    let (relay, relay_base) = relay_for(base, Some(TOKEN)).await;
    let client = client();
    let old = format!("{relay_base}/x/sb/{TOKEN}/keystone/v3/auth/tokens");
    assert_eq!(
        client.get(&old).send().await.unwrap().status(),
        StatusCode::OK
    );
    let entry = client
        .get(format!("{relay_base}/x/sb/{TOKEN}/horizon"))
        .send()
        .await
        .unwrap();
    assert_eq!(entry.status(), StatusCode::FOUND);
    let old_cookie = format!("__stackbox_ui=sb:{TOKEN}:horizon");

    relay.endpoints.replace(
        Some(relay_base.clone()),
        HashMap::from([("sb".into(), exposed("n3w", base))]),
    );
    assert_eq!(
        client.get(&old).send().await.unwrap().status(),
        StatusCode::NOT_FOUND
    );
    let stale = client
        .get(format!("{relay_base}/project/"))
        .header(header::COOKIE, &old_cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(stale.status(), StatusCode::NOT_FOUND);
    let fresh = format!("{relay_base}/x/sb/n3w/keystone/v3/auth/tokens");
    assert_eq!(
        client.get(&fresh).send().await.unwrap().status(),
        StatusCode::OK
    );

    relay
        .endpoints
        .replace(Some(relay_base.clone()), HashMap::new());
    assert_eq!(
        client.get(&fresh).send().await.unwrap().status(),
        StatusCode::NOT_FOUND
    );
}

/// The `ws://` URL of a stackbox's data store tunnel, from the relay's `http://` base.
fn tunnel_url(relay_base: &str, token: &str, service: &str) -> String {
    format!(
        "{}/x/sb/{token}/tcp/{service}",
        crate::link::tcp_tunnel::to_ws_url(relay_base).unwrap()
    )
}

#[tokio::test]
async fn rabbitmq_management_is_a_dashboard_behind_its_cookie() {
    let base = upstream_block().await;
    let (_relay, relay_base) = relay_for(base, Some(TOKEN)).await;
    let client = client();
    let entry = client
        .get(format!("{relay_base}/x/sb/{TOKEN}/rabbitmq-management"))
        .send()
        .await
        .unwrap();
    assert_eq!(entry.status(), StatusCode::FOUND);
    assert_eq!(entry.headers()[header::LOCATION], "/");
    let cookie = entry.headers()[header::SET_COOKIE].to_str().unwrap();
    assert!(
        cookie.starts_with(&format!(
            "__stackbox_ui=sb:{TOKEN}:rabbitmq-management; Path=/; HttpOnly"
        )),
        "cookie: {cookie}"
    );
    let page = client
        .get(format!("{relay_base}/"))
        .header(
            header::COOKIE,
            format!("__stackbox_ui=sb:{TOKEN}:rabbitmq-management"),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(page.status(), StatusCode::OK);
    assert_eq!(page.text().await.unwrap(), "rabbitmq management ui\n");
    let no_cookie = client.get(format!("{relay_base}/")).send().await.unwrap();
    assert_eq!(no_cookie.status(), StatusCode::NOT_FOUND);
}

fn payload(seed: u8) -> Vec<u8> {
    (0..300_000u32)
        .map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed))
        .collect()
}

#[tokio::test]
async fn tunnel_carries_two_concurrent_mariadb_connections_byte_for_byte() {
    let base = upstream_block().await;
    let (_relay, relay_base) = relay_for(base, Some(TOKEN)).await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let local = listener.local_addr().unwrap();
    tokio::spawn(crate::link::tcp_tunnel::serve(
        listener,
        tunnel_url(&relay_base, TOKEN, "mariadb"),
    ));
    let exchange = |seed: u8| async move {
        let data = payload(seed);
        let stream = tokio::net::TcpStream::connect(local).await.unwrap();
        let (mut reader, mut writer) = stream.into_split();
        let sent = data.clone();
        // The write half lives until the echo is read back: dropping it would send a FIN.
        let write = async move {
            writer.write_all(&sent).await.unwrap();
            writer
        };
        let read = async {
            let mut back = vec![0u8; data.len()];
            reader.read_exact(&mut back).await.unwrap();
            back
        };
        let (_writer, back) = tokio::join!(write, read);
        assert!(back == data, "echo differs for connection {seed}");
    };
    tokio::time::timeout(Duration::from_secs(30), async {
        tokio::join!(exchange(1), exchange(2));
    })
    .await
    .expect("the tunnel moved both connections");
}

/// Whether the relay refuses a WebSocket upgrade to `url` with `404`.
async fn refused(url: String) -> bool {
    match tokio_tungstenite::connect_async(url).await {
        Err(tokio_tungstenite::tungstenite::Error::Http(response)) => {
            response.status() == StatusCode::NOT_FOUND
        }
        _ => false,
    }
}

#[tokio::test]
async fn tunnel_script_is_served_to_the_current_token_only() {
    let base = upstream_block().await;
    let (relay, relay_base) = relay_for(base, Some(TOKEN)).await;
    let client = client();
    let url = format!("{relay_base}/x/sb/{TOKEN}/tcp/client.mjs");
    let response = client.get(&url).send().await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert!(
        response.headers()[header::CONTENT_TYPE]
            .to_str()
            .unwrap()
            .starts_with("text/javascript")
    );
    assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
    let body = response.text().await.unwrap();
    let ws = relay_base.replacen("http://", "ws://", 1);
    assert!(
        body.contains(&format!("\"{ws}/x/sb/{TOKEN}/tcp/mariadb\"")),
        "{body}"
    );
    assert!(
        body.contains(&format!("\"{ws}/x/sb/{TOKEN}/tcp/rabbitmq\"")),
        "{body}"
    );
    assert_eq!(
        client
            .get(format!("{relay_base}/x/sb/bad/tcp/client.mjs"))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::NOT_FOUND
    );
    relay.endpoints.replace(None, HashMap::new());
    assert_eq!(
        client.get(&url).send().await.unwrap().status(),
        StatusCode::NOT_FOUND
    );
}

/// Connect to a tunnel's local port, send `data`, and read the echo back.
async fn echo_through(port: u16, data: &[u8]) {
    let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .unwrap();
    stream.write_all(data).await.unwrap();
    let mut back = vec![0u8; data.len()];
    tokio::time::timeout(Duration::from_secs(10), stream.read_exact(&mut back))
        .await
        .expect("the echo arrived")
        .unwrap();
    assert!(back == data, "echo differs");
}

#[tokio::test]
async fn node_runs_the_tunnel_script_and_both_stores_echo_through_it() {
    use tokio::io::{AsyncBufReadExt, BufReader};
    let base = upstream_block().await;
    let (_relay, relay_base) = relay_for(base, Some(TOKEN)).await;
    let script = client()
        .get(format!("{relay_base}/x/sb/{TOKEN}/tcp/client.mjs"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    // Two free ports for the script's listeners, held until node is about to bind them.
    let held: Vec<std::net::TcpListener> = (0..2)
        .map(|_| std::net::TcpListener::bind("127.0.0.1:0").unwrap())
        .collect();
    let mysql_port = held[0].local_addr().unwrap().port();
    let amqp_port = held[1].local_addr().unwrap().port();
    drop(held);

    let mut child = tokio::process::Command::new("node")
        .arg("--input-type=module")
        .env("MYSQL_PORT", mysql_port.to_string())
        .env("AMQP_PORT", amqp_port.to_string())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .expect("node on PATH");
    {
        // Dropping stdin at the end of the block closes it, which ends node's read of the script.
        let mut stdin = child.stdin.take().unwrap();
        stdin.write_all(script.as_bytes()).await.unwrap();
    }
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    let mut mysql_ready = false;
    let mut amqp_ready = false;
    tokio::time::timeout(Duration::from_secs(30), async {
        while !(mysql_ready && amqp_ready) {
            let Some(line) = lines.next_line().await.unwrap() else {
                panic!("node exited before it listened");
            };
            mysql_ready |= line == format!("MariaDB (MySQL) -> 127.0.0.1:{mysql_port}");
            amqp_ready |= line == format!("RabbitMQ (AMQP) -> 127.0.0.1:{amqp_port}");
        }
    })
    .await
    .expect("the script listened on both ports");

    echo_through(mysql_port, &payload(7)).await;
    echo_through(amqp_port, b"AMQP\x00\x00\x09\x01").await;

    child.kill().await.unwrap();
    child.wait().await.unwrap();
}

#[tokio::test]
async fn tunnel_upgrades_are_refused_for_bad_tokens_services_and_a_disabled_relay() {
    let base = upstream_block().await;
    let (relay, relay_base) = relay_for(base, Some(TOKEN)).await;
    assert!(refused(tunnel_url(&relay_base, "bad", "mariadb")).await);
    assert!(refused(tunnel_url(&relay_base, TOKEN, "rabbitmq-management")).await);
    assert!(refused(tunnel_url(&relay_base, TOKEN, "keystone")).await);
    assert!(refused(tunnel_url(&relay_base, "nope", "mariadb")).await);
    relay.endpoints.replace(None, HashMap::new());
    assert!(refused(tunnel_url(&relay_base, TOKEN, "mariadb")).await);
}

/// The status a WebSocket upgrade to `url` is refused with, or `None` when it is accepted.
async fn upgrade_refused_with(url: String) -> Option<StatusCode> {
    match tokio_tungstenite::connect_async(url).await {
        Ok(_) => None,
        Err(tokio_tungstenite::tungstenite::Error::Http(response)) => Some(response.status()),
        Err(error) => panic!("unexpected upgrade error: {error}"),
    }
}

#[tokio::test]
async fn tcp_tunnels_are_capped_and_a_closed_one_frees_its_slot() {
    use futures::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message;

    let base = upstream_block().await;
    let mut relay = Relay::default();
    relay.endpoints = Arc::new(EndpointProxy::with_tcp_limit(2));
    let relay = Arc::new(relay);
    let bind: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let (addr, _task) = relay::listen(relay.clone(), bind, None).await.unwrap();
    let relay_base = format!("http://{addr}");
    relay.endpoints.replace(
        Some(relay_base.clone()),
        HashMap::from([("sb".to_owned(), exposed(TOKEN, base))]),
    );
    let url = tunnel_url(&relay_base, TOKEN, "mariadb");

    let (mut first, _) = tokio_tungstenite::connect_async(url.clone()).await.unwrap();
    let (mut second, _) = tokio_tungstenite::connect_async(url.clone()).await.unwrap();
    assert_eq!(
        upgrade_refused_with(url.clone()).await,
        Some(StatusCode::SERVICE_UNAVAILABLE)
    );

    // Both open tunnels still carry bytes while the third is refused.
    for socket in [&mut first, &mut second] {
        socket
            .send(Message::Binary(b"ping".to_vec().into()))
            .await
            .unwrap();
        let echoed = socket.next().await.unwrap().unwrap();
        assert_eq!(echoed.into_data().as_ref(), b"ping");
    }

    // Closing one frees its slot; the relay releases it as its bridge ends.
    first.close(None).await.unwrap();
    let mut reopened = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Ok((socket, _)) = tokio_tungstenite::connect_async(url.clone()).await {
                return socket;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("a closed tunnel frees its slot");
    reopened
        .send(Message::Binary(b"pong".to_vec().into()))
        .await
        .unwrap();
    let echoed = reopened.next().await.unwrap().unwrap();
    assert_eq!(echoed.into_data().as_ref(), b"pong");
    drop(reopened);
    drop(second);
}
