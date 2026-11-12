//! The OpenStack releases this coordinator can install: the one built into the binary, the
//! branches the public kolla-ansible repository still publishes, and the 2024.x series whose
//! images the farm publishes itself. Only 2024.1 (Caracal) and newer are supported; the
//! end-of-life series keep their compatibility code but are offered only through an explicit
//! opt-in. A release with no template of its own is installed clean, so the choice is not
//! limited to the build's own release.

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

/// Branches of the public kolla-ansible repository. A release keeps `stable/` while it is
/// maintained and moves to `unmaintained/` afterwards; both install the same way.
const BRANCHES: &str =
    "https://opendev.org/api/v1/repos/openstack/kolla-ansible/branches?limit=100";
/// How long a fetched list is reused. The repository gains a branch twice a year.
const TTL: Duration = Duration::from_secs(600);

static CACHE: Mutex<Option<(Instant, BTreeMap<String, String>)>> = Mutex::new(None);

/// The public registry the farm publishes its own Kolla builds to (the end-of-life series and
/// 2024.1/2024.2, which quay no longer carries). A host that keeps a mirror sets
/// `FARM_EOL_REGISTRY`; the images are pulled anonymously.
pub const EOL_REGISTRY: &str = "ghcr.io";
/// The namespace of those builds below [`EOL_REGISTRY`], `owner/repository`. A mirror sets
/// `FARM_EOL_NAMESPACE`.
pub const EOL_NAMESPACE: &str = "preinpost/kolla";

/// The oldest OpenStack release this coordinator supports: Caracal, 2024.1. Older series keep
/// their install code (see [`LEGACY`]) but are neither offered nor accepted unless this host
/// sets `FARM_ALLOW_UNSUPPORTED_RELEASES=1`.
pub const MIN_SUPPORTED_RELEASE: &str = "2024.1";

/// The first release whose Kolla images quay publishes for aarch64 on the base the farm uses.
/// 2025.1–2026.1 have aarch64 images for Debian/Ubuntu only; 2026.2 is the first Rocky 10
/// `-aarch64` set. The farm does not switch its base distro, so earlier releases run on amd64.
const ARM64_FROM: &str = "2026.2";

/// Whether `release` is inside the supported range, or this host opted into older series.
pub fn supported(release: &str) -> bool {
    supported_when(allow_unsupported(), release)
}

/// Whether `release` is inside the range the project supports at all, [`MIN_SUPPORTED_RELEASE`]
/// and newer. The opt-in does not move this line: a farm below it keeps working but stays out of
/// support, which is what the portal marks beside its version.
pub fn in_supported_range(release: &str) -> bool {
    year_release(release) && release >= MIN_SUPPORTED_RELEASE
}

/// Whether this host allows releases below [`MIN_SUPPORTED_RELEASE`]
/// (`FARM_ALLOW_UNSUPPORTED_RELEASES=1`). The switch is read per call, so a running daemon
/// can change its catalog by restarting with the variable set.
pub fn allow_unsupported() -> bool {
    allow_unsupported_value(
        std::env::var("FARM_ALLOW_UNSUPPORTED_RELEASES")
            .ok()
            .as_deref(),
    )
}

/// Why `release` cannot be created or planned here, as the API reports it; `None` when it can.
pub fn unsupported(release: &str) -> Option<String> {
    unsupported_when(allow_unsupported(), release)
}

fn supported_when(allow_unsupported: bool, release: &str) -> bool {
    // The opt-in reopens only the end-of-life series whose compatibility code is still here.
    // 2023.1 and 2023.2 have neither images nor an entry, so they stay unsupported for good.
    in_supported_range(release) || (allow_unsupported && legacy(release).is_some())
}

fn unsupported_when(allow_unsupported: bool, release: &str) -> Option<String> {
    (!supported_when(allow_unsupported, release)).then(|| {
        let hint = if !year_release(release) {
            "name a release as year.release, such as 2024.1"
        } else if !allow_unsupported && legacy(release).is_some() {
            "set FARM_ALLOW_UNSUPPORTED_RELEASES=1 on the coordinator to allow it"
        } else {
            "no images are published for it"
        };
        format!(
            "OpenStack {release} is not supported; the oldest supported release is {MIN_SUPPORTED_RELEASE} (Caracal); {hint}"
        )
    })
}

fn allow_unsupported_value(value: Option<&str>) -> bool {
    value.is_some_and(|value| {
        matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        )
    })
}

/// [`EOL_REGISTRY`], or the one this host's `FARM_EOL_REGISTRY` names.
pub fn eol_registry() -> String {
    override_or("FARM_EOL_REGISTRY", EOL_REGISTRY)
}

/// [`EOL_NAMESPACE`], or the one this host's `FARM_EOL_NAMESPACE` names.
pub fn eol_namespace() -> String {
    override_or("FARM_EOL_NAMESPACE", EOL_NAMESPACE)
}

fn override_or(variable: &str, default: &str) -> String {
    match std::env::var(variable) {
        Ok(value) => override_value(variable, default, &value),
        Err(_) => default.to_owned(),
    }
}

/// `value` when it can name a registry or namespace, else `default` with a warning. Both go
/// into `globals.yml` and an image reference, so a value carrying anything but the characters
/// of a registry path (a space, a quote, a shell metacharacter) is refused rather than written
/// into a file the guest runs.
fn override_value(variable: &str, default: &str, value: &str) -> String {
    let value = value.trim();
    if value.is_empty() {
        return default.to_owned();
    }
    let valid = value.bytes().all(|byte| {
        byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b':' | b'/' | b'-')
    });
    if !valid {
        eprintln!(
            "{variable} is not a registry path (a-z, 0-9, '.', '_', ':', '/', '-'): {value:?}; using {default}"
        );
        return default.to_owned();
    }
    value.to_owned()
}

/// The base distro and Python of a series the farm builds itself. The kolla-ansible tag
/// appends its own base distro version to both the image tag and the Python path, so only
/// the distro is named here (`rocky` becomes `zed-rocky-9`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Build {
    /// What the farm built the images on, `rocky`.
    pub base_distro: &'static str,
    /// Python inside those images, `3.9`; the L3 agent's venv path carries it.
    pub python: &'static str,
}

/// A series kolla-ansible no longer keeps a branch for. Its last tag installs the same way,
/// but the guest needs an older Python and its images belong to an older Kolla.
pub struct Legacy {
    /// What a farm records, `2021.1`.
    pub release: &'static str,
    /// The series name: the image tag and the constraints name, `wallaby`.
    pub series: &'static str,
    /// Last kolla-ansible tag of the series.
    pub kolla: &'static str,
    /// Ansible range the series' quickstart asks for, `ansible>=2.9,<2.10`. The tag does not
    /// declare Ansible as a dependency, so the installer adds it to the venv.
    pub ansible: &'static str,
    /// Whether the tag's launcher has the `install-deps` subcommand (yoga 14.11.0 onwards).
    pub install_deps: bool,
    /// The tag that preserves the `ansible-collection-kolla` branch this series'
    /// `requirements.yml` names (`stable/yoga`): `yoga-eol`. The branch itself is deleted at
    /// end of life, so `install-deps` cannot check it out. Empty for a tag whose
    /// `requirements.yml` does not exist (before 14.11.0).
    pub collection: &'static str,
    /// Python minor the release's Ansible runs on; the guest's own 3.12 is too new.
    pub python: &'static str,
    /// The series' Kolla build, when the farm publishes one itself: it is pulled from
    /// [`eol_registry`]/[`eol_namespace`] instead of the tag's own registry. `None` leaves the
    /// tag's default images in place.
    pub built: Option<Build>,
}

/// End-of-life series, newest first. Every one of their kolla-ansible branches is deleted;
/// the tags below are the last release each series had (`openstack/releases` deliverables).
/// Their images are not on the `quay.io/openstack.kolla` registry these tags default to
/// (`docker_registry` in their `ansible/group_vars/all.yml`, which carries only 2025.1 and
/// later), so each series needs another source. zed is the first the farm builds and publishes
/// itself: `ghcr.io/preinpost/kolla/<service>:zed-rocky-9`, on Rocky 9, for amd64 only. The
/// older series keep the tag's own registry until the farm builds them too.
///
/// `ansible` and `install_deps` come from each tag's own tree, not from guesswork:
/// `doc/source/user/quickstart.rst` at the tag states the Ansible range in prose and installs
/// it with `pip` into the same venv, and `tools/kolla-ansible` at the tag enforces that range
/// and only knows `install-deps` from 14.11.0 on (before that the roles are in-tree and
/// `requirements.yml` does not exist, so an unknown subcommand just prints usage and exits 3).
///
/// `collection` is the tag `openstack/ansible-collection-kolla` kept when it deleted the
/// branch each `requirements.yml` names (`git ls-remote --tags
/// https://opendev.org/openstack/ansible-collection-kolla` lists `yoga-eol` and `zed-eol`, and
/// neither `stable/yoga` nor `stable/zed`). The tag holds the branch's last commit; for both
/// series that is the release they had already cut (`1.4.0` for yoga, `2.3.0` for zed) plus a
/// `.gitreview`/CI change, so it is the same collection code the series last ran.
pub const LEGACY: &[Legacy] = &[
    Legacy {
        release: "2022.2",
        series: "zed",
        kolla: "15.6.0",
        // "at least Ansible 4 and supports up to 5"; launcher takes 2.12 to 2.13.
        ansible: "ansible>=4,<6",
        install_deps: true,
        collection: "zed-eol",
        python: "3.10",
        // Built and published by the farm: Rocky 9 and its Python 3.9, amd64 only.
        built: Some(Build {
            base_distro: "rocky",
            python: "3.9",
        }),
    },
    Legacy {
        release: "2022.1",
        series: "yoga",
        kolla: "14.11.0",
        // "at least Ansible 4 and supports up to 5"; launcher takes 2.11 to 2.12.
        ansible: "ansible>=4,<6",
        install_deps: true,
        collection: "yoga-eol",
        python: "3.10",
        built: None,
    },
    Legacy {
        release: "2021.2",
        series: "xena",
        kolla: "13.9.0",
        // "at least Ansible 2.10 and supports up to 4"; launcher takes 2.10 to 2.11.
        ansible: "ansible>=2.10,<5.0",
        install_deps: false,
        // No `requirements.yml` at the tag: its roles ship in-tree.
        collection: "",
        python: "3.9",
        built: None,
    },
    Legacy {
        release: "2021.1",
        series: "wallaby",
        kolla: "12.8.0",
        // "at least Ansible 2.9 and supports up to 2.10"; launcher takes 2.9 to 2.10.
        ansible: "ansible>=2.9,<3.0",
        install_deps: false,
        collection: "",
        python: "3.9",
        built: None,
    },
    Legacy {
        release: "2020.2",
        series: "victoria",
        kolla: "11.4.0",
        // "at least Ansible 2.9 and supports up to 2.9"; launcher takes 2.9 only.
        ansible: "ansible>=2.9,<2.10",
        install_deps: false,
        collection: "",
        python: "3.9",
        built: None,
    },
    Legacy {
        release: "2020.1",
        series: "ussuri",
        kolla: "10.4.0",
        // "at least Ansible 2.8 and supports up to 2.9"; launcher takes 2.8 to 2.9.
        ansible: "ansible>=2.8,<2.10",
        install_deps: false,
        collection: "",
        python: "3.8",
        built: None,
    },
];

/// Code names of the `year.release` series, oldest first. `openstack/releases` names them in
/// `data/series_status.yaml` (https://opendev.org/openstack/releases/src/branch/master/data/series_status.yaml),
/// and https://releases.openstack.org/ lists the same pairs; a series not named here is shown
/// by its number alone. The name is display only: `series` keeps the `year.release` form the
/// image tag and the upper-constraints path are named by.
const CODENAMES: &[(&str, &str)] = &[
    ("2023.1", "Antelope"),
    ("2023.2", "Bobcat"),
    ("2024.1", "Caracal"),
    ("2024.2", "Dalmatian"),
    ("2025.1", "Epoxy"),
    ("2025.2", "Flamingo"),
    ("2026.1", "Gazpacho"),
    ("2026.2", "Hibiscus"),
    // The next series is named before kolla-ansible branches it, so the portal can label it
    // as soon as a fetched branch listing offers it.
    ("2027.1", "Indri"),
];

/// The code name `release` is shown by, `Caracal` for 2024.1 and `Zed` for 2022.2, or `None`
/// when the series has no published name here. The legacy series never got a `year.release`
/// name, so theirs is the series name capitalized for display.
pub fn codename(release: &str) -> Option<String> {
    if let Some(entry) = legacy(release) {
        return Some(capitalized(entry.series));
    }
    CODENAMES
        .iter()
        .find(|(series, _)| *series == release)
        .map(|(_, name)| (*name).to_owned())
}

/// `zed` as the portal shows it, `Zed`; empty for an empty name.
fn capitalized(series: &str) -> String {
    let mut chars = series.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

/// A supported release whose install differs from a plain `stable/<release>` branch: the
/// branch moved to `unmaintained/`, or was deleted and only the series' last tag remains, and
/// quay no longer publishes its images. The ref and image source are fixed here, so an
/// offline coordinator still resolves the release.
struct Built {
    /// What a farm records, `2024.1`.
    release: &'static str,
    /// kolla-ansible ref to install: `unmaintained/2024.1` while the branch exists, otherwise
    /// the series' last tag (`2024.2-eol`). A fetched branch listing wins when it has one.
    reference: &'static str,
    /// The farm's Kolla images: Rocky and its Python, built for amd64 only.
    build: Build,
    /// Python minor its tooling runs on in the guest; the guest's own for both 2024.x.
    python: &'static str,
    /// Ansible to install when the release's `requirements.txt` leaves it out. 2024.1 accepts
    /// ansible-core up to 2.16, which runs on the guest's Python 3.12; 2024.2 pins 2.16–2.18.
    ansible: &'static str,
    /// The `ansible-collection-kolla` ref when the tag's `requirements.yml` names a branch the
    /// collection deleted (`stable/2024.2`); empty when that branch still exists.
    collection: &'static str,
}

/// Releases the farm builds images for, newest first. Both 2024.x series are inside the
/// supported range: 2025.1 is the first quay still serves. `.github/workflows/eol-images.yml`
/// in `github.com/preinpost/kolla` builds `<service>:<release>-rocky-9` with the image groups
/// from `.github/kolla/groups.json`, and the farm pulls them from [`EOL_REGISTRY`].
const BUILT: &[Built] = &[
    Built {
        release: "2024.2",
        reference: "2024.2-eol",
        build: Build {
            base_distro: "rocky",
            python: "3.9",
        },
        // Its requirements.txt pins ansible-core 2.16–2.18, which runs on Python 3.12.
        python: super::GUEST_PYTHON,
        ansible: "",
        // `stable/2024.2` is gone; 6.3.0 is the last ansible-collection-kolla tag of the
        // series, which holds the branch's last released commit.
        collection: "6.3.0",
    },
    Built {
        release: "2024.1",
        reference: "unmaintained/2024.1",
        build: Build {
            base_distro: "rocky",
            python: "3.9",
        },
        // Its requirements.txt leaves Ansible out. Its launcher and prechecks accept
        // ansible-core up to 2.16, which supports the guest's Python 3.12.
        python: super::GUEST_PYTHON,
        ansible: "ansible-core>=2.16,<2.17",
        // The collection keeps `unmaintained/2024.1`, which is what the tag names.
        collection: "",
    },
];

/// The built entry for `release`, when the farm publishes its images itself.
fn built(release: &str) -> Option<&'static Built> {
    BUILT.iter().find(|entry| entry.release == release)
}

/// The images a series is installed from, when the farm builds and publishes them itself.
#[derive(Clone, Debug, PartialEq)]
pub struct Images {
    /// The registry, `ghcr.io`.
    pub registry: String,
    /// Below-the-registry name, `preinpost/kolla`.
    pub namespace: String,
    /// Base distro the images were built on, `rocky`; the tag appends its version.
    pub base_distro: String,
    /// Python inside those images, `3.9`; the L3 agent's venv path carries it.
    pub python: String,
}

/// What installing one release takes: which kolla-ansible ref, which Python, which images.
#[derive(Clone, Debug, PartialEq)]
pub struct ReleaseInfo {
    /// The name a farm records, `2021.1` or `2026.2`.
    pub release: String,
    /// Series name: the image tag and the upper-constraints name.
    pub series: String,
    /// The series' code name for display, `Caracal`; empty when it has no published name.
    pub codename: String,
    /// kolla-ansible branch (`stable/2026.2`) or tag (`12.8.0`) to install.
    pub reference: String,
    /// The farm's own build of this series' images, when it has one. `None` installs the
    /// tag's default images (`quay.io/openstack.kolla` for a legacy tag, the maintained
    /// branch's own default).
    pub images: Option<Images>,
    /// Python minor its tooling runs on: the guest's own 3.12, or an older minor for a
    /// series whose Ansible predates it.
    pub python: String,
    /// Ansible to install next to kolla-ansible, `ansible>=2.9,<2.10`; empty when the
    /// release's requirements already provide it.
    pub ansible: String,
    /// Whether `kolla-ansible install-deps` exists in this release.
    pub install_deps: bool,
    /// The `ansible-collection-kolla` ref to install instead of the deleted branch the tag's
    /// `requirements.yml` names, `zed-eol`; empty when the tag has no Galaxy step.
    pub collection: String,
    /// Kolla-ansible keeps no branch and the series is end of life: it is offered only with
    /// `FARM_ALLOW_UNSUPPORTED_RELEASES=1`.
    pub eol: bool,
    /// Its images are published for aarch64.
    pub arm64: bool,
}

impl ReleaseInfo {
    /// Base distro of its images: the farm's own build, or the tag's default (`ubuntu` for the
    /// legacy series, `rocky` for a maintained branch).
    pub fn base_distro(&self) -> &str {
        match &self.images {
            Some(images) => &images.base_distro,
            None if self.eol => "ubuntu",
            None => "rocky",
        }
    }

    /// Python inside its containers. The tag's Ubuntu images and a maintained Rocky 10 build
    /// carry the same minor as the release's tooling; the farm's Rocky 9 build is older.
    /// 2025.1 is the last series kolla publishes on Rocky 9, whose Python 3.9 differs from
    /// the guest tooling's 3.12.
    pub fn image_python(&self) -> &str {
        match &self.images {
            Some(images) => &images.python,
            None if self.release == "2025.1" => "3.9",
            None => &self.python,
        }
    }

    /// Whether `kolla-ansible` is the bash launcher of releases before 2024.2, which finds the
    /// playbooks a pip install put beside it only through `VIRTUAL_ENV`. 2024.2 replaced it
    /// with a Python console script that locates its own package.
    pub fn venv_launcher(&self) -> bool {
        self.release.as_str() < "2024.2"
    }
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
            codename: capitalized(entry.series),
            reference: entry.kolla.into(),
            images: entry.built.map(|build| Images {
                registry: eol_registry(),
                namespace: eol_namespace(),
                base_distro: build.base_distro.into(),
                python: build.python.into(),
            }),
            python: entry.python.into(),
            ansible: entry.ansible.into(),
            install_deps: entry.install_deps,
            collection: entry.collection.into(),
            eol: true,
            arm64: false,
        },
        None => match built(release) {
            Some(entry) => ReleaseInfo {
                release: entry.release.into(),
                series: entry.release.into(),
                codename: codename(release).unwrap_or_default(),
                // A branch listing wins while kolla-ansible still publishes the series
                // (`unmaintained/2024.1`); 2024.2 has no branch and falls back to its tag.
                reference: branch(release).unwrap_or_else(|| entry.reference.into()),
                images: Some(Images {
                    registry: eol_registry(),
                    namespace: eol_namespace(),
                    base_distro: entry.build.base_distro.into(),
                    python: entry.build.python.into(),
                }),
                python: entry.python.into(),
                ansible: entry.ansible.into(),
                install_deps: true,
                collection: entry.collection.into(),
                eol: false,
                arm64: false,
            },
            None => ReleaseInfo {
                release: release.into(),
                series: release.into(),
                codename: codename(release).unwrap_or_default(),
                reference: branch(release).unwrap_or_else(|| format!("stable/{release}")),
                // A maintained branch pulls its own images from its default registry.
                images: None,
                python: super::GUEST_PYTHON.into(),
                // A maintained branch's requirements already depend on Ansible.
                ansible: String::new(),
                install_deps: true,
                // Its requirements.yml names a branch the collection still publishes.
                collection: String::new(),
                eol: false,
                arm64: published_for_arm64(release),
            },
        },
    }
}

/// [`info`] with the kolla-ansible ref the coordinator resolved for `release`. A host's
/// agent builds its install script in its own process, which never reads the release
/// catalog, so [`info`] there would fall back to `stable/<release>` even after the series
/// moved to `unmaintained/` (2025.1 did). Without a ref it keeps [`info`]'s answer.
pub fn info_with(release: &str, reference: Option<&str>) -> ReleaseInfo {
    let mut info = info(release);
    if let Some(reference) = reference.filter(|reference| reference_ok(reference)) {
        info.reference = reference.to_owned();
    }
    info
}

/// Whether `reference` can name a kolla-ansible branch (`stable/2026.2`,
/// `unmaintained/2025.1`) or tag (`2024.2-eol`, `12.8.0`). The value arrives from the
/// coordinator and goes into a shell script, so anything else is refused.
pub fn reference_ok(reference: &str) -> bool {
    !reference.is_empty()
        && !reference.starts_with('-')
        && !reference.contains("..")
        && reference
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '/'))
}

/// The exact kolla-ansible ref to install `release` from, resolved against the branch list
/// the public repository publishes. The coordinator resolves it once and sends it with the
/// deploy; the guest then installs that ref instead of the `stable/<release>` guess.
pub async fn reference(release: &str) -> Result<String> {
    // A test can state what a fetched listing holds without warming the process-wide cache,
    // which would change what other tests plan and install.
    #[cfg(test)]
    if let Some(found) = remembered(release) {
        return Ok(found);
    }
    // A series the farm builds images for, or an end-of-life tag, pins its ref. A fetched
    // listing still wins for a built series while it has a branch (`unmaintained/2024.1`).
    if built(release).is_some() || legacy(release).is_some() {
        return Ok(info(release).reference);
    }
    if let Some(found) = branch(release) {
        return Ok(found);
    }
    // Tests never reach the network, and a host that pinned its catalog has no listing to
    // read: both keep the branch `info` names.
    if cfg!(test) || configured().is_some() {
        return Ok(info(release).reference);
    }
    match fetch().await {
        Ok(refs) => {
            if let Ok(mut cache) = CACHE.lock() {
                *cache = Some((Instant::now(), refs));
            }
        }
        Err(error) => {
            // The guest still reaches the repository and its install fails on a ref that does
            // not exist, so the guess is not silent.
            eprintln!(
                "cannot list the kolla-ansible branches to resolve OpenStack {release}: {error:#}; assuming stable/{release}"
            );
            return Ok(format!("stable/{release}"));
        }
    }
    match branch(release) {
        Some(found) => Ok(found),
        None => bail!(
            "kolla-ansible publishes neither stable/{release} nor unmaintained/{release}; an end-of-life series installs from its last tag"
        ),
    }
}

/// A test's answer for [`reference`], keyed by release. Kept apart from [`CACHE`] so a test
/// that states one does not change what another test plans or installs.
#[cfg(test)]
static TEST_REFERENCES: Mutex<Option<BTreeMap<String, String>>> = Mutex::new(None);

/// Seeds the answer [`reference`] gives for a release in this test binary.
#[cfg(test)]
pub fn remember(refs: &[(&str, &str)]) {
    let mut map = TEST_REFERENCES.lock().unwrap();
    let map = map.get_or_insert_with(BTreeMap::new);
    for (release, reference) in refs {
        map.insert((*release).to_owned(), (*reference).to_owned());
    }
}

#[cfg(test)]
fn remembered(release: &str) -> Option<String> {
    TEST_REFERENCES.lock().ok()?.as_ref()?.get(release).cloned()
}

/// Whether the farm's Rocky-based Kolla images of `release` are published for aarch64. The
/// farm does not switch its base distro for arm64: 2025.1–2026.1 publish aarch64 only on
/// Debian/Ubuntu, and the series the farm builds itself (zed, 2024.1, 2024.2) are amd64 only.
fn published_for_arm64(release: &str) -> bool {
    year_release(release) && release >= ARM64_FROM
}

/// Whether a host of `arch` can run `release`: its images must be published for that arch.
pub fn available_on(release: &str, arch: &str) -> bool {
    arches(release).iter().any(|published| published == arch)
}

/// The CPU architectures `release`'s Kolla images are published for, `x86_64` first.
pub fn arches(release: &str) -> Vec<String> {
    let mut arches = vec!["x86_64".to_owned()];
    if published_for_arm64(release) {
        arches.push("aarch64".to_owned());
    }
    arches
}

/// Releases that can be installed, newest first: the branches kolla-ansible still publishes,
/// the 2024.x series the farm builds images for, and (with the opt-in) the end-of-life series
/// whose last tags remain. Falls back to this build's release when the repository cannot be
/// read, so a farm can always be created.
pub async fn published() -> Vec<String> {
    if let Some(list) = configured() {
        return list;
    }
    // Tests never reach the network; `FARM_OPENSTACK_RELEASES` overrides this for air-gapped
    // hosts, which keep working with the release built into the binary.
    if cfg!(test) {
        return with_known(vec![super::OPENSTACK_RELEASE.to_owned()]);
    }
    if let Some(list) = cached() {
        return list;
    }
    match fetch().await {
        Ok(refs) if !refs.is_empty() => {
            if let Ok(mut cache) = CACHE.lock() {
                *cache = Some((Instant::now(), refs.clone()));
            }
            with_known(refs.into_keys().collect())
        }
        Ok(_) => with_known(vec![super::OPENSTACK_RELEASE.to_owned()]),
        Err(error) => {
            eprintln!("cannot list the published OpenStack releases: {error:#}");
            with_known(vec![super::OPENSTACK_RELEASE.to_owned()])
        }
    }
}

/// Whether `release` is one this coordinator can install clean.
pub fn installable(release: &str) -> bool {
    published_sync().iter().any(|known| known == release)
}

/// The list without waiting: the configured one, the cached one, else this build's release.
fn published_sync() -> Vec<String> {
    configured()
        .or_else(cached)
        .unwrap_or_else(|| with_known(vec![super::OPENSTACK_RELEASE.to_owned()]))
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
    (at.elapsed() < TTL).then(|| with_known(refs.into_keys().collect()))
}

/// `FARM_OPENSTACK_RELEASES=2026.2,2025.2` pins the list, for hosts without the repository.
fn configured() -> Option<Vec<String>> {
    let value = std::env::var("FARM_OPENSTACK_RELEASES").ok()?;
    let list = normalize(value.split(',').map(str::trim));
    (!list.is_empty()).then_some(list)
}

/// Adds the releases a branch listing cannot offer: the end-of-life series (only when this
/// host opted in) and the 2024.x series the farm builds images for.
fn with_known(mut list: Vec<String>) -> Vec<String> {
    for entry in LEGACY.iter().filter(|entry| supported(entry.release)) {
        list.push(entry.release.into());
    }
    for entry in BUILT {
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
    let mut refs: BTreeMap<String, String> = BTreeMap::new();
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
        // A maintained branch wins over the unmaintained one of the same release, in
        // whichever order the listing names the two.
        let maintained = name.starts_with("stable/");
        match refs.get(release) {
            Some(kept) if kept.starts_with("stable/") => {}
            Some(_) if !maintained => {}
            _ => {
                refs.insert(release.to_owned(), name.to_owned());
            }
        }
    }
    refs
}

/// Releases named in a branch listing, as [`parse`] did before the refs were kept.
pub fn parse(body: &serde_json::Value) -> Vec<String> {
    normalize_owned(refs(body).into_keys().collect())
}

/// Keeps the supported year.release names (`2024.1` and newer), newest first, and always
/// offers this build's release. A pinned `FARM_OPENSTACK_RELEASES` list passes through here
/// too, so it cannot smuggle in an older series without the opt-in.
fn normalize_owned(mut list: Vec<String>) -> Vec<String> {
    list.retain(|release| supported(release));
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
        assert_eq!(legacy_info.base_distro(), "ubuntu");
        assert_eq!(legacy_info.python, "3.9");
        // The tag's requirements leave Ansible out, so its quickstart installs it separately.
        assert_eq!(legacy_info.ansible, "ansible>=2.9,<3.0");
        // Galaxy collections arrived with yoga (14.11.0); it alone has `install-deps`.
        assert!(!legacy_info.install_deps);
        assert!(legacy_info.collection.is_empty());
        assert!(!legacy_info.arm64);
        // Its Kolla images are amd64 only, so an aarch64 host cannot run it.
        assert!(!available_on("2021.1", "aarch64"));
        assert!(available_on("2021.1", "x86_64"));

        // A release that still has a branch keeps the branch and the guest's Python.
        let current = info(super::super::OPENSTACK_RELEASE);
        assert!(!current.eol);
        assert_eq!(current.reference, "stable/2026.2");
        assert_eq!(current.base_distro(), "rocky");
        assert_eq!(current.python, super::super::GUEST_PYTHON);
        assert!(current.ansible.is_empty());
        assert!(current.install_deps);
        assert!(current.collection.is_empty());
        assert!(current.arm64);
        assert!(available_on("2026.2", "aarch64"));
    }

    #[test]
    fn only_the_series_with_galaxy_collections_carry_the_eol_tag() {
        // The two tags whose `requirements.yml` names a deleted branch, and the tag that holds
        // that branch's last commit in openstack/ansible-collection-kolla.
        for (release, series, collection) in
            [("2022.2", "zed", "zed-eol"), ("2022.1", "yoga", "yoga-eol")]
        {
            let info = info(release);
            assert_eq!(info.series, series);
            assert_eq!(info.collection, collection);
            assert!(info.install_deps);
        }
        // Everything else names no collection, so the installer has nothing to rewrite.
        for release in ["2021.2", "2021.1", "2020.2", "2020.1", "2024.1", "2026.2"] {
            assert!(info(release).collection.is_empty(), "{release}");
        }
    }

    #[test]
    fn the_code_names_are_the_published_series_names() {
        // From openstack/releases `data/series_status.yaml`, which https://releases.openstack.org/
        // renders: every `year.release` the catalog can offer, and the next one it may.
        for (release, name) in [
            ("2023.1", "Antelope"),
            ("2023.2", "Bobcat"),
            ("2024.1", "Caracal"),
            ("2024.2", "Dalmatian"),
            ("2025.1", "Epoxy"),
            ("2025.2", "Flamingo"),
            ("2026.1", "Gazpacho"),
            ("2026.2", "Hibiscus"),
            ("2027.1", "Indri"),
        ] {
            assert_eq!(codename(release).as_deref(), Some(name), "{release}");
            let info = info(release);
            assert_eq!(info.codename, name, "{release}");
            // The code name is display only: the image tag and the upper-constraints path
            // keep the `year.release` name.
            assert_eq!(info.series, release, "{release}");
        }
        // The end-of-life series show their own name, capitalized, without changing the
        // lowercase name their images and constraints carry.
        for (release, series) in [
            ("2022.2", "Zed"),
            ("2022.1", "Yoga"),
            ("2021.2", "Xena"),
            ("2021.1", "Wallaby"),
            ("2020.2", "Victoria"),
            ("2020.1", "Ussuri"),
        ] {
            let info = info(release);
            assert_eq!(info.codename, series, "{release}");
            assert_eq!(info.series, series.to_ascii_lowercase(), "{release}");
        }
        // A series with no published name here is shown by its number alone.
        assert_eq!(codename("2099.1"), None);
        assert!(info("2099.1").codename.is_empty());
        // A code name is not a release name.
        assert_eq!(codename("zed"), None);
    }

    #[test]
    fn only_the_series_the_farm_builds_carry_an_image_source() {
        // zed is the first series the farm builds and publishes itself: Rocky 9, amd64 only.
        assert_eq!(EOL_REGISTRY, "ghcr.io");
        assert_eq!(EOL_NAMESPACE, "preinpost/kolla");
        let zed = info("2022.2");
        let images = zed.images.as_ref().expect("zed is built by the farm");
        assert_eq!(images.registry, eol_registry());
        assert_eq!(images.namespace, eol_namespace());
        assert_eq!(images.base_distro, "rocky");
        assert_eq!(images.python, "3.9");
        assert_eq!(zed.base_distro(), "rocky");
        // kolla-ansible appends the base distro version, `9`, for `zed-rocky-9`; the L3 patch
        // goes into this image Python's venv, not the guest tooling's 3.10.
        assert_eq!(zed.image_python(), "3.9");
        assert_ne!(zed.image_python(), zed.python);

        // 2024.1 and 2024.2 are inside the supported range but no longer on quay: the farm
        // publishes their Rocky 9 images at `ghcr.io/preinpost/kolla/<service>:2024.x-rocky-9`.
        for (release, reference) in [("2024.1", "unmaintained/2024.1"), ("2024.2", "2024.2-eol")] {
            let info = info(release);
            assert!(!info.eol, "{release}");
            assert_eq!(info.series, release);
            // A fetched branch listing may name 2024.1's branch; the fixed ref is the offline
            // answer, and 2024.2 has no branch at all.
            assert_eq!(info.reference, reference, "{release}");
            let images = info
                .images
                .as_ref()
                .expect("{release} is built by the farm");
            assert_eq!(images.registry, eol_registry(), "{release}");
            assert_eq!(images.namespace, eol_namespace(), "{release}");
            assert_eq!(images.base_distro, "rocky", "{release}");
            assert_eq!(images.python, "3.9", "{release}");
            assert_eq!(info.base_distro(), "rocky", "{release}");
            assert_eq!(info.image_python(), "3.9", "{release}");
            assert!(info.install_deps, "{release}");
            assert!(!info.arm64, "{release}");
        }
        // 2024.2's requirements pin ansible-core, which runs on the guest's Python 3.12.
        assert_eq!(info("2024.2").python, super::super::GUEST_PYTHON);
        assert!(info("2024.2").ansible.is_empty());
        // 2024.1 leaves Ansible out; 2.16 is within the range its launcher and prechecks
        // accept and supports the guest's Python 3.12.
        assert_eq!(info("2024.1").python, super::super::GUEST_PYTHON);
        assert_eq!(info("2024.1").ansible, "ansible-core>=2.16,<2.17");
        // 2024.2's requirements.yml names `stable/2024.2`, which the collection deleted; its
        // last series tag holds the branch's last released commit. 2024.1 still has
        // `unmaintained/2024.1`, which the tag already names.
        assert_eq!(info("2024.2").collection, "6.3.0");
        assert!(info("2024.1").collection.is_empty());
        // 2024.1 still installs the bash launcher, which needs VIRTUAL_ENV; 2024.2 replaced it
        // with a Python console script.
        assert!(info("2024.1").venv_launcher());
        assert!(!info("2024.2").venv_launcher());

        // Every other series keeps the tag's own images and Ubuntu base until we build it.
        for release in ["2022.1", "2021.2", "2021.1", "2020.2", "2020.1"] {
            let info = info(release);
            assert!(info.images.is_none(), "{release}");
            assert_eq!(info.base_distro(), "ubuntu", "{release}");
            assert_eq!(info.image_python(), info.python, "{release}");
        }
        // A maintained branch pulls the images its tag names.
        let current = info(super::super::OPENSTACK_RELEASE);
        assert!(current.images.is_none());
        assert_eq!(current.base_distro(), "rocky");
        assert_eq!(current.image_python(), super::super::GUEST_PYTHON);
    }

    #[test]
    fn only_a_registry_path_overrides_the_image_source() {
        // A mirror names a registry and a namespace, and both are written into globals.yml.
        assert_eq!(
            override_value("FARM_EOL_REGISTRY", EOL_REGISTRY, " mirror.example:5000 "),
            "mirror.example:5000"
        );
        assert_eq!(
            override_value("FARM_EOL_NAMESPACE", EOL_NAMESPACE, "team/kolla_mirror"),
            "team/kolla_mirror"
        );
        // Unset or blank keeps the public default.
        assert_eq!(
            override_value("FARM_EOL_REGISTRY", EOL_REGISTRY, "   "),
            EOL_REGISTRY
        );
        // Anything that could break out of the value keeps the default instead.
        for bad in [
            "hub example",
            "hub\nlocalhost",
            "hub;rm -rf /",
            "\"hub\"",
            "hub}docker_registry",
        ] {
            assert_eq!(
                override_value("FARM_EOL_REGISTRY", EOL_REGISTRY, bad),
                EOL_REGISTRY,
                "{bad:?}"
            );
        }
    }

    #[test]
    fn only_caracal_and_newer_are_supported_unless_the_host_opts_in() {
        assert_eq!(MIN_SUPPORTED_RELEASE, "2024.1");
        for release in ["2024.1", "2024.2", "2025.1", "2025.2", "2026.1", "2026.2"] {
            assert!(supported_when(false, release), "{release}");
        }
        for release in ["2022.2", "2022.1", "2021.2", "2020.1"] {
            assert!(!supported_when(false, release), "{release}");
            assert!(supported_when(true, release), "{release}");
            let why = unsupported_when(false, release).expect("refused");
            assert!(why.contains(MIN_SUPPORTED_RELEASE), "{why}");
            assert!(why.contains("FARM_ALLOW_UNSUPPORTED_RELEASES=1"), "{why}");
            // The opt-in keeps working for a series a farm can still be created from.
            assert!(unsupported_when(true, release).is_none(), "{release}");
        }
        // 2023.1 and 2023.2 are neither supported nor reopened by the opt-in.
        for release in ["2023.2", "2023.1"] {
            assert!(!supported_when(false, release), "{release}");
            assert!(!supported_when(true, release), "{release}");
            let why = unsupported_when(false, release).expect("refused");
            assert!(why.contains("no images are published"), "{why}");
            assert!(!why.contains("FARM_ALLOW_UNSUPPORTED_RELEASES"), "{why}");
        }
        // Code names were never a farm's release name, even with the opt-in.
        assert!(!supported_when(true, "zed"));
        assert!(unsupported_when(true, "zed").is_some_and(|why| why.contains("year.release")));
        assert!(!allow_unsupported_value(None));
        assert!(!allow_unsupported_value(Some("")));
        assert!(!allow_unsupported_value(Some("0")));
        assert!(!allow_unsupported_value(Some("no")));
        for value in ["1", "true", " YES ", "on"] {
            assert!(allow_unsupported_value(Some(value)), "{value}");
        }
        // The supported range itself never moves with the opt-in: a farm below it stays
        // marked as outside support, which is what the portal shows beside its version.
        for release in ["2022.2", "2023.2", "2022.1", "zed", ""] {
            assert!(!in_supported_range(release), "{release}");
        }
        for release in ["2024.1", "2025.1", "2026.2"] {
            assert!(in_supported_range(release), "{release}");
        }
    }

    #[test]
    fn arches_follow_the_images_the_farm_would_pull() {
        // The current release is published for Rocky's aarch64 dataset; nothing older is.
        assert_eq!(
            arches(super::super::OPENSTACK_RELEASE),
            ["x86_64", "aarch64"]
        );
        for release in [
            "2024.1", "2024.2", "2025.1", "2025.2", "2026.1", "2022.2", "2020.1",
        ] {
            assert_eq!(arches(release), ["x86_64"], "{release}");
            assert!(available_on(release, "x86_64"), "{release}");
            assert!(!available_on(release, "aarch64"), "{release}");
            assert!(!available_on(release, "riscv64"), "{release}");
        }
    }

    #[test]
    fn the_image_python_follows_the_base_the_tag_publishes() {
        // 2025.1 is the last series on Rocky 9, whose images carry Python 3.9; the L3 patch
        // mounts into that venv, not the guest tooling's 3.12.
        assert_eq!(info("2025.1").python, super::super::GUEST_PYTHON);
        assert_eq!(info("2025.1").image_python(), "3.9");
        // 2025.2 is the first Rocky 10 set, whose Python is the guest's own.
        for release in ["2025.2", "2026.1", super::super::OPENSTACK_RELEASE] {
            assert_eq!(info(release).image_python(), super::super::GUEST_PYTHON);
        }
    }

    #[tokio::test]
    async fn the_coordinator_resolves_the_branch_a_series_moved_to() {
        // The catalog is the only place a stable/unmaintained move is visible; a host's agent
        // process never reads it, so the coordinator resolves the ref and sends it along.
        remember(&[("2025.1", "unmaintained/2025.1")]);
        assert_eq!(reference("2025.1").await.unwrap(), "unmaintained/2025.1");
        // A series the listing does not name keeps the branch `info` writes; the install
        // then fails on a ref that does not exist instead of the deploy pretending it did.
        assert_eq!(reference("2026.1").await.unwrap(), "stable/2026.1");
        // What the agent does with it: the ref the coordinator sent wins over the guess.
        assert_eq!(
            info_with("2026.1", Some("unmaintained/2026.1")).reference,
            "unmaintained/2026.1"
        );
        assert_eq!(info_with("2026.1", None).reference, "stable/2026.1");
        assert_eq!(
            info_with("2025.1", Some("unmaintained/2025.1")).reference,
            "unmaintained/2025.1"
        );
    }

    #[test]
    fn only_a_ref_a_shell_can_take_reaches_the_install_script() {
        for good in [
            "stable/2026.2",
            "unmaintained/2025.1",
            "2024.2-eol",
            "12.8.0",
        ] {
            assert!(reference_ok(good), "{good}");
        }
        for bad in [
            "",
            "-upload-pack=/tmp/x",
            "stable/2025.1; rm -rf /",
            "stable/2025.1$(id)",
            "stable/../2025.1",
            "stable/2025.1\n",
        ] {
            assert!(!reference_ok(bad), "{bad}");
        }
    }

    #[test]
    fn a_branch_listing_keeps_the_ref_to_install_each_release_from() {
        let body = serde_json::json!([
            { "name": "stable/2025.2" },
            { "name": "unmaintained/2024.1" },
            { "name": "stable/feature" },
        ]);
        let found = refs(&body);
        assert_eq!(
            found.get("2025.2").map(String::as_str),
            Some("stable/2025.2")
        );
        assert_eq!(
            found.get("2024.1").map(String::as_str),
            Some("unmaintained/2024.1")
        );
        // While a series moves, the listing can name both branches. The maintained one wins
        // whichever the listing puts first.
        for names in [
            ["unmaintained/2024.1", "stable/2024.1"],
            ["stable/2024.1", "unmaintained/2024.1"],
        ] {
            let body = serde_json::json!(names.map(|name| serde_json::json!({ "name": name })));
            assert_eq!(
                refs(&body).get("2024.1").map(String::as_str),
                Some("stable/2024.1")
            );
        }
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
