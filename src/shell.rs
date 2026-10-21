use std::io::Write;

use crate::kolla::{DeployEvent, EventSink};
use anyhow::{Result, bail};
use microsandbox::{ExecEvent, Sandbox};

/// Run a bash script and return its stdout, failing with stderr on a non-zero exit.
pub async fn run(sandbox: &Sandbox, script: &str) -> Result<String> {
    let out = sandbox.exec("bash", ["-c", script]).await?;
    if !out.status().success {
        bail!(
            "{}: exit {}: {}",
            sandbox.name(),
            out.status().code,
            out.stderr()?.trim()
        );
    }
    Ok(out.stdout()?)
}

/// Run a bash script, streaming its output to stderr and appending it to `log` in the guest.
pub async fn stream(sandbox: &Sandbox, script: &str, log: &str, sink: &EventSink) -> Result<()> {
    let wrapped =
        format!("mkdir -p \"$(dirname {log})\"; set -o pipefail; ( {script} ) 2>&1 | tee -a {log}");
    let mut handle = sandbox
        .exec_stream("bash", ["-c", wrapped.as_str()])
        .await?;
    let mut stdout = std::io::stderr();
    while let Some(event) = handle.recv().await {
        match event {
            ExecEvent::Stdout(bytes) | ExecEvent::Stderr(bytes) => {
                sink(DeployEvent::Output(
                    String::from_utf8_lossy(&bytes).into_owned(),
                ))?;
                stdout.write_all(&bytes)?;
                stdout.flush()?;
            }
            ExecEvent::Exited { code: 0 } => return Ok(()),
            ExecEvent::Exited { code } => bail!("{}: exit {code} (log: {log})", sandbox.name()),
            ExecEvent::Failed(failure) => bail!("{}: failed to start: {failure:?}", sandbox.name()),
            _ => {}
        }
    }
    bail!("{}: output ended without an exit status", sandbox.name())
}
