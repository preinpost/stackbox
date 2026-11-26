//! The golden image every stackbox VM starts from: a disk snapshot of a VM that booted a stock
//! distribution image and ran its setup script (`guest/os/<id>.sh`). msb builds it on each
//! host the first time a stackbox needs it, so hosts need no Docker and a changed recipe
//! simply produces a snapshot under a new name.

use std::time::Duration;

use anyhow::{Context, Result};
use microsandbox::snapshot::{Checkpoint, SnapshotReference};
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
    if available(&reference).await? {
        return Ok(reference);
    }
    let _build = BUILD.lock().await;
    if available(&reference).await? {
        return Ok(reference);
    }
    eprintln!("building golden image {reference} from {}", os.base_image);
    build(os, spec, &reference)
        .await
        .with_context(|| format!("build golden image {reference}"))?;
    Ok(reference)
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
        Checkpoint::builder(snapshot)
            .group(group)
            .from_sandbox(&name)
            .label("stackbox.golden.os", os.id)
            .label("stackbox.golden.base", os.base_image)
            .create()
            .await
            .map_err(|error| capture_failure(error, reference))?;
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
