// CLI dispatch for hive subcommands.

use std::io;

use clap::Subcommand;

use super::KnowledgeScope;
use super::store::HiveStore;

#[derive(Subcommand)]
pub enum HiveCommand {
    /// Turn the hive on (overrides config; broadcasts and brain injection enabled)
    On,

    /// Turn the hive off (overrides config; nothing broadcasts, no injection)
    Off,

    /// List local units that would be broadcast on the next gossip tick
    Preview,

    /// Mark units as exposed (broadcast outbound). Pass a unit ID or `--all`.
    Expose {
        /// Unit ID to expose
        unit_id: Option<String>,
        /// Expose every locally-originated unit currently in the store
        #[arg(long)]
        all: bool,
    },

    /// Hide units from outbound broadcast. Pass a unit ID or `--all`.
    Hide {
        /// Unit ID to hide
        unit_id: Option<String>,
        /// Hide every locally-originated unit currently in the store
        #[arg(long)]
        all: bool,
    },

    /// Show knowledge store overview
    Status,

    /// List knowledge units
    Knowledge {
        /// Filter by source peer
        #[arg(long)]
        from: Option<String>,
        /// Filter by scope (universal, language:X, project:X)
        #[arg(long)]
        scope: Option<String>,
    },

    /// Export all knowledge as JSON
    Export,

    /// Import knowledge from a JSON file
    Import {
        /// Path to JSON file
        file: String,
    },

    /// Remove a knowledge unit
    Forget {
        /// Unit ID to remove
        unit_id: String,
    },

    /// Show or set peer trust levels
    Trust {
        /// Peer ID (omit to list all)
        peer: Option<String>,
        /// Trust level 0.0-1.0 (omit to show current)
        level: Option<f64>,
    },

    /// Show cold storage archive stats, or prune old entries
    Archive {
        /// Prune entries older than N days (e.g., "90d" or "90")
        #[arg(long)]
        prune: Option<String>,
    },

    /// Run distillation pipeline on archive
    Distill,

    /// Show distilled curriculum
    Curriculum,

    /// Share a local skill, command, or hook with the hive
    Share {
        /// Content type: skill, command, or hook
        content_type: String,
        /// Path to the file (skill .md, command .md, or hooks.json entry)
        path: String,
        /// Scope: universal, language:X, or project:X
        #[arg(long, default_value = "universal")]
        scope: String,
    },

    /// Install a received skill, command, or hook from the hive
    Install {
        /// Knowledge unit ID to install
        unit_id: String,
        /// Target directory (default: ~/.claude)
        #[arg(long)]
        target: Option<String>,
        /// Force install even if compatibility checks fail
        #[arg(long)]
        force: bool,
    },

    /// List available shared skills, commands, and hooks from peers
    Shared {
        /// Filter by type: skill, command, hook
        #[arg(long, name = "type")]
        content_type: Option<String>,
        /// Show items from ignored peers
        #[arg(long)]
        show_ignored: bool,
    },

    /// Accept (install) a received artifact and record it as installed
    Accept {
        /// Knowledge unit ID to accept
        unit_id: String,
        /// Target directory (default: ~/.claude)
        #[arg(long)]
        target: Option<String>,
        /// Force accept even if compatibility checks fail
        #[arg(long)]
        force: bool,
    },

    /// Show or set the inbound accept mode (manual, trusted, all)
    AcceptMode {
        /// Mode value (omit to show current)
        mode: Option<String>,
    },

    /// List shared artifacts that haven't been accepted yet
    Pending {
        /// Filter by type: skill, command, hook
        #[arg(long, name = "type")]
        content_type: Option<String>,
    },

    /// List approach clusters (#221: competing approaches to the same problem).
    Clusters {
        /// Filter by problem_key substring
        #[arg(long)]
        problem: Option<String>,
    },

    /// Show one cluster in detail (variants, contributing peers, outcome refs).
    Cluster {
        /// problem_key (e.g., "Bash:cargo test")
        problem_key: String,
    },

    /// Rank knowledge units by effectiveness (win rate over decided outcomes)
    Effectiveness {
        /// Filter by source peer
        #[arg(long)]
        peer: Option<String>,
        /// Filter by category (best_practice, technique, workflow, personal)
        #[arg(long)]
        category: Option<String>,
        /// Filter by injection state (draft, canary, staged, live)
        #[arg(long)]
        state: Option<String>,
        /// Only show units with at least N decided outcomes
        #[arg(long, default_value_t = 0)]
        min_decided: u64,
        /// Cap results to top N rows
        #[arg(long, default_value_t = 50)]
        limit: usize,
    },

    /// List units that ride along in prompts but rarely match decisions
    DeadWeight {
        /// Minimum injections before a unit is eligible for the list
        #[arg(long, default_value_t = super::effectiveness::DEAD_WEIGHT_MIN_INJECTED)]
        min_injected: u64,
        /// Maximum decided outcomes (≤ this counts as dead-weight)
        #[arg(long, default_value_t = super::effectiveness::DEAD_WEIGHT_MAX_DECIDED)]
        max_decided: u64,
    },

    /// Aggregate effectiveness per source peer (weighted by decided outcomes)
    Peers,

    /// Show peers in quarantine or pending manual review (#226)
    Review {
        /// Unfreeze a specific peer (clear the manual freeze flag)
        #[arg(long)]
        unfreeze: Option<String>,
    },

    /// Browse what the hive knows before it auto-injects (#227)
    Explore {
        /// Filter by category (best_practice, technique, workflow, personal)
        #[arg(long)]
        category: Option<String>,
        /// Filter by scope (universal, language:X, project:X)
        #[arg(long)]
        scope: Option<String>,
        /// Filter by source peer
        #[arg(long)]
        peer: Option<String>,
        /// Minimum effective confidence (0.0 to 1.0)
        #[arg(long, default_value_t = 0.0)]
        min_confidence: f64,
        /// Include shared artifacts (skills/commands/hooks)
        #[arg(long)]
        include_artifacts: bool,
        /// Cap results to top N rows
        #[arg(long, default_value_t = 50)]
        limit: usize,
    },

    /// Rank peers by avg confidence in a category (who to learn from)
    Experts {
        /// Category to rank by (best_practice, technique, workflow, personal)
        category: String,
        /// Cap results to top N peers
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },

    /// Preview the curated welcome snapshot a fresh peer would receive
    Welcome,

    /// Show recent merge resolutions (autopsy trail for merger decisions, #228)
    Resolutions {
        /// How many recent resolutions to show (most recent first)
        #[arg(long, default_value_t = 50)]
        limit: usize,
    },

    /// Show per-peer gossip convergence (#229: how widely we've propagated)
    Convergence,

    /// Set or clear per-unit sharing consent (#230)
    Consent {
        /// Knowledge unit ID
        unit_id: String,
        /// Set an expiry (e.g., "7d", "12h"). Pass "never" to clear.
        #[arg(long)]
        expires_in: Option<String>,
        /// Add a peer to the allow list (only these peers receive the unit)
        #[arg(long)]
        allow_peer: Vec<String>,
        /// Add a peer to the exclude list (these peers never receive the unit)
        #[arg(long)]
        exclude_peer: Vec<String>,
        /// Minimum recipient trust tier (confirmed, suggested, unverified, any)
        #[arg(long)]
        min_tier: Option<String>,
        /// Clear all consent constraints for this unit
        #[arg(long)]
        clear: bool,
    },
}

/// Dispatch a hive subcommand.
pub fn dispatch_command(command: &HiveCommand, json_mode: bool) -> io::Result<()> {
    match command {
        HiveCommand::On => cmd_set_mode("on", json_mode),
        HiveCommand::Off => cmd_set_mode("off", json_mode),
        HiveCommand::Preview => cmd_preview(json_mode),
        HiveCommand::Expose { unit_id, all } => cmd_expose(unit_id.as_deref(), *all, json_mode),
        HiveCommand::Hide { unit_id, all } => cmd_hide(unit_id.as_deref(), *all, json_mode),
        HiveCommand::Status => cmd_status(json_mode),
        HiveCommand::Knowledge { from, scope } => {
            cmd_knowledge(from.as_deref(), scope.as_deref(), json_mode)
        }
        HiveCommand::Export => cmd_export(),
        HiveCommand::Import { file } => cmd_import(file),
        HiveCommand::Forget { unit_id } => cmd_forget(unit_id),
        HiveCommand::Trust { peer, level } => cmd_trust(peer.as_deref(), *level, json_mode),
        HiveCommand::Archive { prune } => cmd_archive(prune.as_deref(), json_mode),
        HiveCommand::Distill => cmd_distill(json_mode),
        HiveCommand::Curriculum => cmd_curriculum(json_mode),
        HiveCommand::Share {
            content_type,
            path,
            scope,
        } => cmd_share(content_type, path, scope, json_mode),
        HiveCommand::Install {
            unit_id,
            target,
            force,
        } => cmd_install(unit_id, target.as_deref(), *force, json_mode),
        HiveCommand::Shared {
            content_type,
            show_ignored,
        } => cmd_shared(content_type.as_deref(), *show_ignored, json_mode),
        HiveCommand::Accept {
            unit_id,
            target,
            force,
        } => cmd_install(unit_id, target.as_deref(), *force, json_mode),
        HiveCommand::AcceptMode { mode } => cmd_accept_mode(mode.as_deref(), json_mode),
        HiveCommand::Pending { content_type } => cmd_pending(content_type.as_deref(), json_mode),
        HiveCommand::Clusters { problem } => cmd_clusters(problem.as_deref(), json_mode),
        HiveCommand::Cluster { problem_key } => cmd_cluster_show(problem_key, json_mode),
        HiveCommand::Effectiveness {
            peer,
            category,
            state,
            min_decided,
            limit,
        } => cmd_effectiveness(
            peer.as_deref(),
            category.as_deref(),
            state.as_deref(),
            *min_decided,
            *limit,
            json_mode,
        ),
        HiveCommand::DeadWeight {
            min_injected,
            max_decided,
        } => cmd_dead_weight(*min_injected, *max_decided, json_mode),
        HiveCommand::Peers => cmd_peer_effectiveness(json_mode),
        HiveCommand::Review { unfreeze } => cmd_review(unfreeze.as_deref(), json_mode),
        HiveCommand::Explore {
            category,
            scope,
            peer,
            min_confidence,
            include_artifacts,
            limit,
        } => cmd_explore(
            category.as_deref(),
            scope.as_deref(),
            peer.as_deref(),
            *min_confidence,
            *include_artifacts,
            *limit,
            json_mode,
        ),
        HiveCommand::Experts { category, limit } => cmd_experts(category, *limit, json_mode),
        HiveCommand::Welcome => cmd_welcome(json_mode),
        HiveCommand::Resolutions { limit } => cmd_resolutions(*limit, json_mode),
        HiveCommand::Convergence => cmd_convergence(json_mode),
        HiveCommand::Consent {
            unit_id,
            expires_in,
            allow_peer,
            exclude_peer,
            min_tier,
            clear,
        } => cmd_consent(
            unit_id,
            expires_in.as_deref(),
            allow_peer,
            exclude_peer,
            min_tier.as_deref(),
            *clear,
            json_mode,
        ),
    }
}

fn cmd_resolutions(limit: usize, json_mode: bool) -> io::Result<()> {
    let rows = super::merger::recent_resolutions(limit);

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

fn cmd_convergence(json_mode: bool) -> io::Result<()> {
    let store = HiveStore::load();
    let rows = super::convergence::peer_convergence(&store);
    let median = super::convergence::median_convergence(&rows);
    let converged = super::convergence::converged_peer_count(&rows);

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

fn parse_duration_secs(s: &str) -> Option<u64> {
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

#[allow(clippy::too_many_arguments)]
fn cmd_consent(
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
                consent.expires_at = Some(super::epoch_secs() + secs);
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
                super::MinTrustTier::from_label(tier_str).ok_or_else(|| {
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
                let remaining = e.saturating_sub(super::epoch_secs());
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

fn cmd_review(unfreeze: Option<&str>, json_mode: bool) -> io::Result<()> {
    let mut trust = super::trust::TrustStore::load();

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

    let now = super::epoch_secs();
    let frozen = trust.frozen_peers();
    let quarantined: Vec<&super::trust::PeerTrust> = trust
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

// ────────────────────────────────────────────────────────────────────────────
// Discovery commands (#227)
// ────────────────────────────────────────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
fn cmd_explore(
    category: Option<&str>,
    scope: Option<&str>,
    peer: Option<&str>,
    min_confidence: f64,
    include_artifacts: bool,
    limit: usize,
    json_mode: bool,
) -> io::Result<()> {
    let store = HiveStore::load();
    let trust = super::trust::TrustStore::load();
    let parsed_scope = scope.map(parse_scope);
    let filter = super::discovery::ExploreFilter {
        category,
        scope: parsed_scope.as_ref(),
        peer,
        min_confidence,
        include_artifacts,
    };
    let rows = super::discovery::explore(&store, &trust, &filter, limit);

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

fn cmd_experts(category: &str, limit: usize, json_mode: bool) -> io::Result<()> {
    let store = HiveStore::load();
    let trust = super::trust::TrustStore::load();
    let rows = super::discovery::experts(&store, &trust, category, limit);

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

fn cmd_welcome(json_mode: bool) -> io::Result<()> {
    let store = HiveStore::load();
    let trust = super::trust::TrustStore::load();
    let units = super::discovery::welcome_snapshot(&store, &trust);

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
            "max_units": super::discovery::WELCOME_MAX_UNITS,
            "min_confidence": super::discovery::WELCOME_MIN_CONFIDENCE,
            "units": arr,
        });
        println!("{}", serde_json::to_string_pretty(&payload).unwrap());
        return Ok(());
    }

    println!(
        "Welcome snapshot ({} units, max {}, min confidence {:.0}%):",
        units.len(),
        super::discovery::WELCOME_MAX_UNITS,
        super::discovery::WELCOME_MIN_CONFIDENCE * 100.0
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

// ────────────────────────────────────────────────────────────────────────────
// Effectiveness commands (#225)
// ────────────────────────────────────────────────────────────────────────────

fn parse_injection_state(s: &str) -> Option<super::InjectionState> {
    match s.to_lowercase().as_str() {
        "draft" => Some(super::InjectionState::Draft),
        "canary" => Some(super::InjectionState::Canary),
        "staged" => Some(super::InjectionState::Staged),
        "live" => Some(super::InjectionState::Live),
        _ => None,
    }
}

fn cmd_effectiveness(
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
    let filter = super::effectiveness::EffectivenessFilter {
        peer,
        category,
        state: parsed_state,
        min_decided,
    };
    let mut rows = super::effectiveness::unit_effectiveness(&store, &filter);
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

fn cmd_dead_weight(min_injected: u64, max_decided: u64, json_mode: bool) -> io::Result<()> {
    let store = HiveStore::load();
    let rows = super::effectiveness::dead_weight(&store, min_injected, max_decided);

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

fn cmd_peer_effectiveness(json_mode: bool) -> io::Result<()> {
    let store = HiveStore::load();
    let rows = super::effectiveness::peer_effectiveness(&store);

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

/// `claudectl hive clusters [--problem <key>]`
fn cmd_clusters(problem_filter: Option<&str>, json_mode: bool) -> io::Result<()> {
    let store = HiveStore::load();
    let units = store.all_units();
    let clusters: Vec<&super::KnowledgeUnit> = units
        .iter()
        .copied()
        .filter(|u| matches!(u.content, super::KnowledgeContent::ApproachCluster { .. }))
        .filter(|u| {
            let Some(needle) = problem_filter else {
                return true;
            };
            if let super::KnowledgeContent::ApproachCluster { problem_key, .. } = &u.content {
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
                if let super::KnowledgeContent::ApproachCluster {
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
            if let super::KnowledgeContent::ApproachCluster {
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
fn cmd_cluster_show(problem_key: &str, json_mode: bool) -> io::Result<()> {
    let store = HiveStore::load();
    let units = store.all_units();
    let cluster = units.iter().find(|u| {
        if let super::KnowledgeContent::ApproachCluster {
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
    let super::KnowledgeContent::ApproachCluster {
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

fn truncate_col_cli(s: &str, width: usize) -> String {
    if s.chars().count() <= width {
        s.to_string()
    } else {
        let truncated: String = s.chars().take(width.saturating_sub(1)).collect();
        format!("{truncated}…")
    }
}

/// `claudectl hive on|off`
fn cmd_set_mode(mode: &str, json_mode: bool) -> io::Result<()> {
    super::write_mode_override(mode)?;
    let cfg = crate::config::Config::load();
    let active = super::is_active(cfg.hive.as_ref());

    if json_mode {
        let output = serde_json::json!({
            "mode_override": mode,
            "active": active,
            "config_enabled": cfg.hive.as_ref().map(|h| h.enabled).unwrap_or(false),
        });
        println!("{}", serde_json::to_string_pretty(&output).unwrap());
    } else {
        let label = if active { "active" } else { "inactive" };
        println!("Hive override: {mode} (currently {label})");
        if mode == "off" {
            println!("  Outbound gossip and brain injection paused.");
            println!("  Local store is preserved.");
        } else {
            println!("  Outbound gossip and brain injection enabled.");
        }
        println!();
        println!(
            "Tip: edit ~/.claudectl/hive/mode (or rerun) to change this; the file overrides config."
        );
    }
    Ok(())
}

/// `claudectl hive preview` — what would broadcast on the next gossip tick.
fn cmd_preview(json_mode: bool) -> io::Result<()> {
    let store = HiveStore::load();
    let cfg = crate::config::Config::load();
    let hive_cfg = cfg.hive.clone().unwrap_or_default();
    let active = super::is_active(cfg.hive.as_ref());

    let mode = super::exposure::ShareMode::parse(&hive_cfg.share_mode)
        .unwrap_or(super::exposure::ShareMode::Auto);
    let exposure = super::exposure::ExposureStore::load();
    let filter = super::SharingFilter::from_config(&hive_cfg);

    #[cfg(feature = "relay")]
    let local_id = crate::relay::load_or_create_identity().0;
    #[cfg(not(feature = "relay"))]
    let local_id = super::local_identity();

    // Partition local units into ready / hidden / blocked
    let mut ready = Vec::new();
    let mut hidden = Vec::new();
    let mut blocked: Vec<(&super::KnowledgeUnit, &'static str)> = Vec::new();

    for unit in store.all_units() {
        if unit.source_peer != local_id {
            continue;
        }
        if !unit.category.is_shareable() {
            blocked.push((unit, "personal"));
            continue;
        }
        if !filter.allows(unit) {
            blocked.push((unit, "filter"));
            continue;
        }
        let ttl_secs = hive_cfg.knowledge_ttl_days as u64 * 86400;
        let age = super::epoch_secs().saturating_sub(unit.last_validated_at);
        if age > ttl_secs {
            blocked.push((unit, "expired"));
            continue;
        }
        if unit.propagation_count >= hive_cfg.max_propagation {
            blocked.push((unit, "max-prop"));
            continue;
        }
        if exposure.is_exposed(&unit.id, mode) {
            ready.push(unit);
        } else {
            hidden.push(unit);
        }
    }

    if json_mode {
        let to_json = |units: &[&super::KnowledgeUnit]| -> Vec<serde_json::Value> {
            units
                .iter()
                .map(|u| {
                    serde_json::json!({
                        "id": u.id,
                        "scope": u.scope.to_string(),
                        "category": u.category.label(),
                        "summary": u.content.summary_line(),
                    })
                })
                .collect()
        };
        let output = serde_json::json!({
            "active": active,
            "share_mode": mode.label(),
            "ready": to_json(&ready),
            "hidden": to_json(&hidden),
            "blocked": blocked
                .iter()
                .map(|(u, reason)| serde_json::json!({
                    "id": u.id,
                    "summary": u.content.summary_line(),
                    "reason": reason,
                }))
                .collect::<Vec<_>>(),
        });
        println!("{}", serde_json::to_string_pretty(&output).unwrap());
        return Ok(());
    }

    let active_label = if active { "active" } else { "inactive" };
    println!(
        "Outbound preview ({} mode, hive {active_label})",
        mode.label()
    );
    println!();

    if ready.is_empty() && hidden.is_empty() && blocked.is_empty() {
        println!("  No locally-originated units yet.");
        return Ok(());
    }

    if !ready.is_empty() {
        println!("Ready to broadcast ({}):", ready.len());
        for u in &ready {
            print_preview_row(u);
        }
        println!();
    }
    if !hidden.is_empty() {
        println!("Hidden — won't broadcast ({}):", hidden.len());
        for u in &hidden {
            print_preview_row(u);
        }
        println!();
        println!("  Expose with: claudectl hive expose <id>  (or --all)");
        println!();
    }
    if !blocked.is_empty() {
        println!("Blocked by config or rules ({}):", blocked.len());
        for (u, reason) in &blocked {
            let id_short = short_id(&u.id);
            println!("  {id_short}  [{reason}]  {}", u.content.summary_line());
        }
    }

    Ok(())
}

fn print_preview_row(u: &super::KnowledgeUnit) {
    let id_short = short_id(&u.id);
    println!(
        "  {id_short}  {:<10}  {}",
        u.category.label(),
        u.content.summary_line()
    );
}

fn short_id(id: &str) -> &str {
    if id.len() > 12 { &id[..12] } else { id }
}

/// `claudectl hive expose [unit_id] [--all]`
fn cmd_expose(unit_id: Option<&str>, all: bool, json_mode: bool) -> io::Result<()> {
    apply_exposure(
        unit_id,
        all,
        super::exposure::ExposureState::Expose,
        json_mode,
    )
}

/// `claudectl hive hide [unit_id] [--all]`
fn cmd_hide(unit_id: Option<&str>, all: bool, json_mode: bool) -> io::Result<()> {
    apply_exposure(
        unit_id,
        all,
        super::exposure::ExposureState::Hide,
        json_mode,
    )
}

fn apply_exposure(
    unit_id: Option<&str>,
    all: bool,
    state: super::exposure::ExposureState,
    json_mode: bool,
) -> io::Result<()> {
    if unit_id.is_none() && !all {
        return Err(io::Error::other("specify a unit ID or --all".to_string()));
    }

    let store = HiveStore::load();
    let mut exposure = super::exposure::ExposureStore::load();

    #[cfg(feature = "relay")]
    let local_id = crate::relay::load_or_create_identity().0;
    #[cfg(not(feature = "relay"))]
    let local_id = super::local_identity();

    let mut affected: Vec<String> = Vec::new();

    if all {
        for u in store.all_units() {
            if u.source_peer == local_id {
                exposure.set(&u.id, state);
                affected.push(u.id.clone());
            }
        }
    } else if let Some(id) = unit_id {
        let unit = store
            .get(id)
            .ok_or_else(|| io::Error::other(format!("unknown unit: {id}")))?;
        if unit.source_peer != local_id {
            return Err(io::Error::other(
                "exposure only applies to locally-originated units".to_string(),
            ));
        }
        exposure.set(id, state);
        affected.push(id.to_string());
    }

    exposure
        .save()
        .map_err(|e| io::Error::other(format!("save exposure: {e}")))?;

    let action = match state {
        super::exposure::ExposureState::Expose => "exposed",
        super::exposure::ExposureState::Hide => "hidden",
    };

    if json_mode {
        let output = serde_json::json!({
            "action": action,
            "count": affected.len(),
            "unit_ids": affected,
        });
        println!("{}", serde_json::to_string_pretty(&output).unwrap());
    } else if affected.is_empty() {
        println!("No locally-originated units matched.");
    } else {
        println!("{} {} unit(s).", action, affected.len());
    }

    Ok(())
}

/// `claudectl hive status`
fn cmd_status(json_mode: bool) -> io::Result<()> {
    let store = HiveStore::load();
    let all = store.all_units();
    let cfg = crate::config::Config::load();
    let hive_cfg = cfg.hive.clone().unwrap_or_default();
    let active = super::is_active(cfg.hive.as_ref());
    let mode_override = super::read_mode_override();

    // Count by source
    let mut sources: std::collections::HashMap<String, u32> = std::collections::HashMap::new();
    let mut by_category: std::collections::HashMap<String, u32> = std::collections::HashMap::new();
    for unit in &all {
        *sources.entry(unit.source_peer.clone()).or_insert(0) += 1;
        *by_category
            .entry(unit.category.label().to_string())
            .or_insert(0) += 1;
    }

    // Count conflicts
    let conflict_count = conflict_line_count();

    // Load gossip sync state (only when relay is available)
    #[cfg(feature = "relay")]
    let relay_identity = Some(crate::relay::load_or_create_identity());
    #[cfg(not(feature = "relay"))]
    let relay_identity: Option<String> = None;

    if json_mode {
        #[allow(unused_mut)]
        let mut output = serde_json::json!({
            "active": active,
            "mode_override": mode_override,
            "share_mode": hive_cfg.share_mode,
            "config_enabled": cfg.hive.as_ref().map(|h| h.enabled).unwrap_or(false),
            "total_units": all.len(),
            "max_units": hive_cfg.max_units,
            "sources": sources,
            "categories": by_category,
            "conflicts": conflict_count,
        });
        #[cfg(feature = "relay")]
        if let Some(ref id) = relay_identity {
            output["identity"] = serde_json::json!(id.as_str());
            let gossip = super::gossip::GossipEngine::new(id.as_str(), 5, 30);
            output["sync_states"] = serde_json::json!(gossip.all_sync_states());
        }
        println!("{}", serde_json::to_string_pretty(&output).unwrap());
    } else {
        let state_label = if active { "ON" } else { "OFF" };
        let source_label = match mode_override.as_deref() {
            Some(s) => format!("override: {s}"),
            None => format!(
                "config: {}",
                if cfg.hive.as_ref().map(|h| h.enabled).unwrap_or(false) {
                    "enabled"
                } else {
                    "disabled"
                }
            ),
        };
        println!("Hive Knowledge Store [{state_label}]  ({source_label})");
        println!();
        if let Some(ref id) = relay_identity {
            println!("  Identity: {}", id);
        }
        println!("  Share mode: {}", hive_cfg.share_mode);
        println!("  Total units: {} / {} max", all.len(), hive_cfg.max_units);
        if !by_category.is_empty() {
            println!("  Categories:");
            for (cat, count) in &by_category {
                println!("    {cat}: {count}");
            }
        }
        if sources.is_empty() {
            println!("  No knowledge units yet.");
            println!("  Knowledge is generated automatically during brain distillation.");
        } else {
            println!("  Sources:");
            for (peer, count) in &sources {
                println!("    {peer}: {count} units");
            }
        }
        if conflict_count > 0 {
            println!();
            println!("  Merge conflicts: {conflict_count} (see ~/.claudectl/hive/conflicts.jsonl)");
        }
        #[cfg(feature = "relay")]
        if let Some(ref id) = relay_identity {
            let gossip = super::gossip::GossipEngine::new(id.as_str(), 5, 30);
            let sync_states = gossip.all_sync_states();
            if !sync_states.is_empty() {
                println!();
                println!("  Gossip sync state:");
                for (peer_id, state) in sync_states {
                    println!(
                        "    {peer_id}: {} units sent, last sync epoch {}",
                        state.units_sent.len(),
                        state.last_sync_epoch
                    );
                }
            }
        }
    }

    Ok(())
}

/// Count lines in the conflicts log.
fn conflict_line_count() -> usize {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    let path = std::path::PathBuf::from(home)
        .join(".claudectl")
        .join("hive")
        .join("conflicts.jsonl");
    std::fs::read_to_string(&path)
        .map(|c| c.lines().filter(|l| !l.is_empty()).count())
        .unwrap_or(0)
}

/// `claudectl hive knowledge [--scope X] [--from peer]`
fn cmd_knowledge(
    from_filter: Option<&str>,
    scope_filter: Option<&str>,
    json_mode: bool,
) -> io::Result<()> {
    let store = HiveStore::load();

    let mut units: Vec<&super::KnowledgeUnit> = if let Some(from) = from_filter {
        store.by_source(from)
    } else if let Some(scope_str) = scope_filter {
        let scope = parse_scope(scope_str);
        store.by_scope(&scope)
    } else {
        store.all_units()
    };

    // Sort by confidence descending
    units.sort_by(|a, b| {
        b.confidence
            .partial_cmp(&a.confidence)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    if json_mode {
        println!("{}", serde_json::to_string_pretty(&units).unwrap());
    } else if units.is_empty() {
        println!("No knowledge units found.");
    } else {
        println!(
            "{:<12} {:<15} {:<8} {:<6} {:<8} {:<8} CONTENT",
            "ID", "SCOPE", "CONF", "EVID", "ROLLOUT", "WIN"
        );
        println!("{}", "─".repeat(96));
        for unit in &units {
            let id_short = if unit.id.len() > 11 {
                &unit.id[..11]
            } else {
                &unit.id
            };
            let win = if unit.injection_stats.decided() > 0 {
                format!("{:.0}%", unit.injection_stats.win_rate() * 100.0)
            } else {
                "—".to_string()
            };
            println!(
                "{:<12} {:<15} {:<8.0}% {:<6} {:<8} {:<8} {}",
                id_short,
                unit.scope.to_string(),
                unit.confidence * 100.0,
                unit.evidence_count,
                unit.injection_state.label(),
                win,
                unit.content.summary_line(),
            );
        }
        println!();
        println!("{} units total", units.len());
    }

    Ok(())
}

/// `claudectl hive export`
fn cmd_export() -> io::Result<()> {
    let store = HiveStore::load();
    println!("{}", store.export_json());
    Ok(())
}

/// `claudectl hive import <file>`
fn cmd_import(path: &str) -> io::Result<()> {
    let content =
        std::fs::read_to_string(path).map_err(|e| io::Error::other(format!("read {path}: {e}")))?;

    let mut store = HiveStore::load();
    let count = store
        .import_json(&content)
        .map_err(|e| io::Error::other(format!("import: {e}")))?;
    store
        .save()
        .map_err(|e| io::Error::other(format!("save: {e}")))?;

    println!("Imported {count} new knowledge units.");

    Ok(())
}

/// `claudectl hive forget <unit_id>`
fn cmd_forget(unit_id: &str) -> io::Result<()> {
    let mut store = HiveStore::load();

    if store.remove(unit_id) {
        store
            .save()
            .map_err(|e| io::Error::other(format!("save: {e}")))?;
        println!("Removed knowledge unit: {unit_id}");
    } else {
        eprintln!("Unknown unit: {unit_id}");
        return Err(io::Error::other("unknown unit"));
    }

    Ok(())
}

/// `claudectl hive trust [peer_id] [level]`
fn cmd_trust(peer: Option<&str>, level: Option<f64>, json_mode: bool) -> io::Result<()> {
    let mut trust_store = super::trust::TrustStore::load();

    match (peer, level) {
        (None, _) => {
            // Show all peer trust levels
            let all = trust_store.all();
            if json_mode {
                let peers: Vec<&super::trust::PeerTrust> = all;
                println!("{}", serde_json::to_string_pretty(&peers).unwrap());
            } else if all.is_empty() {
                println!("No peer trust data yet.");
            } else {
                println!(
                    "{:<20} {:<8} {:<10} {:<10} TIER",
                    "PEER", "TRUST", "ACCEPTED", "CONFLICTS"
                );
                println!("{}", "─".repeat(65));
                for trust in &all {
                    println!(
                        "{:<20} {:<8.2} {:<10} {:<10} {}",
                        trust.peer_id,
                        trust.trust_level,
                        trust.knowledge_accepted,
                        trust.knowledge_conflicted,
                        trust.tier().label(),
                    );
                }
            }
        }
        (Some(peer_id), None) => {
            // Show trust for a specific peer
            if let Some(trust) = trust_store.get(peer_id) {
                if json_mode {
                    println!("{}", serde_json::to_string_pretty(trust).unwrap());
                } else {
                    println!("Peer: {}", trust.peer_id);
                    println!("  Trust level: {:.2}", trust.trust_level);
                    println!("  Tier: {}", trust.tier().label());
                    println!("  Accepted: {}", trust.knowledge_accepted);
                    println!("  Conflicts: {}", trust.knowledge_conflicted);
                }
            } else {
                eprintln!("Unknown peer: {peer_id}");
                return Err(io::Error::other("unknown peer"));
            }
        }
        (Some(peer_id), Some(level)) => {
            // Set trust level
            trust_store.set_trust(peer_id, level);
            trust_store.save().map_err(io::Error::other)?;
            let actual = trust_store.get(peer_id).unwrap();
            println!(
                "Set trust for {} to {:.2} ({})",
                peer_id,
                actual.trust_level,
                actual.tier().label()
            );
        }
    }

    Ok(())
}

/// Parse a scope string like "universal", "language:rust", "project:foo".
fn parse_scope(s: &str) -> KnowledgeScope {
    if s == "universal" {
        KnowledgeScope::Universal
    } else if let Some(lang) = s.strip_prefix("language:") {
        KnowledgeScope::Language(lang.to_string())
    } else if let Some(proj) = s.strip_prefix("project:") {
        KnowledgeScope::Project(proj.to_string())
    } else {
        // Try shorthand: "rust" -> Language, anything else -> Project
        KnowledgeScope::Project(s.to_string())
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Frontmatter parsing
// ────────────────────────────────────────────────────────────────────────────

/// Parse YAML frontmatter from a markdown file.
/// Returns key-value pairs from the `---`-delimited block.
fn parse_frontmatter(content: &str) -> std::collections::HashMap<String, String> {
    let mut map = std::collections::HashMap::new();

    let trimmed = content.trim_start();
    if !trimmed.starts_with("---") {
        return map;
    }

    // Find closing ---
    let after_open = &trimmed[3..].trim_start_matches('\r');
    let after_open = after_open.strip_prefix('\n').unwrap_or(after_open);

    let Some(close_pos) = after_open.find("\n---") else {
        return map;
    };

    let yaml_block = &after_open[..close_pos];

    for line in yaml_block.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((key, value)) = line.split_once(':') {
            let key = key.trim().to_string();
            let value = value
                .trim()
                .trim_matches('"')
                .trim_matches('\'')
                .to_string();
            if !key.is_empty() && !value.is_empty() {
                map.insert(key, value);
            }
        }
    }

    map
}

// ────────────────────────────────────────────────────────────────────────────
// Share, Install, Shared commands
// ────────────────────────────────────────────────────────────────────────────

/// Build `ArtifactRequires` from frontmatter overrides + auto-detection.
/// Frontmatter keys: `requires_cli`, `requires_os`, `requires_min_version`.
fn build_requires(
    fm: &std::collections::HashMap<String, String>,
    body: &str,
) -> super::ArtifactRequires {
    // Frontmatter overrides take priority
    let cli = if let Some(val) = fm.get("requires_cli") {
        val.split(',').map(|s| s.trim().to_string()).collect()
    } else {
        super::detect_cli_deps(body)
    };

    let os = if let Some(val) = fm.get("requires_os") {
        val.split(',').map(|s| s.trim().to_string()).collect()
    } else {
        super::detect_os_deps(body)
    };

    let min_version = fm.get("requires_min_version").cloned();

    super::ArtifactRequires {
        cli,
        os,
        min_version,
    }
}

/// `claudectl hive share <type> <path> [--scope X]`
fn cmd_share(content_type: &str, path: &str, scope_str: &str, json_mode: bool) -> io::Result<()> {
    let body =
        std::fs::read_to_string(path).map_err(|e| io::Error::other(format!("read {path}: {e}")))?;

    let scope = parse_scope(scope_str);
    let identity = super::local_identity();
    let now = super::epoch_secs();

    let content = match content_type {
        "skill" => {
            if body.len() > super::MAX_SKILL_BYTES {
                return Err(io::Error::other(format!(
                    "skill body too large: {} bytes (max {})",
                    body.len(),
                    super::MAX_SKILL_BYTES
                )));
            }
            let fm = parse_frontmatter(&body);
            let name = fm
                .get("name")
                .cloned()
                .ok_or_else(|| io::Error::other("skill missing 'name' in frontmatter"))?;
            let description = fm
                .get("description")
                .cloned()
                .unwrap_or_else(|| name.clone());
            let version = fm.get("version").cloned().unwrap_or_else(|| "0.0.0".into());
            let requires = build_requires(&fm, &body);
            super::KnowledgeContent::Skill {
                name,
                description,
                version,
                body,
                requires,
            }
        }
        "command" => {
            if body.len() > super::MAX_COMMAND_BYTES {
                return Err(io::Error::other(format!(
                    "command body too large: {} bytes (max {})",
                    body.len(),
                    super::MAX_COMMAND_BYTES
                )));
            }
            let fm = parse_frontmatter(&body);
            let name = fm
                .get("name")
                .cloned()
                .ok_or_else(|| io::Error::other("command missing 'name' in frontmatter"))?;
            let description = fm
                .get("description")
                .cloned()
                .unwrap_or_else(|| name.clone());
            let args = fm.get("args").cloned();
            let requires = build_requires(&fm, &body);
            super::KnowledgeContent::Command {
                name,
                description,
                args,
                body,
                requires,
            }
        }
        "hook" => {
            if body.len() > super::MAX_HOOK_CONFIG_BYTES {
                return Err(io::Error::other(format!(
                    "hook config too large: {} bytes (max {})",
                    body.len(),
                    super::MAX_HOOK_CONFIG_BYTES
                )));
            }
            // Parse as JSON hook config
            let parsed: serde_json::Value = serde_json::from_str(&body)
                .map_err(|e| io::Error::other(format!("invalid JSON: {e}")))?;

            let event = parsed
                .get("event")
                .and_then(|v| v.as_str())
                .ok_or_else(|| io::Error::other("hook config missing 'event' field"))?
                .to_string();
            let matcher = parsed
                .get("matcher")
                .and_then(|v| v.as_str())
                .unwrap_or("*")
                .to_string();
            let description = parsed
                .get("description")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();

            let sanitized = super::sanitize_hook_config(&body);

            // For hooks: extract command binary as a CLI dep
            let mut requires = super::ArtifactRequires::default();
            if let Some(cmd) = parsed.get("command").and_then(|v| v.as_str()) {
                let binary = cmd.rsplit('/').next().unwrap_or(cmd);
                if !binary.is_empty() {
                    requires.cli.push(binary.to_string());
                }
            }

            super::KnowledgeContent::HookConfig {
                event,
                matcher,
                description,
                config_json: sanitized,
                requires,
            }
        }
        other => {
            return Err(io::Error::other(format!(
                "unknown content type: {other} (expected: skill, command, hook)"
            )));
        }
    };

    let category = match &content {
        super::KnowledgeContent::HookConfig { .. } => super::KnowledgeCategory::WorkflowPattern,
        _ => super::KnowledgeCategory::Technique,
    };

    let unit = super::KnowledgeUnit {
        id: super::gen_ku_id(),
        scope,
        category,
        content,
        evidence_count: 1,
        confidence: 1.0,
        source_peer: identity,
        originated_at: now,
        last_validated_at: now,
        propagation_count: 0,
        version: 1,
        revalidation_interval_secs: 0,
        injection_state: crate::hive::InjectionState::Live,
        injection_stats: crate::hive::InjectionStats {
            injected_count: 0,
            accepted_count: 0,
            overridden_count: 0,
            last_injected_at: 0,
            last_outcome_at: 0,
        },
        sharing_consent: None,
    };

    let summary = unit.content.summary_line();
    let unit_id = unit.id.clone();

    let mut store = HiveStore::load();
    store.insert(unit);
    store
        .save()
        .map_err(|e| io::Error::other(format!("save: {e}")))?;

    // Signal gossip if relay is active
    #[cfg(feature = "relay")]
    super::signal_new_knowledge(1);

    if json_mode {
        let output = serde_json::json!({
            "action": "shared",
            "unit_id": unit_id,
            "content_type": content_type,
            "summary": summary,
        });
        println!("{}", serde_json::to_string_pretty(&output).unwrap());
    } else {
        println!("Shared {content_type}: {summary}");
        println!("  Unit ID: {unit_id}");
    }

    Ok(())
}

/// Outcome of an artifact write — used by both interactive install and auto-accept.
pub enum InstallOutcome {
    Skill {
        name: String,
        version: String,
        path: std::path::PathBuf,
    },
    Command {
        name: String,
        args: Option<String>,
        path: std::path::PathBuf,
    },
    /// Hooks are never auto-installed; we surface the config for the user to review.
    HookConfig {
        event: String,
        matcher: String,
        description: String,
        config_json: String,
    },
}

/// Write a skill or command to disk. Hooks are returned as config-only —
/// callers decide how to present them. Returns None for non-artifact units.
pub fn write_artifact_files(
    unit: &super::KnowledgeUnit,
    base_dir: &std::path::Path,
) -> io::Result<Option<InstallOutcome>> {
    match &unit.content {
        super::KnowledgeContent::Skill {
            name,
            version,
            body,
            ..
        } => {
            let slug = name.to_lowercase().replace(' ', "-");
            let skill_dir = base_dir.join("skills").join(&slug);
            std::fs::create_dir_all(&skill_dir)?;
            let file_path = skill_dir.join("SKILL.md");
            std::fs::write(&file_path, body)?;
            Ok(Some(InstallOutcome::Skill {
                name: name.clone(),
                version: version.clone(),
                path: file_path,
            }))
        }
        super::KnowledgeContent::Command {
            name, body, args, ..
        } => {
            let cmds_dir = base_dir.join("commands");
            std::fs::create_dir_all(&cmds_dir)?;
            let file_path = cmds_dir.join(format!("{name}.md"));
            std::fs::write(&file_path, body)?;
            Ok(Some(InstallOutcome::Command {
                name: name.clone(),
                args: args.clone(),
                path: file_path,
            }))
        }
        super::KnowledgeContent::HookConfig {
            event,
            matcher,
            description,
            config_json,
            ..
        } => Ok(Some(InstallOutcome::HookConfig {
            event: event.clone(),
            matcher: matcher.clone(),
            description: description.clone(),
            config_json: config_json.clone(),
        })),
        _ => Ok(None),
    }
}

/// Default `~/.claude` install root.
pub fn default_install_dir() -> std::path::PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    std::path::PathBuf::from(home).join(".claude")
}

/// `claudectl hive install <unit_id> [--target dir] [--force]`
fn cmd_install(
    unit_id: &str,
    target: Option<&str>,
    force: bool,
    json_mode: bool,
) -> io::Result<()> {
    let store = HiveStore::load();
    let unit = store
        .get(unit_id)
        .ok_or_else(|| io::Error::other(format!("unknown unit: {unit_id}")))?;

    // Check trust tier
    let trust_store = super::trust::TrustStore::load();
    let tier = trust_store
        .get(&unit.source_peer)
        .map(|t| t.tier())
        .unwrap_or(super::trust::TrustTier::Suggested);

    if tier == super::trust::TrustTier::Ignored {
        return Err(io::Error::other(format!(
            "source peer '{}' is in Ignored tier (trust < 0.2). \
             Set higher trust first: claudectl hive trust {} 0.5",
            unit.source_peer, unit.source_peer,
        )));
    }

    // Check compatibility
    if let Some(requires) = super::get_requires(&unit.content) {
        let issues = super::check_compatibility(requires);
        if !issues.is_empty() {
            let has_blocking = issues.iter().any(|i| i.is_blocking());
            for issue in &issues {
                if issue.is_blocking() {
                    eprintln!("Error: {issue}");
                } else {
                    eprintln!("Warning: {issue}");
                }
            }
            if has_blocking && !force {
                return Err(io::Error::other(
                    "compatibility check failed. Use --force to install anyway.",
                ));
            }
        }
    }

    let base_dir = match target {
        Some(t) => std::path::PathBuf::from(t),
        None => default_install_dir(),
    };

    let outcome = write_artifact_files(unit, &base_dir)?.ok_or_else(|| {
        io::Error::other(format!("unit {unit_id} is not a skill, command, or hook"))
    })?;

    let unverified_warning = if tier == super::trust::TrustTier::Unverified {
        Some(format!(
            "Warning: source peer '{}' is unverified. Review before use.",
            unit.source_peer
        ))
    } else {
        None
    };

    let mut tracker = super::accept::InstalledTracker::load();
    let mut record_install = || {
        tracker.record(
            unit_id,
            &unit.source_peer,
            super::accept::AcceptMode::Manual,
        );
        let _ = tracker.save();
    };

    match outcome {
        InstallOutcome::Skill {
            name,
            version,
            path,
        } => {
            if let Some(w) = unverified_warning {
                eprintln!("{w}");
            }
            record_install();
            if json_mode {
                let output = serde_json::json!({
                    "action": "installed",
                    "content_type": "skill",
                    "name": name,
                    "version": version,
                    "path": path.display().to_string(),
                    "trust_tier": tier.label(),
                });
                println!("{}", serde_json::to_string_pretty(&output).unwrap());
            } else {
                println!("Installed skill '{name}' v{version} to {}", path.display());
            }
        }
        InstallOutcome::Command { name, args, path } => {
            if let Some(w) = unverified_warning {
                eprintln!("{w}");
            }
            record_install();
            if json_mode {
                let output = serde_json::json!({
                    "action": "installed",
                    "content_type": "command",
                    "name": name,
                    "args": args,
                    "path": path.display().to_string(),
                    "trust_tier": tier.label(),
                });
                println!("{}", serde_json::to_string_pretty(&output).unwrap());
            } else {
                println!("Installed command '/{name}' to {}", path.display());
            }
        }
        InstallOutcome::HookConfig {
            event,
            matcher,
            description,
            config_json,
        } => {
            if let Some(w) = unverified_warning {
                eprintln!("{w}");
            }
            // Hooks aren't recorded as installed — user pastes config manually.
            if json_mode {
                let output = serde_json::json!({
                    "action": "installed",
                    "content_type": "hook",
                    "event": event,
                    "matcher": matcher,
                    "description": description,
                    "config_json": config_json,
                    "trust_tier": tier.label(),
                    "note": "Add this config to your hooks.json manually",
                });
                println!("{}", serde_json::to_string_pretty(&output).unwrap());
            } else {
                println!("Hook config: {event}[{matcher}] — {description}");
                println!();
                println!("Add the following to your hooks.json:");
                println!("{config_json}");
                println!();
                println!("Note: You must create the hook script implementation yourself.");
            }
        }
    }

    Ok(())
}

/// `claudectl hive shared [--type X] [--show-ignored]`
fn cmd_shared(
    content_type_filter: Option<&str>,
    show_ignored: bool,
    json_mode: bool,
) -> io::Result<()> {
    let store = HiveStore::load();
    let trust_store = super::trust::TrustStore::load();

    let units: Vec<(&super::KnowledgeUnit, super::trust::TrustTier)> = store
        .all_units()
        .into_iter()
        .filter_map(|unit| {
            // Filter to artifact types only
            let type_label = match &unit.content {
                super::KnowledgeContent::Skill { .. } => "skill",
                super::KnowledgeContent::Command { .. } => "command",
                super::KnowledgeContent::HookConfig { .. } => "hook",
                _ => return None,
            };

            // Apply type filter
            if let Some(filter) = content_type_filter {
                if type_label != filter {
                    return None;
                }
            }

            let tier = trust_store
                .get(&unit.source_peer)
                .map(|t| t.tier())
                .unwrap_or(super::trust::TrustTier::Suggested);

            // Skip ignored unless requested
            if tier == super::trust::TrustTier::Ignored && !show_ignored {
                return None;
            }

            Some((unit, tier))
        })
        .collect();

    if json_mode {
        let items: Vec<serde_json::Value> = units
            .iter()
            .map(|(unit, tier)| {
                let compat = super::compat_label(&unit.content);
                let mut obj = serde_json::json!({
                    "id": unit.id,
                    "type": content_type_label(&unit.content),
                    "name": content_name(&unit.content),
                    "source_peer": unit.source_peer,
                    "trust_tier": tier.label(),
                    "compat": compat,
                    "summary": unit.content.summary_line(),
                });
                if let Some(req) = super::get_requires(&unit.content) {
                    obj["requires"] = serde_json::json!(req);
                }
                obj
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&items).unwrap());
    } else if units.is_empty() {
        println!("No shared skills, commands, or hooks available.");
        println!("Share content with: claudectl hive share <skill|command|hook> <path>");
    } else {
        println!(
            "{:<12} {:<8} {:<16} {:<16} {:<6} CONTENT",
            "ID", "TYPE", "SOURCE", "TRUST", "COMPAT"
        );
        println!("{}", "─".repeat(90));
        for (unit, tier) in &units {
            let id_short = if unit.id.len() > 11 {
                &unit.id[..11]
            } else {
                &unit.id
            };
            let type_label = content_type_label(&unit.content);
            let compat = super::compat_label(&unit.content);
            println!(
                "{:<12} {:<8} {:<16} {:<16} {:<6} {}",
                id_short,
                type_label,
                unit.source_peer,
                tier.label(),
                compat,
                unit.content.summary_line(),
            );
        }
        println!();
        println!(
            "{} items total. Install with: claudectl hive install <id>",
            units.len()
        );
    }

    Ok(())
}

/// Get the content type label for display.
fn content_type_label(content: &super::KnowledgeContent) -> &'static str {
    match content {
        super::KnowledgeContent::Skill { .. } => "skill",
        super::KnowledgeContent::Command { .. } => "command",
        super::KnowledgeContent::HookConfig { .. } => "hook",
        _ => "other",
    }
}

/// Get the name from a content unit.
fn content_name(content: &super::KnowledgeContent) -> String {
    match content {
        super::KnowledgeContent::Skill { name, .. } => name.clone(),
        super::KnowledgeContent::Command { name, .. } => name.clone(),
        super::KnowledgeContent::HookConfig { event, matcher, .. } => {
            format!("{event}[{matcher}]")
        }
        _ => String::new(),
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Archive and distillation commands
// ────────────────────────────────────────────────────────────────────────────

/// `claudectl hive archive [--prune Nd]`
fn cmd_archive(prune: Option<&str>, json_mode: bool) -> io::Result<()> {
    if let Some(val) = prune {
        let days_str = val.trim_end_matches('d');
        let days: u32 = days_str
            .parse()
            .map_err(|_| io::Error::other(format!("invalid prune value: {val}")))?;
        let pruned = super::archive::prune_archive(days)
            .map_err(|e| io::Error::other(format!("prune: {e}")))?;
        println!("Pruned {pruned} archive entries older than {days} days.");
        return Ok(());
    }

    let count = super::archive::archive_count();
    let size = super::archive::archive_size_bytes();
    let meta = super::archive::load_curriculum_meta();

    if json_mode {
        let output = serde_json::json!({
            "archive_units": count,
            "archive_size_bytes": size,
            "curriculum": meta,
        });
        println!("{}", serde_json::to_string_pretty(&output).unwrap());
    } else {
        println!("Hive Archive (cold storage)");
        println!();
        println!("  Archive units: {count}");
        println!("  Archive size: {:.1} KB", size as f64 / 1024.0);
        if let Some(m) = meta {
            println!();
            println!("  Latest curriculum:");
            println!("    Version: v{}", m.version);
            println!("    Units: {}", m.unit_count);
            println!("    Source: {} archive units", m.source_archive_units);
        } else {
            println!();
            println!("  No curriculum yet. Run: claudectl hive distill");
        }
    }

    Ok(())
}

/// `claudectl hive distill`
fn cmd_distill(json_mode: bool) -> io::Result<()> {
    println!("Running cold distillation...");
    let report = super::archive::distill_archive()
        .map_err(|e| io::Error::other(format!("distillation failed: {e}")))?;

    if json_mode {
        println!("{}", serde_json::to_string_pretty(&report).unwrap());
    } else {
        println!();
        println!("Distillation complete:");
        println!("  Archive units read: {}", report.archive_units_read);
        println!("  Duplicates merged: {}", report.duplicates_merged);
        println!("  Patterns condensed: {}", report.patterns_condensed);
        println!("  Contradictions resolved: {}", report.contradictions_found);
        println!(
            "  Curriculum: v{} ({} units)",
            report.curriculum_version, report.curriculum_units
        );
    }

    Ok(())
}

/// `claudectl hive curriculum`
fn cmd_curriculum(json_mode: bool) -> io::Result<()> {
    let curriculum = super::archive::load_curriculum();
    let meta = super::archive::load_curriculum_meta();

    if json_mode {
        let output = serde_json::json!({
            "meta": meta,
            "units": curriculum,
        });
        println!("{}", serde_json::to_string_pretty(&output).unwrap());
        return Ok(());
    }

    if curriculum.is_empty() {
        println!("No curriculum yet. Run: claudectl hive distill");
        return Ok(());
    }

    if let Some(m) = &meta {
        println!(
            "Curriculum v{} ({} units, distilled from {} archive units)",
            m.version, m.unit_count, m.source_archive_units
        );
    }
    println!();
    println!(
        "{:<12} {:<14} {:<8} {:<6} CONTENT",
        "ID", "CATEGORY", "CONF", "EVID"
    );
    println!("{}", "─".repeat(80));

    for unit in &curriculum {
        let id_short = if unit.id.len() > 11 {
            &unit.id[..11]
        } else {
            &unit.id
        };
        println!(
            "{:<12} {:<14} {:<8.0}% {:<6} {}",
            id_short,
            unit.category.label(),
            unit.confidence * 100.0,
            unit.evidence_count,
            unit.content.summary_line(),
        );
    }
    println!();
    println!("{} units in curriculum", curriculum.len());

    Ok(())
}

// ────────────────────────────────────────────────────────────────────────────
// Inbound accept controls
// ────────────────────────────────────────────────────────────────────────────

/// `claudectl hive accept-mode [manual|trusted|all]`
fn cmd_accept_mode(mode: Option<&str>, json_mode: bool) -> io::Result<()> {
    use super::accept::AcceptMode;

    match mode {
        None => {
            let current = super::accept::read_mode(AcceptMode::Manual);
            if json_mode {
                let output = serde_json::json!({
                    "accept_mode": current.label(),
                });
                println!("{}", serde_json::to_string_pretty(&output).unwrap());
            } else {
                println!("Accept mode: {}", current.label());
                println!();
                println!("Modes:");
                println!("  manual  — hold all incoming artifacts; user accepts each (default)");
                println!("  trusted — auto-install from peers in the Confirmed trust tier");
                println!(
                    "  all     — auto-install every received skill/command (hooks always manual)"
                );
            }
        }
        Some(m) => {
            let parsed = AcceptMode::parse(m).ok_or_else(|| {
                io::Error::other(format!("invalid accept mode: {m} (manual|trusted|all)"))
            })?;
            super::accept::write_mode(parsed)?;
            println!("Accept mode set to: {}", parsed.label());
            if parsed != AcceptMode::Manual {
                println!("  Newly received skills/commands will be installed automatically.");
                println!("  Hooks always require manual review.");
            }
        }
    }
    Ok(())
}

/// `claudectl hive pending [--type X]` — list shared artifacts not yet accepted.
fn cmd_pending(content_type_filter: Option<&str>, json_mode: bool) -> io::Result<()> {
    let store = HiveStore::load();
    let trust_store = super::trust::TrustStore::load();
    let tracker = super::accept::InstalledTracker::load();

    let pending: Vec<(&super::KnowledgeUnit, super::trust::TrustTier)> = store
        .all_units()
        .into_iter()
        .filter_map(|unit| {
            let type_label = match &unit.content {
                super::KnowledgeContent::Skill { .. } => "skill",
                super::KnowledgeContent::Command { .. } => "command",
                super::KnowledgeContent::HookConfig { .. } => "hook",
                _ => return None,
            };
            if let Some(filter) = content_type_filter {
                if type_label != filter {
                    return None;
                }
            }
            if tracker.is_installed(&unit.id) {
                return None;
            }
            let tier = trust_store
                .get(&unit.source_peer)
                .map(|t| t.tier())
                .unwrap_or(super::trust::TrustTier::Suggested);
            if tier == super::trust::TrustTier::Ignored {
                return None;
            }
            Some((unit, tier))
        })
        .collect();

    if json_mode {
        let items: Vec<serde_json::Value> = pending
            .iter()
            .map(|(unit, tier)| {
                serde_json::json!({
                    "id": unit.id,
                    "type": content_type_label(&unit.content),
                    "name": content_name(&unit.content),
                    "source_peer": unit.source_peer,
                    "trust_tier": tier.label(),
                    "summary": unit.content.summary_line(),
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&items).unwrap());
        return Ok(());
    }

    if pending.is_empty() {
        println!("No pending artifacts. Run `claudectl hive shared` to see all received items.");
        return Ok(());
    }

    println!(
        "{:<12} {:<8} {:<16} {:<14} CONTENT",
        "ID", "TYPE", "SOURCE", "TRUST"
    );
    println!("{}", "─".repeat(80));
    for (unit, tier) in &pending {
        let id_short = if unit.id.len() > 11 {
            &unit.id[..11]
        } else {
            &unit.id
        };
        println!(
            "{:<12} {:<8} {:<16} {:<14} {}",
            id_short,
            content_type_label(&unit.content),
            unit.source_peer,
            tier.label(),
            unit.content.summary_line(),
        );
    }
    println!();
    println!(
        "{} pending. Accept with: claudectl hive accept <id>",
        pending.len()
    );

    Ok(())
}

/// Auto-install eligible artifacts after a gossip merge. Skills/commands only.
/// Returns the number of units installed. Errors are logged but never fatal.
pub fn auto_accept_units(
    units: &[super::KnowledgeUnit],
    base_dir: Option<&std::path::Path>,
) -> usize {
    use super::accept::AcceptMode;

    let mode = super::accept::read_mode(AcceptMode::Manual);
    if mode == AcceptMode::Manual {
        return 0;
    }

    let trust_store = super::trust::TrustStore::load();
    let mut tracker = super::accept::InstalledTracker::load();
    let owned_dir = base_dir.map(std::path::PathBuf::from);
    let dir = owned_dir.unwrap_or_else(default_install_dir);
    let mut installed = 0;

    for unit in units {
        // Don't reinstall.
        if tracker.is_installed(&unit.id) {
            continue;
        }

        // Only auto-install skills and commands; hooks always require manual review.
        let is_artifact = matches!(
            &unit.content,
            super::KnowledgeContent::Skill { .. } | super::KnowledgeContent::Command { .. }
        );
        if !is_artifact {
            continue;
        }

        // Compatibility check — skip on blocking issues.
        if let Some(requires) = super::get_requires(&unit.content) {
            let issues = super::check_compatibility(requires);
            if issues.iter().any(|i| i.is_blocking()) {
                continue;
            }
        }

        // Trust gate.
        let tier = trust_store
            .get(&unit.source_peer)
            .map(|t| t.tier())
            .unwrap_or(super::trust::TrustTier::Suggested);

        let allow = match mode {
            AcceptMode::Manual => false,
            AcceptMode::Trusted => tier == super::trust::TrustTier::Confirmed,
            AcceptMode::All => tier != super::trust::TrustTier::Ignored,
        };
        if !allow {
            continue;
        }

        match write_artifact_files(unit, &dir) {
            Ok(Some(_)) => {
                tracker.record(&unit.id, &unit.source_peer, mode);
                installed += 1;
            }
            Ok(None) => {}
            Err(e) => {
                crate::logger::log("HIVE", &format!("auto-accept failed for {}: {e}", unit.id));
            }
        }
    }

    if installed > 0 {
        let _ = tracker.save();
    }
    installed
}
