#![allow(dead_code)]

use serde::Serialize;

use super::store;
use super::types::*;

/// Result of running one eval scenario.
#[derive(Debug, Clone, Serialize)]
pub struct EvalResult {
    pub name: String,
    pub passed: bool,
    pub detail: String,
}

/// Run all coordination eval scenarios against an in-memory database.
/// Returns results for each scenario.
pub fn run_evals() -> Vec<EvalResult> {
    vec![
        eval_lease_conflict_prevention(),
        eval_lease_release(),
        eval_lease_expiry(),
        eval_handoff_lifecycle(),
        eval_interrupt_lifecycle(),
        eval_interrupt_deduplication(),
        eval_blocker_lifecycle(),
        eval_memory_insert_and_search(),
        eval_event_log_append(),
        eval_deliverable_interrupt_priority_ordering(),
    ]
}

/// Format eval results for CLI display.
pub fn format_results(results: &[EvalResult]) -> String {
    let passed = results.iter().filter(|r| r.passed).count();
    let total = results.len();
    let mut out = String::new();

    out.push_str(&format!("Coordination Eval: {passed}/{total} passed\n\n"));

    for r in results {
        let icon = if r.passed { "PASS" } else { "FAIL" };
        out.push_str(&format!("  [{icon}] {}\n", r.name));
        if !r.passed {
            out.push_str(&format!("         {}\n", r.detail));
        }
    }

    out
}

// -- Scenarios -----------------------------------------------------------------

fn eval_lease_conflict_prevention() -> EvalResult {
    let name = "Lease conflict prevention".into();
    let conn = store::open_memory();

    // Agent 1 claims exclusive lease
    let lease1 = Lease {
        id: "l1".into(),
        owner_session_id: "sess_a".into(),
        owner_agent: "claude-code".into(),
        resource_kind: "path_glob".into(),
        resource_value: "src/app.rs".into(),
        mode: LeaseMode::Exclusive,
        reason: "editing".into(),
        acquired_at: "2026-04-20T10:00:00Z".into(),
        expires_at: None,
        status: LeaseStatus::Active,
    };
    store::upsert_lease(&conn, &lease1).unwrap();

    // Agent 2 tries to claim the same resource
    let conflict = store::find_conflicting_lease(&conn, "path_glob", "src/app.rs", "sess_b");

    match conflict {
        Ok(Some(c)) => EvalResult {
            name,
            passed: c.owner_session_id == "sess_a",
            detail: format!("Conflict detected: owned by {}", c.owner_session_id),
        },
        Ok(None) => EvalResult {
            name,
            passed: false,
            detail: "No conflict detected -- should have blocked".into(),
        },
        Err(e) => EvalResult {
            name,
            passed: false,
            detail: format!("Error: {e}"),
        },
    }
}

fn eval_lease_release() -> EvalResult {
    let name = "Lease release".into();
    let conn = store::open_memory();

    let lease = Lease {
        id: "l_rel".into(),
        owner_session_id: "sess_a".into(),
        owner_agent: "claude-code".into(),
        resource_kind: "file".into(),
        resource_value: "src/main.rs".into(),
        mode: LeaseMode::Exclusive,
        reason: "test".into(),
        acquired_at: "2026-04-20T10:00:00Z".into(),
        expires_at: None,
        status: LeaseStatus::Active,
    };
    store::upsert_lease(&conn, &lease).unwrap();

    let released = store::release_lease(&conn, "l_rel").unwrap();
    let after = store::get_lease(&conn, "l_rel").unwrap();

    let ok = released
        && after
            .map(|l| l.status == LeaseStatus::Released)
            .unwrap_or(false);
    EvalResult {
        name,
        passed: ok,
        detail: if ok {
            "Lease released successfully".into()
        } else {
            "Release failed".into()
        },
    }
}

fn eval_lease_expiry() -> EvalResult {
    let name = "Lease auto-expiry".into();
    let conn = store::open_memory();

    let lease = Lease {
        id: "l_exp".into(),
        owner_session_id: "sess_a".into(),
        owner_agent: "claude-code".into(),
        resource_kind: "file".into(),
        resource_value: "src/old.rs".into(),
        mode: LeaseMode::Exclusive,
        reason: "test".into(),
        acquired_at: "2020-01-01T00:00:00Z".into(),
        expires_at: Some("2020-01-01T01:00:00Z".into()),
        status: LeaseStatus::Active,
    };
    store::upsert_lease(&conn, &lease).unwrap();

    let count = store::expire_stale_leases(&conn).unwrap();
    let after = store::get_lease(&conn, "l_exp").unwrap();

    let ok = count == 1
        && after
            .map(|l| l.status == LeaseStatus::Expired)
            .unwrap_or(false);
    EvalResult {
        name,
        passed: ok,
        detail: if ok {
            "Expired 1 stale lease".into()
        } else {
            format!("Expected 1 expiry, got {count}")
        },
    }
}

fn eval_handoff_lifecycle() -> EvalResult {
    let name = "Handoff create and list".into();
    let conn = store::open_memory();

    let handoff = Handoff {
        id: "h_test".into(),
        from_session_id: "sess_a".into(),
        to_session_id: Some("sess_b".into()),
        task_id: "task_1".into(),
        summary: "Fix path normalization".into(),
        state: HandoffState {
            goal: "Fix paths".into(),
            artifacts: vec!["src/paths.rs".into()],
            attempted: vec![],
            next_steps: vec!["Normalize backslashes".into()],
        },
        priority: "high".into(),
        created_at: "2026-04-20T10:00:00Z".into(),
        acknowledged_at: None,
    };
    store::insert_handoff(&conn, &handoff).unwrap();

    let pending = store::list_pending_handoffs(&conn).unwrap();
    let ok = pending.len() == 1
        && pending[0].id == "h_test"
        && pending[0].state.goal == "Fix paths"
        && pending[0].state.next_steps.len() == 1;

    EvalResult {
        name,
        passed: ok,
        detail: if ok {
            "Handoff created with structured state".into()
        } else {
            format!("Unexpected: {} pending handoffs", pending.len())
        },
    }
}

fn eval_interrupt_lifecycle() -> EvalResult {
    let name = "Interrupt lifecycle: pending -> delivered -> acknowledged".into();
    let conn = store::open_memory();

    let intr = Interrupt {
        id: "i_life".into(),
        interrupt_type: InterruptType::Pause,
        priority: "high".into(),
        target_session_id: "sess_a".into(),
        reason: "test lifecycle".into(),
        payload: None,
        delivery_mode: "safe_boundary".into(),
        max_retries: 3,
        expires_at: None,
        dedupe_key: None,
        state: InterruptState::Pending,
        created_at: "2026-04-20T10:00:00Z".into(),
        delivered_at: None,
        acknowledged_at: None,
    };
    store::insert_interrupt(&conn, &intr).unwrap();

    // pending -> delivered
    let del_ok = store::mark_interrupt_delivered(&conn, "i_life").unwrap();
    let after_del = store::get_interrupt(&conn, "i_life").unwrap().unwrap();

    // delivered -> acknowledged
    let ack_ok = store::mark_interrupt_acknowledged(&conn, "i_life").unwrap();
    let after_ack = store::get_interrupt(&conn, "i_life").unwrap().unwrap();

    let ok = del_ok
        && after_del.state == InterruptState::Delivered
        && after_del.delivered_at.is_some()
        && ack_ok
        && after_ack.state == InterruptState::Acknowledged
        && after_ack.acknowledged_at.is_some();

    EvalResult {
        name,
        passed: ok,
        detail: if ok {
            "Full lifecycle: pending -> delivered -> acknowledged".into()
        } else {
            format!(
                "State after deliver: {:?}, after ack: {:?}",
                after_del.state, after_ack.state
            )
        },
    }
}

fn eval_interrupt_deduplication() -> EvalResult {
    let name = "Interrupt deduplication via dedupe_key".into();
    let conn = store::open_memory();

    let intr1 = Interrupt {
        id: "i_dup1".into(),
        interrupt_type: InterruptType::Compact,
        priority: "medium".into(),
        target_session_id: "sess_a".into(),
        reason: "first".into(),
        payload: None,
        delivery_mode: "safe_boundary".into(),
        max_retries: 3,
        expires_at: None,
        dedupe_key: Some("compact:sess_a".into()),
        state: InterruptState::Pending,
        created_at: "2026-04-20T10:00:00Z".into(),
        delivered_at: None,
        acknowledged_at: None,
    };
    store::insert_interrupt(&conn, &intr1).unwrap();

    let dup = store::find_duplicate_interrupt(&conn, "compact:sess_a").unwrap();
    let ok = dup.is_some() && dup.as_ref().unwrap().id == "i_dup1";

    EvalResult {
        name,
        passed: ok,
        detail: if ok {
            "Duplicate detected by dedupe_key".into()
        } else {
            "Deduplication failed".into()
        },
    }
}

fn eval_blocker_lifecycle() -> EvalResult {
    let name = "Blocker open and resolve".into();
    let conn = store::open_memory();

    let blocker = Blocker {
        id: "b_test".into(),
        task_id: "task_docs".into(),
        depends_on: Some("task_auth".into()),
        waiting_for: "JWT middleware contract".into(),
        status: BlockerStatus::Open,
        owner_session_id: "sess_a".into(),
        created_at: "2026-04-20T10:00:00Z".into(),
        resolved_at: None,
    };
    store::insert_blocker(&conn, &blocker).unwrap();

    let open = store::list_blockers(&conn, Some(BlockerStatus::Open)).unwrap();
    store::resolve_blocker(&conn, "b_test").unwrap();
    let resolved = store::list_blockers(&conn, Some(BlockerStatus::Resolved)).unwrap();
    let still_open = store::list_blockers(&conn, Some(BlockerStatus::Open)).unwrap();

    let ok = open.len() == 1 && resolved.len() == 1 && still_open.is_empty();
    EvalResult {
        name,
        passed: ok,
        detail: if ok {
            "Blocker resolved successfully".into()
        } else {
            format!(
                "open={}, resolved={}, still_open={}",
                open.len(),
                resolved.len(),
                still_open.len()
            )
        },
    }
}

fn eval_memory_insert_and_search() -> EvalResult {
    let name = "Memory insert and FTS5 search".into();
    let conn = store::open_memory();

    let record = MemoryRecord {
        id: "mem_eval".into(),
        mem_type: "workflow".into(),
        scope: serde_json::json!({"project": "claudectl"}),
        subjects: vec![Subject {
            kind: "path".into(),
            value: "src/health.rs".into(),
        }],
        summary: "When changing health thresholds always run integration tests".into(),
        evidence: vec![],
        source: None,
        confidence: 0.9,
        created_at: "2026-04-20T10:00:00Z".into(),
        updated_at: "2026-04-20T10:00:00Z".into(),
        expires_at: None,
        tags: vec!["health".into(), "tests".into()],
    };
    store::insert_memory(&conn, &record).unwrap();

    let results = store::search_memory(&conn, "health thresholds", 10).unwrap();
    let ok = results.len() == 1 && results[0].id == "mem_eval";

    EvalResult {
        name,
        passed: ok,
        detail: if ok {
            "FTS5 search found the record".into()
        } else {
            format!("Search returned {} results", results.len())
        },
    }
}

fn eval_event_log_append() -> EvalResult {
    let name = "Event log append and query".into();
    let conn = store::open_memory();

    for i in 0..5 {
        store::append_event(
            &conn,
            &CoordEvent {
                id: None,
                event_type: EventType::SessionObserved,
                timestamp: format!("2026-04-20T10:0{i}:00Z"),
                session_id: Some(format!("sess_{i}")),
                payload: serde_json::json!({"pid": i}),
            },
        )
        .unwrap();
    }

    let all = store::query_events(&conn, 100, None).unwrap();
    let filtered = store::query_events(&conn, 100, Some("session_observed")).unwrap();

    let ok = all.len() == 5 && filtered.len() == 5;
    EvalResult {
        name,
        passed: ok,
        detail: if ok {
            "5 events appended and queried".into()
        } else {
            format!("all={}, filtered={}", all.len(), filtered.len())
        },
    }
}

fn eval_deliverable_interrupt_priority_ordering() -> EvalResult {
    let name = "Deliverable interrupts ordered by priority".into();
    let conn = store::open_memory();

    let make = |id: &str, priority: &str| Interrupt {
        id: id.into(),
        interrupt_type: InterruptType::Nudge,
        priority: priority.into(),
        target_session_id: "sess_a".into(),
        reason: "test".into(),
        payload: None,
        delivery_mode: "safe_boundary".into(),
        max_retries: 3,
        expires_at: None,
        dedupe_key: None,
        state: InterruptState::Pending,
        created_at: "2026-04-20T10:00:00Z".into(),
        delivered_at: None,
        acknowledged_at: None,
    };

    store::insert_interrupt(&conn, &make("i_low", "low")).unwrap();
    store::insert_interrupt(&conn, &make("i_crit", "critical")).unwrap();
    store::insert_interrupt(&conn, &make("i_med", "medium")).unwrap();
    store::insert_interrupt(&conn, &make("i_high", "high")).unwrap();

    let deliverable = store::list_deliverable_interrupts(&conn).unwrap();
    let order: Vec<&str> = deliverable.iter().map(|i| i.id.as_str()).collect();

    let ok = order == vec!["i_crit", "i_high", "i_med", "i_low"];
    EvalResult {
        name,
        passed: ok,
        detail: if ok {
            "Priority ordering: critical > high > medium > low".into()
        } else {
            format!("Actual order: {order:?}")
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_evals_pass() {
        let results = run_evals();
        for r in &results {
            assert!(r.passed, "Eval failed: {} -- {}", r.name, r.detail);
        }
    }

    #[test]
    fn format_results_shows_pass_fail() {
        let results = vec![
            EvalResult {
                name: "test_pass".into(),
                passed: true,
                detail: "ok".into(),
            },
            EvalResult {
                name: "test_fail".into(),
                passed: false,
                detail: "something broke".into(),
            },
        ];
        let output = format_results(&results);
        assert!(output.contains("[PASS] test_pass"));
        assert!(output.contains("[FAIL] test_fail"));
        assert!(output.contains("something broke"));
        assert!(output.contains("1/2 passed"));
    }
}
