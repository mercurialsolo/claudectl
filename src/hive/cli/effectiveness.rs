//! Extracted from hive/cli.rs — behavior-preserving split.

use super::*;
use crate::hive::store::HiveStore;
use std::io;

pub(crate) fn cmd_effectiveness(
    peer: Option<&str>,
    category: Option<&str>,
    state: Option<&str>,
    min_decided: u64,
    limit: usize,
    json_mode: bool,
) -> io::Result<()> {
    let store = HiveStore::load();
    let parsed_state = state.and_then(parse_injection_state);
    if let Some(s) = state {
        if parsed_state.is_none() {
            eprintln!("Unknown state '{s}'. Expected one of: draft, canary, staged, live.");
            return Err(io::Error::other("invalid state"));
        }
    }
    let filter = crate::hive::effectiveness::EffectivenessFilter {
        peer,
        category,
        state: parsed_state,
        min_decided,
    };
    let mut rows = crate::hive::effectiveness::unit_effectiveness(&store, &filter);
    if limit > 0 && rows.len() > limit {
        rows.truncate(limit);
    }

    if json_mode {
        let arr: Vec<serde_json::Value> = rows
            .iter()
            .map(|r| {
                serde_json::json!({
                    "id": r.unit.id,
                    "peer": r.unit.source_peer,
                    "scope": r.unit.scope.to_string(),
                    "category": r.unit.category.label(),
                    "state": r.unit.injection_state.label(),
                    "injected": r.injected,
                    "accepted": r.accepted,
                    "overridden": r.overridden,
                    "decided": r.decided,
                    "win_rate": r.win_rate,
                    "summary": r.unit.content.summary_line(),
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&arr).unwrap());
        return Ok(());
    }

    if rows.is_empty() {
        println!("No units match the filter (or no decided outcomes yet).");
        return Ok(());
    }

    println!(
        "{:<12} {:<8} {:<8} {:<6} {:<6} {:<6} {:<20} CONTENT",
        "ID", "ROLLOUT", "WIN", "INJ", "ACC", "OVR", "PEER"
    );
    println!("{}", "─".repeat(96));
    for r in &rows {
        let id_short = if r.unit.id.len() > 11 {
            &r.unit.id[..11]
        } else {
            &r.unit.id
        };
        let win = if r.decided > 0 {
            format!("{:.0}%", r.win_rate * 100.0)
        } else {
            "—".to_string()
        };
        let peer_short = if r.unit.source_peer.len() > 19 {
            &r.unit.source_peer[..19]
        } else {
            &r.unit.source_peer
        };
        println!(
            "{:<12} {:<8} {:<8} {:<6} {:<6} {:<6} {:<20} {}",
            id_short,
            r.unit.injection_state.label(),
            win,
            r.injected,
            r.accepted,
            r.overridden,
            peer_short,
            r.unit.content.summary_line(),
        );
    }
    println!();
    println!("{} rows", rows.len());
    Ok(())
}

pub(crate) fn cmd_dead_weight(
    min_injected: u64,
    max_decided: u64,
    json_mode: bool,
) -> io::Result<()> {
    let store = HiveStore::load();
    let rows = crate::hive::effectiveness::dead_weight(&store, min_injected, max_decided);

    if json_mode {
        let arr: Vec<serde_json::Value> = rows
            .iter()
            .map(|u| {
                serde_json::json!({
                    "id": u.id,
                    "peer": u.source_peer,
                    "state": u.injection_state.label(),
                    "injected": u.injection_stats.injected_count,
                    "decided": u.injection_stats.decided(),
                    "summary": u.content.summary_line(),
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&arr).unwrap());
        return Ok(());
    }

    if rows.is_empty() {
        println!(
            "No dead-weight units (≥{min_injected} injections, ≤{max_decided} decided outcomes)."
        );
        return Ok(());
    }

    println!(
        "{:<12} {:<8} {:<6} {:<6} {:<20} CONTENT",
        "ID", "ROLLOUT", "INJ", "DEC", "PEER"
    );
    println!("{}", "─".repeat(96));
    for u in &rows {
        let id_short = if u.id.len() > 11 { &u.id[..11] } else { &u.id };
        let peer_short = if u.source_peer.len() > 19 {
            &u.source_peer[..19]
        } else {
            &u.source_peer
        };
        println!(
            "{:<12} {:<8} {:<6} {:<6} {:<20} {}",
            id_short,
            u.injection_state.label(),
            u.injection_stats.injected_count,
            u.injection_stats.decided(),
            peer_short,
            u.content.summary_line(),
        );
    }
    println!();
    println!(
        "{} dead-weight units (threshold: ≥{min_injected} injections, ≤{max_decided} decided)",
        rows.len()
    );
    Ok(())
}

pub(crate) fn cmd_peer_effectiveness(json_mode: bool) -> io::Result<()> {
    let store = HiveStore::load();
    let rows = crate::hive::effectiveness::peer_effectiveness(&store);

    if json_mode {
        let arr: Vec<serde_json::Value> = rows
            .iter()
            .map(|p| {
                serde_json::json!({
                    "peer_id": p.peer_id,
                    "unit_count": p.unit_count,
                    "total_injected": p.total_injected,
                    "total_accepted": p.total_accepted,
                    "total_overridden": p.total_overridden,
                    "total_decided": p.total_decided(),
                    "weighted_win_rate": p.weighted_win_rate,
                    "dead_weight_count": p.dead_weight_count,
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&arr).unwrap());
        return Ok(());
    }

    if rows.is_empty() {
        println!("No peers with knowledge in the store.");
        return Ok(());
    }

    println!(
        "{:<24} {:<6} {:<6} {:<8} {:<8} {:<8} {:<6}",
        "PEER", "UNITS", "INJ", "DEC", "WIN", "DEAD", ""
    );
    println!("{}", "─".repeat(80));
    for p in &rows {
        let win = if p.total_decided() > 0 {
            format!("{:.0}%", p.weighted_win_rate * 100.0)
        } else {
            "—".to_string()
        };
        let peer_short = if p.peer_id.len() > 23 {
            &p.peer_id[..23]
        } else {
            &p.peer_id
        };
        println!(
            "{:<24} {:<6} {:<6} {:<8} {:<8} {:<8} ",
            peer_short,
            p.unit_count,
            p.total_injected,
            p.total_decided(),
            win,
            p.dead_weight_count,
        );
    }
    println!();
    println!("{} peers", rows.len());
    Ok(())
}

pub(crate) fn cmd_resolutions(limit: usize, json_mode: bool) -> io::Result<()> {
    let rows = crate::hive::merger::recent_resolutions(limit);

    if json_mode {
        println!("{}", serde_json::to_string_pretty(&rows).unwrap());
        return Ok(());
    }

    if rows.is_empty() {
        println!("No merge resolutions recorded yet.");
        return Ok(());
    }

    println!(
        "{:<16} {:<10} {:<8} {:<8} RATIONALE",
        "RESULT", "WIN-PEER", "WIN-S", "LOSE-S"
    );
    println!("{}", "─".repeat(96));
    for r in &rows {
        let result = r.get("result").and_then(|v| v.as_str()).unwrap_or("?");
        let winner_peer = r.get("winner_peer").and_then(|v| v.as_str()).unwrap_or("?");
        let winner_score = r
            .get("winner_score")
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0);
        let loser_score = r.get("loser_score").and_then(|v| v.as_f64()).unwrap_or(0.0);
        let rationale = r.get("rationale").and_then(|v| v.as_str()).unwrap_or("");
        let peer_short = if winner_peer.len() > 9 {
            &winner_peer[..9]
        } else {
            winner_peer
        };
        println!(
            "{:<16} {:<10} {:<8.2} {:<8.2} {}",
            result, peer_short, winner_score, loser_score, rationale
        );
    }
    println!();
    println!("{} resolutions shown", rows.len());
    Ok(())
}

pub(crate) fn cmd_convergence(json_mode: bool) -> io::Result<()> {
    let store = HiveStore::load();
    let rows = crate::hive::convergence::peer_convergence(&store);
    let median = crate::hive::convergence::median_convergence(&rows);
    let converged = crate::hive::convergence::converged_peer_count(&rows);

    if json_mode {
        let payload = serde_json::json!({
            "local_total": store.len(),
            "peer_count": rows.len(),
            "converged_count": converged,
            "median_ratio": median,
            "peers": rows.iter().map(|r| serde_json::json!({
                "peer_id": r.peer_id,
                "local_total": r.local_total,
                "units_sent": r.units_sent,
                "ratio": r.ratio,
                "last_sync_epoch": r.last_sync_epoch,
                "converged": r.is_converged(),
            })).collect::<Vec<_>>(),
        });
        println!("{}", serde_json::to_string_pretty(&payload).unwrap());
        return Ok(());
    }

    if rows.is_empty() {
        println!("No gossip sync state recorded (no peers, or relay feature disabled).");
        return Ok(());
    }

    println!("Local store: {} units", store.len());
    if let Some(m) = median {
        println!("Median peer convergence: {:.0}%", m * 100.0);
    }
    println!("Converged peers (≥90%): {} / {}", converged, rows.len());
    println!();
    println!(
        "{:<24} {:<8} {:<10} {:<8}",
        "PEER", "RATIO", "SENT/TOTAL", "STATUS"
    );
    println!("{}", "─".repeat(64));
    for r in &rows {
        let peer_short = if r.peer_id.len() > 23 {
            &r.peer_id[..23]
        } else {
            &r.peer_id
        };
        let status = if r.is_converged() { "✓" } else { "lagging" };
        println!(
            "{:<24} {:<8.0}% {:>4}/{:<5} {:<8}",
            peer_short,
            r.ratio * 100.0,
            r.units_sent,
            r.local_total,
            status,
        );
    }
    Ok(())
}

/// `claudectl hive clusters [--problem <key>]`
pub(crate) fn cmd_clusters(problem_filter: Option<&str>, json_mode: bool) -> io::Result<()> {
    let store = HiveStore::load();
    let units = store.all_units();
    let clusters: Vec<&crate::hive::KnowledgeUnit> = units
        .iter()
        .copied()
        .filter(|u| {
            matches!(
                u.content,
                crate::hive::KnowledgeContent::ApproachCluster { .. }
            )
        })
        .filter(|u| {
            let Some(needle) = problem_filter else {
                return true;
            };
            if let crate::hive::KnowledgeContent::ApproachCluster { problem_key, .. } = &u.content {
                problem_key.contains(needle)
            } else {
                false
            }
        })
        .collect();

    if json_mode {
        let arr: Vec<serde_json::Value> = clusters
            .iter()
            .map(|u| {
                if let crate::hive::KnowledgeContent::ApproachCluster {
                    problem_key,
                    variants,
                } = &u.content
                {
                    serde_json::json!({
                        "id": u.id,
                        "problem_key": problem_key,
                        "variant_count": variants.len(),
                        "evidence_count": u.evidence_count,
                        "source_peer": u.source_peer,
                        "scope": u.scope.to_string(),
                    })
                } else {
                    serde_json::Value::Null
                }
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&arr).unwrap());
    } else if clusters.is_empty() {
        println!("No approach clusters in the hive yet.");
    } else {
        println!(
            "{:<6} {:<8} {:<26} {:<20} PROBLEM",
            "VRNTS", "EVID", "PEER", "SCOPE"
        );
        for u in clusters {
            if let crate::hive::KnowledgeContent::ApproachCluster {
                problem_key,
                variants,
            } = &u.content
            {
                println!(
                    "{:<6} {:<8} {:<26} {:<20} {}",
                    variants.len(),
                    u.evidence_count,
                    truncate_col_cli(&u.source_peer, 26),
                    truncate_col_cli(&u.scope.to_string(), 20),
                    problem_key
                );
            }
        }
    }
    Ok(())
}

/// `claudectl hive cluster <problem_key>`
pub(crate) fn cmd_cluster_show(problem_key: &str, json_mode: bool) -> io::Result<()> {
    let store = HiveStore::load();
    let units = store.all_units();
    let cluster = units.iter().find(|u| {
        if let crate::hive::KnowledgeContent::ApproachCluster {
            problem_key: pk, ..
        } = &u.content
        {
            pk == problem_key
        } else {
            false
        }
    });

    let Some(unit) = cluster else {
        if json_mode {
            println!("null");
        } else {
            println!("No cluster with problem_key {problem_key:?}.");
        }
        return Ok(());
    };
    let crate::hive::KnowledgeContent::ApproachCluster {
        problem_key,
        variants,
    } = &unit.content
    else {
        unreachable!();
    };

    if json_mode {
        let v = serde_json::json!({
            "id": unit.id,
            "problem_key": problem_key,
            "scope": unit.scope.to_string(),
            "source_peer": unit.source_peer,
            "evidence_count": unit.evidence_count,
            "version": unit.version,
            "variants": variants,
        });
        println!("{}", serde_json::to_string_pretty(&v).unwrap());
    } else {
        println!("Cluster: {problem_key}");
        println!("  ID:       {}", unit.id);
        println!("  Scope:    {}", unit.scope);
        println!("  Owner:    {}", unit.source_peer);
        println!("  Version:  {}", unit.version);
        println!("  Evidence: {}", unit.evidence_count);
        println!("  Variants:");
        for (i, v) in variants.iter().enumerate() {
            let label = (b'A' + i as u8) as char;
            println!("    ({label}) {} (n={})", v.approach_summary, v.evidence);
            if !v.conditions.is_empty() {
                println!("        when: {}", v.conditions.join(", "));
            }
            if !v.contributing_peers.is_empty() {
                println!("        peers: {}", v.contributing_peers.join(", "));
            }
            if let Some(ref outcome) = v.outcome_ref {
                println!("        outcome_ref: {outcome}");
            }
        }
    }
    Ok(())
}

pub(crate) fn parse_injection_state(s: &str) -> Option<crate::hive::InjectionState> {
    match s.to_lowercase().as_str() {
        "draft" => Some(crate::hive::InjectionState::Draft),
        "canary" => Some(crate::hive::InjectionState::Canary),
        "staged" => Some(crate::hive::InjectionState::Staged),
        "live" => Some(crate::hive::InjectionState::Live),
        _ => None,
    }
}
