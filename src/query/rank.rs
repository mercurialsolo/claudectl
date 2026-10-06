//! Deterministic term-overlap ranking (#429, open-cluster RFC §4.5).
//!
//! The RFC's eventual answer path uses Jev to select among candidate spans
//! (§4.5), but §4.6 is explicit that with no `TYPESAFE_API_KEY` the surface
//! "falls back to deterministic intent matching" — and #429 ships that
//! fallback *first*, deliberately, so the classifier arrives as a measurable
//! improvement to a working system rather than being load-bearing on day one.
//!
//! The whole algorithm is term overlap with headings weighted, and it is
//! deliberately plain. Two properties matter more than cleverness here:
//!
//! 1. **Integers end to end.** `ContextIndex` already guarantees a
//!    byte-identical build from identical input; a ranker that sorted on `f64`
//!    would reintroduce `partial_cmp` and platform-dependent ordering into a
//!    surface whose whole claim is determinism. Every score is a `u32` and
//!    every sort is a total order.
//! 2. **Distinct terms, not occurrences.** A span scores once per *query term*
//!    it contains, however many times that term appears. Repetition therefore
//!    buys nothing, which is what stops a 2,000-word section from outranking
//!    the one-paragraph section that actually answers the question — without
//!    needing a length normalizer, and so without needing floats.

use std::collections::HashSet;

/// Weight for a term in the span's *own* heading — the last element of its
/// heading path.
///
/// A heading is the author's own statement of what the section is about, so a
/// hit there is better evidence than a hit in prose. Three is enough that one
/// heading match outranks two body matches.
pub const OWN_HEADING_WEIGHT: u32 = 3;

/// Weight for a term in an *enclosing* heading.
///
/// Ancestors have to count for less, and running this against the real repo is
/// what proved it. `docs/AGENT_BUS.md` is titled "claudectl Agent Bus — Design
/// Specification", so every one of its several dozen subsections inherits both
/// words of "agent bus" — and at full weight they all tied, which handed the
/// top of the result list to whichever subsection happened to be shortest
/// ("Native loop safety", "2. Install the plugin"). An ancestor says the
/// section is *somewhere in* the right document; its own heading says the
/// section is *about* the thing.
pub const ANCESTOR_HEADING_WEIGHT: u32 = 1;

/// Minimum term length for prefix matching. Below this, only exact token
/// matches count — a 3-character prefix matches far too much to be evidence
/// ("con" would hit "config", "connect", "contains" and "context" alike).
pub const MIN_PREFIX_LEN: usize = 4;

/// Cap on how many distinct terms one question contributes.
///
/// Bounds the per-span work at `terms × tokens` for an attacker-supplied
/// question. The question byte cap in `core` already bounds the input size;
/// this bounds the comparisons those bytes can buy.
pub const MAX_TERMS: usize = 32;

/// Words carrying no retrieval signal, so scoring them would rank every span
/// that happens to be wordy.
///
/// Kept short on purpose. An aggressive list starts throwing away real terms
/// ("use", "type" and "where" are all Rust keywords someone may be asking
/// about), and the distinct-terms rule already means a common word can only
/// ever contribute one point.
///
/// The `work` family is on the list for a reason worth recording, because it
/// looks like a content word in a codebase that has worktrees and workspaces.
/// "How does X work?" is the canonical question form — it is three of the four
/// examples in RFC §4.1 — and run against this repo, "work" prefix-matched
/// `workspace`, `work-bearing` and the `/work/proj` paths in the agent-bus
/// examples, which put "Workspace layout" at the top of a question about
/// config layering. Someone who means the git feature types "worktree".
const STOPWORDS: &[&str] = &[
    "a", "an", "and", "any", "are", "as", "at", "be", "but", "by", "can", "do", "does", "done",
    "for", "from", "get", "has", "have", "how", "if", "in", "into", "is", "it", "its", "make",
    "me", "my", "need", "of", "on", "or", "should", "that", "the", "them", "then", "there",
    "these", "this", "to", "want", "was", "we", "what", "when", "which", "who", "why", "will",
    "with", "work", "working", "works", "you", "your",
];

/// Split text into lowercase alphanumeric tokens.
///
/// `_` is kept inside a token so `project_name` stays one term rather than
/// two, since a reader asking about it will type it that way. Everything else
/// non-alphanumeric separates.
pub fn tokenize(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric() && c != '_')
        .filter(|t| !t.is_empty())
        .map(|t| t.to_lowercase())
        .collect()
}

/// Extract the distinct, non-stopword terms a question contributes.
///
/// Order is the question's own, with duplicates dropped — so the returned list
/// can be echoed back to the caller as "this is what I matched on", which is
/// what makes a deterministic ranking auditable rather than merely repeatable.
pub fn terms(question: &str) -> Vec<String> {
    let mut seen: HashSet<String> = HashSet::new();
    let mut out = Vec::new();
    for token in tokenize(question) {
        if token.len() < 2 || STOPWORDS.contains(&token.as_str()) {
            continue;
        }
        if seen.insert(token.clone()) {
            out.push(token);
        }
        if out.len() == MAX_TERMS {
            break;
        }
    }
    out
}

/// Whether `term` matches any token in `tokens`.
///
/// Exact match always counts. A term of at least [`MIN_PREFIX_LEN`] also
/// matches a token it prefixes, so "config" finds "configuration" and "index"
/// finds "indexer" — the common morphological misses — without a stemmer.
/// Deliberately one-directional: a *longer* query term does not match a
/// shorter token, because "configuration" matching "config" would also make
/// every long term match its own fragments.
fn term_hits(term: &str, tokens: &HashSet<&str>) -> bool {
    if tokens.contains(term) {
        return true;
    }
    term.len() >= MIN_PREFIX_LEN && tokens.iter().any(|t| t.starts_with(term))
}

/// Score one candidate span against a question's terms.
///
/// `heading` is the span's heading path (outermost first, its own heading
/// last); `body` is its text. Each term contributes at most once per zone, so
/// the maximum any span can reach is bounded by the question's own term count
/// rather than by its own length.
pub fn score(terms: &[String], heading: &[String], body: &str) -> u32 {
    if terms.is_empty() {
        return 0;
    }
    let (own, ancestors) = match heading.split_last() {
        Some((own, ancestors)) => (std::slice::from_ref(own), ancestors),
        None => (&[] as &[String], &[] as &[String]),
    };
    let own_tokens: Vec<String> = own.iter().flat_map(|h| tokenize(h)).collect();
    let ancestor_tokens: Vec<String> = ancestors.iter().flat_map(|h| tokenize(h)).collect();
    let body_tokens = tokenize(body);

    let own_set: HashSet<&str> = own_tokens.iter().map(|s| s.as_str()).collect();
    let ancestor_set: HashSet<&str> = ancestor_tokens.iter().map(|s| s.as_str()).collect();
    let body_set: HashSet<&str> = body_tokens.iter().map(|s| s.as_str()).collect();

    let mut total = 0u32;
    for term in terms {
        if term_hits(term, &own_set) {
            total += OWN_HEADING_WEIGHT;
        }
        if term_hits(term, &ancestor_set) {
            total += ANCESTOR_HEADING_WEIGHT;
        }
        if term_hits(term, &body_set) {
            total += 1;
        }
    }
    total
}

// ────────────────────────────────────────────────────────────────────────────
// Tests
// ────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokenize_keeps_underscores_and_splits_everything_else() {
        assert_eq!(
            tokenize("Config::project_name — layered(TOML)!"),
            vec!["config", "project_name", "layered", "toml"]
        );
    }

    #[test]
    fn terms_drops_stopwords_and_duplicates_but_keeps_question_order() {
        assert_eq!(
            terms("Where is the config, and how is the config layered?"),
            vec!["where", "config", "layered"]
        );
    }

    #[test]
    fn terms_is_capped() {
        let question: String = (0..100).map(|i| format!("term{i} ")).collect();
        assert_eq!(terms(&question).len(), MAX_TERMS);
    }

    #[test]
    fn a_heading_match_outranks_two_body_matches() {
        let t = terms("config layering");
        let heading = score(&t, &["Config layering".into()], "unrelated prose");
        let body = score(
            &t,
            &["Something else".into()],
            "config and layering in prose",
        );
        assert!(
            heading > body,
            "heading {heading} should beat body {body} — OWN_HEADING_WEIGHT is what makes \
             the author's own section title better evidence than incidental prose"
        );
    }

    #[test]
    fn an_ancestor_heading_counts_for_less_than_the_sections_own() {
        let t = terms("agent bus");
        // The document is called "Agent Bus"; this subsection is not about it.
        let inherited = score(
            &t,
            &[
                "claudectl Agent Bus — Design Specification".into(),
                "Native loop safety".into(),
            ],
            "unrelated prose",
        );
        let own = score(&t, &["Agent Bus".into()], "unrelated prose");
        assert!(
            own > inherited,
            "own {own} must beat inherited {inherited}, or every subsection of a \
             well-titled document ties with the document's own overview"
        );
    }

    #[test]
    fn a_section_with_no_heading_scores_on_its_body_alone() {
        let t = terms("preamble");
        assert_eq!(score(&t, &[], "a preamble before any heading"), 1);
    }

    #[test]
    fn repeating_a_term_does_not_raise_the_score() {
        let t = terms("brain");
        let once = score(&t, &[], "the brain");
        let many = score(&t, &[], &"brain ".repeat(500));
        assert_eq!(
            once, many,
            "distinct terms, not occurrences — this is what replaces a length normalizer"
        );
    }

    #[test]
    fn a_long_span_cannot_outscore_a_short_one_on_volume() {
        let t = terms("terminal backend");
        let short = score(&t, &["Terminal backends".into()], "add one here");
        let long = score(
            &t,
            &["Unrelated".into()],
            &"terminal backend padding words ".repeat(200),
        );
        assert!(short > long, "short {short} should beat long {long}");
    }

    #[test]
    fn prefix_matching_is_one_directional_and_length_gated() {
        let tokens: HashSet<&str> = ["configuration", "cfg"].into_iter().collect();
        assert!(term_hits("config", &tokens), "config → configuration");
        assert!(
            !term_hits("con", &tokens),
            "three characters is not evidence"
        );
        let short: HashSet<&str> = ["config"].into_iter().collect();
        assert!(
            !term_hits("configuration", &short),
            "a longer term must not match a shorter token, or every term matches its fragments"
        );
    }

    #[test]
    fn an_empty_question_scores_nothing() {
        assert_eq!(score(&terms("the and of"), &["Config".into()], "config"), 0);
    }
}
