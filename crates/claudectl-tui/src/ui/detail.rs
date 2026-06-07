use ratatui::{
    Frame,
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Wrap},
};

use crate::app::App;
use claudectl_core::session::ClaudeSession;
use claudectl_core::theme::Theme;

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

    // Bus role binding (#307). Look up by pid first — TUI-bound roles attach
    // by pid so this catches the bind without an extra cwd-prefix lookup. We
    // fall back to cwd_selector matching for cwd-only bindings made via
    // `claudectl bus role bind <name> <cwd>` outside the TUI.
    let bus_role_line = {
        let roles = app.runtime.bus.list_roles();
        let by_pid = roles.iter().find(|r| r.pid == Some(session.pid));
        let by_cwd = roles
            .iter()
            .find(|r| !r.cwd_selector.is_empty() && session.cwd.starts_with(&r.cwd_selector));
        match by_pid.or(by_cwd) {
            Some(r) => detail_line(
                "Bus role",
                &format!(
                    "{} (bound by {})",
                    r.name,
                    if r.pid == Some(session.pid) {
                        "pid"
                    } else {
                        "cwd"
                    }
                ),
                t,
            ),
            None => detail_line("Bus role", "— (Ctrl+R to bind)", t),
        }
    };

    let mut lines = vec![
        detail_line("PID", &pid, t),
        detail_line("Session ID", &session.session_id, t),
        detail_line("CWD", &session.cwd, t),
        detail_line("Project", &session.project_name, t),
        detail_line("Model", &model, t),
        detail_line("Status", &status, t),
        bus_role_line,
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
    ];

    // Cognitive Health section
    if session.has_usage_metrics() && session.decay_score > 0 {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            " Cognitive Health",
            Style::default().fg(t.header).add_modifier(Modifier::BOLD),
        )));

        let decay_label = match session.decay_score {
            0..=29 => "healthy",
            30..=59 => "early decay",
            60..=79 => "significant decay",
            _ => "severe decay",
        };
        lines.push(detail_line(
            "  Decay Score",
            &format!("{}/100 {}", session.decay_score, decay_label),
            t,
        ));
        lines.push(detail_line(
            "  Context",
            &format!("{}%", session.context_percent() as u32),
            t,
        ));

        if let Some(baseline) = session.baseline_tokens_per_edit {
            if session.edit_event_count > 5 && baseline > 0.0 {
                let current =
                    session.total_tokens_at_edit_count as f64 / session.edit_event_count as f64;
                let pct_change = ((current / baseline) - 1.0) * 100.0;
                let arrow = if pct_change > 5.0 { "↓" } else { "→" };
                lines.push(detail_line(
                    "  Efficiency",
                    &format!("{}{:.0}% vs baseline", arrow, pct_change.abs()),
                    t,
                ));
            }
        }

        if session.error_counts_per_window.len() >= 2 {
            let recent = session.error_counts_per_window.last().unwrap_or(&0);
            let first = session.error_counts_per_window.first().unwrap_or(&0);
            if recent > first {
                lines.push(detail_line("  Error trend", "↑ accelerating", t));
            }
        }

        let max_rereads = session
            .file_reads_since_edit
            .values()
            .copied()
            .max()
            .unwrap_or(0);
        if max_rereads >= 2 {
            lines.push(detail_line(
                "  Repetition",
                &format!("{} file re-reads detected", max_rereads),
                t,
            ));
        }

        if session.decay_score >= 30 {
            let suggestion = match session.decay_score {
                30..=49 => "Consider /compact with preservation notes",
                50..=69 => "Compact recommended — preserve architectural decisions",
                70..=84 => "Restart recommended — generate state transfer first",
                _ => "Session compromised — restart with fresh context",
            };
            lines.push(detail_line("  Suggestion", suggestion, t));
        }
    }

    lines.extend([
        Line::from(""),
        detail_line("Command", &command, t),
        detail_line("JSONL", &jsonl, t),
        detail_line("Subagents", &subagents, t),
    ]);

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

    // Coordination section (leases and handoffs for this session)
    #[cfg(feature = "coord")]
    {
        let session_leases: Vec<&claudectl_core::runtime::LeaseSummary> = app
            .coord_leases
            .iter()
            .filter(|l| l.owner_session_id == session.session_id)
            .collect();

        let session_handoffs: Vec<&claudectl_core::runtime::HandoffSummary> = app
            .coord_handoffs
            .iter()
            .filter(|h| {
                h.from_session_id == session.session_id
                    || h.to_session_id.as_deref() == Some(&*session.session_id)
            })
            .collect();

        if !session_leases.is_empty() || !session_handoffs.is_empty() {
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                " Coordination",
                Style::default().fg(t.header).add_modifier(Modifier::BOLD),
            )));

            if !session_leases.is_empty() {
                lines.push(detail_line(
                    "  Leases",
                    &format!("{} active", session_leases.len()),
                    t,
                ));
                for lease in session_leases.iter().take(5) {
                    let resource = format!("{}:{}", lease.resource_kind, lease.resource_value);
                    let expires = lease.expires_at.as_deref().unwrap_or("no expiry");
                    lines.push(detail_line(
                        &format!("    {}", lease.mode),
                        &format!("{resource} ({expires})"),
                        t,
                    ));
                }
            }

            if !session_handoffs.is_empty() {
                lines.push(detail_line(
                    "  Handoffs",
                    &format!("{} pending", session_handoffs.len()),
                    t,
                ));
                for handoff in session_handoffs.iter().take(5) {
                    let direction = if handoff.from_session_id == session.session_id {
                        "to"
                    } else {
                        "from"
                    };
                    let other = if direction == "to" {
                        handoff.to_session_id.as_deref().unwrap_or("unassigned")
                    } else {
                        &handoff.from_session_id
                    };
                    lines.push(detail_line(
                        &format!("    {direction} {other}"),
                        &handoff.summary,
                        t,
                    ));
                }
            }

            // Pending interrupts targeting this session
            let session_interrupts: Vec<&claudectl_core::runtime::InterruptSummary> = app
                .coord_pending_interrupts
                .iter()
                .filter(|i| i.target_session_id == session.session_id)
                .collect();

            if !session_interrupts.is_empty() {
                lines.push(detail_line(
                    "  Interrupts",
                    &format!("{} pending", session_interrupts.len()),
                    t,
                ));
                for intr in session_interrupts.iter().take(5) {
                    lines.push(detail_line(
                        &format!("    {} [{}]", intr.interrupt_type, intr.priority),
                        &intr.reason,
                        t,
                    ));
                }
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
        tools.sort_by_key(|t| std::cmp::Reverse(t.1.calls));
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
