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
    // `ExecHandle` has no `Drop` (microsandbox 0.7.13): leaving this loop early, by an error
    // or by the job dropping this future, would leave the guest script running. The guard
    // terminates it unless the guest already reported how the script ended.
    let control = handle.control();
    let name = sandbox.name().to_string();
    let mut guard = TerminateOnDrop::new(move || {
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            eprintln!("{name}: no runtime to stop the guest script");
            return;
        };
        runtime.spawn(async move {
            if let Err(e) = control.signal(TERMINATE_SIGNAL).await {
                eprintln!("{name}: failed to stop the guest script: {e}");
            }
        });
    });
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
            ExecEvent::Exited { code: 0 } => {
                guard.disarm();
                return Ok(());
            }
            ExecEvent::Exited { code } => {
                guard.disarm();
                // The log keeps the whole run; the reason is its last lines, and a job's error
                // is all a reader sees without opening a shell.
                let tail = last_lines(sandbox, log).await;
                bail!("{}: exit {code} (log: {log}){tail}", sandbox.name())
            }
            ExecEvent::Failed(failure) => {
                guard.disarm();
                bail!("{}: failed to start: {failure:?}", sandbox.name())
            }
            _ => {}
        }
    }
    bail!("{}: output ended without an exit status", sandbox.name())
}

/// SIGTERM: the wrapper traps it and passes it to the whole script; SIGKILL would only
/// stop the wrapper itself.
const TERMINATE_SIGNAL: i32 = 15;

/// Runs `stop` when dropped, unless disarmed first.
struct TerminateOnDrop<F: FnOnce()> {
    stop: Option<F>,
}

impl<F: FnOnce()> TerminateOnDrop<F> {
    fn new(stop: F) -> Self {
        Self { stop: Some(stop) }
    }

    fn disarm(&mut self) {
        self.stop = None;
    }
}

impl<F: FnOnce()> Drop for TerminateOnDrop<F> {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            stop();
        }
    }
}

/// The script `stream` runs: `script` under pipefail, its output teed into `log`.
///
/// The pipeline runs as a job (`set -m`) in its own process group, so a SIGTERM to the
/// wrapper reaches every process of the script, kolla-ansible's children included.
fn wrapped_script(script: &str, log: &str) -> String {
    let log = sh_quote(log);
    format!(
        "step() {{ printf '{STEP_PREFIX}%s\\n' \"$*\"; }}; mkdir -p \"$(dirname {log})\"; set -o pipefail; trap '' TERM INT HUP; set -m; ( trap - TERM INT HUP; {script} ) 2>&1 | tee -a {log} & set +m; trap 'kill -TERM %1 2>/dev/null; wait %1; exit 143' TERM INT HUP; wait %1"
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
    use super::{TerminateOnDrop, step_of, tail_script, wrapped_script};
    use std::cell::Cell;

    #[test]
    fn the_guard_stops_the_script_unless_its_end_was_seen() {
        let stopped = Cell::new(0);
        drop(TerminateOnDrop::new(|| stopped.set(stopped.get() + 1)));
        assert_eq!(stopped.get(), 1);
        let mut seen = TerminateOnDrop::new(|| stopped.set(stopped.get() + 1));
        seen.disarm();
        drop(seen);
        assert_eq!(stopped.get(), 1);
    }

    #[test]
    fn the_wrapper_keeps_the_exit_status() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("run.log");
        let run = |script: &str| {
            std::process::Command::new("bash")
                .arg("-c")
                .arg(wrapped_script(script, log.to_str().unwrap()))
                .status()
                .unwrap()
                .code()
        };
        assert_eq!(run("true"), Some(0));
        assert_eq!(run("exit 7"), Some(7));
        assert_eq!(run("false | cat"), Some(1));
    }

    /// Whether `pid` still runs. A zombie counts as gone: an orphaned script is reparented
    /// to PID 1, and in a CI container that is `tail -f /dev/null`, which never reaps it, so
    /// `kill -0` would still see the exited process.
    fn alive(pid: &str) -> bool {
        let out = std::process::Command::new("ps")
            .args(["-o", "stat=", "-p", pid])
            .output()
            .unwrap();
        let stat = String::from_utf8_lossy(&out.stdout);
        out.status.success() && !stat.trim().is_empty() && !stat.trim_start().starts_with('Z')
    }

    #[test]
    fn terminating_the_wrapper_terminates_the_script_it_runs() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("run.log");
        let pidf = dir.path().join("pid");
        let script = format!("sleep 300 & echo $! > '{}'; wait", pidf.display());
        let mut child = std::process::Command::new("bash")
            .arg("-c")
            .arg(wrapped_script(&script, log.to_str().unwrap()))
            .spawn()
            .unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let pid = loop {
            if let Ok(pid) = std::fs::read_to_string(&pidf)
                && !pid.trim().is_empty()
            {
                break pid.trim().to_string();
            }
            assert!(std::time::Instant::now() < deadline, "script never started");
            std::thread::sleep(std::time::Duration::from_millis(20));
        };
        // What `stream` sends when it stops reading early.
        let wrapper = child.id().to_string();
        std::process::Command::new("kill")
            .args(["-TERM", wrapper.as_str()])
            .status()
            .unwrap();
        child.wait().unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while alive(&pid) && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        let orphan = alive(&pid);
        if orphan {
            std::process::Command::new("kill")
                .args(["-KILL", pid.as_str()])
                .status()
                .unwrap();
        }
        assert!(!orphan, "the script outlived its wrapper");
    }

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
