//! `stackbox tunnel`: a local TCP port that forwards to a stackbox's MariaDB or RabbitMQ AMQP port
//! through its relay. Each connection opens one WebSocket to `/x/<stackbox>/<token>/tcp/<service>`
//! and copies bytes both ways as binary frames; the relay ([`super::relay`]) does the other half.
//!
//! The URL is the one the portal shows (`https://<relay>/x/<stackbox>/<token>/tcp/mariadb`). The
//! local address defaults from its last path segment: `127.0.0.1:3306` for MariaDB, `127.0.0.1:5672`
//! for RabbitMQ.

use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use bytes::Bytes;
use futures::{SinkExt, StreamExt};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};
use tokio_tungstenite::tungstenite::Message;

use super::relay::PING_EVERY;

/// The relay URL as a WebSocket URL: `https://` becomes `wss://`, `http://` becomes `ws://`.
pub fn to_ws_url(url: &str) -> Result<String> {
    let url = url.trim();
    if let Some(rest) = url.strip_prefix("https://") {
        Ok(format!("wss://{rest}"))
    } else if let Some(rest) = url.strip_prefix("http://") {
        Ok(format!("ws://{rest}"))
    } else if url.starts_with("wss://") || url.starts_with("ws://") {
        Ok(url.to_owned())
    } else {
        bail!("the tunnel URL {url} needs an https:// (or http://) scheme")
    }
}

/// The local address a data store's tunnel listens on by default, from the URL's last path segment
/// (query and fragment ignored): `mariadb` is `127.0.0.1:3306`, `rabbitmq` is `127.0.0.1:5672`.
pub fn default_listen_addr(url: &str) -> Option<&'static str> {
    let path = url
        .split(['?', '#'])
        .next()
        .unwrap_or("")
        .trim_end_matches('/');
    if path.ends_with("/mariadb") {
        Some("127.0.0.1:3306")
    } else if path.ends_with("/rabbitmq") {
        Some("127.0.0.1:5672")
    } else {
        None
    }
}

/// Listen on `listen` (or the URL's default address) and forward each connection through the
/// relay at `url`, until Ctrl-C.
pub async fn run(url: &str, listen: Option<&str>) -> Result<()> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let ws_url = to_ws_url(url)?;
    let addr = match listen {
        Some(addr) => addr,
        None => default_listen_addr(url).ok_or_else(|| {
            anyhow!("pass --listen <addr>: the URL does not end in /mariadb or /rabbitmq")
        })?,
    };
    let listener = TcpListener::bind(addr)
        .await
        .with_context(|| format!("bind the tunnel on {addr}"))?;
    println!("listening on {} -> {ws_url}", listener.local_addr()?);
    tokio::select! {
        result = serve(listener, ws_url) => result,
        result = tokio::signal::ctrl_c() => {
            result?;
            Ok(())
        }
    }
}

/// Accept TCP connections on `listener` and forward each one over its own WebSocket to `ws_url`.
/// Connections run concurrently; one that fails ends alone.
pub async fn serve(listener: TcpListener, ws_url: String) -> Result<()> {
    let ws_url = std::sync::Arc::new(ws_url);
    loop {
        let (tcp, peer) = match listener.accept().await {
            Ok(accepted) => accepted,
            Err(error) => {
                // Out of file descriptors and the like: keep listening once it clears.
                eprintln!("tunnel: accept: {error}");
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };
        let ws_url = ws_url.clone();
        tokio::spawn(async move {
            if let Err(error) = forward(tcp, &ws_url).await {
                eprintln!("tunnel: {peer}: {error:#}");
            }
        });
    }
}

/// One TCP connection's bytes, both ways, over one WebSocket.
async fn forward(tcp: TcpStream, ws_url: &str) -> Result<()> {
    let (ws, _) = tokio_tungstenite::connect_async(ws_url)
        .await
        .context("open the tunnel's WebSocket")?;
    let (mut sink, mut stream) = ws.split();
    let _ = tcp.set_nodelay(true);
    let (mut reader, mut writer) = tcp.into_split();
    let mut buf = vec![0u8; 16 * 1024];
    let mut ping = tokio::time::interval(PING_EVERY);
    ping.tick().await;
    loop {
        tokio::select! {
            read = reader.read(&mut buf) => {
                let n = match read {
                    Ok(0) | Err(_) => break,
                    Ok(n) => n,
                };
                sink.send(Message::Binary(Bytes::copy_from_slice(&buf[..n]))).await?;
            }
            message = stream.next() => match message {
                Some(Ok(Message::Binary(data))) => writer.write_all(&data).await?,
                Some(Ok(Message::Close(_))) | None => break,
                Some(Err(error)) => return Err(error.into()),
                Some(Ok(_)) => {}
            },
            _ = ping.tick() => sink.send(Message::Ping(Bytes::new())).await?,
        }
    }
    let _ = writer.shutdown().await;
    let _ = sink.send(Message::Close(None)).await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relay_urls_become_websocket_urls() {
        assert_eq!(
            to_ws_url("https://relay.example/x/sb/t/tcp/mariadb").unwrap(),
            "wss://relay.example/x/sb/t/tcp/mariadb"
        );
        assert_eq!(
            to_ws_url("http://127.0.0.1:10012/x/sb/t/tcp/rabbitmq").unwrap(),
            "ws://127.0.0.1:10012/x/sb/t/tcp/rabbitmq"
        );
        assert_eq!(
            to_ws_url("wss://relay.example/x").unwrap(),
            "wss://relay.example/x"
        );
        assert_eq!(
            to_ws_url("ws://127.0.0.1:1/x").unwrap(),
            "ws://127.0.0.1:1/x"
        );
        assert!(to_ws_url("ftp://relay.example/x").is_err());
        assert!(to_ws_url("relay.example/x").is_err());
    }

    #[test]
    fn data_store_urls_pick_their_default_port() {
        assert_eq!(
            default_listen_addr("https://r/x/sb/t/tcp/mariadb"),
            Some("127.0.0.1:3306")
        );
        assert_eq!(
            default_listen_addr("https://r/x/sb/t/tcp/rabbitmq?x=1"),
            Some("127.0.0.1:5672")
        );
        assert_eq!(
            default_listen_addr("https://r/x/sb/t/rabbitmq-management"),
            None
        );
        assert_eq!(default_listen_addr("https://r/x/sb/t/tcp/other"), None);
    }
}
