//! Bakes an identity of the agent's own sources into every binary, so a stale
//! farm-agent artifact can be told apart from the coordinator that would use it.
//!
//! The identity covers what the protocol depends on (Rust sources and the guest files
//! and image recipes under `guest/`), not the git revision: documentation or portal
//! commits must not force every host to reinstall its agent.
use std::{fs, path::Path};

fn main() {
    println!("cargo:rerun-if-env-changed=FARM_BUILD_ID");
    for path in [
        "build.rs",
        "Cargo.toml",
        "Cargo.lock",
        "src",
        "guest",
        "docker/agent.Dockerfile",
    ] {
        println!("cargo:rerun-if-changed={path}");
    }
    let id = std::env::var("FARM_BUILD_ID")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(source_identity);
    println!("cargo:rustc-env=FARM_BUILD_ID={id}");
}

/// FNV-1a over every source file, so the id changes only when they do.
fn source_identity() -> String {
    let mut files = Vec::new();
    collect(Path::new("src"), &mut files);
    collect(Path::new("guest"), &mut files);
    files.push("Cargo.toml".into());
    files.push("Cargo.lock".into());
    files.push("docker/agent.Dockerfile".into());
    files.sort();
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for file in files {
        let bytes = fs::read(&file).unwrap_or_default();
        for chunk in file.as_bytes().iter().chain(bytes.iter()) {
            hash ^= u64::from(*chunk);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    format!("{hash:016x}")
}

fn collect(dir: &Path, files: &mut Vec<String>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect(&path, files);
        } else if path.starts_with("guest")
            || path
                .extension()
                .is_some_and(|ext| ext == "rs" || ext == "py")
        {
            files.push(path.display().to_string());
        }
    }
}
