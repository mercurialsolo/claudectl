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

use std::cmp::Ordering;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::access::{self, AccessError, GrantStore, Scope};
use crate::context::{ContextIndex, docs::DocSection};

use super::rank;

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
}

impl std::fmt::Display for QueryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            QueryError::Denied => write!(f, "not found"),
            QueryError::BadRequest(m) => write!(f, "{m}"),
            QueryError::Internal(m) => write!(f, "{m}"),
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

    /// Verify `token` for `op` against the *served* project, then record the
    /// use.
    ///
    /// `verify` audits its own denials and does not mutate the grant;
    /// `record_use` is the separate bookkeeping call #427 split out for
    /// exactly this caller. It runs only on success, and its failure is
    /// reported rather than swallowed — a surface that cannot write its audit
    /// trail should not quietly keep answering.
    fn authorize(&self, token: &str, op: Operation) -> Result<access::Grant, QueryError> {
        let want = match op {
            Operation::Ask | Operation::Topics => Scope::ProjectQuery(self.project.clone()),
            Operation::GetDoc => Scope::ProjectDocs(self.project.clone()),
        };
        let now = access::epoch_ms();
        let grant = match self.store.verify(&self.secret, token, Some(&want), now) {
            Ok(g) => g,
            Err(AccessError::Denied) => return Err(QueryError::Denied),
        };
        self.store
            .record_use(&grant.grant_id, Some(op.audit_detail()), now)
            .map_err(QueryError::Internal)?;
        Ok(grant)
    }

    /// Answer a question with ranked verbatim spans.
    pub fn ask(
        &self,
        token: &str,
        question: &str,
        limit: Option<usize>,
    ) -> Result<Answer, QueryError> {
        if question.trim().is_empty() {
            return Err(QueryError::BadRequest("question is empty".into()));
        }
        if question.len() > MAX_QUESTION_BYTES {
            return Err(QueryError::BadRequest(format!(
                "question exceeds {MAX_QUESTION_BYTES} bytes"
            )));
        }
        self.authorize(token, Operation::Ask)?;

        let terms = rank::terms(question);
        let limit = limit.unwrap_or(DEFAULT_SPAN_LIMIT).clamp(1, MAX_SPAN_LIMIT);
        let (spans, truncated) = self.select(&terms, limit);
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
        self.authorize(token, Operation::Topics)?;
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
        if path.trim().is_empty() {
            return Err(QueryError::BadRequest("path is empty".into()));
        }
        self.authorize(token, Operation::GetDoc)?;

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
            return Err(QueryError::Denied);
        }
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
        let grant = access::new_grant(
            "gr_test01".into(),
            "test".into(),
            scopes,
            access::epoch_ms(),
            expires,
        );
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
}
