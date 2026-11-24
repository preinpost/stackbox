//! How the coordinator reaches a host's stackbox-agent. Every host speaks the same protocol;
//! only the transport differs: SSH for a remote host, a child process for this machine.

use super::{
    Credentials, RemoteHost,
    agent::{Agent, Emitter},
    protocol::{AgentError, Frame, MAX_FRAME, Reply, Request, TerminalFrame, deliver},
    ssh::Session,
};
use crate::{
    kolla::EventSink,
    stackbox::{TerminalInput, TerminalIo},
};
use anyhow::{Context, Result, anyhow, bail, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use futures::future::BoxFuture;
use std::{path::PathBuf, sync::Arc, time::Duration};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

pub trait AgentLink: Send + Sync + 'static {
    /// One request and its result; progress goes to `sink`.
    fn call(&self, request: Request, sink: Option<EventSink>) -> BoxFuture<'_, Result<Reply>>;
    /// A shell session, relayed until either side ends it.
    fn terminal(&self, request: Request, io: TerminalIo) -> BoxFuture<'_, Result<()>>;
}

/// A remote host's agent, run over SSH from the artifact pinned by `digest`.
pub struct SshLink {
    pub host: RemoteHost,
    pub credentials: Credentials,
    pub digest: String,
}
impl AgentLink for SshLink {
    fn call(&self, request: Request, sink: Option<EventSink>) -> BoxFuture<'_, Result<Reply>> {
        Box::pin(async move {
            let session = Session::connect(&self.host, self.credentials.clone()).await?;
            session.rpc(&self.digest, &request, sink).await
        })
    }
    fn terminal(&self, request: Request, io: TerminalIo) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            let session = Session::connect(&self.host, self.credentials.clone()).await?;
            session.terminal(&self.digest, &request, io).await
        })
    }
}

/// This machine's agent: the coordinator's own binary, run as `stackbox agent` per request.
pub struct ProcessLink {
    pub program: PathBuf,
    /// Linux stackbox-agent for multi-node guests, passed as `STACKBOX_GUEST_AGENT` once known.
    pub guest_agent: Arc<std::sync::OnceLock<PathBuf>>,
}
impl ProcessLink {
    pub fn current() -> Result<Self> {
        Ok(Self {
            program: std::env::current_exe().context("cannot locate the stackbox binary")?,
            guest_agent: Default::default(),
        })
    }
    fn spawn(&self) -> Result<tokio::process::Child> {
        let mut command = tokio::process::Command::new(&self.program);
        if let Some(path) = self.guest_agent.get()
            && std::env::var_os("STACKBOX_GUEST_AGENT").is_none()
        {
            command.env("STACKBOX_GUEST_AGENT", path);
        }
        command
            .arg("agent")
            .env("PATH", super::host::search_path())
            .env("MSB_BACKEND", "local")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .context("cannot start the local stackbox agent")
    }
}
fn request_line(request: &Request) -> Result<Vec<u8>> {
    let mut line = serde_json::to_vec(request)?;
    line.push(b'\n');
    Ok(line)
}
async fn next_frame<R: tokio::io::AsyncBufRead + Unpin>(reader: &mut R) -> Result<Option<Frame>> {
    use tokio::io::AsyncReadExt;
    let mut line = Vec::new();
    if reader
        .take(MAX_FRAME as u64 + 1)
        .read_until(b'\n', &mut line)
        .await?
        == 0
    {
        return Ok(None);
    }
    ensure!(
        line.len() <= MAX_FRAME,
        "stackbox-agent frame exceeds limit"
    );
    Ok(Some(serde_json::from_slice(&line).map_err(|_| {
        anyhow!("invalid stackbox-agent protocol frame")
    })?))
}
impl AgentLink for ProcessLink {
    fn call(&self, request: Request, sink: Option<EventSink>) -> BoxFuture<'_, Result<Reply>> {
        Box::pin(async move {
            let seconds = request.operation.seconds();
            let mut child = self.spawn()?;
            let mut stdin = child.stdin.take().context("agent has no stdin")?;
            let mut stdout = BufReader::new(child.stdout.take().context("agent has no stdout")?);
            let execute = async {
                stdin.write_all(&request_line(&request)?).await?;
                drop(stdin);
                while let Some(frame) = next_frame(&mut stdout).await? {
                    if let Some(outcome) = deliver(frame, &request.operation, sink.as_ref())? {
                        return outcome;
                    }
                }
                bail!("the local stackbox agent exited without a result")
            };
            tokio::time::timeout(Duration::from_secs(seconds), execute)
                .await
                .map_err(|_| anyhow!("local agent operation timed out"))?
        })
    }
    fn terminal(&self, request: Request, io: TerminalIo) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            let mut child = self.spawn()?;
            let mut stdin = child.stdin.take().context("agent has no stdin")?;
            let mut stdout = BufReader::new(child.stdout.take().context("agent has no stdout")?);
            stdin.write_all(&request_line(&request)?).await?;
            let TerminalIo { mut input, output } = io;
            let mut stdin = Some(stdin);
            loop {
                tokio::select! {
                    frame = next_frame(&mut stdout) => match frame? {
                        Some(Frame::Attached) => {}
                        Some(Frame::Terminal { data }) => {
                            let bytes = STANDARD
                                .decode(data)
                                .map_err(|_| anyhow!("invalid stackbox-agent terminal output"))?;
                            // A departed client also closes `input`, handled below.
                            let _ = output.send(bytes).await;
                        }
                        Some(Frame::Result { result: Reply::Done }) => return Ok(()),
                        Some(Frame::Error { message }) => bail!("{message}"),
                        Some(_) => bail!("unexpected stackbox-agent frame in a shell session"),
                        None => bail!("the local stackbox agent ended the shell without a result"),
                    },
                    message = input.recv(), if stdin.is_some() => {
                        let Some(message) = message else {
                            // EOF makes the agent hang the shell up and report.
                            stdin = None;
                            continue;
                        };
                        let writer = stdin.as_mut().expect("guarded by the select condition");
                        writer.write_all(&terminal_line(message)?).await?;
                    }
                }
            }
        })
    }
}
fn terminal_line(message: TerminalInput) -> Result<Vec<u8>> {
    let frame = match message {
        TerminalInput::Data(data) => TerminalFrame::Input {
            data: STANDARD.encode(data),
        },
        TerminalInput::Resize(size) => TerminalFrame::Resize {
            rows: size.rows,
            cols: size.cols,
        },
        TerminalInput::Close => TerminalFrame::Eof,
    };
    let mut line = serde_json::to_vec(&frame)?;
    line.push(b'\n');
    Ok(line)
}

/// An agent inside the coordinator's process, so tests drive the real agent without VMs.
pub struct InProcessLink {
    pub agent: Arc<Agent>,
}
impl AgentLink for InProcessLink {
    fn call(&self, request: Request, sink: Option<EventSink>) -> BoxFuture<'_, Result<Reply>> {
        Box::pin(async move {
            let operation = request.operation.clone();
            let emit: Emitter = Arc::new(move |frame| {
                deliver(frame, &operation, sink.as_ref())?;
                Ok(())
            });
            if matches!(request.operation, super::protocol::Operation::Shell { .. }) {
                bail!("shell sessions need a terminal stream");
            }
            // Whatever the agent itself fails with is the operation's outcome, as over a wire.
            self.agent
                .execute(request, emit)
                .await
                .map_err(|error| AgentError(format!("{error:#}")).into())
        })
    }
    fn terminal(&self, request: Request, io: TerminalIo) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            let TerminalIo { mut input, output } = io;
            let (reader, mut writer) = tokio::io::duplex(64 * 1024);
            let forward_input = async move {
                while let Some(message) = input.recv().await {
                    if writer.write_all(&terminal_line(message)?).await.is_err() {
                        break;
                    }
                }
                Ok::<_, anyhow::Error>(())
            };
            // The emitter is synchronous; an unbounded hop lets it hand off output.
            let (bytes_tx, mut bytes_rx) = tokio::sync::mpsc::unbounded_channel();
            let emit: Emitter = Arc::new(move |frame| {
                if let Frame::Terminal { data } = frame {
                    let _ = bytes_tx.send(STANDARD.decode(data)?);
                }
                Ok(())
            });
            let forward_output = async move {
                while let Some(bytes) = bytes_rx.recv().await {
                    let _ = output.send(bytes).await;
                }
            };
            let session = self.agent.terminal(&request, BufReader::new(reader), emit);
            let pump = async { tokio::join!(session, forward_output).0 };
            tokio::pin!(pump, forward_input);
            // Closing the input drops the writer, so the agent sees EOF and hangs up.
            let result = tokio::select! {
                result = &mut pump => result,
                input = &mut forward_input => {
                    input?;
                    pump.await
                }
            };
            result.map(|_| ())
        })
    }
}
