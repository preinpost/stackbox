//! Host PCI passthrough candidates: which devices a host has, which of them a stackbox may take,
//! and the root-owned helper that binds them.
//!
//! The protection rules live in `pci_guard.sh`, which both `inspect.sh` (SSH hosts) and the helper
//! run. [`candidates_at`] applies the same rules to a sysfs tree for this machine (local and
//! dialled-in agent hosts), so the two answers agree.

use super::HostPciCandidate;
use crate::model::normalize_pci_bdf;
use anyhow::{Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use utoipa::ToSchema;

/// Where the root-owned helper is installed; the sudoers rule names this exact path.
pub const HELPER_PATH: &str = "/usr/local/libexec/stackbox-vfio-bind";
/// The sudoers drop-in that lets the agent user run the helper without a password.
pub const SUDOERS_PATH: &str = "/etc/sudoers.d/stackbox-vfio";
/// The helper protocol this coordinator speaks; `--check` reports it.
pub const HELPER_PROTOCOL: u32 = 2;

const GUARD: &str = include_str!("pci_guard.sh");
const HELPER: &str = include_str!("vfio_bind.sh");
const INSTALL: &str = include_str!("vfio_install.sh");

/// The helper script as installed: `vfio_bind.sh` with the shared protection rules in place.
pub fn helper_script() -> String {
    HELPER
        .replace("@HELPER_PROTOCOL@", &HELPER_PROTOCOL.to_string())
        .replace("@PCI_GUARD@", GUARD)
}

/// A short name for a PCI class code (six hex digits).
pub fn class_name(class: &str) -> &'static str {
    if class.starts_with("0108") {
        "nvme"
    } else if class.starts_with("01") {
        "storage"
    } else if class.starts_with("02") {
        "network"
    } else if class.starts_with("03") {
        "gpu"
    } else if class.starts_with("04") {
        "audio"
    } else if class.starts_with("0c03") {
        "usb"
    } else {
        "other"
    }
}

/// The facts a host reports about one candidate, before the coordinator derives the rest.
pub struct CandidateFacts {
    pub bdf: String,
    pub vendor_id: String,
    pub product_id: String,
    pub class: String,
    pub iommu_group: Option<u32>,
    pub driver: Option<String>,
    pub group_exclusive: bool,
    /// `/dev/vfio/<group>` is readable and writable by the agent.
    pub group_accessible: bool,
    pub protected_reason: Option<String>,
    pub warning: Option<String>,
}

/// Builds a candidate, deriving the flags the portal shows from the reported facts.
pub fn candidate(facts: CandidateFacts) -> HostPciCandidate {
    let vfio_ready = facts.group_accessible
        && facts.driver.as_deref() == Some("vfio-pci")
        && facts.group_exclusive;
    let bindable =
        facts.protected_reason.is_none() && facts.iommu_group.is_some() && facts.group_exclusive;
    HostPciCandidate {
        class_name: class_name(&facts.class).into(),
        bdf: facts.bdf,
        vendor_id: facts.vendor_id,
        product_id: facts.product_id,
        class: facts.class,
        iommu_group: facts.iommu_group,
        driver: facts.driver,
        group_exclusive: facts.group_exclusive,
        vfio_ready,
        bindable,
        protected_reason: facts.protected_reason,
        warning: facts.warning,
        owning_stackbox: None,
    }
}

/// The controllers the protection rules keep bound to their host drivers.
#[derive(Default)]
struct Protection {
    /// Whether the root disk resolved; when it did not, every device is protected.
    root_known: bool,
    root: BTreeSet<String>,
    nic: BTreeSet<String>,
    usb: BTreeSet<String>,
}

/// The PCI address that `path` (resolved) hangs off: its last PCI path component.
fn controller_of(path: &Path) -> Option<String> {
    let real = std::fs::canonicalize(path).ok()?;
    real.components()
        .filter_map(|part| part.as_os_str().to_str())
        .rfind(|part| part.len() == 12 && normalize_pci_bdf(part).is_ok())
        .map(str::to_owned)
}

/// The controllers under block device `path`, following device-mapper and LVM slaves.
fn block_controllers(path: &Path, out: &mut BTreeSet<String>) {
    let Ok(real) = std::fs::canonicalize(path) else {
        return;
    };
    out.extend(controller_of(&real));
    if let Ok(slaves) = std::fs::read_dir(real.join("slaves")) {
        for slave in slaves.flatten() {
            block_controllers(&slave.path(), out);
        }
    }
}

/// The controllers under network device `path`, following bonding lower devices and bridge ports.
fn net_controllers(path: &Path, out: &mut BTreeSet<String>) {
    let Ok(real) = std::fs::canonicalize(path) else {
        return;
    };
    out.extend(controller_of(&real));
    let mut lower: Vec<PathBuf> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&real) {
        lower.extend(entries.flatten().map(|e| e.path()).filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("lower_"))
        }));
    }
    if let Ok(ports) = std::fs::read_dir(real.join("brif")) {
        lower.extend(ports.flatten().map(|e| e.path()));
    }
    for port in lower {
        net_controllers(&port, out);
    }
}

/// The interface of the default route in `routes` (`ip route show default`), if it has one.
pub fn default_nic(routes: &str) -> Option<&str> {
    let mut tokens = routes.split_whitespace();
    while let Some(token) = tokens.next() {
        if token == "dev" {
            return tokens
                .next()
                .filter(|nic| !nic.contains('/') && !nic.starts_with('.'));
        }
    }
    None
}

/// The `(major, minor)` of a device number from `st_dev`, as the Linux kernel encodes it.
pub fn dev_numbers(dev: u64) -> (u32, u32) {
    let major = ((dev >> 8) & 0xfff) | ((dev >> 32) & !0xfff);
    let minor = (dev & 0xff) | ((dev >> 12) & !0xff);
    (major as u32, minor as u32)
}

fn protection(sysfs: &Path, root_dev: Option<(u32, u32)>, nic: Option<&str>) -> Protection {
    let mut protection = Protection::default();
    if let Some((major, minor)) = root_dev {
        let block = sysfs.join(format!("dev/block/{major}:{minor}"));
        if block.exists() {
            protection.root_known = true;
            block_controllers(&block, &mut protection.root);
        }
    }
    if let Some(nic) = nic {
        net_controllers(&sysfs.join("class/net").join(nic), &mut protection.nic);
    }
    if let Ok(devices) = std::fs::read_dir(sysfs.join("bus/usb/devices")) {
        for device in devices.flatten() {
            let name = device.file_name();
            if name.to_string_lossy().starts_with("usb") {
                continue;
            }
            protection.usb.extend(controller_of(&device.path()));
        }
    }
    protection
}

/// Why a device must not be bound, in the order the shell rules check them.
fn protected_reason(
    bdf: &str,
    group: Option<u32>,
    exclusive: bool,
    protection: &Protection,
) -> Option<&'static str> {
    if group.is_none() {
        Some("no_iommu_group")
    } else if !exclusive {
        Some("shared_iommu_group")
    } else if !protection.root_known || protection.root.contains(bdf) {
        // An unresolved root disk protects every device: fail closed.
        Some("root_disk")
    } else if protection.nic.contains(bdf) {
        Some("default_route_nic")
    } else if protection.usb.contains(bdf) {
        Some("usb_in_use")
    } else {
        None
    }
}

/// Reads a sysfs attribute such as `vendor` (`0x1022`) as lowercase hex without `0x`.
pub(crate) fn sysfs_hex(dev: &Path, attribute: &str, digits: usize) -> String {
    std::fs::read_to_string(dev.join(attribute))
        .map(|text| text.trim().trim_start_matches("0x").to_ascii_lowercase())
        .ok()
        .filter(|text| text.len() == digits)
        .unwrap_or_default()
}

/// The name of the driver bound to a sysfs device, if one is bound.
pub(crate) fn sysfs_driver(dev: &Path) -> Option<String> {
    let link = std::fs::read_link(dev.join("driver")).ok()?;
    link.file_name()?.to_str().map(str::to_owned)
}

/// Every PCI device under `sysfs` except bridges, as candidates. `root_dev` is the root
/// filesystem's device number, `default_route` the text of `ip route show default`, and
/// `accessible` says whether `/dev/vfio/<group>` is usable by this process.
pub fn candidates_at(
    sysfs: &Path,
    root_dev: Option<(u32, u32)>,
    default_route: &str,
    accessible: impl Fn(u32) -> bool,
) -> Vec<HostPciCandidate> {
    let protection = protection(sysfs, root_dev, default_nic(default_route));
    let mut devices: Vec<PathBuf> = std::fs::read_dir(sysfs.join("bus/pci/devices"))
        .map(|entries| entries.flatten().map(|e| e.path()).collect())
        .unwrap_or_default();
    devices.sort();
    devices
        .into_iter()
        .filter_map(|dev| {
            let name = dev.file_name()?.to_str()?;
            let bdf = normalize_pci_bdf(name).ok()?;
            let class = sysfs_hex(&dev, "class", 6);
            if class.is_empty() || class.starts_with("06") {
                return None;
            }
            let iommu_group = std::fs::read_link(dev.join("iommu_group"))
                .ok()
                .and_then(|link| link.file_name()?.to_str()?.parse::<u32>().ok());
            let members: Vec<Option<String>> = iommu_group
                .and_then(|group| {
                    std::fs::read_dir(sysfs.join(format!("kernel/iommu_groups/{group}/devices")))
                        .ok()
                })
                .map(|members| {
                    members
                        .flatten()
                        .filter(|m| m.file_name().to_str() != Some(bdf.as_str()))
                        .map(|m| sysfs_driver(&m.path()))
                        .collect()
                })
                .unwrap_or_default();
            let drivers: Vec<Option<&str>> = members.iter().map(|d| d.as_deref()).collect();
            let exclusive = iommu_group.is_some() && super::group_exclusive(&drivers);
            let warning = std::fs::read_to_string(dev.join("boot_vga"))
                .ok()
                .filter(|text| text.trim() == "1")
                .map(|_| "boot_vga".to_owned());
            Some(candidate(CandidateFacts {
                protected_reason: protected_reason(&bdf, iommu_group, exclusive, &protection)
                    .map(str::to_owned),
                vendor_id: sysfs_hex(&dev, "vendor", 4),
                product_id: sysfs_hex(&dev, "device", 4),
                driver: sysfs_driver(&dev),
                group_accessible: iommu_group.is_some_and(&accessible),
                group_exclusive: exclusive,
                iommu_group,
                class,
                bdf,
                warning,
            }))
        })
        .collect()
}

/// The protocol a helper reports from `--check`, such as `stackbox-vfio-bind protocol=1`.
pub fn parse_helper_check(text: &str) -> Option<u32> {
    let rest = text.split_once("stackbox-vfio-bind protocol=")?.1;
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse().ok()
}

/// The command an agent runs before the helper's verb and device: the helper through sudo, which
/// the installed sudoers rule lets the agent user run without a password.
pub fn sudo_command() -> Vec<String> {
    vec!["sudo".into(), "-n".into(), HELPER_PATH.into()]
}

/// The command a coordinator runs over SSH to bind (`bind`) or unbind (`unbind`) a device.
pub fn helper_command(verb: &str, bdf: &str) -> String {
    format!("sudo -n {HELPER_PATH} {verb} {bdf}\n")
}

/// Whether the helper's output reports that it did `verb` on `bdf`.
pub fn helper_reported(output: &str, verb: &str, bdf: &str) -> bool {
    let marker = format!("STACKBOX\tpci_{verb}\t{bdf}\t");
    output.lines().any(|line| line.starts_with(&marker))
}

/// Checks an agent user name as sudoers and useradd accept it: `[a-z_][a-z0-9_-]*`.
pub fn valid_agent_user(user: &str) -> bool {
    let bytes = user.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 32
        && (bytes[0].is_ascii_lowercase() || bytes[0] == b'_')
        && bytes
            .iter()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'_' | b'-'))
}

/// The self-contained script that installs the helper and its sudoers rule for `agent_user`, the
/// user the coordinator's agent runs `sudo -n` as. Run it on the host with a paste. An invalid
/// user yields a script that fails without changing the host.
///
/// The root-side body lives in `vfio_install.sh`: one function per step (`write_helper`,
/// `install_sudoers`, `verify_helper`, `check_vfio_group`, `print_success`) and a `main`.
pub fn render_vfio_helper_install_script(agent_user: &str) -> String {
    if !valid_agent_user(agent_user) {
        return "echo 'stackbox: the agent user must match [a-z_][a-z0-9_-]*' >&2\nexit 1\n"
            .to_owned();
    }
    // The user is checked above, so it is safe inside the quotes; the helper goes in last so its
    // text is never scanned for placeholders.
    let body = INSTALL
        .replace("@AGENT_USER@", agent_user)
        .replace("@HELPER_PATH@", HELPER_PATH)
        .replace("@SUDOERS_PATH@", SUDOERS_PATH)
        .replace("@HELPER_PROTOCOL@", &HELPER_PROTOCOL.to_string())
        .replace("@HELPER@", &helper_script());
    format!("sudo -v && sudo bash <<'STACKBOX_VFIO_EOF'\n{body}STACKBOX_VFIO_EOF\n")
}

/// The helper install a host needs, as the portal copies it.
#[derive(Clone, Debug, Serialize, Deserialize, ToSchema)]
pub struct PciHelperInstall {
    /// A self-contained shell script; paste it into a root-capable shell on the host.
    pub script: String,
    pub helper_path: String,
    pub sudoers_path: String,
}

/// Binds one PCI device on a host.
#[derive(Clone, Debug, Serialize, Deserialize, ToSchema)]
pub struct PciBindRequest {
    /// The device's address, `DDDD:BB:DD.F`; upper-case hex is accepted and lowered.
    pub bdf: String,
}

/// Reads this machine's own candidates, with the root disk and default route the way the helper
/// sees them, and vfio group access checked as the agent's user.
pub fn local_candidates(route_text: &str) -> Vec<HostPciCandidate> {
    #[cfg(unix)]
    let root_dev = std::fs::metadata("/").ok().map(|meta| {
        use std::os::unix::fs::MetadataExt;
        dev_numbers(meta.dev())
    });
    #[cfg(not(unix))]
    let root_dev = None;
    candidates_at(
        Path::new("/sys"),
        root_dev,
        route_text,
        super::host::vfio_group_accessible,
    )
}

/// Whether a helper result for `verb` names `bdf`, as an error when it does not.
pub fn ensure_reported(output: &str, verb: &str, bdf: &str) -> Result<()> {
    if helper_reported(output, verb, bdf) {
        Ok(())
    } else {
        bail!("the host's stackbox-vfio-bind helper did not confirm {verb} {bdf}")
    }
}

/// Whether an installed helper reporting `version` is the one this coordinator speaks.
pub fn helper_matches(version: Option<u32>) -> bool {
    version == Some(HELPER_PROTOCOL)
}

/// Why a bind needs the helper reinstalled: the installed one speaks another protocol.
pub fn outdated_helper_message(version: u32) -> String {
    format!(
        "installed stackbox-vfio-bind helper (protocol {version}) is outdated; reinstall protocol {HELPER_PROTOCOL} on this host"
    )
}

/// Longest helper reason kept in a job error.
const REASON_LIMIT: usize = 240;

/// The error for a helper that exited non-zero, naming the reason its stderr gave.
pub fn helper_failure(verb: &str, bdf: &str, stderr: &[u8]) -> anyhow::Error {
    anyhow!(
        "the PCI helper failed to {verb} {bdf}: {}",
        helper_reason(&String::from_utf8_lossy(stderr))
    )
}

/// The reason in a helper's stderr: its `stackbox-vfio-bind:` line when it wrote one, else the
/// last meaningful line (sudo's own complaints, for one). Control characters are dropped and the
/// text is cut to [`REASON_LIMIT`] characters.
pub fn helper_reason(stderr: &str) -> String {
    let lines: Vec<String> = stderr
        .lines()
        .map(|line| {
            line.chars()
                .filter(|c| !c.is_control())
                .collect::<String>()
                .trim()
                .to_owned()
        })
        .filter(|line| !line.is_empty())
        .collect();
    let chosen = lines
        .iter()
        .rev()
        .find(|line| line.starts_with("stackbox-vfio-bind:"))
        .or(lines.last());
    match chosen {
        None => "no reason reported".to_owned(),
        Some(text) if text.chars().count() > REASON_LIMIT => {
            let cut: String = text.chars().take(REASON_LIMIT).collect();
            format!("{cut}...")
        }
        Some(text) => text.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn class_names_follow_the_class_prefix() {
        assert_eq!(class_name("030200"), "gpu");
        assert_eq!(class_name("010802"), "nvme");
        assert_eq!(class_name("010601"), "storage");
        assert_eq!(class_name("020000"), "network");
        assert_eq!(class_name("040300"), "audio");
        assert_eq!(class_name("0c0330"), "usb");
        assert_eq!(class_name("118000"), "other");
    }

    #[test]
    fn a_default_route_names_its_interface_and_nothing_else() {
        let text = "default via 192.0.2.1 dev enp3s0 proto dhcp metric 100\n";
        assert_eq!(default_nic(text), Some("enp3s0"));
        assert_eq!(default_nic(""), None);
        assert_eq!(default_nic("default dev ../evil"), None);
    }

    #[test]
    fn device_numbers_decode_the_kernel_encoding() {
        // 8:1 as st_dev: major in bits 8..20, minor split across bits 0..8 and 20..
        let dev = (8u64 << 8) | 1;
        assert_eq!(dev_numbers(dev), (8, 1));
        // Minor 300 splits: its low byte in bits 0..8, the rest from bit 20.
        let wide = 44u64 | (259u64 << 8) | (256u64 << 12);
        assert_eq!(dev_numbers(wide), (259, 300));
    }

    #[test]
    fn helper_check_reports_its_protocol() {
        assert_eq!(
            parse_helper_check("stackbox-vfio-bind protocol=1\n"),
            Some(1)
        );
        assert_eq!(parse_helper_check("protocol=1"), None);
        assert_eq!(parse_helper_check(""), None);
    }

    #[test]
    fn only_the_current_protocol_is_available() {
        assert!(helper_matches(Some(HELPER_PROTOCOL)));
        assert!(!helper_matches(Some(1)));
        assert!(!helper_matches(None));
        assert!(outdated_helper_message(1).contains("outdated"));
    }

    #[test]
    fn the_failure_reason_is_the_helper_line_cleaned_and_cut() {
        let stderr = "sudo: noise\nstackbox-vfio-bind: 0000:02:00.0 is protected (root_disk) and cannot be passed through\n";
        assert_eq!(
            helper_reason(stderr),
            "stackbox-vfio-bind: 0000:02:00.0 is protected (root_disk) and cannot be passed through"
        );
        assert_eq!(
            helper_reason("\n  \nsudo: a password is required\n"),
            "sudo: a password is required"
        );
        assert_eq!(
            helper_reason(" \x1b[31mstackbox-vfio-bind: bad\x07\n"),
            "[31mstackbox-vfio-bind: bad"
        );
        assert_eq!(helper_reason(""), "no reason reported");
        let long = format!("stackbox-vfio-bind: {}", "x".repeat(400));
        let reason = helper_reason(&long);
        assert_eq!(reason.chars().count(), REASON_LIMIT + 3);
        assert!(reason.ends_with("..."));
        let error = helper_failure("bind", "0000:02:00.0", stderr.as_bytes()).to_string();
        assert!(
            error.contains("stackbox-vfio-bind: 0000:02:00.0 is protected (root_disk)"),
            "{error}"
        );
    }

    #[test]
    fn only_a_confirmed_result_for_the_same_device_counts() {
        let output = "STACKBOX\tpci_bind\t0000:04:00.3\tvfio-pci\t12\n";
        assert!(helper_reported(output, "bind", "0000:04:00.3"));
        assert!(!helper_reported(output, "unbind", "0000:04:00.3"));
        assert!(!helper_reported(output, "bind", "0000:04:00.1"));
        assert!(ensure_reported("", "bind", "0000:04:00.3").is_err());
    }

    #[test]
    fn agent_users_are_validated_before_they_reach_sudoers() {
        assert!(valid_agent_user("stackbox"));
        assert!(valid_agent_user("_svc-1"));
        assert!(!valid_agent_user(""));
        assert!(!valid_agent_user("Root"));
        assert!(!valid_agent_user("a b"));
        assert!(!valid_agent_user("x\nALL=(ALL) NOPASSWD: ALL"));
        let refused = render_vfio_helper_install_script("a b");
        assert!(refused.contains("exit 1"));
        assert!(!refused.contains("NOPASSWD"));
    }

    #[test]
    fn the_rendered_sudoers_rule_passes_visudo() {
        let visudo = Path::new("/usr/sbin/visudo");
        if !visudo.exists() {
            return;
        }
        let script = render_vfio_helper_install_script("dev");
        let start = script
            .find("cat > \"$sudoers_tmp\" <<STACKBOX_SUDOERS_EOF\n")
            .expect("sudoers heredoc")
            + "cat > \"$sudoers_tmp\" <<STACKBOX_SUDOERS_EOF\n".len();
        let end = script[start..]
            .find("\nSTACKBOX_SUDOERS_EOF")
            .expect("heredoc end");
        // The heredoc is unquoted, so the shell expands $agent_user and leaves `\:` untouched.
        let rule = script[start..start + end].replace("$agent_user", "dev") + "\n";
        assert!(
            rule.contains("NOPASSWD: /usr/local/libexec/stackbox-vfio-bind\n"),
            "rule: {rule}"
        );
        assert!(!rule.contains('*'), "wildcards are not allowed: {rule}");
        let path =
            std::env::temp_dir().join(format!("stackbox-vfio-sudoers-test-{}", std::process::id()));
        std::fs::write(&path, &rule).unwrap();
        let out = std::process::Command::new(visudo)
            .arg("-cf")
            .arg(&path)
            .output()
            .unwrap();
        let _ = std::fs::remove_file(&path);
        assert!(
            out.status.success(),
            "visudo rejected the rule:\n{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }

    #[test]
    fn the_install_script_writes_the_helper_and_a_checked_sudoers_rule() {
        let script = render_vfio_helper_install_script("stackbox");
        assert!(script.starts_with("sudo -v && sudo bash <<'STACKBOX_VFIO_EOF'"));
        assert!(script.contains("STACKBOX_VFIO_EOF\n"));
        assert!(script.contains("agent_user='stackbox'"));
        assert!(
            script.contains(
                "$agent_user ALL=(root) NOPASSWD: /usr/local/libexec/stackbox-vfio-bind\n"
            )
        );
        assert!(!script.contains("0000\\:*"));
        assert!(script.contains("visudo -cf"));
        assert!(script.contains("chmod 0440"));
        assert!(script.contains("STACKBOX_HELPER_EOF"));
        assert!(script.contains(&format!("PROTOCOL_VERSION={HELPER_PROTOCOL}\n")));
    }

    /// The installer is one function per step, and `main` calls them in order.
    #[test]
    fn the_install_script_is_split_into_steps_called_from_main() {
        let script = render_vfio_helper_install_script("stackbox");
        for func in [
            "write_helper() {",
            "install_sudoers() {",
            "verify_helper() {",
            "check_vfio_group() {",
            "print_success() {",
        ] {
            assert!(script.contains(func), "missing {func}");
        }
        let order = [
            "    write_helper\n",
            "    install_sudoers\n",
            "    verify_helper\n",
            "    check_vfio_group\n",
            "    print_success\n",
        ];
        let mut at = script.find("main() {").expect("main");
        for call in order {
            let pos = script[at..]
                .find(call)
                .unwrap_or_else(|| panic!("no {call}"))
                + at;
            at = pos + call.len();
        }
        assert!(script.trim_end().ends_with("\nmain\nSTACKBOX_VFIO_EOF"));
    }

    /// sudo-rs's visudo rejects wildcards in command arguments, so the rule names the helper alone;
    /// the helper validates `--check`, `bind` and `unbind` and the BDF itself.
    #[test]
    fn the_install_script_grants_the_helper_without_argument_wildcards() {
        let script = render_vfio_helper_install_script("stackbox");
        assert!(
            script.contains(
                "$agent_user ALL=(root) NOPASSWD: /usr/local/libexec/stackbox-vfio-bind\n"
            )
        );
        assert!(!script.contains("0000\\:*"));
        assert!(!script.contains("0000:*"));
        assert!(!script.contains("bind 0000"));
    }

    /// `print_success` prints the confirmation the operator reads, with the group line filled in:
    /// already a member, or the usermod command to run.
    #[test]
    fn print_success_reports_the_helper_sudoers_and_group() {
        let script = render_vfio_helper_install_script("stackbox");
        let body = script.split_once('\n').unwrap().1;
        let body = body.strip_suffix("STACKBOX_VFIO_EOF\n").unwrap();
        let body = body.trim_end().strip_suffix("main").unwrap();
        let dir = tempfile::tempdir().unwrap();
        for (note, expect) in [
            ("이미 포함됨", "VFIO 그룹(kvm): 이미 포함됨"),
            (
                "미포함 → sudo usermod -aG kvm stackbox 실행 후 재로그인 필요",
                "sudo usermod -aG kvm stackbox",
            ),
        ] {
            let path = dir.path().join("success.sh");
            let text = format!("{body}vfio_group=kvm\ngroup_note='{note}'\nprint_success\n");
            std::fs::write(&path, text).unwrap();
            let out = std::process::Command::new("bash")
                .arg(&path)
                .output()
                .unwrap();
            let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
            assert!(
                out.status.success(),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
            assert!(
                stdout.starts_with("✓ stackbox-vfio-bind 설치 완료!\n"),
                "{stdout}"
            );
            assert!(stdout.contains(&format!(
                "  - 헬퍼: /usr/local/libexec/stackbox-vfio-bind (protocol={HELPER_PROTOCOL})\n"
            )));
            assert!(stdout.contains(
                "  - sudoers: /etc/sudoers.d/stackbox-vfio (visudo 검증 통과, stackbox sudo -n 확인)\n"
            ));
            assert!(stdout.contains(expect), "{stdout}");
            assert!(stdout.contains(
                "→ 이제 Stackbox 포탈(/hosts)에서 'SSH 점검'을 누르면 원클릭 바인딩을 사용할 수 있습니다."
            ));
        }
    }
}

/// A fake host's sysfs for the PCI tests: one device per protection rule, the root disk's
/// controller, the default route's NIC and a USB controller with a device on it.
#[cfg(test)]
pub(crate) mod fixture {
    use std::fs;
    use std::path::{Path, PathBuf};

    /// One PCI device of the fixture: its class, ids, bound driver and IOMMU group.
    pub struct Spec {
        pub bdf: &'static str,
        pub class: &'static str,
        pub vendor: &'static str,
        pub product: &'static str,
        pub driver: Option<&'static str>,
        pub group: Option<u32>,
        pub boot_vga: bool,
    }

    /// The devices of the fixture. `0000:00:01.0` is a bridge and must never be a candidate.
    pub const DEVICES: &[Spec] = &[
        Spec {
            bdf: "0000:00:01.0",
            class: "060400",
            vendor: "8086",
            product: "9c01",
            driver: Some("pcieport"),
            group: None,
            boot_vga: false,
        },
        Spec {
            bdf: "0000:04:00.0",
            class: "030000",
            vendor: "10de",
            product: "2204",
            driver: Some("nvidia"),
            group: Some(12),
            boot_vga: true,
        },
        Spec {
            bdf: "0000:04:00.1",
            class: "040300",
            vendor: "10de",
            product: "1aef",
            driver: None,
            group: Some(12),
            boot_vga: false,
        },
        Spec {
            bdf: "0000:05:00.0",
            class: "010802",
            vendor: "144d",
            product: "a80a",
            driver: Some("nvme"),
            group: Some(13),
            boot_vga: false,
        },
        Spec {
            bdf: "0000:06:00.0",
            class: "020000",
            vendor: "8086",
            product: "1502",
            driver: Some("e1000e"),
            group: Some(14),
            boot_vga: false,
        },
        Spec {
            bdf: "0000:07:00.0",
            class: "0c0330",
            vendor: "1912",
            product: "0014",
            driver: Some("xhci_hcd"),
            group: Some(15),
            boot_vga: false,
        },
        Spec {
            bdf: "0000:08:00.0",
            class: "010601",
            vendor: "8086",
            product: "2922",
            driver: None,
            group: Some(16),
            boot_vga: false,
        },
        Spec {
            bdf: "0000:08:00.1",
            class: "010601",
            vendor: "8086",
            product: "2922",
            driver: Some("ahci"),
            group: Some(16),
            boot_vga: false,
        },
        Spec {
            bdf: "0000:09:00.0",
            class: "030200",
            vendor: "10de",
            product: "2230",
            driver: Some("vfio-pci"),
            group: Some(17),
            boot_vga: false,
        },
        Spec {
            bdf: "0000:0a:00.0",
            class: "020000",
            vendor: "8086",
            product: "1533",
            driver: None,
            group: None,
            boot_vga: false,
        },
        Spec {
            bdf: "0000:0b:00.0",
            class: "020000",
            vendor: "8086",
            product: "1533",
            driver: None,
            group: Some(18),
            boot_vga: false,
        },
    ];

    /// The default route the fixture's NIC `eth0` carries.
    pub const ROUTE: &str = "default via 10.0.0.1 dev eth0 proto dhcp metric 100\n";

    fn write(path: PathBuf, text: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    /// Creates the fixture tree under `root`, with the vfio-pci control files and the root disk,
    /// NIC and USB controller the protection rules read.
    pub fn build(root: &Path) {
        for name in [
            "nvidia", "nvme", "e1000e", "xhci_hcd", "ahci", "pcieport", "vfio-pci",
        ] {
            fs::create_dir_all(root.join("bus/pci/drivers").join(name)).unwrap();
        }
        write(root.join("bus/pci/drivers/vfio-pci/bind"), "");
        write(root.join("bus/pci/drivers/vfio-pci/unbind"), "");
        write(root.join("bus/pci/drivers_probe"), "");
        for spec in DEVICES {
            let real = root.join("devices/pci0000:00").join(spec.bdf);
            fs::create_dir_all(&real).unwrap();
            write(real.join("class"), &format!("0x{}\n", spec.class));
            write(real.join("vendor"), &format!("0x{}\n", spec.vendor));
            write(real.join("device"), &format!("0x{}\n", spec.product));
            write(real.join("driver_override"), "");
            if spec.boot_vga {
                write(real.join("boot_vga"), "1\n");
            }
            let bus = root.join("bus/pci/devices");
            fs::create_dir_all(&bus).unwrap();
            std::os::unix::fs::symlink(
                format!("../../../devices/pci0000:00/{}", spec.bdf),
                bus.join(spec.bdf),
            )
            .unwrap();
            if let Some(driver) = spec.driver {
                std::os::unix::fs::symlink(
                    format!("../../../bus/pci/drivers/{driver}"),
                    real.join("driver"),
                )
                .unwrap();
            }
            if let Some(group) = spec.group {
                fs::create_dir_all(root.join(format!("kernel/iommu_groups/{group}/devices")))
                    .unwrap();
                std::os::unix::fs::symlink(
                    format!("../../../kernel/iommu_groups/{group}"),
                    real.join("iommu_group"),
                )
                .unwrap();
                std::os::unix::fs::symlink(
                    format!("../../../../devices/pci0000:00/{}", spec.bdf),
                    root.join(format!("kernel/iommu_groups/{group}/devices/{}", spec.bdf)),
                )
                .unwrap();
            }
        }
        // The root disk: nvme0n1p1 sits under the NVMe controller 0000:05:00.0.
        let root_part = root.join("devices/pci0000:00/0000:05:00.0/nvme/nvme0n1/nvme0n1p1");
        fs::create_dir_all(&root_part).unwrap();
        fs::create_dir_all(root.join("dev/block")).unwrap();
        std::os::unix::fs::symlink(
            "../../devices/pci0000:00/0000:05:00.0/nvme/nvme0n1/nvme0n1p1",
            root.join("dev/block/8:1"),
        )
        .unwrap();
        // The default route's NIC: eth0 on 0000:06:00.0.
        fs::create_dir_all(root.join("devices/pci0000:00/0000:06:00.0/net/eth0")).unwrap();
        fs::create_dir_all(root.join("class/net")).unwrap();
        std::os::unix::fs::symlink(
            "../../devices/pci0000:00/0000:06:00.0/net/eth0",
            root.join("class/net/eth0"),
        )
        .unwrap();
        // A keyboard on the xHCI controller 0000:07:00.0, behind its root hub usb1.
        fs::create_dir_all(root.join("devices/pci0000:00/0000:07:00.0/usb1/1-1")).unwrap();
        fs::create_dir_all(root.join("bus/usb/devices")).unwrap();
        std::os::unix::fs::symlink(
            "../../../devices/pci0000:00/0000:07:00.0/usb1",
            root.join("bus/usb/devices/usb1"),
        )
        .unwrap();
        std::os::unix::fs::symlink(
            "../../../devices/pci0000:00/0000:07:00.0/usb1/1-1",
            root.join("bus/usb/devices/1-1"),
        )
        .unwrap();
    }

    /// The environment that points the installed helper at the fixture, as a test process runs it.
    pub fn helper_env(sysfs: &Path, dev: &Path, conf: &Path, root_dev: &str) -> Vec<String> {
        vec![
            format!("STACKBOX_TEST_SYSFS_ROOT={}", sysfs.display()),
            format!("STACKBOX_TEST_DEV_ROOT={}", dev.display()),
            format!("STACKBOX_TEST_VFIO_CONF={}", conf.display()),
            format!("STACKBOX_TEST_ROOT_DEV={root_dev}"),
            format!("STACKBOX_TEST_DEFAULT_ROUTE={ROUTE}"),
        ]
    }

    /// Whether the test runs as root: the helper then ignores the test seams and would touch the
    /// real sysfs, so the shell tests refuse to run.
    pub fn running_as_root() -> bool {
        #[cfg(unix)]
        {
            // SAFETY: geteuid has no preconditions and cannot fail.
            unsafe { libc::geteuid() == 0 }
        }
        #[cfg(not(unix))]
        {
            false
        }
    }
}

#[cfg(test)]
mod fixture_tests {
    use super::fixture::{ROUTE, build};
    use super::*;
    use std::fs;

    fn scan(root: &Path, accessible: impl Fn(u32) -> bool) -> Vec<HostPciCandidate> {
        candidates_at(root, Some((8, 1)), ROUTE, accessible)
    }

    fn find<'a>(candidates: &'a [HostPciCandidate], bdf: &str) -> &'a HostPciCandidate {
        candidates
            .iter()
            .find(|c| c.bdf == bdf)
            .unwrap_or_else(|| panic!("{bdf} is not a candidate"))
    }

    #[test]
    fn every_device_but_bridges_is_a_candidate_with_its_protection_reason() {
        let dir = tempfile::tempdir().unwrap();
        build(dir.path());
        let candidates = scan(dir.path(), |_| false);
        assert!(
            candidates.iter().all(|c| c.bdf != "0000:00:01.0"),
            "a bridge is never a candidate"
        );
        assert_eq!(candidates.len(), 10);
        let reason = |bdf| find(&candidates, bdf).protected_reason.as_deref();
        assert_eq!(reason("0000:05:00.0"), Some("root_disk"));
        assert_eq!(reason("0000:06:00.0"), Some("default_route_nic"));
        assert_eq!(reason("0000:07:00.0"), Some("usb_in_use"));
        assert_eq!(reason("0000:08:00.0"), Some("shared_iommu_group"));
        assert_eq!(
            reason("0000:08:00.1"),
            None,
            "its sibling is unbound, so the group is exclusive"
        );
        assert_eq!(reason("0000:0a:00.0"), Some("no_iommu_group"));
        assert_eq!(reason("0000:04:00.1"), Some("shared_iommu_group"));
    }

    #[test]
    fn a_free_device_is_bindable_and_the_display_gpu_carries_a_warning() {
        let dir = tempfile::tempdir().unwrap();
        build(dir.path());
        let candidates = scan(dir.path(), |_| false);
        let gpu = find(&candidates, "0000:04:00.0");
        assert_eq!(gpu.class_name, "gpu");
        assert!(gpu.bindable && gpu.group_exclusive && gpu.protected_reason.is_none());
        assert_eq!(gpu.warning.as_deref(), Some("boot_vga"));
        let free = find(&candidates, "0000:0b:00.0");
        assert!(free.bindable);
        assert_eq!(free.driver, None);
        assert!(!free.vfio_ready);
    }

    #[test]
    fn a_vfio_bound_device_is_ready_once_its_group_is_accessible() {
        let dir = tempfile::tempdir().unwrap();
        build(dir.path());
        let not_accessible = scan(dir.path(), |_| false);
        let held = find(&not_accessible, "0000:09:00.0");
        assert_eq!(held.driver.as_deref(), Some("vfio-pci"));
        assert!(held.bindable && !held.vfio_ready);
        let accessible = scan(dir.path(), |group| group == 17);
        let ready = find(&accessible, "0000:09:00.0");
        assert!(ready.vfio_ready && ready.bindable);
        assert_eq!(ready.class_name, "gpu");
    }

    #[test]
    fn an_unknown_root_disk_protects_every_device() {
        let dir = tempfile::tempdir().unwrap();
        build(dir.path());
        let candidates = candidates_at(dir.path(), None, ROUTE, |_| false);
        let free = find(&candidates, "0000:0b:00.0");
        assert_eq!(free.protected_reason.as_deref(), Some("root_disk"));
        assert!(
            !free.bindable,
            "fail closed when the root disk cannot be resolved"
        );
    }

    #[test]
    fn the_default_route_names_the_nic_and_a_bad_name_is_ignored() {
        assert_eq!(default_nic(ROUTE), Some("eth0"));
        assert_eq!(default_nic("default dev ../../x"), None);
    }

    /// `sh -n` accepts both shell scripts as generated, with the guard in place.
    #[test]
    fn the_helper_and_the_inspection_parse_as_sh() {
        let dir = tempfile::tempdir().unwrap();
        let helper = dir.path().join("stackbox-vfio-bind");
        fs::write(&helper, helper_script()).unwrap();
        let inspect = dir.path().join("inspect.sh");
        fs::write(&inspect, crate::remote::ssh::inspect_script()).unwrap();
        for script in [helper, inspect] {
            let out = std::process::Command::new("sh")
                .arg("-n")
                .arg(&script)
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "sh -n {}: {}",
                script.display(),
                String::from_utf8_lossy(&out.stderr)
            );
        }
    }

    /// The install script the portal copies parses as bash, and its embedded helper is the same
    /// script the agent runs.
    #[test]
    fn the_install_script_parses_as_bash() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("install.sh");
        fs::write(&path, render_vfio_helper_install_script("stackbox")).unwrap();
        let out = std::process::Command::new("bash")
            .arg("-n")
            .arg(&path)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

#[cfg(test)]
mod helper_shell_tests {
    use super::fixture::{build, helper_env, running_as_root};
    use super::*;
    use crate::remote::HostInspection;
    use std::fs;
    use std::process::Command;

    /// Runs the installed helper's script as this test user with the fixture's seams.
    struct Helper {
        _dir: tempfile::TempDir,
        script: PathBuf,
        env: Vec<String>,
        sysfs: PathBuf,
        dev: PathBuf,
    }

    fn helper() -> Option<Helper> {
        helper_with_root("8:1")
    }

    /// The helper whose root device, as `findmnt` reported it, is `root_dev`.
    fn helper_with_root(root_dev: &str) -> Option<Helper> {
        if running_as_root() {
            eprintln!("skipping the helper shell tests: root ignores the test seams");
            return None;
        }
        let dir = tempfile::tempdir().unwrap();
        let sysfs = dir.path().join("sys");
        let dev = dir.path().join("dev");
        build(&sysfs);
        fs::create_dir_all(dev.join("vfio")).unwrap();
        fs::write(dev.join("vfio/18"), "").unwrap();
        fs::write(dev.join("vfio/17"), "").unwrap();
        let group = Command::new("id").arg("-gn").output().unwrap();
        let group = String::from_utf8_lossy(&group.stdout).trim().to_owned();
        let conf = dir.path().join("vfio.conf");
        fs::write(&conf, format!("GROUP={group}\n")).unwrap();
        let script = dir.path().join("stackbox-vfio-bind");
        fs::write(&script, helper_script()).unwrap();
        let env = helper_env(&sysfs, &dev, &conf, root_dev);
        Some(Helper {
            _dir: dir,
            script,
            env,
            sysfs,
            dev,
        })
    }

    impl Helper {
        fn run(&self, args: &[&str]) -> std::process::Output {
            Command::new("env")
                .args(&self.env)
                .arg("sh")
                .arg(&self.script)
                .args(args)
                .output()
                .unwrap()
        }
    }

    fn text(output: &[u8]) -> String {
        String::from_utf8_lossy(output).into_owned()
    }

    #[test]
    fn the_helper_answers_check_and_version_without_side_effects() {
        let Some(helper) = helper() else { return };
        let check = helper.run(&["--check"]);
        assert!(check.status.success());
        assert_eq!(
            text(&check.stdout).trim(),
            format!("stackbox-vfio-bind protocol={HELPER_PROTOCOL}")
        );
        let version = helper.run(&["--version"]);
        assert_eq!(text(&version.stdout).trim(), HELPER_PROTOCOL.to_string());
        assert_eq!(
            fs::read_to_string(helper.sysfs.join("bus/pci/devices/0000:0b:00.0/../../../devices/pci0000:00/0000:0b:00.0/driver_override")).unwrap(),
            "",
            "the check left the device alone"
        );
    }

    #[test]
    fn a_malformed_or_out_of_range_address_is_refused_before_sysfs_is_touched() {
        let Some(helper) = helper() else { return };
        for bdf in [
            "0000:0B:00.0",
            "0000:0b:00.8",
            "0000:04:20.0",
            "0000:0b",
            "../../etc/passwd",
            "0000:0b:00.0;id",
        ] {
            let out = helper.run(&["bind", bdf]);
            assert!(!out.status.success(), "{bdf} was accepted");
            assert!(
                text(&out.stderr).contains("invalid PCI address")
                    || text(&out.stderr).contains("above 0x1f"),
                "{bdf}: {}",
                text(&out.stderr)
            );
        }
    }

    #[test]
    fn a_bridge_and_each_protected_device_are_refused_and_left_alone() {
        let Some(helper) = helper() else { return };
        for (bdf, reason) in [
            ("0000:00:01.0", "bridge"),
            ("0000:05:00.0", "root_disk"),
            ("0000:06:00.0", "default_route_nic"),
            ("0000:07:00.0", "usb_in_use"),
            ("0000:08:00.0", "shared_iommu_group"),
            ("0000:0a:00.0", "no_iommu_group"),
        ] {
            let out = helper.run(&["bind", bdf]);
            assert!(!out.status.success(), "{bdf} was bound");
            assert!(
                text(&out.stderr).contains(reason),
                "{bdf}: {}",
                text(&out.stderr)
            );
            let override_file = helper
                .sysfs
                .join("devices/pci0000:00")
                .join(bdf)
                .join("driver_override");
            assert_eq!(
                fs::read_to_string(override_file).unwrap(),
                "",
                "{bdf} was overridden"
            );
        }
    }

    #[test]
    fn a_free_device_binds_to_vfio_pci_and_its_group_node_is_handed_over() {
        let Some(helper) = helper() else { return };
        let out = helper.run(&["bind", "0000:0b:00.0"]);
        assert!(out.status.success(), "{}", text(&out.stderr));
        assert!(
            text(&out.stdout).contains("STACKBOX\tpci_bind\t0000:0b:00.0\tvfio-pci\t18"),
            "{}",
            text(&out.stdout)
        );
        let device = helper.sysfs.join("devices/pci0000:00/0000:0b:00.0");
        assert_eq!(
            fs::read_to_string(device.join("driver_override")).unwrap(),
            "vfio-pci\n"
        );
        assert_eq!(
            fs::read_to_string(helper.sysfs.join("bus/pci/drivers/vfio-pci/bind")).unwrap(),
            "0000:0b:00.0\n"
        );
        let mode = fs::metadata(helper.dev.join("vfio/18")).unwrap();
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(mode.permissions().mode() & 0o777, 0o660);
    }

    /// `findmnt -no MAJ:MIN /` pads its column, so the root device can arrive as `"8:1  "` or with
    /// a leading space and a newline. The root disk is still found: only its controller is kept.
    #[test]
    fn a_padded_root_device_protects_only_the_root_controller() {
        for root in ["8:1  ", " 8:1 \n", "  8:1 \n"] {
            let Some(helper) = helper_with_root(root) else {
                return;
            };
            let root_disk = helper.run(&["bind", "0000:05:00.0"]);
            assert!(
                !root_disk.status.success(),
                "{root:?}: the root disk was bound"
            );
            assert!(
                text(&root_disk.stderr).contains("root_disk"),
                "{root:?}: {}",
                text(&root_disk.stderr)
            );
            let nic = helper.run(&["bind", "0000:06:00.0"]);
            assert!(
                text(&nic.stderr).contains("default_route_nic"),
                "{root:?}: {}",
                text(&nic.stderr)
            );
            let free = helper.run(&["bind", "0000:0b:00.0"]);
            assert!(free.status.success(), "{root:?}: {}", text(&free.stderr));
            assert!(
                text(&free.stdout).contains("STACKBOX\tpci_bind\t0000:0b:00.0\tvfio-pci\t18"),
                "{root:?}: {}",
                text(&free.stdout)
            );
        }
    }

    /// An empty, blank or malformed root device still fails closed: a free device is `root_disk`.
    #[test]
    fn an_empty_blank_or_malformed_root_device_still_fails_closed() {
        for root in ["", "   ", "../foo", "8:1:2"] {
            let Some(helper) = helper_with_root(root) else {
                return;
            };
            let out = helper.run(&["bind", "0000:0b:00.0"]);
            assert!(!out.status.success(), "{root:?} bound a free device");
            assert!(
                text(&out.stderr).contains("root_disk"),
                "{root:?}: {}",
                text(&out.stderr)
            );
        }
    }

    #[test]
    fn unbind_hands_back_only_a_vfio_pci_device() {
        let Some(helper) = helper() else { return };
        let out = helper.run(&["unbind", "0000:09:00.0"]);
        assert!(out.status.success(), "{}", text(&out.stderr));
        let device = helper.sysfs.join("devices/pci0000:00/0000:09:00.0");
        assert_eq!(
            fs::read_to_string(device.join("driver_override")).unwrap(),
            "\n"
        );
        assert_eq!(
            fs::read_to_string(helper.sysfs.join("bus/pci/drivers_probe")).unwrap(),
            "0000:09:00.0\n"
        );
        let other = helper.run(&["unbind", "0000:04:00.0"]);
        assert!(text(&other.stdout).contains("STACKBOX\tpci_unbind\t0000:04:00.0\tnvidia"));
        let untouched = helper
            .sysfs
            .join("devices/pci0000:00/0000:04:00.0/driver_override");
        assert_eq!(fs::read_to_string(untouched).unwrap(), "");
    }

    #[test]
    fn older_state_without_the_pci_fields_still_loads() {
        let inspection: HostInspection = serde_json::from_value(serde_json::json!({
            "os": "linux", "arch": "x86_64", "compatible": true, "kvm_available": true,
            "sudo_available": true, "memory_total_mib": 1, "memory_available_mib": 1,
            "disk_available_mib": 1, "msb_version": "0.8.1", "msb_compatible": true,
            "docker_available": false, "golden_image_available": false,
            "vm_boot_verified": false, "checked_at": 1
        }))
        .unwrap();
        assert!(inspection.pci_candidates.is_empty());
        assert!(!inspection.pci_bind_helper_available);
        assert_eq!(inspection.pci_bind_helper_version, None);
        let job: crate::remote::HostJob = serde_json::from_value(serde_json::json!({
            "id": "j", "host_id": "h", "kind": "check", "status": "succeeded",
            "events": [], "error": null, "created_at": 1, "finished_at": 2
        }))
        .unwrap();
        assert_eq!(job.pci_bdf, None);
        let candidate: HostPciCandidate = serde_json::from_value(serde_json::json!({
            "bdf": "0000:0b:00.0", "vendor_id": "8086", "product_id": "1533",
            "class": "020000", "class_name": "network"
        }))
        .unwrap();
        assert!(!candidate.bindable && candidate.protected_reason.is_none());
    }
}
