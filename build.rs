//! Bakes an identity of the agent's own sources into every binary, so a stale
//! stackbox-agent artifact can be told apart from the coordinator that would use it.
//!
//! The identity covers what the protocol depends on: the Rust sources, every file
//! under `src/` and `guest/` that `include_str!`/`include_bytes!` embeds (kernel
//! patches, shell scripts, Python helpers), and the image recipes. It does not
//! cover the git revision: documentation or portal commits must not force every
//! host to reinstall its agent.
use std::{fs, path::Path};

/// Files the agent build embeds or depends on, relative to the package root.
const ROOT_INPUTS: [&str; 6] = [
    "build.rs",
    "Cargo.toml",
    "Cargo.lock",
    "docker/agent.Dockerfile",
    "docker/agent-cross.Dockerfile",
    "src",
];
/// Extensions of files under `src/` that are embedded into the binary.
const EMBEDDED_EXTENSIONS: [&str; 4] = ["rs", "py", "diff", "sh"];

fn main() {
    println!("cargo:rerun-if-env-changed=STACKBOX_BUILD_ID");
    for path in ROOT_INPUTS.iter().chain(["guest"].iter()) {
        println!("cargo:rerun-if-changed={path}");
    }
    let id = std::env::var("STACKBOX_BUILD_ID")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(source_identity);
    println!("cargo:rustc-env=STACKBOX_BUILD_ID={id}");
}

/// FNV-1a over every input file, so the id changes only when they do.
fn source_identity() -> String {
    let mut files = Vec::new();
    collect(Path::new("src"), false, &mut files);
    collect(Path::new("guest"), true, &mut files);
    files.extend(
        [
            "Cargo.toml",
            "Cargo.lock",
            "docker/agent.Dockerfile",
            "docker/agent-cross.Dockerfile",
        ]
        .map(String::from),
    );
    files.push("build.rs".into());
    files.sort();
    files.dedup();
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for file in files {
        let bytes = fs::read(&file)
            .unwrap_or_else(|err| panic!("build id: cannot read input {file}: {err}"));
        for chunk in file.as_bytes().iter().chain(bytes.iter()) {
            hash ^= u64::from(*chunk);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    format!("{hash:016x}")
}

/// Adds the files under `dir`: every file when `all` is set, otherwise only the
/// embedded extensions. A missing directory is an error, not an empty input.
fn collect(dir: &Path, all: bool, files: &mut Vec<String>) {
    let entries = fs::read_dir(dir)
        .unwrap_or_else(|err| panic!("build id: cannot read directory {}: {err}", dir.display()));
    for entry in entries {
        let path = entry
            .unwrap_or_else(|err| panic!("build id: cannot list {}: {err}", dir.display()))
            .path();
        if path.is_dir() {
            collect(&path, all, files);
        } else if all
            || path
                .extension()
                .and_then(|ext| ext.to_str())
                .is_some_and(|ext| EMBEDDED_EXTENSIONS.contains(&ext))
        {
            files.push(path.display().to_string());
        }
    }
}
