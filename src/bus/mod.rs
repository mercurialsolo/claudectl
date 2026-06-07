//! Agent bus — durable directory + mailbox for running Claude Code instances.
//!
//! See `docs/AGENT_BUS.md` for the design spec. This module implements phases
//! 1–4 of that spec's build order:
//!
//! * §3 `whoami` and `list_agents` discovery tools.
//! * §3 directed `publish` / `read_inbox` messaging.
//! * §4 SQLite-backed persistent mailbox at `~/.claudectl/bus/bus.db`.
//! * §5 cwd-inferred role resolution, with explicit `--role` fallback.
//!
//! Pub/sub claim protocol (§3), `Stop`-hook continue-in-turn delivery (§6),
//! flow guards (§10), and the long-horizon supervisor (§13) are not yet
//! implemented — see the build order in `docs/AGENT_BUS.md` §12.

pub mod cli;
pub mod mcp;
pub mod policy;
pub mod roles;
pub mod stop_hook;
pub mod store;
pub mod suggest;

#[allow(unused_imports)]
pub use roles::{Role, RoleResolution};
