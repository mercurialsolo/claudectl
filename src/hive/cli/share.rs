//! Extracted from hive/cli.rs — behavior-preserving split.

use super::*;
use crate::hive::store::HiveStore;
use std::io;

/// `claudectl hive share <type> <path> [--scope X]`
pub(crate) fn cmd_share(
    content_type: &str,
    path: &str,
    scope_str: &str,
    json_mode: bool,
) -> io::Result<()> {
    let (unit_id, summary) = share_inner(content_type, path, scope_str)?;
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

pub(crate) fn share_inner(
    content_type: &str,
    path: &str,
    scope_str: &str,
) -> io::Result<(String, String)> {
    let body =
        std::fs::read_to_string(path).map_err(|e| io::Error::other(format!("read {path}: {e}")))?;

    let scope = parse_scope(scope_str);
    let identity = crate::hive::local_identity();
    let now = crate::hive::epoch_secs();

    let content = match content_type {
        "skill" => {
            if body.len() > crate::hive::MAX_SKILL_BYTES {
                return Err(io::Error::other(format!(
                    "skill body too large: {} bytes (max {})",
                    body.len(),
                    crate::hive::MAX_SKILL_BYTES
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
            crate::hive::KnowledgeContent::Skill {
                name,
                description,
                version,
                body,
                requires,
            }
        }
        "command" => {
            if body.len() > crate::hive::MAX_COMMAND_BYTES {
                return Err(io::Error::other(format!(
                    "command body too large: {} bytes (max {})",
                    body.len(),
                    crate::hive::MAX_COMMAND_BYTES
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
            crate::hive::KnowledgeContent::Command {
                name,
                description,
                args,
                body,
                requires,
            }
        }
        "hook" => {
            if body.len() > crate::hive::MAX_HOOK_CONFIG_BYTES {
                return Err(io::Error::other(format!(
                    "hook config too large: {} bytes (max {})",
                    body.len(),
                    crate::hive::MAX_HOOK_CONFIG_BYTES
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

            let sanitized = crate::hive::sanitize_hook_config(&body);

            // For hooks: extract command binary as a CLI dep
            let mut requires = crate::hive::ArtifactRequires::default();
            if let Some(cmd) = parsed.get("command").and_then(|v| v.as_str()) {
                let binary = cmd.rsplit('/').next().unwrap_or(cmd);
                if !binary.is_empty() {
                    requires.cli.push(binary.to_string());
                }
            }

            crate::hive::KnowledgeContent::HookConfig {
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
        crate::hive::KnowledgeContent::HookConfig { .. } => {
            crate::hive::KnowledgeCategory::WorkflowPattern
        }
        _ => crate::hive::KnowledgeCategory::Technique,
    };

    let unit = crate::hive::KnowledgeUnit {
        id: crate::hive::gen_ku_id(),
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
    crate::hive::signal_new_knowledge(1);

    Ok((unit_id, summary))
}

/// Share a skill, command, or hook from disk into the local hive store.
///
/// Public wrapper around the internal `cmd_share` so callers outside the CLI
/// dispatch (e.g. the TUI) can share artifacts without duplicating the
/// frontmatter/scope plumbing. Returns the new unit ID and its summary line.
pub fn share_artifact_from_path(
    content_type: &str,
    path: &str,
    scope_str: &str,
) -> io::Result<(String, String)> {
    share_inner(content_type, path, scope_str)
}

/// Build `ArtifactRequires` from frontmatter overrides + auto-detection.
/// Frontmatter keys: `requires_cli`, `requires_os`, `requires_min_version`.
pub(crate) fn build_requires(
    fm: &std::collections::HashMap<String, String>,
    body: &str,
) -> crate::hive::ArtifactRequires {
    // Frontmatter overrides take priority
    let cli = if let Some(val) = fm.get("requires_cli") {
        val.split(',').map(|s| s.trim().to_string()).collect()
    } else {
        crate::hive::detect_cli_deps(body)
    };

    let os = if let Some(val) = fm.get("requires_os") {
        val.split(',').map(|s| s.trim().to_string()).collect()
    } else {
        crate::hive::detect_os_deps(body)
    };

    let min_version = fm.get("requires_min_version").cloned();

    crate::hive::ArtifactRequires {
        cli,
        os,
        min_version,
    }
}

/// Parse YAML frontmatter from a markdown file.
/// Returns key-value pairs from the `---`-delimited block.
pub(crate) fn parse_frontmatter(content: &str) -> std::collections::HashMap<String, String> {
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

/// `claudectl hive install <unit_id> [--target dir] [--force]`
pub(crate) fn cmd_install(
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
    let trust_store = crate::hive::trust::TrustStore::load();
    let tier = trust_store
        .get(&unit.source_peer)
        .map(|t| t.tier())
        .unwrap_or(crate::hive::trust::TrustTier::Suggested);

    if tier == crate::hive::trust::TrustTier::Ignored {
        return Err(io::Error::other(format!(
            "source peer '{}' is in Ignored tier (trust < 0.2). \
             Set higher trust first: claudectl hive trust {} 0.5",
            unit.source_peer, unit.source_peer,
        )));
    }

    // Check compatibility
    if let Some(requires) = crate::hive::get_requires(&unit.content) {
        let issues = crate::hive::check_compatibility(requires);
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

    let unverified_warning = if tier == crate::hive::trust::TrustTier::Unverified {
        Some(format!(
            "Warning: source peer '{}' is unverified. Review before use.",
            unit.source_peer
        ))
    } else {
        None
    };

    let mut tracker = crate::hive::accept::InstalledTracker::load();
    let mut record_install = || {
        tracker.record(
            unit_id,
            &unit.source_peer,
            crate::hive::accept::AcceptMode::Manual,
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

/// Write a skill or command to disk. Hooks are returned as config-only —
/// callers decide how to present them. Returns None for non-artifact units.
pub fn write_artifact_files(
    unit: &crate::hive::KnowledgeUnit,
    base_dir: &std::path::Path,
) -> io::Result<Option<InstallOutcome>> {
    match &unit.content {
        crate::hive::KnowledgeContent::Skill {
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
        crate::hive::KnowledgeContent::Command {
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
        crate::hive::KnowledgeContent::HookConfig {
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

/// `claudectl hive shared [--type X] [--show-ignored]`
pub(crate) fn cmd_shared(
    content_type_filter: Option<&str>,
    show_ignored: bool,
    json_mode: bool,
) -> io::Result<()> {
    let store = HiveStore::load();
    let trust_store = crate::hive::trust::TrustStore::load();

    let units: Vec<(&crate::hive::KnowledgeUnit, crate::hive::trust::TrustTier)> = store
        .all_units()
        .into_iter()
        .filter_map(|unit| {
            // Filter to artifact types only
            let type_label = match &unit.content {
                crate::hive::KnowledgeContent::Skill { .. } => "skill",
                crate::hive::KnowledgeContent::Command { .. } => "command",
                crate::hive::KnowledgeContent::HookConfig { .. } => "hook",
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
                .unwrap_or(crate::hive::trust::TrustTier::Suggested);

            // Skip ignored unless requested
            if tier == crate::hive::trust::TrustTier::Ignored && !show_ignored {
                return None;
            }

            Some((unit, tier))
        })
        .collect();

    if json_mode {
        let items: Vec<serde_json::Value> = units
            .iter()
            .map(|(unit, tier)| {
                let compat = crate::hive::compat_label(&unit.content);
                let mut obj = serde_json::json!({
                    "id": unit.id,
                    "type": content_type_label(&unit.content),
                    "name": content_name(&unit.content),
                    "source_peer": unit.source_peer,
                    "trust_tier": tier.label(),
                    "compat": compat,
                    "summary": unit.content.summary_line(),
                });
                if let Some(req) = crate::hive::get_requires(&unit.content) {
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
            let compat = crate::hive::compat_label(&unit.content);
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
pub(crate) fn content_type_label(content: &crate::hive::KnowledgeContent) -> &'static str {
    match content {
        crate::hive::KnowledgeContent::Skill { .. } => "skill",
        crate::hive::KnowledgeContent::Command { .. } => "command",
        crate::hive::KnowledgeContent::HookConfig { .. } => "hook",
        _ => "other",
    }
}

/// Get the name from a content unit.
pub(crate) fn content_name(content: &crate::hive::KnowledgeContent) -> String {
    match content {
        crate::hive::KnowledgeContent::Skill { name, .. } => name.clone(),
        crate::hive::KnowledgeContent::Command { name, .. } => name.clone(),
        crate::hive::KnowledgeContent::HookConfig { event, matcher, .. } => {
            format!("{event}[{matcher}]")
        }
        _ => String::new(),
    }
}
