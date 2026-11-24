//! This machine as a stackbox host: what the coordinator's own agent runs on.

use super::pci::{sysfs_driver, sysfs_hex};
use super::{GpuKernel, HostInspection, HostPciDevice};
use anyhow::{Context, Result, ensure};
use std::{ffi::OsString, path::Path, sync::Mutex, time::Duration};

/// Set by the last verification boot: what its kernel reported about the GPU stack. A local
/// agent is a new process per request, so the `VerifyVm` reply must carry this; a later
/// `Inspect` in another process cannot remember it.
static GPU_KERNEL: Mutex<Option<GpuKernel>> = Mutex::new(None);

/// What this host's last verification VM read about the booted kernel's GPU features.
pub fn gpu_kernel() -> Option<GpuKernel> {
    GPU_KERNEL.lock().ok().and_then(|gpu| gpu.clone())
}

/// Whether this host's last verification VM saw the custom kernel's `mtty` mdev type.
pub fn mtty_available() -> bool {
    gpu_kernel().is_some_and(|gpu| gpu.mtty_available())
}

/// The PCI devices on this machine bound to `vfio-pci`, read from sysfs the way the inspection
/// script reads them. Other devices are not listed. Off Linux there is no sysfs, so none.
pub fn pci_devices() -> Vec<HostPciDevice> {
    let Ok(entries) = std::fs::read_dir("/sys/bus/pci/devices") else {
        return Vec::new();
    };
    let mut devices: Vec<_> = entries.flatten().map(|entry| entry.path()).collect();
    devices.sort();
    devices
        .into_iter()
        .filter(|dev| sysfs_driver(dev).as_deref() == Some("vfio-pci"))
        .map(|dev| {
            let group = std::fs::read_link(dev.join("iommu_group"))
                .ok()
                .and_then(|link| link.file_name()?.to_str()?.parse::<u32>().ok());
            let members: Vec<Option<String>> = group
                .and_then(|group| {
                    std::fs::read_dir(format!("/sys/kernel/iommu_groups/{group}/devices")).ok()
                })
                .map(|members| {
                    members
                        .flatten()
                        .map(|member| sysfs_driver(&member.path()))
                        .collect()
                })
                .unwrap_or_default();
            let drivers: Vec<Option<&str>> = members.iter().map(|d| d.as_deref()).collect();
            let exclusive = group.is_some() && super::group_exclusive(&drivers);
            let ready = exclusive && group.is_some_and(vfio_group_accessible);
            HostPciDevice {
                bdf: dev
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_ascii_lowercase(),
                vendor_id: sysfs_hex(&dev, "vendor", 4),
                product_id: sysfs_hex(&dev, "device", 4),
                class: sysfs_hex(&dev, "class", 6),
                iommu_group: group,
                driver: Some("vfio-pci".into()),
                group_exclusive: exclusive,
                vfio_ready: ready,
            }
        })
        .collect()
}

/// Whether this user can read and write `/dev/vfio/<group>`, checked with `access(2)` as
/// inspect.sh checks it. Opening the node instead would take a descriptor on a group that a
/// running VM may already hold.
#[cfg(unix)]
pub fn vfio_group_accessible(group: u32) -> bool {
    let Ok(node) = std::ffi::CString::new(format!("/dev/vfio/{group}")) else {
        return false;
    };
    // SAFETY: `node` is a valid NUL-terminated path that outlives the call.
    unsafe { libc::access(node.as_ptr(), libc::R_OK | libc::W_OK) == 0 }
}

#[cfg(not(unix))]
pub fn vfio_group_accessible(_group: u32) -> bool {
    false
}

/// Whether the stackbox-vfio-bind helper is installed root-owned and 0755 and answers `--check`
/// through sudo, with the protocol it reports.
async fn helper_status() -> (bool, Option<u32>) {
    use std::os::unix::fs::MetadataExt;
    let installed = std::fs::metadata(super::pci::HELPER_PATH)
        .is_ok_and(|meta| meta.uid() == 0 && meta.mode() & 0o777 == 0o755 && meta.is_file());
    if !installed {
        return (false, None);
    }
    let version = output("sudo", &["-n", super::pci::HELPER_PATH, "--check"])
        .await
        .and_then(|text| super::pci::parse_helper_check(&text));
    (version.is_some(), version)
}

/// Runs the helper's `bind` or `unbind` for `bdf` through sudo, as the coordinator's SSH hosts do,
/// and checks that it confirmed the change.
pub async fn run_pci_helper(command: &[String], verb: &str, bdf: &str) -> Result<()> {
    let Some((program, args)) = command.split_first() else {
        anyhow::bail!("no PCI helper command configured");
    };
    let mut child = tokio::process::Command::new(program);
    child
        .args(args)
        .arg(verb)
        .arg(bdf)
        .env("PATH", search_path())
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true);
    let run = tokio::time::timeout(Duration::from_secs(60), child.output())
        .await
        .context("the PCI helper timed out")?
        .context("cannot run sudo for the PCI helper")?;
    ensure!(
        run.status.success(),
        "the PCI helper failed to {verb} {bdf}: {}",
        String::from_utf8_lossy(&run.stderr).trim()
    );
    super::pci::ensure_reported(&String::from_utf8_lossy(&run.stdout), verb, bdf)
}

pub fn hypervisor() -> &'static str {
    if cfg!(target_os = "macos") {
        "hvf"
    } else {
        "kvm"
    }
}

/// Hypervisor.framework is always present on supported macOS; Linux needs a writable /dev/kvm.
pub fn hypervisor_available() -> bool {
    cfg!(target_os = "macos")
        || (cfg!(target_os = "linux")
            && std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open("/dev/kvm")
                .is_ok())
}

/// PATH with microsandbox's install directory first, as remote agents get it.
pub fn search_path() -> OsString {
    let home = std::env::var_os("HOME").unwrap_or_default();
    let mut paths = vec![
        Path::new(&home).join(".microsandbox/bin"),
        Path::new(&home).join(".local/bin"),
    ];
    if let Some(path) = std::env::var_os("PATH") {
        paths.extend(std::env::split_paths(&path));
    }
    std::env::join_paths(paths).unwrap_or_default()
}

async fn output(program: &str, args: &[&str]) -> Option<String> {
    let mut command = tokio::process::Command::new(program);
    command
        .args(args)
        .env("PATH", search_path())
        .env("MSB_BACKEND", "local")
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true);
    match tokio::time::timeout(Duration::from_secs(10), command.output()).await {
        Ok(Ok(output)) if output.status.success() => {
            Some(String::from_utf8_lossy(&output.stdout).into_owned())
        }
        _ => None,
    }
}

/// The same facts a remote host's inspection reports, read on this machine.
pub async fn inspect() -> HostInspection {
    let home = std::env::var_os("HOME").unwrap_or_default();
    let (total, available, disk) = tokio::task::spawn_blocking(move || {
        let system = sysinfo::System::new_all();
        let disks = sysinfo::Disks::new_with_refreshed_list();
        let disk = disks
            .iter()
            .filter(|d| Path::new(&home).starts_with(d.mount_point()))
            .max_by_key(|d| d.mount_point().as_os_str().len())
            .map_or(0, |d| d.available_space() / 1024 / 1024);
        (
            system.total_memory() / 1024 / 1024,
            system.available_memory() / 1024 / 1024,
            disk,
        )
    })
    .await
    .unwrap_or_default();
    let version = output("msb", &["--version"]).await.and_then(|text| {
        text.split_whitespace()
            .last()
            .filter(|v| v.len() < 32 && v.bytes().all(|b| b.is_ascii_digit() || b == b'.'))
            .map(str::to_owned)
    });
    // The upstream and fork msb share one version string; the run help tells them apart.
    let msb_pci_capable =
        super::msb_help_supports_pci(&output("msb", &["run", "--help"]).await.unwrap_or_default());
    let golden = crate::stackbox::golden::available(&crate::stackbox::golden::current())
        .await
        .unwrap_or(false);
    let docker = output("docker", &["version", "--format", "{{.Server.Version}}"])
        .await
        .is_some();
    let os = match std::env::consts::OS {
        "linux" => "linux",
        "macos" => "macos",
        _ => "unsupported",
    };
    let arch = match std::env::consts::ARCH {
        "aarch64" => "aarch64",
        "x86_64" => "x86_64",
        _ => "unsupported",
    };
    let kernel = crate::kernel::select_in_process();
    let pci = pci_devices();
    let route = output("ip", &["route", "show", "default"])
        .await
        .unwrap_or_default();
    let pci_candidates = super::pci::local_candidates(&route);
    let (pci_bind_helper_available, pci_bind_helper_version) = helper_status().await;
    let vfio_device = Path::new("/dev/vfio/vfio").exists();
    HostInspection {
        compatible: os != "unsupported" && arch != "unsupported",
        os: os.into(),
        arch: arch.into(),
        kvm_available: hypervisor_available(),
        sudo_available: false,
        memory_total_mib: total,
        memory_available_mib: available,
        disk_available_mib: disk,
        msb_compatible: version.as_deref() == Some(super::MSB_VERSION),
        msb_version: version,
        docker_available: docker,
        golden_image_available: golden,
        custom_kernel: kernel.path.as_ref().map(|path| path.display().to_string()),
        custom_kernel_reason: kernel.reason,
        mtty_available: mtty_available(),
        gpu_kernel: gpu_kernel(),
        msb_pci_capable,
        pci_passthrough_available: super::pci_passthrough_ready(
            kernel.path.is_some(),
            msb_pci_capable,
            vfio_device,
            &pci,
        ),
        pci_devices: pci,
        pci_candidates,
        pci_bind_helper_available,
        pci_bind_helper_version,
        vm_boot_verified: false,
        checked_at: crate::model::now(),
    }
}

/// Install or upgrade msb on this machine with the script SSH preparation runs.
pub async fn install_msb() -> Result<()> {
    ensure!(
        cfg!(any(
            target_os = "linux",
            all(target_os = "macos", target_arch = "aarch64")
        )),
        "msb installation is supported on Linux and Apple Silicon macOS; install msb {} by hand",
        super::MSB_VERSION
    );
    let mut command = tokio::process::Command::new("bash");
    command
        .args(["-c", &super::ssh::install_script()])
        .env("PATH", search_path())
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true);
    let output = tokio::time::timeout(Duration::from_secs(600), command.output())
        .await
        .context("msb installation timed out")?
        .context("cannot run bash")?;
    ensure!(
        output.status.success(),
        "msb {} installation failed (curl and tar required): {}",
        super::MSB_VERSION,
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(())
}

/// Boot, use and remove a small VM on this machine, as a remote host's preparation does.
/// Returns what the guest kernel reported about its GPU stack; the reply to the coordinator
/// carries it, as a process started for one request forgets it afterwards.
pub async fn smoke(token: &str) -> Result<GpuKernel> {
    let selection = crate::kernel::select_in_process();
    let kernel = selection
        .path
        .as_ref()
        .map(|path| path.display().to_string());
    let script = super::ssh::smoke_script(token, kernel.as_deref())?;
    let mut command = tokio::process::Command::new("bash");
    command
        .args(["-c", &script])
        .env("PATH", search_path())
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true);
    // The CLI boots the verification VM, so it needs the same kernel pair the SDK uses:
    // otherwise this host would verify the stock kernel and report no mtty device.
    if let Some(library) = &kernel {
        command
            .env("MSB_PATH", crate::kernel::msb_path())
            .env("MSB_LIBKRUNFW_PATH", library);
    }
    let output = tokio::time::timeout(Duration::from_secs(180), command.output())
        .await
        .context("VM boot verification timed out")?
        .context("cannot run bash")?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    // Two failures are different: an msb that could not boot or run in the VM leaves a broken
    // host, while a boot whose kernel has no mtty leaves a host that is usable but cannot run
    // an mtty GPU stackbox. The script prints STACKBOX-MDEV 0 for the second, so a non-zero exit is
    // the first and a missing marker is a probe that did not run.
    ensure!(
        output.status.success(),
        "VM boot verification failed: msb create/exec/remove on this host did not succeed: {}{}",
        String::from_utf8_lossy(&output.stderr).trim(),
        if stdout.trim().is_empty() {
            String::new()
        } else {
            format!(" (stdout: {})", stdout.trim())
        }
    );
    let gpu = super::ssh::parse_gpu_kernel(&stdout)?;
    // Both markers describe the same kernel; a disagreement means the probe ran wrong.
    ensure!(
        super::ssh::parse_mdev(&stdout)? == gpu.mtty_available(),
        "this host's verification VM reported disagreeing mtty markers: {}",
        stdout.trim()
    );
    let gpu = gpu.for_kernel(
        selection.path.is_some(),
        selection.id.clone(),
        if selection.path.is_some() {
            None
        } else {
            selection.reason.clone()
        },
    );
    // Kept for this process's later `Inspect`, which a dial-in agent answers from the same one.
    if let Ok(mut stored) = GPU_KERNEL.lock() {
        *stored = Some(gpu.clone());
    }
    Ok(gpu)
}
