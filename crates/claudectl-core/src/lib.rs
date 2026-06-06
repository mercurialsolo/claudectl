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

pub mod discovery;
pub mod helpers;
pub mod history;
pub mod logger;
pub mod models;
pub mod monitor;
pub mod process;
pub mod runtime;
pub mod session;
pub mod terminals;
pub mod theme;
pub mod transcript;
