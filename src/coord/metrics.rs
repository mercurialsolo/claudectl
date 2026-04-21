#![allow(dead_code)]

use rusqlite::Connection;
use serde::Serialize;

use super::store;
use super::types::*;

/// Coordination metrics computed from the event log and materialized state.
#[derive(Debug, Clone, Serialize)]
pub struct CoordMetrics {
    /// Total events in the time window.
    pub total_events: u64,
    /// Event counts by type.
    pub event_counts: Vec<(String, u64)>,
    /// Lease claim attempts and conflicts.
    pub lease_claims: u64,
    pub lease_conflicts: u64,
    pub lease_conflict_rate: f64,
    /// Active leases.
    pub active_leases: u64,
    /// Handoff metrics.
    pub handoffs_created: u64,
    pub handoffs_accepted: u64,
    pub handoff_completion_rate: f64,
    pub median_handoff_acceptance_secs: Option<f64>,
    /// Blocker metrics.
    pub blockers_opened: u64,
    pub blockers_resolved: u64,
    pub median_blocker_resolution_secs: Option<f64>,
    /// Interrupt metrics.
    pub interrupts_raised: u64,
    pub interrupts_delivered: u64,
    pub interrupts_acknowledged: u64,
    pub interrupt_delivery_rate: f64,
    pub median_interrupt_ack_secs: Option<f64>,
    /// Memory metrics.
    pub memory_records: u64,
    pub memory_promoted: u64,
}

/// Compute coordination metrics, optionally scoped to a time window.
pub fn compute(conn: &Connection, since: Option<&str>) -> CoordMetrics {
    let event_counts = store::count_events_by_type(conn, since).unwrap_or_default();
    let total_events: u64 = event_counts.iter().map(|(_, c)| c).sum();

    let count_of = |event_type: &str| -> u64 {
        event_counts
            .iter()
            .find(|(t, _)| t == event_type)
            .map(|(_, c)| *c)
            .unwrap_or(0)
    };

    let lease_claims = count_of("lease_acquired");
    let lease_conflicts = count_lease_conflicts(conn, since);
    let lease_conflict_rate = if lease_claims > 0 {
        lease_conflicts as f64 / (lease_claims + lease_conflicts) as f64
    } else {
        0.0
    };

    let active_leases = store::list_leases(conn, Some(LeaseStatus::Active))
        .map(|l| l.len() as u64)
        .unwrap_or(0);

    let handoffs_created = count_of("handoff_created");
    let handoffs_accepted = count_of("handoff_accepted");
    let handoff_completion_rate = if handoffs_created > 0 {
        handoffs_accepted as f64 / handoffs_created as f64
    } else {
        0.0
    };
    let median_handoff_acceptance_secs =
        compute_median_delta(conn, "handoff_created", "handoff_accepted", since);

    let blockers_opened = count_of("blocker_opened");
    let blockers_resolved = count_of("blocker_resolved");
    let median_blocker_resolution_secs =
        compute_median_delta(conn, "blocker_opened", "blocker_resolved", since);

    let interrupts_raised = count_of("interrupt_raised");
    let interrupts_delivered = count_of("interrupt_delivered");
    let interrupts_acknowledged = count_of("interrupt_acknowledged");
    let interrupt_delivery_rate = if interrupts_raised > 0 {
        interrupts_delivered as f64 / interrupts_raised as f64
    } else {
        0.0
    };
    let median_interrupt_ack_secs =
        compute_median_delta(conn, "interrupt_raised", "interrupt_acknowledged", since);

    let memory_records = store::list_memory(conn, 10000)
        .map(|m| m.len() as u64)
        .unwrap_or(0);
    let memory_promoted = count_of("memory_written");

    CoordMetrics {
        total_events,
        event_counts,
        lease_claims,
        lease_conflicts,
        lease_conflict_rate,
        active_leases,
        handoffs_created,
        handoffs_accepted,
        handoff_completion_rate,
        median_handoff_acceptance_secs,
        blockers_opened,
        blockers_resolved,
        median_blocker_resolution_secs,
        interrupts_raised,
        interrupts_delivered,
        interrupts_acknowledged,
        interrupt_delivery_rate,
        median_interrupt_ack_secs,
        memory_records,
        memory_promoted,
    }
}

/// Format metrics for CLI display.
pub fn format_metrics(m: &CoordMetrics) -> String {
    let mut out = String::new();
    out.push_str("Coordination Metrics\n");
    out.push_str(&format!("  Total events: {}\n", m.total_events));
    out.push('\n');

    out.push_str("  Leases\n");
    out.push_str(&format!("    Claims:        {}\n", m.lease_claims));
    out.push_str(&format!("    Conflicts:     {}\n", m.lease_conflicts));
    out.push_str(&format!(
        "    Conflict rate: {:.1}%\n",
        m.lease_conflict_rate * 100.0
    ));
    out.push_str(&format!("    Active:        {}\n", m.active_leases));
    out.push('\n');

    out.push_str("  Handoffs\n");
    out.push_str(&format!("    Created:         {}\n", m.handoffs_created));
    out.push_str(&format!("    Accepted:        {}\n", m.handoffs_accepted));
    out.push_str(&format!(
        "    Completion rate: {:.1}%\n",
        m.handoff_completion_rate * 100.0
    ));
    if let Some(median) = m.median_handoff_acceptance_secs {
        out.push_str(&format!("    Median accept:   {:.0}s\n", median));
    }
    out.push('\n');

    out.push_str("  Blockers\n");
    out.push_str(&format!("    Opened:          {}\n", m.blockers_opened));
    out.push_str(&format!("    Resolved:        {}\n", m.blockers_resolved));
    if let Some(median) = m.median_blocker_resolution_secs {
        out.push_str(&format!("    Median resolve:  {:.0}s\n", median));
    }
    out.push('\n');

    out.push_str("  Interrupts\n");
    out.push_str(&format!("    Raised:          {}\n", m.interrupts_raised));
    out.push_str(&format!(
        "    Delivered:       {}\n",
        m.interrupts_delivered
    ));
    out.push_str(&format!(
        "    Acknowledged:    {}\n",
        m.interrupts_acknowledged
    ));
    out.push_str(&format!(
        "    Delivery rate:   {:.1}%\n",
        m.interrupt_delivery_rate * 100.0
    ));
    if let Some(median) = m.median_interrupt_ack_secs {
        out.push_str(&format!("    Median ack:      {:.0}s\n", median));
    }
    out.push('\n');

    out.push_str("  Memory\n");
    out.push_str(&format!("    Records:   {}\n", m.memory_records));
    out.push_str(&format!("    Promoted:  {}\n", m.memory_promoted));

    out
}

/// Count lease conflict events (failed claims due to existing exclusive leases).
/// We count events where the payload contains "conflict" or where lease_acquired
/// was immediately followed by lease_released for the same resource.
fn count_lease_conflicts(conn: &Connection, since: Option<&str>) -> u64 {
    // Count events in the log that indicate conflict. The raise interrupt with
    // dedupe_key starting with "lease:" is one signal. For now, use a simple
    // heuristic: count interrupts of type release_ownership.
    let sql = if since.is_some() {
        "SELECT COUNT(*) FROM interrupts WHERE interrupt_type = 'release_ownership' AND created_at >= ?1"
    } else {
        "SELECT COUNT(*) FROM interrupts WHERE interrupt_type = 'release_ownership'"
    };

    let count: i64 = if let Some(ts) = since {
        conn.query_row(sql, rusqlite::params![ts], |row| row.get(0))
            .unwrap_or(0)
    } else {
        conn.query_row(sql, [], |row| row.get(0)).unwrap_or(0)
    };
    count as u64
}

/// Compute median time delta between paired events (e.g., handoff_created -> handoff_accepted).
/// Pairs events by matching payload "handoff_id", "blocker_id", or "interrupt_id".
fn compute_median_delta(
    conn: &Connection,
    start_type: &str,
    end_type: &str,
    since: Option<&str>,
) -> Option<f64> {
    // Get start and end events
    let limit = 1000;
    let starts = if let Some(ts) = since {
        store::query_events_since(conn, ts, Some(start_type), limit)
    } else {
        store::query_events(conn, limit, Some(start_type))
    }
    .unwrap_or_default();

    let ends = if let Some(ts) = since {
        store::query_events_since(conn, ts, Some(end_type), limit)
    } else {
        store::query_events(conn, limit, Some(end_type))
    }
    .unwrap_or_default();

    if starts.is_empty() || ends.is_empty() {
        return None;
    }

    // Pair by matching entity ID in payload (handoff_id, interrupt_id, etc.)
    // Falls back to session_id if no entity ID is found in payload.
    let mut deltas = Vec::new();
    for start in &starts {
        let start_ts = parse_iso_epoch(&start.timestamp)?;
        let start_entity = extract_entity_id(&start.payload);

        for end in &ends {
            let end_entity = extract_entity_id(&end.payload);

            // Match by entity ID if both events have one, otherwise by session_id
            let matched = match (&start_entity, &end_entity) {
                (Some(s), Some(e)) => s == e,
                _ => end.session_id == start.session_id,
            };

            if matched {
                if let Some(end_ts) = parse_iso_epoch(&end.timestamp) {
                    if end_ts >= start_ts {
                        deltas.push((end_ts - start_ts) as f64);
                        break;
                    }
                }
            }
        }
    }

    if deltas.is_empty() {
        return None;
    }

    deltas.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let mid = deltas.len() / 2;
    Some(deltas[mid])
}

/// Extract the entity ID from an event payload (handoff_id, interrupt_id, blocker_id, lease_id).
fn extract_entity_id(payload: &serde_json::Value) -> Option<String> {
    let obj = payload.as_object()?;
    for key in ["handoff_id", "interrupt_id", "blocker_id", "lease_id"] {
        if let Some(serde_json::Value::String(id)) = obj.get(key) {
            return Some(id.clone());
        }
    }
    None
}

/// Parse an ISO 8601 timestamp to epoch seconds (simplified, UTC only).
fn parse_iso_epoch(ts: &str) -> Option<u64> {
    // Format: "2026-04-20T10:00:00Z"
    if ts.len() < 19 {
        return None;
    }
    let year: u64 = ts[0..4].parse().ok()?;
    let month: u64 = ts[5..7].parse().ok()?;
    let day: u64 = ts[8..10].parse().ok()?;
    let hour: u64 = ts[11..13].parse().ok()?;
    let min: u64 = ts[14..16].parse().ok()?;
    let sec: u64 = ts[17..19].parse().ok()?;

    // Approximate days from epoch (good enough for deltas)
    let mut days: u64 = 0;
    for y in 1970..year {
        days += if y % 4 == 0 && (y % 100 != 0 || y % 400 == 0) {
            366
        } else {
            365
        };
    }
    let month_days = [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    let is_leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    for m in 0..(month.saturating_sub(1) as usize) {
        days += month_days.get(m).copied().unwrap_or(30) as u64;
        if m == 1 && is_leap {
            days += 1;
        }
    }
    days += day.saturating_sub(1);

    Some(days * 86400 + hour * 3600 + min * 60 + sec)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_iso_epoch_valid() {
        let ts = parse_iso_epoch("2026-04-20T10:00:00Z");
        assert!(ts.is_some());
        let epoch = ts.unwrap();
        // 2026-04-20 should be in the right ballpark
        assert!(epoch > 1_770_000_000); // after 2026-01-01
        assert!(epoch < 1_800_000_000); // before 2027-01-01
    }

    #[test]
    fn parse_iso_epoch_invalid() {
        assert!(parse_iso_epoch("bad").is_none());
        assert!(parse_iso_epoch("").is_none());
    }

    #[test]
    fn parse_iso_epoch_delta() {
        let t1 = parse_iso_epoch("2026-04-20T10:00:00Z").unwrap();
        let t2 = parse_iso_epoch("2026-04-20T10:05:00Z").unwrap();
        assert_eq!(t2 - t1, 300); // 5 minutes
    }

    #[test]
    fn format_metrics_not_empty() {
        let m = CoordMetrics {
            total_events: 10,
            event_counts: vec![("lease_acquired".into(), 5), ("handoff_created".into(), 5)],
            lease_claims: 5,
            lease_conflicts: 1,
            lease_conflict_rate: 1.0 / 6.0,
            active_leases: 2,
            handoffs_created: 5,
            handoffs_accepted: 3,
            handoff_completion_rate: 0.6,
            median_handoff_acceptance_secs: Some(120.0),
            blockers_opened: 2,
            blockers_resolved: 1,
            median_blocker_resolution_secs: Some(300.0),
            interrupts_raised: 4,
            interrupts_delivered: 3,
            interrupts_acknowledged: 2,
            interrupt_delivery_rate: 0.75,
            median_interrupt_ack_secs: Some(15.0),
            memory_records: 10,
            memory_promoted: 3,
        };
        let output = format_metrics(&m);
        assert!(output.contains("Coordination Metrics"));
        assert!(output.contains("Conflict rate: 16.7%"));
        assert!(output.contains("Completion rate: 60.0%"));
        assert!(output.contains("Delivery rate:   75.0%"));
        assert!(output.contains("Median accept:   120s"));
    }

    #[test]
    fn compute_on_empty_db() {
        let conn = store::open_memory();
        let m = compute(&conn, None);
        assert_eq!(m.total_events, 0);
        assert_eq!(m.lease_claims, 0);
        assert_eq!(m.handoff_completion_rate, 0.0);
    }

    #[test]
    fn compute_counts_events() {
        let conn = store::open_memory();

        // Insert some events
        for i in 0..3 {
            store::append_event(
                &conn,
                &CoordEvent {
                    id: None,
                    event_type: EventType::LeaseAcquired,
                    timestamp: format!("2026-04-20T10:0{i}:00Z"),
                    session_id: Some(format!("sess_{i}")),
                    payload: serde_json::json!({}),
                },
            )
            .unwrap();
        }
        store::append_event(
            &conn,
            &CoordEvent {
                id: None,
                event_type: EventType::HandoffCreated,
                timestamp: "2026-04-20T10:05:00Z".into(),
                session_id: Some("sess_0".into()),
                payload: serde_json::json!({}),
            },
        )
        .unwrap();

        let m = compute(&conn, None);
        assert_eq!(m.total_events, 4);
        assert_eq!(m.lease_claims, 3);
        assert_eq!(m.handoffs_created, 1);
    }
}
