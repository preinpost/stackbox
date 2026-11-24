use crate::{
    kolla::Phase,
    model::{LogEntry, now},
};
use anyhow::Result;
use std::{
    collections::{BTreeMap, VecDeque},
    fs::{self, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
};
const RETAIN: usize = 1000;
const TAIL_BYTES: u64 = 4 * 1024 * 1024;
#[derive(Default)]
struct Tail {
    entries: VecDeque<LogEntry>,
    last_id: u64,
}
pub struct Logs {
    dir: PathBuf,
    tails: BTreeMap<String, Tail>,
}
impl Logs {
    pub fn new(dir: &Path) -> Result<Self> {
        let dir = dir.join("logs");
        fs::create_dir_all(&dir)?;
        Ok(Self {
            dir,
            tails: BTreeMap::new(),
        })
    }
    fn load(&mut self, stackbox: &str) -> Result<&mut Tail> {
        if !self.tails.contains_key(stackbox) {
            let path = self.dir.join(format!("{stackbox}.jsonl"));
            let mut tail = Tail::default();
            match OpenOptions::new().read(true).write(true).open(path) {
                Ok(mut file) => {
                    let size = file.metadata()?.len();
                    let offset = size.saturating_sub(TAIL_BYTES);
                    file.seek(SeekFrom::Start(offset))?;
                    let mut bytes = Vec::new();
                    file.read_to_end(&mut bytes)?;
                    // Discard an incomplete write left by a killed daemon before appending.
                    let end = bytes.iter().rposition(|b| *b == b'\n').map_or(0, |i| i + 1);
                    file.set_len(offset + end as u64)?;
                    let start = if offset > 0 {
                        bytes
                            .iter()
                            .position(|b| *b == b'\n')
                            .map_or(end, |i| i + 1)
                    } else {
                        0
                    };
                    for line in bytes[start..end].split(|b| *b == b'\n') {
                        if let Ok(entry) = serde_json::from_slice::<LogEntry>(line) {
                            tail.last_id = entry.id;
                            tail.entries.push_back(entry);
                            if tail.entries.len() > RETAIN {
                                tail.entries.pop_front();
                            }
                        }
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
            self.tails.insert(stackbox.to_owned(), tail);
        }
        Ok(self.tails.get_mut(stackbox).unwrap())
    }
    pub fn append(
        &mut self,
        stackbox: &str,
        job: &str,
        phase: Option<Phase>,
        message: String,
    ) -> Result<()> {
        // Limit individual events so reconnect tails always contain complete recent entries.
        for chunk in message.as_bytes().chunks(16 * 1024) {
            let id = self.load(stackbox)?.last_id + 1;
            let entry = LogEntry {
                id,
                timestamp: now(),
                job_id: job.into(),
                phase,
                message: String::from_utf8_lossy(chunk).into_owned(),
            };
            let mut options = OpenOptions::new();
            options.create(true).append(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let mut file = options.open(self.dir.join(format!("{stackbox}.jsonl")))?;
            let mut bytes = serde_json::to_vec(&entry)?;
            bytes.push(b'\n');
            file.write_all(&bytes)?;
            let tail = self.load(stackbox)?;
            tail.last_id = id;
            tail.entries.push_back(entry);
            if tail.entries.len() > RETAIN {
                tail.entries.pop_front();
            }
        }
        Ok(())
    }
    pub fn tail(&mut self, stackbox: &str, after: u64, limit: usize) -> Result<Vec<LogEntry>> {
        Ok(self
            .load(stackbox)?
            .entries
            .iter()
            .filter(|e| e.id > after)
            .take(limit.min(RETAIN))
            .cloned()
            .collect())
    }
}
