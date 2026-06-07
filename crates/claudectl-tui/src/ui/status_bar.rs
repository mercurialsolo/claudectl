use ratatui::{
    Frame,
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
};

use crate::app::App;

pub fn render_status_bar(frame: &mut Frame, area: Rect, app: &App) {
    let t = &app.theme;
    if app.search_mode {
        let msg = Paragraph::new(Line::from(vec![
            Span::styled(
                " / ",
                Style::default()
                    .fg(t.highlight_key)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(&*app.search_buffer, Style::default().fg(t.text_primary)),
            Span::styled("_", Style::default().fg(t.text_muted)),
        ]));
        frame.render_widget(msg, area);
    } else if app.launch_mode {
        let msg = Paragraph::new(Line::from(vec![
            Span::styled(
                format!(" new[{}]> ", app.launch_form.field.label()),
                Style::default().fg(t.success).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                app.launch_form.active_buffer(),
                Style::default().fg(t.text_primary),
            ),
            Span::styled("_", Style::default().fg(t.text_muted)),
            Span::styled(
                format!("  {}", app.launch_form.summary()),
                Style::default().fg(t.text_muted),
            ),
            Span::styled(
                "  Enter next  Ctrl+Enter launch",
                Style::default().fg(t.text_muted),
            ),
        ]));
        frame.render_widget(msg, area);
    } else if app.input_mode {
        let msg = Paragraph::new(Line::from(vec![
            Span::styled(
                " > ",
                Style::default()
                    .fg(t.input_accent)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(&*app.input_buffer, Style::default().fg(t.text_primary)),
            Span::styled("_", Style::default().fg(t.text_muted)),
        ]));
        frame.render_widget(msg, area);
    } else if app.role_bind_mode {
        // #307 role-bind prompt — same shape as the input prompt but with a
        // distinct prefix so the operator sees they're naming a role, not
        // sending text to the session.
        let msg = Paragraph::new(Line::from(vec![
            Span::styled(
                " role> ",
                Style::default()
                    .fg(t.input_accent)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(&*app.role_bind_buffer, Style::default().fg(t.text_primary)),
            Span::styled("_", Style::default().fg(t.text_muted)),
        ]));
        frame.render_widget(msg, area);
    } else if app.idle_mode_active {
        let idle_mins = app.last_user_interaction.elapsed().as_secs() / 60;
        let tasks = if app.idle_tasks_launched.is_empty() {
            "no tasks running".to_string()
        } else {
            format!("{} task(s) running", app.idle_tasks_launched.len())
        };
        let msg = Paragraph::new(Span::styled(
            format!(" Idle ({idle_mins}m) | {tasks}"),
            Style::default().fg(t.text_muted),
        ));
        frame.render_widget(msg, area);
    } else if !app.status_msg.is_empty() {
        let color = if app.status_msg.starts_with("Error") {
            t.error
        } else {
            t.success
        };
        let msg = Paragraph::new(Span::styled(
            format!(" {}", app.status_msg),
            Style::default().fg(color),
        ));
        frame.render_widget(msg, area);
    } else if app.has_active_filters() {
        let msg = Paragraph::new(Span::styled(
            format!(" {}", app.filter_summary()),
            Style::default().fg(t.header),
        ));
        frame.render_widget(msg, area);
    } else if !app.session_recordings.is_empty() {
        let count = app.session_recordings.len();
        let names: Vec<&str> = app
            .session_recordings
            .keys()
            .filter_map(|pid| {
                app.sessions
                    .iter()
                    .find(|s| s.pid == *pid)
                    .map(|s| s.display_name())
            })
            .collect();
        let label = names.join(", ");
        let text = if count == 1 {
            format!(" REC {label}  (R to stop)")
        } else {
            format!(" REC {count} sessions: {label}  (R to stop)")
        };
        let msg = Paragraph::new(Span::styled(
            text,
            Style::default().fg(t.error).add_modifier(Modifier::BOLD),
        ));
        frame.render_widget(msg, area);
    } else if let Some(ref driver) = app.brain_driver {
        if driver.pending_count() > 0 {
            let count = driver.pending_count();
            let label = if count == 1 {
                "1 suggestion".into()
            } else {
                format!("{count} suggestions")
            };
            let text = format!(" Brain: {label} pending  (b accept / B reject)");
            let msg = Paragraph::new(Span::styled(
                text,
                Style::default().fg(t.header).add_modifier(Modifier::BOLD),
            ));
            frame.render_widget(msg, area);
        } else {
            use claudectl_core::runtime::BrainGateMode;
            let (label, color) = match app.runtime.brain.gate_mode() {
                BrainGateMode::Off => ("Brain: off", t.text_muted),
                BrainGateMode::Auto => ("Brain: auto", t.header),
                BrainGateMode::On => ("Brain: on", t.success),
            };
            let msg = Paragraph::new(Span::styled(
                format!(" {label}  (Ctrl+b toggle)"),
                Style::default().fg(color),
            ));
            frame.render_widget(msg, area);
        }
    }
}
