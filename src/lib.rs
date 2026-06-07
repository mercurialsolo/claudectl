#![allow(unknown_lints)]
#![allow(
    clippy::collapsible_if,
    clippy::manual_is_multiple_of,
    clippy::io_other_error
)]

// ---- Foundational modules now living in claudectl-core (epic #279, PRs for
// #273 + #276 + the hooks/launch/skills move below).
//
// Re-exported under their original names so existing `crate::session::*`
// (etc.) paths keep resolving without rewriting 50+ import sites. Once #275
// extracts the TUI into its own crate it will depend on claudectl-core
// directly and these aliases can disappear.
pub use claudectl_core::{
    discovery, health, helpers, history, hooks, launch, logger, models, monitor, process, rules,
    session, skills, terminals, theme, transcript,
};
pub mod config;

pub mod app;
pub mod brain;
#[cfg(feature = "bus")]
pub mod bus;
#[cfg(feature = "coord")]
pub mod coord;
pub mod demo;
#[cfg(feature = "hive")]
pub mod hive;
pub mod init;
pub mod orchestrator;
pub mod recorder;
#[cfg(feature = "relay")]
pub mod relay;
pub mod runtime;
pub mod session_recorder;
pub mod ui;
