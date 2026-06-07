// Full-screen Skills & Hive mode. When `app.show_skills` is set, the main
// draw loop hands the whole frame to `render_skills_screen` instead of the
// session table. Two tabs: Skills (discovered Claude Code skills + share)
// and Hive (identity, peers, invite, join, start listener). All hive-side
// actions resolve to `claudectl relay …` subprocesses so the TUI event loop
// stays responsive.

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap};

use crate::app::{App, SkillsTab};
use claudectl_core::skills::DiscoveredSkill;
use claudectl_core::theme::Theme;

pub fn render_skills_screen(frame: &mut Frame, area: Rect, app: &App) {
    let t = &app.theme;

    let title = Line::from(vec![
        Span::styled(" claudectl ", Style::default().fg(t.text_primary)),
        Span::styled(
            "│ Skills & Hive ",
            Style::default().fg(t.header).add_modifier(Modifier::BOLD),
        ),
    ]);
    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(t.header));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    // Footer height adapts to whether there's a transient status message;
    // either way it stays anchored to the bottom because the body uses Min.
    let footer_height = if app
        .skills_status_msg
        .as_deref()
        .map(|m| !m.is_empty())
        .unwrap_or(false)
    {
        2
    } else {
        1
    };

    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),             // tab row
            Constraint::Length(2),             // header
            Constraint::Min(3),                // body (list + detail)
            Constraint::Length(footer_height), // hint strip
        ])
        .split(inner);

    render_tab_row(frame, layout[0], app);
    render_status_header(frame, layout[1], app);
    match app.skills_tab {
        SkillsTab::Skills => render_skills_body(frame, layout[2], app),
        SkillsTab::Hive => render_hive_body(frame, layout[2], app),
    }
    render_footer(frame, layout[3], app);
}

fn render_tab_row(frame: &mut Frame, area: Rect, app: &App) {
    let t = &app.theme;
    let mk = |label: &str, active: bool| {
        let style = if active {
            Style::default()
                .fg(t.header)
                .add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
        } else {
            Style::default().fg(t.text_muted)
        };
        Span::styled(format!("  {label}  "), style)
    };
    let line = Line::from(vec![
        mk("Skills", app.skills_tab == SkillsTab::Skills),
        Span::raw("│"),
        mk("Hive", app.skills_tab == SkillsTab::Hive),
        Span::styled("    Tab to switch", Style::default().fg(t.text_muted)),
    ]);
    frame.render_widget(Paragraph::new(line), area);
}

fn render_status_header(frame: &mut Frame, area: Rect, app: &App) {
    let t = &app.theme;
    let hive_status = if cfg!(feature = "hive") {
        "hive: on"
    } else {
        "hive: disabled (build feature off)"
    };
    let relay_status = if cfg!(feature = "relay") {
        if app.hive_listener_running {
            "relay: serving"
        } else {
            "relay: idle"
        }
    } else {
        "relay: not built"
    };

    let body = match app.skills_tab {
        SkillsTab::Skills => Line::from(vec![
            Span::styled(
                format!("{} skills discovered  ", app.skills.len()),
                Style::default()
                    .fg(t.text_primary)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("· {}  ", hive_status),
                Style::default().fg(t.text_muted),
            ),
            Span::styled(
                format!("· {}", relay_status),
                Style::default().fg(t.text_muted),
            ),
        ]),
        SkillsTab::Hive => {
            let identity = app
                .hive_identity
                .as_deref()
                .unwrap_or("(unknown — relay not built)");
            Line::from(vec![
                Span::styled("Identity: ", Style::default().fg(t.text_muted)),
                Span::styled(
                    identity,
                    Style::default()
                        .fg(t.text_primary)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled("    · ", Style::default().fg(t.text_muted)),
                Span::styled(relay_status, Style::default().fg(t.text_muted)),
            ])
        }
    };

    frame.render_widget(Paragraph::new(vec![body, Line::from("")]), area);
}

fn render_skills_body(frame: &mut Frame, area: Rect, app: &App) {
    let t = &app.theme;
    if app.skills.is_empty() {
        let para = Paragraph::new(vec![
            Line::from(""),
            Line::from(Span::styled(
                "  No skills found in ~/.claude/skills, ~/.claude/plugins/*/skills, or ./.claude/skills.",
                Style::default().fg(t.text_muted),
            )),
        ])
        .wrap(Wrap { trim: false });
        frame.render_widget(para, area);
        return;
    }

    // Split body into the list (top) and a 2-line selected-skill detail (bottom).
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(3), Constraint::Length(2)])
        .split(area);

    let items: Vec<ListItem> = app
        .skills
        .iter()
        .map(|s| ListItem::new(skill_line(s, app, t)))
        .collect();

    let mut state = ListState::default();
    state.select(Some(
        app.skills_selected.min(app.skills.len().saturating_sub(1)),
    ));

    let list = List::new(items)
        .highlight_style(Style::default().fg(t.header).add_modifier(Modifier::BOLD))
        .highlight_symbol("▶ ");
    frame.render_stateful_widget(list, chunks[0], &mut state);

    let detail = if let Some(s) = app.skills.get(app.skills_selected) {
        let shared = claudectl_core::skills::is_shared(s, &app.shared_skill_keys);
        let status = if shared {
            "✓ already shared with hive"
        } else if !s.within_share_limit() {
            "⚠ too large to share (>32kb)"
        } else if !cfg!(feature = "hive") {
            "hive feature disabled in this build"
        } else {
            "press s to share with hive"
        };
        vec![
            Line::from(vec![
                Span::styled("Path:   ", Style::default().fg(t.text_muted)),
                Span::styled(
                    s.path.display().to_string(),
                    Style::default().fg(t.text_primary),
                ),
            ]),
            Line::from(vec![
                Span::styled("Status: ", Style::default().fg(t.text_muted)),
                Span::styled(status, Style::default().fg(t.text_primary)),
            ]),
        ]
    } else {
        vec![Line::from(Span::styled(
            "Select a skill with j/k.",
            Style::default().fg(t.text_muted),
        ))]
    };
    frame.render_widget(Paragraph::new(detail), chunks[1]);
}

fn render_hive_body(frame: &mut Frame, area: Rect, app: &App) {
    let t = &app.theme;
    let mut lines: Vec<Line> = Vec::new();

    // Peers
    lines.push(Line::from(Span::styled(
        format!("Known peers ({})", app.hive_known_peers.len()),
        Style::default().fg(t.header).add_modifier(Modifier::BOLD),
    )));
    if app.hive_known_peers.is_empty() {
        lines.push(Line::from(Span::styled(
            "  None yet. Press i for an invite, or J to join one.",
            Style::default().fg(t.text_muted),
        )));
    } else {
        for (id, addr) in &app.hive_known_peers {
            let addr_str = addr.clone().unwrap_or_else(|| "(no last addr)".into());
            lines.push(Line::from(vec![
                Span::styled("  • ", Style::default().fg(t.text_muted)),
                Span::styled(
                    format!("{:<24}", id),
                    Style::default()
                        .fg(t.text_primary)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(addr_str, Style::default().fg(t.text_muted)),
            ]));
        }
    }

    // Last invite
    if let Some(inv) = &app.hive_last_invite {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "Last invite (share with peer)",
            Style::default().fg(t.header).add_modifier(Modifier::BOLD),
        )));
        lines.push(kv_line(t, "  code:  ", &inv.relay_code));
        if !inv.word_phrase.is_empty() {
            lines.push(kv_line(t, "  words: ", &inv.word_phrase));
        }
        if !inv.invite_link.is_empty() {
            lines.push(kv_line(t, "  link:  ", &inv.invite_link));
        }
    }

    // Join input field
    if app.hive_join_input_mode {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "Join code (Enter to confirm, Esc to cancel):",
            Style::default().fg(t.header).add_modifier(Modifier::BOLD),
        )));
        lines.push(Line::from(vec![
            Span::styled("  ▶ ", Style::default().fg(t.highlight_key)),
            Span::styled(
                app.hive_join_buffer.clone(),
                Style::default().fg(t.text_primary),
            ),
            Span::styled("█", Style::default().fg(t.highlight_key)),
        ]));
    }

    let para = Paragraph::new(lines).wrap(Wrap { trim: false });
    frame.render_widget(para, area);
}

fn kv_line<'a>(t: &'a Theme, key: &'a str, value: &'a str) -> Line<'a> {
    Line::from(vec![
        Span::styled(key, Style::default().fg(t.text_muted)),
        Span::styled(value, Style::default().fg(t.text_primary)),
    ])
}

fn skill_line<'a>(skill: &'a DiscoveredSkill, app: &'a App, t: &'a Theme) -> Line<'a> {
    let shared = claudectl_core::skills::is_shared(skill, &app.shared_skill_keys);
    let marker = if shared { "✓" } else { "·" };
    let marker_color = if shared { t.success } else { t.text_muted };

    let source_text = if let Some(p) = &skill.plugin {
        format!("{}:{}", skill.source.label(), p)
    } else {
        skill.source.label().to_string()
    };

    let size_kb = (skill.size_bytes as f64) / 1024.0;
    let too_big = !skill.within_share_limit();
    let size_color = if too_big {
        t.context_warning
    } else {
        t.text_muted
    };

    let desc = if skill.description.is_empty() {
        "(no description)".to_string()
    } else if skill.description.len() > 60 {
        format!("{}…", &skill.description[..59])
    } else {
        skill.description.clone()
    };

    Line::from(vec![
        Span::styled(format!(" {} ", marker), Style::default().fg(marker_color)),
        Span::styled(
            format!("{:<28}", truncate(&skill.name, 28)),
            Style::default()
                .fg(t.text_primary)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("{:<18}", truncate(&source_text, 18)),
            Style::default().fg(t.text_muted),
        ),
        Span::styled(
            format!("{:>6.1}kb  ", size_kb),
            Style::default().fg(size_color),
        ),
        Span::styled(desc, Style::default().fg(t.text_muted)),
    ])
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let cut: String = s.chars().take(max.saturating_sub(1)).collect();
        format!("{cut}…")
    }
}

/// Thin hint strip pinned to the bottom of the screen. Line 1 is the
/// hotkey legend for the active tab; line 2 (if present) is a transient
/// status message. The body section uses `Min` so this stays at the bottom.
fn render_footer(frame: &mut Frame, area: Rect, app: &App) {
    let t = &app.theme;
    let mut lines = Vec::new();

    let hint = match app.skills_tab {
        SkillsTab::Skills => Line::from(vec![
            Span::styled(" j/k", Style::default().fg(t.highlight_key)),
            Span::raw(":nav  "),
            Span::styled("s", Style::default().fg(t.highlight_key)),
            Span::raw(":share  "),
            Span::styled("r", Style::default().fg(t.highlight_key)),
            Span::raw(":rescan  "),
            Span::styled("Tab", Style::default().fg(t.highlight_key)),
            Span::raw(":Hive  "),
            Span::styled("Esc/K", Style::default().fg(t.highlight_key)),
            Span::raw(":close"),
        ]),
        SkillsTab::Hive => Line::from(vec![
            Span::styled(" h", Style::default().fg(t.highlight_key)),
            Span::raw(":start  "),
            Span::styled("i", Style::default().fg(t.highlight_key)),
            Span::raw(":invite  "),
            Span::styled("J", Style::default().fg(t.highlight_key)),
            Span::raw(":join  "),
            Span::styled("r", Style::default().fg(t.highlight_key)),
            Span::raw(":refresh  "),
            Span::styled("Tab", Style::default().fg(t.highlight_key)),
            Span::raw(":Skills  "),
            Span::styled("Esc/K", Style::default().fg(t.highlight_key)),
            Span::raw(":close"),
        ]),
    };
    lines.push(hint);

    if let Some(msg) = &app.skills_status_msg {
        if !msg.is_empty() {
            lines.push(Line::from(Span::styled(
                format!(" {msg}"),
                Style::default().fg(t.success).add_modifier(Modifier::BOLD),
            )));
        }
    }

    frame.render_widget(Paragraph::new(lines), area);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn truncates_long_strings() {
        assert_eq!(truncate("short", 10), "short");
        assert_eq!(truncate("abcdefghijklm", 6), "abcde…");
    }

    #[test]
    fn skill_line_renders_shared_marker() {
        let app = App::new();
        let skill = DiscoveredSkill {
            name: "X".into(),
            description: "d".into(),
            path: PathBuf::from("/tmp/x.md"),
            source: claudectl_core::skills::SkillSource::User,
            plugin: None,
            size_bytes: 100,
        };
        let line = skill_line(&skill, &app, &app.theme);
        assert!(!line.spans.is_empty());
    }
}
