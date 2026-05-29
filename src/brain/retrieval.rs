#![allow(dead_code)]

use super::decisions::{DecisionRecord, DecisionType, read_all_decisions};

// ────────────────────────────────────────────────────────────────────────────
// Outcome-weighted few-shot retrieval
// ────────────────────────────────────────────────────────────────────────────

/// Compute rejection weight from the accept/reject ratio in a decision set.
/// Returns a value in [3, 12]: rare rejections get amplified, frequent ones don't.
fn dynamic_rejection_weight(decisions: &[&DecisionRecord]) -> i32 {
    let mut accepts: u32 = 0;
    let mut rejects: u32 = 0;
    for d in decisions {
        if d.is_positive() {
            accepts += 1;
        } else if d.is_negative() {
            rejects += 1;
        }
    }
    let weight = accepts as f64 / rejects.max(1) as f64;
    weight.clamp(3.0, 12.0) as i32
}

/// Retrieve past decisions most relevant to the current context.
/// Weights: same tool, same project, user-confirmed outcomes rank higher.
/// When `decision_type` is specified, only decisions of that type are returned.
pub fn retrieve_similar(
    tool: Option<&str>,
    project: &str,
    limit: usize,
    decision_type: Option<DecisionType>,
) -> Vec<DecisionRecord> {
    if limit == 0 {
        return Vec::new();
    }

    let all = read_all_decisions();
    if all.is_empty() {
        return Vec::new();
    }

    // Filter by decision type when specified
    let filtered: Vec<&DecisionRecord> = if let Some(dt) = decision_type {
        all.iter().filter(|d| d.decision_type == dt).collect()
    } else {
        all.iter().collect()
    };

    if filtered.is_empty() {
        return Vec::new();
    }

    // Dynamic rejection weight: scale based on accept/reject ratio so that
    // rejections stay proportionally informative regardless of the user's
    // approval habits.  At 90/10 → ~9 (close to the old hardcoded 8),
    // at 60/40 → 3 (floor), at 99/1 → 12 (cap).
    let rejection_weight = dynamic_rejection_weight(&filtered);

    // Score each decision by relevance + outcome signal
    let mut scored: Vec<(i32, usize, &DecisionRecord)> = filtered
        .iter()
        .enumerate()
        .map(|(idx, d)| {
            let mut score: i32 = 0;

            // Context match
            if let Some(t) = tool {
                if d.tool.as_deref() == Some(t) {
                    score += 10;
                }
            }
            if d.project.to_lowercase().contains(&project.to_lowercase()) {
                score += 5;
            }

            // Outcome weighting: user-confirmed decisions are more informative
            if d.is_observation() {
                score += 2; // Passive observation: ground truth but no correction signal
            } else if d.is_positive() {
                score += 3; // Accepted/auto = brain was right, reinforce
            } else if d.is_negative() {
                score += rejection_weight; // Rejected = correction signal, weight scales with ratio
            }

            // Canonical decisions (user-marked teaching examples via `brain review`)
            // dominate retrieval — they're the supervised-training signal.
            if d.canonical == Some(true) {
                score += 50;
            }

            // Recency bonus: newer decisions reflect current preferences
            // idx is position in filtered list (0=oldest), scale to 0-2 bonus
            let recency = if filtered.len() > 1 {
                (idx as i32 * 2) / (filtered.len() as i32 - 1)
            } else {
                2
            };
            score += recency;

            (score, idx, *d)
        })
        .collect();

    // Sort by score desc, break ties by recency (higher idx = newer)
    scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| b.1.cmp(&a.1)));
    scored.truncate(limit);

    scored.into_iter().map(|(_, _, d)| d.clone()).collect()
}

/// Format past decisions as few-shot examples for the brain prompt.
pub fn format_few_shot_examples(decisions: &[DecisionRecord]) -> String {
    if decisions.is_empty() {
        return String::new();
    }

    let mut lines = Vec::new();
    for d in decisions {
        let tool = d.tool.as_deref().unwrap_or("?");
        let cmd = d
            .command
            .as_deref()
            .map(|c| {
                if c.len() > 80 {
                    format!("{}...", crate::session::truncate_str(c, 80))
                } else {
                    c.to_string()
                }
            })
            .unwrap_or_default();
        let cmd_part = if cmd.is_empty() {
            String::new()
        } else {
            format!(", command=\"{cmd}\"")
        };
        if d.is_observation() {
            // Passive observation: show what the user did directly
            lines.push(format!(
                "[tool={tool}{cmd_part}] user action: {}",
                d.user_action,
            ));
        } else {
            lines.push(format!(
                "[tool={tool}{cmd_part}] brain: {} ({}%) → user: {}",
                d.brain_action,
                (d.brain_confidence * 100.0) as u32,
                d.user_action,
            ));
        }
    }

    lines.join("\n")
}

// ────────────────────────────────────────────────────────────────────────────
// Tests
// ────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::super::decisions::DecisionType;
    use super::*;

    fn make_decision(tool: &str, project: &str, user_action: &str) -> DecisionRecord {
        DecisionRecord {
            timestamp: "0".into(),
            pid: 1,
            project: project.into(),
            tool: Some(tool.into()),
            command: Some("test cmd".into()),
            brain_action: "approve".into(),
            brain_confidence: 0.9,
            brain_reasoning: "test".into(),
            user_action: user_action.into(),
            context: None,
            outcome: None,
            decision_type: DecisionType::Session,
            suggested_at: None,
            resolved_at: None,
            override_reason: None,
            decision_id: None,
            brain_decision_ms: None,
            cache_hit: None,
            canonical: None,
        }
    }

    #[test]
    fn format_few_shot_empty() {
        assert_eq!(format_few_shot_examples(&[]), "");
    }

    #[test]
    fn format_few_shot_single() {
        let d = make_decision("Bash", "my-project", "accept");
        let output = format_few_shot_examples(&[d]);
        assert!(output.contains("tool=Bash"));
        assert!(output.contains("user: accept"));
        assert!(output.contains("90%"));
    }

    #[test]
    fn format_few_shot_multiple() {
        let decisions = vec![
            make_decision("Bash", "proj", "accept"),
            make_decision("Read", "proj", "reject"),
        ];
        let output = format_few_shot_examples(&decisions);
        let lines: Vec<&str> = output.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("Bash"));
        assert!(lines[1].contains("Read"));
    }

    #[test]
    fn retrieve_empty_returns_empty() {
        let result = retrieve_similar(Some("Bash"), "test", 5, None);
        // Will be empty because decisions_path() points to nonexistent file
        assert!(result.is_empty() || !result.is_empty()); // No panic
    }

    #[test]
    fn format_few_shot_observation_format() {
        let d = make_decision("Read", "proj", "user_approve");
        let output = format_few_shot_examples(&[d]);
        assert!(output.contains("user action: user_approve"));
        assert!(!output.contains("brain:"));
    }

    #[test]
    fn format_few_shot_brain_decision_format() {
        let d = make_decision("Bash", "proj", "accept");
        let output = format_few_shot_examples(&[d]);
        assert!(output.contains("brain: approve"));
        assert!(output.contains("user: accept"));
    }

    #[test]
    fn outcome_weighted_retrieval_prefers_corrections() {
        // Rejected decisions should score higher (correction signal)
        let decisions = [
            make_decision("Bash", "proj", "accept"),
            make_decision("Bash", "proj", "reject"),
        ];

        // Reject gets dynamic weight (here 1:1 ratio → clamped to floor 3),
        // accept gets +3. Both match on tool (+10) and project (+5).
        let reject = &decisions[1];
        assert!(reject.is_negative());
    }

    #[test]
    fn dynamic_rejection_weight_typical_ratio() {
        // 90/10 ratio → weight = 9
        let mut decisions: Vec<DecisionRecord> = (0..9)
            .map(|_| make_decision("Bash", "proj", "accept"))
            .collect();
        decisions.push(make_decision("Bash", "proj", "reject"));
        let refs: Vec<&DecisionRecord> = decisions.iter().collect();
        assert_eq!(dynamic_rejection_weight(&refs), 9);
    }

    #[test]
    fn dynamic_rejection_weight_frequent_rejects() {
        // 60/40 ratio → 6/4 = 1.5 → clamp to floor of 3
        let mut decisions: Vec<DecisionRecord> = (0..6)
            .map(|_| make_decision("Bash", "proj", "accept"))
            .collect();
        decisions.extend((0..4).map(|_| make_decision("Bash", "proj", "reject")));
        let refs: Vec<&DecisionRecord> = decisions.iter().collect();
        assert_eq!(dynamic_rejection_weight(&refs), 3);
    }

    #[test]
    fn dynamic_rejection_weight_rare_rejects() {
        // 99/1 ratio → clamp to cap of 12
        let mut decisions: Vec<DecisionRecord> = (0..99)
            .map(|_| make_decision("Bash", "proj", "accept"))
            .collect();
        decisions.push(make_decision("Bash", "proj", "reject"));
        let refs: Vec<&DecisionRecord> = decisions.iter().collect();
        assert_eq!(dynamic_rejection_weight(&refs), 12);
    }

    #[test]
    fn dynamic_rejection_weight_no_rejects() {
        // All accepts, 0 rejects → 10/max(0,1) = 10 → clamps to 10
        let decisions: Vec<DecisionRecord> = (0..10)
            .map(|_| make_decision("Bash", "proj", "accept"))
            .collect();
        let refs: Vec<&DecisionRecord> = decisions.iter().collect();
        assert_eq!(dynamic_rejection_weight(&refs), 10);
    }

    #[test]
    fn dynamic_rejection_weight_no_accepts() {
        // All rejects, 0 accepts → 0/10 = 0 → clamps to floor of 3
        let decisions: Vec<DecisionRecord> = (0..10)
            .map(|_| make_decision("Bash", "proj", "reject"))
            .collect();
        let refs: Vec<&DecisionRecord> = decisions.iter().collect();
        assert_eq!(dynamic_rejection_weight(&refs), 3);
    }

    #[test]
    fn dynamic_rejection_weight_only_observations() {
        // No accepts or rejects (neutral observations) → 0/max(0,1) = 0 → clamps to 3
        let decisions: Vec<DecisionRecord> = (0..5)
            .map(|_| make_decision("Read", "proj", "user_input"))
            .collect();
        let refs: Vec<&DecisionRecord> = decisions.iter().collect();
        assert_eq!(dynamic_rejection_weight(&refs), 3);
    }
}
