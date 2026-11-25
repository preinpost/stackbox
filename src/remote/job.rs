//! Host-side jobs that outlive the request which started them. A worker runs the job and
//! records it under `<agent dir>/jobs/<job id>/`; requests only follow those files, so a
//! coordinator that disconnects or restarts stops the follower and attaches again later.
//!
//! A job directory holds `spec.json` (the request that started it), `worker.lock` (held for
//! the worker's whole run), `pid`, `frames.jsonl` (progress frames, one per line),
//! `result.json` (the final `Frame::Result` or `Frame::Error`, written atomically) and
//! `worker.log` (a worker process's stderr).
use super::{
    agent::Emitter,
    protocol::{Frame, Reply, Request},
};
use anyhow::{Context, Result, anyhow, bail};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::{
    ffi::OsString,
    fs::{self, File, OpenOptions},
    io::{ErrorKind, Read, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

/// Finished jobs stay this long for a late attach; starting a new job removes older ones.
const KEEP: Duration = Duration::from_secs(7 * 24 * 3600);

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Spec {
    request: Request,
}

#[derive(Clone)]
pub struct Job {
    dir: PathBuf,
    /// The request that started the job; its coordinator owns the job.
    pub request: Request,
}
/// The worker's hold on its job; the job counts as running while it exists.
pub struct Claim(File);
impl Drop for Claim {
    fn drop(&mut self) {
        // As with the agent's locks: a child forked meanwhile must not keep the job alive.
        let _ = FileExt::unlock(&self.0);
    }
}

impl Job {
    fn path(agent: &Path, id: &str) -> Result<PathBuf> {
        uuid::Uuid::parse_str(id).context("invalid job id")?;
        Ok(agent.join("jobs").join(id))
    }
    /// Record a new job for `request` under `id`, or open the one already recorded there.
    /// The flag is whether this call recorded it, and so has to start its worker.
    pub fn create(agent: &Path, id: &str, request: &Request) -> Result<(Self, bool)> {
        let dir = Self::path(agent, id)?;
        let jobs = dir.parent().context("job directory has no parent")?;
        private_dir(jobs)?;
        if !dir.exists() {
            let staging = jobs.join(format!(".new-{}", uuid::Uuid::new_v4()));
            private_dir(&staging)?;
            let mut spec = private_file(&staging.join("spec.json"), false)?;
            spec.write_all(&serde_json::to_vec(&Spec {
                request: request.clone(),
            })?)?;
            spec.sync_all()?;
            // The rename publishes the job whole; of two starts with one id, one loses here.
            match fs::rename(&staging, &dir) {
                Ok(()) => {
                    sweep(jobs, &dir);
                    return Ok((Self::open(agent, id)?, true));
                }
                Err(_) if dir.exists() => {
                    let _ = fs::remove_dir_all(&staging);
                }
                Err(error) => {
                    let _ = fs::remove_dir_all(&staging);
                    return Err(error.into());
                }
            }
        }
        Ok((Self::open(agent, id)?, false))
    }
    pub fn open(agent: &Path, id: &str) -> Result<Self> {
        let dir = Self::path(agent, id)?;
        let spec: Spec = match fs::read(dir.join("spec.json")) {
            Ok(bytes) => serde_json::from_slice(&bytes).context("invalid host job record")?,
            Err(e) if e.kind() == ErrorKind::NotFound => {
                bail!("job {id} is unknown on this host; it never started or was cleaned up")
            }
            Err(e) => return Err(e.into()),
        };
        Ok(Self {
            dir,
            request: spec.request,
        })
    }
    /// Taken by the worker before it runs the job. It waits out a concurrent `alive` probe,
    /// which holds the lock only for a moment.
    pub fn claim(&self) -> Result<Claim> {
        let file = private_file(&self.dir.join("worker.lock"), false)?;
        file.lock_exclusive()?;
        fs::write(self.dir.join("pid"), std::process::id().to_string())?;
        Ok(Claim(file))
    }
    /// Whether a worker holds the job now.
    pub fn running(&self) -> bool {
        alive(&self.dir)
    }
    pub fn finished(&self) -> bool {
        self.dir.join("result.json").exists()
    }
    /// Where the worker writes the job's progress.
    pub fn recorder(&self) -> Result<Emitter> {
        let file = Mutex::new(private_file(&self.dir.join("frames.jsonl"), true)?);
        Ok(Arc::new(move |frame| {
            let mut line = serde_json::to_vec(&frame)?;
            line.push(b'\n');
            file.lock()
                .map_err(|_| anyhow!("job record poisoned"))?
                .write_all(&line)?;
            Ok(())
        }))
    }
    pub fn finish(&self, outcome: Result<Reply>) -> Result<()> {
        let frame = match outcome {
            Ok(result) => Frame::Result { result },
            Err(error) => Frame::Error {
                message: format!("{error:#}"),
            },
        };
        let path = self.dir.join("result.json.tmp");
        let mut file = private_file(&path, false)?;
        file.set_len(0)?;
        file.write_all(&serde_json::to_vec(&frame)?)?;
        file.sync_all()?;
        fs::rename(path, self.dir.join("result.json"))?;
        Ok(())
    }
    fn result(&self) -> Result<Option<Result<Reply>>> {
        let bytes = match fs::read(self.dir.join("result.json")) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        match serde_json::from_slice(&bytes).context("invalid host job result")? {
            Frame::Result { result } => Ok(Some(Ok(result))),
            Frame::Error { message } => Ok(Some(Err(anyhow!(message)))),
            _ => bail!("invalid host job result"),
        }
    }
    /// Hand the recorded frames after the first `offset` to `emit` as they appear, then
    /// return the job's result.
    pub async fn follow(&self, offset: u64, emit: &Emitter) -> Result<Reply> {
        let mut frames: Option<File> = None;
        let mut buffer = Vec::new();
        let mut seen = 0u64;
        let mut delay = Duration::from_millis(10);
        loop {
            // In this order: a worker writes everything before it lets go of its lock.
            let gone = !alive(&self.dir);
            let result = self.result()?;
            if frames.is_none() {
                frames = match File::open(self.dir.join("frames.jsonl")) {
                    Ok(file) => Some(file),
                    Err(e) if e.kind() == ErrorKind::NotFound => None,
                    Err(e) => return Err(e.into()),
                };
            }
            let before = seen;
            if let Some(file) = &mut frames {
                file.read_to_end(&mut buffer)?;
            }
            while let Some(end) = buffer.iter().position(|b| *b == b'\n') {
                let line: Vec<u8> = buffer.drain(..=end).collect();
                seen += 1;
                if seen > offset {
                    emit(serde_json::from_slice(&line).context("invalid host job frame")?)?;
                }
            }
            if let Some(result) = result {
                return result;
            }
            if gone {
                bail!(
                    "the deploy worker exited without a result; see {} on the host",
                    self.dir.join("worker.log").display()
                );
            }
            delay = if seen > before {
                Duration::from_millis(10)
            } else {
                (delay * 2).min(Duration::from_millis(250))
            };
            tokio::time::sleep(delay).await;
        }
    }
    /// Start `command` with the job directory appended as the job's worker. It runs in a
    /// session of its own, so neither the end of this request nor a signal to the
    /// requester's process group, SSH channel or terminal stops it.
    pub async fn spawn(&self, command: &[OsString]) -> Result<()> {
        let (program, args) = command.split_first().context("no worker command")?;
        let log = private_file(&self.dir.join("worker.log"), true)?;
        let mut worker = tokio::process::Command::new(program);
        worker
            .args(args)
            .arg(&self.dir)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(log);
        #[cfg(unix)]
        // SAFETY: setsid is async-signal-safe and touches no memory of the parent.
        unsafe {
            worker.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        // DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP: no console to lose with the requester,
        // and no Ctrl+C from the requester's group.
        #[cfg(windows)]
        worker.creation_flags(0x0000_0008 | 0x0000_0200);
        let mut child = worker.spawn().context("cannot start the deploy worker")?;
        // Follow only once the worker holds the job, so it cannot look dead before it ran.
        let started = async {
            while !self.dir.join("pid").exists() {
                if child.try_wait()?.is_some() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            Ok::<_, anyhow::Error>(())
        };
        tokio::time::timeout(Duration::from_secs(30), started)
            .await
            .map_err(|_| anyhow!("the deploy worker did not start"))??;
        // Reaps the worker if this agent outlives it, as a dial-in agent does.
        tokio::spawn(async move {
            let _ = child.wait().await;
        });
        Ok(())
    }
}

/// Whether a worker holds the job in `dir`.
fn alive(dir: &Path) -> bool {
    let Ok(file) = File::open(dir.join("worker.lock")) else {
        return false;
    };
    // The fs2 trait method: `File::try_lock_shared` is newer than the crate's rust-version.
    let free = FileExt::try_lock_shared(&file).is_ok();
    if free {
        let _ = FileExt::unlock(&file);
    }
    !free
}

/// Remove jobs that ended more than `KEEP` ago, and staging left by an agent that died
/// while starting one. Failures leave the directory for the next sweep.
fn sweep(jobs: &Path, current: &Path) {
    let Ok(entries) = fs::read_dir(jobs) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path == current {
            continue;
        }
        let modified = |name: &str| fs::metadata(path.join(name)).and_then(|m| m.modified());
        let last = modified("result.json")
            .or_else(|_| modified("frames.jsonl"))
            .or_else(|_| entry.metadata().and_then(|m| m.modified()));
        let old = last
            .ok()
            .and_then(|time| time.elapsed().ok())
            .is_some_and(|age| age > KEEP);
        if old && !alive(&path) {
            let _ = fs::remove_dir_all(&path);
        }
    }
}

fn private_dir(dir: &Path) -> Result<()> {
    fs::create_dir_all(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}
fn private_file(path: &Path, append: bool) -> Result<File> {
    let mut options = OpenOptions::new();
    options.create(true);
    if append {
        options.append(true);
    } else {
        options.read(true).write(true).truncate(false);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    Ok(options.open(path)?)
}
