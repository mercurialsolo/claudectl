#![allow(unknown_lints)]
#![allow(
    clippy::collapsible_if,
    clippy::manual_is_multiple_of,
    clippy::io_other_error
)]

pub mod app;
pub mod brain;
pub mod config;
#[cfg(feature = "coord")]
pub mod coord;
pub mod demo;
pub mod discovery;
pub mod health;
pub mod helpers;
pub mod history;
pub mod hooks;
pub mod init;
pub mod launch;
pub mod logger;
pub mod models;
pub mod monitor;
pub mod orchestrator;
pub mod process;
pub mod recorder;
pub mod rules;
pub mod session;
pub mod session_recorder;
pub mod terminals;
pub mod theme;
pub mod transcript;
pub mod ui;
