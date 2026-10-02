//! Runway: deploy applications to Google Cloud Run from a single `runway.yaml`.

/// Documentation site (used in hints and generated files).
pub const DOCS_URL: &str = "https://echaouchna.github.io/runway/docs";

/// Version shown by `--version`. Edge builds set `RUNWAY_VERSION` at build
/// time (for example `0.1.0-edge.42 (1a2b3c4)`) so reports name the commit.
pub const VERSION: &str = match option_env!("RUNWAY_VERSION") {
    Some(v) => v,
    None => env!("CARGO_PKG_VERSION"),
};

mod branding;
pub mod build;
pub mod cli;
pub mod commands;
pub mod config;
pub mod deploy;
pub mod describe;
pub mod error;
pub mod gcp;
pub mod image_ref;
pub mod naming;
pub mod output;
pub mod plan;
pub mod poll;
pub mod provision;
pub mod retry;
pub mod style;
pub mod traffic;
