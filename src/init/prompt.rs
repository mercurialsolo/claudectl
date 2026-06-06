//! Minimal interactive-prompt helpers for the `claudectl init` wizard.
//!
//! Intentionally thin: stdin/stdout only, no TUI library. Every prompt has a
//! default so a user can mash enter through the wizard and land at sensible
//! values. Non-interactive callers bypass these entirely.

use std::io::{self, BufRead, Write};

/// Yes/no question. `default = true` means hitting enter answers yes.
pub fn yes_no(prompt: &str, default: bool) -> io::Result<bool> {
    let hint = if default { "[Y/n]" } else { "[y/N]" };
    print!("{prompt} {hint} ");
    io::stdout().flush()?;
    let line = read_line()?;
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return Ok(default);
    }
    Ok(matches!(trimmed.chars().next(), Some('y' | 'Y')))
}

/// Free-form line with an optional default. Returns `Some(default)` on empty
/// input when a default is provided, `None` when the user explicitly clears.
pub fn line_or_default(prompt: &str, default: Option<&str>) -> io::Result<Option<String>> {
    match default {
        Some(d) => print!("{prompt} [{d}]: "),
        None => print!("{prompt}: "),
    }
    io::stdout().flush()?;
    let line = read_line()?;
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return Ok(default.map(String::from));
    }
    Ok(Some(trimmed.to_string()))
}

/// Number with a default. Loops until parseable.
pub fn number_or_default(prompt: &str, default: f64) -> io::Result<f64> {
    loop {
        print!("{prompt} [{default}]: ");
        io::stdout().flush()?;
        let line = read_line()?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            return Ok(default);
        }
        match trimmed.parse::<f64>() {
            Ok(n) => return Ok(n),
            Err(_) => println!("  (not a number — try again or press enter for {default})"),
        }
    }
}

fn read_line() -> io::Result<String> {
    let mut buf = String::new();
    io::stdin().lock().read_line(&mut buf)?;
    Ok(buf)
}

/// Print a section header. Used between phases so the wizard reads as a
/// numbered checklist rather than one wall of prompts.
pub fn section_header(idx: usize, total: usize, title: &str) {
    println!();
    println!("─── ({idx}/{total}) {title} ─────────────────────────────");
}

/// Print a small status block for a single phase's outcome.
pub fn phase_outcome(label: &str, summary: &str) {
    println!("  ✓ {label}: {summary}");
}

/// Print a skipped phase.
pub fn phase_skipped(label: &str, reason: &str) {
    println!("  — {label}: skipped ({reason})");
}
