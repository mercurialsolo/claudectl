use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph, Wrap},
};

use crate::app::App;

/// Render a centered help popup showing all keybindings and status colors.
pub fn render_help_overlay(frame: &mut Frame, area: Rect, app: &App) {
    let t = &app.theme;
    let popup = centered_rect(60, 70, area);

    // Clear the area behind the popup
    frame.render_widget(Clear, popup);

    let help_lines = vec![
        Line::from(Span::styled(
            " Keybindings",
            Style::default().fg(t.header).add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(vec![
            Span::styled("  j/k ", Style::default().fg(t.highlight_key)),
            Span::raw("or "),
            Span::styled("Up/Down ", Style::default().fg(t.highlight_key)),
            Span::raw("  Navigate sessions"),
        ]),
        Line::from(vec![
            Span::styled("  Enter          ", Style::default().fg(t.highlight_key)),
            Span::raw("  Toggle detail panel for selected session"),
        ]),
        Line::from(vec![
            Span::styled("  Tab            ", Style::default().fg(t.highlight_key)),
            Span::raw("  Switch to session terminal"),
        ]),
        Line::from(vec![
            Span::styled("  y              ", Style::default().fg(t.highlight_key)),
            Span::raw("  Approve (send Enter to NeedsInput)"),
        ]),
        Line::from(vec![
            Span::styled("  i              ", Style::default().fg(t.highlight_key)),
            Span::raw("  Input mode (type text to session)"),
        ]),
        Line::from(vec![
            Span::styled("  d/x            ", Style::default().fg(t.highlight_key)),
            Span::raw("  Kill session (double-tap to confirm)"),
        ]),
        Line::from(vec![
            Span::styled("  s              ", Style::default().fg(t.highlight_key)),
            Span::raw("  Cycle sort column"),
        ]),
        Line::from(vec![
            Span::styled("  f              ", Style::default().fg(t.highlight_key)),
            Span::raw("  Cycle status filter"),
        ]),
        Line::from(vec![
            Span::styled("  v              ", Style::default().fg(t.highlight_key)),
            Span::raw("  Cycle focus filter"),
        ]),
        Line::from(vec![
            Span::styled("  /              ", Style::default().fg(t.highlight_key)),
            Span::raw("  Search project/model/session text"),
        ]),
        Line::from(vec![
            Span::styled("  z              ", Style::default().fg(t.highlight_key)),
            Span::raw("  Clear all active filters"),
        ]),
        Line::from(vec![
            Span::styled("  a              ", Style::default().fg(t.highlight_key)),
            Span::raw("  Toggle auto-approve (double-tap)"),
        ]),
        Line::from(vec![
            Span::styled("  n              ", Style::default().fg(t.highlight_key)),
            Span::raw(
                "  Launch wizard for cwd, prompt, and resume (GNOME Terminal/tmux/Kitty/WezTerm/Windows Terminal on WSL)",
            ),
        ]),
        Line::from(vec![
            Span::styled("  Enter/Tab      ", Style::default().fg(t.highlight_key)),
            Span::raw("  In launch wizard: next field / move fields"),
        ]),
        Line::from(vec![
            Span::styled("  Ctrl+Enter     ", Style::default().fg(t.highlight_key)),
            Span::raw("  In launch wizard: launch immediately"),
        ]),
        Line::from(vec![
            Span::styled("  c              ", Style::default().fg(t.highlight_key)),
            Span::raw("  Send /compact to session (when idle)"),
        ]),
        Line::from(vec![
            Span::styled("  R              ", Style::default().fg(t.highlight_key)),
            Span::raw("  Record session highlight reel (toggle)"),
        ]),
        Line::from(vec![
            Span::styled("  Ctrl+b         ", Style::default().fg(t.highlight_key)),
            Span::raw("  Toggle brain on/off"),
        ]),
        Line::from(vec![
            Span::styled("  Ctrl+r         ", Style::default().fg(t.highlight_key)),
            Span::raw("  Bind agent-bus role to selected session"),
        ]),
        Line::from(vec![
            Span::styled("  g              ", Style::default().fg(t.highlight_key)),
            Span::raw("  Toggle grouped view by project"),
        ]),
        Line::from(vec![
            Span::styled("  p              ", Style::default().fg(t.highlight_key)),
            Span::raw("  Toggle peers panel (relay feature)"),
        ]),
        Line::from(vec![
            Span::styled("  r              ", Style::default().fg(t.highlight_key)),
            Span::raw("  Force refresh"),
        ]),
        Line::from(vec![
            Span::styled("  K              ", Style::default().fg(t.highlight_key)),
            Span::raw("  Skills & Hive — list skills, share to hive, start/invite/join"),
        ]),
        Line::from(vec![
            Span::styled("  M              ", Style::default().fg(t.highlight_key)),
            Span::raw("  Brain Metrics — scorecard + interactive review/teach loop"),
        ]),
        Line::from(vec![
            Span::styled("  ?              ", Style::default().fg(t.highlight_key)),
            Span::raw("  Toggle this help"),
        ]),
        Line::from(vec![
            Span::styled("  q/Esc          ", Style::default().fg(t.highlight_key)),
            Span::raw("  Quit"),
        ]),
        Line::from(""),
        Line::from(Span::styled(
            " Status Colors",
            Style::default().fg(t.header).add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(vec![
            Span::styled("  Needs Input ", Style::default().fg(t.status_needs_input)),
            Span::raw("  Blocked on user approval/input"),
        ]),
        Line::from(vec![
            Span::styled("  Processing  ", Style::default().fg(t.status_processing)),
            Span::raw("  Actively generating or running tools"),
        ]),
        Line::from(vec![
            Span::styled("  Waiting     ", Style::default().fg(t.status_waiting)),
            Span::raw("  Done responding, awaiting next prompt"),
        ]),
        Line::from(vec![
            Span::styled("  Unknown     ", Style::default().fg(t.status_unknown)),
            Span::raw("  Session is alive but transcript telemetry is unavailable"),
        ]),
        Line::from(vec![
            Span::styled("  Idle        ", Style::default().fg(t.status_idle)),
            Span::raw("  No recent activity"),
        ]),
        Line::from(vec![
            Span::styled("  Finished    ", Style::default().fg(t.status_finished)),
            Span::raw("  Process exited"),
        ]),
        Line::from(""),
        Line::from(Span::styled(
            " Indicators",
            Style::default().fg(t.header).add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(vec![
            Span::styled("  *  ", Style::default().fg(t.highlight_key)),
            Span::raw("after status = auto-approve enabled"),
        ]),
        Line::from(vec![
            Span::styled("  +N ", Style::default().fg(t.highlight_key)),
            Span::raw("after project = N sub-agents tracked"),
        ]),
        Line::from(vec![
            Span::styled("  !! ", Style::default().fg(t.highlight_key)),
            Span::raw("before project = directory conflict"),
        ]),
        Line::from(vec![
            Span::styled("  (Xm Xs) ", Style::default().fg(t.highlight_key)),
            Span::raw("after Needs Input = wait time"),
        ]),
        Line::from(vec![
            Span::styled("  filters ", Style::default().fg(t.highlight_key)),
            Span::raw("in footer/status bar = active triage filters"),
        ]),
        Line::from(""),
        Line::from(Span::styled(
            " Current Terminal",
            Style::default().fg(t.header).add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(format!(
            "  {}",
            claudectl_core::terminals::help_capability_summary()
        )),
        Line::from(vec![
            Span::raw("  Run "),
            Span::styled("claudectl --doctor", Style::default().fg(t.highlight_key)),
            Span::raw(" for prerequisite checks and setup guidance."),
        ]),
        Line::from(""),
        Line::from(Span::styled(
            "  Press any key to dismiss",
            Style::default().fg(t.text_muted),
        )),
    ];

    let block = Block::default()
        .title(" Help ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(t.header));

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
