#![allow(dead_code)]

pub mod archive;
pub mod cli;
pub mod distiller;
pub mod gossip;
pub mod injection;
pub mod merger;
pub mod store;
pub mod trust;

use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

// ────────────────────────────────────────────────────────────────────────────
// Knowledge unit — the atom of shared learning
// ────────────────────────────────────────────────────────────────────────────

/// What kind of knowledge this is — determines whether it should be shared.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KnowledgeCategory {
    /// Tool approval patterns, error handling, safety rules — universal best practices.
    BestPractice,
    /// Instruction scaffolding, prompt patterns, planning vs execution strategies.
    Technique,
    /// Model selection, delegation patterns, agent orchestration choices.
    WorkflowPattern,
    /// Time-of-day habits, approval speed, cost tolerance — personal operating style.
    Personal,
}

impl KnowledgeCategory {
    /// Whether this category should be shared with peers by default.
    pub fn is_shareable(&self) -> bool {
        !matches!(self, Self::Personal)
    }

    /// Whether this category is allowed by a user's share_categories config.
    /// Empty allow_list = share all shareable categories.
    pub fn is_allowed_by(&self, allow_list: &[String]) -> bool {
        if !self.is_shareable() {
            return false;
        }
        if allow_list.is_empty() {
            return true; // empty = no restriction
        }
        allow_list.iter().any(|s| s == self.label())
    }

    pub fn label(&self) -> &'static str {
        match self {
            Self::BestPractice => "best_practice",
            Self::Technique => "technique",
            Self::WorkflowPattern => "workflow",
            Self::Personal => "personal",
        }
    }
}

/// A single piece of shareable knowledge derived from brain distillation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KnowledgeUnit {
    /// Unique ID: `ku_{epoch}_{counter}`
    pub id: String,
    /// What scope this knowledge applies to.
    pub scope: KnowledgeScope,
    /// What kind of knowledge — determines shareability.
    #[serde(default = "default_category")]
    pub category: KnowledgeCategory,
    /// The type and content of the knowledge.
    pub content: KnowledgeContent,
    /// How many local decisions back this knowledge.
    pub evidence_count: u32,
    /// Distillation confidence (0.0 to 1.0).
    pub confidence: f64,
    /// Which peer originated this knowledge.
    pub source_peer: String,
    /// When first created (epoch secs).
    pub originated_at: u64,
    /// When last validated by the originator (epoch secs).
    pub last_validated_at: u64,
    /// How many peers have accepted this knowledge.
    pub propagation_count: u32,
    /// Monotonic version — incremented when the originator updates this unit.
    pub version: u32,
}

fn default_category() -> KnowledgeCategory {
    KnowledgeCategory::BestPractice
}

/// Scope determines where knowledge applies.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "type", content = "value")]
pub enum KnowledgeScope {
    /// Applies to all projects and languages.
    Universal,
    /// Applies to a specific programming language.
    Language(String),
    /// Applies to a specific project (by slug).
    Project(String),
}

/// The actual knowledge payload.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum KnowledgeContent {
    /// A distilled preference pattern (from PreferencePattern).
    Pattern {
        tool: String,
        command_pattern: Option<String>,
        preferred_action: String,
        accept_rate: f64,
        sample_count: u32,
        conditions: Vec<String>,
    },
    /// Per-tool accuracy statistics (from ToolAccuracy).
    ToolAccuracy {
        tool: String,
        total: u32,
        correct: u32,
        confidence_threshold: f64,
    },
    /// A temporal behavior pattern (from TemporalPattern).
    Temporal { description: String, strength: f64 },
    /// A detected friction/error/cost insight (from Insight).
    Insight {
        category: String,
        severity: String,
        summary: String,
        suggestion: Option<String>,
    },
    /// A promoted rule from coord memory.
    PromotedRule { rule: String, source_type: String },
}

// ────────────────────────────────────────────────────────────────────────────
// Semantic key — used for dedup and merge
// ────────────────────────────────────────────────────────────────────────────

/// Truncate a string to at most `max_chars` characters, safe for multi-byte UTF-8.
fn truncate_chars(s: &str, max_chars: usize) -> &str {
    match s.char_indices().nth(max_chars) {
        Some((idx, _)) => &s[..idx],
        None => s,
    }
}

/// Compute a semantic key for dedup/merge.
/// Same knowledge from different peers produces the same semantic key.
pub fn semantic_key(unit: &KnowledgeUnit) -> String {
    let scope_part = match &unit.scope {
        KnowledgeScope::Universal => "universal".to_string(),
        KnowledgeScope::Language(l) => format!("lang:{l}"),
        KnowledgeScope::Project(p) => format!("proj:{p}"),
    };
    let content_part = match &unit.content {
        KnowledgeContent::Pattern {
            tool,
            command_pattern,
            ..
        } => {
            let cmd = command_pattern.as_deref().unwrap_or("*");
            format!("pattern:{tool}:{cmd}")
        }
        KnowledgeContent::ToolAccuracy { tool, .. } => format!("accuracy:{tool}"),
        KnowledgeContent::Temporal { description, .. } => {
            format!("temporal:{}", truncate_chars(description, 40))
        }
        KnowledgeContent::Insight {
            category, summary, ..
        } => {
            format!("insight:{category}:{}", truncate_chars(summary, 40))
        }
        KnowledgeContent::PromotedRule { rule, .. } => {
            format!("rule:{}", truncate_chars(rule, 40))
        }
    };
    format!("{scope_part}/{content_part}")
}

// ────────────────────────────────────────────────────────────────────────────
// Sharing filter — user-controlled exclusions
// ────────────────────────────────────────────────────────────────────────────

/// User-configurable filter for what knowledge to share.
/// Built from HiveConfig's share_categories, exclude_tools, exclude_commands.
#[derive(Debug, Clone, Default)]
pub struct SharingFilter {
    /// Allowed categories (empty = all shareable).
    pub allow_categories: Vec<String>,
    /// Tools to exclude (exact match on tool name).
    pub exclude_tools: Vec<String>,
    /// Command substrings to exclude.
    pub exclude_commands: Vec<String>,
}

impl SharingFilter {
    /// Build from HiveConfig.
    pub fn from_config(cfg: &crate::config::HiveConfig) -> Self {
        SharingFilter {
            allow_categories: cfg.share_categories.clone(),
            exclude_tools: cfg.exclude_tools.clone(),
            exclude_commands: cfg.exclude_commands.clone(),
        }
    }

    /// Check if a knowledge unit passes the user's sharing filter.
    pub fn allows(&self, unit: &KnowledgeUnit) -> bool {
        // Category check
        if !unit.category.is_allowed_by(&self.allow_categories) {
            return false;
        }

        // Tool exclusion
        if let KnowledgeContent::Pattern {
            ref tool,
            ref command_pattern,
            ..
        } = unit.content
        {
            if self.exclude_tools.iter().any(|t| t == tool) {
                return false;
            }
            if let Some(cmd) = command_pattern {
                if self
                    .exclude_commands
                    .iter()
                    .any(|exc| cmd.contains(exc.as_str()))
                {
                    return false;
                }
            }
        }

        // ToolAccuracy tool exclusion
        if let KnowledgeContent::ToolAccuracy { ref tool, .. } = unit.content {
            if self.exclude_tools.iter().any(|t| t == tool) {
                return false;
            }
        }

        true
    }
}

// ────────────────────────────────────────────────────────────────────────────
// ID generation
// ────────────────────────────────────────────────────────────────────────────

static KU_COUNTER: AtomicU64 = AtomicU64::new(0);

pub fn gen_ku_id() -> String {
    let epoch = epoch_secs();
    let seq = KU_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("ku_{epoch}_{seq}")
}

pub fn epoch_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

// ────────────────────────────────────────────────────────────────────────────
// Broadcast channel for triggering gossip after distillation
// ────────────────────────────────────────────────────────────────────────────

use std::sync::Mutex;

/// Global sender for signaling the relay that new knowledge is available.
/// Set once during relay startup; the distillation thread sends a unit count through it.
/// Uses Mutex because mpsc::Sender is Send but not Sync — the Mutex is only contended
/// during initialization and the rare distillation cycle (every 10 decisions).
static HIVE_BROADCAST_TX: Mutex<Option<std::sync::mpsc::Sender<u32>>> = Mutex::new(None);

/// Set the broadcast channel (called once during relay/TUI startup).
pub fn set_broadcast_channel(tx: std::sync::mpsc::Sender<u32>) {
    if let Ok(mut guard) = HIVE_BROADCAST_TX.lock() {
        *guard = Some(tx);
    }
}

/// Signal that new knowledge units are available for gossip.
/// Called from the distillation background thread.
pub fn signal_new_knowledge(count: u32) {
    if let Ok(guard) = HIVE_BROADCAST_TX.lock() {
        if let Some(ref tx) = *guard {
            let _ = tx.send(count);
        }
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Display helpers
// ────────────────────────────────────────────────────────────────────────────

impl KnowledgeContent {
    /// One-line summary for display.
    pub fn summary_line(&self) -> String {
        match self {
            Self::Pattern {
                tool,
                command_pattern,
                preferred_action,
                accept_rate,
                ..
            } => {
                let cmd = command_pattern.as_deref().unwrap_or("*");
                format!(
                    "[{tool}, {cmd}] {preferred_action} ({:.0}%)",
                    accept_rate * 100.0
                )
            }
            Self::ToolAccuracy {
                tool,
                total,
                correct,
                ..
            } => {
                let pct = if *total > 0 {
                    (*correct as f64 / *total as f64) * 100.0
                } else {
                    0.0
                };
                format!("[{tool}] accuracy {correct}/{total} ({pct:.0}%)")
            }
            Self::Temporal {
                description,
                strength,
                ..
            } => {
                format!("temporal: {description} (strength {strength:.2})")
            }
            Self::Insight {
                category,
                severity,
                summary,
                ..
            } => {
                format!("[{severity}] {category}: {summary}")
            }
            Self::PromotedRule { rule, .. } => {
                format!("rule: {rule}")
            }
        }
    }
}

impl std::fmt::Display for KnowledgeScope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Universal => write!(f, "universal"),
            Self::Language(l) => write!(f, "language:{l}"),
            Self::Project(p) => write!(f, "project:{p}"),
        }
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Tests
// ────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_pattern_unit(tool: &str, cmd: Option<&str>) -> KnowledgeUnit {
        KnowledgeUnit {
            id: "ku_1".into(),
            scope: KnowledgeScope::Universal,
            category: KnowledgeCategory::BestPractice,
            content: KnowledgeContent::Pattern {
                tool: tool.into(),
                command_pattern: cmd.map(|s| s.into()),
                preferred_action: "approve".into(),
                accept_rate: 0.95,
                sample_count: 20,
                conditions: vec![],
            },
            evidence_count: 20,
            confidence: 0.95,
            source_peer: "peer-a".into(),
            originated_at: 1000,
            last_validated_at: 2000,
            propagation_count: 0,
            version: 1,
        }
    }

    #[test]
    fn semantic_key_pattern() {
        let unit = sample_pattern_unit("Bash", Some("cargo test"));
        assert_eq!(semantic_key(&unit), "universal/pattern:Bash:cargo test");
    }

    #[test]
    fn semantic_key_pattern_no_command() {
        let unit = sample_pattern_unit("Read", None);
        assert_eq!(semantic_key(&unit), "universal/pattern:Read:*");
    }

    #[test]
    fn semantic_key_with_scope() {
        let mut unit = sample_pattern_unit("Bash", Some("cargo fmt"));
        unit.scope = KnowledgeScope::Language("rust".into());
        assert_eq!(semantic_key(&unit), "lang:rust/pattern:Bash:cargo fmt");

        unit.scope = KnowledgeScope::Project("claudectl".into());
        assert_eq!(semantic_key(&unit), "proj:claudectl/pattern:Bash:cargo fmt");
    }

    #[test]
    fn semantic_key_accuracy() {
        let unit = KnowledgeUnit {
            id: "ku_2".into(),
            scope: KnowledgeScope::Universal,
            category: KnowledgeCategory::BestPractice,
            content: KnowledgeContent::ToolAccuracy {
                tool: "Bash".into(),
                total: 100,
                correct: 85,
                confidence_threshold: 0.7,
            },
            evidence_count: 100,
            confidence: 0.85,
            source_peer: "peer-a".into(),
            originated_at: 1000,
            last_validated_at: 2000,
            propagation_count: 0,
            version: 1,
        };
        assert_eq!(semantic_key(&unit), "universal/accuracy:Bash");
    }

    #[test]
    fn knowledge_unit_serde_roundtrip() {
        let unit = sample_pattern_unit("Bash", Some("cargo test"));
        let json = serde_json::to_string(&unit).unwrap();
        let back: KnowledgeUnit = serde_json::from_str(&json).unwrap();
        assert_eq!(back.id, "ku_1");
        assert_eq!(back.confidence, 0.95);
        assert_eq!(back.source_peer, "peer-a");
    }

    #[test]
    fn content_summary_line() {
        let unit = sample_pattern_unit("Bash", Some("cargo test"));
        let line = unit.content.summary_line();
        assert!(line.contains("Bash"));
        assert!(line.contains("cargo test"));
        assert!(line.contains("approve"));
    }

    #[test]
    fn scope_display() {
        assert_eq!(KnowledgeScope::Universal.to_string(), "universal");
        assert_eq!(
            KnowledgeScope::Language("rust".into()).to_string(),
            "language:rust"
        );
        assert_eq!(
            KnowledgeScope::Project("foo".into()).to_string(),
            "project:foo"
        );
    }

    #[test]
    fn gen_ku_id_unique() {
        let a = gen_ku_id();
        let b = gen_ku_id();
        assert_ne!(a, b);
        assert!(a.starts_with("ku_"));
    }

    #[test]
    fn semantic_key_multibyte_utf8_no_panic() {
        // CJK characters are 3 bytes each — 14 chars = 42 bytes, truncation at
        // char boundary 40 would panic with byte-offset slicing
        let long_cjk = "这是一个用来测试多字节截断的临时模式描述文本超长";
        let unit = KnowledgeUnit {
            id: "ku_utf8".into(),
            scope: KnowledgeScope::Universal,
            category: KnowledgeCategory::BestPractice,
            content: KnowledgeContent::Temporal {
                description: long_cjk.to_string(),
                strength: 0.9,
            },
            evidence_count: 5,
            confidence: 0.9,
            source_peer: "peer-a".into(),
            originated_at: 1000,
            last_validated_at: 2000,
            propagation_count: 0,
            version: 1,
        };
        // Must not panic — truncates at char boundary, not byte boundary
        let key = semantic_key(&unit);
        assert!(key.starts_with("universal/temporal:"));
    }

    #[test]
    fn semantic_key_emoji_no_panic() {
        let emoji_text = "Error streak detected in tests 🎉🎊🎈🎁🎆🎇 more text here";
        let unit = KnowledgeUnit {
            id: "ku_emoji".into(),
            scope: KnowledgeScope::Universal,
            category: KnowledgeCategory::BestPractice,
            content: KnowledgeContent::Insight {
                category: "error_loop".into(),
                severity: "warning".into(),
                summary: emoji_text.to_string(),
                suggestion: None,
            },
            evidence_count: 3,
            confidence: 0.7,
            source_peer: "peer-b".into(),
            originated_at: 1000,
            last_validated_at: 2000,
            propagation_count: 0,
            version: 1,
        };
        let key = semantic_key(&unit);
        assert!(key.starts_with("universal/insight:error_loop:"));
    }
}
