use std::io::Write;

use crate::gpu::sh_quote;
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

/// Marker a generated script prints to report a finer step than the phase.
const STEP_PREFIX: &str = "__FARM_STEP__ ";
/// Longest Ansible task name kept for display; the full text stays in the log.
const STEP_LIMIT: usize = 160;

/// Recognises our own step markers and the Ansible callback's task lines.
fn step_of(line: &str) -> Option<String> {
    let step = line
        .strip_prefix(STEP_PREFIX)
        .or_else(|| line.strip_prefix("TASK "))
        .or_else(|| line.strip_prefix("HANDLER "))?
        .trim();
    (!step.is_empty()).then(|| step.chars().take(STEP_LIMIT).collect())
}

/// Run a bash script, streaming its output to stderr and appending it to `log` in the guest.
///
/// `step "..."` inside the script reports the current step, which the manager shows while
/// the phase runs. Kolla-Ansible phases report their task names through the same path.
pub async fn stream(sandbox: &Sandbox, script: &str, log: &str, sink: &EventSink) -> Result<()> {
    let wrapped = wrapped_script(script, log);
    let mut handle = sandbox
        .exec_stream("bash", ["-c", wrapped.as_str()])
        .await?;
    let mut stdout = std::io::stderr();
    let mut pending = String::new();
    while let Some(event) = handle.recv().await {
        match event {
            ExecEvent::Stdout(bytes) | ExecEvent::Stderr(bytes) => {
                let text = String::from_utf8_lossy(&bytes).into_owned();
                sink(DeployEvent::Output(text.clone()))?;
                stdout.write_all(&bytes)?;
                stdout.flush()?;
                // Steps are line-oriented, but chunks are not.
                pending.push_str(&text);
                while let Some(end) = pending.find('\n') {
                    let line: String = pending.drain(..=end).collect();
                    if let Some(step) = step_of(line.trim_end()) {
                        sink(DeployEvent::Step(step))?;
                    }
                }
                if pending.len() > 8 * 1024 {
                    pending.clear();
                }
            }
            ExecEvent::Exited { code: 0 } => return Ok(()),
            ExecEvent::Exited { code } => {
                // The log keeps the whole run; the reason is its last lines, and a job's error
                // is all a reader sees without opening a shell.
                let tail = last_lines(sandbox, log).await;
                bail!("{}: exit {code} (log: {log}){tail}", sandbox.name())
            }
            ExecEvent::Failed(failure) => bail!("{}: failed to start: {failure:?}", sandbox.name()),
            _ => {}
        }
    }
    bail!("{}: output ended without an exit status", sandbox.name())
}

/// The script `stream` runs: `script` under pipefail, its output teed into `log`.
fn wrapped_script(script: &str, log: &str) -> String {
    let log = sh_quote(log);
    format!(
        "step() {{ printf '{STEP_PREFIX}%s\\n' \"$*\"; }}; mkdir -p \"$(dirname {log})\"; set -o pipefail; ( {script} ) 2>&1 | tee -a {log}"
    )
}

/// The reason a script failed, from the end of its log: step markers dropped, empty lines
/// skipped, three lines at most.
async fn last_lines(sandbox: &Sandbox, log: &str) -> String {
    match run(sandbox, &tail_script(log)).await {
        Ok(tail) if !tail.trim().is_empty() => {
            let lines: Vec<&str> = tail
                .lines()
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .collect();
            format!(": {}", lines.join(" / "))
        }
        _ => String::new(),
    }
}

/// The command `last_lines` runs: the log without step markers and blank lines, last three.
fn tail_script(log: &str) -> String {
    let log = sh_quote(log);
    format!("grep -v '^{STEP_PREFIX}' {log} 2>/dev/null | grep -v '^[[:space:]]*$' | tail -n 3")
}

#[cfg(test)]
mod tests {
    use super::{step_of, tail_script, wrapped_script};

    #[test]
    fn the_wrapper_and_the_tail_run_with_a_log_path_that_has_spaces_and_quotes() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("run dir").join("it's $HOME.log");
        let log = log.to_str().unwrap();
        let script = "echo hello; echo '__FARM_STEP__ copy'; echo last";
        let run = std::process::Command::new("bash")
            .arg("-c")
            .arg(wrapped_script(script, log))
            .output()
            .unwrap();
        assert!(
            run.status.success(),
            "{}",
            String::from_utf8_lossy(&run.stderr)
        );
        assert!(std::fs::read_to_string(log).unwrap().contains("last"));

        let tail = std::process::Command::new("bash")
            .arg("-c")
            .arg(tail_script(log))
            .output()
            .unwrap();
        let tail = String::from_utf8_lossy(&tail.stdout).into_owned();
        assert_eq!(tail, "hello\nlast\n");
    }

    #[test]
    fn recognises_our_markers_and_ansible_tasks_only() {
        assert_eq!(
            step_of("__FARM_STEP__ globals.yml 작성").as_deref(),
            Some("globals.yml 작성")
        );
        assert_eq!(
            step_of("TASK [keystone : Copy config]").as_deref(),
            Some("[keystone : Copy config]")
        );
        assert_eq!(
            step_of("HANDLER [restart mariadb]").as_deref(),
            Some("[restart mariadb]")
        );
        // Ordinary output, task results and empty markers stay out of the step line.
        assert!(step_of("changed: all-in-one").is_none());
        assert!(step_of("Downloading crates ...").is_none());
        assert!(step_of("__FARM_STEP__   ").is_none());
        assert!(step_of("TASKNOTHING").is_none());
    }

    #[test]
    fn long_task_names_are_shortened_for_display() {
        let line = format!("TASK [{}]", "x".repeat(400));
        assert_eq!(step_of(&line).unwrap().chars().count(), super::STEP_LIMIT);
    }
}
