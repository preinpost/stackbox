//! The golden image every stackbox VM starts from: a disk snapshot of a VM that booted a stock
//! distribution image and ran its setup script (`guest/os/<id>.sh`). msb builds it on each
//! host the first time a stackbox needs it, so hosts need no Docker and a changed recipe
//! simply produces a snapshot under a new name.

use std::fs::{File, OpenOptions};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use fs2::FileExt;
use microsandbox::snapshot::{Checkpoint, Snapshot, SnapshotReference};
use microsandbox::{MicrosandboxError, NetworkPolicy, NetworkProfile, Sandbox};
use sha2::{Digest, Sha256};

use super::node::{destroy_node, detach};
use super::template::{ALL_IN_ONE, NodeSpec};
use crate::shell::run;

/// A guest distribution stackboxes can run on.
#[derive(Debug)]
pub struct GuestOs {
    pub id: &'static str,
    /// Stock OCI image the golden VM boots from.
    pub base_image: &'static str,
    /// Run as root in that image, without init, to install the stackbox's packages.
    pub setup: &'static str,
}

pub const ROCKY10: GuestOs = GuestOs {
    id: "rocky10",
    base_image: "rockylinux/rockylinux:10",
    setup: include_str!("../../guest/os/rocky10.sh"),
};
/// New stackboxes run on this distribution.
pub const DEFAULT_OS: &GuestOs = &ROCKY10;

/// Bump to rebuild every host's golden image when the build itself changes.
const RECIPE_VERSION: &str = "1";
const BUILD_STOP_TIMEOUT: Duration = Duration::from_secs(60);
/// One build at a time per process; stackboxes created together wait for the first one.
static BUILD: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Held while one process builds or clears golden groups on this host. msb's group names and the
/// fixed build VM name are shared by every process using the same msb home, so a lock inside one
/// process cannot keep another stackbox (the daemon, a CLI run) from removing a group that is
/// being filled.
/// The file stays open, and so locked, until this guard drops.
struct BuildLock {
    _file: File,
}

/// Per OS, not per recipe: the build VM name `stackbox-golden-<os>-build` is shared by all recipes.
fn build_lock_path(os: &GuestOs) -> Result<PathBuf> {
    let home = microsandbox::config::config()
        .context("resolve the msb home for the golden build lock")?
        .home();
    Ok(home.join(format!("stackbox-golden-{}.lock", os.id)))
}

/// Block until this process holds the lock. Blocking: call it from a blocking task.
fn acquire_build_lock(path: &Path) -> Result<BuildLock> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    }
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path)
        .with_context(|| format!("open {}", path.display()))?;
    file.lock_exclusive()
        .with_context(|| format!("lock {}", path.display()))?;
    Ok(BuildLock { _file: file })
}

/// `stackbox-golden-<os>-<recipe hash>:<recipe hash>`. A stackbox VM's disks have the sizes of the
/// golden VM's, so they are part of the recipe.
pub fn reference(os: &GuestOs, spec: &NodeSpec) -> String {
    let recipe = recipe_hash(os, spec);
    format!("{}:{recipe}", group(os, &recipe))
}

fn recipe_hash(os: &GuestOs, spec: &NodeSpec) -> String {
    let mut hash = Sha256::new();
    for part in [
        RECIPE_VERSION,
        os.base_image,
        os.setup,
        &spec.disk_mib.to_string(),
        &format!("{:?}", spec.container_disk_mib),
    ] {
        hash.update(part.as_bytes());
        hash.update([0]);
    }
    let digest = hash.finalize();
    digest[..6].iter().map(|b| format!("{b:02x}")).collect()
}

/// One msb group per recipe. msb binds a group to the sandbox that first captured into it, and
/// each golden build boots a VM with a new sandbox id, so a group shared across recipes rejects
/// the next recipe's capture.
fn group(os: &GuestOs, recipe: &str) -> String {
    format!("stackbox-golden-{}-{recipe}", os.id)
}

/// The golden image new all-in-one stackboxes on this host start from.
pub fn current() -> String {
    reference(DEFAULT_OS, &ALL_IN_ONE)
}

pub async fn available(reference: &str) -> Result<bool> {
    match Checkpoint::open_ref(SnapshotReference::auto(reference)).await {
        Ok(_) => Ok(true),
        Err(MicrosandboxError::SnapshotNotFound(_)) => Ok(false),
        Err(error) => Err(error).with_context(|| format!("look up {reference}")),
    }
}

/// The golden snapshot for `spec` on `os`, built first if this host has none.
pub async fn ensure(os: &GuestOs, spec: &NodeSpec) -> Result<String> {
    let reference = reference(os, spec);
    ensure_with(
        &reference,
        || available(&reference),
        || async {
            // The in-process mutex first, so only one task per process blocks in the file lock.
            let build = BUILD.lock().await;
            let path = build_lock_path(os)?;
            let file = tokio::task::spawn_blocking(move || acquire_build_lock(&path))
                .await
                .context("join the golden build lock task")??;
            Ok((build, file))
        },
        || async {
            let (group, _) = reference
                .split_once(':')
                .context("golden reference has no group")?;
            clear_leftover_group(group).await;
            eprintln!("building golden image {reference} from {}", os.base_image);
            build(os, spec, &reference)
                .await
                .with_context(|| format!("build golden image {reference}"))
        },
    )
    .await
}

/// The check, lock, re-check, build sequence of `ensure`. The re-check after the lock finds an
/// image another process built while this one waited.
async fn ensure_with<G, Check, Lock, Build>(
    reference: &str,
    mut available: impl FnMut() -> Check,
    lock: impl FnOnce() -> Lock,
    build: impl FnOnce() -> Build,
) -> Result<String>
where
    Check: Future<Output = Result<bool>>,
    Lock: Future<Output = Result<G>>,
    Build: Future<Output = Result<()>>,
{
    if available().await? {
        return Ok(reference.to_owned());
    }
    let _lock = lock().await?;
    if available().await? {
        return Ok(reference.to_owned());
    }
    build().await?;
    Ok(reference.to_owned())
}

/// What stackbox needs from msb's snapshot groups. The cleanup decision is written against this
/// trait so it can be tested without a runtime.
trait SnapshotGroups {
    /// The number of snapshots in `group`, or `None` when no such group exists.
    fn member_count(&self, group: &str) -> impl Future<Output = Result<Option<u64>>> + Send;
    /// Remove the whole group. msb refuses while a capture holds the group's writer lease.
    fn remove(&self, group: &str) -> impl Future<Output = Result<()>> + Send;
}

/// msb's snapshot groups on this host.
struct MsbGroups;

impl SnapshotGroups for MsbGroups {
    async fn member_count(&self, group: &str) -> Result<Option<u64>> {
        match Snapshot::get(group).await {
            Ok(snapshot) => {
                let members = snapshot
                    .checkpoints()
                    .await
                    .with_context(|| format!("list the snapshots of {group}"))?;
                Ok(Some(members.len() as u64))
            }
            Err(MicrosandboxError::SnapshotNotFound(_)) => Ok(None),
            Err(error) => Err(error).with_context(|| format!("look up snapshot group {group}")),
        }
    }

    async fn remove(&self, group: &str) -> Result<()> {
        let snapshot = Snapshot::get(group)
            .await
            .with_context(|| format!("look up snapshot group {group}"))?;
        let removed = snapshot
            .remove()
            .await
            .with_context(|| format!("remove snapshot group {group}"))?;
        check_removed(group, removed.removed.len())
    }
}

/// `remove` deletes every member it reports, so a non-zero count means msb has already deleted
/// those snapshots. The error reports that; it cannot undo it.
fn check_removed(group: &str, removed: usize) -> Result<()> {
    ensure!(
        removed == 0,
        "snapshot group {group} held {removed} snapshot(s) when removed; msb already deleted them"
    );
    Ok(())
}

#[derive(Debug, PartialEq)]
enum Cleanup {
    /// No group under that name.
    Absent,
    /// The group holds snapshots, so it stays.
    Kept(u64),
    Removed,
    /// The group could not be read, so it stays: an unknown state is never removed.
    LookupFailed(String),
    RemoveFailed(String),
}

impl Cleanup {
    /// The line to log, or `None` when there was nothing to do.
    fn log_line(&self, group: &str) -> Option<String> {
        Some(match self {
            Cleanup::Absent => return None,
            Cleanup::Kept(count) => {
                format!("kept snapshot group {group}: it holds {count} snapshot(s)")
            }
            Cleanup::Removed => format!("removed empty snapshot group {group}"),
            Cleanup::LookupFailed(error) => {
                format!("left snapshot group {group} alone: lookup failed: {error}")
            }
            Cleanup::RemoveFailed(error) => format!(
                "could not remove empty snapshot group {group}: {error}; a writer may still hold it, and the next build retries the removal"
            ),
        })
    }
}

/// Remove `group` only when it exists and holds no snapshots. Any uncertainty keeps it.
async fn clear_empty_group(groups: &impl SnapshotGroups, group: &str) -> Cleanup {
    match groups.member_count(group).await {
        Ok(None) => Cleanup::Absent,
        Ok(Some(0)) => match groups.remove(group).await {
            Ok(()) => Cleanup::Removed,
            Err(error) => Cleanup::RemoveFailed(format!("{error:#}")),
        },
        Ok(Some(count)) => Cleanup::Kept(count),
        Err(error) => Cleanup::LookupFailed(format!("{error:#}")),
    }
}

/// The error for a failed capture into `group`. An empty group left behind is cleared first so
/// the same recipe can be built again. The returned error is always the capture error; the
/// cleanup outcome is only logged.
async fn fail_capture(
    groups: &impl SnapshotGroups,
    group: &str,
    error: MicrosandboxError,
    reference: &str,
) -> anyhow::Error {
    let cleanup = clear_empty_group(groups, group).await;
    if let Some(line) = cleanup.log_line(group) {
        eprintln!("{line}");
    }
    capture_failure(error, reference)
}

/// An empty group from an earlier failed capture is bound to a sandbox that no longer exists, so
/// the next capture into it is refused before it starts. Clear it before building.
async fn clear_leftover_group(group: &str) {
    if let Some(line) = clear_empty_group(&MsbGroups, group).await.log_line(group) {
        eprintln!("{line}");
    }
}

/// Explain a capture failure. A group bound to another sandbox instance is left by an earlier
/// golden build on this host; stackbox cannot clear it, so the message says so instead of
/// suggesting a plain retry. The msb error stays as the cause.
fn capture_failure(error: MicrosandboxError, reference: &str) -> anyhow::Error {
    let bound = matches!(
        &error,
        MicrosandboxError::InvalidConfig(message)
            if message.contains("bound to another sandbox instance")
    );
    let error = anyhow::Error::new(error);
    if bound {
        error.context(format!(
            "capture the golden snapshot {reference}: its msb snapshot group is bound to an earlier golden build on this host, so rebuilding does not clear it; remove that group in msb before building this recipe again"
        ))
    } else {
        error.context("capture the golden snapshot")
    }
}

async fn build(os: &GuestOs, spec: &NodeSpec, reference: &str) -> Result<()> {
    let name = format!("stackbox-golden-{}-build", os.id);
    // A build interrupted earlier leaves its VM behind.
    if let Ok(stale) = Sandbox::get(&name).await {
        destroy_node(&stale).await?;
    }
    let mut builder = Sandbox::builder(&name)
        .image(os.base_image)
        .cpus(2)
        .memory(2048)
        .root_disk(spec.disk_mib)
        .network(|n| n.policy(NetworkPolicy::from_profiles([NetworkProfile::Public])))
        .detached(true);
    // A VM made from a snapshot cannot add disks, so the golden VM carries the stackbox's
    // container disks. Docker's overlayfs cannot nest on msb's overlay root.
    if let Some(size) = spec.container_disk_mib {
        for dir in super::node::CONTAINER_DIRS {
            builder = builder.volume(*dir, |m| m.owned_with(|v| v.disk().size(size)));
        }
    }
    let built = async {
        let sandbox = builder.create().await.context("boot the base image")?;
        let setup = run(&sandbox, &format!("{}\nsync", os.setup)).await;
        let stopped = sandbox.stop_with_timeout(BUILD_STOP_TIMEOUT).await;
        detach(&sandbox).await;
        setup.context("run the setup script")?;
        stopped.context("stop the golden VM")?;
        let (group, snapshot) = reference
            .split_once(':')
            .context("golden reference has no group")?;
        let captured = Checkpoint::builder(snapshot)
            .group(group)
            .from_sandbox(&name)
            .label("stackbox.golden.os", os.id)
            .label("stackbox.golden.base", os.base_image)
            .create()
            .await;
        if let Err(error) = captured {
            return Err(fail_capture(&MsbGroups, group, error, reference).await);
        }
        Ok::<_, anyhow::Error>(())
    }
    .await;
    // The snapshot owns copies of the disks; the VM is no longer needed either way.
    if let Ok(handle) = Sandbox::get(&name).await {
        let removed = destroy_node(&handle).await;
        if built.is_ok() {
            removed?;
        }
    }
    built
}

#[cfg(test)]
mod tests {
    use super::*;

    fn group_of(reference: &str) -> &str {
        reference.split_once(':').unwrap().0
    }

    #[test]
    fn each_recipe_gets_its_own_group() {
        // msb binds a group to the sandbox that first captured into it, and a rebuilt golden VM
        // has a new sandbox id, so a changed recipe must not reuse the previous recipe's group.
        let base = reference(&ROCKY10, &ALL_IN_ONE);
        let other_setup = GuestOs {
            setup: "dnf install -y vim",
            ..ROCKY10
        };
        assert_ne!(
            group_of(&base),
            group_of(&reference(&other_setup, &ALL_IN_ONE))
        );
        let smaller = NodeSpec {
            container_disk_mib: Some(1024),
            ..ALL_IN_ONE
        };
        assert_ne!(group_of(&base), group_of(&reference(&ROCKY10, &smaller)));
        // Same recipe, same group: the cache lookup still finds the snapshot.
        assert_eq!(group_of(&base), group_of(&reference(&ROCKY10, &ALL_IN_ONE)));
    }

    #[test]
    fn a_group_bound_to_an_earlier_build_is_explained() {
        let golden = reference(&ROCKY10, &ALL_IN_ONE);
        let msb = MicrosandboxError::InvalidConfig(
            "snapshot group is bound to another sandbox instance (recorded name 'stackbox-golden-rocky10-build'); create a new group for 'stackbox-golden-rocky10-build'".into(),
        );
        let text = format!("{:#}", capture_failure(msb, &golden));
        assert!(text.contains(&golden), "{text}");
        assert!(text.contains("earlier golden build"), "{text}");
        // The msb message stays as the cause.
        assert!(text.contains("bound to another sandbox instance"), "{text}");

        let other = MicrosandboxError::InvalidConfig("disk full".into());
        let text = format!("{:#}", capture_failure(other, &golden));
        assert!(text.contains("disk full"), "{text}");
        assert!(!text.contains("earlier golden build"), "{text}");
    }

    #[test]
    fn recipe_changes_rename_the_golden_image() {
        let base = reference(&ROCKY10, &ALL_IN_ONE);
        assert!(base.starts_with("stackbox-golden-rocky10-"), "{base}");
        assert_eq!(base, current());
        let other_setup = GuestOs {
            setup: "dnf install -y vim",
            ..ROCKY10
        };
        assert_ne!(reference(&other_setup, &ALL_IN_ONE), base);
        let smaller = NodeSpec {
            container_disk_mib: Some(1024),
            ..ALL_IN_ONE
        };
        assert_ne!(reference(&ROCKY10, &smaller), base);
    }

    /// Scripted msb group state. Counters are atomics so the fake stays `Send + Sync`.
    struct FakeGroups {
        lookup: std::result::Result<Option<u64>, &'static str>,
        remove: std::result::Result<(), &'static str>,
        removals: std::sync::atomic::AtomicU32,
    }

    impl FakeGroups {
        fn new(
            lookup: std::result::Result<Option<u64>, &'static str>,
            remove: std::result::Result<(), &'static str>,
        ) -> Self {
            Self {
                lookup,
                remove,
                removals: std::sync::atomic::AtomicU32::new(0),
            }
        }

        fn removals(&self) -> u32 {
            self.removals.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    impl SnapshotGroups for FakeGroups {
        async fn member_count(&self, _group: &str) -> Result<Option<u64>> {
            self.lookup.map_err(anyhow::Error::msg)
        }

        async fn remove(&self, _group: &str) -> Result<()> {
            self.removals
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.remove.map_err(anyhow::Error::msg)
        }
    }

    #[tokio::test]
    async fn an_empty_group_is_removed() {
        let groups = FakeGroups::new(Ok(Some(0)), Ok(()));
        assert_eq!(clear_empty_group(&groups, "g").await, Cleanup::Removed);
        assert_eq!(groups.removals(), 1);
    }

    #[tokio::test]
    async fn a_group_with_snapshots_is_kept() {
        let groups = FakeGroups::new(Ok(Some(2)), Ok(()));
        assert_eq!(clear_empty_group(&groups, "g").await, Cleanup::Kept(2));
        assert_eq!(groups.removals(), 0);
    }

    #[tokio::test]
    async fn an_unreadable_group_is_kept() {
        // Fail safe: an uncertain lookup must never lead to a removal.
        let groups = FakeGroups::new(Err("connection reset"), Ok(()));
        let cleanup = clear_empty_group(&groups, "g").await;
        assert!(
            matches!(cleanup, Cleanup::LookupFailed(ref m) if m.contains("connection reset")),
            "{cleanup:?}"
        );
        assert_eq!(groups.removals(), 0);
    }

    #[tokio::test]
    async fn a_missing_group_needs_nothing() {
        let groups = FakeGroups::new(Ok(None), Ok(()));
        assert_eq!(clear_empty_group(&groups, "g").await, Cleanup::Absent);
        assert_eq!(groups.removals(), 0);
    }

    #[tokio::test]
    async fn a_failed_removal_is_reported() {
        let groups = FakeGroups::new(Ok(Some(0)), Err("active writer"));
        let cleanup = clear_empty_group(&groups, "g").await;
        assert!(
            matches!(cleanup, Cleanup::RemoveFailed(ref m) if m.contains("active writer")),
            "{cleanup:?}"
        );
        assert_eq!(groups.removals(), 1);
    }

    #[tokio::test]
    async fn a_failed_cleanup_keeps_the_capture_error() {
        let groups = FakeGroups::new(Ok(Some(0)), Err("active writer"));
        let error = fail_capture(
            &groups,
            "g",
            MicrosandboxError::InvalidConfig("disk full".into()),
            "g:h",
        )
        .await;
        let text = format!("{error:#}");
        assert!(text.contains("disk full"), "{text}");
        assert!(text.contains("capture the golden snapshot"), "{text}");
        assert!(!text.contains("active writer"), "{text}");
        assert_eq!(groups.removals(), 1);
    }

    #[tokio::test]
    async fn a_successful_cleanup_still_returns_the_capture_error() {
        let groups = FakeGroups::new(Ok(Some(0)), Ok(()));
        let error = fail_capture(
            &groups,
            "g",
            MicrosandboxError::InvalidConfig("disk full".into()),
            "g:h",
        )
        .await;
        let text = format!("{error:#}");
        assert!(text.contains("disk full"), "{text}");
        assert!(text.contains("capture the golden snapshot"), "{text}");
        assert_eq!(groups.removals(), 1);
    }

    #[test]
    fn a_failed_cleanup_says_the_next_build_retries() {
        let line = Cleanup::RemoveFailed("active writer".into())
            .log_line("g")
            .unwrap();
        assert!(line.contains("the next build retries"), "{line}");
    }

    #[test]
    fn a_removal_that_deleted_members_says_so() {
        let text = format!("{:#}", check_removed("g", 2).unwrap_err());
        assert!(text.contains("2 snapshot(s)"), "{text}");
        assert!(text.contains("deleted"), "{text}");
        assert!(check_removed("g", 0).is_ok());
    }

    #[test]
    fn the_build_lock_serializes_holders() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("golden.lock");
        let first = acquire_build_lock(&path).unwrap();
        let released = Arc::new(AtomicBool::new(false));
        let waiter = {
            let path = path.clone();
            let released = released.clone();
            std::thread::spawn(move || {
                let _second = acquire_build_lock(&path).unwrap();
                assert!(
                    released.load(Ordering::SeqCst),
                    "second holder entered while the first still held the lock"
                );
            })
        };
        std::thread::sleep(Duration::from_millis(200));
        released.store(true, Ordering::SeqCst);
        drop(first);
        waiter.join().unwrap();
    }

    #[tokio::test]
    async fn a_cached_image_skips_the_lock() {
        let locks = std::cell::Cell::new(0);
        let built = ensure_with(
            "g:h",
            || async { anyhow::Ok(true) },
            || async {
                locks.set(locks.get() + 1);
                anyhow::Ok(())
            },
            || async { panic!("nothing to build") },
        )
        .await
        .unwrap();
        assert_eq!(built, "g:h");
        assert_eq!(locks.get(), 0);
    }

    #[tokio::test]
    async fn an_image_built_while_waiting_is_reused() {
        // Another stackbox process finished the same recipe while this one waited for the lock.
        let answers = std::cell::RefCell::new(vec![false, true]);
        let built = std::cell::Cell::new(false);
        let reference = ensure_with(
            "g:h",
            || async { anyhow::Ok(answers.borrow_mut().remove(0)) },
            || async { anyhow::Ok(()) },
            || async {
                built.set(true);
                anyhow::Ok(())
            },
        )
        .await
        .unwrap();
        assert_eq!(reference, "g:h");
        assert!(!built.get(), "a second build of the same recipe started");
    }

    #[tokio::test]
    async fn a_missing_image_is_built_under_the_lock() {
        let events = std::cell::RefCell::new(Vec::new());
        ensure_with(
            "g:h",
            || async { anyhow::Ok(false) },
            || async {
                events.borrow_mut().push("lock");
                anyhow::Ok(())
            },
            || async {
                events.borrow_mut().push("build");
                anyhow::Ok(())
            },
        )
        .await
        .unwrap();
        assert_eq!(*events.borrow(), ["lock", "build"]);
    }

    #[test]
    fn cleanup_outcomes_have_log_lines_only_when_something_happened() {
        assert!(Cleanup::Absent.log_line("g").is_none());
        assert!(Cleanup::Removed.log_line("g").unwrap().contains("removed"));
        assert!(Cleanup::Kept(1).log_line("g").unwrap().contains("kept"));
        assert!(
            Cleanup::RemoveFailed("x".into())
                .log_line("g")
                .unwrap()
                .contains("x")
        );
    }

    #[test]
    fn setup_scripts_parse() {
        let output = std::process::Command::new("sh")
            .args(["-n", "-c", ROCKY10.setup])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
