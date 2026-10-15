//! Agent families — what claudectl knows about coding agents other than
//! Claude Code.
//!
//! The hive's substrate is agent-neutral: gossip, merge, trust and
//! membership move `KnowledgeUnit`s and know nothing about sessions. Its two
//! *ends* are not — capture runs through a Claude Code hook and application
//! lands in the brain's own prompt. This module holds the neutral pieces
//! that let a foreign agent's experience reach the same substrate.
//!
//! See `docs/cross-agent-hive.md` (epic #521) for the design, and §9 of that
//! document for the measurements the tables here are built from.

pub mod vocab;
