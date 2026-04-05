use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph, Wrap},
};

/// Render a centered help popup showing all keybindings and status colors.
pub fn render_help_overlay(frame: &mut Frame, area: Rect) {
    let popup = centered_rect(60, 70, area);

    // Clear the area behind the popup
    frame.render_widget(Clear, popup);

    let help_lines = vec![
        Line::from(Span::styled(
            " Keybindings",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(vec![
            Span::styled("  j/k ", Style::default().fg(Color::Yellow)),
            Span::raw("or "),
            Span::styled("Up/Down ", Style::default().fg(Color::Yellow)),
            Span::raw("  Navigate sessions"),
        ]),
        Line::from(vec![
            Span::styled("  Tab/Enter      ", Style::default().fg(Color::Yellow)),
            Span::raw("  Switch to session terminal"),
        ]),
        Line::from(vec![
            Span::styled("  y              ", Style::default().fg(Color::Yellow)),
            Span::raw("  Approve (send Enter to NeedsInput)"),
        ]),
        Line::from(vec![
            Span::styled("  i              ", Style::default().fg(Color::Yellow)),
            Span::raw("  Input mode (type text to session)"),
        ]),
        Line::from(vec![
            Span::styled("  d/x            ", Style::default().fg(Color::Yellow)),
            Span::raw("  Kill session (double-tap to confirm)"),
        ]),
        Line::from(vec![
            Span::styled("  s              ", Style::default().fg(Color::Yellow)),
            Span::raw("  Cycle sort column"),
        ]),
        Line::from(vec![
            Span::styled("  a              ", Style::default().fg(Color::Yellow)),
            Span::raw("  Toggle auto-approve (double-tap)"),
        ]),
        Line::from(vec![
            Span::styled("  r              ", Style::default().fg(Color::Yellow)),
            Span::raw("  Force refresh"),
        ]),
        Line::from(vec![
            Span::styled("  ?              ", Style::default().fg(Color::Yellow)),
            Span::raw("  Toggle this help"),
        ]),
        Line::from(vec![
            Span::styled("  q/Esc          ", Style::default().fg(Color::Yellow)),
            Span::raw("  Quit"),
        ]),
        Line::from(""),
        Line::from(Span::styled(
            " Status Colors",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(vec![
            Span::styled("  Needs Input ", Style::default().fg(Color::Magenta)),
            Span::raw("  Blocked on user approval/input"),
        ]),
        Line::from(vec![
            Span::styled("  Processing  ", Style::default().fg(Color::Green)),
            Span::raw("  Actively generating or running tools"),
        ]),
        Line::from(vec![
            Span::styled("  Waiting     ", Style::default().fg(Color::Yellow)),
            Span::raw("  Done responding, awaiting next prompt"),
        ]),
        Line::from(vec![
            Span::styled("  Idle        ", Style::default().fg(Color::DarkGray)),
            Span::raw("  No recent activity"),
        ]),
        Line::from(vec![
            Span::styled("  Finished    ", Style::default().fg(Color::Red)),
            Span::raw("  Process exited"),
        ]),
        Line::from(""),
        Line::from(Span::styled(
            " Indicators",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(vec![
            Span::styled("  *  ", Style::default().fg(Color::Yellow)),
            Span::raw("after status = auto-approve enabled"),
        ]),
        Line::from(vec![
            Span::styled("  +N ", Style::default().fg(Color::Yellow)),
            Span::raw("after project = N sub-agents running"),
        ]),
        Line::from(""),
        Line::from(Span::styled(
            "  Press any key to dismiss",
            Style::default().fg(Color::DarkGray),
        )),
    ];

    let block = Block::default()
        .title(" Help ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan));

    let paragraph = Paragraph::new(help_lines)
        .block(block)
        .wrap(Wrap { trim: false });

    frame.render_widget(paragraph, popup);
}

/// Return a centered Rect within `r` using the given percentage of width and height.
pub fn centered_rect(percent_x: u16, percent_y: u16, r: Rect) -> Rect {
    let popup_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(r);

    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(popup_layout[1])[1]
}
