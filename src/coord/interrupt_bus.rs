#![allow(dead_code)]

use rusqlite::Connection;

use crate::session::{ClaudeSession, SessionStatus};
use crate::terminals;

use super::store;
use super::types::*;

/// Attempt to deliver pending interrupts to live sessions.
/// Returns a list of (interrupt_id, status_message) for deliveries made or skipped.
pub fn deliver_pending(conn: &Connection, sessions: &[ClaudeSession]) -> Vec<(String, String)> {
    let _ = store::expire_stale_interrupts(conn);

    let interrupts = match store::list_deliverable_interrupts(conn) {
        Ok(list) => list,
        Err(e) => {
            crate::logger::log("INTERRUPT_BUS", &format!("Failed to list interrupts: {e}"));
            return Vec::new();
        }
    };

    let mut results = Vec::new();

    for interrupt in &interrupts {
        // Find matching live session
        let session = sessions
            .iter()
            .find(|s| s.session_id == interrupt.target_session_id);

        let Some(session) = session else {
            // Target session not found among live sessions -- skip, don't expire
            continue;
        };

        // Check delivery mode against session status
        if !can_deliver(interrupt, session) {
            continue;
        }

        // Format and deliver
        let message = format_interrupt_message(interrupt);
        match terminals::send_input(session, &message) {
            Ok(()) => {
                let _ = store::mark_interrupt_delivered(conn, &interrupt.id);
                let _ = store::append_event(
                    conn,
                    &CoordEvent {
                        id: None,
                        event_type: EventType::InterruptDelivered,
                        timestamp: crate::logger::timestamp_now(),
                        session_id: Some(interrupt.target_session_id.clone()),
                        payload: serde_json::json!({
                            "interrupt_id": interrupt.id,
                            "type": interrupt.interrupt_type.as_str(),
                        }),
                    },
                );
                results.push((
                    interrupt.id.clone(),
                    format!(
                        "Interrupt delivered: {} ({}) -> {}",
                        interrupt.interrupt_type,
                        interrupt.priority,
                        session.display_name()
                    ),
                ));
            }
            Err(e) => {
                crate::logger::log(
                    "INTERRUPT_BUS",
                    &format!("Delivery failed for {}: {e}", interrupt.id),
                );
                // Set a 5-minute expiry on interrupts that fail delivery,
                // so they don't persist forever in the pending queue.
                if interrupt.expires_at.is_none() {
                    let expiry = {
                        let epoch = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_secs()
                            + 300; // 5 minutes
                        let d = epoch / 86400;
                        let s = epoch % 86400;
                        let (y, m, day) = crate::logger::days_to_date(d);
                        format!(
                            "{y:04}-{m:02}-{day:02}T{:02}:{:02}:{:02}Z",
                            s / 3600,
                            (s % 3600) / 60,
                            s % 60
                        )
                    };
                    let _ = conn.execute(
                        "UPDATE interrupts SET expires_at = ?1 WHERE id = ?2 AND expires_at IS NULL",
                        rusqlite::params![expiry, interrupt.id],
                    );
                }
            }
        }
    }

    results
}

/// Check whether an interrupt can be delivered to a session based on delivery mode.
fn can_deliver(interrupt: &Interrupt, session: &ClaudeSession) -> bool {
    match interrupt.delivery_mode.as_str() {
        "immediate" => true,
        "safe_boundary" => {
            // Deliver if the session is NOT processing, OR if it IS processing but
            // between tool calls (pending_tool_name is None). Both represent a safe
            // boundary -- the agent is not mid-tool-execution.
            session.status != SessionStatus::Processing || session.pending_tool_name.is_none()
        }
        "waiting_only" => session.status == SessionStatus::WaitingInput,
        "manual_review" => false, // Requires operator action via CLI
        _ => {
            // Unknown mode -- default to safe_boundary behavior
            session.status != SessionStatus::Processing || session.pending_tool_name.is_none()
        }
    }
}

/// Format an interrupt as structured text for delivery to a session.
fn format_interrupt_message(interrupt: &Interrupt) -> String {
    let mut msg = format!(
        "[Interrupt: {}] {}\nPriority: {}",
        interrupt.interrupt_type, interrupt.reason, interrupt.priority
    );

    if let Some(ref payload) = interrupt.payload {
        if let Some(obj) = payload.as_object() {
            for (key, value) in obj {
                let val_str = match value {
                    serde_json::Value::String(s) => s.clone(),
                    other => other.to_string(),
                };
                msg.push_str(&format!("\n{key}: {val_str}"));
            }
        }
    }

    msg
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::RawSession;

    fn test_session(status: SessionStatus) -> ClaudeSession {
        let mut s = ClaudeSession::from_raw(RawSession {
            pid: 1,
            session_id: "s1".into(),
            cwd: "/tmp".into(),
            started_at: 0,
        });
        s.status = status;
        s
    }

    fn test_interrupt(id: &str, delivery_mode: &str, itype: InterruptType) -> Interrupt {
        Interrupt {
            id: id.into(),
            interrupt_type: itype,
            priority: "medium".into(),
            target_session_id: "s1".into(),
            reason: "test".into(),
            payload: None,
            delivery_mode: delivery_mode.into(),
            max_retries: 3,
            expires_at: None,
            dedupe_key: None,
            state: InterruptState::Pending,
            created_at: "2026-04-20T10:00:00Z".into(),
            delivered_at: None,
            acknowledged_at: None,
        }
    }

    #[test]
    fn can_deliver_immediate_always() {
        let interrupt = test_interrupt("i1", "immediate", InterruptType::Stop);

        let session = test_session(SessionStatus::Processing);
        assert!(can_deliver(&interrupt, &session));

        let session = test_session(SessionStatus::WaitingInput);
        assert!(can_deliver(&interrupt, &session));
    }

    #[test]
    fn can_deliver_waiting_only_checks_status() {
        let interrupt = test_interrupt("i2", "waiting_only", InterruptType::Nudge);

        let session = test_session(SessionStatus::Processing);
        assert!(!can_deliver(&interrupt, &session));

        let session = test_session(SessionStatus::WaitingInput);
        assert!(can_deliver(&interrupt, &session));

        let session = test_session(SessionStatus::NeedsInput);
        assert!(!can_deliver(&interrupt, &session));
    }

    #[test]
    fn can_deliver_manual_review_never() {
        let interrupt = test_interrupt("i3", "manual_review", InterruptType::Reroute);

        let session = test_session(SessionStatus::WaitingInput);
        assert!(!can_deliver(&interrupt, &session));
    }

    #[test]
    fn format_interrupt_message_basic() {
        let interrupt = Interrupt {
            id: "i1".into(),
            interrupt_type: InterruptType::Pause,
            priority: "high".into(),
            target_session_id: "s1".into(),
            reason: "Lease conflict on src/app.rs".into(),
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

        let msg = format_interrupt_message(&interrupt);
        assert!(msg.contains("[Interrupt: pause]"));
        assert!(msg.contains("Lease conflict on src/app.rs"));
        assert!(msg.contains("Priority: high"));
    }

    #[test]
    fn format_interrupt_message_with_payload() {
        let interrupt = Interrupt {
            id: "i1".into(),
            interrupt_type: InterruptType::ReleaseOwnership,
            priority: "high".into(),
            target_session_id: "s1".into(),
            reason: "Another agent needs src/app.rs".into(),
            payload: Some(serde_json::json!({"resource": "src/app.rs", "owner": "sess_9"})),
            delivery_mode: "safe_boundary".into(),
            max_retries: 3,
            expires_at: None,
            dedupe_key: None,
            state: InterruptState::Pending,
            created_at: "2026-04-20T10:00:00Z".into(),
            delivered_at: None,
            acknowledged_at: None,
        };

        let msg = format_interrupt_message(&interrupt);
        assert!(msg.contains("resource: src/app.rs"));
        assert!(msg.contains("owner: sess_9"));
    }
}
