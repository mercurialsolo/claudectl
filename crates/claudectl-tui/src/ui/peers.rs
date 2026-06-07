#![allow(dead_code)]
// TUI peers panel: shows connected peers with trust levels and knowledge counts.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};

use claudectl_core::theme::Theme;

/// Info needed to render a peer in the panel.
#[derive(Debug, Clone)]
pub struct PeerDisplayInfo {
    pub peer_id: String,
    pub state: String,
    pub trust: f64,
    pub units_sent: u32,
    pub units_received: u32,
    pub session_count: u32,
}

/// Render the peers panel in the TUI.
pub fn render_peers_panel(frame: &mut Frame, area: Rect, peers: &[PeerDisplayInfo], theme: &Theme) {
    let title = format!(" Peers ({}) ", peers.len());
    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.border));

    if peers.is_empty() {
        let text = Paragraph::new("No connected peers. Use `claudectl relay pair` to get started.")
            .block(block)
            .style(Style::default().fg(theme.text_muted));
        frame.render_widget(text, area);
        return;
    }

    let lines: Vec<Line> = peers
        .iter()
        .map(|p| {
            let state_icon = match p.state.as_str() {
                "connected" => "●",
                "connecting" => "○",
                _ => "○",
            };
            let state_color = match p.state.as_str() {
                "connected" => theme.success,
                "connecting" => theme.context_warning,
                _ => theme.text_muted,
            };

            Line::from(vec![
                Span::styled(format!(" {state_icon} "), Style::default().fg(state_color)),
                Span::styled(
                    format!("{:<16}", p.peer_id),
                    Style::default()
                        .fg(theme.text_primary)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(format!("{:<14}", p.state), Style::default().fg(state_color)),
                Span::styled(
                    format!("trust:{:.1}  ", p.trust),
                    Style::default().fg(theme.text_primary),
                ),
                Span::styled(
                    format!("{}s  ", p.session_count),
                    Style::default().fg(theme.text_primary),
                ),
                Span::styled(
                    format!("↑{} ↓{} kb", p.units_sent, p.units_received),
                    Style::default().fg(theme.text_muted),
                ),
            ])
        })
        .collect();

    let paragraph = Paragraph::new(lines)
        .block(block)
        .style(Style::default().fg(theme.text_primary));
    frame.render_widget(paragraph, area);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn peer_display_info_creation() {
        let info = PeerDisplayInfo {
            peer_id: "test-peer".into(),
            state: "connected".into(),
            trust: 0.8,
            units_sent: 12,
            units_received: 8,
            session_count: 3,
        };
        assert_eq!(info.peer_id, "test-peer");
        assert_eq!(info.trust, 0.8);
        assert_eq!(info.session_count, 3);
    }
}
