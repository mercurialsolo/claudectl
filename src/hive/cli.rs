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
        None => {
            let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
            std::path::PathBuf::from(home).join(".claude")
        }
    };

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

            if tier == super::trust::TrustTier::Unverified {
                eprintln!(
                    "Warning: source peer '{}' is unverified (trust < 0.5). \
                     Review the installed skill before relying on it.",
                    unit.source_peer
                );
            }

            if json_mode {
                let output = serde_json::json!({
                    "action": "installed",
                    "content_type": "skill",
                    "name": name,
                    "version": version,
                    "path": file_path.display().to_string(),
                    "trust_tier": tier.label(),
                });
                println!("{}", serde_json::to_string_pretty(&output).unwrap());
            } else {
                println!(
                    "Installed skill '{name}' v{version} to {}",
                    file_path.display()
                );
            }
        }
        super::KnowledgeContent::Command {
            name, body, args, ..
        } => {
            let cmds_dir = base_dir.join("commands");
            std::fs::create_dir_all(&cmds_dir)?;
            let file_path = cmds_dir.join(format!("{name}.md"));
            std::fs::write(&file_path, body)?;

            if tier == super::trust::TrustTier::Unverified {
                eprintln!(
                    "Warning: source peer '{}' is unverified. Review before use.",
                    unit.source_peer
                );
            }

            if json_mode {
                let output = serde_json::json!({
                    "action": "installed",
                    "content_type": "command",
                    "name": name,
                    "args": args,
                    "path": file_path.display().to_string(),
                    "trust_tier": tier.label(),
                });
                println!("{}", serde_json::to_string_pretty(&output).unwrap());
            } else {
                println!("Installed command '/{name}' to {}", file_path.display());
            }
        }
        super::KnowledgeContent::HookConfig {
            event,
            matcher,
            description,
            config_json,
            ..
        } => {
            if tier == super::trust::TrustTier::Unverified {
                eprintln!(
                    "Warning: source peer '{}' is unverified. Review the hook config carefully.",
                    unit.source_peer
                );
            }

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
        _ => {
            return Err(io::Error::other(format!(
                "unit {unit_id} is not a skill, command, or hook"
            )));
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
