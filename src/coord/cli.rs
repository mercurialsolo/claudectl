use std::io;

use super::store;
use super::types::*;

pub fn dispatch(subcommand: &str, json_mode: bool) -> io::Result<()> {
    let parts: Vec<&str> = subcommand.split_whitespace().collect();
    let cmd = parts.first().copied().unwrap_or("help");

    match cmd {
        "events" => {
            let limit = parts.get(1).and_then(|s| s.parse().ok()).unwrap_or(50);
            let type_filter = parts.get(2).copied();
            list_events(limit, type_filter, json_mode)
        }
        "leases" => list_leases(json_mode),
        "blockers" => list_blockers(json_mode),
        "handoffs" => list_handoffs(json_mode),
        "interrupts" => list_interrupts(json_mode),
        "memory" => handle_memory(&parts[1..], json_mode),
        "claim" => cmd_claim(&parts, json_mode),
        "release" => cmd_release(&parts, json_mode),
        "handoff" => cmd_handoff(&parts, json_mode),
        "accept" => cmd_accept_handoff(&parts, json_mode),
        "block" => cmd_open_blocker(&parts, json_mode),
        "unblock" => cmd_resolve_blocker(&parts, json_mode),
        "raise" => cmd_raise(&parts, json_mode),
        "ack" => cmd_ack(&parts, json_mode),
        "promote" => cmd_promote(&parts, json_mode),
        "context" => cmd_context(&parts, json_mode),
        "adapters" => cmd_adapters(&parts, json_mode),
        "metrics" => cmd_metrics(&parts, json_mode),
        "eval" => cmd_eval(json_mode),
        "prune" => cmd_prune(&parts, json_mode),
        "help" | "" => print_help(),
        _ => {
            eprintln!("Unknown coord subcommand: '{cmd}'");
            eprintln!();
            print_help()
        }
    }
}

fn open_or_exit() -> rusqlite::Connection {
    match store::open() {
        Ok(conn) => conn,
        Err(e) => {
            eprintln!("Failed to open coordination store: {e}");
            std::process::exit(1);
        }
    }
}

// -- Events --------------------------------------------------------------------

fn list_events(limit: usize, type_filter: Option<&str>, json_mode: bool) -> io::Result<()> {
    let conn = open_or_exit();
    let events = store::query_events(&conn, limit, type_filter).map_err(io::Error::other)?;

    if json_mode {
        let json = serde_json::to_string_pretty(&events).unwrap_or_default();
        println!("{json}");
        return Ok(());
    }

    if events.is_empty() {
        println!("No events recorded.");
        return Ok(());
    }

    println!(
        "{:<6} {:<24} {:<22} {:<16} PAYLOAD",
        "ID", "TYPE", "TIMESTAMP", "SESSION"
    );
    println!("{}", "-".repeat(90));

    for event in &events {
        let id = event.id.map(|i| i.to_string()).unwrap_or_default();
        let session = event.session_id.as_deref().unwrap_or("-");
        let payload = truncate(&event.payload.to_string(), 30);
        println!(
            "{:<6} {:<24} {:<22} {:<16} {}",
            id, event.event_type, event.timestamp, session, payload
        );
    }

    println!("\n{} event(s)", events.len());
    Ok(())
}

// -- Leases --------------------------------------------------------------------

fn list_leases(json_mode: bool) -> io::Result<()> {
    let conn = open_or_exit();

    // Expire stale leases before listing
    let _ = store::expire_stale_leases(&conn);

    let leases = store::list_leases(&conn, Some(LeaseStatus::Active)).map_err(io::Error::other)?;

    if json_mode {
        let json = serde_json::to_string_pretty(&leases).unwrap_or_default();
        println!("{json}");
        return Ok(());
    }

    if leases.is_empty() {
        println!("No active leases.");
        return Ok(());
    }

    println!(
        "{:<16} {:<16} {:<20} {:<14} {:<8} EXPIRES",
        "ID", "SESSION", "RESOURCE", "MODE", "STATUS"
    );
    println!("{}", "-".repeat(90));

    for lease in &leases {
        let resource = truncate(
            &format!("{}:{}", lease.resource_kind, lease.resource_value),
            20,
        );
        let expires = lease.expires_at.as_deref().unwrap_or("-");
        println!(
            "{:<16} {:<16} {:<20} {:<14} {:<8} {}",
            truncate(&lease.id, 16),
            truncate(&lease.owner_session_id, 16),
            resource,
            lease.mode,
            lease.status,
            expires
        );
    }

    println!("\n{} active lease(s)", leases.len());
    Ok(())
}

// -- Blockers ------------------------------------------------------------------

fn list_blockers(json_mode: bool) -> io::Result<()> {
    let conn = open_or_exit();
    let blockers =
        store::list_blockers(&conn, Some(BlockerStatus::Open)).map_err(io::Error::other)?;

    if json_mode {
        let json = serde_json::to_string_pretty(&blockers).unwrap_or_default();
        println!("{json}");
        return Ok(());
    }

    if blockers.is_empty() {
        println!("No open blockers.");
        return Ok(());
    }

    println!(
        "{:<16} {:<16} {:<16} {:<8} WAITING FOR",
        "ID", "TASK", "DEPENDS ON", "STATUS"
    );
    println!("{}", "-".repeat(80));

    for b in &blockers {
        let depends = b.depends_on.as_deref().unwrap_or("-");
        println!(
            "{:<16} {:<16} {:<16} {:<8} {}",
            truncate(&b.id, 16),
            truncate(&b.task_id, 16),
            truncate(depends, 16),
            b.status,
            truncate(&b.waiting_for, 40)
        );
    }

    println!("\n{} open blocker(s)", blockers.len());
    Ok(())
}

// -- Handoffs ------------------------------------------------------------------

fn list_handoffs(json_mode: bool) -> io::Result<()> {
    let conn = open_or_exit();
    let handoffs = store::list_handoffs(&conn).map_err(io::Error::other)?;

    if json_mode {
        let json = serde_json::to_string_pretty(&handoffs).unwrap_or_default();
        println!("{json}");
        return Ok(());
    }

    if handoffs.is_empty() {
        println!("No handoffs recorded.");
        return Ok(());
    }

    println!(
        "{:<14} {:<14} {:<14} {:<10} {:<8} SUMMARY",
        "ID", "FROM", "TO", "TASK", "PRIORITY"
    );
    println!("{}", "-".repeat(90));

    for h in &handoffs {
        let to = h.to_session_id.as_deref().unwrap_or("-");
        let ack = if h.acknowledged_at.is_some() {
            " [ack]"
        } else {
            ""
        };
        println!(
            "{:<14} {:<14} {:<14} {:<10} {:<8} {}{}",
            truncate(&h.id, 14),
            truncate(&h.from_session_id, 14),
            truncate(to, 14),
            truncate(&h.task_id, 10),
            h.priority,
            truncate(&h.summary, 30),
            ack
        );
    }

    println!("\n{} handoff(s)", handoffs.len());
    Ok(())
}

// -- Interrupts ----------------------------------------------------------------

fn list_interrupts(json_mode: bool) -> io::Result<()> {
    let conn = open_or_exit();
    let interrupts =
        store::list_interrupts(&conn, Some(InterruptState::Pending)).map_err(io::Error::other)?;

    if json_mode {
        let json = serde_json::to_string_pretty(&interrupts).unwrap_or_default();
        println!("{json}");
        return Ok(());
    }

    if interrupts.is_empty() {
        println!("No pending interrupts.");
        return Ok(());
    }

    println!(
        "{:<14} {:<20} {:<10} {:<16} {:<10} REASON",
        "ID", "TYPE", "PRIORITY", "TARGET", "STATE"
    );
    println!("{}", "-".repeat(90));

    for i in &interrupts {
        println!(
            "{:<14} {:<20} {:<10} {:<16} {:<10} {}",
            truncate(&i.id, 14),
            i.interrupt_type,
            i.priority,
            truncate(&i.target_session_id, 16),
            i.state,
            truncate(&i.reason, 30)
        );
    }

    println!("\n{} pending interrupt(s)", interrupts.len());
    Ok(())
}

// -- Memory --------------------------------------------------------------------

fn handle_memory(args: &[&str], json_mode: bool) -> io::Result<()> {
    let conn = open_or_exit();

    let (records, is_search) = if args.first().copied() == Some("search") && args.len() > 1 {
        let query = args[1..].join(" ");
        let results = store::search_memory(&conn, &query, 20).map_err(io::Error::other)?;
        (results, true)
    } else {
        let results = store::list_memory(&conn, 50).map_err(io::Error::other)?;
        (results, false)
    };

    if json_mode {
        let json = serde_json::to_string_pretty(&records).unwrap_or_default();
        println!("{json}");
        return Ok(());
    }

    if records.is_empty() {
        if is_search {
            println!("No memory records matched the search.");
        } else {
            println!("No memory records stored.");
        }
        return Ok(());
    }

    println!("{:<14} {:<14} {:<10} SUMMARY", "ID", "TYPE", "CONFIDENCE");
    println!("{}", "-".repeat(70));

    for r in &records {
        println!(
            "{:<14} {:<14} {:<10.2} {}",
            truncate(&r.id, 14),
            truncate(&r.mem_type, 14),
            r.confidence,
            truncate(&r.summary, 40)
        );
    }

    println!("\n{} record(s)", records.len());
    Ok(())
}

// -- Claim Ownership -----------------------------------------------------------

fn cmd_claim(parts: &[&str], json_mode: bool) -> io::Result<()> {
    let session = extract_flag(parts, "session");
    let path = extract_flag(parts, "path");

    let (Some(session_id), Some(resource)) = (session, path) else {
        eprintln!(
            "Usage: claudectl --coord \"claim --session <id> --path <resource> [--mode exclusive|advisory] [--reason text]\""
        );
        return Err(io::Error::other("missing required flags"));
    };

    let mode_str = extract_flag(parts, "mode").unwrap_or("exclusive");
    let mode = LeaseMode::parse(mode_str).unwrap_or(LeaseMode::Exclusive);
    let reason = extract_flag_rest(parts, "reason").unwrap_or_default();

    let conn = open_or_exit();
    let _ = store::expire_stale_leases(&conn);

    let lease_id = store::gen_id("lease");
    let now = crate::logger::timestamp_now();
    let lease = Lease {
        id: lease_id.clone(),
        owner_session_id: session_id.to_string(),
        owner_agent: "claude-code".into(),
        resource_kind: "path_glob".into(),
        resource_value: resource.to_string(),
        mode,
        reason,
        acquired_at: now.clone(),
        expires_at: None,
        status: LeaseStatus::Active,
    };

    // Atomic check-and-claim in a single transaction
    if let Some(conflict) = store::claim_lease_atomic(&conn, &lease).map_err(io::Error::other)? {
        let msg = format!(
            "Conflict: {} already holds exclusive lease on {} (lease {})",
            conflict.owner_session_id, resource, conflict.id
        );
        if json_mode {
            let json = serde_json::json!({"error": msg, "conflicting_lease": conflict.id});
            println!(
                "{}",
                serde_json::to_string_pretty(&json).unwrap_or_default()
            );
        } else {
            eprintln!("{msg}");
        }
        return Err(io::Error::other("lease conflict"));
    }

    let event = CoordEvent {
        id: None,
        event_type: EventType::LeaseAcquired,
        timestamp: now,
        session_id: Some(session_id.to_string()),
        payload: serde_json::json!({
            "lease_id": lease_id,
            "resource": resource,
            "mode": mode.as_str(),
        }),
    };
    let _ = store::append_event(&conn, &event);

    if json_mode {
        let json = serde_json::to_string_pretty(&lease).unwrap_or_default();
        println!("{json}");
    } else {
        println!("Lease acquired: {lease_id}");
        println!("  Session:  {session_id}");
        println!("  Resource: {resource}");
        println!("  Mode:     {mode}");
    }
    Ok(())
}

// -- Release Ownership ---------------------------------------------------------

fn cmd_release(parts: &[&str], json_mode: bool) -> io::Result<()> {
    let Some(lease_id) = parts.get(1) else {
        eprintln!("Usage: claudectl --coord \"release <lease_id>\"");
        return Err(io::Error::other("missing lease_id"));
    };

    let conn = open_or_exit();

    let Some(lease) = store::get_lease(&conn, lease_id).map_err(io::Error::other)? else {
        let msg = format!("Lease not found: {lease_id}");
        if json_mode {
            println!("{}", serde_json::json!({"error": msg}));
        } else {
            eprintln!("{msg}");
        }
        return Err(io::Error::other("not found"));
    };

    if lease.status != LeaseStatus::Active {
        let msg = format!("Lease {lease_id} is already {}", lease.status);
        if json_mode {
            println!("{}", serde_json::json!({"error": msg}));
        } else {
            eprintln!("{msg}");
        }
        return Err(io::Error::other("not active"));
    }

    store::release_lease(&conn, lease_id).map_err(io::Error::other)?;

    let now = crate::logger::timestamp_now();
    let event = CoordEvent {
        id: None,
        event_type: EventType::LeaseReleased,
        timestamp: now,
        session_id: Some(lease.owner_session_id.clone()),
        payload: serde_json::json!({
            "lease_id": lease_id,
            "resource": lease.resource_value,
        }),
    };
    let _ = store::append_event(&conn, &event);

    if json_mode {
        println!("{}", serde_json::json!({"released": lease_id}));
    } else {
        println!("Lease released: {lease_id}");
    }
    Ok(())
}

// -- Create Handoff ------------------------------------------------------------

fn cmd_handoff(parts: &[&str], json_mode: bool) -> io::Result<()> {
    let from = extract_flag(parts, "from");
    let task = extract_flag(parts, "task");
    let summary = extract_flag_rest(parts, "summary");

    let (Some(from_session), Some(task_id), Some(summary_text)) = (from, task, summary) else {
        eprintln!(
            "Usage: claudectl --coord \"handoff --from <session> --task <task_id> --summary <text> [--to <session>] [--priority high|medium|low]\""
        );
        return Err(io::Error::other("missing required flags"));
    };

    let to_session = extract_flag(parts, "to").map(|s| s.to_string());
    let priority = extract_flag(parts, "priority").unwrap_or("medium");

    let conn = open_or_exit();
    let handoff_id = store::gen_id("handoff");
    let now = crate::logger::timestamp_now();

    let handoff = Handoff {
        id: handoff_id.clone(),
        from_session_id: from_session.to_string(),
        to_session_id: to_session,
        task_id: task_id.to_string(),
        summary: summary_text.clone(),
        state: HandoffState {
            goal: summary_text,
            artifacts: vec![],
            attempted: vec![],
            next_steps: vec![],
        },
        priority: priority.to_string(),
        created_at: now.clone(),
        acknowledged_at: None,
    };

    store::insert_handoff(&conn, &handoff).map_err(io::Error::other)?;

    let event = CoordEvent {
        id: None,
        event_type: EventType::HandoffCreated,
        timestamp: now,
        session_id: Some(from_session.to_string()),
        payload: serde_json::json!({
            "handoff_id": handoff_id,
            "task_id": task_id,
            "to": handoff.to_session_id,
        }),
    };
    let _ = store::append_event(&conn, &event);

    if json_mode {
        let json = serde_json::to_string_pretty(&handoff).unwrap_or_default();
        println!("{json}");
    } else {
        println!("Handoff created: {handoff_id}");
        println!("  From:     {from_session}");
        if let Some(ref to) = handoff.to_session_id {
            println!("  To:       {to}");
        }
        println!("  Task:     {task_id}");
        println!("  Priority: {priority}");
    }
    Ok(())
}

// -- Accept Handoff ------------------------------------------------------------

fn cmd_accept_handoff(parts: &[&str], json_mode: bool) -> io::Result<()> {
    let Some(handoff_id) = parts.get(1) else {
        eprintln!("Usage: claudectl --coord \"accept <handoff_id>\"");
        return Err(io::Error::other("missing handoff_id"));
    };

    let conn = open_or_exit();

    let Some(handoff) = store::get_handoff(&conn, handoff_id).map_err(io::Error::other)? else {
        let msg = format!("Handoff not found: {handoff_id}");
        if json_mode {
            println!("{}", serde_json::json!({"error": msg}));
        } else {
            eprintln!("{msg}");
        }
        return Err(io::Error::other("not found"));
    };

    if handoff.acknowledged_at.is_some() {
        let msg = format!("Handoff {handoff_id} is already accepted");
        if json_mode {
            println!("{}", serde_json::json!({"error": msg}));
        } else {
            eprintln!("{msg}");
        }
        return Err(io::Error::other("already accepted"));
    }

    store::accept_handoff(&conn, handoff_id).map_err(io::Error::other)?;

    let now = crate::logger::timestamp_now();
    let event = CoordEvent {
        id: None,
        event_type: EventType::HandoffAccepted,
        timestamp: now,
        session_id: handoff.to_session_id.clone(),
        payload: serde_json::json!({
            "handoff_id": handoff_id,
            "from": handoff.from_session_id,
            "task_id": handoff.task_id,
        }),
    };
    let _ = store::append_event(&conn, &event);

    if json_mode {
        println!("{}", serde_json::json!({"accepted": handoff_id}));
    } else {
        println!("Handoff accepted: {handoff_id}");
    }
    Ok(())
}

// -- Blockers ------------------------------------------------------------------

fn cmd_open_blocker(parts: &[&str], json_mode: bool) -> io::Result<()> {
    let task = extract_flag(parts, "task");
    let waiting_for = extract_flag_rest(parts, "waiting-for");
    let session = extract_flag(parts, "session");

    let (Some(task_id), Some(waiting_text), Some(session_id)) = (task, waiting_for, session) else {
        eprintln!(
            "Usage: claudectl --coord \"block --task <id> --session <id> --waiting-for <text> [--depends-on <task_id>]\""
        );
        return Err(io::Error::other("missing required flags"));
    };

    let depends_on = extract_flag(parts, "depends-on").map(|s| s.to_string());
    let conn = open_or_exit();
    let blocker_id = store::gen_id("blocker");
    let now = crate::logger::timestamp_now();

    let blocker = Blocker {
        id: blocker_id.clone(),
        task_id: task_id.to_string(),
        depends_on,
        waiting_for: waiting_text,
        status: BlockerStatus::Open,
        owner_session_id: session_id.to_string(),
        created_at: now.clone(),
        resolved_at: None,
    };

    store::insert_blocker(&conn, &blocker).map_err(io::Error::other)?;

    let event = CoordEvent {
        id: None,
        event_type: EventType::BlockerOpened,
        timestamp: now,
        session_id: Some(session_id.to_string()),
        payload: serde_json::json!({
            "blocker_id": blocker_id,
            "task_id": task_id,
        }),
    };
    let _ = store::append_event(&conn, &event);

    if json_mode {
        let json = serde_json::to_string_pretty(&blocker).unwrap_or_default();
        println!("{json}");
    } else {
        println!("Blocker opened: {blocker_id}");
        println!("  Task:        {task_id}");
        println!("  Waiting for: {}", blocker.waiting_for);
    }
    Ok(())
}

fn cmd_resolve_blocker(parts: &[&str], json_mode: bool) -> io::Result<()> {
    let Some(blocker_id) = parts.get(1) else {
        eprintln!("Usage: claudectl --coord \"unblock <blocker_id>\"");
        return Err(io::Error::other("missing blocker_id"));
    };

    let conn = open_or_exit();

    let blockers = store::list_blockers(&conn, None).map_err(io::Error::other)?;
    let blocker = blockers.iter().find(|b| b.id == *blocker_id);

    let Some(blocker) = blocker else {
        let msg = format!("Blocker not found: {blocker_id}");
        if json_mode {
            println!("{}", serde_json::json!({"error": msg}));
        } else {
            eprintln!("{msg}");
        }
        return Err(io::Error::other("not found"));
    };

    if blocker.status == BlockerStatus::Resolved {
        let msg = format!("Blocker {blocker_id} is already resolved");
        if json_mode {
            println!("{}", serde_json::json!({"error": msg}));
        } else {
            eprintln!("{msg}");
        }
        return Err(io::Error::other("already resolved"));
    }

    store::resolve_blocker(&conn, blocker_id).map_err(io::Error::other)?;

    let now = crate::logger::timestamp_now();
    let event = CoordEvent {
        id: None,
        event_type: EventType::BlockerResolved,
        timestamp: now,
        session_id: Some(blocker.owner_session_id.clone()),
        payload: serde_json::json!({
            "blocker_id": blocker_id,
            "task_id": blocker.task_id,
        }),
    };
    let _ = store::append_event(&conn, &event);

    if json_mode {
        println!("{}", serde_json::json!({"resolved": blocker_id}));
    } else {
        println!("Blocker resolved: {blocker_id}");
    }
    Ok(())
}

// -- Raise Interrupt -----------------------------------------------------------

fn cmd_raise(parts: &[&str], json_mode: bool) -> io::Result<()> {
    let itype_str = extract_flag(parts, "type");
    let target = extract_flag(parts, "target");
    let reason = extract_flag_rest(parts, "reason");

    let (Some(itype_str), Some(target_session), Some(reason_text)) = (itype_str, target, reason)
    else {
        eprintln!(
            "Usage: claudectl --coord \"raise --type <type> --target <session> --reason <text> [--priority high] [--delivery safe_boundary] [--dedupe key] [--expires secs]\""
        );
        return Err(io::Error::other("missing required flags"));
    };

    let Some(itype) = InterruptType::parse(itype_str) else {
        eprintln!("Unknown interrupt type: '{itype_str}'");
        eprintln!(
            "Valid types: nudge, request_input, pause, compact, reroute, release_ownership, stop, resume, dependency_unblocked, handoff_ready"
        );
        return Err(io::Error::other("invalid type"));
    };

    let priority = extract_flag(parts, "priority").unwrap_or("medium");
    let delivery = extract_flag(parts, "delivery").unwrap_or("safe_boundary");
    let dedupe_key = extract_flag(parts, "dedupe").map(|s| s.to_string());
    let expires_secs: Option<u64> = extract_flag(parts, "expires").and_then(|s| s.parse().ok());

    let conn = open_or_exit();

    // Deduplication check
    if let Some(ref key) = dedupe_key {
        if let Ok(Some(existing)) = store::find_duplicate_interrupt(&conn, key) {
            let msg = format!(
                "Duplicate interrupt exists: {} (dedupe_key: {key})",
                existing.id
            );
            if json_mode {
                println!(
                    "{}",
                    serde_json::json!({"error": msg, "existing_id": existing.id})
                );
            } else {
                eprintln!("{msg}");
            }
            return Err(io::Error::other("duplicate"));
        }
    }

    let intr_id = store::gen_id("intr");
    let now = crate::logger::timestamp_now();

    let expires_at = expires_secs.map(|secs| {
        let epoch = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
            + secs;
        // Approximate ISO timestamp from epoch (good enough for comparison)
        format_epoch_iso(epoch)
    });

    let interrupt = Interrupt {
        id: intr_id.clone(),
        interrupt_type: itype,
        priority: priority.to_string(),
        target_session_id: target_session.to_string(),
        reason: reason_text,
        payload: None,
        delivery_mode: delivery.to_string(),
        max_retries: 3,
        expires_at,
        dedupe_key,
        state: InterruptState::Pending,
        created_at: now.clone(),
        delivered_at: None,
        acknowledged_at: None,
    };

    store::insert_interrupt(&conn, &interrupt).map_err(io::Error::other)?;

    let event = CoordEvent {
        id: None,
        event_type: EventType::InterruptRaised,
        timestamp: now,
        session_id: Some(target_session.to_string()),
        payload: serde_json::json!({
            "interrupt_id": intr_id,
            "type": itype.as_str(),
            "priority": priority,
        }),
    };
    let _ = store::append_event(&conn, &event);

    if json_mode {
        let json = serde_json::to_string_pretty(&interrupt).unwrap_or_default();
        println!("{json}");
    } else {
        println!("Interrupt raised: {intr_id}");
        println!("  Type:     {itype}");
        println!("  Target:   {target_session}");
        println!("  Priority: {priority}");
        println!("  Delivery: {delivery}");
    }
    Ok(())
}

// -- Acknowledge Interrupt -----------------------------------------------------

fn cmd_ack(parts: &[&str], json_mode: bool) -> io::Result<()> {
    let Some(intr_id) = parts.get(1) else {
        eprintln!("Usage: claudectl --coord \"ack <interrupt_id>\"");
        return Err(io::Error::other("missing interrupt_id"));
    };

    let conn = open_or_exit();

    let Some(interrupt) = store::get_interrupt(&conn, intr_id).map_err(io::Error::other)? else {
        let msg = format!("Interrupt not found: {intr_id}");
        if json_mode {
            println!("{}", serde_json::json!({"error": msg}));
        } else {
            eprintln!("{msg}");
        }
        return Err(io::Error::other("not found"));
    };

    if interrupt.state != InterruptState::Delivered {
        let msg = format!(
            "Interrupt {intr_id} is in '{}' state (must be 'delivered' to acknowledge)",
            interrupt.state
        );
        if json_mode {
            println!("{}", serde_json::json!({"error": msg}));
        } else {
            eprintln!("{msg}");
        }
        return Err(io::Error::other("wrong state"));
    }

    store::mark_interrupt_acknowledged(&conn, intr_id).map_err(io::Error::other)?;

    let now = crate::logger::timestamp_now();
    let event = CoordEvent {
        id: None,
        event_type: EventType::InterruptAcknowledged,
        timestamp: now,
        session_id: Some(interrupt.target_session_id.clone()),
        payload: serde_json::json!({
            "interrupt_id": intr_id,
            "type": interrupt.interrupt_type.as_str(),
        }),
    };
    let _ = store::append_event(&conn, &event);

    if json_mode {
        println!("{}", serde_json::json!({"acknowledged": intr_id}));
    } else {
        println!("Interrupt acknowledged: {intr_id}");
    }
    Ok(())
}

/// Format an epoch timestamp as ISO 8601 (simplified, UTC).
fn format_epoch_iso(epoch_secs: u64) -> String {
    let days = epoch_secs / 86400;
    let secs_in_day = epoch_secs % 86400;
    let h = secs_in_day / 3600;
    let m = (secs_in_day % 3600) / 60;
    let s = secs_in_day % 60;

    // Simplified date calculation (same as logger.rs)
    let (year, month, day) = crate::logger::days_to_date(days);
    format!("{year:04}-{month:02}-{day:02}T{h:02}:{m:02}:{s:02}Z")
}

// -- Promote Memory ------------------------------------------------------------

fn cmd_promote(parts: &[&str], json_mode: bool) -> io::Result<()> {
    let project = extract_flag(parts, "project");

    let Some(project_name) = project else {
        eprintln!("Usage: claudectl --coord \"promote --project <name>\"");
        return Err(io::Error::other("missing --project"));
    };

    match super::promotion::promote_project(project_name) {
        Ok(count) => {
            if json_mode {
                println!(
                    "{}",
                    serde_json::json!({"promoted": count, "project": project_name})
                );
            } else {
                println!(
                    "Promoted {count} pattern(s) from project '{project_name}' to coordination memory."
                );
            }
            Ok(())
        }
        Err(e) => {
            if json_mode {
                println!("{}", serde_json::json!({"error": e}));
            } else {
                eprintln!("{e}");
            }
            Err(io::Error::other(e))
        }
    }
}

// -- Show Context --------------------------------------------------------------

fn cmd_context(parts: &[&str], json_mode: bool) -> io::Result<()> {
    let session_id = extract_flag(parts, "session");

    let Some(session_id) = session_id else {
        eprintln!("Usage: claudectl --coord \"context --session <id>\"");
        return Err(io::Error::other("missing --session"));
    };

    // Build a minimal session for the injection engine
    let session = crate::session::ClaudeSession::from_raw(crate::session::RawSession {
        pid: 0,
        session_id: session_id.to_string(),
        cwd: std::env::current_dir()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| ".".into()),
        started_at: 0,
    });

    let ctx = super::injection::build_coordination_context(&session);

    if json_mode {
        println!(
            "{}",
            serde_json::json!({"session_id": session_id, "context": ctx})
        );
    } else if ctx.is_empty() {
        println!("No coordination context for session '{session_id}'.");
    } else {
        println!("Coordination context for session '{session_id}':");
        println!();
        println!("{ctx}");
    }
    Ok(())
}

// -- Adapters ------------------------------------------------------------------

fn cmd_adapters(parts: &[&str], json_mode: bool) -> io::Result<()> {
    use super::adapter;

    let filter = parts.get(1).copied();

    if json_mode {
        let adapters: Vec<serde_json::Value> = adapter::all_adapters()
            .iter()
            .filter(|a| filter.is_none() || filter == Some(a.family().as_str()))
            .map(|a| {
                serde_json::json!({
                    "family": a.family().as_str(),
                    "capabilities": a.capabilities(),
                    "sessions": a.discover_sessions().len(),
                })
            })
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&adapters).unwrap_or_default()
        );
        return Ok(());
    }

    let adapters = adapter::all_adapters();
    let filtered: Vec<_> = adapters
        .iter()
        .filter(|a| filter.is_none() || filter == Some(a.family().as_str()))
        .collect();

    if filtered.is_empty() {
        if let Some(name) = filter {
            eprintln!("Unknown adapter: '{name}'");
            eprintln!("Available: claude-code, codex");
        }
        return Ok(());
    }

    for a in &filtered {
        let caps = a.capabilities();
        let sessions = a.discover_sessions();
        println!("Adapter: {}", a.family());
        println!("  Sessions discovered: {}", sessions.len());
        println!("  Capabilities ({}/9):", caps.count());
        println!("    discover_sessions:  {}", yn(caps.discover_sessions));
        println!("    monitor_state:      {}", yn(caps.monitor_state));
        println!("    send_input:         {}", yn(caps.send_input));
        println!("    deliver_interrupt:  {}", yn(caps.deliver_interrupt));
        println!("    request_checkpoint: {}", yn(caps.request_checkpoint));
        println!("    request_compaction: {}", yn(caps.request_compaction));
        println!("    pause:              {}", yn(caps.pause));
        println!("    resume:             {}", yn(caps.resume));
        println!("    terminate:          {}", yn(caps.terminate));

        if !sessions.is_empty() {
            println!("  Active sessions:");
            for s in sessions.iter().take(10) {
                let pid_str = s.pid.map(|p| p.to_string()).unwrap_or_else(|| "-".into());
                println!("    {} (pid {}, {})", s.session_id, pid_str, s.cwd);
            }
        }
        println!();
    }
    Ok(())
}

fn yn(b: bool) -> &'static str {
    if b { "yes" } else { "no" }
}

// -- Metrics -------------------------------------------------------------------

fn cmd_metrics(parts: &[&str], json_mode: bool) -> io::Result<()> {
    let conn = open_or_exit();

    let since = extract_flag(parts, "since");

    let m = super::metrics::compute(&conn, since);

    if json_mode {
        let json = serde_json::to_string_pretty(&m).unwrap_or_default();
        println!("{json}");
    } else {
        print!("{}", super::metrics::format_metrics(&m));
    }
    Ok(())
}

// -- Eval ----------------------------------------------------------------------

fn cmd_eval(json_mode: bool) -> io::Result<()> {
    let results = super::evals::run_evals();

    if json_mode {
        let json = serde_json::to_string_pretty(&results).unwrap_or_default();
        println!("{json}");
    } else {
        print!("{}", super::evals::format_results(&results));
    }

    let all_passed = results.iter().all(|r| r.passed);
    if !all_passed {
        return Err(io::Error::other("some evals failed"));
    }
    Ok(())
}

// -- Prune ---------------------------------------------------------------------

fn cmd_prune(parts: &[&str], json_mode: bool) -> io::Result<()> {
    let days: Option<u64> = extract_flag(parts, "days").and_then(|s| s.parse().ok());
    let conn = open_or_exit();

    match store::prune(&conn, days) {
        Ok(count) => {
            if json_mode {
                println!(
                    "{}",
                    serde_json::json!({"pruned": count, "retention_days": days.unwrap_or(30)})
                );
            } else {
                let d = days.unwrap_or(30);
                println!("Pruned {count} rows (retention: {d} days).");
            }
            Ok(())
        }
        Err(e) => {
            eprintln!("Prune failed: {e}");
            Err(io::Error::other(e))
        }
    }
}

// -- Help ----------------------------------------------------------------------

fn print_help() -> io::Result<()> {
    println!("Coordination Layer");
    println!();
    println!("Usage: claudectl --coord <subcommand>");
    println!();
    println!("Read commands:");
    println!("  events [N] [type]   Show last N events (default 50), optionally filtered by type");
    println!("  leases              Show active ownership leases");
    println!("  blockers            Show open blockers");
    println!("  handoffs            Show handoffs");
    println!("  interrupts          Show pending interrupts");
    println!("  memory              List recent memory records");
    println!("  memory search <q>   Search memory records (full-text)");
    println!("  context --session <id>  Show coordination context that would be injected");
    println!();
    println!("Write commands:");
    println!(
        "  claim --session <id> --path <resource> [--mode exclusive|advisory] [--reason text]"
    );
    println!("  release <lease_id>");
    println!(
        "  handoff --from <session> --task <id> --summary <text> [--to <session>] [--priority high|medium|low]"
    );
    println!(
        "  raise --type <type> --target <session> --reason <text> [--priority high] [--delivery safe_boundary] [--dedupe key] [--expires secs]"
    );
    println!("  accept <handoff_id>");
    println!("  block --task <id> --session <id> --waiting-for <text> [--depends-on <task_id>]");
    println!("  unblock <blocker_id>");
    println!("  ack <interrupt_id>");
    println!("  promote --project <name>  Promote brain patterns to coordination memory");
    println!();
    println!("Adapters:");
    println!("  adapters [family]   List registered agent adapters and their capabilities");
    println!();
    println!("Evaluation:");
    println!("  metrics [--since ts]  Show coordination metrics from event log");
    println!("  eval                  Run coordination eval scenarios");
    println!();
    println!("Maintenance:");
    println!(
        "  prune [--days N]      Delete old events, resolved blockers, expired leases (default: 30 days)"
    );
    println!();
    println!("  help                Show this help");
    println!();
    println!("Add --json for machine-readable output.");
    Ok(())
}

// -- Helpers -------------------------------------------------------------------

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}...", &s[..max.saturating_sub(3)])
    }
}

/// Extract a single-value flag: `--flag value` -> `Some("value")`.
fn extract_flag<'a>(parts: &[&'a str], flag: &str) -> Option<&'a str> {
    let target = format!("--{flag}");
    parts
        .iter()
        .position(|p| *p == target)
        .and_then(|i| parts.get(i + 1).copied())
}

/// Extract a multi-word flag value: collects tokens after `--flag` until the next `--*` flag.
fn extract_flag_rest(parts: &[&str], flag: &str) -> Option<String> {
    let target = format!("--{flag}");
    let start = parts.iter().position(|p| *p == target)?;
    let mut words = Vec::new();
    for p in &parts[start + 1..] {
        if p.starts_with("--") {
            break;
        }
        words.push(*p);
    }
    if words.is_empty() {
        None
    } else {
        Some(words.join(" "))
    }
}
