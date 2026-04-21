#![allow(dead_code)]

use crate::brain::preferences::{DistilledPreferences, PreferencePattern};

use super::store;
use super::types::{MemoryRecord, Subject};

/// Minimum confidence to promote a pattern into coordination memory.
const MIN_CONFIDENCE: f64 = 0.80;
/// Minimum sample count to promote a pattern.
const MIN_SAMPLES: u32 = 5;

/// Promote high-confidence patterns from distilled preferences into coordination memory.
/// Returns the number of memory records created or updated.
pub fn promote_from_preferences(
    project: &str,
    prefs: &DistilledPreferences,
) -> Result<u32, String> {
    let conn = store::open()?;
    let now = crate::logger::timestamp_now();
    let mut count = 0u32;

    for pattern in &prefs.patterns {
        if !should_promote(pattern) {
            continue;
        }

        let record = pattern_to_memory(project, pattern, &now);
        store::insert_memory(&conn, &record)?;

        let event = super::types::CoordEvent {
            id: None,
            event_type: super::types::EventType::MemoryWritten,
            timestamp: now.clone(),
            session_id: None,
            payload: serde_json::json!({
                "memory_id": record.id,
                "mem_type": record.mem_type,
                "project": project,
                "source": "promotion",
            }),
        };
        let _ = store::append_event(&conn, &event);
        count += 1;
    }

    Ok(count)
}

/// Load preferences for a project and promote eligible patterns.
pub fn promote_project(project: &str) -> Result<u32, String> {
    let prefs = crate::brain::preferences::load_preferences_for_project(project)
        .ok_or_else(|| format!("No preferences found for project: {project}"))?;
    promote_from_preferences(project, &prefs)
}

fn should_promote(pattern: &PreferencePattern) -> bool {
    pattern.confidence >= MIN_CONFIDENCE && pattern.sample_count >= MIN_SAMPLES
}

/// Convert a preference pattern to a coordination memory record.
/// Uses a deterministic ID so re-promotion updates existing records.
fn pattern_to_memory(project: &str, pattern: &PreferencePattern, now: &str) -> MemoryRecord {
    let cmd = pattern.command_pattern.as_deref().unwrap_or("*");

    // Deterministic ID: same pattern always maps to same record
    let id = format!(
        "prom_{}_{}_{:x}",
        slug(project),
        slug(&pattern.tool),
        simple_hash(cmd)
    );

    let mem_type =
        if pattern.preferred_action == "approve" || pattern.preferred_action == "auto_execute" {
            "workflow"
        } else {
            "preference"
        };

    let summary = format_pattern_summary(pattern);

    let mut subjects = vec![Subject {
        kind: "tool".into(),
        value: pattern.tool.clone(),
    }];
    if let Some(ref cmd_pat) = pattern.command_pattern {
        subjects.push(Subject {
            kind: "command_pattern".into(),
            value: cmd_pat.clone(),
        });
    }

    let mut tags = vec![
        "promoted".to_string(),
        pattern.tool.to_lowercase(),
        mem_type.to_string(),
    ];
    if let Some(ref cmd_pat) = pattern.command_pattern {
        // Add first word of command as a tag for searchability
        if let Some(first) = cmd_pat.split_whitespace().next() {
            tags.push(first.to_lowercase());
        }
    }

    MemoryRecord {
        id,
        mem_type: mem_type.into(),
        scope: serde_json::json!({"project": project}),
        subjects,
        summary,
        evidence: vec![],
        source: Some(serde_json::json!({
            "kind": "preference_distillation",
            "sample_count": pattern.sample_count,
            "confidence": pattern.confidence,
            "accept_rate": pattern.accept_rate,
        })),
        confidence: pattern.confidence,
        created_at: now.into(),
        updated_at: now.into(),
        expires_at: None,
        tags,
    }
}

fn format_pattern_summary(pattern: &PreferencePattern) -> String {
    let tool = &pattern.tool;
    let action = &pattern.preferred_action;
    let rate = (pattern.accept_rate * 100.0) as u32;

    match &pattern.command_pattern {
        Some(cmd) => format!(
            "{action} `{tool}` commands matching `{cmd}` ({rate}% accept rate, {} samples)",
            pattern.sample_count
        ),
        None => format!(
            "{action} all `{tool}` commands ({rate}% accept rate, {} samples)",
            pattern.sample_count
        ),
    }
}

/// Simple filesystem-safe slug.
fn slug(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect::<String>()
        .to_lowercase()
}

/// Simple non-cryptographic hash for deterministic IDs.
fn simple_hash(s: &str) -> u64 {
    let mut h: u64 = 5381;
    for b in s.bytes() {
        h = h.wrapping_mul(33).wrapping_add(b as u64);
    }
    h
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_pattern(
        tool: &str,
        cmd: Option<&str>,
        confidence: f64,
        samples: u32,
    ) -> PreferencePattern {
        PreferencePattern {
            tool: tool.into(),
            command_pattern: cmd.map(|s| s.into()),
            preferred_action: "approve".into(),
            sample_count: samples,
            accept_rate: 0.95,
            conditions: vec![],
            confidence,
        }
    }

    #[test]
    fn should_promote_checks_thresholds() {
        let high = make_pattern("Bash", Some("cargo test"), 0.90, 10);
        assert!(should_promote(&high));

        let low_conf = make_pattern("Bash", Some("rm -rf"), 0.50, 10);
        assert!(!should_promote(&low_conf));

        let low_samples = make_pattern("Bash", Some("cargo build"), 0.95, 3);
        assert!(!should_promote(&low_samples));
    }

    #[test]
    fn pattern_to_memory_deterministic_id() {
        let pattern = make_pattern("Bash", Some("cargo test"), 0.90, 10);
        let m1 = pattern_to_memory("myproject", &pattern, "2026-04-20T10:00:00Z");
        let m2 = pattern_to_memory("myproject", &pattern, "2026-04-20T11:00:00Z");
        // Same pattern, same project -> same ID (deterministic)
        assert_eq!(m1.id, m2.id);
        // Different timestamp should update, not duplicate
        assert_eq!(m2.updated_at, "2026-04-20T11:00:00Z");
    }

    #[test]
    fn pattern_to_memory_has_correct_type() {
        let approve = make_pattern("Read", None, 0.85, 8);
        let m = pattern_to_memory("proj", &approve, "2026-04-20T10:00:00Z");
        assert_eq!(m.mem_type, "workflow");

        let mut deny = make_pattern("Bash", Some("rm -rf /"), 0.90, 6);
        deny.preferred_action = "deny".into();
        let m = pattern_to_memory("proj", &deny, "2026-04-20T10:00:00Z");
        assert_eq!(m.mem_type, "preference");
    }

    #[test]
    fn format_pattern_summary_with_command() {
        let p = make_pattern("Bash", Some("cargo test"), 0.90, 10);
        let s = format_pattern_summary(&p);
        assert!(s.contains("cargo test"));
        assert!(s.contains("95%"));
        assert!(s.contains("10 samples"));
    }

    #[test]
    fn format_pattern_summary_without_command() {
        let p = make_pattern("Read", None, 0.85, 8);
        let s = format_pattern_summary(&p);
        assert!(s.contains("all `Read` commands"));
    }

    #[test]
    fn slug_is_filesystem_safe() {
        assert_eq!(slug("my-project"), "my-project");
        assert_eq!(slug("src/app.rs"), "src_app_rs");
        assert_eq!(slug("Hello World!"), "hello_world_");
    }

    #[test]
    fn simple_hash_is_deterministic() {
        assert_eq!(simple_hash("cargo test"), simple_hash("cargo test"));
        assert_ne!(simple_hash("cargo test"), simple_hash("cargo build"));
    }
}
