use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Cell, Paragraph, Row, Table},
};

use crate::app::{App, SORT_COLUMNS};
use crate::session::SessionStatus;

use super::detail::render_detail_panel;
use super::help::render_help_overlay;
use super::status_bar::render_status_bar;

pub fn render(frame: &mut Frame, area: Rect, app: &App) {
    let has_status = !app.status_msg.is_empty() || app.input_mode || app.launch_mode;
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
    if app.sessions.is_empty() {
        let empty_lines = vec![
            Line::from(""),
            Line::from(""),
            Line::from(Span::styled(
                "No active Claude Code sessions found.",
                Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(""),
            Line::from(vec![
                Span::styled("  Press ", Style::default().fg(Color::DarkGray)),
                Span::styled(
                    "n",
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    " to launch a new session, or start ",
                    Style::default().fg(Color::DarkGray),
                ),
                Span::styled(
                    "claude",
                    Style::default()
                        .fg(Color::Green)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    " in another terminal.",
                    Style::default().fg(Color::DarkGray),
                ),
            ]),
            Line::from(""),
            Line::from(vec![
                Span::styled("  Press ", Style::default().fg(Color::DarkGray)),
                Span::styled(
                    "?",
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(" for help.", Style::default().fg(Color::DarkGray)),
            ]),
        ];

        let block = Block::default()
            .title(" claudectl ")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::DarkGray));

        let empty_widget = Paragraph::new(empty_lines)
            .block(block)
            .alignment(Alignment::Center);

        frame.render_widget(empty_widget, chunks[0]);

        if has_status && chunks.len() > 1 {
            render_status_bar(frame, chunks[1], app);
        }

        if app.show_help {
            render_help_overlay(frame, area);
        }
        return;
    }

    // Build header with sort indicator
    let header_names = [
        "PID", "Project", "Status", "Context", "Cost", "$/hr", "Elapsed", "CPU%", "MEM", "In/Out",
        "Activity",
    ];

    // Map sort_column index to header index:
    // 0=Status->2, 1=Context->3, 2=Cost->4, 3=$/hr->5, 4=Elapsed->6
    let sort_header_idx = match app.sort_column {
        0 => 2, // Status
        1 => 3, // Context
        2 => 4, // Cost
        3 => 5, // $/hr
        4 => 6, // Elapsed
        _ => usize::MAX,
    };

    let header_cells = header_names.iter().enumerate().map(|(i, h)| {
        let label = if i == sort_header_idx {
            format!("{h} \u{25bc}") // ▼ sort indicator
        } else {
            (*h).to_string()
        };
        Cell::from(label).style(
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )
    });

    let header = Row::new(header_cells).height(1);

    let rows: Vec<Row> = if app.grouped_view {
        let groups = app.project_groups();
        let mut rows = Vec::new();
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
                Cell::from(header_text).style(
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ),
            ];
            for _ in 2..11 {
                cells.push(Cell::from(""));
            }
            rows.push(Row::new(cells));

            // Session rows under this group
            for s in app.sessions.iter().filter(|s| s.project_name == group.name) {
                rows.push(session_row(s, app));
            }
        }
        rows
    } else {
        app.sessions.iter().map(|s| session_row(s, app)).collect()
    };

    let widths = [
        Constraint::Length(7),  // PID
        Constraint::Min(10),    // Project (flex)
        Constraint::Length(14), // Status (wider for * indicator)
        Constraint::Length(13), // Context bar
        Constraint::Length(8),  // Cost
        Constraint::Length(9),  // $/hr
        Constraint::Length(10), // Elapsed
        Constraint::Length(6),  // CPU%
        Constraint::Length(5),  // MEM
        Constraint::Length(14), // Tokens
        Constraint::Length(16), // Activity sparkline
    ];

    let count = app.sessions.len();
    let active = app
        .sessions
        .iter()
        .filter(|s| {
            matches!(
                s.status,
                SessionStatus::Processing | SessionStatus::NeedsInput
            )
        })
        .count();
    let total_cost: f64 = app.sessions.iter().map(|s| s.cost_usd).sum();
    let selected = app.table_state.selected().map(|i| i + 1).unwrap_or(0);

    let cost_str = if total_cost < 1.0 {
        format!("${total_cost:.2}")
    } else {
        format!("${total_cost:.1}")
    };

    let sort_name = SORT_COLUMNS[app.sort_column];

    let mut footer_spans = vec![
        Span::styled(
            format!(" {count} sessions ({active} active) "),
            Style::default().fg(Color::DarkGray),
        ),
        Span::styled(format!("{cost_str} "), Style::default().fg(Color::Yellow)),
        Span::styled(
            format!("[{selected}/{count}]"),
            Style::default().fg(Color::DarkGray),
        ),
    ];

    if app.debug {
        footer_spans.push(Span::styled(
            format!("  {}", app.debug_timings.format()),
            Style::default().fg(Color::Cyan),
        ));
    } else {
        // Contextual hints based on selected session state
        let hint = match app.selected_session().map(|s| s.status) {
            Some(SessionStatus::NeedsInput) => {
                "  y:approve i:type Tab:go d:kill ?:help".to_string()
            }
            _ => {
                format!(
                    "  q:quit j/k:nav Tab:go y:approve i:input d:kill s:sort({sort_name}) ?:help"
                )
            }
        };
        footer_spans.push(Span::styled(hint, Style::default().fg(Color::DarkGray)));
    }

    let footer = Line::from(footer_spans);

    let block = Block::default()
        .title(" claudectl ")
        .title_bottom(footer)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray));

    let table = Table::new(rows, widths)
        .header(header)
        .block(block)
        .row_highlight_style(
            Style::default()
                .add_modifier(Modifier::REVERSED)
                .fg(Color::White),
        )
        .highlight_symbol("\u{25b6} "); // ▶

    frame.render_stateful_widget(table, chunks[0], &mut app.table_state.clone());

    // Detail panel
    let mut next_chunk = 1;
    if show_detail {
        if let Some(session) = app.selected_session() {
            render_detail_panel(frame, chunks[next_chunk], session);
        }
        next_chunk += 1;
    }

    // Status / input bar
    if has_status && next_chunk < chunks.len() {
        render_status_bar(frame, chunks[next_chunk], app);
    }

    // Help overlay
    if app.show_help {
        render_help_overlay(frame, area);
    }
}

fn session_row<'a>(s: &'a crate::session::ClaudeSession, app: &'a App) -> Row<'a> {
    let status_style = Style::default().fg(s.status.color());

    let status_text = if app.auto_approve.contains(&s.pid) {
        format!("{}*", s.status)
    } else {
        s.status.to_string()
    };

    let project_text = if s.subagent_count > 0 {
        format!("{} +{}", s.display_name(), s.subagent_count)
    } else {
        s.display_name().to_string()
    };

    let ctx_pct = s.context_percent();
    let ctx_color = if ctx_pct > 80.0 {
        Color::Red
    } else if ctx_pct > 50.0 {
        Color::Yellow
    } else {
        Color::Green
    };

    let burn_color = if s.burn_rate_per_hr > 10.0 {
        Color::Red
    } else if s.burn_rate_per_hr > 1.0 {
        Color::Yellow
    } else {
        Color::DarkGray
    };

    // Cost cell with budget indicator
    let (cost_text, cost_color) = if let Some(budget) = app.budget_usd {
        let pct = s.cost_usd / budget * 100.0;
        let text = format!("{} {:.0}%", s.format_cost(), pct);
        let color = if pct >= 100.0 {
            Color::Red
        } else if pct >= 80.0 {
            Color::LightRed
        } else {
            Color::Yellow
        };
        (text, color)
    } else {
        (s.format_cost(), Color::Yellow)
    };

    Row::new(vec![
        Cell::from(s.pid.to_string()),
        Cell::from(project_text),
        Cell::from(status_text).style(status_style),
        Cell::from(s.format_context_bar(6)).style(Style::default().fg(ctx_color)),
        Cell::from(cost_text).style(Style::default().fg(cost_color)),
        Cell::from(s.format_burn_rate()).style(Style::default().fg(burn_color)),
        Cell::from(s.format_elapsed()),
        Cell::from(format!("{:.1}", s.cpu_percent)),
        Cell::from(s.format_mem()),
        Cell::from(s.format_tokens()),
        Cell::from(s.format_sparkline()).style(Style::default().fg(Color::Blue)),
    ])
}
