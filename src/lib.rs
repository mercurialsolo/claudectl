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
// `app` and `ui` now live in `claudectl-tui` (issue #275). Re-exported so
// existing `crate::app::*` / `crate::ui::*` paths in main.rs and elsewhere
// resolve unchanged. The only ui module still in the binary is the
// brain-screen renderer (depends on binary-only `brain::metrics` +
// `brain::risk`), surfaced separately as `crate::brain_screen`.
pub use claudectl_tui::{app, demo, recorder, session_recorder, ui};
pub mod config;

pub mod brain;
pub mod brain_screen;
#[cfg(feature = "bus")]
pub mod bus;
#[cfg(feature = "coord")]
pub mod coord;
#[cfg(feature = "hive")]
pub mod hive;
pub mod init;
pub mod orchestrator;
#[cfg(feature = "relay")]
pub mod relay;
pub mod runtime;
