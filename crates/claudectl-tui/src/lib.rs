//! claudectl-tui — terminal UI, dashboard recording, and demo fixtures.
//!
//! Carved out from the binary crate as the second step of the workspace
//! refactor (epic #279, issue #275). Starts with the standalone helpers
//! (`recorder`, `session_recorder`, `demo`) that have no dependency on
//! `App` or the binary's brain/coord/bus subsystems. Follow-up PRs will
//! migrate `app.rs` and the `ui/*` render modules.
//!
//! Depends on `claudectl-core` for session / transcript types. Does not
//! depend on the binary crate.

#![allow(unknown_lints)]
#![allow(
    clippy::collapsible_if,
    clippy::manual_is_multiple_of,
    clippy::io_other_error
)]

pub mod demo;
pub mod recorder;
pub mod session_recorder;
