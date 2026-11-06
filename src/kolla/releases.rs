//! The OpenStack releases this coordinator can install: the one built into the binary, the
//! stable branches the public kolla-ansible repository still publishes, and the end-of-life
//! series whose branches are gone but whose last tags remain. A release with no template of
//! its own is installed clean from its branch or tag, so the choice is not limited to the
//! build's own release.

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

/// Branches of the public kolla-ansible repository. A release keeps `stable/` while it is
/// maintained and moves to `unmaintained/` afterwards; both install the same way.
const BRANCHES: &str =
    "https://opendev.org/api/v1/repos/openstack/kolla-ansible/branches?limit=100";
/// How long a fetched list is reused. The repository gains a branch twice a year.
const TTL: Duration = Duration::from_secs(600);

static CACHE: Mutex<Option<(Instant, BTreeMap<String, String>)>> = Mutex::new(None);

/// A series kolla-ansible no longer keeps a branch for. Its last tag installs the same way,
/// but the guest needs an older Python and its images belong to an older Kolla.
pub struct Legacy {
    /// What a farm records, `2021.1`.
    pub release: &'static str,
    /// The series name: the image tag and the constraints name, `wallaby`.
    pub series: &'static str,
    /// Last kolla-ansible tag of the series.
    pub kolla: &'static str,
    /// Python minor the release's Ansible runs on; the guest's own 3.12 is too new.
    pub python: &'static str,
}

/// End-of-life series, newest first. Every one of their kolla-ansible branches is deleted;
/// the tags below are the last release each series had (`openstack/releases` deliverables).
/// Their images are `kolla/<distro>-source-*:<series>` and published for amd64 only, so only
/// an x86_64 host can run them.
pub const LEGACY: &[Legacy] = &[
    Legacy {
        release: "2022.2",
        series: "zed",
        kolla: "15.6.0",
        python: "3.10",
    },
    Legacy {
        release: "2022.1",
        series: "yoga",
        kolla: "14.11.0",
        python: "3.10",
    },
    Legacy {
        release: "2021.2",
        series: "xena",
        kolla: "13.9.0",
        python: "3.9",
    },
    Legacy {
        release: "2021.1",
        series: "wallaby",
        kolla: "12.8.0",
        python: "3.9",
    },
    Legacy {
        release: "2020.2",
        series: "victoria",
        kolla: "11.4.0",
        python: "3.9",
    },
    Legacy {
        release: "2020.1",
        series: "ussuri",
        kolla: "10.4.0",
        python: "3.8",
    },
];

/// What installing one release takes: which kolla-ansible ref, which Python, which images.
#[derive(Clone, Debug, PartialEq)]
pub struct ReleaseInfo {
    /// The name a farm records, `2021.1` or `2026.2`.
    pub release: String,
    /// Series name: the image tag and the upper-constraints name.
    pub series: String,
    /// kolla-ansible branch (`stable/2026.2`) or tag (`12.8.0`) to install.
    pub reference: String,
    /// Base distro of its images, `rocky` now and `ubuntu` for the legacy series.
    pub base_distro: String,
    /// Python minor its tooling runs on, `3.9`; the guest ships 3.12.
    pub python: String,
    /// No maintained branch is left: the series is end of life.
    pub eol: bool,
    /// Its images are published for aarch64.
    pub arm64: bool,
}

/// The legacy entry for `release`, if the series is end of life.
pub fn legacy(release: &str) -> Option<&'static Legacy> {
    LEGACY.iter().find(|entry| entry.release == release)
}

/// How `release` is installed: its branch or last tag, its Python, its images.
pub fn info(release: &str) -> ReleaseInfo {
    match legacy(release) {
        Some(entry) => ReleaseInfo {
            release: entry.release.into(),
            series: entry.series.into(),
            reference: entry.kolla.into(),
            base_distro: "ubuntu".into(),
            python: entry.python.into(),
            eol: true,
            arm64: false,
        },
        None => ReleaseInfo {
            release: release.into(),
            series: release.into(),
            reference: branch(release).unwrap_or_else(|| format!("stable/{release}")),
            base_distro: "rocky".into(),
            python: super::GUEST_PYTHON.into(),
            eol: false,
            arm64: true,
        },
    }
}

/// Whether a host of `arch` can run `release`. The legacy series' Kolla images are published
/// for amd64 only, so an aarch64 host cannot boot them.
pub fn available_on(release: &str, arch: &str) -> bool {
    !(arch == "aarch64" && legacy(release).is_some())
}

/// Releases that can be installed, newest first: the branches kolla-ansible still publishes
/// and the end-of-life series whose last tags remain. Falls back to this build's release when
/// the repository cannot be read, so a farm can always be created.
pub async fn published() -> Vec<String> {
    if let Some(list) = configured() {
        return list;
    }
    // Tests never reach the network; `FARM_OPENSTACK_RELEASES` overrides this for air-gapped
    // hosts, which keep working with the release built into the binary.
    if cfg!(test) {
        return with_legacy(vec![super::OPENSTACK_RELEASE.to_owned()]);
    }
    if let Some(list) = cached() {
        return list;
    }
    let list = match fetch().await {
        Ok(refs) if !refs.is_empty() => {
            if let Ok(mut cache) = CACHE.lock() {
                *cache = Some((Instant::now(), refs.clone()));
            }
            with_legacy(refs.into_keys().collect())
        }
        Ok(_) => with_legacy(vec![super::OPENSTACK_RELEASE.to_owned()]),
        Err(error) => {
            eprintln!("cannot list the published OpenStack releases: {error:#}");
            with_legacy(vec![super::OPENSTACK_RELEASE.to_owned()])
        }
    };
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
        .unwrap_or_else(|| with_legacy(vec![super::OPENSTACK_RELEASE.to_owned()]))
}

/// The branch kolla-ansible keeps for `release`, `stable/` or `unmaintained/`, when a fetched
/// listing is cached. An end-of-life series has none and is installed from its tag instead.
fn branch(release: &str) -> Option<String> {
    let (at, refs) = CACHE.lock().ok()?.clone()?;
    (at.elapsed() < TTL)
        .then(|| refs.get(release).cloned())
        .flatten()
}

fn cached() -> Option<Vec<String>> {
    let (at, refs) = CACHE.lock().ok()?.clone()?;
    (at.elapsed() < TTL).then(|| with_legacy(refs.into_keys().collect()))
}

/// `FARM_OPENSTACK_RELEASES=2026.2,2025.2` pins the list, for hosts without the repository.
fn configured() -> Option<Vec<String>> {
    let value = std::env::var("FARM_OPENSTACK_RELEASES").ok()?;
    let list = normalize(value.split(',').map(str::trim));
    (!list.is_empty()).then_some(list)
}

/// Adds the end-of-life series to a list of releases that still have a branch.
fn with_legacy(mut list: Vec<String>) -> Vec<String> {
    for entry in LEGACY {
        list.push(entry.release.into());
    }
    normalize_owned(list)
}

async fn fetch() -> Result<BTreeMap<String, String>> {
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
    Ok(refs(&body))
}

/// The `stable/<release>` and `unmaintained/<release>` branches in a Gitea listing, each with
/// the ref to install it from.
pub fn refs(body: &serde_json::Value) -> BTreeMap<String, String> {
    let names = body
        .as_array()
        .map(|branches| {
            branches
                .iter()
                .filter_map(|branch| branch.get("name")?.as_str())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let mut refs = BTreeMap::new();
    for name in names {
        let Some(release) = name
            .strip_prefix("stable/")
            .or_else(|| name.strip_prefix("unmaintained/"))
        else {
            continue;
        };
        if !year_release(release) {
            continue;
        }
        // A maintained branch wins over the unmaintained one of the same release.
        refs.insert(release.to_owned(), name.to_owned());
    }
    refs
}

/// Releases named in a branch listing, as [`parse`] did before the refs were kept.
pub fn parse(body: &serde_json::Value) -> Vec<String> {
    normalize_owned(refs(body).into_keys().collect())
}

/// Keeps year.release names (`2026.2`), newest first, and always offers this build's release.
fn normalize_owned(mut list: Vec<String>) -> Vec<String> {
    list.retain(|release| year_release(release));
    list.sort();
    list.dedup();
    list.reverse();
    if !list.iter().any(|r| r == super::OPENSTACK_RELEASE) {
        list.push(super::OPENSTACK_RELEASE.to_owned());
    }
    list
}

fn normalize<'a>(releases: impl Iterator<Item = &'a str>) -> Vec<String> {
    normalize_owned(releases.map(str::to_owned).collect())
}

/// `2026.2`: a year and one release number. The legacy series reached before the year names
/// (`victoria`) are offered from [`LEGACY`] instead, under the names a farm records.
fn year_release(release: &str) -> bool {
    let (year, number) = release.split_once('.').unwrap_or_default();
    year.len() == 4
        && year.bytes().all(|c| c.is_ascii_digit())
        && number.len() == 1
        && number.bytes().all(|c| c.is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_end_of_life_series_installs_from_its_last_tag_on_its_own_python() {
        let legacy_info = info("2021.1");
        assert!(legacy_info.eol);
        assert_eq!(legacy_info.series, "wallaby");
        assert_eq!(legacy_info.reference, "12.8.0");
        assert_eq!(legacy_info.base_distro, "ubuntu");
        assert_eq!(legacy_info.python, "3.9");
        assert!(!legacy_info.arm64);
        // Its Kolla images are amd64 only, so an aarch64 host cannot run it.
        assert!(!available_on("2021.1", "aarch64"));
        assert!(available_on("2021.1", "x86_64"));

        // A release that still has a branch keeps the branch and the guest's Python.
        let current = info(super::super::OPENSTACK_RELEASE);
        assert!(!current.eol);
        assert_eq!(current.reference, "stable/2026.2");
        assert_eq!(current.base_distro, "rocky");
        assert_eq!(current.python, super::super::GUEST_PYTHON);
        assert!(current.arm64);
        assert!(available_on("2026.2", "aarch64"));
    }

    #[test]
    fn a_branch_listing_keeps_the_ref_to_install_each_release_from() {
        let body = serde_json::json!([
            { "name": "stable/2025.2" },
            { "name": "unmaintained/2024.1" },
            { "name": "stable/feature" },
        ]);
        let refs = refs(&body);
        assert_eq!(
            refs.get("2025.2").map(String::as_str),
            Some("stable/2025.2")
        );
        assert_eq!(
            refs.get("2024.1").map(String::as_str),
            Some("unmaintained/2024.1")
        );
    }

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
