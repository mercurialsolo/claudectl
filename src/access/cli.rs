//! `claudectl access` — issue, inspect and revoke capability grants
//! (#427, RFC §5).

use std::io;

use clap::Subcommand;

use super::token;
use super::{Grant, GrantStore, Scope, new_grant};

/// Default grant lifetime when `--expires` is omitted.
const DEFAULT_EXPIRES: &str = "30d";

/// How many tries before we give up finding an unused grant id.
const ID_MINT_ATTEMPTS: usize = 8;

/// Deciding one escalation (#446).
///
/// Approve and deny are the same write with a different state, so they share a
/// shape. Neither needs the project's index: the verdict is a one-line record,
/// and `query serve` retrieves the spans when the caller next polls. That is
/// what lets the owner decide from any directory.
#[derive(Debug, Subcommand)]
pub enum EscalationAction {
    /// Let the caller have an answer to this question
    Approve {
        /// Escalation id, e.g. esc_7f2a1b9c4d3e
        escalation_id: String,
        /// Your own words, shown to the caller alongside the spans
        #[arg(long)]
        note: Option<String>,
    },
    /// Refuse this question
    Deny {
        /// Escalation id, e.g. esc_7f2a1b9c4d3e
        escalation_id: String,
        /// Your own words, shown to the caller
        #[arg(long)]
        note: Option<String>,
    },
}

#[derive(Debug, Subcommand)]
pub enum AccessCommand {
    /// Issue a grant and print its token once
    Grant {
        /// Project the grant is scoped to
        #[arg(long)]
        project: String,
        /// Human label, so `access list` is readable later
        #[arg(long)]
        label: String,
        /// Comma-separated scopes, e.g. `project.query,project.docs`
        #[arg(long, default_value = "project.query")]
        scopes: String,
        /// Lifetime: 30d, 24h, 90m, 1w
        #[arg(long, default_value = DEFAULT_EXPIRES)]
        expires: String,
    },

    /// List grants with last-used and use counts
    List,

    /// Show what a grant actually asked for, or every entry when no id is given
    Audit {
        /// Grant id, e.g. gr_7f2a1b. Omit to show the whole log, including
        /// denials recorded against unparseable tokens.
        grant_id: Option<String>,
    },

    /// Show queries classification escalated for your review, or decide one
    ///
    /// §4.3's "anything else" row: a query that was neither answerable with
    /// confidence nor clearly refusable is queued instead of guessed at. The
    /// caller got `pending_review` and an id, and polls it for your verdict.
    Escalations {
        #[command(subcommand)]
        action: Option<EscalationAction>,
    },

    /// Revoke a grant immediately
    Revoke {
        /// Grant id, e.g. gr_7f2a1b
        grant_id: String,
    },
}

pub fn dispatch_command(command: &AccessCommand, json_mode: bool) -> io::Result<()> {
    match command {
        AccessCommand::Grant {
            project,
            label,
            scopes,
            expires,
        } => cmd_grant(project, label, scopes, expires, json_mode),
        AccessCommand::List => cmd_list(json_mode),
        AccessCommand::Audit { grant_id } => cmd_audit(grant_id.as_deref(), json_mode),
        AccessCommand::Escalations { action } => match action {
            None => cmd_escalations(json_mode),
            Some(EscalationAction::Approve {
                escalation_id,
                note,
            }) => cmd_decide(
                escalation_id,
                crate::query::escalate::VerdictState::Approved,
                note.as_deref(),
                json_mode,
            ),
            Some(EscalationAction::Deny {
                escalation_id,
                note,
            }) => cmd_decide(
                escalation_id,
                crate::query::escalate::VerdictState::Denied,
                note.as_deref(),
                json_mode,
            ),
        },
        AccessCommand::Revoke { grant_id } => cmd_revoke(grant_id, json_mode),
    }
}

/// Parse `--expires`, rejecting what the shared parser lets through.
///
/// `history::parse_duration` accepts `0d`, and a zero-length grant is almost
/// certainly a typo rather than a request. (It used to overflow on absurd
/// values too; that is now `checked_mul` in core.)
fn parse_expires(expr: &str) -> Result<u64, String> {
    let secs = crate::history::parse_duration(expr).ok_or_else(|| {
        format!("invalid --expires '{expr}' (expected a value like 30d, 24h, 90m or 1w)")
    })?;
    if secs == 0 {
        return Err(format!(
            "--expires '{expr}' is zero — the grant would be expired on arrival"
        ));
    }
    secs.checked_mul(1000)
        .ok_or_else(|| format!("--expires '{expr}' is implausibly far in the future"))
}

fn parse_scopes(raw: &str, project: &str) -> Result<Vec<Scope>, String> {
    let mut out: Vec<Scope> = Vec::new();
    for piece in raw.split(',') {
        let piece = piece.trim();
        if piece.is_empty() {
            continue;
        }
        let scope = Scope::parse_with_default(piece, Some(project))?;
        // `--project` reads as the bound on the grant, and it is the only value
        // `cmd_grant` validates up front — so a qualifier written into
        // `--scopes` must not quietly win over it.
        if scope.qualifier() != project {
            return Err(format!(
                "scope '{piece}' is scoped to '{}' but --project is '{project}' \
                 — drop the qualifier, or pass the project you mean",
                scope.qualifier()
            ));
        }
        if let Some(reason) = scope.unissuable_reason() {
            return Err(format!("cannot issue '{scope}': {reason}"));
        }
        if !out.contains(&scope) {
            out.push(scope);
        }
    }
    if out.is_empty() {
        return Err("no scopes given — pass --scopes project.query".into());
    }
    Ok(out)
}

fn cmd_grant(
    project: &str,
    label: &str,
    scopes_raw: &str,
    expires_raw: &str,
    json_mode: bool,
) -> io::Result<()> {
    super::scope::validate_qualifier(project)
        .map_err(|e| io::Error::other(format!("invalid --project: {e}")))?;
    if label.trim().is_empty() {
        return Err(io::Error::other("--label must not be empty"));
    }

    let scopes = parse_scopes(scopes_raw, project).map_err(io::Error::other)?;
    let ttl_ms = parse_expires(expires_raw).map_err(io::Error::other)?;

    let store = GrantStore::open_default().map_err(io::Error::other)?;
    let secret = token::load_or_create_secret(store.root()).map_err(io::Error::other)?;

    let issued_ms = super::epoch_ms();
    let expires_ms = issued_ms
        .checked_add(ttl_ms)
        .ok_or_else(|| io::Error::other("expiry overflows"))?;

    // 24 bits of id, so retry on the (unlikely) collision rather than
    // overwriting a live grant.
    let mut last_err = String::new();
    for _ in 0..ID_MINT_ATTEMPTS {
        let grant = new_grant(
            super::gen_grant_id(),
            label.to_string(),
            scopes.clone(),
            issued_ms,
            expires_ms,
        );
        match store.create(&grant) {
            Ok(()) => {
                let tok = token::mint(&secret, &grant.grant_id, &grant.scopes, grant.expires_ms);
                print_new_grant(&grant, &tok, json_mode);
                return Ok(());
            }
            // Only an id collision is worth another draw. Retrying a full disk
            // or an unwritable home eight times and then blaming the id would
            // point the operator at the wrong thing entirely.
            Err(e) if e.contains("already exists") => last_err = e,
            Err(e) => return Err(io::Error::other(e)),
        }
    }
    Err(io::Error::other(format!(
        "could not mint an unused grant id after {ID_MINT_ATTEMPTS} attempts: {last_err}"
    )))
}

fn print_new_grant(grant: &Grant, tok: &str, json_mode: bool) {
    if json_mode {
        let scopes: Vec<String> = grant.scopes.iter().map(|s| s.to_string()).collect();
        let json = serde_json::json!({
            "grant_id": grant.grant_id,
            "label": grant.label,
            "scopes": scopes,
            "issued_ms": grant.issued_ms,
            "expires_ms": grant.expires_ms,
            "token": tok,
        });
        println!(
            "{}",
            serde_json::to_string_pretty(&json).unwrap_or_default()
        );
        return;
    }

    println!("Grant {} issued for \"{}\"", grant.grant_id, grant.label);
    println!();
    println!("TOKEN: {tok}");
    println!();
    println!("This is the only time the token is shown. Store it now.");
    println!();
    println!("Scopes:");
    for s in &grant.scopes {
        println!("  {s}");
    }
    println!("Expires: {}", fmt_ms(grant.expires_ms));
    println!();
    println!(
        "Revoke any time with: claudectl access revoke {}",
        grant.grant_id
    );
}

fn cmd_list(json_mode: bool) -> io::Result<()> {
    let store = GrantStore::open_default().map_err(io::Error::other)?;
    let grants = store.list();
    let now = super::epoch_ms();

    if json_mode {
        let rows: Vec<serde_json::Value> = grants
            .iter()
            .map(|g| {
                serde_json::json!({
                    "grant_id": g.grant_id,
                    "label": g.label,
                    "scopes": g.scopes.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
                    "state": state_label(g, now),
                    "issued_ms": g.issued_ms,
                    "expires_ms": g.expires_ms,
                    "last_used_ms": g.last_used_ms,
                    "use_count": g.use_count,
                    // Separate fields rather than folded into `state`, which
                    // is one cell: a flagged grant is still active, and a
                    // script should be able to see both facts.
                    "flagged_ms": g.flagged_ms,
                    "flag_reason": g.flag_reason,
                })
            })
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&rows).unwrap_or_default()
        );
        return Ok(());
    }

    if grants.is_empty() {
        println!("No grants issued.");
        println!("Issue one with: claudectl access grant --project <p> --label <l>");
        return Ok(());
    }

    println!(
        "{:<11} {:<9} {:<26} {:>6}  {:<12} SCOPES",
        "GRANT", "STATE", "LABEL", "USES", "LAST USED"
    );
    for g in &grants {
        let scopes: Vec<String> = g.scopes.iter().map(|s| s.to_string()).collect();
        println!(
            "{:<11} {:<9} {:<26} {:>6}  {:<12} {}",
            g.grant_id,
            state_label(g, now),
            truncate(&visible(&g.label), 26),
            g.use_count,
            g.last_used_ms.map(fmt_ms).unwrap_or_else(|| "never".into()),
            scopes.join(",")
        );
    }
    Ok(())
}

fn cmd_audit(grant_id: Option<&str>, json_mode: bool) -> io::Result<()> {
    // A denial against a token too malformed to parse is recorded under a
    // sentinel id, which `is_valid_grant_id` rejects — so without the no-arg
    // form those entries would be written and then unreadable from the CLI,
    // which is exactly the garbage-token probing the log exists to surface.
    if let Some(id) = grant_id
        && !super::is_valid_grant_id(id)
    {
        return Err(io::Error::other(format!("invalid grant id: {id}")));
    }
    let store = GrantStore::open_default().map_err(io::Error::other)?;
    let entries = store.read_audit(grant_id);

    if json_mode {
        println!(
            "{}",
            serde_json::to_string_pretty(&entries).unwrap_or_default()
        );
        return Ok(());
    }

    // A grant with no entries and no record is worth distinguishing from one
    // that exists but was never used.
    if entries.is_empty() {
        match grant_id {
            None => println!("No audit entries yet."),
            Some(id) => match store.load(id).map_err(io::Error::other)? {
                Some(_) => println!("No audit entries for {id} — issued but never used."),
                None => println!("No such grant: {id}"),
            },
        }
        return Ok(());
    }

    // QUESTION is the column #431 exists for: caps stop the abuse you
    // anticipated, and this is how the abuse nobody anticipated becomes
    // visible. `cited` is deliberately not a column — it is a list, it belongs
    // in `--json`, and the table has to stay readable at 120 columns.
    // EVENT is 10 wide because #430 added `escalated`, which is 9 characters
    // and would otherwise shunt every later column one place right.
    println!(
        "{:<12} {:<14} {:<10} {:<17} {:<18} QUESTION",
        "GRANT", "WHEN", "EVENT", "REASON", "DETAIL"
    );
    for e in &entries {
        println!(
            "{:<12} {:<14} {:<10} {:<17} {:<18} {}",
            truncate(&e.grant_id, 12),
            fmt_ms(e.ts_ms),
            e.event,
            e.reason.as_deref().unwrap_or("-"),
            // `detail` is operator-generated today, but it is a free-form
            // field #429 onward appends to, so it gets the same treatment.
            truncate(&visible(e.detail.as_deref().unwrap_or("-")), 18),
            truncate(&visible(e.question.as_deref().unwrap_or("")), 44),
        );
        // Indented under its own row rather than a column: five numbers do not
        // fit a table, and an owner tuning thresholds is reading them one line
        // at a time anyway. Absent for every request on an unclassified
        // surface, so #431's output is unchanged there.
        if let Some(c) = &e.classification {
            println!("{:<12} {c}", "");
        }
    }
    Ok(())
}

/// Render the escalation queue with each row's state (#430, #446, RFC §4.3).
///
/// The STATE column is what makes the queue drainable: a row is pending,
/// decided, or expired, and `approve`/`deny` act only on the first.
fn cmd_escalations(json_mode: bool) -> io::Result<()> {
    use crate::query::thresholds::ESCALATION_TTL_MS;

    let root = super::access_dir().map_err(io::Error::other)?;
    let queue = crate::query::escalate::EscalationQueue::in_access_dir(&root);
    let rows = queue.read();
    let now = super::epoch_ms();

    if json_mode {
        // State folded in rather than left to the caller: it is derived from a
        // verdict file plus the clock, and a consumer of `--json` cannot
        // compute it from the row alone.
        let enriched: Vec<_> = rows
            .iter()
            .map(|e| {
                let state = queue.state(e, now, ESCALATION_TTL_MS);
                serde_json::json!({
                    "id": e.id,
                    "ts_ms": e.ts_ms,
                    "grant_id": e.grant_id,
                    "project": e.project,
                    "question": e.question,
                    "classification": e.classification,
                    "fingerprint": e.fingerprint,
                    "state": state.as_str(),
                    "note": match &state {
                        crate::query::escalate::EscalationState::Approved { note, .. }
                        | crate::query::escalate::EscalationState::Denied { note, .. } => note.clone(),
                        _ => None,
                    },
                })
            })
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&enriched).unwrap_or_default()
        );
        return Ok(());
    }

    if rows.is_empty() {
        println!("No escalations.");
        println!("Queue: {}", queue.path().display());
        return Ok(());
    }

    println!(
        "{:<17} {:<12} {:<10} {:<14} QUESTION",
        "ESCALATION", "GRANT", "WHEN", "STATE"
    );
    let mut pending = 0usize;
    for e in &rows {
        let state = queue.state(e, now, ESCALATION_TTL_MS);
        // `is_terminal` already excludes expired — only `Pending` is false.
        if !state.is_terminal() {
            pending += 1;
        }
        println!(
            "{:<17} {:<12} {:<10} {:<14} {}",
            e.id,
            truncate(&e.grant_id, 12),
            fmt_ms_at(e.ts_ms, now),
            state.as_str(),
            truncate(&e.question, 46)
        );
        // The five numbers are why it is here, so they are not hidden behind
        // `--json`.
        println!("                  {}", e.classification.audit_summary());
    }
    if pending > 0 {
        println!();
        println!(
            "{pending} awaiting you. Decide with: claudectl access escalations approve <id>  (or deny)"
        );
    }
    Ok(())
}

/// Record the owner's decision on one escalation (#446).
///
/// Refuses a row that is already decided, or already expired. Both refusals are
/// loud here — unlike the query surface's opaque `404`, this caller *is* the
/// owner, so naming the reason costs nothing and silently re-deciding would be
/// the worse outcome.
fn cmd_decide(
    escalation_id: &str,
    state: crate::query::escalate::VerdictState,
    note: Option<&str>,
    json_mode: bool,
) -> io::Result<()> {
    use crate::query::escalate::EscalationQueue;
    use crate::query::thresholds::ESCALATION_TTL_MS;

    let root = super::access_dir().map_err(io::Error::other)?;
    let queue = EscalationQueue::in_access_dir(&root);

    let entry = queue
        .find(escalation_id)
        .ok_or_else(|| io::Error::other(format!("no such escalation: {escalation_id}")))?;

    // `decide` re-checks the state itself and refuses an already-decided or
    // expired row, so there is no check to duplicate here.
    let now = super::epoch_ms();
    let verdict = queue
        .decide(
            &entry,
            state,
            note.map(str::to_string),
            now,
            ESCALATION_TTL_MS,
        )
        .map_err(io::Error::other)?;

    if json_mode {
        let json = serde_json::json!({
            "escalation_id": entry.id,
            "state": verdict.state.as_str(),
            "grant_id": entry.grant_id,
            "note": verdict.note,
        });
        println!(
            "{}",
            serde_json::to_string_pretty(&json).unwrap_or_default()
        );
        return Ok(());
    }

    println!("{} {}.", verdict.state.as_str(), entry.id);
    println!("Question: {}", truncate(&entry.question, 70));
    if let Some(n) = &verdict.note {
        println!("Note: {n}");
    }
    match state {
        crate::query::escalate::VerdictState::Approved => println!(
            "{} sees spans the next time it polls — retrieved from the index as it is then, \
             not as it was when the question was asked.",
            entry.grant_id
        ),
        crate::query::escalate::VerdictState::Denied => println!(
            "{} sees the refusal the next time it polls.",
            entry.grant_id
        ),
    }
    Ok(())
}

fn cmd_revoke(grant_id: &str, json_mode: bool) -> io::Result<()> {
    if !super::is_valid_grant_id(grant_id) {
        return Err(io::Error::other(format!("invalid grant id: {grant_id}")));
    }
    let store = GrantStore::open_default().map_err(io::Error::other)?;
    let grant = store.revoke(grant_id).map_err(io::Error::other)?;

    if json_mode {
        let json = serde_json::json!({
            "grant_id": grant.grant_id,
            "revoked": true,
        });
        println!(
            "{}",
            serde_json::to_string_pretty(&json).unwrap_or_default()
        );
    } else {
        println!("Revoked {} (\"{}\").", grant.grant_id, grant.label);
        println!("Its token stops verifying immediately — nothing to restart.");
    }
    Ok(())
}

/// One cell, so the states are ordered by what the owner most needs to see.
///
/// `revoked` and `expired` come first because they say the grant cannot be
/// used. `flagged` comes next: a flagged grant *is* still active, which is
/// exactly why the flag has to win the cell — "active" would hide the one
/// thing asking for attention. `--json` carries `flagged_ms` and
/// `flag_reason` as their own fields, so nothing is lost.
fn state_label(grant: &Grant, now_ms: u64) -> &'static str {
    if grant.revoked {
        "revoked"
    } else if grant.is_expired_at(now_ms) {
        "expired"
    } else if grant.flagged_ms.is_some() {
        "flagged"
    } else {
        "active"
    }
}

fn truncate(s: &str, width: usize) -> String {
    claudectl_core::helpers::truncate_cell(s, width)
}

/// Replace every control character with a visible marker.
///
/// `question` in an audit line is **third-party text**. `truncate_for_audit`
/// bounds its bytes and strips nothing, and `truncate_cell` trims by character
/// count, so without this a question containing a newline forges a plausible
/// extra row in `access audit` — and an ANSI escape can clear the operator's
/// screen or rewrite their terminal title. The 44-character ceiling is no
/// defence: a newline inside the first 44 characters survives it.
///
/// Sanitised at **render**, never on the way into `audit.jsonl`. The log's
/// whole purpose is to record what was actually asked, and scrubbing on write
/// would destroy the evidence it exists to keep. `--json` needs no help here:
/// `serde_json` escapes control characters itself.
///
/// Replaced rather than deleted, because an owner should be able to see that
/// *something* was there. `is_control` covers C0, DEL and the C1 range, which
/// is where every escape-sequence introducer lives.
fn visible(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_control() { '\u{fffd}' } else { c })
        .collect()
}

/// Render an epoch-ms timestamp relative to now — `3d ago`, `in 30d`.
///
/// Core has no epoch-ms formatter and `chrono_now_iso` only renders the
/// current instant, so rather than hand-rolling a calendar this answers the
/// question the operator actually has: is this grant stale, and when does it
/// lapse. Granularity is deliberately coarse, and rounds to nearest rather
/// than truncating — a grant issued with `--expires 30d` should not read back
/// as `in 29d` a millisecond later.
fn fmt_ms(ms: u64) -> String {
    fmt_ms_at(ms, super::epoch_ms())
}

fn fmt_ms_at(ms: u64, now_ms: u64) -> String {
    // Moved to core so `hive::identity` can share it: that module is not
    // behind the `relay` feature this one is, and the sync-only build has a
    // hive but no grants.
    claudectl_core::helpers::fmt_ms_at(ms, now_ms)
}

// ────────────────────────────────────────────────────────────────────────────
// Tests
// ────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expires_accepts_the_documented_forms() {
        assert_eq!(parse_expires("30d").unwrap(), 30 * 86_400 * 1000);
        assert_eq!(parse_expires("24h").unwrap(), 24 * 3_600 * 1000);
        assert_eq!(parse_expires("90m").unwrap(), 90 * 60 * 1000);
        assert_eq!(parse_expires("1w").unwrap(), 7 * 86_400 * 1000);
    }

    #[test]
    fn expires_rejects_zero_and_garbage() {
        // A zero-length grant is a typo, not a request.
        assert!(parse_expires("0d").is_err());
        assert!(parse_expires("0h").is_err());
        // The shared parser takes only a single-char suffix, so these are junk.
        assert!(parse_expires("30dd").is_err());
        assert!(parse_expires("30").is_err());
        assert!(parse_expires("").is_err());
        assert!(parse_expires("tomorrow").is_err());
        assert!(parse_expires("-5d").is_err());
    }

    #[test]
    fn expires_rejects_an_overflowing_value() {
        assert!(parse_expires(&format!("{}w", u64::MAX)).is_err());
    }

    #[test]
    fn scopes_take_the_project_as_their_qualifier() {
        let got = parse_scopes("project.query,project.docs", "claudectl").unwrap();
        assert_eq!(
            got.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
            vec!["project.query:claudectl", "project.docs:claudectl"]
        );
    }

    #[test]
    fn scopes_tolerate_spacing_and_duplicates() {
        let got = parse_scopes(" project.query , project.query ,", "p").unwrap();
        assert_eq!(got.len(), 1, "duplicates should collapse");
    }

    #[test]
    fn scopes_reject_an_empty_list() {
        assert!(parse_scopes("", "p").is_err());
        assert!(parse_scopes(" , ", "p").is_err());
    }

    #[test]
    fn scopes_refuse_what_this_phase_cannot_issue() {
        // Q8: defined, issued to nobody.
        let err = parse_scopes("fleet.read", "p").unwrap_err();
        assert!(err.contains("Q8"), "got {err}");
        // Needs named hives (#424).
        let err = parse_scopes("hive.read", "h").unwrap_err();
        assert!(err.contains("#424"), "got {err}");
    }

    #[test]
    fn scopes_refuse_a_write_verb() {
        assert!(parse_scopes("project.write", "p").is_err());
    }

    #[test]
    fn a_thirty_day_grant_reads_back_as_thirty_days() {
        // Truncating would say "in 29d" a millisecond after issuing, which
        // reads like the grant was mis-issued.
        let now = 1_000_000_000_000;
        let thirty_days = 30 * 86_400 * 1000;
        assert_eq!(fmt_ms_at(now + thirty_days - 5, now), "in 30d");
    }

    #[test]
    fn relative_times_pick_a_sensible_unit_and_direction() {
        let now = 1_000_000_000_000;
        assert_eq!(fmt_ms_at(now, now), "just now");
        assert_eq!(fmt_ms_at(now - 30_000, now), "just now");
        assert_eq!(fmt_ms_at(now - 600_000, now), "10m ago");
        assert_eq!(fmt_ms_at(now - 7_200_000, now), "2h ago");
        assert_eq!(fmt_ms_at(now - 3 * 86_400_000, now), "3d ago");
        assert_eq!(fmt_ms_at(now + 600_000, now), "in 10m");
    }

    #[test]
    fn a_question_cannot_forge_a_row_or_drive_the_terminal() {
        // `question` is third-party text and goes straight to stdout. A
        // newline inside the first 44 characters survives the truncation and
        // forges a plausible extra audit row; an ANSI escape can clear the
        // screen or rewrite the terminal title.
        let forged = "probe\ngr_other     1m ago     allowed  -       query.ask   benign";
        let out = visible(forged);
        assert!(!out.contains('\n'), "{out}");
        assert!(out.starts_with("probe\u{fffd}gr_other"), "{out}");

        let escape = "\u{1b}[2Jcleared\u{7}";
        let out = visible(escape);
        assert!(!out.contains('\u{1b}'), "{out}");
        assert!(!out.contains('\u{7}'), "{out}");
        // Replaced, not deleted: the owner should see that something was
        // there rather than read a sentence that silently lost characters.
        assert!(out.contains("cleared"), "{out}");
        assert!(out.starts_with('\u{fffd}'), "{out}");

        // Ordinary text, including multi-byte, passes through untouched.
        assert_eq!(visible("héllo — ok"), "héllo — ok");
    }

    #[test]
    fn state_label_prefers_revoked_over_expired() {
        let mut g = new_grant(
            "gr_1aaaaa".into(),
            "l".into(),
            vec!["project.query:p".parse().unwrap()],
            0,
            1000,
        );
        assert_eq!(state_label(&g, 0), "active");
        assert_eq!(state_label(&g, 2000), "expired");
        g.revoked = true;
        assert_eq!(state_label(&g, 0), "revoked");
        // A grant that is both should read as revoked — that was the deliberate act.
        assert_eq!(state_label(&g, 2000), "revoked");
    }

    #[test]
    fn a_flagged_grant_reads_as_flagged_but_revoking_still_wins() {
        let mut g = new_grant(
            "gr_2aaaaa".into(),
            "l".into(),
            vec!["project.query:p".parse().unwrap()],
            0,
            1000,
        );
        g.flagged_ms = Some(500);
        // Still usable, which is the point — flagging does not revoke.
        assert_eq!(state_label(&g, 0), "flagged");
        // But a state that says the grant cannot be used outranks it.
        assert_eq!(state_label(&g, 2000), "expired");
        g.revoked = true;
        assert_eq!(state_label(&g, 0), "revoked");
    }
}
