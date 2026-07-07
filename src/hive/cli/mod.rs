// CLI dispatch for hive subcommands.

use std::io;

use clap::Subcommand;

use crate::hive::KnowledgeScope;
use crate::hive::store::HiveStore;

// Behavior-preserving split of the former monolithic cli.rs. Command handlers
// grouped by concern; dispatch_command below routes to them.
mod effectiveness;
mod onboarding;
mod share;
use effectiveness::*;
use onboarding::*;
pub use share::share_artifact_from_path;
use share::*;

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
        #[arg(long, default_value_t = crate::hive::effectiveness::DEAD_WEIGHT_MIN_INJECTED)]
        min_injected: u64,
        /// Maximum decided outcomes (≤ this counts as dead-weight)
        #[arg(long, default_value_t = crate::hive::effectiveness::DEAD_WEIGHT_MAX_DECIDED)]
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

// ────────────────────────────────────────────────────────────────────────────
// Discovery commands (#227)
// ────────────────────────────────────────────────────────────────────────────

// ────────────────────────────────────────────────────────────────────────────
// Effectiveness commands (#225)
// ────────────────────────────────────────────────────────────────────────────

pub(crate) fn truncate_col_cli(s: &str, width: usize) -> String {
    if s.chars().count() <= width {
        s.to_string()
    } else {
        let truncated: String = s.chars().take(width.saturating_sub(1)).collect();
        format!("{truncated}…")
    }
}

/// `claudectl hive on|off`
fn cmd_set_mode(mode: &str, json_mode: bool) -> io::Result<()> {
    crate::hive::write_mode_override(mode)?;
    let cfg = crate::config::Config::load();
    let active = crate::hive::is_active(cfg.hive.as_ref());

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
    let active = crate::hive::is_active(cfg.hive.as_ref());

    let mode = crate::hive::exposure::ShareMode::parse(&hive_cfg.share_mode)
        .unwrap_or(crate::hive::exposure::ShareMode::Auto);
    let exposure = crate::hive::exposure::ExposureStore::load();
    let filter = crate::hive::SharingFilter::from_config(&hive_cfg);

    #[cfg(feature = "relay")]
    let local_id = crate::relay::load_or_create_identity().0;
    #[cfg(not(feature = "relay"))]
    let local_id = crate::hive::local_identity();

    // Partition local units into ready / hidden / blocked
    let mut ready = Vec::new();
    let mut hidden = Vec::new();
    let mut blocked: Vec<(&crate::hive::KnowledgeUnit, &'static str)> = Vec::new();

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
        let age = crate::hive::epoch_secs().saturating_sub(unit.last_validated_at);
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
        let to_json = |units: &[&crate::hive::KnowledgeUnit]| -> Vec<serde_json::Value> {
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

fn print_preview_row(u: &crate::hive::KnowledgeUnit) {
    let id_short = short_id(&u.id);
    println!(
        "  {id_short}  {:<10}  {}",
        u.category.label(),
        u.content.summary_line()
    );
}

pub(crate) fn short_id(id: &str) -> &str {
    if id.len() > 12 { &id[..12] } else { id }
}

/// `claudectl hive expose [unit_id] [--all]`
fn cmd_expose(unit_id: Option<&str>, all: bool, json_mode: bool) -> io::Result<()> {
    apply_exposure(
        unit_id,
        all,
        crate::hive::exposure::ExposureState::Expose,
        json_mode,
    )
}

/// `claudectl hive hide [unit_id] [--all]`
fn cmd_hide(unit_id: Option<&str>, all: bool, json_mode: bool) -> io::Result<()> {
    apply_exposure(
        unit_id,
        all,
        crate::hive::exposure::ExposureState::Hide,
        json_mode,
    )
}

fn apply_exposure(
    unit_id: Option<&str>,
    all: bool,
    state: crate::hive::exposure::ExposureState,
    json_mode: bool,
) -> io::Result<()> {
    if unit_id.is_none() && !all {
        return Err(io::Error::other("specify a unit ID or --all".to_string()));
    }

    let store = HiveStore::load();
    let mut exposure = crate::hive::exposure::ExposureStore::load();

    #[cfg(feature = "relay")]
    let local_id = crate::relay::load_or_create_identity().0;
    #[cfg(not(feature = "relay"))]
    let local_id = crate::hive::local_identity();

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
        crate::hive::exposure::ExposureState::Expose => "exposed",
        crate::hive::exposure::ExposureState::Hide => "hidden",
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
    let active = crate::hive::is_active(cfg.hive.as_ref());
    let mode_override = crate::hive::read_mode_override();

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
            let gossip = crate::hive::gossip::GossipEngine::new(id.as_str(), 5, 30);
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
            let gossip = crate::hive::gossip::GossipEngine::new(id.as_str(), 5, 30);
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

    let mut units: Vec<&crate::hive::KnowledgeUnit> = if let Some(from) = from_filter {
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
    let mut trust_store = crate::hive::trust::TrustStore::load();

    match (peer, level) {
        (None, _) => {
            // Show all peer trust levels
            let all = trust_store.all();
            if json_mode {
                let peers: Vec<&crate::hive::trust::PeerTrust> = all;
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
pub(crate) fn parse_scope(s: &str) -> KnowledgeScope {
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

// ────────────────────────────────────────────────────────────────────────────
// Share, Install, Shared commands
// ────────────────────────────────────────────────────────────────────────────

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
        let pruned = crate::hive::archive::prune_archive(days)
            .map_err(|e| io::Error::other(format!("prune: {e}")))?;
        println!("Pruned {pruned} archive entries older than {days} days.");
        return Ok(());
    }

    let count = crate::hive::archive::archive_count();
    let size = crate::hive::archive::archive_size_bytes();
    let meta = crate::hive::archive::load_curriculum_meta();

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
    let report = crate::hive::archive::distill_archive()
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
    let curriculum = crate::hive::archive::load_curriculum();
    let meta = crate::hive::archive::load_curriculum_meta();

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
    use crate::hive::accept::AcceptMode;

    match mode {
        None => {
            let current = crate::hive::accept::read_mode(AcceptMode::Manual);
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
            crate::hive::accept::write_mode(parsed)?;
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
    let trust_store = crate::hive::trust::TrustStore::load();
    let tracker = crate::hive::accept::InstalledTracker::load();

    let pending: Vec<(&crate::hive::KnowledgeUnit, crate::hive::trust::TrustTier)> = store
        .all_units()
        .into_iter()
        .filter_map(|unit| {
            let type_label = match &unit.content {
                crate::hive::KnowledgeContent::Skill { .. } => "skill",
                crate::hive::KnowledgeContent::Command { .. } => "command",
                crate::hive::KnowledgeContent::HookConfig { .. } => "hook",
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
                .unwrap_or(crate::hive::trust::TrustTier::Suggested);
            if tier == crate::hive::trust::TrustTier::Ignored {
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
    units: &[crate::hive::KnowledgeUnit],
    base_dir: Option<&std::path::Path>,
) -> usize {
    use crate::hive::accept::AcceptMode;

    let mode = crate::hive::accept::read_mode(AcceptMode::Manual);
    if mode == AcceptMode::Manual {
        return 0;
    }

    let trust_store = crate::hive::trust::TrustStore::load();
    let mut tracker = crate::hive::accept::InstalledTracker::load();
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
            crate::hive::KnowledgeContent::Skill { .. }
                | crate::hive::KnowledgeContent::Command { .. }
        );
        if !is_artifact {
            continue;
        }

        // Compatibility check — skip on blocking issues.
        if let Some(requires) = crate::hive::get_requires(&unit.content) {
            let issues = crate::hive::check_compatibility(requires);
            if issues.iter().any(|i| i.is_blocking()) {
                continue;
            }
        }

        // Trust gate.
        let tier = trust_store
            .get(&unit.source_peer)
            .map(|t| t.tier())
            .unwrap_or(crate::hive::trust::TrustTier::Suggested);

        let allow = match mode {
            AcceptMode::Manual => false,
            AcceptMode::Trusted => tier == crate::hive::trust::TrustTier::Confirmed,
            AcceptMode::All => tier != crate::hive::trust::TrustTier::Ignored,
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
