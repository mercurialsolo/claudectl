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
// TUI peripherals (recording + demo fixtures) now live in `claudectl-tui`.
// Re-exported under their original names so existing `crate::recorder::*` /
// `crate::demo::*` / `crate::session_recorder::*` paths in main.rs and app.rs
// keep resolving without rewriting each call site.
pub use claudectl_tui::{demo, recorder, session_recorder};
pub mod config;

pub mod app;
pub mod brain;
#[cfg(feature = "bus")]
pub mod bus;
#[cfg(feature = "coord")]
pub mod coord;
pub mod demo_peers;
#[cfg(feature = "hive")]
pub mod hive;
pub mod init;
pub mod orchestrator;
#[cfg(feature = "relay")]
pub mod relay;
pub mod runtime;
pub mod ui;
