//! The one authorize → retrieve → select → respond path, shared by both
//! surfaces (#429, open-cluster RFC §4.5, §4.7).
//!
//! Everything a third party can reach goes through this type. `http.rs` and
//! `mcp.rs` are transport adapters over it and contain no policy of their own,
//! which is what makes "both run through the same path" (§4.7) a structural
//! fact rather than a convention two files have to keep agreeing on.
//!
//! ## One server, one project
//!
//! The RFC routes on `/api/v1/project/<project>/…`, which reads as though a
//! name can be resolved to a directory. Nothing in claudectl can do that. A
//! session's `project_name` is its cwd basename
//! (`claudectl_core::session`), so the mapping is many-to-one — every worktree
//! of a repo collapses to a different name, two unrelated repos sharing a
//! basename collapse to the same one — and `~/.claude/projects/<slug>` is a
//! lossy `/`→`-` substitution that cannot be inverted.
//!
//! So this process serves exactly one project: the repository it was started
//! in. The name is the operator's, supplied or defaulted from the repo root's
//! basename, and it is the *only* name this process will ever answer to. The
//! `<project>` segment in a request is checked for equality against it and
//! never resolved, never joined to a path, never passed to `verify`. A request
//! naming anything else is a `404` indistinguishable from an unmatched route.
//! Serving a second project means a second process on a second port.
//!
//! ## Why the scope comes from the server, not the request
//!
//! `authorize` builds the required `Scope` from `self.project`. If it built it
//! from the request's `<project>` segment instead, a token minted for
//! `project.query:other` presented against `/project/other/query` would
//! satisfy `verify` on a server serving `claudectl`, and correctness would
//! then rest on remembering to compare the segment separately. Deriving the
//! scope from the served project makes that class of mistake unrepresentable.
//!
//! ## Guardrail order
//!
//! `authorize` runs `verify` → rate limit → daily budget, and each placement
//! is load-bearing rather than incidental — the bucket map's key space, what a
//! throttled request is allowed to spend, and which denials charge at all all
//! depend on it. The reasoning is on `authorize` itself (#431, RFC §4.8).

use std::cmp::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use serde::{Deserialize, Serialize};

use crate::access::grant::ChargeError;
use crate::access::{self, DenyReason, GrantStore, Scope};
use crate::context::{ContextIndex, docs::DocSection};
use crate::rate_limit::RateLimiter;

use super::rank;

/// Window the per-grant rate limit is measured over.
///
/// A grant's `rate_limit_per_min` is the capacity; this is the "per minute".
pub const RATE_LIMIT_WINDOW_SECS: u32 = 60;

/// Largest question this surface will consider.
///
/// Matches `bus::policy::DEFAULT_MAX_BODY_BYTES`, which set the house value
/// for "attacker-supplied text we are willing to hold." Not reused directly:
/// `bus::policy` lives behind the `bus` feature and the HTTP surface is
/// `relay`-only, and moving that module is not this change's job.
pub const MAX_QUESTION_BYTES: usize = 8192;

/// Spans returned when the caller names no limit.
pub const DEFAULT_SPAN_LIMIT: usize = 5;

/// Ceiling on `limit`, whatever the caller asks for.
pub const MAX_SPAN_LIMIT: usize = 20;

/// Soft cap on the total span text in one response (RFC §4.8).
///
/// Reached mid-assembly, the response keeps the spans already selected and
/// reports `truncated` — a short honest answer beats a 2 MB one.
pub const MAX_RESPONSE_BYTES: usize = 32 * 1024;

/// Which part of the index a span came from, so a citation can be read
/// without guessing from the path shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "bus", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum SpanSource {
    /// A section of `CLAUDE.md`.
    ClaudeMd,
    /// A section of `README.md`.
    Readme,
    /// A section of a file under `docs/`.
    Docs,
    /// A module's `//!` header.
    ModuleDoc,
    /// One signature plus its doc comment from the module map.
    ModuleItem,
    /// A published skill's description.
    Skill,
    /// An exposed hive knowledge unit's summary.
    HiveUnit,
}

/// One verbatim span with its citation.
///
/// `text` is copied out of the index unchanged. Nothing on this surface
/// generates prose: a confabulated claim about someone's architecture is worse
/// than no answer (RFC §4.5), and selection cannot invent a fact the index
/// does not already contain.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "bus", derive(schemars::JsonSchema))]
pub struct Span {
    pub source: SpanSource,
    /// Work-tree-relative path, or empty for skills and hive units, which the
    /// index carries by name rather than by file.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub path: String,
    /// Enclosing headings, outermost first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub heading_path: Vec<String>,
    pub text: String,
    pub score: u32,
}

/// The answer to one question.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "bus", derive(schemars::JsonSchema))]
pub struct Answer {
    pub project: String,
    /// The index's content fingerprint, so a caller can tell a changed answer
    /// from a changed project.
    pub fingerprint: String,
    /// The terms the ranking actually matched on, in the question's own order.
    ///
    /// Published because a deterministic ranking that cannot be inspected is
    /// only repeatable, not auditable — this is how a caller sees why a span
    /// was chosen, and how an owner reproduces a ranking by hand.
    pub matched_terms: Vec<String>,
    pub spans: Vec<Span>,
    /// Whether the byte cap stopped span assembly early.
    pub truncated: bool,
}

/// One entry in the table of contents.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "bus", derive(schemars::JsonSchema))]
pub struct Topic {
    pub source: SpanSource,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub path: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub heading_path: Vec<String>,
}

/// What is answerable, with no bodies.
///
/// A caller needs this to ask a useful question at all — asking blind is how
/// you get a scored zero and no idea why. It is also the cheapest honest
/// statement of the boundary: everything listed here is publishable, and
/// nothing outside it exists as far as this surface is concerned.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "bus", derive(schemars::JsonSchema))]
pub struct Topics {
    pub project: String,
    pub fingerprint: String,
    pub topics: Vec<Topic>,
    /// Categories the owner's exposure policy is withholding, so an empty
    /// index reads as a decision rather than as a broken install.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub categories_hidden: Vec<String>,
}

/// Every indexed section of one documentation file, verbatim.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "bus", derive(schemars::JsonSchema))]
pub struct Document {
    pub project: String,
    pub fingerprint: String,
    pub path: String,
    pub sections: Vec<Span>,
}

/// What went wrong, in the shapes a transport can map to a status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QueryError {
    /// The caller may not have this, or it does not exist, and they are not
    /// told which. RFC §3.3: `404`, never `403`.
    Denied,
    /// The request is malformed in a way that says nothing about what exists.
    BadRequest(String),
    /// The server failed. Never carries anything index-derived.
    Internal(String),
    /// A guardrail refused an otherwise-valid request (#431, RFC §4.8).
    ///
    /// Distinct from `Denied`, and told apart in the response on purpose. The
    /// caller here has already proved they hold a valid, in-scope token — so
    /// telling them they are over their own rate limit leaks nothing §3.3
    /// protects, and leaving them to guess why a working grant went quiet
    /// would be hostile.
    Throttled {
        message: &'static str,
        /// Seconds until the request would succeed, for `Retry-After`.
        retry_after_secs: Option<u64>,
    },
}

impl std::fmt::Display for QueryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            QueryError::Denied => write!(f, "not found"),
            QueryError::BadRequest(m) => write!(f, "{m}"),
            QueryError::Internal(m) => write!(f, "{m}"),
            QueryError::Throttled { message, .. } => write!(f, "{message}"),
        }
    }
}

impl std::error::Error for QueryError {}

/// Which capability an operation needs.
///
/// Split along the line §3.3 already draws: `project.query` is "ask questions
/// about the project", `project.docs` is "retrieve published doc spans
/// verbatim". `topics` sits under `project.query` because it is the discovery
/// half of asking — `--scopes` defaults to `project.query` alone, and a grant
/// that can ask but cannot see what is answerable is a grant that cannot ask
/// anything useful.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Operation {
    Ask,
    Topics,
    GetDoc,
}

impl Operation {
    fn audit_detail(&self) -> &'static str {
        match self {
            Operation::Ask => "query.ask",
            Operation::Topics => "query.topics",
            Operation::GetDoc => "query.get_doc",
        }
    }
}

/// The read-only query core for one project.
pub struct QueryCore {
    project: String,
    index: Arc<ContextIndex>,
    store: GrantStore,
    secret: [u8; 32],
    /// Per-grant token buckets (#431, RFC §4.8). Keyed on *verified* grant
    /// ids — see the eviction note in `crate::rate_limit`.
    limiter: RateLimiter,
    /// Serialises **every** read-modify-write this process makes on a grant
    /// file — `charge_daily_budget` and `record_query_use` both.
    ///
    /// Two concurrent requests could otherwise read the same `budget_used` and
    /// both write `+1`, losing a charge. Holding the lock for only one of the
    /// two writers does not close that: the unsynchronised one reads the
    /// record, the locked one charges, and the first then writes its stale
    /// copy back. #431 shipped with exactly that gap.
    ///
    /// **What this lock cannot do is order writes against another process.**
    /// `access revoke` runs in the CLI, not here, so a stale write from this
    /// process could clobber a revocation. That is why `revoked` is backed by
    /// a create-only tombstone rather than by this mutex — see
    /// `GrantStore::is_tombstoned`. What remains cross-process-racy is the
    /// benign direction only: a lost budget charge or use-count bump between
    /// two `query serve` processes sharing one access dir.
    budget_lock: Mutex<()>,
}

impl QueryCore {
    /// Assemble a core over an already-built index.
    ///
    /// The store and secret are injected rather than opened per request: a
    /// handler calling `GrantStore::open_default()` would make every test
    /// write to the operator's real `~/.claudectl/access`, and would reload
    /// the HMAC key on a path a third party can drive.
    pub fn new(
        project: String,
        index: Arc<ContextIndex>,
        store: GrantStore,
        secret: [u8; 32],
    ) -> Self {
        QueryCore {
            project,
            index,
            store,
            secret,
            limiter: RateLimiter::new(
                // Unused by this caller: every acquisition passes the grant's
                // own `rate_limit_per_min`. Only the window matters here.
                crate::rate_limit::DEFAULT_CAPACITY,
                RATE_LIMIT_WINDOW_SECS,
            ),
            budget_lock: Mutex::new(()),
        }
    }

    pub fn project(&self) -> &str {
        &self.project
    }

    pub fn index(&self) -> &ContextIndex {
        &self.index
    }

    /// Whether `requested` names the project this process serves.
    ///
    /// Compared, never resolved. The caller maps `false` to the same `404` an
    /// unmatched route gets, so a probe cannot tell "wrong project" from "no
    /// such route" from "no such project".
    pub fn serves(&self, requested: &str) -> bool {
        requested == self.project
    }

    /// Verify `token` for `op` against the *served* project, then spend one
    /// unit of the grant's rate limit and daily budget.
    ///
    /// Order is `verify` → rate limit → daily budget, and each step is placed
    /// where it is for a reason:
    ///
    /// - **Rate limit after `verify`.** Checking it first would mean a bucket
    ///   per *claimed* grant id, and the map has no eviction — someone walking
    ///   24-bit ids could pin 16M buckets. After verification the key is
    ///   always an id that has presented a valid MAC, so the map is bounded by
    ///   real grants. The cost is one HMAC per throttled request, which is
    ///   microseconds.
    /// - **Rate-limited requests do not reach the budget.** They were never
    ///   evaluated, so charging the day's allowance for them would let a burst
    ///   consume a budget it was refused the use of.
    /// - **A `MissingScope` denial charges the budget; no other denial does.**
    ///   §4.8 says "denied queries count, so probing is self-limiting", and
    ///   `MissingScope` is the one denial that proves the caller holds a valid
    ///   token and is probing other verbs or projects. Charging `BadMac` or
    ///   `UnknownGrant` would let anyone who guesses a grant id drain the real
    ///   holder's budget — turning a defence against probing into a
    ///   denial-of-service against the person it protects.
    ///
    /// Returns the grant on success. The caller records the use *after* the
    /// work succeeds, so the audit line can carry what came back.
    fn authorize(
        &self,
        token: &str,
        op: Operation,
        question: Option<&str>,
        now_ms: u64,
    ) -> Result<access::Grant, QueryError> {
        let want = match op {
            Operation::Ask | Operation::Topics => Scope::ProjectQuery(self.project.clone()),
            Operation::GetDoc => Scope::ProjectDocs(self.project.clone()),
        };

        let grant = match self
            .store
            .verify_detailed(&self.secret, token, Some(&want), now_ms)
        {
            Ok(g) => g,
            Err((DenyReason::MissingScope, Some(grant_id))) => {
                // Throttled on the same bucket as a successful request, and
                // for the same reason the limit sits after `verify`: this id
                // has presented a valid MAC, so it is already eligible to key
                // a bucket. Leaving it out meant a holder with a wrong-scope
                // token was *unthrottled* — each request costing an HMAC, a
                // grant read, a grant write and two `audit.jsonl` appends at
                // wire speed — which contradicted the per-grant ceiling this
                // surface documents.
                //
                // What the throttle actually bounds is the grant-file writes
                // and the budget charges. `verify_detailed` has already
                // appended its `missing_scope` line by the time this runs, so
                // audit appends stay one per request: the same disk-write
                // vector an unauthenticated flood has, and the TLS
                // terminator's job either way.
                //
                // No second `rate_limited` line here. The `missing_scope` line
                // is already the record of the attempt, and halving the
                // appends matters more than narrating which guard stopped it.
                if !self.acquire(&grant_id, now_ms) {
                    return Err(QueryError::Denied);
                }
                // The result is discarded on purpose: the response is `Denied`
                // whether the charge landed, was already exhausted, or failed
                // against the store — and each of those wrote its own audit
                // line on the way through.
                let _ = self.charge_budget(&grant_id, question, now_ms);
                return Err(QueryError::Denied);
            }
            Err(_) => return Err(QueryError::Denied),
        };

        // One `Instant`, read once and used for both calls. Two separate
        // `Instant::now()`s let a token refill between them, and then
        // `retry_after_secs` returns `None` and the 429 ships without the
        // `Retry-After` header `docs/access.md` promises.
        let now = Instant::now();
        if !self
            .limiter
            .try_acquire_with_capacity(&grant.grant_id, grant.rate_limit_per_min, now)
        {
            let retry_after = self.limiter.retry_after_secs(&grant.grant_id, now);
            self.store.audit_denied_outside_verify(
                &grant.grant_id,
                DenyReason::RateLimited,
                Some(format!("{}/min", grant.rate_limit_per_min)),
                question,
                now_ms,
            );
            return Err(QueryError::Throttled {
                message: "rate limited",
                retry_after_secs: retry_after,
            });
        }

        self.charge_budget(&grant.grant_id, question, now_ms)?;
        Ok(grant)
    }

    /// Take one token from `grant_id`'s bucket, at that grant's own capacity.
    ///
    /// The capacity lives on the grant, so this loads it. That is not a new
    /// class of I/O on either caller's path: the success path has the grant in
    /// hand and `charge_budget` loads it again anyway, and the `MissingScope`
    /// path is a denial. A grant that cannot be read falls back to the
    /// limiter's own default rather than going unthrottled.
    fn acquire(&self, grant_id: &str, _now_ms: u64) -> bool {
        let capacity = self
            .store
            .load(grant_id)
            .ok()
            .flatten()
            .map(|g| g.rate_limit_per_min)
            .unwrap_or(crate::rate_limit::DEFAULT_CAPACITY);
        self.limiter
            .try_acquire_with_capacity(grant_id, capacity, Instant::now())
    }

    /// Record a successful use, under the grant-file lock.
    ///
    /// `record_query_use` is load → bump `use_count` → `update`, so it is a
    /// second read-modify-write on the grant file and needs the same lock
    /// `charge_daily_budget` takes. Without it, two concurrent requests could
    /// both read the same record and the second write would drop the first's
    /// charge — the lost-write window the mutex was supposed to close.
    fn record_use(
        &self,
        grant_id: &str,
        detail: Option<&str>,
        question: Option<&str>,
        cited: Option<Vec<String>>,
        now_ms: u64,
    ) -> Result<(), QueryError> {
        let _guard = self
            .budget_lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        self.store
            .record_query_use(grant_id, detail, question, cited, now_ms)
            .map_err(QueryError::Internal)
    }

    /// Spend one unit of the grant's daily budget, under the process lock.
    fn charge_budget(
        &self,
        grant_id: &str,
        question: Option<&str>,
        now_ms: u64,
    ) -> Result<(), QueryError> {
        let _guard = self
            .budget_lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match self.store.charge_daily_budget(grant_id, question, now_ms) {
            Ok(_) => Ok(()),
            // `charge_daily_budget` already wrote the audit line, question
            // included, so this only shapes the refusal.
            Err(ChargeError::Exhausted) => Err(QueryError::Throttled {
                message: "daily budget exhausted",
                retry_after_secs: Some(seconds_until_utc_midnight(now_ms)),
            }),
            // Fail closed. A store that cannot be read or written means the
            // surface cannot meter, and a `500` here would hand out
            // *unmetered* requests to anyone who can induce a transient
            // failure — so this is the same opaque denial as everything else.
            // The real reason is already in the audit log.
            Err(ChargeError::Store(_)) => Err(QueryError::Denied),
        }
    }

    /// Answer a question with ranked verbatim spans.
    pub fn ask(
        &self,
        token: &str,
        question: &str,
        limit: Option<usize>,
    ) -> Result<Answer, QueryError> {
        self.ask_at(token, question, limit, access::epoch_ms())
    }

    /// [`Self::ask`] with the clock injected.
    ///
    /// The daily budget rolls over on a UTC day boundary, so testing it means
    /// controlling the clock. Same convention `GrantStore::verify` already
    /// uses: the `_at` form is the real implementation and the bare form
    /// supplies `epoch_ms()`.
    pub fn ask_at(
        &self,
        token: &str,
        question: &str,
        limit: Option<usize>,
        now_ms: u64,
    ) -> Result<Answer, QueryError> {
        if question.trim().is_empty() {
            return Err(QueryError::BadRequest("question is empty".into()));
        }
        if question.len() > MAX_QUESTION_BYTES {
            return Err(QueryError::BadRequest(format!(
                "question exceeds {MAX_QUESTION_BYTES} bytes"
            )));
        }
        let grant = self.authorize(token, Operation::Ask, Some(question), now_ms)?;

        let terms = rank::terms(question);
        let limit = limit.unwrap_or(DEFAULT_SPAN_LIMIT).clamp(1, MAX_SPAN_LIMIT);
        let (spans, truncated) = self.select(&terms, limit);

        // Record after the work, so the line says what actually came back.
        self.record_use(
            &grant.grant_id,
            Some(Operation::Ask.audit_detail()),
            Some(question),
            Some(cited_paths(&spans)),
            now_ms,
        )?;

        Ok(Answer {
            project: self.project.clone(),
            fingerprint: self.index.fingerprint(),
            matched_terms: terms,
            spans,
            truncated,
        })
    }

    /// List what is answerable, with no bodies.
    pub fn topics(&self, token: &str) -> Result<Topics, QueryError> {
        self.topics_at(token, access::epoch_ms())
    }

    /// [`Self::topics`] with the clock injected.
    pub fn topics_at(&self, token: &str, now_ms: u64) -> Result<Topics, QueryError> {
        let grant = self.authorize(token, Operation::Topics, None, now_ms)?;
        let mut topics = Vec::new();
        for (source, sections) in self.doc_channels() {
            for section in sections {
                topics.push(Topic {
                    source,
                    path: section.path.clone(),
                    heading_path: section.heading_path.clone(),
                });
            }
        }
        for module in &self.index.module_map {
            topics.push(Topic {
                source: SpanSource::ModuleItem,
                path: module.path.clone(),
                heading_path: Vec::new(),
            });
        }
        for skill in &self.index.skills {
            topics.push(Topic {
                source: SpanSource::Skill,
                path: String::new(),
                heading_path: vec![skill.name.clone()],
            });
        }
        for unit in &self.index.hive_units {
            topics.push(Topic {
                source: SpanSource::HiveUnit,
                path: String::new(),
                heading_path: vec![unit.category.clone(), unit.id.clone()],
            });
        }
        // Recorded after the list is built, matching `ask` and `get_doc`. The
        // build cannot fail today, but the module documents "record after the
        // work" as the invariant and holding it in two of three operations is
        // how a future fallible step logs a use that did not happen.
        self.record_use(
            &grant.grant_id,
            Some(Operation::Topics.audit_detail()),
            None,
            None,
            now_ms,
        )?;
        Ok(Topics {
            project: self.project.clone(),
            fingerprint: self.index.fingerprint(),
            topics,
            categories_hidden: self.index.stats.categories_hidden.clone(),
        })
    }

    /// Return every indexed section of one documentation file, verbatim.
    ///
    /// `path` is matched against the paths already *in the index*. It is never
    /// joined, canonicalized or opened — this is the one place a third party's
    /// string looks like a path, and treating it as a lookup key rather than a
    /// path is what keeps that resemblance harmless. An unmatched path is the
    /// same `404` as a path the owner is withholding.
    pub fn get_doc(&self, token: &str, path: &str) -> Result<Document, QueryError> {
        self.get_doc_at(token, path, access::epoch_ms())
    }

    /// [`Self::get_doc`] with the clock injected.
    pub fn get_doc_at(&self, token: &str, path: &str, now_ms: u64) -> Result<Document, QueryError> {
        if path.trim().is_empty() {
            return Err(QueryError::BadRequest("path is empty".into()));
        }
        // The requested path is the question here: it is what the caller asked
        // for, and the only thing worth auditing about a doc fetch.
        let grant = self.authorize(token, Operation::GetDoc, Some(path), now_ms)?;

        let mut sections = Vec::new();
        for (source, channel) in self.doc_channels() {
            for section in channel {
                if section.path == path {
                    sections.push(Span {
                        source,
                        path: section.path.clone(),
                        heading_path: section.heading_path.clone(),
                        text: section.body.clone(),
                        score: 0,
                    });
                }
            }
        }
        if sections.is_empty() {
            // Audited before returning. `authorize` already charged the
            // budget, so without this a holder enumerating doc paths drained
            // `budget_used` while `access audit` showed nothing — the two
            // commands disagreed and neither could be reconciled with the
            // other. The caller still gets the same opaque `404`; the line is
            // for the owner, and carries the path as `question` so it reads
            // the same way a hit does.
            self.store.audit_denied_outside_verify(
                &grant.grant_id,
                DenyReason::NotIndexed,
                Some(Operation::GetDoc.audit_detail().to_string()),
                Some(path),
                now_ms,
            );
            return Err(QueryError::Denied);
        }
        self.record_use(
            &grant.grant_id,
            Some(Operation::GetDoc.audit_detail()),
            Some(path),
            Some(vec![path.to_string()]),
            now_ms,
        )?;
        Ok(Document {
            project: self.project.clone(),
            fingerprint: self.index.fingerprint(),
            path: path.to_string(),
            sections,
        })
    }

    /// The three documentation channels, paired with the source they report.
    fn doc_channels(&self) -> [(SpanSource, &Vec<DocSection>); 3] {
        [
            (SpanSource::ClaudeMd, &self.index.claude_md),
            (SpanSource::Readme, &self.index.readme),
            (SpanSource::Docs, &self.index.docs),
        ]
    }

    /// Score every candidate span, keep the best `limit`, and stop adding once
    /// the byte cap is reached.
    ///
    /// Selection, not generation: the chosen spans are copied out of the index
    /// unchanged.
    fn select(&self, terms: &[String], limit: usize) -> (Vec<Span>, bool) {
        let mut scored: Vec<Span> = Vec::new();
        let mut consider = |source: SpanSource, path: &str, heading: Vec<String>, text: &str| {
            let score = rank::score(terms, &heading, text);
            if score > 0 {
                scored.push(Span {
                    source,
                    path: path.to_string(),
                    heading_path: heading,
                    text: text.to_string(),
                    score,
                });
            }
        };

        for (source, channel) in self.doc_channels() {
            for section in channel {
                consider(
                    source,
                    &section.path,
                    section.heading_path.clone(),
                    &section.body,
                );
            }
        }
        for module in &self.index.module_map {
            if !module.module_doc.is_empty() {
                consider(
                    SpanSource::ModuleDoc,
                    &module.path,
                    Vec::new(),
                    &module.module_doc,
                );
            }
            for item in &module.items {
                consider(
                    SpanSource::ModuleItem,
                    &module.path,
                    item_heading(item),
                    &item_text(item),
                );
            }
        }
        for skill in &self.index.skills {
            consider(
                SpanSource::Skill,
                "",
                vec![skill.name.clone()],
                &skill.description,
            );
        }
        for unit in &self.index.hive_units {
            consider(
                SpanSource::HiveUnit,
                "",
                vec![unit.category.clone(), unit.id.clone()],
                &unit.summary,
            );
        }

        // Total order, so the same index and question always produce the same
        // bytes.
        //
        // Source comes second, before length, and that ordering was earned the
        // hard way. §4.5 answers from *published documentation*; the module map
        // is the structural supplement. With length second, a question about
        // config layering tied `docs/configuration.md` against
        // `src/lib.rs › pub mod config` and gave the top slot to the one-line
        // signature — "shorter is more specific" is true between two prose
        // sections and false between prose and a bare signature. `SpanSource`
        // is declared in exactly this order of preference.
        scored.sort_by(|a, b| {
            b.score
                .cmp(&a.score)
                .then_with(|| cmp_source(a.source, b.source))
                .then_with(|| a.text.len().cmp(&b.text.len()))
                .then_with(|| a.path.cmp(&b.path))
                .then_with(|| a.heading_path.cmp(&b.heading_path))
                .then_with(|| a.text.cmp(&b.text))
        });

        let mut out = Vec::new();
        let mut bytes = 0usize;
        let mut truncated = false;
        for span in scored.into_iter().take(limit) {
            if bytes + span.text.len() > MAX_RESPONSE_BYTES && !out.is_empty() {
                truncated = true;
                break;
            }
            bytes += span.text.len();
            out.push(span);
        }
        (out, truncated)
    }
}

/// The distinct, non-empty paths a set of spans cited, for the audit line.
///
/// Deduplicated and sorted: several spans from one file are one citation as
/// far as "what left the machine" is concerned, and a stable order keeps the
/// log diffable. Skills and hive units carry no path and are skipped — the
/// `detail` field already says which operation ran.
fn cited_paths(spans: &[Span]) -> Vec<String> {
    let mut paths: Vec<String> = spans
        .iter()
        .filter(|s| !s.path.is_empty())
        .map(|s| s.path.clone())
        .collect();
    paths.sort();
    paths.dedup();
    paths
}

/// Seconds from `now_ms` to the next UTC midnight, when a daily budget resets.
fn seconds_until_utc_midnight(now_ms: u64) -> u64 {
    let into_day = now_ms % access::MS_PER_DAY;
    (access::MS_PER_DAY - into_day).div_ceil(1000)
}

/// Last-resort tiebreak, so the sort is a total order even for two spans
/// identical in every published field.
fn cmp_source(a: SpanSource, b: SpanSource) -> Ordering {
    (a as u8).cmp(&(b as u8))
}

/// The heading path for a module item: its enclosing `impl` block, if any,
/// then its own signature.
///
/// The signature belongs in the heading rather than only in the body, and
/// running this against the real repo is what settled it. A question about
/// config layering returned three spans all citing
/// `src/config.rs › impl Config` — three different functions, indistinguishable
/// in the citation, which is not a citation. A signature is also the item's own
/// one-line statement of what it is, which is the same argument that earns a
/// markdown heading its weight.
fn item_heading(item: &crate::context::module_map::ItemSummary) -> Vec<String> {
    let mut heading = Vec::new();
    if !item.context.is_empty() {
        heading.push(item.context.clone());
    }
    heading.push(item.signature.clone());
    heading
}

/// The body for a module item: its doc comment, or its signature when it has
/// none, so a span is never empty.
fn item_text(item: &crate::context::module_map::ItemSummary) -> String {
    if item.doc.is_empty() {
        item.signature.clone()
    } else {
        item.doc.clone()
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Tests
// ────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::exposure::ExposureState;
    use crate::context::{Category, IndexExposure, ShareMode};

    /// A core over a fixture repo, with a real grant minted into a temp store.
    ///
    /// Returns `None` when git is unavailable, matching `src/context`'s
    /// convention of skipping rather than failing on a box without it.
    struct Harness {
        core: QueryCore,
        token: String,
        _repo: tempfile::TempDir,
        _store: tempfile::TempDir,
    }

    fn harness(scopes: Vec<Scope>) -> Option<Harness> {
        harness_with_limits(scopes, None, None)
    }

    /// `harness`, with the grant's guardrails overridden.
    fn harness_with_limits(
        scopes: Vec<Scope>,
        rate_limit_per_min: Option<u32>,
        daily_query_budget: Option<u32>,
    ) -> Option<Harness> {
        let files = [
            (
                "CLAUDE.md",
                "# claudectl\n\n## Config layering\n\nCLI flags beat TOML.\n",
            ),
            ("README.md", "# readme\n\nA swarm orchestrator.\n"),
            (
                "docs/terminals.md",
                "# Terminals\n\n## Adding a terminal backend\n\nImplement the trait in terminals/mod.rs.\n",
            ),
            (
                "src/thing.rs",
                "//! A thing.\n\n/// Open the store.\npub fn open(path: &str) -> Store { todo!() }\n",
            ),
        ];
        let (repo, root) = crate::context::tests_support::git_fixture(&files)?;
        // Hide the categories that read the operator's real `$HOME`, so the
        // fixture is the whole index.
        let mut gate = IndexExposure::all_hidden();
        for c in [
            Category::ClaudeMd,
            Category::Readme,
            Category::Docs,
            Category::ModuleMap,
        ] {
            gate.set(c, ExposureState::Expose);
        }
        let index = ContextIndex::build_with(&root, &gate, ShareMode::Manual).ok()?;

        let store_dir = tempfile::tempdir().ok()?;
        let store = GrantStore::new(store_dir.path());
        let secret = access::token::load_or_create_secret(store.root()).ok()?;
        let project = "fixture".to_string();
        let expires = access::epoch_ms() + 60_000;
        let mut grant = access::new_grant(
            "gr_test01".into(),
            "test".into(),
            scopes,
            access::epoch_ms(),
            expires,
        );
        if let Some(r) = rate_limit_per_min {
            grant.rate_limit_per_min = r;
        }
        if let Some(b) = daily_query_budget {
            grant.daily_query_budget = b;
        }
        store.create(&grant).ok()?;
        let token = access::token::mint(&secret, &grant.grant_id, &grant.scopes, grant.expires_ms);

        Some(Harness {
            core: QueryCore::new(project, Arc::new(index), store, secret),
            token,
            _repo: repo,
            _store: store_dir,
        })
    }

    #[test]
    fn a_question_returns_cited_spans_from_the_index() {
        let Some(h) = harness(vec![Scope::ProjectQuery("fixture".into())]) else {
            eprintln!("skipping: git unavailable");
            return;
        };
        let answer = h
            .core
            .ask(&h.token, "how is config layering done?", None)
            .expect("authorized question");
        assert!(!answer.spans.is_empty(), "expected spans, got none");
        let top = &answer.spans[0];
        assert_eq!(top.path, "CLAUDE.md");
        assert_eq!(top.heading_path, vec!["claudectl", "Config layering"]);
        assert!(
            top.text.contains("CLI flags beat TOML"),
            "span text must be verbatim from the index, got {:?}",
            top.text
        );
        assert!(answer.matched_terms.contains(&"config".to_string()));
        assert!(answer.fingerprint.starts_with("fnv1a:"));
    }

    #[test]
    fn a_grant_without_project_query_is_denied() {
        let Some(h) = harness(vec![Scope::ProjectDocs("fixture".into())]) else {
            eprintln!("skipping: git unavailable");
            return;
        };
        assert_eq!(
            h.core.ask(&h.token, "anything", None),
            Err(QueryError::Denied),
            "project.docs must not buy question answering"
        );
    }

    #[test]
    fn a_grant_for_another_project_is_denied() {
        let Some(h) = harness(vec![Scope::ProjectQuery("other".into())]) else {
            eprintln!("skipping: git unavailable");
            return;
        };
        assert_eq!(
            h.core.ask(&h.token, "anything", None),
            Err(QueryError::Denied),
            "the scope is built from the served project, so another project's token cannot pass"
        );
    }

    #[test]
    fn a_garbage_token_is_denied_without_distinguishing_itself() {
        let Some(h) = harness(vec![Scope::ProjectQuery("fixture".into())]) else {
            eprintln!("skipping: git unavailable");
            return;
        };
        assert_eq!(
            h.core.ask("not-a-token", "q", None),
            Err(QueryError::Denied)
        );
        assert_eq!(
            h.core.ask("cctl_gr_nope_0123456789abcdef", "q", None),
            Err(QueryError::Denied)
        );
    }

    #[test]
    fn get_doc_needs_project_docs_and_returns_sections_verbatim() {
        let Some(h) = harness(vec![
            Scope::ProjectQuery("fixture".into()),
            Scope::ProjectDocs("fixture".into()),
        ]) else {
            eprintln!("skipping: git unavailable");
            return;
        };
        let doc = h
            .core
            .get_doc(&h.token, "docs/terminals.md")
            .expect("authorized doc fetch");
        assert_eq!(doc.path, "docs/terminals.md");
        assert!(
            doc.sections
                .iter()
                .any(|s| s.text.contains("Implement the trait")),
            "sections must be verbatim, got {:?}",
            doc.sections
        );
    }

    #[test]
    fn get_doc_on_an_unindexed_path_is_denied_not_a_filesystem_read() {
        let Some(h) = harness(vec![Scope::ProjectDocs("fixture".into())]) else {
            eprintln!("skipping: git unavailable");
            return;
        };
        for path in [
            "../../../etc/passwd",
            "/etc/passwd",
            ".env",
            "src/thing.rs",
            "docs/nope.md",
        ] {
            assert_eq!(
                h.core.get_doc(&h.token, path),
                Err(QueryError::Denied),
                "{path} is not an indexed doc path and must look like every other miss"
            );
        }
    }

    #[test]
    fn topics_lists_what_is_answerable_without_bodies() {
        let Some(h) = harness(vec![Scope::ProjectQuery("fixture".into())]) else {
            eprintln!("skipping: git unavailable");
            return;
        };
        let topics = h.core.topics(&h.token).expect("authorized topics");
        assert!(
            topics
                .topics
                .iter()
                .any(|t| t.heading_path.contains(&"Config layering".to_string())),
            "expected the CLAUDE.md heading in the table of contents"
        );
        assert!(
            topics
                .topics
                .iter()
                .any(|t| t.source == SpanSource::ModuleItem && t.path == "src/thing.rs"),
            "expected the module map path"
        );
    }

    #[test]
    fn serves_compares_and_never_resolves() {
        let Some(h) = harness(vec![Scope::ProjectQuery("fixture".into())]) else {
            eprintln!("skipping: git unavailable");
            return;
        };
        assert!(h.core.serves("fixture"));
        assert!(!h.core.serves("other"));
        assert!(!h.core.serves("Fixture"));
    }

    #[test]
    fn an_empty_or_oversized_question_is_a_bad_request_before_authorization() {
        let Some(h) = harness(vec![Scope::ProjectQuery("fixture".into())]) else {
            eprintln!("skipping: git unavailable");
            return;
        };
        assert!(matches!(
            h.core.ask(&h.token, "   ", None),
            Err(QueryError::BadRequest(_))
        ));
        let huge = "x".repeat(MAX_QUESTION_BYTES + 1);
        assert!(matches!(
            h.core.ask(&h.token, &huge, None),
            Err(QueryError::BadRequest(_))
        ));
    }

    #[test]
    fn a_question_matching_nothing_returns_no_spans_rather_than_a_guess() {
        let Some(h) = harness(vec![Scope::ProjectQuery("fixture".into())]) else {
            eprintln!("skipping: git unavailable");
            return;
        };
        let answer = h
            .core
            .ask(&h.token, "zygomorphic quetzalcoatl", None)
            .expect("authorized");
        assert!(
            answer.spans.is_empty(),
            "selection must return nothing rather than the least-bad span"
        );
    }

    /// At equal score, a documentation section must come before a module item.
    ///
    /// The fixture's `src/thing.rs` has `pub fn open(path: &str)` and its
    /// `docs/terminals.md` has a section mentioning the same word, so both
    /// match "open" with nothing else to separate them.
    #[test]
    fn prose_outranks_a_bare_signature_at_equal_score() {
        let Some(h) = harness(vec![Scope::ProjectQuery("fixture".into())]) else {
            eprintln!("skipping: git unavailable");
            return;
        };
        let answer = h.core.ask(&h.token, "store", None).expect("authorized");
        let sources: Vec<SpanSource> = answer.spans.iter().map(|s| s.source).collect();
        // Whatever matched, every doc span must precede every module span.
        let first_module = sources
            .iter()
            .position(|s| matches!(s, SpanSource::ModuleDoc | SpanSource::ModuleItem));
        let last_doc = sources.iter().rposition(|s| {
            matches!(
                s,
                SpanSource::ClaudeMd | SpanSource::Readme | SpanSource::Docs
            )
        });
        if let (Some(m), Some(d)) = (first_module, last_doc) {
            assert!(d < m, "docs must precede code at equal score: {sources:?}");
        }
    }

    /// A module item's citation has to name the item, not just its impl block.
    #[test]
    fn a_module_item_citation_identifies_the_item() {
        let Some(h) = harness(vec![Scope::ProjectQuery("fixture".into())]) else {
            eprintln!("skipping: git unavailable");
            return;
        };
        let answer = h.core.ask(&h.token, "open the store", Some(20)).unwrap();
        let item = answer
            .spans
            .iter()
            .find(|s| s.source == SpanSource::ModuleItem)
            .expect("expected a module item span");
        assert_eq!(item.path, "src/thing.rs");
        assert!(
            item.heading_path.iter().any(|h| h.contains("pub fn open")),
            "the signature must be in the heading path, or two items in one impl \
             block are indistinguishable: {:?}",
            item.heading_path
        );
    }

    #[test]
    fn the_span_limit_is_clamped() {
        let Some(h) = harness(vec![Scope::ProjectQuery("fixture".into())]) else {
            eprintln!("skipping: git unavailable");
            return;
        };
        let answer = h
            .core
            .ask(&h.token, "store terminal config readme", Some(9_999))
            .expect("authorized");
        assert!(answer.spans.len() <= MAX_SPAN_LIMIT);
    }

    #[test]
    fn the_same_question_twice_gives_byte_identical_spans() {
        let Some(h) = harness(vec![Scope::ProjectQuery("fixture".into())]) else {
            eprintln!("skipping: git unavailable");
            return;
        };
        let a = h.core.ask(&h.token, "config layering", None).unwrap();
        let b = h.core.ask(&h.token, "config layering", None).unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn a_successful_query_records_the_use_and_a_denial_does_not() {
        let Some(h) = harness(vec![Scope::ProjectQuery("fixture".into())]) else {
            eprintln!("skipping: git unavailable");
            return;
        };
        h.core.ask(&h.token, "config", None).unwrap();
        h.core.ask(&h.token, "layering", None).unwrap();
        let _ = h.core.ask("cctl_gr_nope_00", "config", None);
        let grant = h.core.store.load("gr_test01").unwrap().unwrap();
        assert_eq!(grant.use_count, 2, "only authorized calls count");
        assert!(grant.last_used_ms.is_some());

        let audit = h.core.store.read_audit(None);
        assert!(
            audit
                .iter()
                .any(|e| e.detail.as_deref() == Some("query.ask")),
            "an allowed query must be auditable, got {audit:?}"
        );
    }

    // ── #431: guardrails ────────────────────────────────────────────────────

    /// A day's worth of milliseconds into a known UTC day, so `budget_day`
    /// arithmetic is readable in the tests below.
    const DAY: u64 = access::MS_PER_DAY;
    const NOON: u64 = DAY * 20_000 + DAY / 2;

    fn query_scope() -> Vec<Scope> {
        vec![Scope::ProjectQuery("fixture".into())]
    }

    #[test]
    fn the_rate_limit_is_enforced_per_grant_and_reports_a_retry_hint() {
        let Some(h) = harness_with_limits(query_scope(), Some(2), None) else {
            eprintln!("skipping: git unavailable");
            return;
        };
        assert!(h.core.ask_at(&h.token, "config", None, NOON).is_ok());
        assert!(h.core.ask_at(&h.token, "config", None, NOON).is_ok());
        match h.core.ask_at(&h.token, "config", None, NOON) {
            Err(QueryError::Throttled {
                message,
                retry_after_secs,
            }) => {
                assert_eq!(message, "rate limited");
                assert!(
                    retry_after_secs.is_some_and(|s| s > 0),
                    "a throttled caller needs to know when to come back, got {retry_after_secs:?}"
                );
            }
            other => panic!("expected Throttled, got {other:?}"),
        }
    }

    #[test]
    fn a_rate_limited_request_is_audited_and_does_not_spend_the_daily_budget() {
        let Some(h) = harness_with_limits(query_scope(), Some(1), Some(10)) else {
            eprintln!("skipping: git unavailable");
            return;
        };
        h.core.ask_at(&h.token, "config", None, NOON).unwrap();
        assert!(h.core.ask_at(&h.token, "config", None, NOON).is_err());

        let grant = h.core.store.load("gr_test01").unwrap().unwrap();
        assert_eq!(
            grant.budget_used, 1,
            "the throttled request was never evaluated, so it must not be charged"
        );
        assert_eq!(
            grant.use_count, 1,
            "`record_query_use` runs only after the work succeeds"
        );
        let audit = h.core.store.read_audit(None);
        assert!(
            audit
                .iter()
                .any(|e| e.reason.as_deref() == Some("rate_limited")),
            "got {audit:?}"
        );
    }

    #[test]
    fn the_daily_budget_is_enforced_and_rolls_over_the_next_day() {
        let Some(h) = harness_with_limits(query_scope(), Some(1000), Some(2)) else {
            eprintln!("skipping: git unavailable");
            return;
        };
        assert!(h.core.ask_at(&h.token, "config", None, NOON).is_ok());
        assert!(h.core.ask_at(&h.token, "config", None, NOON).is_ok());
        match h.core.ask_at(&h.token, "config", None, NOON) {
            Err(QueryError::Throttled {
                message,
                retry_after_secs,
            }) => {
                assert_eq!(message, "daily budget exhausted");
                // Noon, so roughly half a day to the reset.
                let s = retry_after_secs.expect("a budget refusal knows when it resets");
                assert!((43_000..=43_300).contains(&s), "got {s}s");
            }
            other => panic!("expected Throttled, got {other:?}"),
        }

        // Tomorrow the counter resets without anything having run in between.
        assert!(
            h.core.ask_at(&h.token, "config", None, NOON + DAY).is_ok(),
            "the budget must roll over on the UTC day boundary"
        );
        let grant = h.core.store.load("gr_test01").unwrap().unwrap();
        assert_eq!(grant.budget_used, 1, "a fresh day starts from one");
        assert_eq!(grant.budget_day, access::utc_day(NOON + DAY));
    }

    /// §4.8's "denied queries count, so probing is self-limiting", read in the
    /// one way that does not hand an attacker a denial-of-service.
    #[test]
    fn a_missing_scope_denial_charges_the_budget_but_a_bad_mac_does_not() {
        let Some(h) = harness_with_limits(query_scope(), Some(1000), Some(10)) else {
            eprintln!("skipping: git unavailable");
            return;
        };
        // Holds a valid token, probing a verb it was not granted.
        assert_eq!(
            h.core.get_doc_at(&h.token, "docs/terminals.md", NOON),
            Err(QueryError::Denied)
        );
        let after_probe = h.core.store.load("gr_test01").unwrap().unwrap();
        assert_eq!(
            after_probe.budget_used, 1,
            "a valid token probing another scope is exactly what the budget is for"
        );

        // Does not hold the token. Charging this would let anyone who guesses a
        // 24-bit grant id drain the real holder's allowance.
        let forged = "cctl_gr_test01_00000000000000000000000000000000";
        assert_eq!(
            h.core.ask_at(forged, "config", None, NOON),
            Err(QueryError::Denied)
        );
        let after_forgery = h.core.store.load("gr_test01").unwrap().unwrap();
        assert_eq!(
            after_forgery.budget_used, 1,
            "a bad MAC must not spend the legitimate holder's budget"
        );
    }

    /// Probing past an exhausted budget writes two denial lines for one
    /// request — the missing scope *and* the exhausted budget. Both happened,
    /// so both are logged; pinned here so the double write reads as intended
    /// rather than being rediscovered as a bug.
    /// Each grant is throttled on its own limit, not a server-wide default.
    ///
    /// This is the test that catches someone later "simplifying"
    /// `try_acquire_with_capacity` back to `try_acquire` — which would
    /// silently give every grant the limiter's default and make
    /// `rate_limit_per_min` decorative.
    #[test]
    fn two_grants_on_one_server_are_throttled_on_their_own_limits() {
        let Some(h) = harness_with_limits(query_scope(), Some(1000), None) else {
            eprintln!("skipping: git unavailable");
            return;
        };
        // A second grant into the same store, allowed one query a minute.
        let mut tight = access::new_grant(
            "gr_test02".into(),
            "tight".into(),
            query_scope(),
            access::epoch_ms(),
            access::epoch_ms() + 60_000,
        );
        tight.rate_limit_per_min = 1;
        h.core.store.create(&tight).unwrap();
        let tight_token = access::token::mint(
            &h.core.secret,
            &tight.grant_id,
            &tight.scopes,
            tight.expires_ms,
        );

        assert!(h.core.ask_at(&tight_token, "config", None, NOON).is_ok());
        assert!(
            matches!(
                h.core.ask_at(&tight_token, "config", None, NOON),
                Err(QueryError::Throttled { .. })
            ),
            "gr_test02 allows one per minute"
        );

        // The generous grant is untouched by its neighbour's exhausted bucket.
        for _ in 0..5 {
            assert!(
                h.core.ask_at(&h.token, "config", None, NOON).is_ok(),
                "gr_test01's 1000/min must not be capped by gr_test02's 1/min"
            );
        }
    }

    #[test]
    fn a_missing_scope_probe_past_the_budget_audits_both_facts() {
        let Some(h) = harness_with_limits(query_scope(), Some(1000), Some(1)) else {
            eprintln!("skipping: git unavailable");
            return;
        };
        // Spend the single unit on a legitimate query.
        h.core.ask_at(&h.token, "config", None, NOON).unwrap();
        // Now probe a scope this grant lacks, with nothing left to charge.
        assert_eq!(
            h.core.get_doc_at(&h.token, "docs/terminals.md", NOON),
            Err(QueryError::Denied)
        );

        let audit = h.core.store.read_audit(Some("gr_test01"));
        let count = |reason: &str| {
            audit
                .iter()
                .filter(|e| e.reason.as_deref() == Some(reason))
                .count()
        };
        assert_eq!(count("missing_scope"), 1, "got {audit:?}");
        assert_eq!(count("budget_exhausted"), 1, "got {audit:?}");
    }

    /// A store that cannot be metered must refuse, not answer.
    ///
    /// Returning `Internal` (a `500`) here would hand out *unmetered* requests
    /// to anyone able to induce a transient store failure, because the charge
    /// never lands. The refusal is the same opaque denial as everything else.
    #[test]
    fn a_store_that_cannot_record_the_charge_denies_rather_than_answering() {
        let Some(h) = harness_with_limits(query_scope(), Some(1000), Some(100)) else {
            eprintln!("skipping: git unavailable");
            return;
        };
        assert!(h.core.ask_at(&h.token, "config", None, NOON).is_ok());

        // The grant vanishes between verification and the charge — what a
        // concurrent `access revoke` or a half-finished rename looks like.
        // `verify` reads it, so remove it after a successful read by pointing
        // the store at a grant file we then delete.
        let path = h.core.store.grants_dir().join("gr_test01.json");
        let saved = std::fs::read(&path).expect("grant file");
        std::fs::remove_file(&path).expect("remove");
        assert_eq!(
            h.core.ask_at(&h.token, "config", None, NOON),
            Err(QueryError::Denied),
            "an unverifiable grant is a denial, and never an answer"
        );

        // Restore and confirm the surface recovers rather than latching.
        std::fs::write(&path, saved).expect("restore");
        assert!(h.core.ask_at(&h.token, "config", None, NOON).is_ok());
    }

    #[test]
    fn a_freshly_minted_grant_serializes_without_accounting_noise() {
        // `docs/access.md` prints a grant file verbatim. #431 added two
        // counters, and a brand-new grant must not start showing them.
        let grant = access::new_grant(
            "gr_fresh1".into(),
            "fresh".into(),
            vec![Scope::ProjectQuery("p".into())],
            1,
            2,
        );
        let json = serde_json::to_string(&grant).unwrap();
        assert!(!json.contains("budget_day"), "got {json}");
        assert!(!json.contains("budget_used"), "got {json}");

        // And they round-trip once they are non-zero.
        let mut used = grant.clone();
        used.budget_day = 20_000;
        used.budget_used = 7;
        let back: access::Grant =
            serde_json::from_str(&serde_json::to_string(&used).unwrap()).expect("round trip");
        assert_eq!(back, used);
    }

    #[test]
    fn an_unknown_grant_does_not_charge_anything() {
        let Some(h) = harness_with_limits(query_scope(), Some(1000), Some(10)) else {
            eprintln!("skipping: git unavailable");
            return;
        };
        let _ = h
            .core
            .ask_at("cctl_gr_nope00_0123456789abcdef", "config", None, NOON);
        let grant = h.core.store.load("gr_test01").unwrap().unwrap();
        assert_eq!(grant.budget_used, 0);
    }

    #[test]
    fn the_audit_line_records_the_question_and_what_was_cited() {
        let Some(h) = harness(query_scope()) else {
            eprintln!("skipping: git unavailable");
            return;
        };
        let answer = h
            .core
            .ask_at(&h.token, "how is config layering done?", None, NOON)
            .unwrap();
        assert!(!answer.spans.is_empty());

        let audit = h.core.store.read_audit(Some("gr_test01"));
        let entry = audit
            .iter()
            .find(|e| e.detail.as_deref() == Some("query.ask"))
            .expect("an allowed ask must be audited");
        assert_eq!(
            entry.question.as_deref(),
            Some("how is config layering done?"),
            "the log has to say what was asked, not only how often"
        );
        let cited = entry.cited.as_ref().expect("cited paths");
        assert!(cited.contains(&"CLAUDE.md".to_string()), "got {cited:?}");
        // Deduplicated and sorted, so several spans from one file read as one
        // citation and the log stays diffable.
        let mut sorted = cited.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(*cited, sorted);
    }

    #[test]
    fn a_doc_fetch_audits_the_path_it_was_asked_for() {
        let Some(h) = harness(vec![Scope::ProjectDocs("fixture".into())]) else {
            eprintln!("skipping: git unavailable");
            return;
        };
        h.core
            .get_doc_at(&h.token, "docs/terminals.md", NOON)
            .unwrap();
        let audit = h.core.store.read_audit(Some("gr_test01"));
        let entry = audit
            .iter()
            .find(|e| e.detail.as_deref() == Some("query.get_doc"))
            .expect("audited");
        assert_eq!(entry.question.as_deref(), Some("docs/terminals.md"));
        assert_eq!(
            entry.cited.as_deref(),
            Some(&["docs/terminals.md".to_string()][..])
        );
    }

    #[test]
    fn topics_spends_the_budget_like_any_other_query() {
        let Some(h) = harness_with_limits(query_scope(), Some(1000), Some(1)) else {
            eprintln!("skipping: git unavailable");
            return;
        };
        assert!(h.core.topics_at(&h.token, NOON).is_ok());
        assert!(
            matches!(
                h.core.topics_at(&h.token, NOON),
                Err(QueryError::Throttled { .. })
            ),
            "listing topics is a query — it costs a unit like the rest"
        );
    }

    #[test]
    fn cited_paths_drops_pathless_spans_and_deduplicates() {
        let span = |source, path: &str| Span {
            source,
            path: path.to_string(),
            heading_path: Vec::new(),
            text: "t".into(),
            score: 1,
        };
        let spans = vec![
            span(SpanSource::Docs, "docs/b.md"),
            span(SpanSource::Docs, "docs/a.md"),
            span(SpanSource::Docs, "docs/b.md"),
            // Skills and hive units carry no path.
            span(SpanSource::Skill, ""),
        ];
        assert_eq!(cited_paths(&spans), vec!["docs/a.md", "docs/b.md"]);
    }

    #[test]
    fn seconds_until_midnight_spans_the_day() {
        assert_eq!(seconds_until_utc_midnight(DAY * 7), 86_400);
        assert_eq!(seconds_until_utc_midnight(DAY * 7 + DAY / 2), 43_200);
        // One millisecond before midnight still rounds up to a whole second,
        // so a `Retry-After: 0` can never tell a caller to retry immediately
        // into the same refusal.
        assert_eq!(seconds_until_utc_midnight(DAY * 8 - 1), 1);
        // Exactly midnight: a whole day ahead, not zero.
        assert_eq!(seconds_until_utc_midnight(DAY * 8), 86_400);
    }

    // ────────────────────────────────────────────────────────────────────
    // Review follow-ups on #431
    // ────────────────────────────────────────────────────────────────────

    #[test]
    fn a_doc_path_that_is_not_indexed_is_audited_rather_than_silently_charged() {
        let Some(h) = harness(vec![
            Scope::ProjectQuery("fixture".into()),
            Scope::ProjectDocs("fixture".into()),
        ]) else {
            eprintln!("skipping: git unavailable");
            return;
        };
        assert_eq!(
            h.core.get_doc(&h.token, "docs/nope.md"),
            Err(QueryError::Denied),
            "the caller still gets the one opaque refusal"
        );

        // The owner's side is the point: before this, the budget moved and the
        // log said nothing, so `access list` and `access audit` disagreed.
        let audit = h.core.store.read_audit(Some("gr_test01"));
        let last = audit.last().expect("a line");
        assert_eq!(last.event, "denied");
        assert_eq!(last.reason.as_deref(), Some("not_indexed"));
        assert_eq!(last.question.as_deref(), Some("docs/nope.md"));

        let g = h
            .core
            .store
            .load("gr_test01")
            .unwrap()
            .expect("grant present");
        assert_eq!(g.budget_used, 1, "it did cost a query");
        assert_eq!(g.use_count, 0, "but it was not a use");
    }

    #[test]
    fn a_wrong_scope_probe_is_throttled_like_any_other_request() {
        // One request per minute, and a grant that holds `project.query` only
        // while asking for a doc — so every request is a `MissingScope`
        // denial. Before this the arm returned before the rate limit and the
        // probe was unthrottled.
        let Some(h) = harness_with_limits(
            vec![Scope::ProjectQuery("fixture".into())],
            Some(1),
            Some(500),
        ) else {
            eprintln!("skipping: git unavailable");
            return;
        };
        for i in 0..4 {
            assert_eq!(
                h.core.get_doc(&h.token, "CLAUDE.md"),
                Err(QueryError::Denied),
                "request {i} must be refused"
            );
        }

        let g = h
            .core
            .store
            .load("gr_test01")
            .unwrap()
            .expect("grant present");
        // One token in the bucket, so exactly one request got as far as the
        // charge. The other three were stopped before touching the grant file.
        assert_eq!(
            g.budget_used, 1,
            "the throttle must bound the grant-file writes"
        );

        // And the refusal stays opaque. A 429 here would tell the caller this
        // project recognises their token, which §3.3 exists to withhold — the
        // success path earns a 429 by proving scope; this caller did not.
        let audit = h.core.store.read_audit(Some("gr_test01"));
        assert!(
            audit
                .iter()
                .all(|e| e.reason.as_deref() != Some("rate_limited")),
            "a throttled missing-scope probe must not add a second line"
        );
        assert_eq!(
            audit
                .iter()
                .filter(|e| e.reason.as_deref() == Some("missing_scope"))
                .count(),
            4,
            "verify_detailed logs each attempt before the throttle runs — the \
             appends are not what the throttle bounds"
        );
    }

    #[test]
    fn a_throttled_request_always_carries_a_retry_hint() {
        let Some(h) = harness_with_limits(
            vec![Scope::ProjectQuery("fixture".into())],
            Some(1),
            Some(500),
        ) else {
            eprintln!("skipping: git unavailable");
            return;
        };
        h.core
            .ask(&h.token, "how is config layering done?", None)
            .expect("first");
        match h.core.ask(&h.token, "again", None) {
            Err(QueryError::Throttled {
                retry_after_secs, ..
            }) => {
                // One `Instant` for the acquire and the hint. With two, a
                // refill between them returned `None` and the 429 shipped bare
                // while `docs/access.md` promised the header.
                assert!(
                    retry_after_secs.is_some(),
                    "docs/access.md promises a Retry-After on every 429"
                );
            }
            other => panic!("expected a throttle: {other:?}"),
        }
    }
}
