use std::fs::{File, OpenOptions};
use std::io::Write;
use std::sync::Mutex;

static LOGGER: Mutex<Option<File>> = Mutex::new(None);

/// Initialize the logger with a file path. Call once at startup.
pub fn init(path: &str) -> std::io::Result<()> {
    let file = OpenOptions::new().create(true).append(true).open(path)?;
    *LOGGER.lock().unwrap() = Some(file);
    log("INFO", "Logger initialized");
    Ok(())
}

/// Write a log line with timestamp and level.
pub fn log(level: &str, message: &str) {
    let Ok(mut guard) = LOGGER.lock() else {
        return;
    };
    let Some(ref mut file) = *guard else {
        return;
    };
    let now = chrono_now();
    let _ = writeln!(file, "{now} [{level}] {message}");
}

/// Format current time as ISO 8601 without external crate.
fn chrono_now() -> String {
    let d = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let total_secs = d.as_secs();
    let secs_in_day = total_secs % 86400;
    let h = secs_in_day / 3600;
    let m = (secs_in_day % 3600) / 60;
    let s = secs_in_day % 60;

    // Approximate date (good enough for logs, not worth a crate)
    let days = total_secs / 86400;
    let (year, month, day) = days_to_date(days);

    format!("{year:04}-{month:02}-{day:02}T{h:02}:{m:02}:{s:02}Z")
}

/// Convert days since epoch to (year, month, day). Simplified civil calendar.
fn days_to_date(days: u64) -> (u64, u64, u64) {
    // Algorithm from http://howardhinnant.github.io/date_algorithms.html
    let z = days + 719468;
    let era = z / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_days_to_date_epoch() {
        assert_eq!(days_to_date(0), (1970, 1, 1));
    }

    #[test]
    fn test_days_to_date_known() {
        // 2026-04-11 = 20554 days since epoch
        assert_eq!(days_to_date(20554), (2026, 4, 11));
    }

    #[test]
    fn test_log_without_init() {
        // Should not panic even without init
        log("DEBUG", "test message");
    }

    #[test]
    fn test_init_and_log() {
        let file = tempfile::NamedTempFile::new().unwrap();
        let path = file.path().to_str().unwrap();
        init(path).unwrap();
        log("INFO", "hello world");
        log("WARN", "something happened");

        // Read back
        let content = std::fs::read_to_string(path).unwrap();
        assert!(content.contains("[INFO] Logger initialized"));
        assert!(content.contains("[INFO] hello world"));
        assert!(content.contains("[WARN] something happened"));

        // Clean up global state
        *LOGGER.lock().unwrap() = None;
    }
}
