use ratatui::{
    Frame,
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Wrap},
};

use crate::app::App;
use crate::session::ClaudeSession;
use crate::theme::Theme;

pub fn render_detail_panel(frame: &mut Frame, area: Rect, session: &ClaudeSession, app: &App) {
    let t = &app.theme;
    let pid = session.pid.to_string();
    let status = session.status.to_string();
    let elapsed = session.format_elapsed();
    let model = if session.model.is_empty() {
        "-".to_string()
    } else {
        session.model.clone()
    };
    let tty = if session.tty.is_empty() {
        "-".to_string()
    } else {
        session.tty.clone()
    };
    let input_tok = format_tokens(session.total_input_tokens);
    let output_tok = format_tokens(session.total_output_tokens);
    let cache_read = format_tokens(session.cache_read_tokens);
    let cache_write = format_tokens(session.cache_write_tokens);
    let context_str = if session.has_usage_metrics() {
        format!(
            "{} / {} ({}%)",
            format_tokens(session.context_tokens),
            format_tokens(session.context_max),
            session.context_percent() as u32
        )
    } else {
        "n/a".to_string()
    };
    let cost = session.format_cost();
    let burn_rate = session.format_burn_rate();
    let estimate = if session.cost_estimate_unverified {
        format!("{} (unverified)", session.model_profile_source)
    } else {
        session.model_profile_source.clone()
    };
    let command = if session.command_args.is_empty() {
        "claude".to_string()
    } else {
        session.command_args.clone()
    };
    let jsonl = session
        .jsonl_path
        .as_ref()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "-".into());
    let subagents = session.format_subagent_summary();
    let subagent_breakdown = session.subagent_breakdown();
    let telemetry = if session.has_usage_metrics() {
        format!("{} (usage metrics available)", session.telemetry_label())
    } else {
        session.telemetry_label().to_string()
    };

    let mut lines = vec![
        detail_line("PID", &pid, t),
        detail_line("Session ID", &session.session_id, t),
        detail_line("CWD", &session.cwd, t),
        detail_line("Project", &session.project_name, t),
        detail_line("Model", &model, t),
        detail_line("Status", &status, t),
        detail_line("Telemetry", &telemetry, t),
        detail_line("TTY", &tty, t),
        detail_line("Elapsed", &elapsed, t),
        Line::from(""),
        Line::from(Span::styled(
            " Tokens",
            Style::default().fg(t.header).add_modifier(Modifier::BOLD),
        )),
        detail_line("  Input", &input_tok, t),
        detail_line("  Output", &output_tok, t),
        detail_line("  Cache Read", &cache_read, t),
        detail_line("  Cache Write", &cache_write, t),
        detail_line("  Context", &context_str, t),
        Line::from(""),
        Line::from(Span::styled(
            " Cost",
            Style::default().fg(t.header).add_modifier(Modifier::BOLD),
        )),
        detail_line("  Total", &cost, t),
        detail_line("  Burn Rate", &burn_rate, t),
        detail_line("  Estimate", &estimate, t),
        Line::from(""),
        detail_line("Command", &command, t),
        detail_line("JSONL", &jsonl, t),
        detail_line("Subagents", &subagents, t),
    ];

    if !subagent_breakdown.is_empty() {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            " Subagent Breakdown",
            Style::default().fg(t.header).add_modifier(Modifier::BOLD),
        )));
        for row in subagent_breakdown.iter().take(10) {
            lines.push(detail_line(
                &format!("  {}", row.display_label()),
                &format!(
                    "{} | {} | {}",
                    row.state_label(),
                    row.format_cost(),
                    row.format_tokens()
                ),
                t,
            ));
        }
        if subagent_breakdown.len() > 10 {
            lines.push(detail_line(
                "",
                &format!("  ... and {} more", subagent_breakdown.len() - 10),
                t,
            ));
        }
    }

    // Recent errors section
    if !session.recent_errors.is_empty() {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            format!(" Recent Errors ({})", session.recent_errors.len()),
            Style::default().fg(t.error).add_modifier(Modifier::BOLD),
        )));
        for err in session.recent_errors.iter().rev().take(5) {
            lines.push(detail_line(
                &format!("  {}", err.tool_name),
                &err.message,
                t,
            ));
        }
    }

    // File conflicts section
    {
        let pid = session.pid;
        let conflicting_files: Vec<(&String, &Vec<u32>)> = app
            .file_conflicts
            .iter()
            .filter(|(_, pids)| pids.contains(&pid))
            .collect();
        if !conflicting_files.is_empty() {
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                format!(" File Conflicts ({})", conflicting_files.len()),
                Style::default().fg(t.error).add_modifier(Modifier::BOLD),
            )));
            for (file, pids) in conflicting_files.iter().take(10) {
                let others: Vec<&str> = pids
                    .iter()
                    .filter(|&&p| p != pid)
                    .filter_map(|p| {
                        app.sessions
                            .iter()
                            .find(|s| s.pid == *p)
                            .map(|s| s.display_name())
                    })
                    .collect();
                let short = file.rsplit('/').next().unwrap_or(file);
                lines.push(detail_line(
                    &format!("  {short}"),
                    &format!("also edited by {}", others.join(", ")),
                    t,
                ));
            }
        }
    }

    // Tool usage section
    if !session.tool_usage.is_empty() {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            " Tool Usage",
            Style::default().fg(t.header).add_modifier(Modifier::BOLD),
        )));
        let mut tools: Vec<_> = session.tool_usage.iter().collect();
        tools.sort_by(|a, b| b.1.calls.cmp(&a.1.calls));
        for (name, stats) in tools.iter().take(10) {
            lines.push(detail_line(
                &format!("  {name}"),
                &format!("{} calls", stats.calls),
                t,
            ));
        }
    }

    // Files modified section
    if !session.files_modified.is_empty() {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            format!(" Files Modified ({})", session.files_modified.len()),
            Style::default().fg(t.header).add_modifier(Modifier::BOLD),
        )));
        // Sort by edit count descending, show up to 10
        let mut files: Vec<_> = session.files_modified.iter().collect();
        files.sort_by(|a, b| b.1.cmp(a.1));
        for (path, count) in files.iter().take(10) {
            // Show just the filename, or last 2 path components for context
            let short = shorten_path(path);
            let suffix = if **count > 1 {
                format!("  ({count} edits)")
            } else {
                String::new()
            };
            lines.push(detail_line("", &format!("{short}{suffix}"), t));
        }
        if files.len() > 10 {
            lines.push(detail_line(
                "",
                &format!("  ... and {} more", files.len() - 10),
                t,
            ));
        }
    }

    let block = Block::default()
        .title(" Session Detail ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(t.border));

    let paragraph = Paragraph::new(lines)
        .block(block)
        .wrap(Wrap { trim: false });

    frame.render_widget(paragraph, area);
}

fn detail_line(label: &str, value: &str, t: &Theme) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!(" {label:<15}"), Style::default().fg(t.text_muted)),
        Span::styled(value.to_string(), Style::default().fg(t.text_primary)),
    ])
}

/// Shorten a file path to last 2 components (e.g., "src/main.rs").
fn shorten_path(path: &str) -> String {
    let parts: Vec<&str> = path.rsplit('/').take(2).collect();
    match parts.len() {
        2 => format!("{}/{}", parts[1], parts[0]),
        1 => parts[0].to_string(),
        _ => path.to_string(),
    }
}

fn format_tokens(n: u64) -> String {
    if n == 0 {
        return "-".to_string();
    }
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}
