// CLI dispatch for hive subcommands.

use std::io;

use super::KnowledgeScope;
use super::store::HiveStore;

/// Dispatch a hive subcommand.
pub fn dispatch(subcommand: &str, _json_mode: bool) -> io::Result<()> {
    let parts: Vec<&str> = subcommand.split_whitespace().collect();
    match parts.first().copied() {
        Some("status") => cmd_status(_json_mode),
        Some("knowledge") => cmd_knowledge(&parts[1..], _json_mode),
        Some("export") => cmd_export(),
        Some("import") => cmd_import(&parts[1..]),
        Some("forget") => cmd_forget(&parts[1..]),
        Some("trust") => cmd_trust(&parts[1..], _json_mode),
        Some("archive") => cmd_archive(&parts[1..], _json_mode),
        Some("distill") => cmd_distill(_json_mode),
        Some("curriculum") => cmd_curriculum(_json_mode),
        Some(other) => {
            eprintln!("Unknown hive subcommand: {other}");
            print_hive_help();
            Err(io::Error::other("unknown subcommand"))
        }
        None => {
            print_hive_help();
            Ok(())
        }
    }
}

fn print_hive_help() {
    eprintln!("Usage: claudectl --hive <subcommand>");
    eprintln!();
    eprintln!("Knowledge:");
    eprintln!("  status                       Show knowledge store overview");
    eprintln!("  knowledge [--from P] [--scope S]  List knowledge units");
    eprintln!("  export                       Export all knowledge as JSON");
    eprintln!("  import <file>                Import knowledge from JSON");
    eprintln!("  forget <unit-id>             Remove a knowledge unit");
    eprintln!();
    eprintln!("Trust:");
    eprintln!("  trust                        Show all peer trust levels");
    eprintln!("  trust <peer>                 Show trust for one peer");
    eprintln!("  trust <peer> <0.0-1.0>       Set trust level");
    eprintln!();
    eprintln!("Archive & Distillation:");
    eprintln!("  archive                      Show cold storage stats");
    eprintln!("  archive --prune <Nd>         Prune entries older than N days");
    eprintln!("  distill                      Run distillation pipeline");
    eprintln!("  curriculum                   Show distilled curriculum");
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

    // Load gossip sync state
    let local_id = crate::relay::load_or_create_identity();
    let gossip = super::gossip::GossipEngine::new(local_id.as_str(), 5, 30);
    let sync_states = gossip.all_sync_states();

    if json_mode {
        let output = serde_json::json!({
            "identity": local_id.as_str(),
            "total_units": all.len(),
            "max_units": hive_cfg.max_units,
            "sources": sources,
            "categories": by_category,
            "conflicts": conflict_count,
            "sync_states": sync_states,
        });
        println!("{}", serde_json::to_string_pretty(&output).unwrap());
    } else {
        println!("Hive Knowledge Store");
        println!();
        println!("  Identity: {}", local_id);
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

/// `claudectl hive knowledge [--scope X] [--from peer] [--json]`
fn cmd_knowledge(args: &[&str], json_mode: bool) -> io::Result<()> {
    let store = HiveStore::load();
    let mut scope_filter: Option<String> = None;
    let mut from_filter: Option<String> = None;

    let mut i = 0;
    while i < args.len() {
        if args[i] == "--scope" {
            i += 1;
            scope_filter = args.get(i).map(|s| s.to_string());
        } else if args[i] == "--from" {
            i += 1;
            from_filter = args.get(i).map(|s| s.to_string());
        }
        i += 1;
    }

    let mut units: Vec<&super::KnowledgeUnit> = if let Some(ref from) = from_filter {
        store.by_source(from)
    } else if let Some(ref scope_str) = scope_filter {
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
fn cmd_import(args: &[&str]) -> io::Result<()> {
    if args.is_empty() {
        eprintln!("Usage: claudectl --hive \"import <file>\"");
        return Err(io::Error::other("missing file path"));
    }

    let path = args[0];
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
fn cmd_forget(args: &[&str]) -> io::Result<()> {
    if args.is_empty() {
        eprintln!("Usage: claudectl --hive \"forget <unit-id>\"");
        return Err(io::Error::other("missing unit id"));
    }

    let unit_id = args[0];
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
fn cmd_trust(args: &[&str], json_mode: bool) -> io::Result<()> {
    let mut trust_store = super::trust::TrustStore::load();

    match args.len() {
        0 => {
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
        1 => {
            // Show trust for a specific peer
            let peer_id = args[0];
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
        _ => {
            // Set trust level
            let peer_id = args[0];
            let level: f64 = args[1]
                .parse()
                .map_err(|_| io::Error::other("invalid trust level (must be 0.0-1.0)"))?;
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
fn cmd_archive(args: &[&str], json_mode: bool) -> io::Result<()> {
    let mut prune_days: Option<u32> = None;
    let mut i = 0;
    while i < args.len() {
        if args[i] == "--prune" {
            i += 1;
            if let Some(val) = args.get(i) {
                let days_str = val.trim_end_matches('d');
                prune_days = days_str.parse().ok();
            }
        }
        i += 1;
    }

    if let Some(days) = prune_days {
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
            println!("  No curriculum yet. Run: claudectl --hive distill");
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
        println!("No curriculum yet. Run: claudectl --hive distill");
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
