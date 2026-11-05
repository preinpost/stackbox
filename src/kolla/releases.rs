//! The OpenStack releases this coordinator can install: the one built into the binary, plus
//! the stable branches the public kolla-ansible repository publishes. A release with no
//! template of its own is installed clean from its branch, so the choice is not limited to
//! the build's own release.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

/// Branches of the public kolla-ansible repository. A release keeps `stable/` while it is
/// maintained and moves to `unmaintained/` afterwards; both install the same way.
const BRANCHES: &str =
    "https://opendev.org/api/v1/repos/openstack/kolla-ansible/branches?limit=100";
/// How long a fetched list is reused. The repository gains a branch twice a year.
const TTL: Duration = Duration::from_secs(600);

static CACHE: Mutex<Option<(Instant, Vec<String>)>> = Mutex::new(None);

/// Releases that can be installed, newest first. Falls back to this build's release when the
/// repository cannot be read, so a farm can always be created.
pub async fn published() -> Vec<String> {
    if let Some(list) = configured() {
        return list;
    }
    // Tests never reach the network; `FARM_OPENSTACK_RELEASES` overrides this for air-gapped
    // hosts, which keep working with the release built into the binary.
    if cfg!(test) {
        return vec![super::OPENSTACK_RELEASE.to_owned()];
    }
    if let Some(list) = cached() {
        return list;
    }
    let list = match fetch().await {
        Ok(list) if !list.is_empty() => list,
        Ok(_) => vec![super::OPENSTACK_RELEASE.to_owned()],
        Err(error) => {
            eprintln!("cannot list the published OpenStack releases: {error:#}");
            vec![super::OPENSTACK_RELEASE.to_owned()]
        }
    };
    if let Ok(mut cache) = CACHE.lock() {
        *cache = Some((Instant::now(), list.clone()));
    }
    list
}

/// Whether `release` is one this coordinator can install clean.
pub fn installable(release: &str) -> bool {
    published_sync().iter().any(|known| known == release)
}

/// The list without waiting: the configured one, the cached one, else this build's release.
fn published_sync() -> Vec<String> {
    configured()
        .or_else(cached)
        .unwrap_or_else(|| vec![super::OPENSTACK_RELEASE.to_owned()])
}

fn cached() -> Option<Vec<String>> {
    let (at, list) = CACHE.lock().ok()?.clone()?;
    (at.elapsed() < TTL).then_some(list)
}

/// `FARM_OPENSTACK_RELEASES=2026.2,2025.2` pins the list, for hosts without the repository.
fn configured() -> Option<Vec<String>> {
    let value = std::env::var("FARM_OPENSTACK_RELEASES").ok()?;
    let list = normalize(value.split(',').map(str::trim));
    (!list.is_empty()).then_some(list)
}

async fn fetch() -> Result<Vec<String>> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(8))
        .build()
        .context("http client")?;
    let response = client
        .get(BRANCHES)
        .header("accept", "application/json")
        .send()
        .await
        .context("kolla-ansible branches")?
        .error_for_status()
        .context("kolla-ansible branches")?;
    let body: serde_json::Value = response.json().await.context("kolla-ansible branches")?;
    Ok(parse(&body))
}

/// Names of the `stable/<release>` and `unmaintained/<release>` branches in a Gitea listing.
pub fn parse(body: &serde_json::Value) -> Vec<String> {
    let names = body
        .as_array()
        .map(|branches| {
            branches
                .iter()
                .filter_map(|branch| branch.get("name")?.as_str())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    normalize(names.into_iter().filter_map(|name| {
        name.strip_prefix("stable/")
            .or_else(|| name.strip_prefix("unmaintained/"))
    }))
}

/// Keeps year.release names (`2026.2`), newest first, and always offers this build's release.
fn normalize<'a>(releases: impl Iterator<Item = &'a str>) -> Vec<String> {
    let mut list: Vec<String> = releases
        .filter(|release| {
            let (year, number) = release.split_once('.').unwrap_or_default();
            year.len() == 4
                && year.bytes().all(|c| c.is_ascii_digit())
                && number.len() == 1
                && number.bytes().all(|c| c.is_ascii_digit())
        })
        .map(str::to_owned)
        .collect();
    list.sort();
    list.dedup();
    list.reverse();
    if !list.iter().any(|r| r == super::OPENSTACK_RELEASE) {
        list.push(super::OPENSTACK_RELEASE.to_owned());
    }
    list
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_stable_release_branches_newest_first_and_the_builds_own_release() {
        let body = serde_json::json!([
            { "name": "master" },
            { "name": "stable/2025.2" },
            { "name": "stable/2026.1" },
            { "name": "stable/feature" },
            { "name": "stable/2026.2" },
            { "name": "unmaintained/2024.1" },
        ]);
        let list = parse(&body);
        assert_eq!(
            list,
            [
                super::super::OPENSTACK_RELEASE,
                "2026.1",
                "2025.2",
                "2024.1"
            ]
        );
        // A repository without our own release still offers it.
        assert!(
            parse(&serde_json::json!([{ "name": "stable/2024.1" }]))
                .contains(&super::super::OPENSTACK_RELEASE.to_owned())
        );
        // Anything that is not a year.release branch is ignored.
        assert_eq!(
            parse(&serde_json::json!({ "message": "no" })),
            vec![super::super::OPENSTACK_RELEASE.to_owned()]
        );
    }
}
