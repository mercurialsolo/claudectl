//! Read-only project query surface (#429, #430, open-cluster RFC §4.3–§4.8).
//!
//! The whole path wired end to end — authorize → classify → retrieve → select
//! → respond.
//!
//! #429 shipped it with **deterministic term matching** where §4.3 puts Jev,
//! and that ordering was deliberate (spec §10). A deterministic surface that
//! works is the thing a classifier can be *measured* against, and it means Jev
//! arrives as an improvement to a working system rather than being
//! load-bearing on day one — which is what keeps §4.4's "Jev is the router,
//! code is the boundary" true in practice rather than only on paper.
//!
//! #430 added the classifier on top, **off unless `TYPESAFE_API_KEY` is set**.
//! With no key the surface behaves exactly as #429 shipped it; the
//! deterministic path is not a degraded mode, it is the default one.
//!
//! ## What the boundary actually is
//!
//! Three properties, each held by construction rather than by policy:
//!
//! 1. **Answers come only from a pre-built [`crate::context::ContextIndex`].**
//!    The index is assembled from `git ls-files --cached` with no directory
//!    walk anywhere, so source bodies, transcripts, `.env` files and untracked
//!    work have no code path into a response. `get_doc` looks a caller's
//!    string up *against indexed paths* and never opens a file.
//! 2. **Answers are verbatim spans with citations.** There is no generation
//!    step. Selection cannot invent a fact the index does not contain, and a
//!    confabulated claim about someone's architecture would be worse than no
//!    answer on a third-party-facing endpoint.
//! 3. **No verb mutates the project.** The only write anywhere in this module
//!    is `GrantStore::record_use` — the grant's own `use_count` and audit
//!    line, which is how an owner sees a grant being hammered.
//!
//! ## Layout
//!
//! - [`core`] — the single authorize → classify → retrieve → select path. All
//!   the policy that is not a threshold.
//! - [`rank`] — the deterministic ranker. Integers, total orders, no floats.
//! - [`jev`] — the Jev wire contract. Request, response, `curl`. No policy.
//! - [`thresholds`] — the six numbers §4.3 expects to be tuned, alone in a
//!   file so a tuning pass is one diff.
//! - [`classify`] — §4.3's routing table as a pure function, plus the two ways
//!   to have no classifier.
//! - [`escalate`] — the owner's queue for §4.3's "anything else" row.
//! - [`spend`] — the monthly Jev spend ceiling (#431's deferred §4.8 row).
//! - `http` — `POST /query`, `GET /topics`, `POST /doc`, bearer-authenticated.
//! - `mcp` — the same three operations as MCP tools over stdio.
//! - [`cli`] — `claudectl query serve` and `claudectl query stdio`.
//!
//! ## Feature gating
//!
//! The module is gated on `relay`, because `access` is: the grant MAC comes
//! from `relay::crypto`. `mcp` additionally needs `bus`, which is what carries
//! `rmcp`, Tokio and `schemars` — so the response types in [`core`] derive
//! `JsonSchema` only under `bus`, since that is the only configuration in
//! which the crate exists.

pub mod classify;
pub mod cli;
pub mod core;
pub mod escalate;
pub mod http;
pub mod jev;
#[cfg(feature = "bus")]
pub mod mcp;
pub mod rank;
pub mod spend;
pub mod thresholds;

// No re-exports: `core` is the surface and naming it at the call site is
// what makes "every policy decision lives in one file" visible from the
// import list of the two transports.
