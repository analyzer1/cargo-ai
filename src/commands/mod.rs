//! Command execution modules for the `cargo-ai` CLI.
//!
//! Each submodule owns one command area and keeps `main.rs` dispatch-only.
pub mod account;
pub mod add;
pub mod agents;
pub mod auth;
#[cfg(feature = "developer-tools")]
pub mod build;
pub mod credentials;
pub mod definition_source;
#[cfg(feature = "developer-tools")]
pub mod hatch;
pub mod hatch_pipeline;
pub mod init;
pub mod local_packages;
pub mod mail;
pub mod new;
#[cfg(feature = "developer-tools")]
pub mod package;
pub(crate) mod package_dependencies;
pub(crate) mod package_inspection;
pub(crate) mod package_lock;
pub(crate) mod package_metadata;
#[cfg(feature = "developer-tools")]
pub(crate) mod package_publication;
pub mod packages;
pub mod profile;
pub mod run;
pub mod runtime;
pub mod runtime_actions;
#[path = "../../templates/src/runtime_data.rs"]
pub(crate) mod runtime_data;
pub mod scaffold;
pub mod tools;
pub mod version;
