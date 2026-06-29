// CLI dispatch for relay subcommands.

use std::io;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use clap::Subcommand;

use super::crypto;
use super::delegation::{self, DelegationContext};
use super::listener::RelayListener;
use super::mesh::PeerRegistry;
use super::peer::PeerConnection;
use super::{
    PENDING_PEER_ID, RelayMessage, clear_pending_psk, forget_peer, gen_msg_id, is_valid_peer_id,
    list_known_peers, load_or_create_identity, load_peer_meta, load_peer_psk, load_pending_psk,
    save_peer_psk, save_pending_psk,
};

#[derive(Subcommand)]
pub enum RelayCommand {
    /// Start relay listener
    Serve {
        /// Port to listen on
        #[arg(long, default_value_t = 9847)]
        port: u16,
        /// HTTP API port for coordinator mode (enables /api/sessions, /api/workers, /api/heartbeat)
        #[arg(long)]
        http_port: Option<u16>,
        /// Bearer token for HTTP API authentication
        #[arg(long)]
        auth_token: Option<String>,
    },

    /// Generate a raw PSK pairing code
    Pair,

    /// Accept a pairing code from another peer
    Accept {
        /// The pair code
        code: String,
        /// The peer identity
        peer_id: String,
    },

    /// Connect to a remote relay
    Connect {
        /// Remote address (host:port)
        addr: String,
    },

    /// List known peers
    Peers,

    /// Disconnect from a peer (informational in standalone mode)
    Disconnect {
        /// Peer ID to disconnect
        peer_id: String,
    },

    /// Remove all data for a peer
    Forget {
        /// Peer ID to forget
        peer_id: String,
    },

    /// Show this instance's relay identity
    Identity,

    /// Delegate a task to a remote peer
    Delegate {
        /// Target peer ID
        peer: String,
        /// Prompt to send
        prompt: String,
        /// Working directory for the task
        #[arg(long)]
        cwd: Option<String>,
        /// Git ref for the task context
        #[arg(long)]
        git_ref: Option<String>,
    },

    /// Show remote task status
    Status,

    /// Interrupt a remote task
    Interrupt {
        /// Peer that owns the task (required to route the interrupt)
        #[arg(long)]
        peer: String,
        /// Task ID
        task_id: String,
        /// Interrupt type (nudge, stop, reroute)
        interrupt_type: String,
        /// Optional reason
        reason: Vec<String>,
    },

    /// Generate invite code/link/phrase
    Invite {
        /// Show QR code
        #[arg(long)]
        qr: bool,
        /// Show word phrase
        #[arg(long)]
        words: bool,
    },

    /// Join using any invite format (relay code, word phrase, or invite link)
    Join {
        /// Invite code, word phrase, or invite link
        input: Vec<String>,
    },

    /// Scan LAN for nearby claudectl instances
    Discover,
}

/// Dispatch a relay subcommand.
pub fn dispatch_command(command: &RelayCommand, json_mode: bool) -> io::Result<()> {
    match command {
        RelayCommand::Serve {
            port,
            http_port,
            auth_token,
        } => cmd_serve(*port, http_port.as_ref().copied(), auth_token.as_deref()),
        RelayCommand::Pair => cmd_pair(json_mode),
        RelayCommand::Accept { code, peer_id } => cmd_accept(code, peer_id),
        RelayCommand::Connect { addr } => cmd_connect(addr),
        RelayCommand::Peers => cmd_peers(json_mode),
        RelayCommand::Disconnect { peer_id } => cmd_disconnect(peer_id),
        RelayCommand::Forget { peer_id } => cmd_forget(peer_id),
        RelayCommand::Identity => cmd_identity(json_mode),
        RelayCommand::Delegate {
            peer,
            prompt,
            cwd,
            git_ref,
        } => cmd_delegate(peer, prompt, cwd.as_deref(), git_ref.clone(), json_mode),
        RelayCommand::Status => cmd_task_status(json_mode),
        RelayCommand::Interrupt {
            peer,
            task_id,
            interrupt_type,
            reason,
        } => cmd_interrupt(peer, task_id, interrupt_type, reason),
        RelayCommand::Invite { qr, words } => cmd_invite(*qr, *words, json_mode),
        RelayCommand::Join { input } => cmd_join(input),
        RelayCommand::Discover => cmd_discover(json_mode),
    }
}

/// `claudectl relay serve [--port PORT] [--http-port PORT] [--auth-token TOKEN]`
/// Start the relay listener in the foreground.
fn cmd_serve(port: u16, http_port: Option<u16>, auth_token: Option<&str>) -> io::Result<()> {
    let mut port = port;

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

    // Start HTTP coordinator server if configured
    let http_port = http_port.or(relay_cfg.http_port);
    let auth_token_str = auth_token
        .map(|s| s.to_string())
        .or(relay_cfg.auth_token.clone());

    let coord_state = Arc::new(Mutex::new(super::http::CoordinatorState {
        identity: identity.as_str().to_string(),
        workers: std::collections::HashMap::new(),
        local_sessions: Vec::new(),
    }));

    let _http_server = if let (Some(hp), Some(token)) = (http_port, &auth_token_str) {
        let http_addr: SocketAddr = format!("{}:{hp}", relay_cfg.listen_addr)
            .parse()
            .map_err(|e| io::Error::other(format!("invalid http addr: {e}")))?;
        let server =
            super::http::HttpServer::start(http_addr, token.to_string(), Arc::clone(&coord_state))?;
        println!("HTTP API on http://{}", server.addr);
        Some(server)
    } else {
        None
    };

    println!("Press Ctrl+C to stop.");

    // Initialize worker for task delegation
    let mut worker = super::worker::RemoteWorker::new(identity.as_str());

    // Initialize hive gossip engine (only when hive feature is enabled)
    #[cfg(feature = "hive")]
    let (mut hive_store, mut gossip, broadcast_rx) = {
        let hive_enabled = crate::hive::is_active(Some(&hive_cfg));
        let store = hive_enabled.then(crate::hive::store::HiveStore::load);
        let gossip_engine = hive_enabled.then(|| {
            let mut engine = crate::hive::gossip::GossipEngine::new(
                identity.as_str(),
                hive_cfg.max_propagation,
                hive_cfg.knowledge_ttl_days,
            );
            engine.set_sharing_filter(crate::hive::SharingFilter::from_config(&hive_cfg));
            if let Some(mode) = crate::hive::exposure::ShareMode::parse(&hive_cfg.share_mode) {
                engine.set_share_mode(mode);
            }
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
                        reg.handle_heartbeat(&from_peer, &msg.payload);
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
                            let installed = crate::hive::cli::auto_accept_units(&accepted, None);
                            if installed > 0 {
                                println!(
                                    "[{}] Auto-installed {installed} artifact(s)",
                                    crate::logger::timestamp_now()
                                );
                            }
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
                            let (stats, merged) = gossip.handle_snapshot(hive_store, &msg);
                            println!(
                                "[{}] KnowledgeSnapshot from {}: {} accepted",
                                crate::logger::timestamp_now(),
                                from_peer,
                                stats.accepted
                            );
                            let installed = crate::hive::cli::auto_accept_units(&merged, None);
                            if installed > 0 {
                                println!(
                                    "[{}] Auto-installed {installed} artifact(s)",
                                    crate::logger::timestamp_now()
                                );
                            }
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

            let events = reg.tick(identity.as_str(), None);
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

            // Sync worker states to the HTTP coordinator state
            if let Ok(mut cs) = coord_state.lock() {
                for (k, v) in reg.all_worker_states() {
                    cs.workers.insert(k.clone(), v.clone());
                }
                // Expire workers that vanished from the registry
                let registry_keys: std::collections::HashSet<&String> =
                    reg.all_worker_states().keys().collect();
                cs.workers.retain(|k, _| registry_keys.contains(k));
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
            "They should run: claudectl relay accept {} {}",
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
fn cmd_accept(code: &str, peer_id: &str) -> io::Result<()> {
    if !is_valid_peer_id(peer_id) || peer_id == PENDING_PEER_ID {
        return Err(io::Error::other(format!("invalid peer id: {peer_id}")));
    }

    let psk =
        crypto::parse_psk(code).map_err(|e| io::Error::other(format!("invalid code: {e}")))?;

    save_peer_psk(peer_id, &psk).map_err(io::Error::other)?;

    clear_pending_psk();

    println!("Paired with peer: {}", peer_id);
    println!("PSK stored. You can now connect with:");
    println!("  claudectl relay connect <host>:<port>");

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
                        reg.handle_heartbeat(peer_id, &msg.payload);
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
            let events = reg.tick(identity.as_str(), None);
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

/// Open a one-shot authenticated connection to `peer_id`, send `msg`, and close
/// (#378). This is how `relay delegate`/`interrupt` actually reach a peer from a
/// standalone CLI process — it connects directly to the peer using the stored
/// PSK + address, independent of any running `relay serve` daemon. Returns the
/// resolved remote id on success; an `Err` here must surface as a non-zero exit
/// so scripts never mistake a built-but-unsent message for a delivered one.
fn send_message_to_peer(
    peer_id: &str,
    identity: &super::PeerId,
    msg: &RelayMessage,
) -> Result<String, String> {
    let psk = load_peer_psk(peer_id).ok_or_else(|| {
        format!("peer '{peer_id}' is not paired — run `claudectl relay pair` first")
    })?;
    let meta = load_peer_meta(peer_id)
        .ok_or_else(|| format!("no stored address for peer '{peer_id}' — pair or connect first"))?;
    let addr_str = meta
        .get("addr")
        .and_then(|v| v.as_str())
        .ok_or_else(|| format!("peer '{peer_id}' metadata has no address"))?;
    let addr: SocketAddr = addr_str
        .parse()
        .map_err(|e| format!("invalid stored address '{addr_str}' for '{peer_id}': {e}"))?;

    let (remote_id, registry) = try_connect(addr, &psk, identity)?;
    if remote_id != peer_id {
        return Err(format!(
            "remote identity mismatch at {addr}: expected {peer_id}, got {remote_id}"
        ));
    }
    registry
        .lock()
        .map_err(|_| "registry lock poisoned".to_string())?
        .send_to(&remote_id, msg)?;
    // Let the frame flush to the peer before the connection drops at scope end.
    std::thread::sleep(std::time::Duration::from_millis(250));
    Ok(remote_id)
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
fn cmd_connect(addr_str: &str) -> io::Result<()> {
    let addr: SocketAddr = addr_str
        .parse()
        .map_err(|e| io::Error::other(format!("invalid address '{addr_str}': {e}")))?;

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

    eprintln!("Could not connect to {}", addr_str);
    eprintln!("Make sure you have paired with this peer first:");
    eprintln!("  1. Remote runs: claudectl relay pair");
    eprintln!("  2. You run:     claudectl relay accept <code> <peer-id>");
    Err(io::Error::other("connection failed"))
}

/// `claudectl relay peers`
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
            println!("No paired peers. Run 'claudectl relay pair' to get started.");
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
fn cmd_disconnect(peer_id: &str) -> io::Result<()> {
    // In standalone CLI mode, we can't disconnect a live connection
    // (that's handled by the TUI/serve loop). Just inform the user.
    println!("Note: to disconnect a live connection, stop the relay serve/connect process.");
    println!(
        "To remove the pairing entirely, use: claudectl relay forget {}",
        peer_id
    );
    Ok(())
}

/// `claudectl relay forget <peer_id>`
/// Remove all data for a peer.
fn cmd_forget(peer_id: &str) -> io::Result<()> {
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
fn cmd_delegate(
    peer_id: &str,
    prompt: &str,
    cwd: Option<&str>,
    git_ref: Option<String>,
    json_mode: bool,
) -> io::Result<()> {
    let identity = load_or_create_identity();
    let task_id = gen_msg_id().replace("msg_", "task_");

    let context = DelegationContext {
        git_ref,
        ..Default::default()
    };

    let msg =
        delegation::build_delegate_message(&task_id, prompt, cwd, &context, identity.as_str())
            .map_err(|e| io::Error::other(format!("build message: {e}")))?;

    // Actually transmit to the peer (#378). On failure we must exit non-zero so
    // callers don't treat a built-but-unsent message as delivered.
    let sent = send_message_to_peer(peer_id, &identity, &msg);

    if json_mode {
        let output = serde_json::json!({
            "task_id": task_id,
            "peer": peer_id,
            "prompt": prompt,
            "cwd": cwd,
            "status": if sent.is_ok() { "delegated" } else { "failed" },
            "error": sent.as_ref().err(),
        });
        println!("{}", serde_json::to_string_pretty(&output).unwrap());
    } else {
        match &sent {
            Ok(remote) => {
                println!("Task {task_id} delegated to peer {remote}");
                println!("  Prompt: {prompt}");
                if let Some(c) = cwd {
                    println!("  CWD: {c}");
                }
            }
            Err(e) => {
                eprintln!("Failed to delegate task {task_id} to {peer_id}: {e}");
            }
        }
    }

    sent.map(|_| ()).map_err(io::Error::other)
}

/// `claudectl relay status`
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
fn cmd_interrupt(
    peer_id: &str,
    task_id: &str,
    interrupt_type: &str,
    reason: &[String],
) -> io::Result<()> {
    let reason_str = reason.join(" ");

    let identity = load_or_create_identity();
    let msg = delegation::build_interrupt_message(
        task_id,
        interrupt_type,
        &reason_str,
        identity.as_str(),
    );

    // Route the interrupt to the peer that owns the task (#378).
    let sent = send_message_to_peer(peer_id, &identity, &msg);

    match &sent {
        Ok(remote) => {
            println!("Interrupt sent for task {task_id} to peer {remote}");
            println!("  Type: {interrupt_type}");
            if !reason_str.is_empty() {
                println!("  Reason: {reason_str}");
            }
            println!("  Message ID: {}", msg.id);
        }
        Err(e) => {
            eprintln!("Failed to send interrupt for task {task_id}: {e}");
        }
    }

    sent.map(|_| ()).map_err(io::Error::other)
}

// ────────────────────────────────────────────────────────────────────────────
// Discovery commands: invite, join, discover
// ────────────────────────────────────────────────────────────────────────────

/// `claudectl relay invite [--qr] [--words]`
fn cmd_invite(show_qr: bool, show_words: bool, json_mode: bool) -> io::Result<()> {
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
    println!("  claudectl relay join {}", relay_code);
    if show_words {
        println!("  claudectl relay join {}", word_phrase);
    }
    println!("  claudectl relay join {}", invite_link);
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
fn cmd_join(input: &[String]) -> io::Result<()> {
    if input.is_empty() {
        eprintln!("Usage: claudectl relay join <relay-code | invite-link | word-phrase>");
        return Err(io::Error::other("missing argument"));
    }

    let input = input.join(" ");
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

/// `claudectl relay discover`
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
        println!("Make sure peers are running: claudectl relay serve");
        println!("Or use invite codes: claudectl relay invite");
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
        println!("To pair, run: claudectl relay invite on the remote machine,");
        println!("then:         claudectl relay join <code> here.");
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
