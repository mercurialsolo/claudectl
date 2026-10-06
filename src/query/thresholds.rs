//! The numbers the routing table compares against, and nothing else.
//!
//! They live in their own file because they are the part of #430 that is
//! *expected* to change. §4.3 is explicit that these are "starting values to be
//! tuned against real traffic, not constants", and the vendor's own confidence
//! guidance says thresholds must be validated on your own data. A tuning pass
//! should be one diff against one file, with the reasoning for each number
//! sitting next to it.
//!
//! # What has and has not been measured
//!
//! No request has been sent to `api.typesafe.ai` from this codebase — there is
//! no API key on the development machine. So these are the spec's values,
//! unvalidated against live traffic.
//!
//! What *is* measured is the **router**: `fixtures/adversarial.json` holds
//! adversarial queries paired with hand-authored Jev answers derived from the
//! published criteria, and the suite asserts each one routes as intended,
//! including at each threshold exactly, one step past it, and where two rules
//! fire at once. That proves the table is wired the way §4.3 describes. It does
//! not prove `0.15` is the right place to stand.

/// Above this, a query is denied as secret-seeking.
///
/// Paranoid on purpose, and §4.3 says why: "a false deny costs the third party
/// one rephrase, a false allow costs you a leak". The asymmetry is the whole
/// argument — there is no symmetric cost to balance against.
pub const SEEKS_SENSITIVE_DENY: f64 = 0.15;

/// Above this, a query is denied *and the grant is flagged* for owner review.
///
/// Same value and same asymmetry as [`SEEKS_SENSITIVE_DENY`], with one extra
/// consequence: an injection attempt says something about the holder, not just
/// the query, so it is worth the owner's attention even when the deny already
/// handled it.
pub const INJECTION_DENY: f64 = 0.15;

/// Below this, the query is declined as being about some other codebase.
///
/// `0.5` rather than something stricter because the cost is mis-routing, not
/// exposure: the index contains only this project, so a wrong-project question
/// that slips through retrieves nothing useful anyway. A strict value here
/// would decline legitimate questions that merely mention another tool.
pub const SCOPE_MATCH_DECLINE: f64 = 0.5;

/// How sure Jev must be that a query is `out_of_scope` before the surface
/// declines on that basis alone.
///
/// Higher than [`INTENT_CONFIDENCE_ANSWER`] because declining is a louder act
/// than answering: an uncertain `out_of_scope` should escalate to a human
/// rather than turn a real question away.
pub const OUT_OF_SCOPE_CONFIDENCE: f64 = 0.6;

/// How likely the query must be answerable from published documentation before
/// the surface answers it.
///
/// `0.7` leaves the middle band — "probably answerable, not clearly" — to the
/// escalation queue, which is the behaviour §4.3 asks for: genuine ambiguity
/// goes to a person, not to a guess.
pub const ANSWERABLE_FROM_DOCS: f64 = 0.7;

/// How sure the `intent` choice must be before the surface answers.
///
/// Lenient at `0.5` because `intent` does not gate exposure — it names a
/// category. Its job in the answer rule is to rule out "Jev had no idea what
/// this question was", which a coin flip across six categories already does.
pub const INTENT_CONFIDENCE_ANSWER: f64 = 0.5;

/// Span limit when a configured classifier is unavailable.
///
/// The strict fallback is not about the index holding something dangerous —
/// §4.4 is explicit that it does not. It is about not becoming *more
/// permissive* than the owner configured: they turned a gate on, the gate is
/// down, and the surface should answer less, not the same.
pub const DEGRADED_SPAN_LIMIT: usize = 3;

/// Minimum rank score for a span to be returned on the degraded path.
///
/// One term matching one body once scores `1`. Requiring `2` means a span must
/// either match a heading or match twice, which is the cheapest available
/// proxy for "this is actually about the question".
pub const DEGRADED_MIN_SCORE: u32 = 2;
