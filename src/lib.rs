#![recursion_limit = "256"]

pub mod api;
pub mod gpu;
pub mod kernel;
pub mod kolla;
pub mod link;
pub mod manager;
pub mod model;
pub mod remote;
pub mod s3;
mod shell;
pub mod stackbox;

/// Revision this binary was built from, baked in by `build.rs`.
pub const BUILD_ID: &str = env!("STACKBOX_BUILD_ID");
