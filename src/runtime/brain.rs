//! Bind `BrainView` to the binary's brain subsystem.

use claudectl_core::runtime::{BrainGateMode, BrainView, DecisionSummary};

use crate::brain;

pub struct LiveBrainView;

impl BrainView for LiveBrainView {
    fn gate_mode(&self) -> BrainGateMode {
        parse_gate_mode(&brain::read_gate_mode())
    }

    fn recent_decisions(&self, n: usize) -> Vec<DecisionSummary> {
        let mut all = brain::decisions::read_all_decisions();
        // brain::decisions::read_all_decisions returns oldest-first; the TUI
        // wants newest-first.
        all.reverse();
        all.into_iter().take(n).map(summary_from_record).collect()
    }

    fn decision_count(&self) -> usize {
        brain::decisions::read_all_decisions().len()
    }
}

/// String → enum. Unknown values fall back to `On` to match
/// `brain::read_gate_mode`'s "no file" default.
fn parse_gate_mode(raw: &str) -> BrainGateMode {
    match raw.trim().to_lowercase().as_str() {
        "off" => BrainGateMode::Off,
        "auto" => BrainGateMode::Auto,
        _ => BrainGateMode::On,
    }
}

fn summary_from_record(r: brain::decisions::DecisionRecord) -> DecisionSummary {
    DecisionSummary::from(&r)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gate_mode_parsing_recognizes_known_values() {
        assert_eq!(parse_gate_mode("on"), BrainGateMode::On);
        assert_eq!(parse_gate_mode("OFF"), BrainGateMode::Off);
        assert_eq!(parse_gate_mode(" auto "), BrainGateMode::Auto);
    }

    #[test]
    fn gate_mode_parsing_falls_back_to_on() {
        // Matches the file-missing default in `brain::read_gate_mode`.
        assert_eq!(parse_gate_mode(""), BrainGateMode::On);
        assert_eq!(parse_gate_mode("garbage"), BrainGateMode::On);
    }
}
