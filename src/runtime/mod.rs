//! Binary-side implementations of the `claudectl-core` runtime traits.
//!
//! Each submodule provides a `Live*` adapter that reads from the binary
//! crate's actual subsystem (brain, coord, bus, discovery) and projects it
//! into the core-owned DTOs the TUI will consume.
//!
//! The runtime is **read-only**. Side-effecting paths (terminate, inject,
//! log_decision) still live as direct calls in the binary until #275 maps
//! them onto a follow-up `Actions` trait.
//!
//! Tracking: workspace-refactor epic #279, issue #274.

use std::sync::Arc;

use claudectl_core::runtime::Runtime;

mod actions;
mod brain;
mod brain_driver;
mod brain_review;
mod bus;
mod coord;
mod orchestrator;
mod sessions;

pub use actions::LiveActions;
pub use brain::LiveBrainView;
// LiveBrainDriver is the only stateful runtime adapter; the TUI's `App` is the
// owner. Re-exported for tests and the future `App::new` wiring (next PR).
#[allow(unused_imports)]
pub use brain_driver::LiveBrainDriver;
pub use brain_review::LiveBrainReviewView;
pub use bus::LiveBusView;
pub use coord::LiveCoordView;
pub use orchestrator::LiveOrchestrator;
pub use sessions::LiveSessionSource;

/// Assemble the production runtime: each view backed by the corresponding
/// binary-crate subsystem. Cheap — every view is a unit struct that holds no
/// state, all the work happens in the trait method calls.
///
/// Unused until #275 wires the TUI through `Runtime`. Kept on the public
/// surface so reviewers can verify it compiles end-to-end today.
#[allow(dead_code)]
pub fn build_runtime() -> Runtime {
    Runtime::new(
        Arc::new(LiveSessionSource),
        Arc::new(LiveBrainView),
        Arc::new(LiveCoordView),
        Arc::new(LiveBusView),
        Arc::new(LiveActions),
        Arc::new(LiveBrainReviewView),
        Arc::new(LiveOrchestrator),
    )
}
