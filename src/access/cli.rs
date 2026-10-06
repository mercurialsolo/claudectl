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

    /// Show what a grant actually asked for
    Audit {
        /// Grant id, e.g. gr_7f2a1b
        grant_id: String,
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
        AccessCommand::Audit { grant_id } => cmd_audit(grant_id, json_mode),
        AccessCommand::Revoke { grant_id } => cmd_revoke(grant_id, json_mode),
    }
}

/// Parse `--expires`, rejecting what the shared parser lets through.
///
/// `history::parse_duration` accepts `0d` and multiplies without an overflow
/// guard. A zero-length grant is almost certainly a typo, and a grant that
/// wraps past the epoch is worse than an error.
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

    let store = GrantStore::open_default();
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
            Err(e) => last_err = e,
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
    let store = GrantStore::open_default();
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
            truncate(&g.label, 26),
            g.use_count,
            g.last_used_ms.map(fmt_ms).unwrap_or_else(|| "never".into()),
            scopes.join(",")
        );
    }
    Ok(())
}

fn cmd_audit(grant_id: &str, json_mode: bool) -> io::Result<()> {
    if !super::is_valid_grant_id(grant_id) {
        return Err(io::Error::other(format!("invalid grant id: {grant_id}")));
    }
    let store = GrantStore::open_default();
    let entries = store.read_audit(Some(grant_id));

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
        match store.load(grant_id).map_err(io::Error::other)? {
            Some(_) => println!("No audit entries for {grant_id} — issued but never used."),
            None => println!("No such grant: {grant_id}"),
        }
        return Ok(());
    }

    println!("{:<22} {:<8} {:<16} DETAIL", "WHEN", "EVENT", "REASON");
    for e in &entries {
        println!(
            "{:<22} {:<8} {:<16} {}",
            fmt_ms(e.ts_ms),
            e.event,
            e.reason.as_deref().unwrap_or("-"),
            e.detail.as_deref().unwrap_or("")
        );
    }
    Ok(())
}

fn cmd_revoke(grant_id: &str, json_mode: bool) -> io::Result<()> {
    if !super::is_valid_grant_id(grant_id) {
        return Err(io::Error::other(format!("invalid grant id: {grant_id}")));
    }
    let store = GrantStore::open_default();
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

fn state_label(grant: &Grant, now_ms: u64) -> &'static str {
    if grant.revoked {
        "revoked"
    } else if grant.is_expired_at(now_ms) {
        "expired"
    } else {
        "active"
    }
}

fn truncate(s: &str, width: usize) -> String {
    claudectl_core::helpers::truncate_cell(s, width)
}

/// Render an epoch-ms timestamp relative to now — `3d ago`, `in 29d`.
///
/// Core has no epoch-ms formatter and `chrono_now_iso` only renders the
/// current instant, so rather than hand-rolling a calendar this answers the
/// question the operator actually has: is this grant stale, and when does it
/// lapse. Granularity is deliberately coarse.
fn fmt_ms(ms: u64) -> String {
    fmt_ms_at(ms, super::epoch_ms())
}

fn fmt_ms_at(ms: u64, now_ms: u64) -> String {
    let (delta_ms, future) = if ms >= now_ms {
        (ms - now_ms, true)
    } else {
        (now_ms - ms, false)
    };
    let secs = delta_ms / 1000;
    let span = match secs {
        0..=59 => "just now".to_string(),
        60..=3599 => format!("{}m", secs / 60),
        3600..=86_399 => format!("{}h", secs / 3600),
        _ => format!("{}d", secs / 86_400),
    };
    if span == "just now" {
        span
    } else if future {
        format!("in {span}")
    } else {
        format!("{span} ago")
    }
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
}
