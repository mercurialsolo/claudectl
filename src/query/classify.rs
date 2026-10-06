//! Classification policy: §4.3's routing table, and what happens when the
//! classifier is not there.
//!
//! [`jev`](super::jev) knows the wire and nothing else. This file knows what a
//! probability means. [`route`] is a pure function of one [`Classification`],
//! so the table can be tested as a table — which is what
//! `fixtures/adversarial.json` does.
//!
//! # Jev is the router; code is the boundary
//!
//! §4.4 calls this "the single most important thing in this document", and it
//! constrains where this file sits. Classification runs *after* the grant's
//! scopes are verified and after its rate limit and daily budget are charged,
//! so Jev never sees a query the caller was not already entitled to ask. And
//! it runs *before* retrieval only in the sense of ordering — no classification
//! outcome can widen what retrieval may read, because retrieval reads the
//! pre-built index (§4.2) and nothing else. Removing this file entirely would
//! make the surface blunter, not less safe.
//!
//! # Two ways to have no classifier
//!
//! | state | behaviour |
//! | --- | --- |
//! | not configured (no `TYPESAFE_API_KEY`), or `jev_enabled = false` | exactly #429 — the deterministic baseline, unchanged |
//! | configured but unavailable (unreachable, 401, 429, malformed, spend ceiling) | **strict**: span limit capped at 3, spans scoring below 2 dropped |
//!
//! §4.6 lumps "no key" in with the fallback and asks for "a stricter
//! default-deny" in both cases. This is a deliberate departure, for two
//! reasons. #429 shipped the unconfigured path as the default behaviour of the
//! whole surface, and quietly tightening it here would regress a shipped
//! surface for every owner who never asked for classification. And §4.4 is
//! explicit that the index contains nothing sensitive by construction, so extra
//! strictness buys little on its own.
//!
//! What the strict mode *is* for is consistency: if an owner turned a gate on
//! and the gate is down, the surface must not become more permissive than they
//! configured. That is a fallback from an enabled gate, not a penalty for
//! never enabling one.

use std::path::Path;

use super::jev::{self, Classification, Intent, Transport};
use super::spend::{DEFAULT_MONTHLY_USD, SpendLedger};
use super::thresholds as th;

/// How much of the project's own prose is allowed to leave the machine.
///
/// §4.6: "Only the query text and a one-paragraph project summary leave the
/// machine." 512 bytes is about a paragraph, and the cap is on bytes rather
/// than characters so the limit cannot be stretched by multi-byte text.
pub const MAX_SUMMARY_BYTES: usize = 512;

/// Owner-set knobs. The API key is **not** here — see [`jev::API_KEY_ENV`].
#[derive(Debug, Clone, PartialEq)]
pub struct JevSettings {
    /// The hard off switch. A present key is the opt-in; this exists so an
    /// owner with a key in their environment for other reasons can still keep
    /// the surface local.
    pub enabled: bool,
    pub model: String,
    pub monthly_usd: f64,
}

impl Default for JevSettings {
    fn default() -> Self {
        JevSettings {
            enabled: true,
            model: jev::DEFAULT_MODEL.to_string(),
            monthly_usd: DEFAULT_MONTHLY_USD,
        }
    }
}

/// Which deny fired. Both answer the caller with the same opaque refusal; they
/// differ in what the owner is told and whether the grant is flagged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DenyKind {
    SeeksSensitive,
    InjectionAttempt,
}

impl DenyKind {
    pub fn as_str(self) -> &'static str {
        match self {
            DenyKind::SeeksSensitive => "seeks_sensitive",
            DenyKind::InjectionAttempt => "injection_attempt",
        }
    }

    /// Whether this deny also marks the grant for the owner's attention.
    ///
    /// Only injection does. A secret-seeking question is often a confused
    /// question; instruction-override framing says something about the holder
    /// rather than about the query.
    pub fn flags_grant(self) -> bool {
        matches!(self, DenyKind::InjectionAttempt)
    }
}

/// Which decline fired. Unlike a deny, a decline is *explained* to the caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeclineKind {
    WrongProject,
    OutOfScope,
}

impl DeclineKind {
    pub fn as_str(self) -> &'static str {
        match self {
            DeclineKind::WrongProject => "wrong_project",
            DeclineKind::OutOfScope => "out_of_scope",
        }
    }

    pub fn message(self) -> &'static str {
        match self {
            DeclineKind::WrongProject => "this question looks like it is about another codebase",
            DeclineKind::OutOfScope => {
                "this is not a question about this project's code or practices"
            }
        }
    }

    /// §4.3 asks a decline to carry "a pointer to what *is* answerable".
    ///
    /// A fixed string naming the categories, not an inlined topic list:
    /// `topics` is a metered operation, and a decline that quietly performs one
    /// would hand out a free listing to every caller who asked the wrong
    /// question.
    pub fn hint(self) -> &'static str {
        "answerable: structure, conventions, dependencies, api_usage, operations. \
         GET /api/v1/project/<project>/topics lists what is indexed."
    }
}

/// What the surface should do with this query.
#[derive(Debug, Clone, PartialEq)]
pub enum Route {
    /// Retrieve and answer.
    Answer {
        /// `None` when no classification happened.
        classification: Option<Classification>,
        /// `None` → no classifier is configured, so #429's behaviour applies
        /// unchanged. `Some(detail)` → a configured classifier was
        /// unavailable, so answer strictly and audit `detail`.
        degraded: Option<&'static str>,
    },
    /// Refuse, opaquely. The caller is told nothing about which signal fired.
    Deny {
        kind: DenyKind,
        classification: Classification,
    },
    /// Refuse, with a reason and a pointer.
    Decline {
        kind: DeclineKind,
        classification: Classification,
    },
    /// Queue for the owner and answer "pending review".
    Escalate(Classification),
}

impl Route {
    /// A stable label for the fixture suite.
    ///
    /// Test-only on purpose: production code matches on the variants, and a
    /// string that production branches on would be a second, stringly-typed
    /// copy of the routing table.
    #[cfg(test)]
    pub fn label(&self) -> String {
        match self {
            Route::Answer {
                degraded: Some(d), ..
            } => format!("degraded:{d}"),
            Route::Answer { .. } => "answer".into(),
            Route::Deny { kind, .. } => format!("deny:{}", kind.as_str()),
            Route::Decline { kind, .. } => format!("decline:{}", kind.as_str()),
            Route::Escalate(_) => "escalate".into(),
        }
    }

    /// The `classification` field for the audit line, when there is one.
    pub fn audit_classification(&self) -> Option<String> {
        match self {
            Route::Answer {
                classification: Some(c),
                ..
            } => Some(c.audit_summary()),
            Route::Answer {
                degraded: Some(d), ..
            } => Some((*d).to_string()),
            Route::Answer { .. } => None,
            Route::Deny {
                kind,
                classification,
            } => Some(format!(
                "deny={} {}",
                kind.as_str(),
                classification.audit_summary()
            )),
            Route::Decline {
                kind,
                classification,
            } => Some(format!(
                "decline={} {}",
                kind.as_str(),
                classification.audit_summary()
            )),
            Route::Escalate(c) => Some(format!("escalate {}", c.audit_summary())),
        }
    }
}

/// What [`Classifier::classify`] decided, plus whether the call was metered.
///
/// Separate from [`Route`] because the two are independent: a failed meter
/// must never change the routing decision, and the routing decision must
/// never hide a failed meter.
#[derive(Debug, Clone, PartialEq)]
pub struct Outcome {
    pub route: Route,
    /// `Some("jev.unmetered")` when the monthly ledger could not record this
    /// call's cost.
    ///
    /// Surfaced on **every** route, not just an answer. An unwritable ledger
    /// reads as zero spend, so the monthly ceiling silently stops biting — and
    /// an adversarial holder produces denies and declines, which is exactly
    /// where a marker attached only to answers would never appear.
    pub unmetered: Option<&'static str>,
}

impl Outcome {
    fn metered(route: Route) -> Self {
        Outcome {
            route,
            unmetered: None,
        }
    }

    /// The `classification` field for the audit line.
    ///
    /// Combines the route's own summary with the metering marker, so a failed
    /// meter never costs the five numbers an owner needs to tune thresholds —
    /// the request was classified and paid for either way.
    pub fn audit_classification(&self) -> Option<String> {
        match (self.route.audit_classification(), self.unmetered) {
            (Some(c), Some(note)) => Some(format!("{c} {note}")),
            (Some(c), None) => Some(c),
            (None, Some(note)) => Some(note.to_string()),
            (None, None) => None,
        }
    }

    /// The route's label, for tests that do not care about metering.
    #[cfg(test)]
    pub fn label(&self) -> String {
        self.route.label()
    }

    /// Whether the answer path should be the strict one.
    ///
    /// A configured classifier that could not be reached, or a call that could
    /// not be metered: either way the owner's configuration is not fully in
    /// force, so the surface answers less rather than the same.
    pub fn answer_strictly(&self) -> bool {
        self.unmetered.is_some()
            || matches!(
                self.route,
                Route::Answer {
                    degraded: Some(_),
                    ..
                }
            )
    }
}

/// §4.3's routing table, as a pure function.
///
/// Evaluation order is load-bearing and tested:
///
/// 1. **Injection before sensitive.** Both deny, so the only difference is
///    whether the grant is flagged — and a query that trips both absolutely
///    warrants the flag. §4.3 lists the conditions as a table rather than a
///    sequence, so this picks the order that loses the least information.
/// 2. **Both denies before either decline.** A secret-seeking question about
///    another project is a deny, not a polite "wrong project" with a pointer
///    to what else is available.
/// 3. **Both declines before the answer rule**, so a confidently out-of-scope
///    question is turned away rather than answered because it happened to look
///    documentable.
/// 4. **Escalate is the fallthrough**, which is what makes the middle band a
///    human's problem rather than a guess.
pub fn route(c: Classification) -> Route {
    if c.injection_attempt > th::INJECTION_DENY {
        return Route::Deny {
            kind: DenyKind::InjectionAttempt,
            classification: c,
        };
    }
    if c.seeks_sensitive > th::SEEKS_SENSITIVE_DENY {
        return Route::Deny {
            kind: DenyKind::SeeksSensitive,
            classification: c,
        };
    }
    if c.scope_match < th::SCOPE_MATCH_DECLINE {
        return Route::Decline {
            kind: DeclineKind::WrongProject,
            classification: c,
        };
    }
    if c.intent == Intent::OutOfScope && c.intent_confidence > th::OUT_OF_SCOPE_CONFIDENCE {
        return Route::Decline {
            kind: DeclineKind::OutOfScope,
            classification: c,
        };
    }
    if c.answerable_from_docs > th::ANSWERABLE_FROM_DOCS
        && c.intent_confidence > th::INTENT_CONFIDENCE_ANSWER
    {
        return Route::Answer {
            classification: Some(c),
            degraded: None,
        };
    }
    Route::Escalate(c)
}

/// The deterministic baseline: no classifier configured, #429 unchanged.
fn baseline() -> Route {
    Route::Answer {
        classification: None,
        degraded: None,
    }
}

/// A configured classifier that could not be used.
fn degraded(detail: &'static str) -> Route {
    Route::Answer {
        classification: None,
        degraded: Some(detail),
    }
}

enum State {
    /// No classifier. Carries the reason for the startup banner, not for the
    /// audit log — a static fact belongs where it is printed once, not on
    /// every request line.
    Inactive(&'static str),
    Active {
        model: String,
        monthly_usd: f64,
        transport: Box<dyn Transport>,
        spend: SpendLedger,
    },
}

/// The classifier, active or not.
///
/// Held unconditionally by `QueryCore` rather than as an `Option`, so there is
/// exactly one `classify` call site and no `if let` around the gate.
pub struct Classifier {
    state: State,
}

impl Classifier {
    /// No classification, for the stated reason.
    pub fn inactive(reason: &'static str) -> Self {
        Classifier {
            state: State::Inactive(reason),
        }
    }

    /// Build from the environment and the owner's settings.
    ///
    /// The key is read once, at startup, rather than per request: a surface
    /// whose credentials can change under it mid-run is harder to reason about
    /// than one that is restarted.
    pub fn from_env(settings: &JevSettings, access_root: &Path) -> Self {
        if !settings.enabled {
            return Self::inactive("disabled in config");
        }
        // Trimmed, not just checked: `export TYPESAFE_API_KEY=$(cat keyfile)`
        // routinely carries a trailing newline, and an untrimmed key would
        // split the `-H @-` block into a header plus a blank line.
        let key = std::env::var(jev::API_KEY_ENV).unwrap_or_default();
        let key = key.trim();
        if key.is_empty() {
            return Self::inactive("TYPESAFE_API_KEY is not set");
        }
        Self::with_transport(
            settings,
            access_root,
            Box::new(jev::CurlTransport::new(key.to_string())),
        )
    }

    /// Active over an injected transport. The production path goes through
    /// [`Self::from_env`]; this is what makes the privacy claim testable.
    pub fn with_transport(
        settings: &JevSettings,
        access_root: &Path,
        transport: Box<dyn Transport>,
    ) -> Self {
        Classifier {
            state: State::Active {
                model: settings.model.clone(),
                monthly_usd: settings.monthly_usd,
                transport,
                spend: SpendLedger::in_access_dir(access_root),
            },
        }
    }

    pub fn is_active(&self) -> bool {
        matches!(self.state, State::Active { .. })
    }

    /// One line for `query serve`'s startup banner.
    pub fn describe(&self) -> String {
        match &self.state {
            State::Inactive(reason) => {
                format!("off ({reason}) — deterministic term matching")
            }
            State::Active {
                model, monthly_usd, ..
            } => format!("Jev {model}, ceiling ${monthly_usd:.2}/month"),
        }
    }

    /// Classify, or explain why not.
    ///
    /// Sequence: ceiling → call → route → charge.
    ///
    /// The ceiling is checked *before* the call because a request cannot be
    /// un-sent, and the charge happens *after* [`route`] so a ledger that
    /// cannot be written can never weaken a deny. A failed charge is recorded
    /// in the audit string rather than swallowed — it means the month is
    /// under-counted, which the owner should see.
    pub fn classify(&self, project: &str, summary: &str, question: &str, now_ms: u64) -> Outcome {
        let State::Active {
            model,
            monthly_usd,
            transport,
            spend,
        } = &self.state
        else {
            return Outcome::metered(baseline());
        };

        if spend.exceeded(*monthly_usd, now_ms) {
            return Outcome::metered(degraded("jev.spend_ceiling"));
        }

        let classification = match jev::call(transport.as_ref(), model, project, summary, question)
        {
            Ok(c) => c,
            // Every variant degrades closed; the detail is the whole
            // difference, because "you forgot the key" and "the API is
            // down" need different responses from the owner. The mapping
            // lives on `JevError` so there is one copy of it.
            Err(e) => return Outcome::metered(degraded(e.audit_detail())),
        };

        let routed = route(classification);
        if spend.charge(classification.input_tokens, now_ms).is_err() {
            // The decision stands — it was already paid for, and discarding a
            // deny because a ledger write failed would be the fail-open #431
            // removed from `charge_daily_budget`. What changes is that the
            // audit line says so on every route, and the answer path tightens.
            return Outcome {
                route: routed,
                unmetered: Some("jev.unmetered"),
            };
        }
        Outcome::metered(routed)
    }
}

/// The one paragraph of local prose that leaves the machine.
///
/// §4.6 says "a one-paragraph project summary", from `CLAUDE.md`. The first
/// section of the project's own `CLAUDE.md` is the project describing itself;
/// `README.md` is the fallback for a project that has no `CLAUDE.md`, because
/// the alternative is sending Jev no context at all and getting worse
/// classification for it.
///
/// Truncated on a char boundary, so attacker-influenced UTF-8 cannot panic the
/// slice — the same care `truncate_for_audit` takes.
pub fn project_summary(index: &crate::context::ContextIndex) -> String {
    let body = index
        .claude_md
        .first()
        .map(|s| s.body.as_str())
        .filter(|b| !b.trim().is_empty())
        .or_else(|| {
            index
                .readme
                .first()
                .map(|s| s.body.as_str())
                .filter(|b| !b.trim().is_empty())
        })
        .unwrap_or_default()
        .trim();
    if body.len() <= MAX_SUMMARY_BYTES {
        return body.to_string();
    }
    let mut cut = MAX_SUMMARY_BYTES;
    while cut > 0 && !body.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}…", &body[..cut])
}

/// Fake transports, shared with `core`'s integration tests.
///
/// Lives here rather than in each test module because the routing table and
/// the path that acts on it must be exercised against the *same* fake — two
/// hand-rolled stubs would be two chances for one of them to drift from the
/// documented response shape.
#[cfg(test)]
pub mod test_support {
    use std::sync::{Arc, Mutex};

    use super::super::jev::Transport;

    /// Replies with a fixed `(status, body)` and records every request body.
    pub struct Fake {
        bodies: Mutex<Vec<String>>,
        status: u16,
        body: String,
        /// `true` to fail at the transport layer, as a timeout does.
        dead: bool,
    }

    impl Fake {
        pub fn replying(status: u16, body: String) -> Arc<Self> {
            Arc::new(Fake {
                bodies: Mutex::new(Vec::new()),
                status,
                body,
                dead: false,
            })
        }

        /// A documented `200` carrying the five answers you name.
        pub fn classifying(
            intent: &str,
            intent_confidence: f64,
            docs: f64,
            sens: f64,
            inj: f64,
            scope: f64,
        ) -> Arc<Self> {
            Self::replying(
                200,
                answers_json(intent, intent_confidence, docs, sens, inj, scope),
            )
        }

        /// A transport that never reaches the service.
        pub fn dead() -> Arc<Self> {
            Arc::new(Fake {
                bodies: Mutex::new(Vec::new()),
                status: 0,
                body: String::new(),
                dead: true,
            })
        }

        /// Every request body this transport was handed.
        pub fn sent(&self) -> Vec<String> {
            self.bodies.lock().unwrap().clone()
        }
    }

    // On `Arc<Fake>` so a test can keep a handle and still hand the classifier
    // a `Box<dyn Transport>`.
    impl Transport for Arc<Fake> {
        fn post(&self, body: &str) -> Result<(u16, String), String> {
            self.bodies.lock().unwrap().push(body.to_string());
            if self.dead {
                return Err("curl exit 28: operation timed out".into());
            }
            Ok((self.status, self.body.clone()))
        }
    }

    /// The documented response shape, built once so every fake agrees with it.
    pub fn answers_json(
        intent: &str,
        intent_confidence: f64,
        docs: f64,
        sens: f64,
        inj: f64,
        scope: f64,
    ) -> String {
        serde_json::json!({
            "model": "jev-1.13.0",
            "answers": {
                "intent": { "type": "choice", "choice": intent, "confidence": intent_confidence },
                "answerable_from_docs": { "type": "noul", "noul": docs },
                "seeks_sensitive": { "type": "noul", "noul": sens },
                "injection_attempt": { "type": "noul", "noul": inj },
                "scope_match": { "type": "noul", "noul": scope }
            },
            "usage": { "input_tokens": 600, "output_tokens": 0 }
        })
        .to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::{Fake, answers_json as answers};
    use super::*;

    const NOW: u64 = 1_791_200_000_000;

    fn tmpdir(tag: &str) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!(
            "claudectl-classify-{tag}-{}-{}",
            std::process::id(),
            crate::access::epoch_ms()
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn clean(intent: Intent) -> Classification {
        Classification {
            intent,
            intent_confidence: 0.9,
            answerable_from_docs: 0.95,
            seeks_sensitive: 0.01,
            injection_attempt: 0.01,
            scope_match: 0.99,
            input_tokens: 600,
        }
    }

    // ---- the table ----

    #[test]
    fn a_clean_documentable_question_is_answered() {
        assert_eq!(route(clean(Intent::Structure)).label(), "answer");
    }

    #[test]
    fn the_thresholds_are_exclusive_at_the_boundary() {
        // `> 0.15` means exactly 0.15 does not deny. Stated here so a future
        // change from `>` to `>=` fails a test instead of silently tightening.
        let mut c = clean(Intent::Structure);
        c.seeks_sensitive = th::SEEKS_SENSITIVE_DENY;
        assert_eq!(route(c).label(), "answer");
        c.seeks_sensitive = th::SEEKS_SENSITIVE_DENY + 0.001;
        assert_eq!(route(c).label(), "deny:seeks_sensitive");

        // `< 0.5` means exactly 0.5 does not decline.
        let mut c = clean(Intent::Structure);
        c.scope_match = th::SCOPE_MATCH_DECLINE;
        assert_eq!(route(c).label(), "answer");
        c.scope_match = th::SCOPE_MATCH_DECLINE - 0.001;
        assert_eq!(route(c).label(), "decline:wrong_project");

        // `> 0.7` to answer: exactly 0.7 is not enough, so it escalates.
        let mut c = clean(Intent::Structure);
        c.answerable_from_docs = th::ANSWERABLE_FROM_DOCS;
        assert_eq!(route(c).label(), "escalate");
        c.answerable_from_docs = th::ANSWERABLE_FROM_DOCS + 0.001;
        assert_eq!(route(c).label(), "answer");
    }

    #[test]
    fn injection_outranks_sensitive_so_the_grant_still_gets_flagged() {
        let mut c = clean(Intent::Structure);
        c.seeks_sensitive = 0.99;
        c.injection_attempt = 0.99;
        match route(c) {
            Route::Deny { kind, .. } => {
                assert_eq!(kind, DenyKind::InjectionAttempt);
                assert!(kind.flags_grant());
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_deny_outranks_a_decline() {
        // Secret-seeking *and* about another codebase: deny, not a polite
        // pointer to what else is available.
        let mut c = clean(Intent::Structure);
        c.seeks_sensitive = 0.9;
        c.scope_match = 0.1;
        assert_eq!(route(c).label(), "deny:seeks_sensitive");

        // Injection framing wrapped around a perfectly documentable question.
        let mut c = clean(Intent::Structure);
        c.injection_attempt = 0.8;
        c.answerable_from_docs = 0.99;
        assert_eq!(route(c).label(), "deny:injection_attempt");
    }

    #[test]
    fn an_out_of_scope_intent_only_declines_once_it_is_confident() {
        let mut c = clean(Intent::OutOfScope);
        c.intent_confidence = th::OUT_OF_SCOPE_CONFIDENCE + 0.01;
        assert_eq!(route(c).label(), "decline:out_of_scope");

        // Exactly at the line does not decline, and the answer rule looks at
        // how *confident* the intent is rather than at which intent it was —
        // so a clearly documentable question still gets answered.
        c.intent_confidence = th::OUT_OF_SCOPE_CONFIDENCE;
        assert_eq!(route(c).label(), "answer");

        // Drop the documentation signal and the same query escalates. Below
        // the decline threshold, `out_of_scope` is a hint, not a verdict.
        c.answerable_from_docs = 0.4;
        assert_eq!(route(c).label(), "escalate");
    }

    #[test]
    fn a_vague_question_escalates() {
        let mut c = clean(Intent::Structure);
        c.answerable_from_docs = 0.4;
        c.intent_confidence = 0.3;
        assert_eq!(route(c).label(), "escalate");
    }

    #[test]
    fn only_injection_flags_the_grant() {
        assert!(!DenyKind::SeeksSensitive.flags_grant());
        assert!(DenyKind::InjectionAttempt.flags_grant());
    }

    // ---- the fixture suite ----

    #[derive(serde::Deserialize)]
    struct Fixture {
        name: String,
        question: String,
        answers: FixtureAnswers,
        expect: String,
        #[allow(dead_code)]
        why: String,
    }

    #[derive(serde::Deserialize)]
    struct FixtureAnswers {
        intent: String,
        intent_confidence: f64,
        answerable_from_docs: f64,
        seeks_sensitive: f64,
        injection_attempt: f64,
        scope_match: f64,
    }

    #[test]
    fn the_adversarial_fixtures_route_as_intended() {
        let raw = include_str!("fixtures/adversarial.json");
        let fixtures: Vec<Fixture> = serde_json::from_str(raw).expect("fixtures parse");
        assert!(
            fixtures.len() >= 20,
            "the acceptance asks for a suite, not a sample: {}",
            fixtures.len()
        );
        for f in &fixtures {
            // Through the real parser, so a fixture also pins the wire shape.
            let body = answers(
                &f.answers.intent,
                f.answers.intent_confidence,
                f.answers.answerable_from_docs,
                f.answers.seeks_sensitive,
                f.answers.injection_attempt,
                f.answers.scope_match,
            );
            let c = jev::parse_response(&body)
                .unwrap_or_else(|e| panic!("fixture `{}` has a bad shape: {e:?}", f.name));
            assert_eq!(
                route(c).label(),
                f.expect,
                "fixture `{}` ({}) routed wrong",
                f.name,
                f.question
            );
        }
    }

    #[test]
    fn the_fixture_suite_covers_every_row_of_the_table() {
        let raw = include_str!("fixtures/adversarial.json");
        let fixtures: Vec<Fixture> = serde_json::from_str(raw).unwrap();
        for expected in [
            "answer",
            "escalate",
            "deny:seeks_sensitive",
            "deny:injection_attempt",
            "decline:wrong_project",
            "decline:out_of_scope",
        ] {
            assert!(
                fixtures.iter().any(|f| f.expect == expected),
                "no fixture exercises `{expected}`"
            );
        }
    }

    // ---- privacy ----

    #[test]
    fn a_request_carries_the_question_and_nothing_from_the_index() {
        // The privacy claim in §4.6, as a test. Without this it is prose.
        let dir = tmpdir("privacy");
        let fake = Fake::classifying("structure", 0.9, 0.95, 0.01, 0.01, 0.99);
        let c = Classifier::with_transport(&JevSettings::default(), &dir, Box::new(fake.clone()));

        let outcome = c.classify(
            "claudectl",
            "claudectl orchestrates a swarm of Claude Code agents.",
            "where does config layering live?",
            NOW,
        );
        assert_eq!(outcome.route.label(), "answer");

        let bodies = fake.sent();
        assert_eq!(bodies.len(), 1, "one request, five answers");
        let sent = &bodies[0];
        assert!(sent.contains("where does config layering live?"));
        assert!(sent.contains("claudectl orchestrates a swarm"));

        // The structural half, and the one with teeth at this level: `state`
        // carries exactly the two local strings §4.6 permits plus the fixed
        // category list, so a new field cannot be added without failing here.
        //
        // Asserting that specific index strings are *absent* would be vacuous
        // here — `classify` takes `project`, `summary` and `question` and has
        // no `ContextIndex` to leak from. That claim is tested one layer up,
        // where the index exists, by
        // `query::core::tests::a_classification_request_cannot_carry_index_content`.
        let json: serde_json::Value = serde_json::from_str(sent).unwrap();
        let state = json["state"].as_object().unwrap();
        let mut keys: Vec<&str> = state.keys().map(|k| k.as_str()).collect();
        keys.sort();
        assert_eq!(
            keys,
            [
                "available_categories",
                "project",
                "project_summary",
                "query"
            ]
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn the_summary_is_one_paragraph_cut_on_a_char_boundary() {
        // Multi-byte, so a naive slice at 512 would panic.
        let long = "é".repeat(600);
        let index = crate::context::ContextIndex {
            root: String::new(),
            claude_md: vec![crate::context::docs::DocSection {
                path: "CLAUDE.md".into(),
                heading_path: vec![],
                body: long,
            }],
            readme: vec![],
            docs: vec![],
            module_map: vec![],
            skills: vec![],
            hive_units: vec![],
            stats: Default::default(),
        };
        let s = project_summary(&index);
        assert!(s.ends_with('…'));
        assert!(s.len() <= MAX_SUMMARY_BYTES + '…'.len_utf8());
    }

    #[test]
    fn the_summary_falls_back_to_the_readme_and_then_to_nothing() {
        let mut index = crate::context::ContextIndex {
            root: String::new(),
            claude_md: vec![],
            readme: vec![crate::context::docs::DocSection {
                path: "README.md".into(),
                heading_path: vec![],
                body: "  from the readme  ".into(),
            }],
            docs: vec![],
            module_map: vec![],
            skills: vec![],
            hive_units: vec![],
            stats: Default::default(),
        };
        assert_eq!(project_summary(&index), "from the readme");
        index.readme.clear();
        assert_eq!(project_summary(&index), "");
    }

    // ---- degrade closed ----

    #[test]
    fn an_unconfigured_classifier_keeps_the_shipped_baseline() {
        let c = Classifier::inactive("TYPESAFE_API_KEY is not set");
        assert!(!c.is_active());
        let r = c.classify("p", "s", "q", NOW);
        assert_eq!(r.label(), "answer");
        // No classification field on the audit line: nothing was classified,
        // and #429's log shape is unchanged for owners who never opted in.
        assert_eq!(r.audit_classification(), None);
    }

    #[test]
    fn an_unreachable_classifier_degrades_closed() {
        let dir = tmpdir("unreachable");
        let c = Classifier::with_transport(&JevSettings::default(), &dir, Box::new(Fake::dead()));
        let r = c.classify("p", "s", "q", NOW);
        assert_eq!(r.label(), "degraded:jev.unreachable");
        assert_eq!(
            r.audit_classification().as_deref(),
            Some("jev.unreachable"),
            "the owner must be able to tell an outage from a missing key"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_bad_key_and_a_throttled_api_degrade_with_their_own_details() {
        let dir = tmpdir("statuses");
        for (status, want) in [
            (401, "degraded:jev.unauthorized"),
            (429, "degraded:jev.rate_limited"),
            (503, "degraded:jev.unreachable"),
            (200, "degraded:jev.malformed"), // 200 with an empty body
        ] {
            let c = Classifier::with_transport(
                &JevSettings::default(),
                &dir,
                Box::new(Fake::replying(status, String::new())),
            );
            assert_eq!(c.classify("p", "s", "q", NOW).label(), want, "{status}");
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn the_spend_ceiling_degrades_without_calling_and_without_charging() {
        let dir = tmpdir("ceiling");
        let ledger = SpendLedger::in_access_dir(&dir);
        // Pre-write the ledger past the ceiling: one million input tokens is
        // $0.042, and the ceiling below is a cent.
        ledger.charge(1_000_000, NOW).unwrap();
        let before = std::fs::read_to_string(ledger.path()).unwrap();

        let settings = JevSettings {
            monthly_usd: 0.01,
            ..Default::default()
        };
        let fake = Fake::classifying("structure", 0.9, 0.95, 0.01, 0.01, 0.99);
        let c = Classifier::with_transport(&settings, &dir, Box::new(fake.clone()));
        assert_eq!(
            c.classify("p", "s", "q", NOW).label(),
            "degraded:jev.spend_ceiling"
        );
        assert!(
            fake.sent().is_empty(),
            "the ceiling must be checked before the request, not after"
        );
        assert_eq!(std::fs::read_to_string(ledger.path()).unwrap(), before);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_successful_classification_charges_the_month() {
        let dir = tmpdir("charge");
        let c = Classifier::with_transport(
            &JevSettings::default(),
            &dir,
            Box::new(Fake::classifying("structure", 0.9, 0.95, 0.01, 0.01, 0.99)),
        );
        assert_eq!(c.classify("p", "s", "q", NOW).label(), "answer");
        let spent = SpendLedger::in_access_dir(&dir).read(NOW);
        assert_eq!(spent.calls, 1);
        assert!(spent.usd > 0.0);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_failed_meter_never_changes_the_route_and_is_audited_on_all_of_them() {
        // A ledger that cannot be written must never soften a refusal — that
        // is the fail-open #431 removed from `charge_daily_budget`. It must
        // also never go unnoticed: an unwritable ledger reads as zero spend,
        // so the monthly ceiling stops biting, and an adversarial holder
        // produces denies and declines rather than answers. A marker attached
        // only to the answer path would never appear for them.
        let mut sensitive = clean(Intent::Structure);
        sensitive.seeks_sensitive = 0.9;
        let mut wrong_project = clean(Intent::Structure);
        wrong_project.scope_match = 0.1;
        let mut vague = clean(Intent::Structure);
        vague.answerable_from_docs = 0.4;

        for (c, expected_route) in [
            (sensitive, "deny:seeks_sensitive"),
            (wrong_project, "decline:wrong_project"),
            (vague, "escalate"),
            (clean(Intent::Structure), "answer"),
        ] {
            let outcome = Outcome {
                route: route(c),
                unmetered: Some("jev.unmetered"),
            };
            assert_eq!(
                outcome.route.label(),
                expected_route,
                "the route must be untouched"
            );
            let line = outcome
                .audit_classification()
                .expect("a classified request always has a line");
            assert!(
                line.ends_with("jev.unmetered"),
                "the metering failure must be visible on {expected_route}: {line}"
            );
            // And the five numbers survive, which is the whole reason the
            // audit line exists — the request was classified and paid for.
            assert!(line.contains("intent=structure/"), "{line}");
            assert!(outcome.answer_strictly(), "an unmetered answer tightens");
        }
    }

    #[test]
    fn a_metered_answer_carries_only_the_numbers() {
        let outcome = Outcome {
            route: route(clean(Intent::Structure)),
            unmetered: None,
        };
        let line = outcome.audit_classification().unwrap();
        assert!(!line.contains("jev.unmetered"), "{line}");
        assert!(!outcome.answer_strictly());
    }

    #[test]
    fn disabled_in_config_is_inactive_before_the_key_is_even_looked_at() {
        let settings = JevSettings {
            enabled: false,
            ..Default::default()
        };
        let c = Classifier::from_env(&settings, Path::new("/nonexistent"));
        assert!(!c.is_active());
        assert!(
            c.describe().contains("disabled in config"),
            "{}",
            c.describe()
        );
    }

    #[test]
    fn the_banner_says_which_model_and_what_the_ceiling_is() {
        let dir = tmpdir("banner");
        let c = Classifier::with_transport(
            &JevSettings::default(),
            &dir,
            Box::new(Fake::replying(200, String::new())),
        );
        let line = c.describe();
        assert!(line.contains("jev-latest"), "{line}");
        assert!(line.contains("$5.00/month"), "{line}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_decline_points_at_categories_rather_than_running_topics() {
        let hint = DeclineKind::OutOfScope.hint();
        assert!(hint.contains("structure"));
        assert!(hint.contains("/topics"));
        // No index content: the pointer is a fixed string, so a decline cannot
        // become a free listing.
        assert_eq!(hint, DeclineKind::WrongProject.hint());
    }

    #[test]
    fn the_audit_string_names_the_outcome_and_the_numbers() {
        let mut c = clean(Intent::Structure);
        c.injection_attempt = 0.9;
        let line = route(c).audit_classification().unwrap();
        assert!(line.starts_with("deny=injection_attempt"), "{line}");
        assert!(line.contains("inj=0.90"), "{line}");
    }
}
