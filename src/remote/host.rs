//! This machine as a farm host: what the coordinator's own agent runs on.

use super::HostInspection;
use anyhow::{Context, Result, ensure};
use std::{ffi::OsString, path::Path, time::Duration};

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
    let golden = output("msb", &["image", "list", "--quiet"])
        .await
        .is_some_and(|list| list.lines().any(|l| l.trim() == crate::farm::BASE_IMAGE));
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
pub async fn smoke(token: &str) -> Result<()> {
    let script = super::ssh::smoke_script(token)?;
    let mut command = tokio::process::Command::new("bash");
    command
        .args(["-c", &script])
        .env("PATH", search_path())
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true);
    let output = tokio::time::timeout(Duration::from_secs(180), command.output())
        .await
        .context("VM boot verification timed out")?
        .context("cannot run bash")?;
    ensure!(
        output.status.success(),
        "VM boot verification failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(())
}
