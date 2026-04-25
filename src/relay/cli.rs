// CLI dispatch for relay subcommands.

use std::io;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use super::crypto;
use super::delegation::{self, DelegationContext};
use super::listener::RelayListener;
use super::mesh::PeerRegistry;
use super::peer::PeerConnection;
use super::{
    PENDING_PEER_ID, clear_pending_psk, forget_peer, gen_msg_id, is_valid_peer_id,
    list_known_peers, load_or_create_identity, load_peer_meta, load_peer_psk, load_pending_psk,
    save_peer_psk, save_pending_psk,
};

/// Dispatch a relay subcommand.
pub fn dispatch(subcommand: &str, json_mode: bool) -> io::Result<()> {
    let parts: Vec<&str> = subcommand.split_whitespace().collect();
    match parts.first().copied() {
        Some("serve") => cmd_serve(&parts[1..]),
        Some("pair") => cmd_pair(json_mode),
        Some("accept") => cmd_accept(&parts[1..]),
        Some("connect") => cmd_connect(&parts[1..]),
        Some("peers") => cmd_peers(json_mode),
        Some("disconnect") => cmd_disconnect(&parts[1..]),
        Some("forget") => cmd_forget(&parts[1..]),
        Some("identity") => cmd_identity(json_mode),
        Some("delegate") => cmd_delegate(&parts[1..], json_mode),
        Some("status") => cmd_task_status(json_mode),
        Some("interrupt") => cmd_interrupt(&parts[1..]),
        Some("invite") => cmd_invite(&parts[1..], json_mode),
        Some("join") => cmd_join(&parts[1..]),
        Some("discover") => cmd_discover(json_mode),
        Some(other) => {
            eprintln!("Unknown relay subcommand: {other}");
            print_relay_help();
            Err(io::Error::other("unknown subcommand"))
        }
        None => {
            print_relay_help();
            Ok(())
        }
    }
}

fn print_relay_help() {
    eprintln!("Usage: claudectl --relay <subcommand>");
    eprintln!();
    eprintln!("Connection:");
    eprintln!("  serve [--port N]             Start relay listener");
    eprintln!("  invite [--qr] [--words]      Generate invite code/link/phrase");
    eprintln!("  join <code|link|phrase>       Connect using any invite format");
    eprintln!("  discover                     Scan LAN for nearby instances");
    eprintln!("  connect <host:port>          Connect to a known peer");
    eprintln!();
    eprintln!("Pairing:");
    eprintln!("  pair                         Generate raw PSK code");
    eprintln!("  accept <code> <peer-id>      Accept a PSK from a peer");
    eprintln!("  peers [--json]               List known/connected peers");
    eprintln!("  forget <peer-id>             Remove a peer");
    eprintln!("  identity                     Show this instance's relay identity");
    eprintln!();
    eprintln!("Delegation:");
    eprintln!("  delegate <peer> <prompt>     Delegate a task to a remote peer");
    eprintln!("  status                       Show remote task status");
    eprintln!("  interrupt <task> <type>       Interrupt a remote task (nudge/stop)");
}

/// `claudectl relay serve [--port PORT]`
/// Start the relay listener in the foreground.
fn cmd_serve(args: &[&str]) -> io::Result<()> {
    let mut port: u16 = 9847;
    let mut i = 0;
    while i < args.len() {
        if args[i] == "--port" {
            i += 1;
            if let Some(p) = args.get(i) {
                port = p
                    .parse()
                    .map_err(|_| io::Error::other("invalid port number"))?;
            }
        }
        i += 1;
    }

    // Load config for relay/hive settings
    let cfg = crate::config::Config::load();
    let relay_cfg = cfg.relay.unwrap_or_default();
    #[cfg(feature = "hive")]
    let hive_cfg = cfg.hive.unwrap_or_default();

    let identity = load_or_create_identity();
    // CLI --port overrides config; config overrides default
    if port == 9847 {
        port = relay_cfg.listen_port;
    }
    let listen_addr = format!("{}:{port}", relay_cfg.listen_addr);
    let addr: SocketAddr = listen_addr
        .parse()
        .map_err(|e| io::Error::other(format!("invalid addr '{listen_addr}': {e}")))?;

    let registry = Arc::new(Mutex::new(PeerRegistry::new(
        relay_cfg.heartbeat_interval_secs,
    )));
    let listener = RelayListener::start(
        addr,
        Arc::clone(&registry),
        identity.clone(),
        relay_cfg.max_peers,
    )?;

    println!("Relay listening on {} as {}", listener.addr, identity);
    println!("Press Ctrl+C to stop.");

    // Initialize worker for task delegation
    let mut worker = super::worker::RemoteWorker::new(identity.as_str());

    // Initialize hive gossip engine (only when hive feature is enabled)
    #[cfg(feature = "hive")]
    let (mut hive_store, mut gossip, broadcast_rx) = {
        let hive_enabled = hive_cfg.enabled;
        let store = hive_enabled.then(crate::hive::store::HiveStore::load);
        let gossip_engine = hive_enabled.then(|| {
            let mut engine = crate::hive::gossip::GossipEngine::new(
                identity.as_str(),
                hive_cfg.max_propagation,
                hive_cfg.knowledge_ttl_days,
            );
            engine.set_sharing_filter(crate::hive::SharingFilter::from_config(&hive_cfg));
            engine
        });
        let rx = if hive_enabled {
            let (tx, rx) = std::sync::mpsc::channel::<u32>();
            crate::hive::set_broadcast_channel(tx);
            Some(rx)
        } else {
            None
        };
        (store, gossip_engine, rx)
    };

    // Block on Ctrl+C
    let running = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let r = Arc::clone(&running);
    let _ = ctrlc::set_handler(move || {
        r.store(false, std::sync::atomic::Ordering::Relaxed);
    });

    while running.load(std::sync::atomic::Ordering::Relaxed) {
        std::thread::sleep(std::time::Duration::from_secs(1));

        // Process incoming messages and tick
        if let Ok(mut reg) = registry.lock() {
            let messages = reg.drain_messages();
            for (from_peer, msg) in messages {
                match msg.msg_type {
                    super::MessageType::Heartbeat => {
                        reg.handle_heartbeat(&from_peer);
                    }
                    super::MessageType::DelegateTask => {
                        match super::delegation::parse_delegate_message(&msg) {
                            Ok((task_id, prompt, cwd, context)) => {
                                println!(
                                    "[{}] DelegateTask '{}' from {}",
                                    crate::logger::timestamp_now(),
                                    task_id,
                                    from_peer
                                );
                                match worker.accept_task(
                                    &task_id,
                                    &prompt,
                                    cwd.as_deref(),
                                    context,
                                    from_peer.as_str(),
                                ) {
                                    Ok(status_msg) => {
                                        let _ = reg.send_to(from_peer.as_str(), &status_msg);
                                    }
                                    Err(e) => {
                                        eprintln!("  Failed to accept task: {e}");
                                    }
                                }
                            }
                            Err(e) => eprintln!("  Bad DelegateTask message: {e}"),
                        }
                    }
                    super::MessageType::TaskInterrupt => {
                        match super::delegation::parse_interrupt_message(&msg) {
                            Ok((task_id, itype, reason)) => {
                                println!(
                                    "[{}] TaskInterrupt '{}' ({}) from {}",
                                    crate::logger::timestamp_now(),
                                    task_id,
                                    itype,
                                    from_peer
                                );
                                if let Some(resp) =
                                    worker.handle_interrupt(&task_id, &itype, &reason)
                                {
                                    let _ = reg.send_to(from_peer.as_str(), &resp);
                                }
                            }
                            Err(e) => eprintln!("  Bad TaskInterrupt message: {e}"),
                        }
                    }
                    super::MessageType::TaskStatus | super::MessageType::TaskHandoff => {
                        println!(
                            "[{}] {:?} from {}",
                            crate::logger::timestamp_now(),
                            msg.msg_type,
                            from_peer
                        );
                    }
                    #[cfg(feature = "hive")]
                    super::MessageType::KnowledgeSync => {
                        if let (Some(gossip), Some(hive_store)) =
                            (gossip.as_mut(), hive_store.as_mut())
                        {
                            let (stats, accepted) = gossip.handle_sync(hive_store, &msg);
                            println!(
                                "[{}] KnowledgeSync from {}: {} accepted, {} rejected",
                                crate::logger::timestamp_now(),
                                from_peer,
                                stats.accepted,
                                stats.rejected
                            );
                            if !accepted.is_empty() {
                                let connected = reg.connected_peers();
                                let prop_msgs = gossip.propagate(&accepted, &from_peer, &connected);
                                for (target, prop_msg) in prop_msgs {
                                    let _ = reg.send_to(target.as_str(), &prop_msg);
                                }
                            }
                        }
                    }
                    #[cfg(feature = "hive")]
                    super::MessageType::KnowledgeRequest => {
                        if let (Some(gossip), Some(hive_store)) =
                            (gossip.as_ref(), hive_store.as_ref())
                        {
                            let snapshots = gossip.handle_request(hive_store, &msg);
                            for snap in snapshots {
                                let _ = reg.send_to(from_peer.as_str(), &snap);
                            }
                        }
                    }
                    #[cfg(feature = "hive")]
                    super::MessageType::KnowledgeSnapshot => {
                        if let (Some(gossip), Some(hive_store)) =
                            (gossip.as_mut(), hive_store.as_mut())
                        {
                            let stats = gossip.handle_snapshot(hive_store, &msg);
                            println!(
                                "[{}] KnowledgeSnapshot from {}: {} accepted",
                                crate::logger::timestamp_now(),
                                from_peer,
                                stats.accepted
                            );
                        }
                    }
                    _ => {
                        println!(
                            "[{}] {:?} from {}",
                            crate::logger::timestamp_now(),
                            msg.msg_type,
                            from_peer
                        );
                    }
                }
            }

            // Tick worker — send status updates back to controllers
            let worker_msgs = worker.tick();
            for (target_peer, msg) in worker_msgs {
                let _ = reg.send_to(&target_peer, &msg);
            }

            // Check if brain distillation produced new knowledge to gossip
            #[cfg(feature = "hive")]
            if let (Some(broadcast_rx), Some(gossip), Some(hive_store)) =
                (broadcast_rx.as_ref(), gossip.as_mut(), hive_store.as_ref())
            {
                while broadcast_rx.try_recv().is_ok() {
                    let connected = reg.connected_peers();
                    let sync_msgs = gossip.generate_sync_messages(hive_store, &connected);
                    for (target, sync_msg) in sync_msgs {
                        let _ = reg.send_to(target.as_str(), &sync_msg);
                    }
                }
            }

            let events = reg.tick(identity.as_str());
            for event in events {
                match event {
                    super::mesh::MeshEvent::PeerDisconnected(id) => {
                        println!("Peer {} disconnected", id);
                    }
                    super::mesh::MeshEvent::ReconnectScheduled(id, delay) => {
                        println!("Reconnect to {} in {:?}", id, delay);
                    }
                    super::mesh::MeshEvent::ReconnectNeeded(id, addr) => {
                        println!("Reconnecting to {} ...", id);
                        match reconnect_peer(&mut reg, &id, addr, &identity) {
                            Ok(()) => println!("Reconnected to {}", id),
                            Err(e) => println!("Reconnect to {} failed: {}", id, e),
                        }
                    }
                }
            }
        }
    }

    listener.stop();
    println!("\nRelay stopped.");
    Ok(())
}

/// `claudectl relay pair`
/// Generate a new PSK and display it.
fn cmd_pair(json_mode: bool) -> io::Result<()> {
    let identity = load_or_create_identity();
    let psk = crypto::generate_psk();
    let code = crypto::format_psk(&psk);

    if json_mode {
        let json = serde_json::json!({
            "identity": identity.as_str(),
            "pair_code": code,
        });
        println!("{}", serde_json::to_string_pretty(&json).unwrap());
    } else {
        println!("Your identity: {}", identity);
        println!();
        println!("PAIR CODE: {}", code);
        println!();
        println!("Share this code with the peer you want to connect.");
        println!(
            "They should run: claudectl --relay \"accept {} {}\"",
            code, identity
        );
    }

    // Store the canonical (code-derived) PSK locally — both sides must derive the
    // same key from the code. The raw `psk` has 32 random bytes but format_psk only
    // encodes 8 bytes; parse_psk derives the remaining 24 deterministically. We must
    // store the canonical form so both sides match during HMAC verification.
    let canonical_psk = crypto::parse_psk(&code).expect("just-generated code must parse");
    save_pending_psk(&canonical_psk).map_err(io::Error::other)?;

    Ok(())
}

/// `claudectl relay accept <code> <peer_id>`
/// Accept a pairing code from another peer.
fn cmd_accept(args: &[&str]) -> io::Result<()> {
    if args.len() < 2 {
        eprintln!("Usage: claudectl --relay \"accept <pair-code> <peer-id>\"");
        return Err(io::Error::other("missing arguments"));
    }

    let code = args[0];
    let peer_id = args[1];
    if !is_valid_peer_id(peer_id) || peer_id == PENDING_PEER_ID {
        return Err(io::Error::other(format!("invalid peer id: {peer_id}")));
    }

    let psk =
        crypto::parse_psk(code).map_err(|e| io::Error::other(format!("invalid code: {e}")))?;

    save_peer_psk(peer_id, &psk).map_err(io::Error::other)?;

    clear_pending_psk();

    println!("Paired with peer: {}", peer_id);
    println!("PSK stored. You can now connect with:");
    println!("  claudectl --relay \"connect <host>:<port>\"");

    Ok(())
}

/// Shared event loop for a connected peer. Blocks until Ctrl+C.
fn run_connect_loop(registry: &Arc<Mutex<PeerRegistry>>, identity: &str) {
    let running = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let r = Arc::clone(&running);
    let _ = ctrlc::set_handler(move || {
        r.store(false, std::sync::atomic::Ordering::Relaxed);
    });
    let identity = super::PeerId(identity.to_string());

    while running.load(std::sync::atomic::Ordering::Relaxed) {
        std::thread::sleep(std::time::Duration::from_secs(1));
        if let Ok(mut reg) = registry.lock() {
            let messages = reg.drain_messages();
            for (peer_id, msg) in &messages {
                match msg.msg_type {
                    super::MessageType::Heartbeat => {
                        reg.handle_heartbeat(peer_id);
                    }
                    _ => {
                        println!(
                            "[{}] {:?} from {}",
                            crate::logger::timestamp_now(),
                            msg.msg_type,
                            peer_id
                        );
                    }
                }
            }
            let events = reg.tick(identity.as_str());
            for event in events {
                match event {
                    super::mesh::MeshEvent::PeerDisconnected(id) => {
                        println!("Peer {} disconnected", id);
                    }
                    super::mesh::MeshEvent::ReconnectScheduled(id, delay) => {
                        println!("Reconnect to {} in {:?}", id, delay);
                    }
                    super::mesh::MeshEvent::ReconnectNeeded(id, addr) => {
                        println!("Reconnecting to {} ...", id);
                        match reconnect_peer(&mut reg, &id, addr, &identity) {
                            Ok(()) => println!("Reconnected to {}", id),
                            Err(e) => println!("Reconnect to {} failed: {}", id, e),
                        }
                    }
                }
            }
        }
    }
    println!("\nDisconnected.");
}

/// Try to connect using a specific PSK. Returns Ok(registry) on success.
fn try_connect(
    addr: SocketAddr,
    psk: &[u8; 32],
    identity: &super::PeerId,
) -> Result<(String, Arc<Mutex<PeerRegistry>>), String> {
    let registry = Arc::new(Mutex::new(PeerRegistry::new(30)));
    let tx = {
        let reg = registry.lock().unwrap();
        reg.message_tx()
    };

    let conn = PeerConnection::connect(addr, psk, identity, tx)?;
    let remote_id = conn.peer_id.0.clone();
    if let Ok(mut reg) = registry.lock() {
        reg.add_peer(conn);
    }
    Ok((remote_id, registry))
}

/// Reconnect an existing peer in a registry using its stored PSK.
fn reconnect_peer(
    reg: &mut PeerRegistry,
    peer_id: &super::PeerId,
    addr: Option<SocketAddr>,
    identity: &super::PeerId,
) -> Result<(), String> {
    let addr = addr.ok_or("missing reconnect address")?;
    let psk = load_peer_psk(peer_id.as_str()).ok_or("missing peer PSK")?;
    let tx = reg.message_tx();
    let conn = PeerConnection::connect(addr, &psk, identity, tx)?;
    if conn.peer_id != *peer_id {
        return Err(format!(
            "remote identity mismatch: expected {}, got {}",
            peer_id, conn.peer_id
        ));
    }
    reg.add_peer(conn);
    Ok(())
}

/// `claudectl relay connect <host:port>`
/// Connect to a remote relay.
fn cmd_connect(args: &[&str]) -> io::Result<()> {
    if args.is_empty() {
        eprintln!("Usage: claudectl --relay \"connect <host:port>\"");
        return Err(io::Error::other("missing address"));
    }

    let addr: SocketAddr = args[0]
        .parse()
        .map_err(|e| io::Error::other(format!("invalid address '{}': {e}", args[0])))?;

    let identity = load_or_create_identity();

    // Try all known peer PSKs
    for peer_id in &list_known_peers() {
        if let Some(psk) = load_peer_psk(peer_id) {
            if let Ok((remote_id, registry)) = try_connect(addr, &psk, &identity) {
                if remote_id == *peer_id && is_valid_peer_id(&remote_id) {
                    println!("Connected to {} ({})", remote_id, addr);
                    let _ = super::save_peer_meta(&remote_id, &addr.to_string());
                    run_connect_loop(&registry, identity.as_str());
                    return Ok(());
                }
            }
        }
    }

    // Try the pending pair key
    if let Some(psk) = load_pending_psk() {
        if let Ok((remote_id, registry)) = try_connect(addr, &psk, &identity) {
            if is_valid_peer_id(&remote_id) && remote_id != PENDING_PEER_ID {
                println!("Connected to {} ({})", remote_id, addr);
                let _ = save_peer_psk(&remote_id, &psk);
                let _ = super::save_peer_meta(&remote_id, &addr.to_string());
                clear_pending_psk();
                run_connect_loop(&registry, identity.as_str());
                return Ok(());
            }
        }
    }

    eprintln!("Could not connect to {}", args[0]);
    eprintln!("Make sure you have paired with this peer first:");
    eprintln!("  1. Remote runs: claudectl --relay pair");
    eprintln!("  2. You run:     claudectl --relay \"accept <code> <peer-id>\"");
    Err(io::Error::other("connection failed"))
}

/// `claudectl relay peers [--json]`
/// List known peers and their status.
fn cmd_peers(json_mode: bool) -> io::Result<()> {
    let identity = load_or_create_identity();
    let known = list_known_peers();

    if json_mode {
        let peers: Vec<serde_json::Value> = known
            .iter()
            .map(|id| {
                let meta = load_peer_meta(id).unwrap_or(serde_json::json!({}));
                serde_json::json!({
                    "peer_id": id,
                    "addr": meta.get("addr").and_then(|v| v.as_str()).unwrap_or("unknown"),
                    "last_seen": meta.get("last_seen").and_then(|v| v.as_u64()).unwrap_or(0),
                    "has_psk": load_peer_psk(id).is_some(),
                })
            })
            .collect();
        let output = serde_json::json!({
            "identity": identity.as_str(),
            "peers": peers,
        });
        println!("{}", serde_json::to_string_pretty(&output).unwrap());
    } else {
        println!("Identity: {}", identity);
        println!();
        if known.is_empty() {
            println!("No paired peers. Run 'claudectl --relay pair' to get started.");
        } else {
            println!("{:<20} {:<24} PAIRED", "PEER", "ADDRESS");
            println!("{}", "─".repeat(56));
            for id in &known {
                let meta = load_peer_meta(id).unwrap_or(serde_json::json!({}));
                let addr = meta
                    .get("addr")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown");
                let has_psk = if load_peer_psk(id).is_some() {
                    "yes"
                } else {
                    "no"
                };
                println!("{:<20} {:<24} {}", id, addr, has_psk);
            }
        }
    }
    Ok(())
}

/// `claudectl relay disconnect <peer_id>`
fn cmd_disconnect(args: &[&str]) -> io::Result<()> {
    if args.is_empty() {
        eprintln!("Usage: claudectl --relay \"disconnect <peer-id>\"");
        return Err(io::Error::other("missing peer id"));
    }
    // In standalone CLI mode, we can't disconnect a live connection
    // (that's handled by the TUI/serve loop). Just inform the user.
    println!("Note: to disconnect a live connection, stop the relay serve/connect process.");
    println!(
        "To remove the pairing entirely, use: claudectl --relay \"forget {}\"",
        args[0]
    );
    Ok(())
}

/// `claudectl relay forget <peer_id>`
/// Remove all data for a peer.
fn cmd_forget(args: &[&str]) -> io::Result<()> {
    if args.is_empty() {
        eprintln!("Usage: claudectl --relay \"forget <peer-id>\"");
        return Err(io::Error::other("missing peer id"));
    }
    let peer_id = args[0];
    if load_peer_psk(peer_id).is_none() {
        eprintln!("Unknown peer: {}", peer_id);
        return Err(io::Error::other("unknown peer"));
    }
    forget_peer(peer_id);
    println!("Forgot peer: {}", peer_id);
    Ok(())
}

/// `claudectl relay identity`
/// Show this instance's relay identity.
fn cmd_identity(json_mode: bool) -> io::Result<()> {
    let identity = load_or_create_identity();
    if json_mode {
        println!("{}", serde_json::json!({ "identity": identity.as_str() }));
    } else {
        println!("{}", identity);
    }
    Ok(())
}

// ────────────────────────────────────────────────────────────────────────────
// Phase 2: Delegation commands
// ────────────────────────────────────────────────────────────────────────────

/// `claudectl relay delegate <peer_id> "<prompt>" [--cwd /path] [--git-ref branch]`
fn cmd_delegate(args: &[&str], json_mode: bool) -> io::Result<()> {
    if args.len() < 2 {
        eprintln!(
            "Usage: claudectl --relay \"delegate <peer-id> <prompt> [--cwd /path] [--git-ref branch]\""
        );
        return Err(io::Error::other("missing arguments"));
    }

    let peer_id = args[0];
    let mut prompt_parts: Vec<&str> = Vec::new();
    let mut cwd: Option<&str> = None;
    let mut git_ref: Option<String> = None;

    let mut i = 1;
    while i < args.len() {
        if args[i] == "--cwd" {
            i += 1;
            cwd = args.get(i).copied();
        } else if args[i] == "--git-ref" {
            i += 1;
            git_ref = args.get(i).map(|s| s.to_string());
        } else {
            prompt_parts.push(args[i]);
        }
        i += 1;
    }

    let prompt = prompt_parts
        .join(" ")
        .trim_matches('"')
        .trim_matches('\'')
        .to_string();
    if prompt.is_empty() {
        eprintln!("Usage: claudectl --relay \"delegate <peer-id> <prompt> [--cwd /path]\"");
        return Err(io::Error::other("missing prompt"));
    }

    let identity = load_or_create_identity();
    let task_id = gen_msg_id().replace("msg_", "task_");

    let context = DelegationContext {
        git_ref,
        ..Default::default()
    };

    let msg =
        delegation::build_delegate_message(&task_id, &prompt, cwd, &context, identity.as_str())
            .map_err(|e| io::Error::other(format!("build message: {e}")))?;

    if json_mode {
        let output = serde_json::json!({
            "task_id": task_id,
            "peer": peer_id,
            "prompt": prompt,
            "cwd": cwd,
            "status": "delegated",
            "message": msg,
        });
        println!("{}", serde_json::to_string_pretty(&output).unwrap());
    } else {
        println!("Task {} delegated to peer {}", task_id, peer_id);
        println!("  Prompt: {}", prompt);
        if let Some(c) = cwd {
            println!("  CWD: {}", c);
        }
        println!();
        println!("Note: In standalone CLI mode, the message is built but not sent.");
        println!("Use `claudectl relay serve` or TUI mode for live delegation.");
    }

    Ok(())
}

/// `claudectl relay status [--json]`
/// Show status of delegated tasks.
fn cmd_task_status(json_mode: bool) -> io::Result<()> {
    // In standalone CLI mode, we don't have a live relay connection.
    // Show info about the delegation subsystem.
    let identity = load_or_create_identity();

    if json_mode {
        let output = serde_json::json!({
            "identity": identity.as_str(),
            "active_delegated_tasks": 0,
            "note": "Live task status requires relay serve or TUI mode",
        });
        println!("{}", serde_json::to_string_pretty(&output).unwrap());
    } else {
        println!("Relay identity: {}", identity);
        println!();
        println!("No active delegated tasks.");
        println!("Live task status requires `claudectl relay serve` or TUI mode.");
    }

    Ok(())
}

/// `claudectl relay interrupt <task_id> <type> [reason]`
fn cmd_interrupt(args: &[&str]) -> io::Result<()> {
    if args.len() < 2 {
        eprintln!("Usage: claudectl --relay \"interrupt <task-id> <type> [reason]\"");
        eprintln!("Types: nudge, stop, reroute");
        return Err(io::Error::other("missing arguments"));
    }

    let task_id = args[0];
    let interrupt_type = args[1];
    let reason = if args.len() > 2 {
        args[2..].join(" ")
    } else {
        String::new()
    };

    let identity = load_or_create_identity();
    let msg =
        delegation::build_interrupt_message(task_id, interrupt_type, &reason, identity.as_str());

    println!("Interrupt built for task {}", task_id);
    println!("  Type: {}", interrupt_type);
    if !reason.is_empty() {
        println!("  Reason: {}", reason);
    }
    println!("  Message ID: {}", msg.id);
    println!();
    println!("Note: In standalone CLI mode, the message is built but not sent.");
    println!("Use `claudectl relay serve` or TUI mode for live interrupts.");

    Ok(())
}

// ────────────────────────────────────────────────────────────────────────────
// Discovery commands: invite, join, discover
// ────────────────────────────────────────────────────────────────────────────

/// `claudectl relay invite [--qr] [--words] [--json]`
fn cmd_invite(args: &[&str], json_mode: bool) -> io::Result<()> {
    let show_qr = args.contains(&"--qr");
    let show_words = args.contains(&"--words");

    let identity = load_or_create_identity();
    let cfg = crate::config::Config::load();
    let relay_cfg = cfg.relay.unwrap_or_default();

    // Detect our LAN IP
    let ip = detect_local_ip().unwrap_or_else(|| "127.0.0.1".to_string());
    let port = relay_cfg.listen_port;
    let addr: std::net::SocketAddr = format!("{ip}:{port}")
        .parse()
        .map_err(|e| io::Error::other(format!("invalid addr: {e}")))?;

    // Generate a canonical PSK
    let raw_psk = crypto::generate_psk();
    let code = crypto::format_psk(&raw_psk);
    let canonical_psk = crypto::parse_psk(&code).expect("just-generated code must parse");

    // Build all formats
    let invite_link = super::invite::build_invite_link(identity.as_str(), &addr, &canonical_psk);
    let relay_code = super::invite::encode_relay_code(&addr, &canonical_psk);
    let word_phrase = super::invite::encode_words(&addr, &canonical_psk);

    if json_mode {
        let output = serde_json::json!({
            "identity": identity.as_str(),
            "invite_link": invite_link,
            "relay_code": relay_code,
            "word_phrase": word_phrase,
            "addr": addr.to_string(),
        });
        println!("{}", serde_json::to_string_pretty(&output).unwrap());
        return Ok(());
    }

    println!("Your identity: {}", identity);
    println!();

    // Relay code (short, speakable)
    println!("  RELAY CODE:  {}", relay_code);
    println!();

    // Word phrase (memorable)
    if show_words {
        println!("  WORD PHRASE: {}", word_phrase);
        println!();
    }

    // Invite link (full)
    println!("  INVITE LINK: {}", invite_link);
    println!();

    // Join instructions
    println!("Share any of the above with your peer. They run:");
    println!();
    println!("  claudectl --relay \"join {}\"", relay_code);
    if show_words {
        println!("  claudectl --relay \"join {}\"", word_phrase);
    }
    println!("  claudectl --relay \"join {}\"", invite_link);
    println!();

    // QR code
    if show_qr {
        println!("QR Code (scan to join):");
        println!();
        println!("{}", super::invite::render_qr(&invite_link));
    }

    // Also store as pending (for the serve side to accept)
    let pending_path = super::peers_dir().join("_pending.key");
    let _ = std::fs::create_dir_all(super::peers_dir());
    let _ = std::fs::write(&pending_path, crypto::hex_encode(&canonical_psk));

    Ok(())
}

/// `claudectl relay join <code|link|words>`
fn cmd_join(args: &[&str]) -> io::Result<()> {
    if args.is_empty() {
        eprintln!("Usage: claudectl --relay \"join <relay-code | invite-link | word-phrase>\"");
        return Err(io::Error::other("missing argument"));
    }

    let input = args.join(" ");
    let identity = load_or_create_identity();

    // Detect format and parse
    let (addr, psk, remote_identity) = if input.starts_with("cctl://") {
        // Invite link
        let (id, addr, psk) = super::invite::parse_invite_link(&input)
            .map_err(|e| io::Error::other(format!("invalid invite link: {e}")))?;
        (addr, psk, Some(id))
    } else if input.contains('-')
        && input
            .split('-')
            .all(|w| w.len() <= 5 && w.chars().all(|c| c.is_ascii_alphabetic()))
    {
        // Word phrase (all segments are short alphabetic words)
        let (addr, psk) = super::invite::decode_words(&input)
            .map_err(|e| io::Error::other(format!("invalid word phrase: {e}")))?;
        (addr, psk, None)
    } else {
        // Relay code (base32 alphanumeric)
        let (addr, psk) = super::invite::decode_relay_code(&input)
            .map_err(|e| io::Error::other(format!("invalid relay code: {e}")))?;
        (addr, psk, None)
    };

    println!("Connecting to {}...", addr);

    // Try connecting
    let (remote_id, registry) = try_connect(addr, &psk, &identity)
        .map_err(|e| io::Error::other(format!("connection failed: {e}")))?;

    // Verify identity if provided in the link
    if let Some(ref expected) = remote_identity {
        if remote_id != *expected {
            println!(
                "Warning: expected peer '{}' but connected to '{}'",
                expected, remote_id
            );
        }
    }

    println!("Paired with {} ({})", remote_id, addr);

    // Save PSK and metadata
    let _ = save_peer_psk(&remote_id, &psk);
    let _ = super::save_peer_meta(&remote_id, &addr.to_string());

    // Run the connection loop
    run_connect_loop(&registry, identity.as_str());

    Ok(())
}

/// `claudectl relay discover [--json]`
fn cmd_discover(json_mode: bool) -> io::Result<()> {
    let identity = load_or_create_identity();

    println!("Scanning LAN for claudectl instances (3 seconds)...");
    println!();

    let peers = super::lan::scan_lan(std::time::Duration::from_secs(3), identity.as_str());

    if json_mode {
        let json_peers: Vec<serde_json::Value> = peers
            .iter()
            .map(|p| {
                serde_json::json!({
                    "identity": p.identity,
                    "addr": p.relay_addr().to_string(),
                    "version": p.version,
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&json_peers).unwrap());
        return Ok(());
    }

    if peers.is_empty() {
        println!("No claudectl instances found on the local network.");
        println!();
        println!("Make sure peers are running: claudectl --relay serve");
        println!("Or use invite codes: claudectl --relay invite");
    } else {
        println!("Found {} instance(s):", peers.len());
        println!();
        println!("  {:<20} {:<24} VERSION", "IDENTITY", "ADDRESS");
        println!("  {}", "─".repeat(56));
        for peer in &peers {
            let paired = if load_peer_psk(&peer.identity).is_some() {
                " (paired)"
            } else {
                ""
            };
            println!(
                "  {:<20} {:<24} {}{}",
                peer.identity,
                peer.relay_addr().to_string(),
                peer.version,
                paired,
            );
        }
        println!();
        println!("To pair, run: claudectl --relay \"invite\" on the remote machine,");
        println!("then:         claudectl --relay \"join <code>\" here.");
    }

    Ok(())
}

/// Detect the local LAN IP address (not loopback).
fn detect_local_ip() -> Option<String> {
    // Connect a UDP socket to a public address to determine our LAN IP
    // (No actual data is sent — this just triggers route lookup)
    let socket = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("8.8.8.8:80").ok()?;
    let local_addr = socket.local_addr().ok()?;
    Some(local_addr.ip().to_string())
}
