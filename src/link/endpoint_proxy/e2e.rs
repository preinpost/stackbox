//! End to end: the real relay listener in front of mock OpenStack services on a block of
//! consecutive local ports, so `port_base + offset` addresses them as a stackbox's do.

use super::*;
use crate::link::relay::{self, Relay};
use axum::{Router, extract::Request, response::IntoResponse};
use std::{net::SocketAddr, sync::Arc};

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

/// Serve the mocks on six consecutive ports (offsets 0 through 5, which covers Keystone, Glance,
/// Horizon and Nova), and return the first one.
async fn upstream_block() -> u16 {
    loop {
        let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = probe.local_addr().unwrap().port();
        drop(probe);
        if base > 65_000 {
            continue;
        }
        let mut listeners = Vec::new();
        for offset in 0..6 {
            match std::net::TcpListener::bind(("127.0.0.1", base + offset)) {
                Ok(listener) => listeners.push(listener),
                Err(_) => break,
            }
        }
        if listeners.len() < 6 {
            continue;
        }
        for (offset, listener) in listeners.into_iter().enumerate() {
            listener.set_nonblocking(true).unwrap();
            let listener = tokio::net::TcpListener::from_std(listener).unwrap();
            let app = mock(base, offset as u16);
            tokio::spawn(async move { axum::serve(listener, app).await });
        }
        return base;
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
    not_found(format!("{relay_base}/x/sb/{TOKEN}/rabbitmq-management/")).await;
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
