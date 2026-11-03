use crate::model::{FarmRecord, Job, SnapshotRecord};
use anyhow::{Context, Result, ensure};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
};

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct State {
    pub version: u32,
    #[serde(default = "coordinator_id")]
    pub coordinator_id: String,
    pub farms: BTreeMap<String, FarmRecord>,
    pub jobs: BTreeMap<String, Job>,
    #[serde(default)]
    pub hosts: BTreeMap<String, crate::remote::StoredHost>,
    #[serde(default)]
    pub host_jobs: BTreeMap<String, crate::remote::HostJob>,
    #[serde(default)]
    pub snapshots: BTreeMap<String, SnapshotRecord>,
    #[serde(default)]
    pub buckets: BTreeMap<String, crate::s3::StoredBucket>,
    /// Multipart uploads to abort, by upload ID.
    #[serde(default)]
    pub uploads: BTreeMap<String, crate::s3::PendingUpload>,
    /// Where farms that span hosts join; none disables them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relay: Option<crate::link::relay::RelaySettings>,
    /// Each farm that spans hosts' uplink keys, by host, sealed by the vault.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub relay_keys: BTreeMap<String, BTreeMap<String, crate::remote::SealedCredentials>>,
}
impl Default for State {
    fn default() -> Self {
        Self {
            version: 1,
            coordinator_id: coordinator_id(),
            farms: BTreeMap::new(),
            jobs: BTreeMap::new(),
            hosts: BTreeMap::new(),
            host_jobs: BTreeMap::new(),
            snapshots: BTreeMap::new(),
            buckets: BTreeMap::new(),
            uploads: BTreeMap::new(),
            relay: None,
            relay_keys: BTreeMap::new(),
        }
    }
}
fn coordinator_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

pub struct Store {
    pub dir: PathBuf,
    pub state: State,
    pub revision: u64,
    _lock: File,
}
impl Store {
    pub fn open(dir: &Path) -> Result<Self> {
        fs::create_dir_all(dir)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
        }
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(dir.join("state.lock"))?;
        lock.try_lock_exclusive().context("farm state is locked by another process; use the running daemon's API or stop it before using the CLI")?;
        let path = dir.join("state.json");
        let state: State = match fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .context("invalid farm state.json; refusing to overwrite it")?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => State::default(),
            Err(e) => return Err(e.into()),
        };
        ensure!(
            state.version == 1,
            "unsupported state version {}",
            state.version
        );
        Ok(Self {
            dir: dir.into(),
            state,
            revision: 0,
            _lock: lock,
        })
    }
    /// Persist before publishing the in-memory change. Failed writes leave the previous state intact.
    pub fn update<T>(&mut self, f: impl FnOnce(&mut State) -> Result<T>) -> Result<T> {
        let mut next = self.state.clone();
        let result = f(&mut next)?;
        let path = self.dir.join("state.json.tmp");
        let mut options = OpenOptions::new();
        options.create(true).truncate(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&path)?;
        file.write_all(&serde_json::to_vec_pretty(&next)?)?;
        file.sync_all()?;
        fs::rename(path, self.dir.join("state.json"))?;
        File::open(&self.dir)?.sync_all()?;
        self.state = next;
        self.revision += 1;
        Ok(result)
    }
}
