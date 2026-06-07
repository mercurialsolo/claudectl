// Full-screen Brain Review mode. When `app.show_brain` is set, the main
// draw loop hands the whole frame to `render_brain_screen` instead of the
// session table. Two tabs:
//
// - Scorecard: the periodic-review composite (north star + guardrails + per-
//   tier accuracy + latency + cache + counterfactual summary), kept in sync
//   with the `--brain-stats scorecard` CLI output.
// - Review:    the prioritized review queue, with key-driven mark / note /
//   skip — same flow as `--brain-review` but rendered inline so the user can
//   work it during a normal dashboard session.

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap};

use crate::app::{App, BrainTab};
use crate::brain::decisions::DecisionRecord;
use crate::brain::metrics::{
    CacheSummary, LatencySummary, TierStats, compute_cache, compute_counterfactuals,
    compute_latency, compute_tier_stats,
};
use crate::brain::risk::{RiskTier, classify_risk};

pub fn render_brain_screen(frame: &mut Frame, area: Rect, app: &App) {
    let t = &app.theme;

    let title = Line::from(vec![
        Span::styled(" claudectl ", Style::default().fg(t.text_primary)),
        Span::styled(
            "│ Brain Review ",
            Style::default().fg(t.header).add_modifier(Modifier::BOLD),
        ),
    ]);
    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(t.header));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let footer_height = if app
        .brain_status_msg
        .as_deref()
        .map(|m| !m.is_empty())
        .unwrap_or(false)
    {
        2
    } else {
        1
    };

    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),             // tab row
            Constraint::Length(1),             // counts header
            Constraint::Min(3),                // body
            Constraint::Length(footer_height), // hint strip
        ])
        .split(inner);

    render_tab_row(frame, layout[0], app);
    render_counts_header(frame, layout[1], app);
    match app.brain_tab {
        BrainTab::Scorecard => render_scorecard(frame, layout[2], app),
        BrainTab::Review => render_review(frame, layout[2], app),
    }
    render_footer(frame, layout[3], app);
}

fn render_tab_row(frame: &mut Frame, area: Rect, app: &App) {
    let t = &app.theme;
    let mk = |label: &str, active: bool| {
        let style = if active {
            Style::default()
                .fg(t.header)
                .add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
        } else {
            Style::default().fg(t.text_muted)
        };
        Span::styled(format!("  {label}  "), style)
    };
    let line = Line::from(vec![
        mk("Scorecard", app.brain_tab == BrainTab::Scorecard),
        Span::raw("│"),
        mk(
            &format!("Review ({})", app.brain_queue.len()),
            app.brain_tab == BrainTab::Review,
        ),
        Span::styled("    Tab to switch", Style::default().fg(t.text_muted)),
    ]);
    frame.render_widget(Paragraph::new(line), area);
}

fn render_counts_header(frame: &mut Frame, area: Rect, app: &App) {
    let t = &app.theme;
    let total = app.brain_decisions_cache.len();
    let canonical = app
        .brain_decisions_cache
        .iter()
        .filter(|d| d.canonical == Some(true))
        .count();
    let with_brain = app
        .brain_decisions_cache
        .iter()
        .filter(|d| !d.brain_action.is_empty())
        .count();
    let line = Line::from(vec![
        Span::styled(
            format!("decisions: {total}  "),
            Style::default().fg(t.text_muted),
        ),
        Span::styled(
            format!("brain-involved: {with_brain}  "),
            Style::default().fg(t.text_muted),
        ),
        Span::styled(
            format!("canonical: {canonical}"),
            Style::default().fg(t.text_muted),
        ),
    ]);
    frame.render_widget(Paragraph::new(line), area);
}

// ── Scorecard tab ────────────────────────────────────────────────────────

fn render_scorecard(frame: &mut Frame, area: Rect, app: &App) {
    let t = &app.theme;
    let decisions = &app.brain_decisions_cache;
    // Project once for every compute_* call below — the metrics surface
    // operates on the core `DecisionSummary` DTO so it can be shared with
    // future callers outside the binary.
    let summaries: Vec<claudectl_core::runtime::DecisionSummary> =
        decisions.iter().map(Into::into).collect();

    let total_with_brain = decisions
        .iter()
        .filter(|d| !d.brain_action.is_empty())
        .count();
    let correct = decisions
        .iter()
        .filter(|d| !d.brain_action.is_empty() && d.is_positive())
        .count();
    let north_star = if total_with_brain > 0 {
        (correct as f64 / total_with_brain as f64) * 100.0
    } else {
        0.0
    };

    let tier_stats = compute_tier_stats(&summaries);
    let latency = compute_latency(&summaries);
    let cache = compute_cache(&summaries);
    let cfs = compute_counterfactuals(&summaries);
    let brain_right = cfs.iter().filter(|c| c.brain_was_right).count();
    let user_right = cfs.len() - brain_right;
    let canonical_count = decisions
        .iter()
        .filter(|d| d.canonical == Some(true))
        .count();

    let override_window: Vec<&DecisionRecord> = decisions
        .iter()
        .rev()
        .filter(|d| !d.brain_action.is_empty())
        .take(50)
        .collect();
    let override_rate = if override_window.is_empty() {
        None
    } else {
        let n = override_window.iter().filter(|d| d.is_negative()).count();
        Some((n as f64 / override_window.len() as f64) * 100.0)
    };

    let muted = Style::default().fg(t.text_muted);
    let ok = Style::default()
        .fg(t.text_primary)
        .add_modifier(Modifier::BOLD);
    let warn = Style::default().fg(t.header).add_modifier(Modifier::BOLD);

    let mut lines: Vec<Line> = Vec::new();

    lines.push(Line::from(Span::styled("NORTH STAR", warn)));
    if total_with_brain == 0 {
        lines.push(Line::from(Span::styled(
            "  Auto-handled accuracy:  —  (no brain decisions yet)",
            muted,
        )));
    } else {
        let marker = if north_star >= 85.0 { "✓" } else { "⚠" };
        lines.push(Line::from(Span::styled(
            format!(
                "  Auto-handled accuracy:  {:.1}% {}  (n = {}, target ≥ 85%)",
                north_star, marker, total_with_brain
            ),
            ok,
        )));
    }
    lines.push(Line::from(""));

    lines.push(Line::from(Span::styled("GUARDRAILS", warn)));
    let critical = tier_stats
        .iter()
        .find(|s| matches!(s.tier, RiskTier::Critical));
    let critical_line = match critical {
        Some(s) if s.n > 0 => format!(
            "  Critical-tier false-approves:  {} of {} ({:.1}%) {}   target = 0",
            s.false_approves,
            s.n,
            s.false_approve_pct(),
            if s.false_approves == 0 { "✓" } else { "✗" }
        ),
        _ => "  Critical-tier false-approves:  no Critical samples yet".to_string(),
    };
    lines.push(Line::from(Span::styled(critical_line, muted)));
    if let Some(rate) = override_rate {
        let marker = if rate < 20.0 { "✓" } else { "⚠" };
        lines.push(Line::from(Span::styled(
            format!(
                "  Override rate (last 50):       {:.1}% {}   target ↓ (learning)",
                rate, marker
            ),
            muted,
        )));
    } else {
        lines.push(Line::from(Span::styled(
            "  Override rate (last 50):       no instrumented samples yet",
            muted,
        )));
    }
    lines.push(Line::from(""));

    lines.push(Line::from(Span::styled("LATENCY", warn)));
    if latency.n == 0 {
        lines.push(Line::from(Span::styled(
            "  No instrumented samples yet — recorded on each brain decision.",
            muted,
        )));
    } else {
        lines.push(Line::from(Span::styled(
            format_latency_line(&latency),
            muted,
        )));
    }
    lines.push(Line::from(""));

    lines.push(Line::from(Span::styled("CACHE HIT RATE", warn)));
    lines.push(Line::from(Span::styled(format_cache_line(&cache), muted)));
    lines.push(Line::from(""));

    lines.push(Line::from(Span::styled("PER-RISK-TIER ACCURACY", warn)));
    for s in &tier_stats {
        lines.push(Line::from(Span::styled(format_tier_line(s), muted)));
    }
    lines.push(Line::from(""));

    lines.push(Line::from(Span::styled("COUNTERFACTUAL HITS", warn)));
    lines.push(Line::from(Span::styled(
        format!(
            "  Brain was right (user override → failure):  {}",
            brain_right
        ),
        muted,
    )));
    lines.push(Line::from(Span::styled(
        format!(
            "  User was right (brain over-cautious):       {}",
            user_right
        ),
        muted,
    )));
    lines.push(Line::from(""));

    lines.push(Line::from(Span::styled("REVIEW STATUS", warn)));
    lines.push(Line::from(Span::styled(
        format!(
            "  Total: {}   marked canonical: {} ({:.1}%)   queue: {}",
            decisions.len(),
            canonical_count,
            if decisions.is_empty() {
                0.0
            } else {
                (canonical_count as f64 / decisions.len() as f64) * 100.0
            },
            app.brain_queue.len()
        ),
        muted,
    )));
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "  → Tab to switch to the Review queue and mark canonical decisions.",
        Style::default().fg(t.text_primary),
    )));

    let para = Paragraph::new(lines).wrap(Wrap { trim: false });
    frame.render_widget(para, area);
}

fn format_latency_line(s: &LatencySummary) -> String {
    let marker = if s.p95_ms <= 1000 { "✓" } else { "⚠" };
    format!(
        "  p50 {} ms  |  p95 {} ms {}  |  p99 {} ms  |  n = {}",
        s.p50_ms, s.p95_ms, marker, s.p99_ms, s.n
    )
}

fn format_cache_line(s: &CacheSummary) -> String {
    if s.instrumented == 0 {
        return "  No instrumented samples yet — recorded on each brain decision.".to_string();
    }
    format!(
        "  {:.1}%  ({} of {} decisions handled without an LLM call)",
        s.hit_rate(),
        s.hits,
        s.instrumented
    )
}

fn format_tier_line(s: &TierStats) -> String {
    if s.n == 0 {
        format!("  {:<10}  n = 0", s.tier.label())
    } else {
        format!(
            "  {:<10}  {:5.1}%   n = {:<4}  false-approves = {}",
            s.tier.label(),
            s.accuracy_pct(),
            s.n,
            s.false_approves
        )
    }
}

// ── Review tab ───────────────────────────────────────────────────────────

fn render_review(frame: &mut Frame, area: Rect, app: &App) {
    let t = &app.theme;

    if app.brain_queue.is_empty() {
        let para = Paragraph::new(vec![
            Line::from(""),
            Line::from(Span::styled(
                "  The review queue is empty.",
                Style::default()
                    .fg(t.text_primary)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(""),
            Line::from(Span::styled(
                "  Either the brain has been right on every confident call,",
                Style::default().fg(t.text_muted),
            )),
            Line::from(Span::styled(
                "  or outcome attribution hasn't caught up yet. Press `r` to refresh,",
                Style::default().fg(t.text_muted),
            )),
            Line::from(Span::styled(
                "  or Tab to inspect the Scorecard.",
                Style::default().fg(t.text_muted),
            )),
        ])
        .wrap(Wrap { trim: false });
        frame.render_widget(para, area);
        return;
    }

    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(45), Constraint::Percentage(55)])
        .split(area);

    render_review_list(frame, cols[0], app);
    render_review_detail(frame, cols[1], app);
}

fn render_review_list(frame: &mut Frame, area: Rect, app: &App) {
    let t = &app.theme;

    let items: Vec<ListItem> = app
        .brain_queue
        .iter()
        .enumerate()
        .map(|(i, item)| {
            let tier = classify_risk(item.record.tool.as_deref(), item.record.command.as_deref());
            let header = format!(
                "[{score:>3}] {tool:<8} {tier:<8}",
                score = item.score,
                tool = item.record.tool.as_deref().unwrap_or("?"),
                tier = tier.label(),
            );
            let reason = truncate(&item.reason, 60);
            let style = if i == app.brain_review_selected {
                Style::default().fg(t.header).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(t.text_primary)
            };
            ListItem::new(vec![
                Line::from(Span::styled(header, style)),
                Line::from(Span::styled(
                    format!("    {reason}"),
                    Style::default().fg(t.text_muted),
                )),
            ])
        })
        .collect();

    let block = Block::default()
        .borders(Borders::RIGHT)
        .border_style(Style::default().fg(t.text_muted));
    let list = List::new(items).block(block).highlight_symbol("▌ ");

    let mut state = ListState::default();
    state.select(Some(
        app.brain_review_selected
            .min(app.brain_queue.len().saturating_sub(1)),
    ));
    frame.render_stateful_widget(list, area, &mut state);
}

fn render_review_detail(frame: &mut Frame, area: Rect, app: &App) {
    let t = &app.theme;
    let Some(item) = app.brain_queue.get(app.brain_review_selected) else {
        return;
    };
    let d = &item.record;
    let tier = classify_risk(d.tool.as_deref(), d.command.as_deref());

    let muted = Style::default().fg(t.text_muted);
    let primary = Style::default().fg(t.text_primary);
    let bold = primary.add_modifier(Modifier::BOLD);

    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::from(Span::styled(
        format!("  Reason: {}", item.reason),
        bold,
    )));
    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::styled("  tier:        ", muted),
        Span::styled(format!("{tier}"), primary),
    ]));
    lines.push(Line::from(vec![
        Span::styled("  project:     ", muted),
        Span::styled(&d.project, primary),
    ]));
    lines.push(Line::from(vec![
        Span::styled("  tool:        ", muted),
        Span::styled(d.tool.as_deref().unwrap_or("(none)"), primary),
    ]));
    if let Some(cmd) = &d.command {
        lines.push(Line::from(Span::styled("  command:", muted)));
        for chunk in wrap_lines(cmd, 70) {
            lines.push(Line::from(Span::styled(format!("    {}", chunk), primary)));
        }
    }
    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::styled("  brain:       ", muted),
        Span::styled(
            format!(
                "{} ({:.0}% confidence)",
                d.brain_action,
                d.brain_confidence * 100.0
            ),
            primary,
        ),
    ]));
    if !d.brain_reasoning.is_empty() {
        lines.push(Line::from(Span::styled("  reasoning:", muted)));
        for chunk in wrap_lines(&d.brain_reasoning, 70) {
            lines.push(Line::from(Span::styled(format!("    {}", chunk), primary)));
        }
    }
    lines.push(Line::from(vec![
        Span::styled("  user:        ", muted),
        Span::styled(&d.user_action, primary),
    ]));
    if let Some(reason) = &d.override_reason {
        lines.push(Line::from(vec![
            Span::styled("  override:    ", muted),
            Span::styled(reason, primary),
        ]));
    }
    if let Some(ms) = d.brain_decision_ms {
        lines.push(Line::from(vec![
            Span::styled("  latency:     ", muted),
            Span::styled(format!("{ms} ms"), primary),
        ]));
    }
    if let Some(hit) = d.cache_hit {
        lines.push(Line::from(vec![
            Span::styled("  cache_hit:   ", muted),
            Span::styled(format!("{hit}"), primary),
        ]));
    }
    if let Some(ctx) = &d.context {
        lines.push(Line::from(vec![
            Span::styled("  cost:        ", muted),
            Span::styled(format!("${:.4}", ctx.cost_usd), primary),
        ]));
        lines.push(Line::from(vec![
            Span::styled("  model:       ", muted),
            Span::styled(&ctx.model, primary),
        ]));
    }

    let para = Paragraph::new(lines)
        .block(Block::default().borders(Borders::NONE))
        .wrap(Wrap { trim: false });
    frame.render_widget(para, area);
}

// ── Footer ───────────────────────────────────────────────────────────────

fn render_footer(frame: &mut Frame, area: Rect, app: &App) {
    let t = &app.theme;

    let hint = if app.brain_note_input_mode {
        Line::from(vec![
            Span::styled("note: ", Style::default().fg(t.header)),
            Span::styled(&app.brain_note_buffer, Style::default().fg(t.text_primary)),
            Span::styled(
                "    Enter to save · Esc to cancel",
                Style::default().fg(t.text_muted),
            ),
        ])
    } else {
        match app.brain_tab {
            BrainTab::Scorecard => Line::from(vec![
                key(t, "Tab"),
                Span::raw(":Switch tabs  "),
                key(t, "r"),
                Span::raw(":Refresh  "),
                key(t, "Esc/M/q"),
                Span::raw(":Close"),
            ]),
            BrainTab::Review => Line::from(vec![
                key(t, "j/k"),
                Span::raw(":Move  "),
                key(t, "m"),
                Span::raw(":Mark canonical  "),
                key(t, "n"),
                Span::raw(":Mark+note  "),
                key(t, "s"),
                Span::raw(":Skip  "),
                key(t, "Tab"),
                Span::raw(":Scorecard  "),
                key(t, "Esc/M"),
                Span::raw(":Close"),
            ]),
        }
    };

    frame.render_widget(Paragraph::new(hint), area);

    if let Some(msg) = &app.brain_status_msg {
        if !msg.is_empty() {
            let footer_msg_area = Rect {
                x: area.x,
                y: area.y + 1,
                width: area.width,
                height: 1,
            };
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    format!("  {msg}"),
                    Style::default().fg(t.header),
                ))),
                footer_msg_area,
            );
        }
    }
}

fn key(theme: &crate::theme::Theme, label: &str) -> Span<'static> {
    Span::styled(
        label.to_string(),
        Style::default()
            .fg(theme.header)
            .add_modifier(Modifier::BOLD),
    )
}

// ── Utilities ────────────────────────────────────────────────────────────

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}…", &s[..max])
    }
}

fn wrap_lines(s: &str, width: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    for word in s.split_whitespace() {
        if current.is_empty() {
            current.push_str(word);
        } else if current.len() + 1 + word.len() <= width {
            current.push(' ');
            current.push_str(word);
        } else {
            out.push(std::mem::take(&mut current));
            current.push_str(word);
        }
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}
