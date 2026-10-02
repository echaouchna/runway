//! Runway: deploy applications to Google Cloud Run from a single `runway.yaml`.

/// Documentation site (used in hints and generated files).
pub const DOCS_URL: &str = "https://OWNER.github.io/runway/docs";

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
