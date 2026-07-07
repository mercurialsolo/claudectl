//! Extracted from app/mod.rs — behavior-preserving split.
#![allow(clippy::too_many_lines)]

use super::*;

impl App {
    pub fn clear_filters(&mut self) {
        self.status_filter = StatusFilter::All;
        self.focus_filter = FocusFilter::All;
        self.search_query.clear();
        self.search_buffer.clear();
        self.search_mode = false;
        self.normalize_selection();
        self.status_msg = "Filters cleared".into();
    }

    pub fn cycle_status_filter(&mut self) {
        self.status_filter = self.status_filter.next();
        self.normalize_selection();
        self.status_msg = format!("Status filter: {}", self.status_filter.label());
    }

    pub fn cycle_focus_filter(&mut self) {
        self.focus_filter = self.focus_filter.next();
        self.normalize_selection();
        self.status_msg = format!("Focus filter: {}", self.focus_filter.label());
    }

    pub fn has_active_filters(&self) -> bool {
        self.status_filter != StatusFilter::All
            || self.focus_filter != FocusFilter::All
            || !self.search_query.trim().is_empty()
    }

    pub fn filter_summary(&self) -> String {
        let mut parts = Vec::new();
        if self.status_filter != StatusFilter::All {
            parts.push(format!("status={}", self.status_filter.label()));
        }
        if self.focus_filter != FocusFilter::All {
            parts.push(format!("focus={}", self.focus_filter.label()));
        }
        if !self.search_query.trim().is_empty() {
            parts.push(format!("search=\"{}\"", self.search_query));
        }
        if parts.is_empty() {
            "filters: none".to_string()
        } else {
            format!("filters: {}", parts.join(" | "))
        }
    }

    pub fn visible_session_indices(&self) -> Vec<usize> {
        self.sessions
            .iter()
            .enumerate()
            .filter_map(|(idx, session)| self.matches_filters(session).then_some(idx))
            .collect()
    }

    pub fn visible_sessions(&self) -> Vec<&ClaudeSession> {
        self.visible_session_indices()
            .into_iter()
            .filter_map(|idx| self.sessions.get(idx))
            .collect()
    }

    pub fn visible_session_count(&self) -> usize {
        self.visible_session_indices().len()
    }

    pub(super) fn normalize_selection(&mut self) {
        let len = self.visible_session_count();
        if len == 0 {
            self.table_state.select(None);
        } else if self.table_state.selected().is_none() {
            self.table_state.select(Some(0));
        } else if let Some(sel) = self.table_state.selected() {
            if sel >= len {
                self.table_state.select(Some(len - 1));
            }
        }
    }

    pub(super) fn matches_filters(&self, session: &ClaudeSession) -> bool {
        self.status_filter.matches(session.status)
            && self.matches_focus_filter(session)
            && self.matches_search_query(session)
    }

    pub(super) fn matches_focus_filter(&self, session: &ClaudeSession) -> bool {
        let over_budget = self
            .budget_usd
            .map(|budget| session.has_usage_metrics() && session.cost_usd >= budget)
            .unwrap_or(false);
        let high_context = session.has_usage_metrics()
            && session.context_percent() >= self.context_warn_threshold as f64;
        let unknown_telemetry = !session.has_usage_metrics();
        let conflict = self.conflict_pids.contains(&session.pid);

        match self.focus_filter {
            FocusFilter::All => true,
            FocusFilter::Attention => {
                session.status == SessionStatus::NeedsInput
                    || over_budget
                    || high_context
                    || unknown_telemetry
                    || conflict
            }
            FocusFilter::OverBudget => over_budget,
            FocusFilter::HighContext => high_context,
            FocusFilter::UnknownTelemetry => unknown_telemetry,
            FocusFilter::Conflict => conflict,
        }
    }

    pub(super) fn matches_search_query(&self, session: &ClaudeSession) -> bool {
        let query = self.search_query.trim();
        if query.is_empty() {
            return true;
        }

        let query = query.to_ascii_lowercase();
        let fields = [
            session.display_name().to_string(),
            session.project_name.clone(),
            session.model.clone(),
            session.cwd.clone(),
            session.session_id.clone(),
        ];

        fields
            .iter()
            .any(|field| field.to_ascii_lowercase().contains(&query))
    }
}
