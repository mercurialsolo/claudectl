use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;

/// A completed session record persisted to CSV.
#[derive(Debug, Clone)]
pub struct SessionRecord {
    pub timestamp: String, // ISO 8601
    pub pid: u32,
    pub project: String,
    pub model: String,
    pub duration_secs: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cost_usd: f64,
}

fn history_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join(".local")
        .join("share")
        .join("claudectl")
}

fn history_path() -> PathBuf {
    history_dir().join("history.csv")
}

/// Append a session record to the history CSV.
pub fn record_session(session: &crate::session::ClaudeSession) {
    let dir = history_dir();
    if fs::create_dir_all(&dir).is_err() {
        return;
    }

    let path = history_path();
    let needs_header = !path.exists();

    let file = OpenOptions::new().create(true).append(true).open(&path);

    let Ok(mut file) = file else { return };

    if needs_header {
        let _ = writeln!(
            file,
            "timestamp,pid,project,model,duration_secs,input_tokens,output_tokens,cost_usd"
        );
    }

    let ts = crate::logger::timestamp_now();
    let project = session.display_name().replace(',', ";");
    let model = session.model.replace(',', ";");

    let _ = writeln!(
        file,
        "{},{},{},{},{},{},{},{:.4}",
        ts,
        session.pid,
        project,
        model,
        session.elapsed.as_secs(),
        session.total_input_tokens,
        session.total_output_tokens,
        session.cost_usd,
    );
}

/// Load all history records, optionally filtered by a time window.
pub fn load_history(since_secs: Option<u64>) -> Vec<SessionRecord> {
    let path = history_path();
    let file = match File::open(&path) {
        Ok(f) => f,
        Err(_) => return Vec::new(),
    };

    let now_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    let reader = BufReader::new(file);
    let mut records = Vec::new();

    for (i, line) in reader.lines().enumerate() {
        let Ok(line) = line else { continue };
        if i == 0 && line.starts_with("timestamp") {
            continue; // skip header
        }

        let fields: Vec<&str> = line.splitn(8, ',').collect();
        if fields.len() < 8 {
            continue;
        }

        let record = SessionRecord {
            timestamp: fields[0].to_string(),
            pid: fields[1].parse().unwrap_or(0),
            project: fields[2].to_string(),
            model: fields[3].to_string(),
            duration_secs: fields[4].parse().unwrap_or(0),
            input_tokens: fields[5].parse().unwrap_or(0),
            output_tokens: fields[6].parse().unwrap_or(0),
            cost_usd: fields[7].parse().unwrap_or(0.0),
        };

        // Filter by time window if specified
        if let Some(window) = since_secs {
            if let Some(record_secs) = parse_timestamp_epoch(&record.timestamp) {
                if now_secs.saturating_sub(record_secs) > window {
                    continue;
                }
            }
        }

        records.push(record);
    }

    records
}

/// Parse an ISO 8601 timestamp to epoch seconds (simplified).
fn parse_timestamp_epoch(ts: &str) -> Option<u64> {
    // Format: 2026-04-11T14:30:00Z
    if ts.len() < 19 {
        return None;
    }
    let year: u64 = ts[0..4].parse().ok()?;
    let month: u64 = ts[5..7].parse().ok()?;
    let day: u64 = ts[8..10].parse().ok()?;
    let hour: u64 = ts[11..13].parse().ok()?;
    let min: u64 = ts[14..16].parse().ok()?;
    let sec: u64 = ts[17..19].parse().ok()?;

    // Approximate days from epoch (good enough for filtering)
    let mut days: u64 = 0;
    for y in 1970..year {
        days += if is_leap(y) { 366 } else { 365 };
    }
    let month_days = [0, 31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    for m in 1..month {
        days += month_days[m as usize];
        if m == 2 && is_leap(year) {
            days += 1;
        }
    }
    days += day - 1;

    Some(days * 86400 + hour * 3600 + min * 60 + sec)
}

fn is_leap(y: u64) -> bool {
    y % 4 == 0 && (y % 100 != 0 || y % 400 == 0)
}

/// Print a tabular history view.
pub fn print_history(since: &str) {
    let since_secs = parse_duration(since);
    let records = load_history(since_secs);

    if records.is_empty() {
        println!("No session history found.");
        if since_secs.is_some() {
            println!("  (filtered to last {since})");
        }
        return;
    }

    println!(
        "{:<22} {:<7} {:<20} {:<12} {:>10} {:>12} {:>12} {:>10}",
        "Timestamp", "PID", "Project", "Model", "Duration", "Input", "Output", "Cost"
    );
    println!("{}", "-".repeat(110));

    let mut total_cost = 0.0;
    let mut total_duration = 0u64;
    let mut total_input = 0u64;
    let mut total_output = 0u64;

    for r in &records {
        let dur = format_duration(r.duration_secs);
        let cost = if r.cost_usd < 1.0 {
            format!("${:.2}", r.cost_usd)
        } else {
            format!("${:.1}", r.cost_usd)
        };

        println!(
            "{:<22} {:<7} {:<20} {:<12} {:>10} {:>12} {:>12} {:>10}",
            &r.timestamp[..19.min(r.timestamp.len())],
            r.pid,
            truncate(&r.project, 20),
            truncate(&r.model, 12),
            dur,
            format_count(r.input_tokens),
            format_count(r.output_tokens),
            cost,
        );

        total_cost += r.cost_usd;
        total_duration += r.duration_secs;
        total_input += r.input_tokens;
        total_output += r.output_tokens;
    }

    println!("{}", "-".repeat(110));
    let total_cost_str = if total_cost < 1.0 {
        format!("${:.2}", total_cost)
    } else {
        format!("${:.1}", total_cost)
    };
    println!(
        "{:<22} {:<7} {:<20} {:<12} {:>10} {:>12} {:>12} {:>10}",
        format!("{} sessions", records.len()),
        "",
        "",
        "",
        format_duration(total_duration),
        format_count(total_input),
        format_count(total_output),
        total_cost_str,
    );
}

/// Print aggregate statistics.
pub fn print_stats(since: &str) {
    let since_secs = parse_duration(since);
    let records = load_history(since_secs);

    if records.is_empty() {
        println!("No session history found.");
        return;
    }

    let total_cost: f64 = records.iter().map(|r| r.cost_usd).sum();
    let total_duration: u64 = records.iter().map(|r| r.duration_secs).sum();
    let total_input: u64 = records.iter().map(|r| r.input_tokens).sum();
    let total_output: u64 = records.iter().map(|r| r.output_tokens).sum();
    let avg_cost = total_cost / records.len() as f64;
    let avg_duration = total_duration / records.len() as u64;

    println!("Session Statistics (last {since})");
    println!("{}", "=".repeat(45));
    println!("  Sessions:         {}", records.len());
    println!("  Total cost:       ${:.2}", total_cost);
    println!("  Avg cost/session: ${:.2}", avg_cost);
    println!("  Total duration:   {}", format_duration(total_duration));
    println!("  Avg duration:     {}", format_duration(avg_duration));
    println!(
        "  Total tokens:     {} in / {} out",
        format_count(total_input),
        format_count(total_output)
    );
    println!();

    // Per-project breakdown
    let mut projects: std::collections::HashMap<String, (f64, u64, usize)> =
        std::collections::HashMap::new();
    for r in &records {
        let entry = projects.entry(r.project.clone()).or_default();
        entry.0 += r.cost_usd;
        entry.1 += r.duration_secs;
        entry.2 += 1;
    }

    let mut project_list: Vec<_> = projects.into_iter().collect();
    project_list.sort_by(|a, b| b.1.0.partial_cmp(&a.1.0).unwrap());

    println!("  Per-project breakdown:");
    println!(
        "  {:<25} {:>8} {:>10} {:>10}",
        "Project", "Sessions", "Duration", "Cost"
    );
    println!("  {}", "-".repeat(55));
    for (name, (cost, dur, count)) in &project_list {
        let cost_str = if *cost < 1.0 {
            format!("${:.2}", cost)
        } else {
            format!("${:.1}", cost)
        };
        println!(
            "  {:<25} {:>8} {:>10} {:>10}",
            truncate(name, 25),
            count,
            format_duration(*dur),
            cost_str,
        );
    }

    // Per-model breakdown
    let mut models: std::collections::HashMap<String, (f64, usize)> =
        std::collections::HashMap::new();
    for r in &records {
        let model = if r.model.is_empty() {
            "unknown".to_string()
        } else {
            r.model.clone()
        };
        let entry = models.entry(model).or_default();
        entry.0 += r.cost_usd;
        entry.1 += 1;
    }

    let mut model_list: Vec<_> = models.into_iter().collect();
    model_list.sort_by(|a, b| b.1.0.partial_cmp(&a.1.0).unwrap());

    println!();
    println!("  Per-model breakdown:");
    println!("  {:<20} {:>8} {:>10}", "Model", "Sessions", "Cost");
    println!("  {}", "-".repeat(40));
    for (name, (cost, count)) in &model_list {
        let cost_str = if *cost < 1.0 {
            format!("${:.2}", cost)
        } else {
            format!("${:.1}", cost)
        };
        println!("  {:<20} {:>8} {:>10}", name, count, cost_str);
    }
}

/// Weekly usage summary for the TUI title bar.
#[derive(Debug, Clone, Default)]
pub struct WeeklySummary {
    pub cost_usd: f64,
    pub total_tokens: u64,
    #[allow(dead_code)]
    pub session_count: usize,
    pub today_cost_usd: f64,
}

/// Compute weekly and daily cost/token summary from history.
pub fn weekly_summary() -> WeeklySummary {
    let week_secs = 7 * 86400;
    let day_secs = 86400;
    let week_records = load_history(Some(week_secs));
    let day_records = load_history(Some(day_secs));

    WeeklySummary {
        cost_usd: week_records.iter().map(|r| r.cost_usd).sum(),
        total_tokens: week_records
            .iter()
            .map(|r| r.input_tokens + r.output_tokens)
            .sum(),
        session_count: week_records.len(),
        today_cost_usd: day_records.iter().map(|r| r.cost_usd).sum(),
    }
}

/// Parse a duration string like "24h", "30m", "7d" into seconds.
pub fn parse_duration(s: &str) -> Option<u64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let (num_str, unit) = s.split_at(s.len() - 1);
    let num: u64 = num_str.parse().ok()?;
    match unit {
        "s" => Some(num),
        "m" => Some(num * 60),
        "h" => Some(num * 3600),
        "d" => Some(num * 86400),
        "w" => Some(num * 604800),
        _ => None,
    }
}

fn format_duration(secs: u64) -> String {
    let h = secs / 3600;
    let m = (secs % 3600) / 60;
    let s = secs % 60;
    if h > 0 {
        format!("{h}h{m:02}m")
    } else {
        format!("{m}m{s:02}s")
    }
}

fn format_count(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}...", &s[..max - 3])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_duration() {
        assert_eq!(parse_duration("24h"), Some(86400));
        assert_eq!(parse_duration("30m"), Some(1800));
        assert_eq!(parse_duration("7d"), Some(604800));
        assert_eq!(parse_duration("1w"), Some(604800));
        assert_eq!(parse_duration(""), None);
        assert_eq!(parse_duration("abc"), None);
    }

    #[test]
    fn test_format_duration() {
        assert_eq!(format_duration(3661), "1h01m");
        assert_eq!(format_duration(125), "2m05s");
        assert_eq!(format_duration(0), "0m00s");
    }

    #[test]
    fn test_truncate() {
        assert_eq!(truncate("hello", 10), "hello");
        assert_eq!(truncate("hello world!", 8), "hello...");
    }

    #[test]
    fn test_parse_timestamp_epoch() {
        // 2026-01-01T00:00:00Z
        let ts = parse_timestamp_epoch("2026-01-01T00:00:00Z").unwrap();
        // Should be reasonable (after 2025)
        assert!(ts > 1735689600); // 2025-01-01
        assert!(ts < 1798761600); // 2027-01-01
    }

    #[test]
    fn test_is_leap() {
        assert!(is_leap(2000));
        assert!(is_leap(2024));
        assert!(!is_leap(1900));
        assert!(!is_leap(2023));
    }
}
