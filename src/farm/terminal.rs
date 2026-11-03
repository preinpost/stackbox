//! Interactive login shell in a farm's all-in-one VM, for the portal's Shell tab.

use std::time::Duration;

use anyhow::{Context, Result, bail};
use microsandbox::{ExecEvent, Sandbox};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

use super::node::connect;

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
}

/// Keystrokes in, terminal output out. Closing `input` ends the session.
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
    sandbox.detach().await;
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
    let stdin = handle.take_stdin().context("shell has no input")?;
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
            message = input.recv() => match message {
                Some(TerminalInput::Data(data)) => stdin.write(data).await?,
                Some(TerminalInput::Resize(size)) => control.resize(size.rows, size.cols).await?,
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
