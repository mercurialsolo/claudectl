use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Cell, Paragraph, Row, Table},
};

use crate::app::{App, SORT_COLUMNS};
use crate::session::{ClaudeSession, SessionStatus, SubagentBreakdown, SubagentState};

use super::detail::render_detail_panel;
use super::help::render_help_overlay;
use super::status_bar::render_status_bar;

pub fn render(frame: &mut Frame, area: Rect, app: &App) {
    let t = &app.theme;
    let visible_sessions = app.visible_sessions();
    let has_status = !app.status_msg.is_empty()
        || app.input_mode
        || app.launch_mode
        || app.search_mode
        || app.has_active_filters();
    let show_detail = app.detail_panel && app.selected_session().is_some();

    let mut constraints = Vec::new();
    if show_detail {
        constraints.push(Constraint::Percentage(55)); // table
        constraints.push(Constraint::Percentage(45)); // detail
    } else {
        constraints.push(Constraint::Min(3));
    }
    if has_status {
        constraints.push(Constraint::Length(1));
    }

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(area)
        .to_vec();

    // Empty state: show onboarding message when no sessions found
    if app.data_snapshot().sessions.is_empty() {
        let launch_hint = if crate::terminals::can_launch_session() {
            "  Press n for the launch wizard, or start claude in another terminal."
        } else {
            "  Start claude in GNOME Terminal, tmux, Kitty, WezTerm, Windows Terminal on WSL, or another terminal."
        };
        let empty_lines = vec![
            Line::from(""),
            Line::from(""),
            Line::from(Span::styled(
                "No active Claude Code sessions found.",
                Style::default()
                    .fg(t.text_muted)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(""),
            Line::from(launch_hint),
            Line::from("  Run claudectl --doctor if terminal switching or launch fails."),
            Line::from(""),
            Line::from(vec![
                Span::styled("  Press ", Style::default().fg(t.text_muted)),
                Span::styled(
                    "?",
                    Style::default()
                        .fg(t.highlight_key)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(" for help.", Style::default().fg(t.text_muted)),
            ]),
        ];

        let block = Block::default()
            .title(" claudectl ")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(t.border));

        let empty_widget = Paragraph::new(empty_lines)
            .block(block)
            .alignment(Alignment::Center);

        frame.render_widget(empty_widget, chunks[0]);

        if has_status && chunks.len() > 1 {
            render_status_bar(frame, chunks[1], app);
        }

        if app.show_help {
            render_help_overlay(frame, area, app);
        }
        return;
    }

    if visible_sessions.is_empty() {
        let empty_lines = vec![
            Line::from(""),
            Line::from(""),
            Line::from(Span::styled(
                "No sessions match the current filters.",
                Style::default()
                    .fg(t.text_muted)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(""),
            Line::from(format!("  {}", app.filter_summary())),
            Line::from(""),
            Line::from("  Press z to clear filters, or / to edit the search query."),
        ];

        let block = Block::default()
            .title(" claudectl ")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(t.border));

        let empty_widget = Paragraph::new(empty_lines)
            .block(block)
            .alignment(Alignment::Center);

        frame.render_widget(empty_widget, chunks[0]);

        if has_status && chunks.len() > 1 {
            render_status_bar(frame, chunks[1], app);
        }

        if app.show_help {
            render_help_overlay(frame, area, app);
        }
        return;
    }

    // Build header with sort indicator
    let header_names = [
        "PID", "Name", "Project", "Status", "Context", "Cost", "$/hr", "Elapsed", "Last", "CPU%",
        "MEM", "In/Out", "Activity",
    ];

    // Map sort_column index to header index:
    // 0=Status->3, 1=Context->4, 2=Cost->5, 3=$/hr->6, 4=Elapsed->7, 5=Last->8, 6=Name->1
    let sort_header_idx = match app.sort_column {
        0 => 3, // Status
        1 => 4, // Context
        2 => 5, // Cost
        3 => 6, // $/hr
        4 => 7, // Elapsed
        5 => 8, // Last
        6 => 1, // Name
        _ => usize::MAX,
    };

    let sort_arrow = if app.sort_reversed {
        '\u{25b2}' // ▲ reversed (arrow points "up" = top is the end)
    } else {
        '\u{25bc}' // ▼ natural direction
    };
    let header_cells = header_names.iter().enumerate().map(|(i, h)| {
        let label = if i == sort_header_idx {
            format!("{h} {sort_arrow}")
        } else {
            (*h).to_string()
        };
        Cell::from(label).style(Style::default().fg(t.header).add_modifier(Modifier::BOLD))
    });

    let header = Row::new(header_cells).height(1);

    let selected_pid = app.selected_session().map(|s| s.pid);
    let mut selected_row_idx = None;
    let rows: Vec<Row> = if app.grouped_view {
        let groups = app.project_groups();
        let mut rows = Vec::new();
        let mut row_idx = 0usize;
        for group in &groups {
            // Group header row
            let cost_str = if group.total_cost < 1.0 {
                format!("${:.2}", group.total_cost)
            } else {
                format!("${:.1}", group.total_cost)
            };
            let header_text = format!(
                "{} ({} sessions, {} active, {}, ctx {:.0}%)",
                group.name,
                group.session_count,
                group.active_count,
                cost_str,
                group.avg_context_pct
            );
            let mut cells: Vec<Cell> = vec![
                Cell::from(""),
                Cell::from(""),
                Cell::from(header_text)
                    .style(Style::default().fg(t.header).add_modifier(Modifier::BOLD)),
            ];
            for _ in 3..13 {
                cells.push(Cell::from(""));
            }
            rows.push(Row::new(cells));
            row_idx += 1;

            // Session rows under this group
            for s in visible_sessions
                .iter()
                .filter(|s| s.project_name == group.name)
            {
                if Some(s.pid) == selected_pid {
                    selected_row_idx = Some(row_idx);
                }
                let session_rows = render_rows_for_session(s, app);
                row_idx += session_rows.len();
                rows.extend(session_rows);
            }
        }
        rows
    } else {
        let mut rows = Vec::new();
        let mut row_idx = 0usize;
        let parked_count = visible_sessions
            .iter()
            .filter(|s| app.is_parked(&s.session_id))
            .count();
        let mut separator_inserted = false;
        for s in visible_sessions.iter() {
            let is_parked = app.is_parked(&s.session_id);
            // When we see the first parked session, insert a visual separator
            // so the section is obviously distinct from the working set above.
            if is_parked && !separator_inserted {
                rows.push(parked_separator_row(app, parked_count));
                row_idx += 1;
                separator_inserted = true;
            }
            if Some(s.pid) == selected_pid {
                selected_row_idx = Some(row_idx);
            }
            let session_rows = render_rows_for_session(s, app);
            row_idx += session_rows.len();
            rows.extend(session_rows);
        }
        rows
    };

    let widths = [
        Constraint::Length(7),  // PID
        Constraint::Min(14),    // Name (flex)
        Constraint::Min(10),    // Project (flex)
        Constraint::Length(14), // Status (wider for * indicator)
        Constraint::Length(13), // Context bar
        Constraint::Length(8),  // Cost
        Constraint::Length(9),  // $/hr
        Constraint::Length(10), // Elapsed
        Constraint::Length(6),  // Last (relative age)
        Constraint::Length(6),  // CPU%
        Constraint::Length(5),  // MEM
        Constraint::Length(14), // Tokens
        Constraint::Length(16), // Activity sparkline
    ];

    let count = visible_sessions.len();
    let total_sessions = app.data_snapshot().sessions.len();
    let active = visible_sessions
        .iter()
        .filter(|s| {
            matches!(
                s.status,
                SessionStatus::Processing | SessionStatus::NeedsInput
            )
        })
        .count();
    let total_cost: f64 = visible_sessions.iter().map(|s| s.cost_usd).sum();
    let total_tokens: u64 = visible_sessions
        .iter()
        .map(|s| s.total_input_tokens + s.total_output_tokens)
        .sum();
    let missing_usage = visible_sessions
        .iter()
        .filter(|s| !s.has_usage_metrics())
        .count();
    let selected = app.table_state.selected().map(|i| i + 1).unwrap_or(0);

    let cost_str = if total_cost < 1.0 {
        format!("${total_cost:.2}")
    } else {
        format!("${total_cost:.1}")
    };

    let tokens_str = format_token_count(total_tokens);
    let partial_str = if missing_usage > 0 {
        format!(" +{missing_usage} n/a")
    } else {
        String::new()
    };

    let sort_name = SORT_COLUMNS[app.sort_column];

    let mut footer_spans = vec![
        Span::styled(
            if app.has_active_filters() {
                format!(" {count}/{total_sessions} shown ({active} active) ")
            } else {
                format!(" {count} sessions ({active} active) ")
            },
            Style::default().fg(t.footer),
        ),
        Span::styled(format!("{cost_str} "), Style::default().fg(t.cost)),
        Span::styled(
            format!("{tokens_str}{partial_str} "),
            Style::default().fg(t.footer),
        ),
        Span::styled(
            format!("[{selected}/{count}]"),
            Style::default().fg(t.footer),
        ),
    ];

    if app.has_active_filters() {
        footer_spans.push(Span::styled(
            format!(" {} ", app.filter_summary()),
            Style::default().fg(t.header),
        ));
    }

    if app.debug {
        footer_spans.push(Span::styled(
            format!("  {}", app.debug_timings.format()),
            Style::default().fg(t.header),
        ));
    } else {
        // Contextual hints based on selected session state
        let hint = match app.selected_session().map(|s| s.status) {
            Some(SessionStatus::NeedsInput) => {
                "  y:approve i:type c:compact R:record Tab:go f/v:filter /:search z:clear d:kill ?:help".to_string()
            }
            _ => {
                format!(
                    "  q:quit j/k:nav Tab:go y:approve i:input c:compact R:record f/v:filter /:search z:clear d:kill s:sort({sort_name}) ?:help"
                )
            }
        };
        footer_spans.push(Span::styled(hint, Style::default().fg(t.footer)));
    }

    let footer = Line::from(footer_spans);

    // Title with usage throughput + recording indicator
    let rec_indicator = if !app.session_recordings.is_empty() {
        let count = app.session_recordings.len();
        if count == 1 {
            " \u{25cf} REC ".to_string()
        } else {
            format!(" \u{25cf} REC {count} ")
        }
    } else {
        String::new()
    };

    let mut title_spans: Vec<Span> = vec![Span::styled(
        " claudectl ",
        Style::default().fg(t.text_primary),
    )];

    if !rec_indicator.is_empty() {
        title_spans.push(Span::styled(
            rec_indicator,
            Style::default()
                .fg(ratatui::style::Color::Red)
                .add_modifier(Modifier::BOLD),
        ));
    }

    // Always-visible throughput panel sourced from the usage ledger
    // (JSONL-backed, independent of session-close detection). Suppressed only
    // until the first scan has populated the ledger.
    let snap = app.data_snapshot();
    let week = &snap.ledger_week;
    let month = &snap.ledger_month;
    if week.msg_count > 0 || month.msg_count > 0 {
        let week_tokens = format_token_count_nonempty(week.total_tokens());
        let month_tokens = format_token_count_nonempty(month.total_tokens());
        let week_cost = format_cost(week.cost_usd);
        let month_cost = format_cost(month.cost_usd);
        let eta_str = match app.budget_eta() {
            Some((spent, limit, eta, _urgency)) => {
                format!(
                    " \u{2502} {}/{} (ETA: {eta})",
                    format_cost(spent),
                    format_cost(limit),
                )
            }
            None => String::new(),
        };
        title_spans.push(Span::styled(
            format!(
                "\u{2502} week: {week_tokens} (~{week_cost}) \u{2502} month: {month_tokens} (~{month_cost}){eta_str} "
            ),
            Style::default().fg(t.footer),
        ));
    }

    let title = Line::from(title_spans);

    let block = Block::default()
        .title(title)
        .title_bottom(footer)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(t.border));

    let table = Table::new(rows, widths)
        .header(header)
        .block(block)
        .row_highlight_style(
            Style::default()
                .add_modifier(Modifier::REVERSED)
                .fg(t.text_primary),
        )
        .highlight_symbol("\u{25b6} "); // ▶

    let mut render_state = app.table_state.clone();
    render_state.select(selected_row_idx);
    frame.render_stateful_widget(table, chunks[0], &mut render_state);

    // Detail panel
    let mut next_chunk = 1;
    if show_detail {
        if let Some(session) = app.selected_session() {
            render_detail_panel(frame, chunks[next_chunk], &session, app);
        }
        next_chunk += 1;
    }

    // Status / input bar
    if has_status && next_chunk < chunks.len() {
        render_status_bar(frame, chunks[next_chunk], app);
    }

    // Help overlay
    if app.show_help {
        render_help_overlay(frame, area, app);
    }
}

/// A visual separator inserted between the working set and the parked
/// section in non-grouped view. Not selectable (navigation uses
/// `visible_session_indices`, which doesn't include this row).
fn parked_separator_row(app: &App, parked_count: usize) -> Row<'static> {
    let t = &app.theme;
    let label = format!("── parked ({parked_count}) ──");
    let mut cells: Vec<Cell> = vec![
        Cell::from(""),
        Cell::from(""),
        Cell::from(label).style(
            Style::default()
                .fg(t.text_muted)
                .add_modifier(Modifier::BOLD),
        ),
    ];
    for _ in 3..13 {
        cells.push(Cell::from(""));
    }
    Row::new(cells)
}

fn render_rows_for_session(s: &ClaudeSession, app: &App) -> Vec<Row<'static>> {
    let mut rows = vec![session_row(s, app)];
    let breakdown = s.subagent_breakdown();
    let total = breakdown.len();
    for (index, row) in breakdown.iter().enumerate() {
        rows.push(subagent_row(s, row, app, index, total));
    }
    // Parked sessions render in muted color at the row level. Cell-level
    // styles (status color, cost color, etc.) still apply, so the signal
    // isn't lost — just visually de-emphasized.
    if app.is_parked(&s.session_id) {
        let muted = Style::default().fg(app.theme.text_muted);
        rows = rows.into_iter().map(|r| r.style(muted)).collect();
    }
    rows
}

fn session_row(s: &ClaudeSession, app: &App) -> Row<'static> {
    let t = &app.theme;
    // Color escalation for NeedsInput based on wait time
    let status_style = if s.status == SessionStatus::NeedsInput {
        let wait_secs = app.wait_duration(s.pid).map(|d| d.as_secs()).unwrap_or(0);
        let color = if wait_secs >= 300 {
            t.cost_danger // Red after 5 min
        } else if wait_secs >= 60 {
            t.cost_warning // Orange/yellow after 1 min
        } else {
            t.status_needs_input
        };
        Style::default().fg(color)
    } else {
        Style::default().fg(t.status_color(&s.status))
    };

    let has_brain_suggestion = app
        .brain_engine
        .as_ref()
        .is_some_and(|e| e.pending.contains_key(&s.pid));

    let status_text = if app.auto_approve.contains(&s.pid) {
        format!("{}*", s.status)
    } else if has_brain_suggestion {
        let action = app
            .brain_engine
            .as_ref()
            .and_then(|e| e.pending.get(&s.pid))
            .map(|sg| sg.action.label())
            .unwrap_or("?");
        format!("{} [b:{}]", s.status, action)
    } else if s.status == SessionStatus::Unknown {
        s.telemetry_status.short_label().to_string()
    } else if s.status == SessionStatus::NeedsInput {
        match app.format_wait_time(s.pid) {
            Some(wait) => format!("{} ({})", s.status, wait),
            None => s.status.to_string(),
        }
    } else {
        s.status.to_string()
    };

    let decorations = CellDecorations {
        file_conflict: app.file_conflict_pids.contains(&s.pid),
        worktree_conflict: app.conflict_pids.contains(&s.pid),
        recording: app.session_recordings.contains_key(&s.pid),
        health_icon: crate::health::status_icon(s, &app.health_thresholds),
    };
    let (name_text, project_text) = build_name_and_project_text(s, &decorations);

    let ctx_pct = s.context_percent();
    let ctx_color = if !s.has_usage_metrics() {
        t.text_muted
    } else if ctx_pct > 80.0 {
        t.context_danger
    } else if ctx_pct > 50.0 {
        t.context_warning
    } else {
        t.context_ok
    };

    let burn_color = if !s.has_usage_metrics() {
        t.text_muted
    } else if s.burn_rate_per_hr > 10.0 {
        t.burn_rate_high
    } else if s.burn_rate_per_hr > 1.0 {
        t.burn_rate_mid
    } else {
        t.burn_rate_low
    };

    // Cost cell with budget indicator
    let (cost_text, cost_color) = if !s.has_usage_metrics() {
        (s.format_cost(), t.text_muted)
    } else if let Some(budget) = app.budget_usd {
        let pct = s.cost_usd / budget * 100.0;
        let text = format!("{} {:.0}%", s.format_cost(), pct);
        let color = if pct >= 100.0 {
            t.cost_danger
        } else if pct >= 80.0 {
            t.cost_warning
        } else {
            t.cost
        };
        (text, color)
    } else {
        (s.format_cost(), t.cost)
    };

    Row::new(vec![
        Cell::from(s.pid.to_string()),
        Cell::from(name_text),
        Cell::from(project_text),
        Cell::from(status_text).style(status_style),
        Cell::from(s.format_context_bar(6)).style(Style::default().fg(ctx_color)),
        Cell::from(cost_text).style(Style::default().fg(cost_color)),
        Cell::from(s.format_burn_rate()).style(Style::default().fg(burn_color)),
        Cell::from(s.format_elapsed()),
        Cell::from(format_last_user_age(s.last_user_message_ts))
            .style(Style::default().fg(t.text_muted)),
        Cell::from(format!("{:.1}", s.cpu_percent)),
        Cell::from(s.format_mem()),
        Cell::from(s.format_tokens()),
        Cell::from(s.format_sparkline()).style(Style::default().fg(t.sparkline)),
    ])
}

fn subagent_row(
    parent: &ClaudeSession,
    row: &SubagentBreakdown,
    app: &App,
    index: usize,
    total: usize,
) -> Row<'static> {
    let t = &app.theme;
    let branch = if index + 1 == total {
        "\u{2514}\u{2500} "
    } else {
        "\u{251c}\u{2500} "
    };
    let branch_text = format!("{branch}{}", row.display_label());
    let status_text = row.state_label();
    let status_style = match row.state {
        SubagentState::Active => Style::default().fg(t.status_processing),
        SubagentState::Completed => Style::default().fg(t.text_muted),
    };
    let row_style = Style::default().fg(t.text_muted);

    // Place the tree branch under whichever column holds the parent's
    // primary identifier, so the visual hierarchy reads cleanly.
    let (name_cell, project_cell) = if parent.session_name.is_empty() {
        (
            Cell::from("").style(row_style),
            Cell::from(branch_text).style(row_style),
        )
    } else {
        (
            Cell::from(branch_text).style(row_style),
            Cell::from("").style(row_style),
        )
    };

    Row::new(vec![
        Cell::from(""),
        name_cell,
        project_cell,
        Cell::from(status_text).style(status_style),
        Cell::from("-").style(row_style),
        Cell::from(row.format_cost()).style(Style::default().fg(t.cost)),
        Cell::from("-").style(row_style),
        Cell::from("-").style(row_style),
        Cell::from("-").style(row_style),
        Cell::from("-").style(row_style),
        Cell::from("-").style(row_style),
        Cell::from(row.format_tokens()).style(row_style),
        Cell::from("-").style(row_style),
    ])
}

/// Per-row decoration flags that determine where indicator prefixes/suffixes
/// attach in the Name/Project cell pair. Extracted as a struct so the text
/// builder below can be unit-tested without depending on `App`.
pub(crate) struct CellDecorations<'a> {
    pub file_conflict: bool,
    pub worktree_conflict: bool,
    pub recording: bool,
    pub health_icon: &'a str,
}

/// Produce the (Name, Project) cell text pair for a session row. Conflict
/// markers (`!F`/`!!`) always attach to Project — they describe the working
/// directory. Recording, subagent count, and health icon attach to Name when
/// the session has been renamed, else fall back to Project. An unnamed
/// session's Name cell is always empty.
pub(crate) fn build_name_and_project_text(
    s: &ClaudeSession,
    dec: &CellDecorations<'_>,
) -> (String, String) {
    let conflict_prefix = if dec.file_conflict {
        "!F "
    } else if dec.worktree_conflict {
        "!! "
    } else {
        ""
    };
    let rec_prefix = if dec.recording { "REC " } else { "" };
    let health_suffix = if dec.health_icon.is_empty() {
        String::new()
    } else {
        format!(" {}", dec.health_icon)
    };
    let subagent_suffix = if s.subagent_count > 0 {
        format!(" +{}", s.subagent_count)
    } else {
        String::new()
    };
    if s.session_name.is_empty() {
        (
            String::new(),
            format!(
                "{conflict_prefix}{rec_prefix}{}{subagent_suffix}{health_suffix}",
                s.project_name
            ),
        )
    } else {
        (
            format!(
                "{rec_prefix}{}{subagent_suffix}{health_suffix}",
                s.session_name
            ),
            format!("{conflict_prefix}{}", s.project_name),
        )
    }
}

/// Format the age of a "last user message" timestamp (unix epoch millis)
/// relative to wall-clock now. Returns a compact 1-5 char representation:
/// `—` for never, `Ns`/`Nm`/`Nh`/`Nd` otherwise. A future timestamp (clock
/// skew or invalid data) also renders as `—` rather than a negative age.
fn format_last_user_age(ts_ms: u64) -> String {
    if ts_ms == 0 {
        return "—".into();
    }
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    if ts_ms > now_ms {
        return "—".into();
    }
    let secs = (now_ms - ts_ms) / 1000;
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else if secs < 86_400 {
        format!("{}h", secs / 3600)
    } else {
        format!("{}d", secs / 86_400)
    }
}

fn format_token_count(n: u64) -> String {
    if n == 0 {
        return String::new();
    }
    if n >= 1_000_000_000_000 {
        format!("{:.1}T tok", n as f64 / 1_000_000_000_000.0)
    } else if n >= 1_000_000_000 {
        format!("{:.1}B tok", n as f64 / 1_000_000_000.0)
    } else if n >= 1_000_000 {
        format!("{:.1}M tok", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.0}k tok", n as f64 / 1_000.0)
    } else {
        format!("{n} tok")
    }
}

/// Title-bar variant that always renders a token count (including `0 tok`),
/// so the `week: <n> | month: <m>` panel doesn't get a lopsided empty slot
/// when one window is populated and the other isn't.
fn format_token_count_nonempty(n: u64) -> String {
    if n == 0 {
        "0 tok".to_string()
    } else {
        format_token_count(n)
    }
}

fn format_cost(v: f64) -> String {
    if v < 1.0 {
        format!("${v:.2}")
    } else {
        format!("${v:.1}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{ClaudeSession, RawSession};

    fn session_with(name: &str, project_cwd: &str, subagents: usize) -> ClaudeSession {
        let raw = RawSession {
            pid: 1,
            session_id: "s".into(),
            cwd: project_cwd.into(),
            started_at: 0,
            name: None,
        };
        let mut s = ClaudeSession::from_raw(raw);
        s.session_name = name.into();
        s.subagent_count = subagents;
        s
    }

    fn flags(
        file_conflict: bool,
        worktree_conflict: bool,
        recording: bool,
        health_icon: &str,
    ) -> CellDecorations<'_> {
        CellDecorations {
            file_conflict,
            worktree_conflict,
            recording,
            health_icon,
        }
    }

    #[test]
    fn named_session_plain() {
        let s = session_with("feat/foo", "/tmp/proj", 0);
        let (name, project) = build_name_and_project_text(&s, &flags(false, false, false, ""));
        assert_eq!(name, "feat/foo");
        assert_eq!(project, "proj");
    }

    #[test]
    fn unnamed_session_plain() {
        let s = session_with("", "/tmp/proj", 0);
        let (name, project) = build_name_and_project_text(&s, &flags(false, false, false, ""));
        assert_eq!(name, "");
        assert_eq!(project, "proj");
    }

    #[test]
    fn named_with_worktree_conflict_puts_marker_on_project() {
        let s = session_with("feat/foo", "/tmp/proj", 0);
        let (name, project) = build_name_and_project_text(&s, &flags(false, true, false, ""));
        assert_eq!(
            name, "feat/foo",
            "Name cell must not carry the conflict marker"
        );
        assert_eq!(project, "!! proj");
    }

    #[test]
    fn named_with_file_conflict_puts_fmarker_on_project() {
        let s = session_with("feat/foo", "/tmp/proj", 0);
        let (name, project) = build_name_and_project_text(&s, &flags(true, false, false, ""));
        assert_eq!(name, "feat/foo");
        assert_eq!(project, "!F proj");
    }

    #[test]
    fn named_with_recording_and_subagents_decorates_name() {
        let s = session_with("feat/foo", "/tmp/proj", 2);
        let (name, project) = build_name_and_project_text(&s, &flags(false, false, true, ""));
        assert_eq!(name, "REC feat/foo +2");
        assert_eq!(
            project, "proj",
            "Project cell stays plain when session-level decorations ride with Name"
        );
    }

    #[test]
    fn named_with_health_icon_on_name_only() {
        let s = session_with("feat/foo", "/tmp/proj", 0);
        let (name, project) = build_name_and_project_text(&s, &flags(false, false, false, "⚠"));
        assert_eq!(name, "feat/foo ⚠");
        assert_eq!(project, "proj");
    }

    #[test]
    fn unnamed_collects_every_decoration_on_project() {
        // Unnamed sessions have an empty Name cell, so every indicator falls
        // back onto the Project cell — matches pre-split behavior.
        let s = session_with("", "/tmp/proj", 3);
        let (name, project) = build_name_and_project_text(&s, &flags(false, true, true, "⚠"));
        assert_eq!(name, "");
        assert_eq!(project, "!! REC proj +3 ⚠");
    }

    #[test]
    fn named_with_conflict_and_session_decorations_splits_cleanly() {
        // Named + worktree conflict + recording + subagents + health should
        // split into conflict-on-Project and everything-else-on-Name.
        let s = session_with("feat/foo", "/tmp/proj", 1);
        let (name, project) = build_name_and_project_text(&s, &flags(false, true, true, "⚠"));
        assert_eq!(name, "REC feat/foo +1 ⚠");
        assert_eq!(project, "!! proj");
    }

    #[test]
    fn file_conflict_takes_precedence_over_worktree_conflict() {
        let s = session_with("feat/foo", "/tmp/proj", 0);
        let (_, project) = build_name_and_project_text(&s, &flags(true, true, false, ""));
        assert_eq!(project, "!F proj");
    }

    #[test]
    fn format_last_user_age_never() {
        assert_eq!(format_last_user_age(0), "—");
    }

    #[test]
    fn format_last_user_age_future_ts_is_never() {
        let future = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
            + 60_000;
        assert_eq!(format_last_user_age(future), "—");
    }

    #[test]
    fn format_last_user_age_buckets() {
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        assert_eq!(format_last_user_age(now_ms.saturating_sub(10_000)), "10s");
        assert_eq!(format_last_user_age(now_ms.saturating_sub(120_000)), "2m");
        assert_eq!(
            format_last_user_age(now_ms.saturating_sub(3 * 3_600_000)),
            "3h"
        );
        assert_eq!(
            format_last_user_age(now_ms.saturating_sub(2 * 86_400_000)),
            "2d"
        );
    }

    #[test]
    fn format_token_count_bucket_tiers() {
        assert_eq!(format_token_count(0), "");
        assert_eq!(format_token_count(42), "42 tok");
        assert_eq!(format_token_count(1_500), "2k tok");
        assert_eq!(format_token_count(47_300_000), "47.3M tok");
        assert_eq!(format_token_count(2_612_000_000), "2.6B tok");
        assert_eq!(format_token_count(169_100_000_000), "169.1B tok");
        assert_eq!(format_token_count(1_500_000_000_000), "1.5T tok");
        assert_eq!(format_token_count(42_000_000_000_000), "42.0T tok");
    }
}
