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
    pub rpc_stackboxes:
        Mutex<std::collections::BTreeMap<String, Vec<crate::stackbox::StackboxNode>>>,
    pub fail_scan: AtomicBool,
    /// Answer `Hello` like an agent from before template transfers.
    pub legacy_agent: AtomicBool,
    /// Snapshot IDs this host keeps a copy of, with their references.
    pub cached: Mutex<std::collections::BTreeMap<String, String>>,
    /// Public port bases that stackboxes of other coordinators hold on this host.
    pub reserved_ports: Mutex<Vec<u16>>,
    /// The stackbox kernel path this host's shell inspection would report.
    pub kernel: Mutex<Option<String>>,
    /// Why an installed kernel is not used, as the shell inspection would report it.
    pub kernel_reason: Mutex<Option<String>>,
    /// Answer purges without the count of other coordinators' VMs, like an agent that never reported it.
    pub purge_unreported: AtomicBool,
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
            kernel_upload: false,
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
    /// A stackbox kernel upload arrived: stdin carries the library, not a script.
    kernel_upload: bool,
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
                // EOF has nothing to echo; the fixture's shell keeps its session open.
                TerminalFrame::Eof => continue,
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
            kernel_upload: false,
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
        let text = String::from_utf8_lossy(command);
        assert!(text.starts_with("timeout ") || text.contains("/stackbox-agent"));
        // The kernel upload carries its script in the command and the library on stdin.
        self.kernel_upload = text.contains("libkrunfw") && text.contains("sha256sum -c");
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
        if self.kernel_upload {
            self.observed
                .calls
                .lock()
                .unwrap()
                .push("kernel-upload".into());
            *self.observed.kernel.lock().unwrap() = Some(
                "/home/operator/.local/share/stackbox/libkrunfw/0123456789abcdef/libkrunfw.so.5.6.1"
                    .into(),
            );
            session.exit_status_request(channel, 0)?;
            session.eof(channel)?;
            session.close(channel)?;
            return Ok(());
        }
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
                    supports_template_transfer: !self.observed.legacy_agent.load(Ordering::SeqCst),
                },
                Operation::FindSnapshot { snapshot_id } => Reply::Found {
                    reference: self.observed.cached.lock().unwrap().get(&snapshot_id).cloned(),
                    nodes: Vec::new(),
                },
                Operation::FetchArchive { url, .. } => {
                    self.observed
                        .calls
                        .lock()
                        .unwrap()
                        .push(format!("fetch:{}", url.split('?').next().unwrap_or_default()));
                    Reply::Done
                }
                Operation::ImportArchive { snapshot_id, .. } => {
                    let reference = format!("ssh/{snapshot_id}");
                    self.observed
                        .cached
                        .lock()
                        .unwrap()
                        .insert(snapshot_id, reference.clone());
                    Reply::Imported {
                        snapshot: crate::stackbox::ImportedSnapshot {
                            reference,
                            labels: Default::default(),
                            size_bytes: Some(4096),
                            nodes: Vec::new(),
                        },
                    }
                }
                Operation::PublicPorts => Reply::PublicPorts {
                    bases: self.observed.reserved_ports.lock().unwrap().clone(),
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
                        stackboxes: self.observed.rpc_stackboxes.lock().unwrap().clone(),
                    }
                }
                Operation::Create {
                    stackbox_id,
                    plan,
                    share,
                    source,
                    exposure,
                    ..
                } => {
                    if self.observed.reserved_ports.lock().unwrap().contains(&exposure.port_base) {
                        session.data(
                            channel,
                            format!(
                                "{}\n",
                                serde_json::to_string(&Frame::Error {
                                    message: "remote public ports are reserved by another stackbox".into()
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
                    if let Some(source) = &source {
                        self.observed
                            .calls
                            .lock()
                            .unwrap()
                            .push(format!("create-from:{source}"));
                    }
                    if let Some(share) = &share {
                        self.observed
                            .calls
                            .lock()
                            .unwrap()
                            .push(format!("share:{}", share.nodes.join(",")));
                    }
                    let nodes: Vec<_> = plan
                        .nodes
                        .into_iter()
                        .filter(|node| {
                            share.as_ref().is_none_or(|share| {
                                crate::stackbox::suffix(node.role, node.index)
                                    .is_some_and(|s| share.nodes.contains(&s))
                            })
                        })
                        .map(|node| crate::stackbox::StackboxNode {
                            index: node.index,
                            name: crate::stackbox::node_name(&stackbox_id, node.role, node.index),
                            role: Some(node.role),
                            status: "running".into(),
                            memory_mib: node.memory_mib,
                            ip: Some("10.250.0.1".into()),
                        })
                        .collect();
                    self.observed
                        .rpc_stackboxes
                        .lock()
                        .unwrap()
                        .insert(stackbox_id, nodes.clone());
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
                Operation::Delete { stackbox_id, .. } => {
                    self.observed.rpc_stackboxes.lock().unwrap().remove(&stackbox_id);
                    Reply::Done
                }
                Operation::Relink { stackbox_id, url, .. } => {
                    self.observed
                        .calls
                        .lock()
                        .unwrap()
                        .push(format!("relink:{stackbox_id}:{url}"));
                    Reply::Done
                }
                Operation::Prepare { stackbox_id, .. } => {
                    self.observed
                        .calls
                        .lock()
                        .unwrap()
                        .push(format!("prepare:{stackbox_id}"));
                    Reply::Done
                }
                Operation::Expand {
                    stackbox_id,
                    plan,
                    add,
                    source,
                    ..
                } => {
                    if let Some(source) = &source {
                        self.observed
                            .calls
                            .lock()
                            .unwrap()
                            .push(format!("expand-from:{source}"));
                    }
                    self.observed
                        .calls
                        .lock()
                        .unwrap()
                        .push(format!("expand:{stackbox_id}:{}", add.join(",")));
                    let nodes: Vec<_> = plan
                        .nodes
                        .iter()
                        .filter(|node| {
                            crate::stackbox::suffix(node.role, node.index)
                                .is_some_and(|suffix| add.iter().any(|add| add == &suffix))
                        })
                        .map(|node| crate::stackbox::StackboxNode {
                            index: node.index,
                            name: crate::stackbox::node_name(&stackbox_id, node.role, node.index),
                            role: Some(node.role),
                            status: "running".into(),
                            memory_mib: node.memory_mib,
                            ip: Some("10.250.0.1".into()),
                        })
                        .collect();
                    self.observed
                        .rpc_stackboxes
                        .lock()
                        .unwrap()
                        .entry(stackbox_id)
                        .or_default()
                        .extend(nodes.clone());
                    Reply::Nodes { nodes }
                }
                Operation::SshKeys { .. } => Reply::SshKeys {
                    public_key: "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIFixture".into(),
                    private_key: "-----BEGIN OPENSSH PRIVATE KEY-----\nfixture\n-----END OPENSSH PRIVATE KEY-----\n".into(),
                },
                Operation::DeployCompute { stackbox_id, index, .. } => {
                    self.observed
                        .calls
                        .lock()
                        .unwrap()
                        .push(format!("deploy-compute:{stackbox_id}:{index}"));
                    Reply::Done
                }
                // The fake host holds only this coordinator's VMs, so both scopes remove them all.
                Operation::Purge | Operation::PurgeAll | Operation::PurgeOwn => {
                    let removed = self.observed.rpc_stackboxes.lock().unwrap().len();
                    self.observed.rpc_stackboxes.lock().unwrap().clear();
                    // `purge_unreported` makes the fake answer the older shape, which carries no count.
                    if matches!(request.operation, Operation::Purge)
                        || self.observed.purge_unreported.load(Ordering::SeqCst)
                    {
                        Reply::Purged { stackboxes: removed }
                    } else {
                        Reply::ScopedPurged {
                            stackboxes: removed,
                            kept_for_others: 0,
                        }
                    }
                }
                Operation::RemoveGolden => {
                    self.observed
                        .calls
                        .lock()
                        .unwrap()
                        .push("remove-golden".into());
                    Reply::GoldenRemoved { groups: 1 }
                }
                Operation::Shell { .. }
                | Operation::VerifyVm
                | Operation::InstallMsb
                | Operation::Settle { .. }
                | Operation::ForgetComputes { .. }
                | Operation::AttachPci { .. }
                | Operation::ExportSnapshot { .. } => Reply::Done,
                Operation::RemoveSnapshot { reference } => {
                    self.observed
                        .calls
                        .lock()
                        .unwrap()
                        .push(format!("remove-snapshot:{reference}"));
                    self.observed
                        .cached
                        .lock()
                        .unwrap()
                        .retain(|_, cached| *cached != reference);
                    Reply::Done
                }
                Operation::ReceiveArchive { .. }
                | Operation::ExportArchive { .. }
                | Operation::SendArchive { .. } => {
                    unimplemented!("the SSH fixture does not take streamed archives")
                }
                Operation::Restore {
                    stackbox_id,
                    plan,
                    exposure,
                    ..
                } => {
                    if self.observed.reserved_ports.lock().unwrap().contains(&exposure.port_base) {
                        session.data(
                            channel,
                            format!(
                                "{}\n",
                                serde_json::to_string(&Frame::Error {
                                    message: "remote public ports are reserved by another stackbox".into()
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
                    let nodes: Vec<_> = plan
                        .nodes
                        .into_iter()
                        .map(|node| crate::stackbox::StackboxNode {
                            index: node.index,
                            name: crate::stackbox::node_name(&stackbox_id, node.role, node.index),
                            role: Some(node.role),
                            status: "running".into(),
                            memory_mib: node.memory_mib,
                            ip: Some("10.250.0.1".into()),
                        })
                        .collect();
                    self.observed
                        .rpc_stackboxes
                        .lock()
                        .unwrap()
                        .insert(stackbox_id, nodes.clone());
                    Reply::Nodes { nodes }
                }
                Operation::Freeze { .. }
                | Operation::Impair { .. }
                | Operation::ImportSnapshot { .. }
                | Operation::PciBind { .. }
                | Operation::PciUnbind { .. }
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
                        relay_exposure: None,
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
                "STACKBOX\tos\tLinux\nSTACKBOX\tarch\t{}\nSTACKBOX\tkvm\t1\nSTACKBOX\tsudo\t0\nSTACKBOX\tmemory_total\t32768\nSTACKBOX\tmemory_available\t24000\nSTACKBOX\tdisk_available\t200000\nSTACKBOX\tmsb\t{}\nSTACKBOX\tdocker\t1\nSTACKBOX\tgolden\t{}\nSTACKBOX\tlibkrunfw\t{}\nSTACKBOX\tlibkrunfw_note\t{}\n",
                std::env::consts::ARCH,
                self.observed
                    .msb
                    .lock()
                    .unwrap()
                    .as_deref()
                    .map_or(String::new(), |version| format!("msb {version}")),
                u8::from(self.observed.golden.load(Ordering::SeqCst)),
                self.observed
                    .kernel
                    .lock()
                    .unwrap()
                    .clone()
                    .unwrap_or_default(),
                self.observed
                    .kernel_reason
                    .lock()
                    .unwrap()
                    .clone()
                    .unwrap_or_default()
            )
        } else if script.contains("libkrunfw") && script.contains("sha256sum -c --status") {
            self.observed
                .calls
                .lock()
                .unwrap()
                .push("kernel-check".into());
            "missing".into()
        } else if script.contains("manifest.txt.tmp") {
            self.observed
                .calls
                .lock()
                .unwrap()
                .push("kernel-manifest".into());
            String::new()
        } else if script
            .contains("STACKBOX_MSB_RELEASE_BASE:-https://github.com/preinpost/microsandbox")
        {
            self.observed.calls.lock().unwrap().push("install".into());
            *self.observed.msb.lock().unwrap() = Some(MSB_VERSION.into());
            String::new()
        } else if script.contains("name=stackbox-check-") {
            self.observed.calls.lock().unwrap().push("smoke".into());
            if self.observed.fail_smoke.load(Ordering::SeqCst) {
                code = 1;
            }
            // Deliberately include credential-looking output; callers must never persist it.
            // The markers are what preparation reads: the kernel it installed came up with
            // VFIO and the mdev bus built in, and registers the sample mtty device.
            let mtty = u8::from(self.observed.kernel.lock().unwrap().is_some());
            let types = if mtty == 1 {
                "STACKBOX-GPU type mtty-1 24\n"
            } else {
                ""
            };
            format!(
                "fixture-password-only PRIVATE-KEY-DO-NOT-PERSIST\nSTACKBOX-GPU vfio {mtty}\nSTACKBOX-GPU mdev {mtty}\nSTACKBOX-GPU mtty {mtty}\nSTACKBOX-GPU release 6.12.113\n{types}STACKBOX-MDEV {mtty}\n"
            )
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
