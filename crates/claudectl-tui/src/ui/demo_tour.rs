//! Narrated guided-tour overlay (#373).
//!
//! Draws a centered narration card on top of the live demo dashboard so a
//! first-time user sees the real UI behind each explanation. Modeled on the
//! help overlay: `Clear` the region, then render a bordered `Paragraph`.

use ratatui::{
    Frame,
    layout::{Alignment, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph, Wrap},
};

use crate::app::App;
use crate::ui::help::centered_rect;

/// Render the current tour step. No-op when no tour is active.
pub fn render(frame: &mut Frame, area: Rect, app: &App) {
    let Some(tour) = &app.demo_tour else {
        return;
    };
    let t = &app.theme;
    let step = tour.step();
    let (pos, total) = tour.progress();

    // A short card near the bottom so the dashboard above stays visible.
    let popup = bottom_card(70, 34, area);
    frame.render_widget(Clear, popup);

    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::from(vec![
        Span::styled(
            format!(" {} ", step.title),
            Style::default().fg(t.header).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("  ({pos}/{total})"),
            Style::default().fg(t.text_muted),
        ),
    ]));
    lines.push(Line::from(""));
    for para in step.body.split("\n\n") {
        lines.push(Line::from(Span::styled(
            para.to_string(),
            Style::default().fg(t.text_primary),
        )));
        lines.push(Line::from(""));
    }
    lines.push(Line::from(vec![
        Span::styled("space/→", Style::default().fg(t.highlight_key)),
        Span::styled(" next   ", Style::default().fg(t.text_muted)),
        Span::styled("←", Style::default().fg(t.highlight_key)),
        Span::styled(" back   ", Style::default().fg(t.text_muted)),
        Span::styled("Esc", Style::default().fg(t.highlight_key)),
        Span::styled(" skip tour", Style::default().fg(t.text_muted)),
    ]));

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(t.header))
        .title(Span::styled(
            " claudectl demo — guided tour ",
            Style::default().fg(t.header).add_modifier(Modifier::BOLD),
        ))
        .title_alignment(Alignment::Center);

    let para = Paragraph::new(lines).block(block).wrap(Wrap { trim: true });
    frame.render_widget(para, popup);
}

/// A centered card occupying the lower portion of the screen, so the session
/// table above it stays on view while the narration explains it.
fn bottom_card(percent_x: u16, percent_y: u16, area: Rect) -> Rect {
    let full = centered_rect(percent_x, percent_y, area);
    // Push the card toward the bottom third of the available height.
    let y = area
        .y
        .saturating_add(area.height.saturating_sub(full.height).saturating_sub(1));
    Rect {
        x: full.x,
        y: y.min(area.y + area.height.saturating_sub(full.height)),
        width: full.width,
        height: full.height,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::App;
    use ratatui::{Terminal, backend::TestBackend};

    fn rendered_text(app: &App) -> String {
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| render(f, f.area(), app)).unwrap();
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect()
    }

    #[test]
    fn renders_current_step_without_panic() {
        let mut app = App::new();
        app.demo_tour = Some(crate::demo::DemoTour::new());
        let text = rendered_text(&app);
        assert!(text.contains("guided tour"), "missing title chrome");
        assert!(text.contains("Welcome to claudectl"), "missing step 1 body");
        assert!(text.contains("1/"), "missing progress indicator");
    }

    #[test]
    fn no_tour_renders_nothing() {
        let app = App::new();
        // No panic and an empty (space-only) buffer when no tour is active.
        let text = rendered_text(&app);
        assert!(!text.contains("guided tour"));
    }
}
