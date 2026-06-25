// Full-screen Supervisor mode. When `app.show_supervisor` is set, the main
// draw loop hands the whole frame to `render_supervisor_screen` instead of the
// session table. It renders the coord task ledger — one row per supervisor
// task lifecycle (state, attempts, latest session) — so an operator can see
// tracked, verified work without dropping to `claudectl supervisor status`.
//
// Read-only for now (#368, increment 1). One-key retry/approve/cancel/drain
// and the cost + verifier-verdict columns are the next increment.

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, Paragraph};

use crate::app::App;
use claudectl_core::runtime::TaskSummary;
use claudectl_core::theme::Theme;

/// Color a task state by lifecycle phase, reusing the session status palette so
/// the dashboard and the supervisor panel read consistently.
fn state_color(state: &str, t: &Theme) -> Color {
    match state {
        "done" => t.status_finished,
        "needs_human" => t.status_needs_input,
        "running" | "verifying" | "assigned" => t.status_processing,
        "retrying" | "resuming" => t.status_waiting,
        "cancelled" => t.error,
        _ => t.text_muted, // pending / ready
    }
}

/// Last path segment of a session id / uuid, truncated for the narrow column.
fn short_session(id: &str) -> String {
    let tail = id.rsplit(['/', '-']).next().unwrap_or(id);
    tail.chars().take(8).collect()
}

pub fn render_supervisor_screen(frame: &mut Frame, area: Rect, app: &App) {
    let t = &app.theme;

    let title = Line::from(vec![
        Span::styled(" claudectl ", Style::default().fg(t.text_primary)),
        Span::styled(
            "│ Supervisor ",
            Style::default().fg(t.header).add_modifier(Modifier::BOLD),
        ),
    ]);
    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(t.border));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // column header
            Constraint::Min(1),    // task list
            Constraint::Length(1), // footer hint
        ])
        .split(inner);

    render_header(frame, layout[0], t);
    render_task_list(frame, layout[1], app);
    render_footer(frame, layout[2], app);
}

fn render_header(frame: &mut Frame, area: Rect, t: &Theme) {
    let header = format!(
        " {:<10} {:>6}  {:<28} {:<10} {:<10} {}",
        "STATE", "TRIES", "TASK", "ROLE", "SESSION", "UPDATED"
    );
    let p = Paragraph::new(Line::from(Span::styled(
        header,
        Style::default().fg(t.header).add_modifier(Modifier::BOLD),
    )));
    frame.render_widget(p, area);
}

fn render_task_list(frame: &mut Frame, area: Rect, app: &App) {
    let t = &app.theme;
    let tasks: &[TaskSummary] = &app.coord_tasks;

    if tasks.is_empty() {
        let empty = Paragraph::new(Line::from(Span::styled(
            "  No supervisor tasks. Submit with `claudectl supervisor run tasks.toml`.",
            Style::default().fg(t.text_muted),
        )));
        frame.render_widget(empty, area);
        return;
    }

    let selected = app.supervisor_selected.min(tasks.len().saturating_sub(1));
    let items: Vec<ListItem> = tasks
        .iter()
        .enumerate()
        .map(|(i, task)| {
            let is_sel = i == selected;
            let tries = format!("{}/{}", task.attempts, task.max_retries);
            let session = task
                .last_session_id
                .as_deref()
                .map(short_session)
                .unwrap_or_else(|| "—".into());
            let role = task.role.as_deref().unwrap_or("—");
            let updated = task
                .updated_at
                .split('T')
                .nth(1)
                .unwrap_or(&task.updated_at);
            let updated = updated.trim_end_matches('Z');

            let row = Line::from(vec![
                Span::raw(if is_sel { " ▸ " } else { "   " }),
                Span::styled(
                    format!("{:<8}", task.state),
                    Style::default()
                        .fg(state_color(&task.state, t))
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(format!("{tries:>6}  "), Style::default().fg(t.text_muted)),
                Span::styled(
                    format!("{:<28}", truncate(&task.name, 28)),
                    Style::default().fg(t.text_primary),
                ),
                Span::styled(
                    format!("{:<10} ", truncate(role, 10)),
                    Style::default().fg(t.text_muted),
                ),
                Span::styled(format!("{session:<10} "), Style::default().fg(t.text_muted)),
                Span::styled(updated.to_string(), Style::default().fg(t.text_muted)),
            ]);
            let style = if is_sel {
                Style::default().bg(t.border)
            } else {
                Style::default()
            };
            ListItem::new(row).style(style)
        })
        .collect();

    frame.render_widget(List::new(items), area);
}

fn render_footer(frame: &mut Frame, area: Rect, app: &App) {
    let t = &app.theme;
    if let Some(msg) = app
        .supervisor_status_msg
        .as_deref()
        .filter(|m| !m.is_empty())
    {
        let p = Paragraph::new(Line::from(Span::styled(
            format!("  {msg}"),
            Style::default().fg(t.success),
        )));
        frame.render_widget(p, area);
        return;
    }
    let hint = Line::from(vec![
        Span::styled("  j/k", Style::default().fg(t.highlight_key)),
        Span::styled(" move  ", Style::default().fg(t.text_muted)),
        Span::styled("r", Style::default().fg(t.highlight_key)),
        Span::styled(" refresh  ", Style::default().fg(t.text_muted)),
        Span::styled("Esc/T", Style::default().fg(t.highlight_key)),
        Span::styled(" close", Style::default().fg(t.text_muted)),
    ]);
    frame.render_widget(Paragraph::new(hint), area);
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let kept: String = s.chars().take(max.saturating_sub(1)).collect();
        format!("{kept}…")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_keeps_short_strings() {
        assert_eq!(truncate("short", 28), "short");
    }

    #[test]
    fn truncate_clips_long_strings_with_ellipsis() {
        let out = truncate("0123456789", 5);
        assert_eq!(out.chars().count(), 5);
        assert!(out.ends_with('…'));
    }

    #[test]
    fn short_session_takes_trailing_segment() {
        assert_eq!(short_session("path/to/sess-abcdef1234"), "abcdef12");
        assert_eq!(short_session("plainid"), "plainid");
    }

    #[test]
    fn state_color_distinguishes_terminal_states() {
        let t = Theme::from_mode(claudectl_core::theme::ThemeMode::Dark);
        assert_eq!(state_color("done", &t), t.status_finished);
        assert_eq!(state_color("needs_human", &t), t.status_needs_input);
        assert_eq!(state_color("cancelled", &t), t.error);
        // Unknown / pending falls back to muted.
        assert_eq!(state_color("pending", &t), t.text_muted);
    }
}
