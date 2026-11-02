//! Loopback SSH protocol fixture. It emulates host commands; it never runs installations.
use super::*;
use russh::{
    Channel, ChannelId,
    keys::{HashAlg, PrivateKey, PublicKey, ssh_key::private::Ed25519Keypair},
    server::{self, Server as _},
};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};

#[derive(Default)]
pub struct Observed {
    pub auth_attempts: AtomicUsize,
    /// The `msb --version` the host reports; None until something is installed.
    pub msb: Mutex<Option<String>>,
    pub golden: AtomicBool,
    pub fail_smoke: AtomicBool,
    pub calls: Mutex<Vec<String>>,
    pub rpc_farms: Mutex<std::collections::BTreeMap<String, Vec<crate::farm::FarmNode>>>,
    pub fail_scan: AtomicBool,
}
pub fn client_key() -> PrivateKey {
    PrivateKey::from(Ed25519Keypair::from_seed(&[19; 32]))
}
pub struct Fixture {
    pub port: u16,
    pub fingerprint: String,
    pub observed: Arc<Observed>,
    handle: server::RunningServerHandle,
    task: tokio::task::JoinHandle<()>,
}
impl Fixture {
    pub async fn start() -> Self {
        let key = PrivateKey::from(Ed25519Keypair::from_seed(&[9; 32]));
        let fingerprint = key.public_key().fingerprint(HashAlg::Sha256).to_string();
        let config = Arc::new(server::Config {
            keys: vec![key],
            auth_rejection_time: std::time::Duration::ZERO,
            auth_rejection_time_initial: Some(std::time::Duration::ZERO),
            ..Default::default()
        });
        let socket = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = socket.local_addr().unwrap().port();
        let observed = Arc::new(Observed::default());
        let mut handler = Handler {
            observed: observed.clone(),
            input: Vec::new(),
            shell: false,
        };
        let (send, receive) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            let running = handler.run_on_socket(config, &socket);
            let _ = send.send(running.handle());
            running.await.unwrap();
        });
        let handle = receive.await.unwrap();
        Self {
            port,
            fingerprint,
            observed,
            handle,
            task,
        }
    }
    pub fn registration(&self, id: &str) -> RegisterHost {
        RegisterHost {
            id: id.into(),
            address: "127.0.0.1".into(),
            port: self.port,
            username: "operator".into(),
            fingerprint: self.fingerprint.clone(),
            credentials: Credentials::Password {
                password: "fixture-password-only".into(),
            },
        }
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.handle.shutdown("test finished".into());
        self.task.abort();
    }
}
#[derive(Clone)]
struct Handler {
    observed: Arc<Observed>,
    input: Vec<u8>,
    /// A shell request arrived: stdin stays open and carries terminal frames.
    shell: bool,
}
impl Handler {
    fn frame(
        session: &mut server::Session,
        channel: ChannelId,
        frame: &super::protocol::Frame,
    ) -> Result<(), russh::Error> {
        session.data(
            channel,
            format!("{}\n", serde_json::to_string(frame).unwrap()).into_bytes(),
        )
    }
    /// Echo terminal input back and report resizes as `ROWSxCOLS`.
    fn relay_shell(
        &mut self,
        channel: ChannelId,
        session: &mut server::Session,
    ) -> Result<(), russh::Error> {
        use super::protocol::{Frame, Operation, Request, TerminalFrame};
        use base64::{Engine, engine::general_purpose::STANDARD};
        while let Some(end) = self.input.iter().position(|b| *b == b'\n') {
            if !self.shell {
                // Scripts have newlines too; leave anything but a shell request untouched.
                let Ok(Request {
                    operation: Operation::Shell { .. },
                    ..
                }) = serde_json::from_slice(&self.input[..end])
                else {
                    return Ok(());
                };
                self.input.drain(..=end);
                self.shell = true;
                self.observed.calls.lock().unwrap().push("rpc:shell".into());
                Self::frame(session, channel, &Frame::Attached)?;
                continue;
            }
            let line: Vec<u8> = self.input.drain(..=end).collect();
            let data = match serde_json::from_slice(&line).unwrap() {
                TerminalFrame::Input { data } => data,
                TerminalFrame::Resize { rows, cols } => STANDARD.encode(format!("{rows}x{cols}")),
            };
            Self::frame(session, channel, &Frame::Terminal { data })?;
        }
        Ok(())
    }
}
impl server::Server for Handler {
    type Handler = Self;
    fn new_client(&mut self, _: Option<std::net::SocketAddr>) -> Self {
        Self {
            observed: self.observed.clone(),
            input: Vec::new(),
            shell: false,
        }
    }
}
impl server::Handler for Handler {
    type Error = russh::Error;
    async fn auth_password(
        &mut self,
        user: &str,
        password: &str,
    ) -> Result<server::Auth, Self::Error> {
        self.observed.auth_attempts.fetch_add(1, Ordering::SeqCst);
        Ok(
            if user == "operator" && password == "fixture-password-only" {
                server::Auth::Accept
            } else {
                server::Auth::reject()
            },
        )
    }
    async fn auth_publickey(
        &mut self,
        user: &str,
        key: &PublicKey,
    ) -> Result<server::Auth, Self::Error> {
        self.observed.auth_attempts.fetch_add(1, Ordering::SeqCst);
        Ok(if user == "operator" && key == client_key().public_key() {
            server::Auth::Accept
        } else {
            server::Auth::reject()
        })
    }
    async fn channel_open_session(
        &mut self,
        _: Channel<server::Msg>,
        reply: server::ChannelOpenHandle,
        _: &mut server::Session,
    ) -> Result<(), Self::Error> {
        reply.accept().await;
        Ok(())
    }
    async fn exec_request(
        &mut self,
        channel: ChannelId,
        command: &[u8],
        session: &mut server::Session,
    ) -> Result<(), Self::Error> {
        assert!(
            String::from_utf8_lossy(command).starts_with("timeout ")
                || String::from_utf8_lossy(command).contains("/farm-agent")
        );
        self.input.clear();
        self.shell = false;
        session.channel_success(channel)?;
        Ok(())
    }
    async fn data(
        &mut self,
        channel: ChannelId,
        data: &[u8],
        session: &mut server::Session,
    ) -> Result<(), Self::Error> {
        self.input.extend_from_slice(data);
        // One-shot requests have no newline; only a shell request is line-delimited.
        self.relay_shell(channel, session)
    }
    async fn channel_eof(
        &mut self,
        channel: ChannelId,
        session: &mut server::Session,
    ) -> Result<(), Self::Error> {
        if self.shell {
            use super::protocol::{Frame, Reply};
            Self::frame(
                session,
                channel,
                &Frame::Result {
                    result: Reply::Done,
                },
            )?;
            session.exit_status_request(channel, 0)?;
            session.eof(channel)?;
            session.close(channel)?;
            return Ok(());
        }
        if let Ok(request) = serde_json::from_slice::<super::protocol::Request>(&self.input) {
            use super::protocol::{Frame, Operation, Reply, VERSION};
            self.observed.calls.lock().unwrap().push(format!(
                "rpc:{:?}",
                std::mem::discriminant(&request.operation)
            ));
            let mut frames = Vec::new();
            let reply = match request.operation {
                Operation::Hello => Reply::Hello {
                    version: VERSION,
                    arch: std::env::consts::ARCH.into(),
                    supports_single_vm: true,
                    build_id: crate::BUILD_ID.into(),
                },
                Operation::Scan => {
                    if self.observed.fail_scan.load(Ordering::SeqCst) {
                        session.data(
                            channel,
                            format!(
                                "{}\n",
                                serde_json::to_string(&Frame::Error {
                                    message: "fixture scan unavailable".into()
                                })
                                .unwrap()
                            )
                            .into_bytes(),
                        )?;
                        session.exit_status_request(channel, 1)?;
                        session.eof(channel)?;
                        session.close(channel)?;
                        return Ok(());
                    }
                    Reply::Inventory {
                        farms: self.observed.rpc_farms.lock().unwrap().clone(),
                    }
                }
                Operation::Create { farm_id, plan, .. } => {
                    let nodes: Vec<_> = plan
                        .nodes
                        .into_iter()
                        .map(|node| crate::farm::FarmNode {
                            name: format!("{farm_id}-{}", node.role),
                            role: Some(node.role),
                            status: "running".into(),
                            memory_mib: node.memory_mib,
                            ip: Some("10.250.0.1".into()),
                        })
                        .collect();
                    self.observed
                        .rpc_farms
                        .lock()
                        .unwrap()
                        .insert(farm_id, nodes.clone());
                    Reply::Nodes { nodes }
                }
                Operation::Deploy { from, .. } => {
                    frames.push(Frame::Phase { phase: from });
                    frames.push(Frame::Output {
                        message: "remote deploy streamed output\n".into(),
                    });
                    Reply::Done
                }
                Operation::Attach { job_id, offset } => {
                    self.observed
                        .calls
                        .lock()
                        .unwrap()
                        .push(format!("attach:{job_id}:{offset}"));
                    frames.push(Frame::Output {
                        message: "remote deploy re-attached output\n".into(),
                    });
                    Reply::Done
                }
                Operation::Delete { farm_id, .. } => {
                    self.observed.rpc_farms.lock().unwrap().remove(&farm_id);
                    Reply::Done
                }
                Operation::Purge => {
                    let removed = self.observed.rpc_farms.lock().unwrap().len();
                    self.observed.rpc_farms.lock().unwrap().clear();
                    Reply::Purged { farms: removed }
                }
                Operation::Shell { .. }
                | Operation::VerifyVm
                | Operation::InstallMsb
                | Operation::Settle { .. }
                | Operation::ExportSnapshot { .. }
                | Operation::RemoveSnapshot { .. } => Reply::Done,
                Operation::Freeze { .. }
                | Operation::Restore { .. }
                | Operation::ImportSnapshot { .. }
                | Operation::Inspect => {
                    unimplemented!("the SSH fixture does not emulate snapshots")
                }
                Operation::Endpoints { .. } => Reply::Endpoints {
                    endpoints: crate::model::Endpoints {
                        services: std::collections::BTreeMap::new(),
                        credentials: std::collections::BTreeMap::from([(
                            "OS_PASSWORD".into(),
                            "remote-guest-secret".into(),
                        )]),
                        datastores: Vec::new(),
                    },
                },
            };
            frames.push(Frame::Result { result: reply });
            for frame in frames {
                session.data(
                    channel,
                    format!("{}\n", serde_json::to_string(&frame).unwrap()).into_bytes(),
                )?;
            }
            session.exit_status_request(channel, 0)?;
            session.eof(channel)?;
            session.close(channel)?;
            return Ok(());
        }
        let script = String::from_utf8_lossy(&self.input);
        let mut code = 0;
        let output = if script.contains("field()") {
            self.observed.calls.lock().unwrap().push("inspect".into());
            format!(
                "FARM\tos\tLinux\nFARM\tarch\t{}\nFARM\tkvm\t1\nFARM\tsudo\t0\nFARM\tmemory_total\t32768\nFARM\tmemory_available\t24000\nFARM\tdisk_available\t200000\nFARM\tmsb\t{}\nFARM\tdocker\t1\nFARM\tgolden\t{}\n",
                std::env::consts::ARCH,
                self.observed
                    .msb
                    .lock()
                    .unwrap()
                    .as_deref()
                    .map_or(String::new(), |version| format!("msb {version}")),
                u8::from(self.observed.golden.load(Ordering::SeqCst))
            )
        } else if script.contains("base=https://github.com") {
            self.observed.calls.lock().unwrap().push("install".into());
            *self.observed.msb.lock().unwrap() = Some(MSB_VERSION.into());
            String::new()
        } else if script.contains("docker build") {
            self.observed.calls.lock().unwrap().push("golden".into());
            self.observed.golden.store(true, Ordering::SeqCst);
            String::new()
        } else if script.contains("name=farm-check-") {
            self.observed.calls.lock().unwrap().push("smoke".into());
            if self.observed.fail_smoke.load(Ordering::SeqCst) {
                code = 1;
            }
            // Deliberately include credential-looking output; callers must never persist it.
            "fixture-password-only PRIVATE-KEY-DO-NOT-PERSIST".into()
        } else {
            code = 1;
            "unsupported fixture command".into()
        };
        session.data(channel, output.into_bytes())?;
        session.exit_status_request(channel, code)?;
        session.eof(channel)?;
        session.close(channel)?;
        Ok(())
    }
}
