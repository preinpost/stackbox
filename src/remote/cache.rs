//! Snapshots a coordinator copied to this host to boot templates from. An archive arrives
//! in a staging file, from a presigned bucket URL or streamed by the coordinator, and is
//! loaded only once its size and SHA-256 match what the coordinator expects. The loaded
//! copies are remembered per coordinator in `cache.json`, so `FindSnapshot` can answer
//! for them. A snapshot frozen on this host leaves the other way: exported into a staging
//! file, then streamed to the coordinator, which checks its size and SHA-256.

use super::{
    agent::{Emitter, lock},
    protocol::{Frame, MAX_FRAME, TerminalFrame},
};
use crate::{
    farm::{ImportedSnapshot, NodeSnapshot},
    manager::Backend,
};
use anyhow::{Context, Result, anyhow, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Cached {
    reference: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    nodes: Vec<NodeSnapshot>,
    sha256: String,
    size_bytes: u64,
}
/// Cached copies by coordinator, then by snapshot ID.
#[derive(Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Cache {
    snapshots: BTreeMap<String, BTreeMap<String, Cached>>,
}

/// Progress lines at most every this many bytes.
const REPORT_EVERY: u64 = 256 * 1024 * 1024;

fn read(dir: &Path) -> Result<Cache> {
    match std::fs::read(dir.join("cache.json")) {
        Ok(bytes) => serde_json::from_slice(&bytes).context("invalid template cache record"),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Cache::default()),
        Err(e) => Err(e.into()),
    }
}
fn write(dir: &Path, cache: &Cache) -> Result<()> {
    let path = dir.join("cache.json.tmp");
    std::fs::write(&path, serde_json::to_vec(cache)?)?;
    std::fs::rename(path, dir.join("cache.json"))?;
    Ok(())
}
/// Change the record under its lock, waiting briefly for another request holding it.
async fn update<T>(dir: &Path, f: impl FnOnce(&mut Cache) -> Result<T>) -> Result<T> {
    let mut tries = 0;
    let _lock = loop {
        match lock(dir, "cache.lock") {
            Ok(lock) => break lock,
            Err(_) if tries < 50 => {
                tries += 1;
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
            Err(error) => return Err(error),
        }
    };
    let mut cache = read(dir)?;
    let result = f(&mut cache)?;
    write(dir, &cache)?;
    Ok(result)
}

fn staging(dir: &Path, transfer_id: &str) -> Result<(PathBuf, PathBuf)> {
    uuid::Uuid::parse_str(transfer_id).context("invalid transfer id")?;
    let dir = dir.join("transfers");
    std::fs::create_dir_all(&dir)?;
    Ok((
        dir.join(format!("{transfer_id}.partial")),
        dir.join(format!("{transfer_id}.msb")),
    ))
}
fn check_digest(sha256: &str) -> Result<()> {
    ensure!(
        sha256.len() == 64 && sha256.bytes().all(|b| b.is_ascii_hexdigit()),
        "invalid SHA-256"
    );
    Ok(())
}
/// Publish a received archive under its final name once it is what was promised.
fn verify(
    partial: &Path,
    done: &Path,
    size: u64,
    sha256: &str,
    expected: (&str, u64),
) -> Result<()> {
    let (want_sha, want_size) = expected;
    if size != want_size || !sha256.eq_ignore_ascii_case(want_sha) {
        let _ = std::fs::remove_file(partial);
        return Err(anyhow!(
            "the received archive has {size} bytes with SHA-256 {sha256}, but {want_size} bytes with SHA-256 {want_sha} were expected; it is not imported"
        ));
    }
    std::fs::rename(partial, done)?;
    Ok(())
}
fn progress(emit: &Emitter, what: &str, done: u64, total: u64, reported: &mut u64) {
    if done >= *reported + REPORT_EVERY || (done == total && done != *reported) {
        *reported = done;
        let _ = emit(Frame::Output {
            message: format!(
                "{what}: {} of {} MiB\n",
                done / (1024 * 1024),
                total / (1024 * 1024)
            ),
        });
    }
}

/// The reference (and per-VM references for a multi-node copy) of `coordinator`'s cached
/// copy of `snapshot_id`, if msb still has every member. A copy msb lost is forgotten.
pub async fn find(
    dir: &Path,
    backend: &dyn Backend,
    coordinator: &str,
    snapshot_id: &str,
) -> Result<Option<(String, Vec<NodeSnapshot>)>> {
    crate::model::validate_snapshot_id(snapshot_id)?;
    let Some(cached) = read(dir)?
        .snapshots
        .get(coordinator)
        .and_then(|snapshots| snapshots.get(snapshot_id))
        .cloned()
    else {
        return Ok(None);
    };
    let present = if cached.nodes.is_empty() {
        backend
            .snapshot_labels(cached.reference.clone())
            .await?
            .is_some()
    } else {
        let mut all = true;
        for node in &cached.nodes {
            if backend
                .snapshot_labels(node.reference.clone())
                .await?
                .is_none()
            {
                all = false;
                break;
            }
        }
        all
    };
    if present {
        return Ok(Some((cached.reference, cached.nodes)));
    }
    update(dir, |cache| {
        if let Some(snapshots) = cache.snapshots.get_mut(coordinator) {
            snapshots.remove(snapshot_id);
        }
        Ok(())
    })
    .await?;
    Ok(None)
}

pub async fn fetch(
    dir: &Path,
    transfer_id: &str,
    url: &str,
    ca_pem: Option<&str>,
    expected: (&str, u64),
    emit: &Emitter,
) -> Result<()> {
    check_digest(expected.0)?;
    let (partial, done) = staging(dir, transfer_id)?;
    let url = reqwest::Url::parse(url).map_err(|_| anyhow!("invalid archive URL"))?;
    let mut reported = 0;
    let fetched = crate::s3::client::fetch(&url, ca_pem, &partial, |bytes, _| {
        progress(
            emit,
            "downloaded from the bucket",
            bytes,
            expected.1,
            &mut reported,
        )
    })
    .await;
    let fetched = match fetched {
        Ok(fetched) => fetched,
        Err(error) => {
            let _ = std::fs::remove_file(&partial);
            return Err(error);
        }
    };
    verify(&partial, &done, fetched.size, &fetched.sha256, expected)
}

/// Write the archive the coordinator streams as `TerminalFrame::Input` lines until EOF.
pub async fn receive<R>(
    dir: &Path,
    transfer_id: &str,
    expected: (&str, u64),
    input: R,
) -> Result<()>
where
    R: tokio::io::AsyncBufRead + Unpin,
{
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};
    check_digest(expected.0)?;
    let (partial, done) = staging(dir, transfer_id)?;
    let written = async {
        let mut file = tokio::fs::File::create(&partial).await?;
        let mut hash = Sha256::new();
        let mut size = 0u64;
        let mut input = input;
        let mut line = String::new();
        loop {
            line.clear();
            let read = (&mut input)
                .take(MAX_FRAME as u64 + 1)
                .read_line(&mut line)
                .await?;
            if read == 0 {
                break;
            }
            ensure!(line.len() <= MAX_FRAME, "archive frame exceeds limit");
            let TerminalFrame::Input { data } =
                serde_json::from_str(&line).context("invalid archive frame")?
            else {
                anyhow::bail!("unexpected frame in an archive stream");
            };
            let data = STANDARD.decode(data).context("invalid archive data")?;
            size += data.len() as u64;
            ensure!(size <= expected.1, "the archive is longer than announced");
            hash.update(&data);
            file.write_all(&data).await?;
        }
        file.sync_all().await?;
        Ok::<_, anyhow::Error>((size, format!("{:x}", hash.finalize())))
    }
    .await;
    let (size, sha256) = match written {
        Ok(written) => written,
        Err(error) => {
            let _ = std::fs::remove_file(&partial);
            return Err(error);
        }
    };
    verify(&partial, &done, size, &sha256, expected)
}

/// Load a verified staging file as `snapshot_id` and remember it for `coordinator`.
pub async fn import(
    dir: &Path,
    backend: &dyn Backend,
    coordinator: &str,
    transfer_id: &str,
    snapshot_id: &str,
) -> Result<ImportedSnapshot> {
    crate::model::validate_snapshot_id(snapshot_id)?;
    let (_, archive) = staging(dir, transfer_id)?;
    ensure!(
        archive.is_file(),
        "transfer {transfer_id} has no verified archive on this host"
    );
    // Another coordinator's copy of the same ID would share its msb group.
    let taken = read(dir)?
        .snapshots
        .iter()
        .any(|(owner, snapshots)| owner != coordinator && snapshots.contains_key(snapshot_id));
    if taken {
        let _ = std::fs::remove_file(&archive);
        anyhow::bail!("another coordinator keeps a snapshot '{snapshot_id}' on this host");
    }
    let size_bytes = std::fs::metadata(&archive)?.len();
    let sha256 = {
        let archive = archive.clone();
        tokio::task::spawn_blocking(move || -> Result<String> {
            let mut file = std::fs::File::open(archive)?;
            let mut hash = Sha256::new();
            std::io::copy(&mut file, &mut hash)?;
            Ok(format!("{:x}", hash.finalize()))
        })
        .await??
    };
    let is_bundle = {
        let archive = archive.clone();
        tokio::task::spawn_blocking(move || crate::manager::bundle::is_bundle(&archive)).await??
    };
    let imported = if is_bundle {
        import_bundle(dir, backend, transfer_id, &archive, snapshot_id).await
    } else {
        backend
            .import_snapshot(archive.clone(), snapshot_id.into())
            .await
    };
    let _ = std::fs::remove_file(&archive);
    let imported = imported?;
    update(dir, |cache| {
        cache
            .snapshots
            .entry(coordinator.into())
            .or_default()
            .insert(
                snapshot_id.into(),
                Cached {
                    reference: imported.reference.clone(),
                    nodes: imported.nodes.clone(),
                    sha256,
                    size_bytes,
                },
            );
        Ok(())
    })
    .await?;
    Ok(imported)
}

async fn import_bundle(
    dir: &Path,
    backend: &dyn Backend,
    transfer_id: &str,
    archive: &Path,
    snapshot_id: &str,
) -> Result<ImportedSnapshot> {
    let stage_dir = dir
        .join("transfers")
        .join(format!("{transfer_id}.bundle-import"));
    let _ = std::fs::remove_dir_all(&stage_dir);
    let loaded = async {
        let extracted = {
            let archive = archive.to_owned();
            let dest = stage_dir.clone();
            tokio::task::spawn_blocking(move || crate::manager::bundle::unpack(&archive, &dest))
                .await??
        };
        let mut nodes: Vec<NodeSnapshot> = Vec::with_capacity(extracted.nodes.len());
        let mut control_labels = None;
        for (node, node_path) in &extracted.nodes {
            let imported = match backend
                .import_snapshot(node_path.clone(), snapshot_id.into())
                .await
                .with_context(|| format!("import VM '{node}' of bundle {snapshot_id}"))
            {
                Ok(imported) => imported,
                Err(error) => {
                    for loaded in nodes.iter().rev() {
                        let _ = backend.remove_snapshot(loaded.reference.clone()).await;
                    }
                    return Err(error);
                }
            };
            if node == "control" {
                control_labels = Some(imported.labels);
            }
            nodes.push(NodeSnapshot {
                node: node.clone(),
                reference: imported.reference,
                size_bytes: imported.size_bytes,
            });
        }
        let Some(labels) = control_labels else {
            for loaded in nodes.iter().rev() {
                let _ = backend.remove_snapshot(loaded.reference.clone()).await;
            }
            anyhow::bail!("multi-node bundle has no control VM snapshot");
        };
        nodes.sort_by_key(|n| {
            let index = n
                .node
                .strip_prefix("compute-")
                .and_then(|s| s.parse::<u8>().ok())
                .unwrap_or(0);
            (n.node != "control", index)
        });
        let reference = nodes[0].reference.clone();
        let size_bytes = nodes.iter().map(|n| n.size_bytes).sum();
        Ok(ImportedSnapshot {
            reference,
            labels,
            size_bytes,
            nodes,
        })
    }
    .await;
    let _ = std::fs::remove_dir_all(&stage_dir);
    loaded
}

/// Export the snapshot at `reference` into transfer `transfer_id`'s staging file, for
/// [`send`]. Returns its SHA-256 and size.
pub async fn export(
    dir: &Path,
    backend: &dyn Backend,
    transfer_id: &str,
    reference: &str,
) -> Result<(String, u64)> {
    let (partial, done) = staging(dir, transfer_id)?;
    let _ = std::fs::remove_file(&partial);
    if let Err(error) = backend
        .export_snapshot(reference.into(), partial.clone())
        .await
    {
        let _ = std::fs::remove_file(&partial);
        return Err(error);
    }
    std::fs::rename(&partial, &done)?;
    let hashed = tokio::task::spawn_blocking({
        let done = done.clone();
        move || -> Result<(String, u64)> {
            let mut file = std::fs::File::open(done)?;
            let mut hash = Sha256::new();
            let size = std::io::copy(&mut file, &mut hash)?;
            Ok((format!("{:x}", hash.finalize()), size))
        }
    })
    .await?;
    if hashed.is_err() {
        let _ = std::fs::remove_file(&done);
    }
    hashed
}

/// Stream transfer `transfer_id`'s exported archive as `Frame::Terminal` output, then
/// delete it whether or not it all went out.
pub async fn send(dir: &Path, transfer_id: &str, emit: &Emitter) -> Result<()> {
    use tokio::io::AsyncReadExt;
    // Base64 in a JSON line must stay under the frame limit.
    const CHUNK: usize = 96 * 1024;
    let (_, done) = staging(dir, transfer_id)?;
    ensure!(
        done.is_file(),
        "transfer {transfer_id} has no exported archive on this host"
    );
    let sent = async {
        let mut file = tokio::fs::File::open(&done).await?;
        let mut chunk = vec![0; CHUNK];
        loop {
            let read = file.read(&mut chunk).await?;
            if read == 0 {
                return Ok::<_, anyhow::Error>(());
            }
            emit(Frame::Terminal {
                data: STANDARD.encode(&chunk[..read]),
            })?;
        }
    }
    .await;
    let _ = std::fs::remove_file(&done);
    sent
}

/// Forget cached copies that reference `reference` after it was removed.
pub async fn forget(dir: &Path, reference: &str) -> Result<()> {
    if !dir.join("cache.json").exists() {
        return Ok(());
    }
    update(dir, |cache| {
        for snapshots in cache.snapshots.values_mut() {
            snapshots.retain(|_, cached| {
                cached.reference != reference
                    && !cached.nodes.iter().any(|n| n.reference == reference)
            });
        }
        cache.snapshots.retain(|_, snapshots| !snapshots.is_empty());
        Ok(())
    })
    .await
}
