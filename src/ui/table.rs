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
) {
    let chunks = if status_msg.is_empty() {
        vec![area]
    } else {
        Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(3), Constraint::Length(1)])
            .split(area)
            .to_vec()
    };

    let header_cells = [
        "PID", "Project", "Status", "Model", "TTY", "Elapsed", "CPU%", "MEM", "Cost", "Tokens",
    ]
    .iter()
    .map(|h| Cell::from(*h).style(Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)));

    let header = Row::new(header_cells).height(1);

    let rows = sessions.iter().map(|s| {
        let status_style = Style::default().fg(s.status.color());

        Row::new(vec![
            Cell::from(s.pid.to_string()),
            Cell::from(s.display_name().to_string()),
            Cell::from(s.status.to_string()).style(status_style),
            Cell::from(s.model.clone()).style(Style::default().fg(Color::DarkGray)),
            Cell::from(s.tty.clone()),
            Cell::from(s.format_elapsed()),
            Cell::from(format!("{:.1}", s.cpu_percent)),
            Cell::from(s.format_mem()),
            Cell::from(s.format_cost()).style(Style::default().fg(Color::Yellow)),
            Cell::from(s.format_tokens()),
        ])
    });

    let widths = [
        Constraint::Length(7),    // PID
        Constraint::Min(12),      // Project (flex)
        Constraint::Length(12),   // Status
        Constraint::Length(11),   // Model
        Constraint::Length(9),    // TTY
        Constraint::Length(10),   // Elapsed
        Constraint::Length(7),    // CPU%
        Constraint::Length(7),    // MEM
        Constraint::Length(8),    // Cost
        Constraint::Length(14),   // Tokens
    ];

    let count = sessions.len();
    let active = sessions
        .iter()
        .filter(|s| matches!(s.status, crate::session::SessionStatus::Processing | crate::session::SessionStatus::Paused))
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
            "  q:quit  j/k:nav  Tab:switch  d:kill  ?:help",
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

    // Status message bar
    if !status_msg.is_empty() && chunks.len() > 1 {
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
