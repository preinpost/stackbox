//! Interactive login shell in a farm's all-in-one VM, for the portal's Shell tab.

use std::time::Duration;

use anyhow::{Context, Result, bail};
use microsandbox::{ExecEvent, Sandbox};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

use super::node::{connect, detach};

/// How long a hung-up shell may take to exit before it is killed.
const HANGUP_GRACE: Duration = Duration::from_secs(2);
const SIGHUP: i32 = 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TerminalSize {
    pub rows: u16,
    pub cols: u16,
}
impl TerminalSize {
    pub fn validate(self) -> Result<Self> {
        if !(1..=500).contains(&self.rows) || !(1..=1000).contains(&self.cols) {
            bail!("terminal size must be 1-500 rows and 1-1000 columns");
        }
        Ok(self)
    }
}

#[derive(Debug)]
pub enum TerminalInput {
    Data(Vec<u8>),
    Resize(TerminalSize),
    /// The writer is done: close the shell's stdin and keep the session running until the
    /// shell exits. Dropping the sender instead means the client left, which hangs the shell up.
    Close,
}

/// Keystrokes in, terminal output out. Closing `input` ends the session; sending
/// [`TerminalInput::Close`] only ends the shell's stdin.
pub struct TerminalIo {
    pub input: mpsc::Receiver<TerminalInput>,
    pub output: mpsc::Sender<Vec<u8>>,
}

/// Run a login shell in one of the farm's VMs until it exits or the client goes away.
/// `node` names a multi-node VM by its suffix (`control`, `compute-1`); `None` opens the
/// all-in-one or control VM. Returns the shell's exit code, or `None` when the client closed
/// the session.
pub async fn attach(
    farm_id: &str,
    node: Option<&str>,
    size: TerminalSize,
    io: TerminalIo,
) -> Result<Option<i32>> {
    let name = match node {
        None => super::all_in_one_name(farm_id).await?,
        Some(node) => super::get_farm(farm_id)
            .await?
            .into_iter()
            .find(|vm| {
                vm.role
                    .and_then(|role| super::suffix(role, vm.index))
                    .as_deref()
                    == Some(node)
            })
            .map(|vm| vm.name)
            .with_context(|| format!("farm '{farm_id}' has no VM '{node}'"))?,
    };
    let sandbox = connect(&name).await?;
    let result = session(&sandbox, size, io).await;
    detach(&sandbox).await;
    result
}

async fn session(sandbox: &Sandbox, size: TerminalSize, io: TerminalIo) -> Result<Option<i32>> {
    let mut handle = sandbox
        .exec_stream_with("/bin/bash", |exec| {
            exec.arg("-l")
                .cwd("/root")
                .env("HOME", "/root")
                .env("TERM", "xterm-256color")
                .stdin_pipe()
                .tty(true)
        })
        .await
        .context("start shell")?;
    let control = handle.control();
    let mut stdin = Some(handle.take_stdin().context("shell has no input")?);
    control.resize(size.rows, size.cols).await?;
    let TerminalIo { mut input, output } = io;
    loop {
        tokio::select! {
            event = handle.recv() => match event {
                Some(ExecEvent::Stdout(bytes) | ExecEvent::Stderr(bytes)) => {
                    if output.send(bytes.to_vec()).await.is_err() {
                        break;
                    }
                }
                Some(ExecEvent::Exited { code }) => return Ok(Some(code)),
                Some(ExecEvent::Failed(failure)) => bail!("shell failed to start: {failure:?}"),
                Some(_) => {}
                None => bail!("shell session ended without an exit status"),
            },
            message = input.recv(), if stdin.is_some() => match message {
                Some(TerminalInput::Data(data)) => {
                    stdin
                        .as_ref()
                        .expect("guarded by the select condition")
                        .write(data)
                        .await?
                }
                Some(TerminalInput::Resize(size)) => control.resize(size.rows, size.cols).await?,
                // EOF, as a script session sends at the end of its text. The shell keeps
                // running its jobs and reports its exit; only a departed client is hung up.
                Some(TerminalInput::Close) => {
                    if let Some(stdin) = stdin.take() {
                        stdin.close().await?;
                    }
                }
                None => break,
            },
        }
    }
    // The client left: hang up like a closed terminal, so the shell takes its jobs with it.
    control.signal(SIGHUP).await.ok();
    let exited = tokio::time::timeout(HANGUP_GRACE, async {
        while let Some(event) = handle.recv().await {
            if matches!(event, ExecEvent::Exited { .. } | ExecEvent::Failed(_)) {
                return;
            }
        }
    })
    .await;
    if exited.is_err() {
        control.kill().await.ok();
    }
    Ok(None)
}

/// A non-interactive command run through the same shell a portal user gets. The shell is
/// switched to `bash -s`, so readline does not echo the script and the output can be
/// delimited; farm uses this for the OpenStack CLI calls that read and write a farm's
/// instance metadata. The session is owned by the caller's backend, which may be local or
/// an agent on another host.
pub struct ScriptSession {
    input: mpsc::Sender<TerminalInput>,
    output: mpsc::Receiver<Vec<u8>>,
    session: futures::future::BoxFuture<'static, Result<()>>,
}

/// How long a control VM script may take before its session is abandoned. Reading the
/// instance list calls the OpenStack API for each server, so this is generous.
const SCRIPT_TIMEOUT: Duration = Duration::from_secs(120);

impl ScriptSession {
    /// `session` runs the shell; it must return after the caller closes the shell's stdin
    /// and the shell exits.
    pub fn new(
        session: impl FnOnce(TerminalIo) -> futures::future::BoxFuture<'static, Result<()>>,
    ) -> Self {
        let (input, input_rx) = mpsc::channel(4);
        let (output_tx, output) = mpsc::channel(64);
        Self {
            input,
            output,
            session: session(TerminalIo {
                input: input_rx,
                output: output_tx,
            }),
        }
    }

    /// Runs `script` under `bash -s` and returns its stdout. A non-zero exit status is an
    /// error carrying the script's stderr. The markers are random per call; the echoed
    /// command line cannot be mistaken for output because it carries the escapes
    /// literally, not the newlines they print.
    pub async fn run(self, script: &str) -> Result<String> {
        let ScriptSession {
            input,
            mut output,
            session,
        } = self;
        tokio::pin!(session);
        let marker = uuid::Uuid::new_v4().simple().to_string();
        let ready = format!("__FARM_READY_{marker}__");
        let begin = format!("__FARM_BEGIN_{marker}__");
        let stderr_marker = format!("__FARM_STDERR_{marker}__");
        let end = format!("__FARM_END_{marker}__");
        // The first line runs before readline can see the script. `exec bash -s` reads
        // the script from stdin without echoing it, and `exit 0` at the end of the wrapper
        // ends it. The output files carry the random marker, so they need no mktemp(1).
        let out = format!("/tmp/farm-script-{marker}.out");
        let err = format!("/tmp/farm-script-{marker}.err");
        // `-onlcr` keeps the PTY from turning every newline into CRLF; the parser also
        // normalises CRLF below, because a shell without stty(1) cannot set it.
        //
        // `2>/dev/null` is what keeps the captured output to the script's own: bash treats
        // itself as interactive when its stdin and stderr are terminals, and an interactive
        // `bash -s` prints a prompt and echoes every line it reads, both of which would land
        // between the markers. farm's instance list is JSON on stdout, so a prompt there is
        // a parse error. bash does not touch the terminal settings when it is not
        // interactive, so `stty -echo` above also holds for the script's own input.
        let start = format!(
            "PS1=; PS2=; stty -echo -onlcr 2>/dev/null; printf '\\n{ready}\\n'; exec bash -s 2>/dev/null\n"
        );
        input
            .send(TerminalInput::Data(start.into_bytes()))
            .await
            .context("the farm VM's shell closed before the script")?;
        let wrapper = format!(
            "printf '\\n{begin}\\n'\n( {script}\n) >\"{out}\" 2>\"{err}\"\n__farm_rc=$?\ncat \"{out}\"\nprintf '\\n{stderr_marker}\\n'\ncat \"{err}\"\nprintf '\\n{end} %s\\n' \"$__farm_rc\"\nrm -f \"{out}\" \"{err}\"\nexit 0\n"
        );
        let mut buffer = Vec::new();
        let mut sent = false;
        let deadline = tokio::time::Instant::now() + SCRIPT_TIMEOUT;
        loop {
            tokio::select! {
                result = &mut session => {
                    result?;
                    break;
                }
                data = output.recv() => match data {
                    Some(bytes) => {
                        buffer.extend_from_slice(&bytes);
                        if !sent
                            && (contains(&buffer, format!("\n{ready}\n").as_bytes())
                                || contains(&buffer, format!("\r\n{ready}\r\n").as_bytes()))
                        {
                            input
                                .send(TerminalInput::Data(wrapper.clone().into_bytes()))
                                .await
                                .context("the farm VM's shell closed before the script")?;
                            // EOF tells `bash -s` that the script is complete. A PTY, which is
                            // what this shell is, cannot deliver EOF (the microsandbox agent keeps
                            // the master open), so the wrapper's last line exits the shell itself;
                            // the script's status travels in the end marker either way. The sender
                            // stays alive: dropping it is a client that left, and the session would
                            // hang the shell up before the script has run.
                            input
                                .send(TerminalInput::Close)
                                .await
                                .context("the farm VM's shell closed before the script")?;
                            sent = true;
                        }
                    }
                    None => {
                        session.await?;
                        break;
                    }
                },
                _ = tokio::time::sleep_until(deadline) => {
                    bail!("the farm VM's shell did not finish the script within {SCRIPT_TIMEOUT:?}");
                }
            }
        }
        while let Ok(bytes) = output.try_recv() {
            buffer.extend_from_slice(&bytes);
        }
        let text = String::from_utf8_lossy(&buffer).replace("\r\n", "\n");
        let begin_marker = format!("\n{begin}\n");
        let after_begin = text
            .find(&begin_marker)
            .map(|index| index + begin_marker.len())
            .with_context(|| "the farm VM's shell did not run the script")?;
        let rest = &text[after_begin..];
        let (stdout, rest) = rest
            .split_once(&format!("\n{stderr_marker}\n"))
            .with_context(|| "the farm VM's shell did not report the script's end")?;
        let (stderr, rest) = rest
            .split_once(&format!("\n{end} "))
            .with_context(|| "the farm VM's shell did not report the script's exit status")?;
        let code = rest
            .lines()
            .next()
            .unwrap_or_default()
            .trim()
            .parse::<i32>()
            .with_context(|| "the farm VM's shell reported no exit status")?;
        if code != 0 {
            let stderr = stderr.trim();
            bail!(
                "the control VM script failed (exit {code}){}",
                if stderr.is_empty() {
                    String::new()
                } else {
                    format!(": {stderr}")
                }
            );
        }
        Ok(stdout.trim_end().to_owned())
    }
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

/// Run a script in the all-in-one or control VM of `farm_id`; `node` names a multi-node
/// VM by suffix when the caller needs another one.
pub async fn run_script(farm_id: &str, node: Option<&str>, script: &str) -> Result<String> {
    let farm_id = farm_id.to_owned();
    let node = node.map(str::to_owned);
    ScriptSession::new(move |io| {
        Box::pin(async move {
            attach(
                &farm_id,
                node.as_deref(),
                TerminalSize {
                    rows: 40,
                    cols: 240,
                },
                io,
            )
            .await
            .map(|_| ())
        })
    })
    .run(script)
    .await
}

#[cfg(test)]
mod tests {
    use super::TerminalSize;

    #[test]
    fn terminal_size_rejects_empty_or_absurd_dimensions() {
        assert!(TerminalSize { rows: 24, cols: 80 }.validate().is_ok());
        assert!(TerminalSize { rows: 0, cols: 80 }.validate().is_err());
        assert!(TerminalSize { rows: 24, cols: 0 }.validate().is_err());
        assert!(
            TerminalSize {
                rows: 501,
                cols: 80
            }
            .validate()
            .is_err()
        );
        assert!(
            TerminalSize {
                rows: 24,
                cols: 1001
            }
            .validate()
            .is_err()
        );
    }
}
