// TUI peers panel: every paired peer, and whether it has been heard from
// lately. Pairing outlives any connection, so a machine that's gone stays
// listed until `relay forget` or `relay prune`; the status says which ones
// are actually there.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};

use claudectl_core::runtime::{KnownPeer, PeerLiveness};
use claudectl_core::theme::Theme;

/// Render the peers panel in the TUI.
pub fn render_peers_panel(frame: &mut Frame, area: Rect, peers: &[KnownPeer], theme: &Theme) {
    let live = peers.iter().filter(|p| p.liveness.is_live()).count();
    let title = format!(" Peers ({live} live / {} paired) ", peers.len());
    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.border));

    if peers.is_empty() {
        let text = Paragraph::new("No paired peers. Use `claudectl relay invite` to get started.")
            .block(block)
            .style(Style::default().fg(theme.text_muted));
        frame.render_widget(text, area);
        return;
    }

    let w = peers.iter().map(|p| p.id.len()).max().unwrap_or(0);
    let lines: Vec<Line> = peers
        .iter()
        .map(|p| {
            let (icon, color) = match p.liveness {
                PeerLiveness::Live { .. } => ("●", theme.success),
                PeerLiveness::Stale { .. } => ("○", theme.context_warning),
                PeerLiveness::NeverHeard => ("○", theme.text_muted),
            };
            Line::from(vec![
                Span::styled(format!(" {icon} "), Style::default().fg(color)),
                Span::styled(
                    format!("{:<w$}  ", p.id),
                    Style::default()
                        .fg(theme.text_primary)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!("{:<6}", p.liveness.status()),
                    Style::default().fg(color),
                ),
                Span::styled(
                    format!("{:<17}", p.liveness.last_heard_cell()),
                    Style::default().fg(theme.text_primary),
                ),
                Span::styled(
                    p.addr.clone().unwrap_or_default(),
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
