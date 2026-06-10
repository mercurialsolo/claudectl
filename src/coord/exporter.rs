// Allow dead_code: `serve` is the public entry point the headless daemon
// calls in PR8 once the `--exporter` flag lands; the rendering helpers
// are exercised by the test suite.
#![allow(dead_code)]
//! Prometheus / OpenMetrics exporter (#349, RFC §10).
//!
//! Hand-rolled HTTP listener so we don't pull a web framework just to
//! serve `/metrics`. Binds a TCP socket, reads the request line, and
//! responds with the standard Prometheus text format. Spawns a worker
//! thread per connection so the headless tick loop never blocks on a
//! slow scraper.
//!
//! Why an exporter is the bridge from solo tool to team infra:
//!
//! - `claudectl_tasks_by_state{state="RUNNING"} 4`
//! - `claudectl_fleet_cost_usd_total 12.45`
//! - `claudectl_retries_total{cause="verify_fail"} 7`
//! - `claudectl_verifier_pass_rate{kind="brain"} 0.83`
//!
//! Dashboards (Grafana, Datadog) read these without any
//! claudectl-specific glue.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use rusqlite::Connection;

use super::store;

/// Listen for HTTP scrapes on `bind` and serve `/metrics` snapshots
/// computed from the coord DB. Returns the shutdown handle the
/// headless daemon can use to drain on SIGTERM.
///
/// `bind` should look like `0.0.0.0:9464` or `127.0.0.1:9464`. The
/// listener is non-blocking with a short poll interval so the worker
/// can react to the shutdown flag promptly.
pub fn serve(bind: &str) -> Result<ExporterHandle, String> {
    let listener = TcpListener::bind(bind).map_err(|e| format!("bind {bind}: {e}"))?;
    listener
        .set_nonblocking(true)
        .map_err(|e| format!("non-blocking: {e}"))?;
    let shutdown = Arc::new(AtomicBool::new(false));
    let shutdown_handle = shutdown.clone();
    let handle = thread::Builder::new()
        .name("supervisor-exporter".into())
        .spawn(move || serve_loop(listener, shutdown))
        .map_err(|e| format!("spawn exporter thread: {e}"))?;
    Ok(ExporterHandle {
        shutdown: shutdown_handle,
        thread: Some(handle),
    })
}

/// Caller-side shutdown. Drop or explicitly `.stop()` to bring the
/// listener down.
pub struct ExporterHandle {
    shutdown: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

impl ExporterHandle {
    pub fn stop(&mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for ExporterHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

fn serve_loop(listener: TcpListener, shutdown: Arc<AtomicBool>) {
    loop {
        if shutdown.load(Ordering::SeqCst) {
            return;
        }
        match listener.accept() {
            Ok((stream, _addr)) => {
                let s = shutdown.clone();
                thread::spawn(move || handle_request(stream, s));
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(100));
            }
            Err(_) => {
                thread::sleep(Duration::from_millis(100));
            }
        }
    }
}

fn handle_request(mut stream: TcpStream, _shutdown: Arc<AtomicBool>) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(3)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(3)));
    let mut buf = [0u8; 1024];
    let n = match stream.read(&mut buf) {
        Ok(n) => n,
        Err(_) => return,
    };
    let request = String::from_utf8_lossy(&buf[..n]);
    let first_line = request.lines().next().unwrap_or("");
    if !first_line.starts_with("GET /metrics") {
        // Anything other than /metrics gets a 404 — keeps the surface
        // honest about what we expose.
        let _ = stream
            .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
        return;
    }
    let body = match render_metrics() {
        Ok(s) => s,
        Err(e) => {
            let err = format!("# error: {e}\n");
            let response = format!(
                "HTTP/1.1 500 Internal Server Error\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                err.len(),
                err
            );
            let _ = stream.write_all(response.as_bytes());
            return;
        }
    };
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/plain; version=0.0.4\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    let _ = stream.write_all(response.as_bytes());
}

/// Render the current metric snapshot. Opens a fresh coord connection
/// per scrape — cheap with WAL, and sidesteps any concurrent-borrow
/// lifetime juggling with the reconciler's connection.
fn render_metrics() -> Result<String, String> {
    let conn = store::open()?;
    let snapshot = snapshot(&conn)?;
    Ok(format_prometheus(&snapshot))
}

#[derive(Debug, Default, Clone)]
pub struct MetricSnapshot {
    pub tasks_by_state: Vec<(String, u64)>,
    pub fleet_cost_usd_total: f64,
    pub retries_by_cause: Vec<(String, u64)>,
    pub verifier_pass_rate: Vec<(String, f64)>,
}

pub fn snapshot(conn: &Connection) -> Result<MetricSnapshot, String> {
    let mut out = MetricSnapshot::default();

    // tasks_by_state
    let mut stmt = conn
        .prepare("SELECT state, COUNT(*) FROM tasks GROUP BY state")
        .map_err(|e| format!("prepare tasks: {e}"))?;
    let rows = stmt
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)? as u64))
        })
        .map_err(|e| format!("tasks query: {e}"))?;
    for r in rows {
        out.tasks_by_state.push(r.map_err(|e| format!("row: {e}"))?);
    }

    // fleet_cost_usd_total — sum of attempt costs + verifier costs.
    let attempt_cost: f64 = conn
        .query_row(
            "SELECT COALESCE(SUM(cost_usd), 0) FROM task_attempts",
            [],
            |row| row.get(0),
        )
        .unwrap_or(0.0);
    let verify_cost: f64 = conn
        .query_row(
            "SELECT COALESCE(SUM(cost_usd), 0) FROM task_verifications",
            [],
            |row| row.get(0),
        )
        .unwrap_or(0.0);
    out.fleet_cost_usd_total = attempt_cost + verify_cost;

    // retries_by_cause — count transitions into Retrying / Resuming.
    let mut stmt = conn
        .prepare(
            "SELECT cause, COUNT(*) FROM task_transitions
             WHERE to_state IN ('RETRYING','RESUMING')
             GROUP BY cause",
        )
        .map_err(|e| format!("prepare retries: {e}"))?;
    let rows = stmt
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)? as u64))
        })
        .map_err(|e| format!("retries query: {e}"))?;
    for r in rows {
        out.retries_by_cause
            .push(r.map_err(|e| format!("row: {e}"))?);
    }

    // verifier_pass_rate per kind. `pass / (pass + fail)`; emitted as a
    // gauge in [0.0, 1.0].
    let mut stmt = conn
        .prepare(
            "SELECT kind,
                    SUM(CASE WHEN verdict = 'PASS' THEN 1 ELSE 0 END) AS passes,
                    COUNT(*) AS total
             FROM task_verifications
             GROUP BY kind",
        )
        .map_err(|e| format!("prepare pass-rate: {e}"))?;
    let rows = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)? as f64,
                row.get::<_, i64>(2)? as f64,
            ))
        })
        .map_err(|e| format!("pass-rate query: {e}"))?;
    for r in rows {
        let (kind, passes, total) = r.map_err(|e| format!("row: {e}"))?;
        let rate = if total > 0.0 { passes / total } else { 0.0 };
        out.verifier_pass_rate.push((kind, rate));
    }

    Ok(out)
}

pub fn format_prometheus(snap: &MetricSnapshot) -> String {
    let mut out = String::new();

    out.push_str("# HELP claudectl_tasks_by_state Tasks bucketed by current state.\n");
    out.push_str("# TYPE claudectl_tasks_by_state gauge\n");
    for (state, n) in &snap.tasks_by_state {
        out.push_str(&format!(
            "claudectl_tasks_by_state{{state=\"{state}\"}} {n}\n"
        ));
    }

    out.push_str("# HELP claudectl_fleet_cost_usd_total Cumulative USD spend across attempts and verifiers.\n");
    out.push_str("# TYPE claudectl_fleet_cost_usd_total counter\n");
    out.push_str(&format!(
        "claudectl_fleet_cost_usd_total {}\n",
        snap.fleet_cost_usd_total
    ));

    out.push_str(
        "# HELP claudectl_retries_total Transitions into RETRYING / RESUMING bucketed by cause.\n",
    );
    out.push_str("# TYPE claudectl_retries_total counter\n");
    for (cause, n) in &snap.retries_by_cause {
        out.push_str(&format!(
            "claudectl_retries_total{{cause=\"{}\"}} {n}\n",
            escape_label(cause)
        ));
    }

    out.push_str(
        "# HELP claudectl_verifier_pass_rate Verifier verdicts: passes / total per verifier kind.\n",
    );
    out.push_str("# TYPE claudectl_verifier_pass_rate gauge\n");
    for (kind, rate) in &snap.verifier_pass_rate {
        out.push_str(&format!(
            "claudectl_verifier_pass_rate{{kind=\"{kind}\"}} {rate}\n"
        ));
    }

    out
}

/// Prometheus label escaping: backslash, double-quote, newline.
fn escape_label(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::coord::tasks::{NewTask, insert_task};

    fn sample(name: &str) -> NewTask {
        NewTask {
            name: name.into(),
            role: None,
            cwd: "/x".into(),
            prompt: "do".into(),
            model: None,
            budget_usd: None,
            max_retries: None,
            timeout_min: None,
            depends_on: vec![],
            policy: None,
            verifiers: vec![],
        }
    }

    #[test]
    fn snapshot_buckets_tasks_by_state() {
        let conn = store::open_memory();
        insert_task(&conn, &sample("a")).unwrap();
        insert_task(&conn, &sample("b")).unwrap();
        let snap = snapshot(&conn).unwrap();
        let pending: u64 = snap
            .tasks_by_state
            .iter()
            .find(|(s, _)| s == "PENDING")
            .map(|(_, n)| *n)
            .unwrap();
        assert_eq!(pending, 2);
    }

    #[test]
    fn prometheus_format_includes_help_and_type_lines() {
        let snap = MetricSnapshot {
            tasks_by_state: vec![("RUNNING".into(), 3), ("DONE".into(), 7)],
            fleet_cost_usd_total: 12.45,
            retries_by_cause: vec![("verify_fail".into(), 2)],
            verifier_pass_rate: vec![("run".into(), 0.83)],
        };
        let text = format_prometheus(&snap);
        assert!(text.contains("# HELP claudectl_tasks_by_state"));
        assert!(text.contains("# TYPE claudectl_tasks_by_state gauge"));
        assert!(text.contains(r#"claudectl_tasks_by_state{state="RUNNING"} 3"#));
        assert!(text.contains("claudectl_fleet_cost_usd_total 12.45"));
        assert!(text.contains(r#"claudectl_retries_total{cause="verify_fail"} 2"#));
        assert!(text.contains(r#"claudectl_verifier_pass_rate{kind="run"} 0.83"#));
    }

    #[test]
    fn label_escaping_handles_quotes_and_newlines() {
        assert_eq!(escape_label(r#"a"b"#), r#"a\"b"#);
        assert_eq!(escape_label("a\nb"), "a\\nb");
        assert_eq!(escape_label(r"a\b"), r"a\\b");
    }

    #[test]
    fn pass_rate_handles_division_by_zero_safely() {
        let conn = store::open_memory();
        // No verifier rows ⇒ rate map is empty (avoids 0/0).
        let snap = snapshot(&conn).unwrap();
        assert!(snap.verifier_pass_rate.is_empty());
    }
}
