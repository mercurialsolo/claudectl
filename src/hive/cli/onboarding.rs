//! Extracted from hive/cli.rs — behavior-preserving split.

use super::*;
use crate::hive::store::HiveStore;
use std::io;

#[allow(clippy::too_many_arguments)]
pub(crate) fn cmd_consent(
    unit_id: &str,
    expires_in: Option<&str>,
    allow_peer: &[String],
    exclude_peer: &[String],
    min_tier: Option<&str>,
    clear: bool,
    json_mode: bool,
) -> io::Result<()> {
    let mut store = HiveStore::load();
    let mut unit = match store.get(unit_id) {
        Some(u) => u.clone(),
        None => {
            eprintln!("Unknown unit: {unit_id}");
            return Err(io::Error::other("unknown unit"));
        }
    };

    if clear {
        unit.sharing_consent = None;
    } else {
        let mut consent = unit.sharing_consent.clone().unwrap_or_default();

        if let Some(expr) = expires_in {
            if expr == "never" {
                consent.expires_at = None;
            } else {
                let secs = parse_duration_secs(expr).ok_or_else(|| {
                    io::Error::other(format!(
                        "invalid duration '{expr}' (use 7d, 12h, 30m, 60s, or 'never')"
                    ))
                })?;
                consent.expires_at = Some(crate::hive::epoch_secs() + secs);
            }
        }

        if !allow_peer.is_empty() {
            let entry = consent
                .allow_peers
                .get_or_insert_with(std::collections::HashSet::new);
            for p in allow_peer {
                entry.insert(p.clone());
            }
        }

        if !exclude_peer.is_empty() {
            let entry = consent
                .exclude_peers
                .get_or_insert_with(std::collections::HashSet::new);
            for p in exclude_peer {
                entry.insert(p.clone());
            }
        }

        if let Some(tier_str) = min_tier {
            consent.min_trust_tier =
                crate::hive::MinTrustTier::from_label(tier_str).ok_or_else(|| {
                    io::Error::other(format!(
                        "invalid tier '{tier_str}' (use confirmed, suggested, unverified, any)"
                    ))
                })?;
        }

        unit.sharing_consent = Some(consent);
    }

    store.insert(unit.clone());
    store
        .save()
        .map_err(|e| io::Error::other(format!("save: {e}")))?;

    if json_mode {
        let payload = serde_json::json!({
            "unit_id": unit.id,
            "sharing_consent": unit.sharing_consent,
        });
        println!("{}", serde_json::to_string_pretty(&payload).unwrap());
        return Ok(());
    }

    match &unit.sharing_consent {
        None => {
            println!("Cleared sharing consent for {unit_id}.");
        }
        Some(c) => {
            println!("Sharing consent for {unit_id}:");
            if let Some(e) = c.expires_at {
                let remaining = e.saturating_sub(crate::hive::epoch_secs());
                println!("  expires_at: {e} ({}s from now)", remaining);
            } else {
                println!("  expires_at: never");
            }
            println!("  min_trust_tier: {}", c.min_trust_tier.label());
            if let Some(allow) = &c.allow_peers {
                let mut v: Vec<&String> = allow.iter().collect();
                v.sort();
                println!("  allow_peers: {v:?}");
            }
            if let Some(ex) = &c.exclude_peers {
                let mut v: Vec<&String> = ex.iter().collect();
                v.sort();
                println!("  exclude_peers: {v:?}");
            }
        }
    }

    Ok(())
}

pub(crate) fn cmd_review(unfreeze: Option<&str>, json_mode: bool) -> io::Result<()> {
    let mut trust = crate::hive::trust::TrustStore::load();

    if let Some(peer) = unfreeze {
        if trust.get(peer).is_none() {
            eprintln!("Unknown peer: {peer}");
            return Err(io::Error::other("unknown peer"));
        }
        trust.unfreeze(peer);
        trust
            .save()
            .map_err(|e| io::Error::other(format!("save: {e}")))?;
        println!("Unfroze peer {peer}.");
        return Ok(());
    }

    let now = crate::hive::epoch_secs();
    let frozen = trust.frozen_peers();
    let quarantined: Vec<&crate::hive::trust::PeerTrust> = trust
        .all()
        .into_iter()
        .filter(|p| !p.frozen && p.quarantine_active(now))
        .collect();

    if json_mode {
        let payload = serde_json::json!({
            "frozen": frozen.iter().map(|p| serde_json::json!({
                "peer_id": p.peer_id,
                "trust_level": p.trust_level,
                "freeze_reason": p.freeze_reason,
                "first_seen": p.first_seen,
                "received_today": p.received_today(now),
                "last_anomaly_at": p.last_anomaly_at,
            })).collect::<Vec<_>>(),
            "quarantined": quarantined.iter().map(|p| serde_json::json!({
                "peer_id": p.peer_id,
                "trust_level": p.trust_level,
                "quarantined_until": p.quarantined_until,
                "first_seen": p.first_seen,
            })).collect::<Vec<_>>(),
        });
        println!("{}", serde_json::to_string_pretty(&payload).unwrap());
        return Ok(());
    }

    if frozen.is_empty() && quarantined.is_empty() {
        println!("No peers in review (no quarantines, no freezes).");
        return Ok(());
    }

    if !frozen.is_empty() {
        println!("Frozen peers ({}):", frozen.len());
        for p in &frozen {
            let reason = p.freeze_reason.as_deref().unwrap_or("(no reason recorded)");
            println!("  • {} — {reason}", p.peer_id);
        }
        println!("  Clear with: claudectl hive review --unfreeze <peer_id>");
        println!();
    }

    if !quarantined.is_empty() {
        println!("Quarantined peers ({}):", quarantined.len());
        for p in &quarantined {
            let remaining_days = p.quarantined_until.saturating_sub(now).div_ceil(86_400);
            println!(
                "  • {} — {remaining_days}d remaining (trust {:.2})",
                p.peer_id, p.trust_level
            );
        }
    }

    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn cmd_explore(
    category: Option<&str>,
    scope: Option<&str>,
    peer: Option<&str>,
    min_confidence: f64,
    include_artifacts: bool,
    limit: usize,
    json_mode: bool,
) -> io::Result<()> {
    let store = HiveStore::load();
    let trust = crate::hive::trust::TrustStore::load();
    let parsed_scope = scope.map(parse_scope);
    let filter = crate::hive::discovery::ExploreFilter {
        category,
        scope: parsed_scope.as_ref(),
        peer,
        min_confidence,
        include_artifacts,
    };
    let rows = crate::hive::discovery::explore(&store, &trust, &filter, limit);

    if json_mode {
        let arr: Vec<serde_json::Value> = rows
            .iter()
            .map(|r| {
                serde_json::json!({
                    "id": r.unit.id,
                    "scope": r.unit.scope.to_string(),
                    "category": r.unit.category.label(),
                    "peer": r.unit.source_peer,
                    "tier": r.tier.label(),
                    "effective_confidence": r.effective_confidence,
                    "evidence": r.unit.evidence_count,
                    "rollout": r.unit.injection_state.label(),
                    "summary": r.unit.content.summary_line(),
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&arr).unwrap());
        return Ok(());
    }

    if rows.is_empty() {
        println!("No knowledge units match the filter.");
        return Ok(());
    }

    println!(
        "{:<12} {:<20} {:<6} {:<20} CONTENT",
        "ID", "TIER", "CONF", "PEER"
    );
    println!("{}", "─".repeat(96));
    for r in &rows {
        let id_short = if r.unit.id.len() > 11 {
            &r.unit.id[..11]
        } else {
            &r.unit.id
        };
        let peer_short = if r.unit.source_peer.len() > 19 {
            &r.unit.source_peer[..19]
        } else {
            &r.unit.source_peer
        };
        println!(
            "{:<12} {:<20} {:<6.0}% {:<20} {}",
            id_short,
            r.tier.label(),
            r.effective_confidence * 100.0,
            peer_short,
            r.unit.content.summary_line(),
        );
    }
    println!();
    println!("{} units shown", rows.len());
    Ok(())
}

pub(crate) fn cmd_experts(category: &str, limit: usize, json_mode: bool) -> io::Result<()> {
    let store = HiveStore::load();
    let trust = crate::hive::trust::TrustStore::load();
    let rows = crate::hive::discovery::experts(&store, &trust, category, limit);

    if json_mode {
        let arr: Vec<serde_json::Value> = rows
            .iter()
            .map(|r| {
                serde_json::json!({
                    "peer_id": r.peer_id,
                    "category": r.category,
                    "tier": r.tier.label(),
                    "unit_count": r.unit_count,
                    "avg_confidence": r.avg_confidence,
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&arr).unwrap());
        return Ok(());
    }

    if rows.is_empty() {
        println!("No peers contribute to category '{category}'.");
        return Ok(());
    }

    println!(
        "{:<24} {:<20} {:<6} {:<6}",
        "PEER", "TIER", "UNITS", "AVG CONF"
    );
    println!("{}", "─".repeat(72));
    for r in &rows {
        let peer_short = if r.peer_id.len() > 23 {
            &r.peer_id[..23]
        } else {
            &r.peer_id
        };
        println!(
            "{:<24} {:<20} {:<6} {:<6.0}%",
            peer_short,
            r.tier.label(),
            r.unit_count,
            r.avg_confidence * 100.0,
        );
    }
    println!();
    println!("{} peers in category '{category}'", rows.len());
    Ok(())
}

pub(crate) fn cmd_welcome(json_mode: bool) -> io::Result<()> {
    let store = HiveStore::load();
    let trust = crate::hive::trust::TrustStore::load();
    let units = crate::hive::discovery::welcome_snapshot(&store, &trust);

    if json_mode {
        let arr: Vec<serde_json::Value> = units
            .iter()
            .map(|u| {
                serde_json::json!({
                    "id": u.id,
                    "scope": u.scope.to_string(),
                    "category": u.category.label(),
                    "peer": u.source_peer,
                    "confidence": u.confidence,
                    "summary": u.content.summary_line(),
                })
            })
            .collect();
        let payload = serde_json::json!({
            "size": units.len(),
            "max_units": crate::hive::discovery::WELCOME_MAX_UNITS,
            "min_confidence": crate::hive::discovery::WELCOME_MIN_CONFIDENCE,
            "units": arr,
        });
        println!("{}", serde_json::to_string_pretty(&payload).unwrap());
        return Ok(());
    }

    println!(
        "Welcome snapshot ({} units, max {}, min confidence {:.0}%):",
        units.len(),
        crate::hive::discovery::WELCOME_MAX_UNITS,
        crate::hive::discovery::WELCOME_MIN_CONFIDENCE * 100.0
    );
    if units.is_empty() {
        println!();
        println!("(empty — no Live units from Suggested+ peers above the confidence floor)");
        return Ok(());
    }
    println!("{}", "─".repeat(96));
    println!("{:<12} {:<6} {:<20} CONTENT", "ID", "CONF", "PEER");
    for u in &units {
        let id_short = if u.id.len() > 11 { &u.id[..11] } else { &u.id };
        let peer_short = if u.source_peer.len() > 19 {
            &u.source_peer[..19]
        } else {
            &u.source_peer
        };
        println!(
            "{:<12} {:<6.0}% {:<20} {}",
            id_short,
            u.confidence * 100.0,
            peer_short,
            u.content.summary_line(),
        );
    }
    Ok(())
}

pub(crate) fn parse_duration_secs(s: &str) -> Option<u64> {
    if s == "never" {
        return Some(0);
    }
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return None;
    }
    let (num_str, unit) = trimmed.split_at(trimmed.len() - 1);
    let n: u64 = num_str.parse().ok()?;
    let multiplier = match unit {
        "s" => 1,
        "m" => 60,
        "h" => 3_600,
        "d" => 86_400,
        _ => return None,
    };
    Some(n.saturating_mul(multiplier))
}
