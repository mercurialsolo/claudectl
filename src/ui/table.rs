use ratatui::{
    Frame,
    layout::{Constraint, Layout, Direction, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Cell, Paragraph, Row, Table, TableState},
};

use crate::session::ClaudeSession;

pub fn render(
    frame: &mut Frame,
    area: Rect,
    sessions: &[ClaudeSession],
    table_state: &mut TableState,
    status_msg: &str,
    input_mode: bool,
    input_buffer: &str,
) {
    let has_status = !status_msg.is_empty() || input_mode;
    let chunks = if !has_status {
        vec![area]
    } else {
        Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(3), Constraint::Length(1)])
            .split(area)
            .to_vec()
    };

    let header_cells = [
        "PID", "Project", "Status", "Context", "Cost", "$/hr", "Elapsed", "CPU%", "MEM", "Tokens",
    ]
    .iter()
    .map(|h| Cell::from(*h).style(Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)));

    let header = Row::new(header_cells).height(1);

    let rows = sessions.iter().map(|s| {
        let status_style = Style::default().fg(s.status.color());

        // Color context bar based on usage
        let ctx_pct = s.context_percent();
        let ctx_color = if ctx_pct > 80.0 {
            Color::Red
        } else if ctx_pct > 50.0 {
            Color::Yellow
        } else {
            Color::Green
        };

        Row::new(vec![
            Cell::from(s.pid.to_string()),
            Cell::from(s.display_name().to_string()),
            Cell::from(s.status.to_string()).style(status_style),
            Cell::from(s.format_context_bar(6)).style(Style::default().fg(ctx_color)),
            Cell::from(s.format_cost()).style(Style::default().fg(Color::Yellow)),
            Cell::from(s.format_burn_rate()).style(Style::default().fg(
                if s.burn_rate_per_hr > 10.0 { Color::Red }
                else if s.burn_rate_per_hr > 1.0 { Color::Yellow }
                else { Color::DarkGray }
            )),
            Cell::from(s.format_elapsed()),
            Cell::from(format!("{:.1}", s.cpu_percent)),
            Cell::from(s.format_mem()),
            Cell::from(s.format_tokens()),
        ])
    });

    let widths = [
        Constraint::Length(7),    // PID
        Constraint::Min(10),      // Project (flex)
        Constraint::Length(12),   // Status
        Constraint::Length(13),   // Context bar
        Constraint::Length(8),    // Cost
        Constraint::Length(9),    // $/hr
        Constraint::Length(10),   // Elapsed
        Constraint::Length(6),    // CPU%
        Constraint::Length(5),    // MEM
        Constraint::Length(14),   // Tokens
    ];

    let count = sessions.len();
    let active = sessions
        .iter()
        .filter(|s| matches!(s.status, crate::session::SessionStatus::Processing | crate::session::SessionStatus::NeedsInput))
        .count();
    let total_cost: f64 = sessions.iter().map(|s| s.cost_usd).sum();
    let selected = table_state
        .selected()
        .map(|i| i + 1)
        .unwrap_or(0);

    let cost_str = if total_cost < 1.0 {
        format!("${total_cost:.2}")
    } else {
        format!("${total_cost:.1}")
    };

    let footer = Line::from(vec![
        Span::styled(
            format!(" {count} sessions ({active} active) "),
            Style::default().fg(Color::DarkGray),
        ),
        Span::styled(
            format!("${cost_str} "),
            Style::default().fg(Color::Yellow),
        ),
        Span::styled(
            format!("[{selected}/{count}]"),
            Style::default().fg(Color::DarkGray),
        ),
        Span::styled(
            "  q:quit  j/k:nav  Tab:go  y:approve  i:input  d:kill  r:refresh",
            Style::default().fg(Color::DarkGray),
        ),
    ]);

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
        .highlight_symbol("▶ ");

    frame.render_stateful_widget(table, area, table_state);

    // Status / input bar
    if chunks.len() > 1 {
        if input_mode {
            let msg = Paragraph::new(Line::from(vec![
                Span::styled(" > ", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
                Span::styled(input_buffer, Style::default().fg(Color::White)),
                Span::styled("_", Style::default().fg(Color::DarkGray)),
            ]));
            frame.render_widget(msg, chunks[1]);
        } else if !status_msg.is_empty() {
            let color = if status_msg.starts_with("Error") {
                Color::Red
            } else {
                Color::Green
            };
            let msg = Paragraph::new(Span::styled(
                format!(" {status_msg}"),
                Style::default().fg(color),
            ));
            frame.render_widget(msg, chunks[1]);
        }
    }
}
