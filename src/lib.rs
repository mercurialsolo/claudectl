#![allow(unknown_lints)]
#![allow(
    clippy::collapsible_if,
    clippy::manual_is_multiple_of,
    clippy::io_other_error
)]

// ---- Foundational modules now living in claudectl-core (epic #279, PR for #273)
//
// Re-exported under their original names so existing `crate::session::*`
// (etc.) paths keep resolving without rewriting 50+ import sites. Once #275
// extracts the TUI into its own crate it will depend on claudectl-core
// directly and these aliases can disappear.
pub use claudectl_core::{
    discovery, helpers, history, logger, models, monitor, process, session, terminals, theme,
    transcript,
};
pub mod config;

pub mod app;
pub mod brain;
#[cfg(feature = "bus")]
pub mod bus;
#[cfg(feature = "coord")]
pub mod coord;
pub mod demo;
pub mod health;
#[cfg(feature = "hive")]
pub mod hive;
pub mod hooks;
pub mod init;
pub mod launch;
pub mod orchestrator;
pub mod recorder;
#[cfg(feature = "relay")]
pub mod relay;
pub mod rules;
pub mod session_recorder;
pub mod skills;
pub mod ui;
