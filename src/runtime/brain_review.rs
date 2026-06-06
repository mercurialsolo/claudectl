//! Bind `BrainReviewView` to the binary's decision log + review-queue builder.
//!
//! Projects the full `brain::decisions::DecisionRecord` shape to the core
//! `DecisionSummary` DTO (including the optional Brain-Review-only fields
//! added in this PR), then wraps `brain::review::ReviewItem` as
//! `ReviewItemSummary` with the same projection.

use claudectl_core::runtime::{BrainReviewView, DecisionSummary, ReviewItemSummary};

use crate::brain;

pub struct LiveBrainReviewView;

impl BrainReviewView for LiveBrainReviewView {
    fn all_decisions(&self) -> Vec<DecisionSummary> {
        let mut all = brain::decisions::read_all_decisions();
        // The on-disk log is oldest-first; the UI wants newest-first.
        all.reverse();
        all.into_iter().map(summary_from).collect()
    }

    fn review_queue(&self) -> Vec<ReviewItemSummary> {
        // The queue-builder operates on records in their original order; we
        // reverse afterwards so the UI sees newest-first within a score tier.
        let records = brain::decisions::read_all_decisions();
        let queue = brain::review::build_queue(&records);
        queue.into_iter().map(item_summary_from).collect()
    }
}

fn summary_from(r: brain::decisions::DecisionRecord) -> DecisionSummary {
    DecisionSummary {
        id: r.decision_id.unwrap_or_default(),
        timestamp: r.timestamp,
        action: r.brain_action,
        confidence: Some(r.brain_confidence),
        project: Some(r.project),
        tool: r.tool,
        command: r.command,
        reasoning: Some(r.brain_reasoning).filter(|s| !s.is_empty()),
        user_action: Some(r.user_action),
        override_reason: r.override_reason,
        brain_decision_ms: r.brain_decision_ms,
        canonical: r.canonical,
        cache_hit: r.cache_hit,
        cost_usd: r.context.as_ref().map(|c| c.cost_usd),
        model: r.context.as_ref().map(|c| c.model.clone()),
    }
}

fn item_summary_from(item: brain::review::ReviewItem) -> ReviewItemSummary {
    ReviewItemSummary {
        decision: summary_from(item.record),
        reason: item.reason,
        score: item.score as f64,
    }
}
