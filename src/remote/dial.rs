//! Hosts that dial in. Instead of the coordinator reaching a host over SSH or as a child
//! process, the host's agent holds a WebSocket open to the coordinator and serves the same
//! requests over it. This works behind NAT, and for a coordinator that cannot run processes
//! or open SSH sessions itself.

use super::{
    agent::{Agent, Emitter},
    protocol::{Frame, Request, TerminalFrame},
};
use anyhow::{Context, Result, bail};
use futures::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, sync::Arc, time::Duration};
use tokio::{io::AsyncWriteExt, sync::mpsc};
use tokio_tungstenite::tungstenite::{Message, client::IntoClientRequest, http::HeaderValue};

/// One message on a dial-in socket. Several requests share the socket, told apart by `session`.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Envelope {
    /// Coordinator → agent: run a request.
    Open { session: u64, request: Box<Request> },
    /// Coordinator → agent: keystrokes or a resize for a shell session.
    Input { session: u64, frame: TerminalFrame },
    /// Coordinator → agent: the shell's client left; hang it up.
    Close { session: u64 },
    /// Agent → coordinator: progress, terminal output or the outcome of a session.
    Frame { session: u64, frame: Frame },
}

pub const CONNECT_PATH: &str = "/api/v1/agent/connect";

/// `ws(s)://<coordinator>/api/v1/agent/connect` for the coordinator's base URL.
pub fn connect_url(coordinator: &str) -> Result<String> {
    let base = coordinator.trim_end_matches('/');
    let (scheme, rest) = base
        .split_once("://")
        .context("coordinator URL needs a scheme, such as https://stackbox.example")?;
    let scheme = match scheme {
        "http" | "ws" => "ws",
        "https" | "wss" => "wss",
        other => bail!("unsupported coordinator URL scheme '{other}'"),
    };
    Ok(format!("{scheme}://{rest}{CONNECT_PATH}"))
}

/// Keep a connection to the coordinator, reconnecting with backoff. Runs until killed.
pub async fn run(coordinator: &str, token: &str, agent: Arc<Agent>) -> Result<()> {
    let url = connect_url(coordinator)?;
    // tokio-tungstenite builds its TLS client from the process-wide provider.
    let _ = rustls::crypto::ring::default_provider().install_default();
    let mut delay = Duration::from_secs(1);
    loop {
        match serve(&url, token, agent.clone()).await {
            Ok(()) => {
                eprintln!("stackbox agent: coordinator closed the connection");
                delay = Duration::from_secs(1);
            }
            Err(error) => eprintln!("stackbox agent: {error:#}"),
        }
        tokio::time::sleep(delay).await;
        delay = (delay * 2).min(Duration::from_secs(30));
    }
}

/// One connection: serve sessions until the socket closes. Running sessions stop with it,
/// as they would when an SSH connection drops.
pub async fn serve(url: &str, token: &str, agent: Arc<Agent>) -> Result<()> {
    let mut request = url.into_client_request()?;
    request.headers_mut().insert(
        "authorization",
        HeaderValue::from_str(&format!("Bearer {token}"))?,
    );
    let (socket, _) = tokio_tungstenite::connect_async(request)
        .await
        .with_context(|| format!("cannot connect to {url}"))?;
    eprintln!("stackbox agent: connected to {url}");
    let (mut sink, mut stream) = socket.split();
    let (outgoing, mut outbox) = mpsc::unbounded_channel::<Envelope>();
    let writer = async move {
        while let Some(envelope) = outbox.recv().await {
            sink.send(Message::Text(serde_json::to_string(&envelope)?.into()))
                .await?;
        }
        Ok::<_, anyhow::Error>(())
    };
    let mut sessions = Sessions::default();
    let reader = async {
        while let Some(message) = stream.next().await {
            let text = match message? {
                Message::Text(text) => text,
                Message::Close(_) => break,
                _ => continue,
            };
            match serde_json::from_str::<Envelope>(&text).context("invalid coordinator message")? {
                Envelope::Open { session, request } => {
                    sessions.open(session, *request, agent.clone(), outgoing.clone())
                }
                Envelope::Input { session, frame } => sessions.input(session, frame).await,
                Envelope::Close { session } => sessions.close(session),
                Envelope::Frame { .. } => bail!("coordinator sent an agent frame"),
            }
        }
        Ok(())
    };
    let result = tokio::select! {
        result = reader => result,
        result = writer => result,
    };
    sessions.abort_all();
    result
}

#[derive(Default)]
struct Sessions {
    tasks: HashMap<u64, tokio::task::JoinHandle<()>>,
    /// Shell input, written to the session's terminal stream.
    inputs: HashMap<u64, tokio::io::DuplexStream>,
}
impl Sessions {
    fn open(
        &mut self,
        session: u64,
        request: Request,
        agent: Arc<Agent>,
        outgoing: mpsc::UnboundedSender<Envelope>,
    ) {
        self.tasks.retain(|_, task| !task.is_finished());
        let emit: Emitter = {
            let outgoing = outgoing.clone();
            Arc::new(move |frame| {
                outgoing
                    .send(Envelope::Frame { session, frame })
                    .map_err(|_| anyhow::anyhow!("coordinator connection closed"))
            })
        };
        let shell = request.operation.streams_input();
        let reader = shell.then(|| {
            let (reader, writer) = tokio::io::duplex(64 * 1024);
            self.inputs.insert(session, writer);
            reader
        });
        let task = tokio::spawn(async move {
            let result = match reader {
                Some(reader) => {
                    agent
                        .terminal(&request, tokio::io::BufReader::new(reader), emit.clone())
                        .await
                }
                None => agent.execute(request, emit.clone()).await,
            };
            let _ = emit(match result {
                Ok(result) => Frame::Result { result },
                Err(error) => Frame::Error {
                    message: format!("{error:#}"),
                },
            });
        });
        self.tasks.insert(session, task);
    }
    async fn input(&mut self, session: u64, frame: TerminalFrame) {
        let Some(writer) = self.inputs.get_mut(&session) else {
            return;
        };
        let mut line = serde_json::to_vec(&frame).unwrap_or_default();
        line.push(b'\n');
        if writer.write_all(&line).await.is_err() {
            self.inputs.remove(&session);
        }
    }
    /// EOF on the shell's input makes the agent hang it up and report.
    fn close(&mut self, session: u64) {
        self.inputs.remove(&session);
    }
    fn abort_all(&mut self) {
        for (_, task) in self.tasks.drain() {
            task.abort();
        }
        self.inputs.clear();
    }
}
