use super::{Credentials, HostInspection, HostKey, RemoteHost, SshTarget};
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

fn upload_command(digest: &str) -> String {
    let script = format!(
        r#"set -eu
umask 077
dir="$HOME/.local/share/farm/agents/{digest}"
mkdir -p "$dir"
tmp=$(mktemp "$dir/upload.XXXXXX")
trap 'rm -f "$tmp"' EXIT HUP INT TERM
cat > "$tmp"
printf '%s  %s\n' '{digest}' "$tmp" | sha256sum -c - > /dev/null
chmod 700 "$tmp"
mv -f "$tmp" "$dir/farm-agent"
"#
    );
    format!("timeout 180 bash -c '{}'", script.replace('\'', "'\\''"))
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
            let command = upload_command(digest);
            let upload = async {
                let channel = self.handle.channel_open_session().await?;
                channel.exec(true, command).await?;
                let (mut reader, writer) = channel.split();
                let send = async {
                    writer.data(&artifact.bytes[..]).await?;
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
                    ensure!(code == Some(0), "agent upload failed");
                    Ok::<_, anyhow::Error>(())
                };
                tokio::try_join!(send, receive)?;
                Ok::<_, anyhow::Error>(())
            };
            tokio::time::timeout(Duration::from_secs(190), upload)
                .await
                .map_err(|_| anyhow!("agent upload timed out"))?
                .map_err(|_| anyhow!("agent upload / checksum verification failed"))?;
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
    pub async fn smoke(&self, token: &str) -> Result<()> {
        // Token comes exclusively from a locally generated UUID, never an API field.
        self.run(
            &smoke_script(token)?,
            180,
            "VM boot / exec / cleanup verification",
        )
        .await?;
        Ok(())
    }
}

/// Install msb and libkrunfw at [`super::MSB_VERSION`], or upgrade an older msb to it.
pub(crate) fn install_script() -> String {
    include_str!("install.sh").replace("@MSB_VERSION@", super::MSB_VERSION)
}

/// Report the host's facts; `golden` says whether this build's golden image is built yet.
fn inspect_script() -> String {
    include_str!("inspect.sh").replace("@GOLDEN@", &crate::farm::golden::current())
}

/// Boot a small VM from the base image the golden image is built from, run a command in it
/// and remove it.
pub(crate) fn smoke_script(token: &str) -> Result<String> {
    // Token comes exclusively from a locally generated UUID, never an API field.
    ensure!(uuid::Uuid::parse_str(token).is_ok(), "invalid smoke token");
    Ok(format!(
        "{COMMON}\nname=farm-check-{token}\ntrap 'msb remove --force \"$name\" >/dev/null 2>&1' EXIT\nmsb create --name \"$name\" --cpus 1 --memory 512M --root-disk 2G --max-duration 120s --no-net {image}\nmsb exec \"$name\" -- /bin/true\nmsb remove --force \"$name\"\ntrap - EXIT\n",
        image = crate::farm::golden::DEFAULT_OS.base_image
    ))
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
        let command = upload_command(&digest)
            .strip_prefix("timeout 180 ")
            .unwrap()
            .replace(
                "$HOME/.local/share/farm/agents",
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
        let installed = dir.path().join(&digest).join("farm-agent");
        assert_eq!(std::fs::read(&installed).unwrap(), payload);
        assert!(!run(b"corrupted bytes").success());
        assert_eq!(std::fs::read(&installed).unwrap(), payload);
        assert_eq!(
            std::fs::read_dir(dir.path().join(digest)).unwrap().count(),
            1
        );
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
