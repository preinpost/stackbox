use super::{Credentials, GpuKernel, HostInspection, HostKey, RemoteHost, SshTarget};
use anyhow::{Context, Result, anyhow, bail, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use russh::{
    ChannelMsg, client,
    keys::{HashAlg, PrivateKeyWithHashAlg, PublicKeyOrCertificate},
};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
    time::Duration,
};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_OUTPUT: usize = 128 * 1024;
pub const COMMON: &str = "set -eu\nexport PATH=\"$HOME/.microsandbox/bin:$HOME/.local/bin:$PATH\"\nexport MSB_BACKEND=local\n";

fn upload_command(digest: &str, dir: &str, name: &str, seconds: u64, mode: &str) -> String {
    let script = format!(
        r#"set -eu
umask 077
dir="$HOME/{dir}"
mkdir -p "$dir"
tmp=$(mktemp "$dir/upload.XXXXXX")
trap 'rm -f "$tmp"' EXIT HUP INT TERM
cat > "$tmp"
printf '%s  %s\n' '{digest}' "$tmp" | sha256sum -c - > /dev/null
chmod {mode} "$tmp"
mv -f "$tmp" "$dir/{name}"
"#
    );
    format!(
        "timeout {seconds} bash -c '{}'",
        script.replace('\'', "'\\''")
    )
}

struct Verify {
    expected: Option<String>,
    observed: Arc<Mutex<Option<HostKey>>>,
}
impl client::Handler for Verify {
    type Error = russh::Error;
    async fn check_server_key(
        &mut self,
        server: &PublicKeyOrCertificate,
    ) -> Result<bool, Self::Error> {
        let key = server.public_key();
        let fingerprint = key.fingerprint(HashAlg::Sha256).to_string();
        *self.observed.lock().unwrap() = Some(HostKey {
            fingerprint: fingerprint.clone(),
            algorithm: key.algorithm().to_string(),
        });
        // Discovery deliberately rejects the connection before authentication.
        Ok(self.expected.as_ref() == Some(&fingerprint) && server.certificate().is_none())
    }
}
fn config() -> Arc<client::Config> {
    Arc::new(client::Config {
        inactivity_timeout: Some(Duration::from_secs(1500)),
        // An idle shell sends nothing; keepalives hold the connection and detect a dead host.
        keepalive_interval: Some(Duration::from_secs(60)),
        ..Default::default()
    })
}
pub async fn fingerprint(target: &SshTarget) -> Result<HostKey> {
    target.validate()?;
    let observed = Arc::new(Mutex::new(None));
    let handler = Verify {
        expected: None,
        observed: observed.clone(),
    };
    let connected = tokio::time::timeout(
        CONNECT_TIMEOUT,
        client::connect(config(), (target.address.as_str(), target.port), handler),
    )
    .await;
    let key = observed.lock().unwrap().take();
    key.ok_or_else(|| {
        anyhow!(
            "{}; check the address, port and firewall",
            handshake_failure(connected.map(|r| r.map(drop)))
        )
    })
}

/// Names why a handshake failed without echoing server-supplied text such as algorithm lists.
fn handshake_failure(
    connected: Result<Result<(), russh::Error>, tokio::time::error::Elapsed>,
) -> String {
    match connected {
        Err(_) => format!(
            "SSH handshake timed out after {}s",
            CONNECT_TIMEOUT.as_secs()
        ),
        Ok(Ok(())) => "SSH handshake failed".into(),
        Ok(Err(russh::Error::NoCommonAlgo { kind, .. })) => {
            format!("SSH handshake failed: no common {kind:?} algorithm")
        }
        Ok(Err(error)) => format!("SSH handshake failed: {error}"),
    }
}

pub struct Session {
    handle: client::Handle<Verify>,
}
impl Session {
    /// Write `bytes` to the remote path the command prepares, failing on a checksum
    /// mismatch; stdin carries the bytes, so nothing is echoed or logged.
    async fn upload(&self, command: String, bytes: &[u8], timeout: Duration) -> Result<()> {
        let upload = async {
            let channel = self.handle.channel_open_session().await?;
            channel.exec(true, command).await?;
            let (mut reader, writer) = channel.split();
            let send = async {
                writer.data(bytes).await?;
                writer.eof().await?;
                Ok::<_, anyhow::Error>(())
            };
            let receive = async {
                let mut code = None;
                while let Some(event) = reader.wait().await {
                    if let ChannelMsg::ExitStatus { exit_status } = event {
                        code = Some(exit_status);
                    }
                }
                ensure!(
                    code == Some(0),
                    "remote upload checksum verification failed"
                );
                Ok::<_, anyhow::Error>(())
            };
            tokio::try_join!(send, receive)?;
            Ok::<_, anyhow::Error>(())
        };
        tokio::time::timeout(timeout, upload)
            .await
            .map_err(|_| anyhow!("remote upload timed out"))?
            .map_err(|_| anyhow!("remote upload / checksum verification failed"))?;
        Ok(())
    }

    pub async fn install_agent(
        &self,
        artifact: &super::artifact::Artifact,
        coordinator_id: &str,
    ) -> Result<super::InstalledAgent> {
        let digest = &artifact.info.sha256;
        super::artifact::validate_digest(digest)?;
        let script = format!(
            "{COMMON}path=\"$HOME/.local/share/farm/agents/{digest}/farm-agent\"\nif [ -f \"$path\" ] && printf '%s  %s\\n' '{digest}' \"$path\" | sha256sum -c --status; then printf cached; else printf missing; fi\n"
        );
        if self.run(&script, 15, "Agent checksum check").await?.trim() != "cached" {
            let command = upload_command(
                digest,
                &format!(".local/share/farm/agents/{digest}"),
                "farm-agent",
                180,
                "700",
            );
            self.upload(command, &artifact.bytes[..], Duration::from_secs(190))
                .await?;
        }
        let request = super::protocol::Request {
            version: super::protocol::VERSION,
            coordinator_id: coordinator_id.into(),
            operation: super::protocol::Operation::Hello,
        };
        match self.rpc(digest, &request, None).await? {
            super::protocol::Reply::Hello {
                version,
                arch,
                supports_single_vm,
                build_id,
                ..
            } if version == artifact.info.protocol
                && arch == artifact.info.arch
                && supports_single_vm
                && build_id == artifact.info.build_id =>
            {
                Ok(super::InstalledAgent {
                    artifact: artifact.info.clone(),
                    verified_at: crate::model::now(),
                })
            }
            _ => bail!(
                "the installed farm-agent does not match this artifact (protocol, architecture, build {b}); rebuild with `task agent-build` and install it again",
                b = artifact.info.build_id
            ),
        }
    }

    pub async fn rpc(
        &self,
        digest: &str,
        request: &super::protocol::Request,
        sink: Option<crate::kolla::EventSink>,
    ) -> Result<super::protocol::Reply> {
        use super::protocol::deliver;
        super::artifact::validate_digest(digest)?;
        let seconds = request.operation.seconds();
        let command = agent_command(digest, seconds);
        let payload = serde_json::to_vec(request)?;
        let execute = async {
            let mut channel = self
                .handle
                .channel_open_session()
                .await
                .map_err(|_| anyhow!("SSH agent channel failed"))?;
            channel
                .exec(true, command)
                .await
                .map_err(|_| anyhow!("SSH agent execution failed"))?;
            channel
                .data(&payload[..])
                .await
                .map_err(|_| anyhow!("SSH agent request failed"))?;
            channel
                .eof()
                .await
                .map_err(|_| anyhow!("SSH agent request EOF failed"))?;
            let mut buffer = Vec::new();
            let mut terminal = None;
            let mut code = None;
            while let Some(event) = channel.wait().await {
                match event {
                    ChannelMsg::Data { data } => {
                        buffer.extend_from_slice(&data);
                        for frame in drain_frames(&mut buffer)? {
                            ensure!(
                                terminal.is_none(),
                                "farm-agent sent data after a terminal result"
                            );
                            terminal = deliver(frame, &request.operation, sink.as_ref())?;
                        }
                    }
                    ChannelMsg::ExitStatus { exit_status } => code = Some(exit_status),
                    _ => {}
                }
            }
            ensure!(buffer.is_empty(), "truncated farm-agent frame");
            let result = terminal.context(
                "farm-agent disconnected without a result; inspect the remote job before retrying",
            )??;
            ensure!(
                code == Some(0),
                "farm-agent exited unsuccessfully; inspect the remote host"
            );
            Ok::<_, anyhow::Error>(result)
        };
        tokio::time::timeout(Duration::from_secs(seconds + 20), execute)
            .await
            .map_err(|_| {
                anyhow!("remote operation timed out; inspect the remote host before retrying")
            })?
    }
    /// Relay a shell in a remote farm's VM through its farm-agent until either side ends it.
    pub async fn terminal(
        &self,
        digest: &str,
        request: &super::protocol::Request,
        io: crate::farm::TerminalIo,
    ) -> Result<()> {
        use super::protocol::{Frame, Reply, TerminalFrame};
        use crate::farm::TerminalInput;
        super::artifact::validate_digest(digest)?;
        let channel = self
            .handle
            .channel_open_session()
            .await
            .map_err(|_| anyhow!("SSH agent channel failed"))?;
        channel
            .exec(true, agent_command(digest, request.operation.seconds()))
            .await
            .map_err(|_| anyhow!("SSH agent execution failed"))?;
        let (mut reader, writer) = channel.split();
        let writer = &writer;
        let send = |line: Vec<u8>| async move {
            writer
                .data(&line[..])
                .await
                .map_err(|_| anyhow!("SSH agent input failed"))
        };
        let mut line = serde_json::to_vec(request)?;
        line.push(b'\n');
        send(line).await?;
        let crate::farm::TerminalIo { mut input, output } = io;
        // An agent from before shell sessions waits for EOF instead of answering.
        let attach = tokio::time::sleep(TERMINAL_ATTACH_TIMEOUT);
        tokio::pin!(attach);
        let mut attached = false;
        let mut buffer = Vec::new();
        let mut client_open = true;
        loop {
            tokio::select! {
                _ = &mut attach, if !attached => bail!(
                    "the farm-agent on this host has no shell sessions; install the agent again"
                ),
                // Once the client leaves, the agent hangs the shell up and reports; don't wait forever.
                _ = tokio::time::sleep(TERMINAL_CLOSE_TIMEOUT), if !client_open => return Ok(()),
                message = reader.wait() => match message {
                    Some(ChannelMsg::Data { data }) => {
                        buffer.extend_from_slice(&data);
                        for frame in drain_frames(&mut buffer)? {
                            match frame {
                                Frame::Attached => attached = true,
                                Frame::Terminal { data } if attached => {
                                    let bytes = STANDARD
                                        .decode(data)
                                        .map_err(|_| anyhow!("invalid farm-agent terminal output"))?;
                                    // A departed client also closes `input`, handled below.
                                    let _ = output.send(bytes).await;
                                }
                                Frame::Result { result: Reply::Done } => return Ok(()),
                                Frame::Error { message } => bail!("{message}"),
                                _ => bail!("unexpected farm-agent frame in a shell session"),
                            }
                        }
                    }
                    Some(_) => {}
                    None => bail!("farm-agent shell session closed without a result"),
                },
                message = input.recv(), if client_open => {
                    let frame = match message {
                        Some(TerminalInput::Data(data)) => TerminalFrame::Input {
                            data: STANDARD.encode(data),
                        },
                        Some(TerminalInput::Resize(size)) => TerminalFrame::Resize {
                            rows: size.rows,
                            cols: size.cols,
                        },
                        Some(TerminalInput::Close) => TerminalFrame::Eof,
                        None => {
                            client_open = false;
                            writer.eof().await.ok();
                            continue;
                        }
                    };
                    let mut line = serde_json::to_vec(&frame)?;
                    line.push(b'\n');
                    send(line).await?;
                }
            }
        }
    }
    pub async fn test_connection(request: &super::ConnectionTestRequest) -> Result<String> {
        let host = RemoteHost {
            id: "probe".into(),
            kind: super::HostKind::Ssh,
            address: request.address.clone(),
            port: request.port,
            username: request.username.clone(),
            fingerprint: request.fingerprint.clone(),
            auth_method: Some(request.credentials.method()),
            status: super::HostStatus::Registered,
            inspection: None,
            agent: None,
            capacity: None,
            active_job: None,
            error: None,
            connected: false,
            created_at: 0,
            updated_at: 0,
        };
        let session = Self::connect(&host, request.credentials.clone()).await?;
        let identity = session.run("uname -srm\n", 10, "connection test").await?;
        let identity = identity.trim();
        ensure!(
            !identity.is_empty()
                && identity.len() <= 200
                && identity.chars().all(|c| c.is_ascii() && !c.is_control()),
            "unexpected connection test output"
        );
        Ok(identity.to_owned())
    }
    pub async fn connect(host: &RemoteHost, credentials: Credentials) -> Result<Self> {
        let observed = Arc::new(Mutex::new(None));
        let handler = Verify {
            expected: Some(host.fingerprint.clone()),
            observed: observed.clone(),
        };
        let connected = tokio::time::timeout(
            CONNECT_TIMEOUT,
            client::connect(config(), (host.address.as_str(), host.port), handler),
        )
        .await;
        let mut handle = match connected {
            Ok(Ok(handle)) => handle,
            failed => {
                if observed
                    .lock()
                    .unwrap()
                    .as_ref()
                    .is_some_and(|key| key.fingerprint != host.fingerprint)
                {
                    bail!(
                        "SSH host key changed; verify the server identity before registering it again"
                    );
                }
                bail!(
                    "{}; check the server identity, address, port and firewall",
                    handshake_failure(failed.map(|r| r.map(drop)))
                );
            }
        };
        let authenticate = async {
            let result = match &credentials {
                Credentials::Password { password } => {
                    handle
                        .authenticate_password(&host.username, password.as_str())
                        .await
                }
                Credentials::PrivateKey { .. } => {
                    // Encrypted key derivation must not block the async executor.
                    let key = tokio::task::spawn_blocking(move || match &credentials {
                        Credentials::PrivateKey {
                            private_key,
                            passphrase,
                        } => russh::keys::decode_secret_key(private_key, passphrase.as_deref())
                            .map_err(|_| anyhow!("invalid private key or passphrase")),
                        _ => unreachable!(),
                    })
                    .await
                    .map_err(|_| anyhow!("key decoding failed"))??;
                    let hash = handle
                        .best_supported_rsa_hash()
                        .await
                        .map_err(|_| anyhow!("SSH signature negotiation failed"))?
                        .flatten();
                    handle
                        .authenticate_publickey(
                            &host.username,
                            PrivateKeyWithHashAlg::new(Arc::new(key), hash),
                        )
                        .await
                }
            }
            .map_err(|_| anyhow!("SSH authentication failed; check the username and credential"))?;
            ensure!(
                result.success(),
                "SSH authentication rejected; check the username and credential"
            );
            Ok::<_, anyhow::Error>(())
        };
        tokio::time::timeout(CONNECT_TIMEOUT, authenticate)
            .await
            .map_err(|_| anyhow!("SSH authentication timed out"))??;
        Ok(Self { handle })
    }

    /// Only fixed, project-owned scripts. Remote stdout/stderr never enters persisted logs.
    pub async fn run(&self, script: &str, seconds: u64, stage: &str) -> Result<String> {
        let execute = async {
            let mut channel = self.handle.channel_open_session().await?;
            // timeout is a remote bound as well, including after coordinator disconnection.
            channel
                .exec(true, format!("timeout {seconds} bash -s"))
                .await?;
            channel.data(script.as_bytes()).await?;
            channel.eof().await?;
            let mut output = Vec::new();
            let mut code = None;
            while let Some(message) = channel.wait().await {
                match message {
                    ChannelMsg::Data { data } => {
                        ensure!(
                            output.len() + data.len() <= MAX_OUTPUT,
                            "output limit exceeded"
                        );
                        output.extend_from_slice(&data);
                    }
                    ChannelMsg::ExitStatus { exit_status } => code = Some(exit_status),
                    _ => {}
                }
            }
            ensure!(code == Some(0), "remote command failed");
            Ok::<_, anyhow::Error>(String::from_utf8_lossy(&output).into_owned())
        };
        match tokio::time::timeout(Duration::from_secs(seconds + 10), execute).await {
            Ok(Ok(output)) => Ok(output),
            _ => bail!("{stage} failed or timed out; check remote prerequisites and retry"),
        }
    }
    pub async fn inspect(&self) -> Result<HostInspection> {
        parse_inspection(&self.run(&inspect_script(), 45, "Host inspection").await?)
    }
    /// Install this build's farm kernel on the host, under `~/.local/share/farm/libkrunfw`.
    /// The library is verified before it is published and the manifest goes last, so an
    /// interrupted upload leaves the stock kernel in place rather than a half kernel.
    pub async fn install_kernel(&self, artifact: &crate::kernel::Artifact) -> Result<()> {
        crate::kernel::validate_id(&artifact.id)?;
        crate::kernel::validate_filename(&artifact.filename)?;
        super::artifact::validate_digest(&artifact.sha256)?;
        let dir = format!(".local/share/farm/libkrunfw/{}", artifact.id);
        let script = format!(
            "{COMMON}path=\"$HOME/{dir}/{name}\"\nif [ -f \"$path\" ] && printf '%s  %s\\n' '{sha256}' \"$path\" | sha256sum -c --status; then printf cached; else printf missing; fi\n",
            name = artifact.filename,
            sha256 = artifact.sha256
        );
        if self
            .run(&script, 15, "Farm kernel checksum check")
            .await?
            .trim()
            != "cached"
        {
            let command = upload_command(&artifact.sha256, &dir, &artifact.filename, 300, "644");
            self.upload(
                command,
                &std::fs::read(&artifact.library)?,
                Duration::from_secs(310),
            )
            .await?;
        }
        use base64::{Engine, engine::general_purpose::STANDARD};
        let manifest = STANDARD.encode(artifact.manifest.as_bytes());
        let script = format!(
            "set -eu\numask 022\ndir=\"$HOME/{dir}\"\nmkdir -p \"$dir\"\nprintf '%s' '{manifest}' | base64 -d > \"$dir/manifest.txt.tmp\"\nmv -f \"$dir/manifest.txt.tmp\" \"$dir/manifest.txt\"\n"
        );
        self.run(&script, 30, "Farm kernel manifest").await?;
        Ok(())
    }
    pub async fn install(&self) -> Result<()> {
        self.run(
            &install_script(),
            600,
            &format!(
                "Pinned msb {} installation (curl, tar, sha256sum and flock required)",
                super::MSB_VERSION
            ),
        )
        .await?;
        Ok(())
    }
    /// Boot the verification VM and read what its kernel reports about the GPU stack.
    /// `reason` explains a stock boot when a kernel was installed but not used.
    pub async fn smoke(
        &self,
        token: &str,
        kernel: Option<&str>,
        reason: Option<&str>,
    ) -> Result<GpuKernel> {
        // Token comes exclusively from a locally generated UUID, never an API field.
        let output = self
            .run(
                &smoke_script(token, kernel)?,
                180,
                "VM boot / exec / cleanup verification",
            )
            .await?;
        let gpu = parse_gpu_kernel(&output)?;
        // Both markers describe the same kernel; a disagreement means the probe ran wrong.
        ensure!(
            parse_mdev(&output)? == gpu.mtty_available(),
            "the verification VM's mtty markers disagree: {}",
            output.trim()
        );
        Ok(gpu.for_kernel(
            kernel.is_some(),
            kernel.and_then(|path| crate::kernel::installed_id(std::path::Path::new(path))),
            if kernel.is_some() {
                None
            } else {
                reason.map(str::to_owned)
            },
        ))
    }
}

/// Install msb and libkrunfw at [`super::MSB_VERSION`], or upgrade an older msb to it.
pub(crate) fn install_script() -> String {
    include_str!("install.sh").replace("@MSB_VERSION@", super::MSB_VERSION)
}

/// Report the host's facts; `golden` says whether this build's golden image is built yet.
fn inspect_script() -> String {
    include_str!("inspect.sh")
        .replace("@GOLDEN@", &crate::farm::golden::current())
        .replace("@MSB_VERSION@", super::MSB_VERSION)
}

/// Quote one shell word, so a path from an inspection reaches the script as data.
fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

/// The guest-side probe: one process reads the kernel's GPU stack. farm's kernel carries
/// VFIO, mdev and the `mtty` driver built in, so an absent path means the feature did not
/// come up, not that a module is unloaded. The last line is the marker earlier coordinators
/// read. No single quotes: this runs inside a single-quoted `sh -c` word.
const GPU_PROBE: &str = "vfio=0; [ -e /dev/vfio/vfio ] && vfio=1; echo \"FARM-GPU vfio $vfio\"; \
bus=0; [ -d /sys/class/mdev_bus ] && bus=1; echo \"FARM-GPU mdev $bus\"; \
mtty=0; [ -d /sys/class/mdev_bus/mtty ] && mtty=1; echo \"FARM-GPU mtty $mtty\"; \
printf \"FARM-GPU release %s\\n\" \"$(uname -r 2>/dev/null || true)\"; \
for t in /sys/class/mdev_bus/*/mdev_supported_types/*; do \
  [ -d \"$t\" ] || continue; \
  n=${t##*/}; c=0; \
  [ -r \"$t/available_instances\" ] && c=$(cat \"$t/available_instances\"); \
  printf \"FARM-GPU type %s %s\\n\" \"$n\" \"$c\"; \
done; \
if [ -e /sys/class/mdev_bus/mtty/mdev_supported_types/mtty-1 ]; then echo FARM-MDEV 1; else echo FARM-MDEV 0; fi; \
exit 0";

/// Boot a small VM from the base image the golden image is built from, run a command in it
/// and remove it. `kernel` is the host's installed farm kernel: without it the CLI boots
/// the stock microsandbox kernel, and the boot cannot report an mtty device.
pub(crate) fn smoke_script(token: &str, kernel: Option<&str>) -> Result<String> {
    // Token comes exclusively from a locally generated UUID, never an API field.
    ensure!(uuid::Uuid::parse_str(token).is_ok(), "invalid smoke token");
    let kernel = kernel
        .map(|path| {
            format!(
                "export MSB_PATH=\"$HOME/.microsandbox/bin/msb\"\nexport MSB_LIBKRUNFW_PATH={}\n",
                shell_quote(path)
            )
        })
        .unwrap_or_default();
    // Every msb command reads stdin from /dev/null. `bash -s` feeds this script on
    // stdin over SSH, so an msb that attached to the terminal would swallow the commands
    // after it; that once hid the mdev probe and the cleanup.
    Ok(format!(
        "{COMMON}\n{kernel}name=farm-check-{token}\ntrap 'msb remove --force \"$name\" < /dev/null >/dev/null 2>&1' EXIT\nmsb create --name \"$name\" --cpus 1 --memory 512M --root-disk 2G --max-duration 120s --no-net {image} < /dev/null\nmsb exec \"$name\" -- /bin/true < /dev/null\n# The farm kernel exposes the kernel's sample mtty mediated device; the stock one has none.\nmsb exec \"$name\" -- /bin/sh -c '{GPU_PROBE}' < /dev/null\nmsb remove --force \"$name\" < /dev/null\ntrap - EXIT\n",
        image = crate::farm::golden::DEFAULT_OS.base_image
    ))
}

/// What the verification VM reported about the GPU stack; [`GpuKernel::for_kernel`] names
/// the kernel afterwards. A probe that printed nothing is an error, not a stock kernel: a
/// boot whose kernel has no mtty device still prints its zero markers.
pub(crate) fn parse_gpu_kernel(output: &str) -> Result<GpuKernel> {
    let flag = |value: Option<&str>, what: &str| -> Result<bool> {
        match value {
            Some("1") => Ok(true),
            Some("0") => Ok(false),
            other => anyhow::bail!("invalid GPU probe value for {what}: {other:?}"),
        }
    };
    let mut gpu = GpuKernel::default();
    let mut probed = false;
    for line in output.lines() {
        let Some(fields) = line.strip_prefix("FARM-GPU ") else {
            continue;
        };
        let mut parts = fields.split_whitespace();
        match parts.next() {
            Some("vfio") => {
                gpu.vfio = flag(parts.next(), "vfio")?;
                probed = true;
            }
            Some("mdev") => gpu.vfio_mdev = flag(parts.next(), "mdev")?,
            Some("mtty") => gpu.mtty_registered = flag(parts.next(), "mtty")?,
            Some("release") => gpu.kernel_release = parts.next().unwrap_or_default().to_string(),
            Some("type") => {
                let name = parts
                    .next()
                    .context("a GPU probe type line without a name")?;
                let available_instances = parts
                    .next()
                    .unwrap_or("0")
                    .parse()
                    .context("a GPU probe type line with an invalid instance count")?;
                gpu.mdev_types.push(super::MdevType {
                    name: name.to_string(),
                    available_instances,
                });
            }
            other => anyhow::bail!("unknown GPU probe line for {other:?}: {line}"),
        }
    }
    ensure!(
        probed,
        "the verification VM booted but its GPU probe did not run: {}",
        output.trim()
    );
    Ok(gpu)
}

/// Whether a verification boot saw the mtty device type. A missing marker means the probe
/// never ran, which is different from a boot whose kernel has no such device.
pub(crate) fn parse_mdev(output: &str) -> Result<bool> {
    ensure!(
        output.contains("FARM-MDEV 0") || output.contains("FARM-MDEV 1"),
        "the verification VM booted but its mdev probe did not run: {}",
        output.trim()
    );
    Ok(output.contains("FARM-MDEV 1"))
}

const TERMINAL_ATTACH_TIMEOUT: Duration = Duration::from_secs(30);
const TERMINAL_CLOSE_TIMEOUT: Duration = Duration::from_secs(10);

fn agent_command(digest: &str, seconds: u64) -> String {
    format!(
        "export PATH=\"$HOME/.microsandbox/bin:$HOME/.local/bin:$PATH\"; export MSB_BACKEND=local; timeout --kill-after=10s {seconds} \"$HOME/.local/share/farm/agents/{digest}/farm-agent\""
    )
}

/// Complete newline-delimited frames at the front of `buffer`.
fn drain_frames(buffer: &mut Vec<u8>) -> Result<Vec<super::protocol::Frame>> {
    use super::protocol::MAX_FRAME;
    let mut frames = Vec::new();
    while let Some(index) = buffer.iter().position(|b| *b == b'\n') {
        ensure!(index <= MAX_FRAME, "farm-agent frame exceeds limit");
        let bytes: Vec<u8> = buffer.drain(..=index).collect();
        frames.push(
            serde_json::from_slice(&bytes)
                .map_err(|_| anyhow!("invalid farm-agent protocol frame"))?,
        );
    }
    ensure!(buffer.len() <= MAX_FRAME, "farm-agent frame exceeds limit");
    Ok(frames)
}

fn parse_inspection(output: &str) -> Result<HostInspection> {
    let mut values = BTreeMap::new();
    for line in output.lines() {
        if let Some(fields) = line.strip_prefix("FARM\t")
            && let Some((key, value)) = fields.split_once('\t')
        {
            values.insert(key, value);
        }
    }
    let field = |key| {
        values
            .get(key)
            .copied()
            .ok_or_else(|| anyhow!("incomplete host inspection response"))
    };
    let os = match field("os")? {
        "Linux" => "linux",
        "Darwin" => "macos",
        _ => "unsupported",
    }
    .to_string();
    let arch = match field("arch")? {
        "aarch64" | "arm64" => "aarch64",
        "x86_64" => "x86_64",
        _ => "unsupported",
    }
    .to_string();
    let version = field("msb")?
        .split_whitespace()
        .last()
        .filter(|v| {
            !v.is_empty() && v.len() < 32 && v.bytes().all(|b| b.is_ascii_digit() || b == b'.')
        })
        .map(str::to_owned);
    let integer = |key| -> Result<u64> {
        field(key)?
            .parse()
            .map_err(|_| anyhow!("invalid resource values in host inspection"))
    };
    Ok(HostInspection {
        compatible: os == "linux" && matches!(arch.as_str(), "aarch64" | "x86_64"),
        os,
        arch,
        kvm_available: field("kvm")? == "1",
        sudo_available: field("sudo")? == "1",
        memory_total_mib: integer("memory_total")?,
        memory_available_mib: integer("memory_available")?,
        disk_available_mib: integer("disk_available")?,
        msb_compatible: version.as_deref() == Some(super::MSB_VERSION),
        msb_version: version,
        docker_available: field("docker")? == "1",
        golden_image_available: field("golden")? == "1",
        // A kernel the host's agent boots from, installed by preparation or named by
        // FARM_LIBKRUNFW_PATH; absent when the stock one boots.
        custom_kernel: values
            .get("libkrunfw")
            .copied()
            .filter(|value| !value.is_empty())
            .map(str::to_owned),
        // An installed kernel the host cannot use, such as one built for another msb.
        custom_kernel_reason: values
            .get("libkrunfw_note")
            .copied()
            .filter(|value| !value.is_empty())
            .map(str::to_owned),
        mtty_available: false,
        gpu_kernel: None,
        vm_boot_verified: false,
        checked_at: crate::model::now(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote::{
        AuthMethod, HostStatus,
        test_server::{Fixture, client_key},
    };
    use std::sync::atomic::Ordering;
    #[test]
    fn upload_shell_handles_quoting_checksum_failure_and_atomic_replacement() {
        use sha2::{Digest, Sha256};
        use std::{
            io::Write,
            process::{Command, Stdio},
        };
        let dir = tempfile::tempdir().unwrap();
        let payload = b"binary fixture for SSH upload";
        let digest = format!("{:x}", Sha256::digest(payload));
        let command = upload_command(
            &digest,
            ".local/share/farm/agents/test",
            "farm-agent",
            180,
            "700",
        )
        .strip_prefix("timeout 180 ")
        .unwrap()
        .replace(
            "$HOME/.local/share/farm/agents/test",
            dir.path().to_str().unwrap(),
        );
        let run = |bytes: &[u8]| {
            let mut child = Command::new("bash")
                .args(["-c", &command])
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap();
            child.stdin.take().unwrap().write_all(bytes).unwrap();
            child.wait().unwrap()
        };
        assert!(run(payload).success());
        let installed = dir.path().join("farm-agent");
        assert_eq!(std::fs::read(&installed).unwrap(), payload);
        assert!(!run(b"corrupted bytes").success());
        assert_eq!(std::fs::read(&installed).unwrap(), payload);
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn the_verification_script_keeps_stdin_away_from_msb() {
        // The script arrives on stdin over SSH (`bash -s`); an msb that consumed it would
        // silently drop the mdev probe and the cleanup.
        let script = smoke_script(
            &uuid::Uuid::new_v4().to_string(),
            Some("/k/libkrunfw.so.5.6.1"),
        )
        .unwrap();
        for line in script
            .lines()
            .filter(|line| line.trim_start().starts_with("msb ") || line.contains("msb remove"))
        {
            assert!(line.contains("< /dev/null"), "{line}");
        }
        assert!(script.contains("export MSB_LIBKRUNFW_PATH='/k/libkrunfw.so.5.6.1'"));
        assert!(script.contains("FARM-MDEV 1") && script.contains("FARM-MDEV 0"));
        // One probe process reads the GPU stack; it is a single-quoted `sh -c` word, so a
        // quote inside would end that word early.
        assert!(!GPU_PROBE.contains('\''), "{GPU_PROBE}");
        // Under pipefail a reader that stops early fails the pipeline (AGENTS.md).
        for command in ["grep -q", "| head", "grep -m"] {
            assert!(!GPU_PROBE.contains(command), "{GPU_PROBE}");
        }
        for path in [
            "/dev/vfio/vfio",
            "/sys/class/mdev_bus",
            "/sys/class/mdev_bus/mtty",
            "/mdev_supported_types/",
            "available_instances",
            "uname -r",
        ] {
            assert!(GPU_PROBE.contains(path), "{path} missing from the probe");
        }
        assert!(
            script.contains(&format!("-- /bin/sh -c '{GPU_PROBE}'")),
            "{script}"
        );
        // Without a kernel the stock one boots: no pair is exported.
        let stock = smoke_script(&uuid::Uuid::new_v4().to_string(), None).unwrap();
        assert!(!stock.contains("MSB_LIBKRUNFW_PATH"));
    }

    #[test]
    fn the_gpu_probe_reports_types_counts_release_and_kernel() {
        use crate::remote::{GpuKernelKind, MdevType};
        let output = "FARM-MDEV 1\nFARM-GPU vfio 1\nFARM-GPU mdev 1\nFARM-GPU mtty 1\nFARM-GPU release 6.12.113\nFARM-GPU type mtty-1 24\nFARM-GPU type mtty-0 7\n";
        assert!(parse_mdev(output).unwrap());
        let gpu = parse_gpu_kernel(output).unwrap().for_kernel(
            true,
            Some("0123456789abcdef".into()),
            None,
        );
        assert_eq!(gpu.kernel, GpuKernelKind::Custom);
        assert_eq!(gpu.kernel_id.as_deref(), Some("0123456789abcdef"));
        assert!(gpu.vfio && gpu.vfio_mdev && gpu.mtty_registered);
        assert_eq!(
            gpu.mdev_types,
            [
                MdevType {
                    name: "mtty-1".into(),
                    available_instances: 24
                },
                MdevType {
                    name: "mtty-0".into(),
                    available_instances: 7
                }
            ]
        );
        assert_eq!(gpu.kernel_release, "6.12.113");
        assert!(gpu.mtty_available());

        // The stock kernel boots without any of it; the zero markers are still printed.
        let stock = parse_gpu_kernel(
            "FARM-MDEV 0\nFARM-GPU vfio 0\nFARM-GPU mdev 0\nFARM-GPU mtty 0\nFARM-GPU release 6.1.0\n",
        )
        .unwrap()
        .for_kernel(false, None, Some("the kernel was built for msb 0.7.12".into()));
        assert_eq!(stock.kernel, GpuKernelKind::Stock);
        assert!(!stock.mtty_available());
        assert_eq!(
            stock.reason.as_deref(),
            Some("the kernel was built for msb 0.7.12")
        );
        assert!(stock.mdev_types.is_empty());
    }

    #[test]
    fn a_boot_without_the_gpu_probe_is_an_error_not_a_stock_kernel() {
        let error = parse_gpu_kernel("FARM-MDEV 0\n").unwrap_err().to_string();
        assert!(error.contains("GPU probe did not run"), "{error}");
        let error = parse_gpu_kernel("FARM-GPU vfio maybe")
            .unwrap_err()
            .to_string();
        assert!(error.contains("invalid GPU probe value"), "{error}");
    }

    #[test]
    fn the_kernel_upload_script_targets_the_installed_kernel_directory() {
        let artifact = crate::kernel::Artifact {
            id: "0123456789abcdef".into(),
            manifest: "msb 0.7.13\narch x86_64\n".into(),
            library: std::path::PathBuf::from("/build/libkrunfw.so.5.6.1"),
            filename: "libkrunfw.so.5.6.1".into(),
            sha256: "b".repeat(64),
            msb: "0.7.13".into(),
            arch: "x86_64".into(),
        };
        let script = upload_command(
            &artifact.sha256,
            &format!(".local/share/farm/libkrunfw/{}", artifact.id),
            &artifact.filename,
            300,
            "644",
        );
        assert!(script.starts_with("timeout 300 bash -c '"), "{script}");
        assert!(
            script.contains("$HOME/.local/share/farm/libkrunfw/0123456789abcdef"),
            "{script}"
        );
        assert!(script.contains("chmod 644"), "{script}");
        assert!(script.contains("sha256sum -c - > /dev/null"), "{script}");
        assert!(
            script.contains("mv -f \"$tmp\" \"$dir/libkrunfw.so.5.6.1\""),
            "{script}"
        );
        // The script the kernel check and the manifest write run are plain shell, not a
        // REPL; they must not hide a failure behind a pipe that ends early.
        let check = format!(
            "{COMMON}path=\"$HOME/.local/share/farm/libkrunfw/{0}/{1}\"\nif [ -f \"$path\" ] && printf '%s  %s\\n' '{2}' \"$path\" | sha256sum -c --status; then printf cached; else printf missing; fi\n",
            artifact.id, artifact.filename, artifact.sha256
        );
        assert!(!check.contains("grep -q"), "{check}");
        assert!(!check.contains("| head"), "{check}");
    }
    #[tokio::test]
    async fn handshake_errors_name_the_local_cause_but_not_server_text() {
        let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = closed.local_addr().unwrap().port();
        drop(closed);
        let error = fingerprint(&SshTarget {
            address: "127.0.0.1".into(),
            port,
        })
        .await
        .unwrap_err()
        .to_string();
        assert!(
            error.starts_with("SSH handshake failed: Connection refused"),
            "{error}"
        );
        let error = handshake_failure(Ok(Err(russh::Error::NoCommonAlgo {
            kind: russh::AlgorithmKind::Kex,
            ours: vec!["curve25519-sha256".into()],
            theirs: vec!["server-chosen-text".into()],
        })));
        assert_eq!(error, "SSH handshake failed: no common Kex algorithm");
    }
    fn host(fixture: &Fixture) -> RemoteHost {
        RemoteHost {
            kind: crate::remote::HostKind::Ssh,
            id: "test".into(),
            address: "127.0.0.1".into(),
            port: fixture.port,
            username: "operator".into(),
            fingerprint: fixture.fingerprint.clone(),
            auth_method: Some(AuthMethod::Password),
            status: HostStatus::Registered,
            inspection: None,
            agent: None,
            capacity: None,
            active_job: None,
            error: None,
            connected: false,
            created_at: 0,
            updated_at: 0,
        }
    }
    #[tokio::test]
    async fn discovers_without_auth_and_rejects_changed_key_before_sending_a_credential() {
        let fixture = Fixture::start().await;
        let key = fingerprint(&SshTarget {
            address: "127.0.0.1".into(),
            port: fixture.port,
        })
        .await
        .unwrap();
        assert_eq!(key.fingerprint, fixture.fingerprint);
        assert_eq!(fixture.observed.auth_attempts.load(Ordering::SeqCst), 0);
        let mut target = host(&fixture);
        target.fingerprint = client_key()
            .public_key()
            .fingerprint(HashAlg::Sha256)
            .to_string();
        let error = Session::connect(&target, fixture.registration("unused").credentials)
            .await
            .err()
            .unwrap();
        assert!(error.to_string().contains("host key changed"));
        assert_eq!(fixture.observed.auth_attempts.load(Ordering::SeqCst), 0);
    }
    #[tokio::test]
    async fn authenticates_password_and_passphrase_protected_key_and_executes_over_real_ssh() {
        let fixture = Fixture::start().await;
        let target = host(&fixture);
        assert!(
            Session::connect(
                &target,
                Credentials::Password {
                    password: "wrong".into()
                }
            )
            .await
            .is_err()
        );
        let session = Session::connect(&target, fixture.registration("unused").credentials)
            .await
            .unwrap();
        assert_eq!(session.inspect().await.unwrap().memory_total_mib, 32768);
        use russh::keys::ssh_key::{Cipher, Kdf, LineEnding};
        let encrypted = client_key()
            .encrypt_with(
                Cipher::Aes256Ctr,
                Kdf::Bcrypt {
                    salt: vec![7; 16],
                    rounds: 4,
                },
                1234,
                "key-test-passphrase",
            )
            .unwrap()
            .to_openssh(LineEnding::LF)
            .unwrap();
        let credentials = Credentials::PrivateKey {
            private_key: encrypted.to_string(),
            passphrase: Some("key-test-passphrase".into()),
        };
        credentials.validate().unwrap();
        let session = Session::connect(&target, credentials).await.unwrap();
        assert!(session.inspect().await.unwrap().kvm_available);
    }
    #[tokio::test]
    async fn shell_session_streams_both_ways_and_ends_when_the_client_leaves() {
        use crate::farm::{TerminalInput, TerminalIo, TerminalSize};
        let fixture = Fixture::start().await;
        let session = Session::connect(&host(&fixture), fixture.registration("unused").credentials)
            .await
            .unwrap();
        let (input, input_rx) = tokio::sync::mpsc::channel(8);
        let (output_tx, mut output) = tokio::sync::mpsc::channel(8);
        let request = super::super::protocol::Request {
            version: super::super::protocol::VERSION,
            coordinator_id: uuid::Uuid::new_v4().to_string(),
            operation: super::super::protocol::Operation::Shell {
                farm_id: "demo".into(),
                generation: uuid::Uuid::new_v4().to_string(),
                node: None,
                size: TerminalSize { rows: 24, cols: 80 },
            },
        };
        let digest = "a".repeat(64);
        let relay = session.terminal(
            &digest,
            &request,
            TerminalIo {
                input: input_rx,
                output: output_tx,
            },
        );
        let client = async {
            input
                .send(TerminalInput::Data(b"uptime\r".to_vec()))
                .await
                .unwrap();
            assert_eq!(output.recv().await.unwrap(), b"uptime\r");
            input
                .send(TerminalInput::Resize(TerminalSize {
                    rows: 40,
                    cols: 120,
                }))
                .await
                .unwrap();
            assert_eq!(output.recv().await.unwrap(), b"40x120");
            drop(input);
        };
        let (result, ()) = tokio::join!(relay, client);
        result.unwrap();
        assert!(
            fixture
                .observed
                .calls
                .lock()
                .unwrap()
                .contains(&"rpc:shell".to_owned())
        );
    }
    #[test]
    fn rejects_incomplete_untrusted_inspection_output() {
        assert!(parse_inspection("FARM\tos\tLinux").is_err());
        assert!(parse_inspection("random shell startup output").is_err());
    }
}
