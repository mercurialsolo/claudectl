//! claudectl-core — foundational types and IO primitives.
//!
//! Carved out from the binary crate as the first step of the workspace
//! refactor (epic #279). The binary, TUI, brain, bus, and every future crate
//! depend on this; this crate depends on nothing claudectl-specific in
//! return. Dependency direction is enforced by CI (#277) once the rest of
//! the epic lands.

#![allow(unknown_lints)]
#![allow(
    clippy::collapsible_if,
    clippy::manual_is_multiple_of,
    clippy::io_other_error
)]

pub mod config;
pub mod discovery;
pub mod health;
pub mod helpers;
pub mod history;
pub mod hooks;
pub mod launch;
pub mod logger;
pub mod models;
pub mod monitor;
pub mod process;
pub mod rules;
pub mod runtime;
pub mod session;
pub mod skills;
pub mod terminals;
pub mod theme;
pub mod transcript;
