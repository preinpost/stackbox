//! Bakes the source revision into every binary so a stale farm-agent artifact can be
//! told apart from the coordinator that uploaded it.
use std::process::Command;

fn main() {
    // Recompute whenever the sources or the checked-out revision change: a plain
    // `.git/HEAD` watch misses commits, which only touch the branch ref.
    for path in [
        "build.rs",
        "Cargo.toml",
        "Cargo.lock",
        "src",
        ".git/HEAD",
        ".git/refs",
    ] {
        println!("cargo:rerun-if-changed={path}");
    }
    println!("cargo:rerun-if-env-changed=FARM_BUILD_ID");
    let id = std::env::var("FARM_BUILD_ID")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(from_git);
    println!("cargo:rustc-env=FARM_BUILD_ID={id}");
}

/// Docker builds copy only the working tree, so the caller can pass the revision in.
fn from_git() -> String {
    let git = |args: &[&str]| {
        Command::new("git")
            .args(args)
            .output()
            .ok()
            .filter(|out| out.status.success())
            .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_owned())
    };
    let Some(head) = git(&["rev-parse", "--short=12", "HEAD"]) else {
        return "unknown".into();
    };
    match git(&["status", "--porcelain"]) {
        Some(status) if !status.is_empty() => format!("{head}-dirty"),
        Some(_) => head,
        None => head,
    }
}
