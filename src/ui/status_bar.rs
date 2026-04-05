use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
};

use crate::app::App;

pub fn render_status_bar(frame: &mut Frame, area: Rect, app: &App) {
    if app.input_mode {
        let msg = Paragraph::new(Line::from(vec![
            Span::styled(
                " > ",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(&*app.input_buffer, Style::default().fg(Color::White)),
            Span::styled("_", Style::default().fg(Color::DarkGray)),
        ]));
        frame.render_widget(msg, area);
    } else if !app.status_msg.is_empty() {
        let color = if app.status_msg.starts_with("Error") {
            Color::Red
        } else {
            Color::Green
        };
        let msg = Paragraph::new(Span::styled(
            format!(" {}", app.status_msg),
            Style::default().fg(color),
        ));
        frame.render_widget(msg, area);
    }
}
