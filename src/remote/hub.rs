//! The coordinator's side of dial-in hosts: the open agent sockets, and a link that sends
//! requests over them.

use super::{
    dial::Envelope,
    link::AgentLink,
    protocol::{Frame, Reply, Request, TerminalFrame, deliver},
};
use crate::{
    farm::{TerminalInput, TerminalIo},
    kolla::EventSink,
};
use anyhow::{Context, Result, anyhow, bail};
use base64::{Engine, engine::general_purpose::STANDARD};
use futures::future::BoxFuture;
use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::sync::mpsc;

struct Connection {
    /// Distinguishes a reconnect from the socket it replaced.
    id: u64,
    outgoing: mpsc::UnboundedSender<Envelope>,
    sessions: Arc<Mutex<HashMap<u64, mpsc::UnboundedSender<Frame>>>>,
}

/// Connected agents by host ID.
#[derive(Default)]
pub struct Hub {
    agents: Mutex<HashMap<String, Connection>>,
    next: AtomicU64,
}

/// A host's socket, from `Hub::attach` until it is dropped.
pub struct Attached {
    hub: Arc<Hub>,
    host: String,
    id: u64,
    pub outgoing: mpsc::UnboundedReceiver<Envelope>,
    sessions: Arc<Mutex<HashMap<u64, mpsc::UnboundedSender<Frame>>>>,
}
impl Attached {
    /// Route an agent frame to the session waiting for it.
    pub fn deliver(&self, session: u64, frame: Frame) {
        let sessions = self.sessions.lock().unwrap();
        if let Some(sender) = sessions.get(&session) {
            let _ = sender.send(frame);
        }
    }
}
impl Drop for Attached {
    fn drop(&mut self) {
        let mut agents = self.hub.agents.lock().unwrap();
        // A newer socket for the same host stays.
        if agents.get(&self.host).is_some_and(|c| c.id == self.id) {
            agents.remove(&self.host);
        }
        // Waiting sessions see their channel close: the agent is gone.
        self.sessions.lock().unwrap().clear();
    }
}

impl Hub {
    /// Register the host's socket, replacing an older one.
    pub fn attach(self: &Arc<Self>, host: &str) -> Attached {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let (outgoing, outbox) = mpsc::unbounded_channel();
        let sessions = Arc::new(Mutex::new(HashMap::new()));
        let replaced = self.agents.lock().unwrap().insert(
            host.to_owned(),
            Connection {
                id,
                outgoing,
                sessions: sessions.clone(),
            },
        );
        if let Some(old) = replaced {
            old.sessions.lock().unwrap().clear();
        }
        Attached {
            hub: self.clone(),
            host: host.to_owned(),
            id,
            outgoing: outbox,
            sessions,
        }
    }
    pub fn connected(&self, host: &str) -> bool {
        self.agents.lock().unwrap().contains_key(host)
    }
    fn open(&self, host: &str, request: Request) -> Result<Session> {
        let agents = self.agents.lock().unwrap();
        let connection = agents
            .get(host)
            .ok_or_else(|| anyhow!("the agent of host '{host}' is not connected"))?;
        let session = self.next.fetch_add(1, Ordering::Relaxed);
        let (frames_tx, frames) = mpsc::unbounded_channel();
        connection
            .sessions
            .lock()
            .unwrap()
            .insert(session, frames_tx);
        connection
            .outgoing
            .send(Envelope::Open {
                session,
                request: Box::new(request),
            })
            .map_err(|_| anyhow!("the agent of host '{host}' disconnected"))?;
        Ok(Session {
            id: session,
            frames,
            outgoing: connection.outgoing.clone(),
            sessions: connection.sessions.clone(),
        })
    }
}

struct Session {
    id: u64,
    frames: mpsc::UnboundedReceiver<Frame>,
    outgoing: mpsc::UnboundedSender<Envelope>,
    sessions: Arc<Mutex<HashMap<u64, mpsc::UnboundedSender<Frame>>>>,
}
impl Session {
    async fn next(&mut self) -> Result<Frame> {
        self.frames
            .recv()
            .await
            .context("the host's agent disconnected during the request")
    }
}
impl Drop for Session {
    fn drop(&mut self) {
        self.sessions.lock().unwrap().remove(&self.id);
    }
}

/// A dial-in host's agent, reached through its open socket.
pub struct HubLink {
    pub hub: Arc<Hub>,
    pub host: String,
}
impl AgentLink for HubLink {
    fn call(&self, request: Request, sink: Option<EventSink>) -> BoxFuture<'_, Result<Reply>> {
        Box::pin(async move {
            let seconds = request.operation.seconds();
            let operation = request.operation.clone();
            let mut session = self.hub.open(&self.host, request)?;
            let execute = async {
                loop {
                    if let Some(outcome) =
                        deliver(session.next().await?, &operation, sink.as_ref())?
                    {
                        return outcome;
                    }
                }
            };
            tokio::time::timeout(Duration::from_secs(seconds), execute)
                .await
                .map_err(|_| anyhow!("agent operation timed out"))?
        })
    }
    fn terminal(&self, request: Request, io: TerminalIo) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            let mut session = self.hub.open(&self.host, request)?;
            let TerminalIo { mut input, output } = io;
            let mut open = true;
            loop {
                tokio::select! {
                    frame = session.next() => match frame? {
                        Frame::Attached => {}
                        Frame::Terminal { data } => {
                            let bytes = STANDARD
                                .decode(data)
                                .map_err(|_| anyhow!("invalid farm-agent terminal output"))?;
                            // A departed client also closes `input`, handled below.
                            let _ = output.send(bytes).await;
                        }
                        Frame::Result { result: Reply::Done } => return Ok(()),
                        Frame::Error { message } => bail!("{message}"),
                        _ => bail!("unexpected farm-agent frame in a shell session"),
                    },
                    message = input.recv(), if open => {
                        let id = session.id;
                        let envelope = match message {
                            Some(TerminalInput::Data(data)) => Envelope::Input {
                                session: id,
                                frame: TerminalFrame::Input { data: STANDARD.encode(data) },
                            },
                            Some(TerminalInput::Resize(size)) => Envelope::Input {
                                session: id,
                                frame: TerminalFrame::Resize { rows: size.rows, cols: size.cols },
                            },
                            Some(TerminalInput::Close) => Envelope::Input {
                                session: id,
                                frame: TerminalFrame::Eof,
                            },
                            None => {
                                open = false;
                                Envelope::Close { session: id }
                            }
                        };
                        session
                            .outgoing
                            .send(envelope)
                            .map_err(|_| anyhow!("the host's agent disconnected"))?;
                    }
                }
            }
        })
    }
}
