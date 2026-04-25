// CLI dispatch for hive subcommands.

use std::io;

use clap::Subcommand;

use super::KnowledgeScope;
use super::store::HiveStore;

#[derive(Subcommand)]
pub enum HiveCommand {
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
}

/// Dispatch a hive subcommand.
pub fn dispatch_command(command: &HiveCommand, json_mode: bool) -> io::Result<()> {
    match command {
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
    }
}

/// `claudectl hive status`
fn cmd_status(json_mode: bool) -> io::Result<()> {
    let store = HiveStore::load();
    let all = store.all_units();
    let cfg = crate::config::Config::load();
    let hive_cfg = cfg.hive.unwrap_or_default();

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
        println!("Hive Knowledge Store");
        println!();
        if let Some(ref id) = relay_identity {
            println!("  Identity: {}", id);
        }
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
            "{:<12} {:<15} {:<8} {:<6} CONTENT",
            "ID", "SCOPE", "CONF", "EVID"
        );
        println!("{}", "─".repeat(80));
        for unit in &units {
            let id_short = if unit.id.len() > 11 {
                &unit.id[..11]
            } else {
                &unit.id
            };
            println!(
                "{:<12} {:<15} {:<8.0}% {:<6} {}",
                id_short,
                unit.scope.to_string(),
                unit.confidence * 100.0,
                unit.evidence_count,
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
