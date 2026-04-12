#![allow(unknown_lints)]
#![allow(
    clippy::collapsible_if,
    clippy::manual_is_multiple_of,
    clippy::io_other_error
)]

pub mod app;
pub mod config;
pub mod discovery;
pub mod history;
pub mod hooks;
pub mod logger;
pub mod monitor;
pub mod orchestrator;
pub mod process;
pub mod session;
pub mod terminals;
pub mod theme;
pub mod ui;
