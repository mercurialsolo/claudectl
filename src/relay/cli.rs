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
        /// Bind address for the HTTP API [default: 127.0.0.1]. The API is
        /// plaintext — use a tunnel rather than binding 0.0.0.0.
        #[arg(long)]
        http_addr: Option<String>,
        /// Bearer token for HTTP API authentication
        #[arg(long)]
        auth_token: Option<String>,
    },

    /// Keep `relay serve` running across logout and reboot (#438, macOS)
    ///
    /// Installs a launchd agent that starts the relay at login and restarts it
    /// if it dies. Without this, nothing keeps a relay alive and the cluster
    /// view goes stale the moment you close a terminal.
    ///
    /// Re-run it to update the agent after changing ports or upgrading.
    InstallAgent {
        /// Port the relay should listen on
        #[arg(long, default_value_t = 9847)]
        port: u16,
        /// Also serve the coordinator HTTP API on this port
        #[arg(long)]
        http_port: Option<u16>,
        /// Bind address for the HTTP API [default: 127.0.0.1]
        #[arg(long)]
        http_addr: Option<String>,
        /// Bearer token for the HTTP API. Note this is stored in the plist
        /// under ~/Library/LaunchAgents, which is readable by your user.
        #[arg(long)]
        auth_token: Option<String>,
    },

    /// Remove the launchd agent (#438, macOS)
    UninstallAgent,

    /// Whether the launchd agent is installed and running (#438, macOS)
    AgentStatus,

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

    /// Show every session running across the cluster (local + all peers)
    Fleet,
}

/// Dispatch a relay subcommand.
pub fn dispatch_command(command: &RelayCommand, json_mode: bool) -> io::Result<()> {
    match command {
        RelayCommand::Serve {
            port,
            http_port,
            http_addr,
            auth_token,
        } => cmd_serve(
            *port,
            http_port.as_ref().copied(),
            http_addr.as_deref(),
            auth_token.as_deref(),
        ),
        RelayCommand::InstallAgent {
            port,
            http_port,
            http_addr,
            auth_token,
        } => cmd_install_agent(
            super::agent::AgentConfig {
                port: *port,
                http_port: *http_port,
                http_addr: http_addr.clone(),
                auth_token: auth_token.clone(),
            },
            json_mode,
        ),
        RelayCommand::UninstallAgent => cmd_uninstall_agent(json_mode),
        RelayCommand::AgentStatus => cmd_agent_status(json_mode),
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
        RelayCommand::Fleet => cmd_fleet(json_mode),
    }
}

/// Join a host and port into something `SocketAddr` will parse.
///
/// A bare IPv6 literal needs brackets: `::1` and `9876` make `[::1]:9876`, not
/// `::1:9876`. `--http-addr ::1` is a plausible thing to type now that the HTTP
/// API defaults to loopback, so handle it rather than failing to parse.
fn join_host_port(host: &str, port: u16) -> String {
    if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
}

/// `claudectl relay serve [--port PORT] [--http-port PORT] [--http-addr ADDR] [--auth-token TOKEN]`
/// Start the relay listener in the foreground.
fn cmd_serve(
    port: u16,
    http_port: Option<u16>,
    http_addr: Option<&str>,
    auth_token: Option<&str>,
) -> io::Result<()> {
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
    let listen_addr = join_host_port(&relay_cfg.listen_addr, port);
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

    // The HTTP API binds its own address, not the peer transport's. The peer
    // transport is HMAC-authenticated and meant to be reachable; the HTTP API is
    // plaintext with a bearer token, so it defaults to loopback (#426).
    let http_host = http_addr.unwrap_or(&relay_cfg.http_addr);

    // An empty token would bind a listener that rejects every request, so treat
    // it as unconfigured rather than half-starting the API.
    let auth_token_str = auth_token_str.filter(|t| !t.is_empty());

    // The API needs both a port and a token. Saying so beats falling through
    // silently and leaving the operator's dashboard on connection-refused.
    match (http_port, &auth_token_str) {
        (Some(_), None) => eprintln!(
            "warning: --http-port was given without a non-empty --auth-token, \
             so the HTTP API is not running."
        ),
        (None, Some(_)) => eprintln!(
            "warning: an auth token is set but no --http-port, so the HTTP API \
             is not running."
        ),
        _ => {}
    }

    let _http_server = if let (Some(hp), Some(token)) = (http_port, &auth_token_str) {
        let http_bind = join_host_port(http_host, hp);
        let http_sock: SocketAddr = http_bind
            .parse()
            .map_err(|e| io::Error::other(format!("invalid http addr '{http_bind}': {e}")))?;
        let server =
            super::http::HttpServer::start(http_sock, token.to_string(), Arc::clone(&coord_state))?;
        println!("HTTP API on http://{}", server.addr);
        if claudectl_core::helpers::is_exposed_bind(&server.addr) {
            eprintln!(
                "warning: HTTP API is reachable from the network on {}.",
                server.addr
            );
            eprintln!("         It is plaintext HTTP with a bearer token and no rate limiting.");
            eprintln!("         Prefer --http-addr 127.0.0.1 fronted by a tunnel (Cloudflare");
            eprintln!("         Tunnel, Tailscale Funnel, ssh -R). See docs/relay.md.");
        }
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

    // LAN discovery. `relay discover` has always *scanned* for announcements,
    // but nothing ever sent one — `start_announcer` had no callers anywhere, and
    // `#![allow(dead_code)]` on `relay/mod.rs` kept that quiet. So `discover`
    // returned "no instances found" even with a relay running next to it, while
    // telling the operator to start the thing that was already running (#433).
    //
    // Note the flag polarity: `start_announcer` takes a *shutdown* flag (it
    // loops while that is `false`), whereas this function's `running` means the
    // opposite. Passing `running` directly stops the thread on its first check,
    // which looks exactly like a working announcer that sends nothing — so the
    // announcer gets its own flag, set when the serve loop exits.
    let lan_shutdown = Arc::new(std::sync::atomic::AtomicBool::new(false));

    // The hive block this machine advertises (#433). Read once at startup, like
    // the index in `query serve`: a rename takes a relay restart, which is said
    // plainly in the banner rather than left to be discovered.
    //
    // A malformed identity file costs the hive block, not the relay — the
    // machine is still worth discovering, so the error is printed and
    // advertisement continues without a hive.
    let hive_peers = Arc::new(std::sync::atomic::AtomicU32::new(0));
    let hive_units = Arc::new(std::sync::atomic::AtomicU32::new(0));
    // #434: the owner approving a queued join request is not necessarily at this
    // terminal, so a pending request fires a hook as well as printing.
    #[cfg(feature = "hive")]
    let hook_registry = crate::config::load_hooks();

    // #434: membership decisions answer to the same identity the advertiser
    // uses, so it is loaded once here and both read it. Re-reading it per
    // message would also let a rename take effect mid-run, which the banner
    // below promises it does not.
    #[cfg(feature = "hive")]
    let hive_identity: Option<crate::hive::identity::HiveIdentity> =
        match crate::hive::identity::load() {
            Ok(Some(id)) => Some(id),
            Ok(None) => {
                println!(
                    "Hive: unnamed, so nothing is advertised (claudectl hive identity set --name X)"
                );
                None
            }
            Err(e) => {
                eprintln!("warning: hive identity unreadable, advertising no hive: {e}");
                None
            }
        };
    // The gate is on only while a hive is named. `None` means every paired peer
    // is treated exactly as it was before #434, so nobody's existing setup goes
    // quiet on upgrade.
    #[cfg(feature = "hive")]
    let hive_roster = hive_identity
        .as_ref()
        .map(|_| crate::hive::membership::Roster::local());

    #[cfg(feature = "hive")]
    let hive_advert = hive_identity.as_ref().map(|id| {
        // `effective_join_policy`, never the stored field: #432 records
        // consent for `open` in the data precisely so this cannot put an
        // unconfirmed `open` on the wire.
        let effective = id.effective_join_policy();
        println!(
            "Hive: \"{}\" ({}) advertised as join_policy={}",
            id.name,
            id.hive_id,
            effective.as_str()
        );
        if !id.open_is_acknowledged() {
            println!(
                "  (stored policy is `open` but was never confirmed — advertising `{}`)",
                effective.as_str()
            );
        }
        println!(
            "  Members: {} admitted, {} awaiting approval",
            crate::hive::membership::list_members().len(),
            crate::hive::membership::pending_requests().len()
        );
        super::lan::HiveAdvert {
            id: id.hive_id.clone(),
            name: id.name.clone(),
            join_policy: effective.as_str().to_string(),
            peers: Arc::clone(&hive_peers),
            units: Arc::clone(&hive_units),
        }
    });
    #[cfg(not(feature = "hive"))]
    let hive_advert: Option<super::lan::HiveAdvert> = None;

    let lan_handle = if relay_cfg.lan_announce {
        println!(
            "LAN discovery: announcing every {}s on UDP {}",
            super::lan::ANNOUNCE_INTERVAL_SECS,
            super::lan::LAN_PORT
        );
        Some(super::lan::start_announcer(
            identity.clone(),
            port,
            super::lan::ANNOUNCE_INTERVAL_SECS,
            Arc::clone(&lan_shutdown),
            hive_advert,
        ))
    } else {
        println!("LAN discovery: off ([relay] lan_announce = false)");
        None
    };

    // This machine's sessions, advertised to peers on every heartbeat.
    let mut local_feed = super::advertise::LocalSessionFeed::new();

    while running.load(std::sync::atomic::Ordering::Relaxed) {
        std::thread::sleep(std::time::Duration::from_secs(1));

        // Collect before taking the registry lock: enrichment shells out to
        // `ps` and reads JSONL tails, and holding the lock across that would
        // stall incoming messages.
        let collected = local_feed.collect_if_due(std::time::Instant::now());

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
                    super::MessageType::HiveJoinRequest => {
                        // `from_peer` here is the id the *connection*
                        // authenticated as, not a string off the payload — it
                        // becomes a filename, so it must be the former.
                        let request: super::hivejoin::JoinRequestPayload = serde_json::from_value(
                            msg.payload.clone(),
                        )
                        .unwrap_or(super::hivejoin::JoinRequestPayload {
                            hive_id: None,
                            label: None,
                            grant: None,
                        });
                        let result = super::hivejoin::decide_join(
                            hive_identity.as_ref(),
                            &crate::hive::membership::Roster::local(),
                            from_peer.as_str(),
                            &request,
                        );
                        println!(
                            "[{}] hive join request from {}: {}{}",
                            crate::logger::timestamp_now(),
                            from_peer,
                            result.state.as_str(),
                            result
                                .reason
                                .as_deref()
                                .map(|r| format!(" — {r}"))
                                .unwrap_or_default()
                        );
                        if result.state == super::hivejoin::JoinResultState::Pending {
                            // The owner is not necessarily watching this
                            // terminal, so the queue gets an event too.
                            hook_registry.fire_env(
                                crate::hooks::HookEvent::HiveJoinRequest,
                                &[
                                    ("CLAUDECTL_HIVE_JOIN_PEER", from_peer.as_str()),
                                    (
                                        "CLAUDECTL_HIVE_JOIN_HIVE",
                                        result.hive_id.as_deref().unwrap_or(""),
                                    ),
                                    (
                                        "CLAUDECTL_HIVE_JOIN_NAME",
                                        result.name.as_deref().unwrap_or(""),
                                    ),
                                ],
                            );
                        }
                        let reply = super::hivejoin::build_join_result(identity.as_str(), &result);
                        let _ = reg.send_to(from_peer.as_str(), &reply);
                    }
                    #[cfg(feature = "hive")]
                    super::MessageType::KnowledgeRejected => {
                        eprintln!(
                            "[{}] {} refused our knowledge: {}",
                            crate::logger::timestamp_now(),
                            from_peer,
                            super::hivejoin::rejection_reason(&msg.payload)
                        );
                    }
                    #[cfg(feature = "hive")]
                    super::MessageType::HiveJoinResult => {
                        // We are the joiner: the host has answered, possibly
                        // long after `hive join` exited (an owner approving a
                        // queued request). Record it so `hive status` is true.
                        match crate::hive::cli::apply_join_result(from_peer.as_str(), &msg.payload)
                        {
                            Ok(Some(line)) => {
                                println!("[{}] {line}", crate::logger::timestamp_now())
                            }
                            Ok(None) => {}
                            Err(e) => eprintln!(
                                "[{}] could not record hive join result: {e}",
                                crate::logger::timestamp_now()
                            ),
                        }
                    }
                    #[cfg(feature = "hive")]
                    super::MessageType::KnowledgeSync => {
                        // #434: a peer that is not in the hive does not get to
                        // contribute to it. Dropping inbound units matters as
                        // much as refusing to send: an `ask` hive that gated
                        // only its own sends would still merge whatever an
                        // unapproved peer pushed.
                        if let Some(rejection) = super::hivejoin::knowledge_refusal(
                            hive_roster.as_ref(),
                            from_peer.as_str(),
                            identity.as_str(),
                            &msg.payload,
                        ) {
                            // Refused on the wire, not merely dropped (#435): a
                            // reader that believes it is contributing and is
                            // silently ignored cannot tell that from a network
                            // fault.
                            println!(
                                "[{}] KnowledgeSync from {} refused — {}",
                                crate::logger::timestamp_now(),
                                from_peer,
                                super::hivejoin::rejection_reason(&rejection.payload)
                            );
                            let _ = reg.send_to(from_peer.as_str(), &rejection);
                        } else if let (Some(gossip), Some(hive_store)) =
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
                                let connected = hive_gossip_targets(
                                    hive_roster.as_ref(),
                                    reg.connected_peers(),
                                );
                                let prop_msgs = gossip.propagate(&accepted, &from_peer, &connected);
                                for (target, prop_msg) in prop_msgs {
                                    let _ = reg.send_to(target.as_str(), &prop_msg);
                                }
                            }
                        }
                    }
                    #[cfg(feature = "hive")]
                    super::MessageType::KnowledgeRequest => {
                        // Asking for a snapshot is *receiving*, so a reader may.
                        if !hive_may_receive(hive_roster.as_ref(), from_peer.as_str()) {
                            println!(
                                "[{}] KnowledgeRequest from {} refused — not a member of this hive",
                                crate::logger::timestamp_now(),
                                from_peer
                            );
                        } else if let (Some(gossip), Some(hive_store)) =
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
                        if !hive_may_contribute(hive_roster.as_ref(), from_peer.as_str()) {
                            println!(
                                "[{}] KnowledgeSnapshot from {} refused — not a contributor to this hive",
                                crate::logger::timestamp_now(),
                                from_peer
                            );
                        } else if let (Some(gossip), Some(hive_store)) =
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
                    // #434: only hive members are sync targets.
                    let connected =
                        hive_gossip_targets(hive_roster.as_ref(), reg.connected_peers());
                    let sync_msgs = gossip.generate_sync_messages(hive_store, &connected);
                    for (target, sync_msg) in sync_msgs {
                        let _ = reg.send_to(target.as_str(), &sync_msg);
                    }
                }
            }

            let events = reg.tick(identity.as_str(), Some(local_feed.sessions()));
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
                if collected {
                    // Without this the coordinator reported every peer's
                    // sessions but none of its own.
                    cs.local_sessions = local_feed.sessions().to_vec();
                }
            }

            // Publish the fleet snapshot the local TUI reads. Same beat as
            // collection, so a peer's heartbeat shows up within one interval.
            if collected {
                super::advertise::publish_snapshot(identity.as_str(), &reg);
            }

            // Counts for the LAN hive advertisement (#433). Stored here, where
            // the registry lock is already held, and read by the announcer
            // thread through an atomic — so nothing it does can block this loop.
            hive_peers.store(
                reg.connected_count() as u32,
                std::sync::atomic::Ordering::Relaxed,
            );
        }

        #[cfg(feature = "hive")]
        if let Some(store) = hive_store.as_ref() {
            hive_units.store(store.len() as u32, std::sync::atomic::Ordering::Relaxed);
        }
    }

    listener.stop();
    // Tell the announcer to stop, then wait for it so it is not killed
    // mid-`send_to`. Its sleep is the announce interval, so this can take that
    // long — which is why it happens after `listener.stop()` rather than before.
    lan_shutdown.store(true, std::sync::atomic::Ordering::Relaxed);
    if let Some(h) = lan_handle {
        let _ = h.join();
    }
    println!("\nRelay stopped.");
    Ok(())
}

/// `claudectl relay pair`
/// Generate a new PSK and display it.
/// `relay install-agent` (#438).
fn cmd_install_agent(cfg: super::agent::AgentConfig, json_mode: bool) -> io::Result<()> {
    let path = super::agent::install(&cfg).map_err(io::Error::other)?;
    let (out_log, err_log) = super::agent::log_paths();
    let loaded = super::agent::is_loaded();

    if json_mode {
        let json = serde_json::json!({
            "installed": true,
            "loaded": loaded,
            "plist": path.display().to_string(),
            "label": super::agent::AGENT_LABEL,
            "port": cfg.port,
            "stdout_log": out_log.display().to_string(),
            "stderr_log": err_log.display().to_string(),
        });
        println!(
            "{}",
            serde_json::to_string_pretty(&json).unwrap_or_default()
        );
        return Ok(());
    }

    println!("Installed the relay agent.");
    println!();
    println!("  plist:  {}", path.display());
    println!("  label:  {}", super::agent::AGENT_LABEL);
    println!("  port:   {}", cfg.port);
    println!("  logs:   {}", out_log.display());
    println!("          {}", err_log.display());
    println!();
    if loaded {
        println!("It is running now, starts at login, and restarts if it dies.");
    } else {
        // Installed-but-not-loaded is worth saying out loud rather than
        // reporting success: it is the state `doctor` will flag.
        println!("The plist is written but launchd does not report it as loaded.");
        println!("Check the error log above, then: claudectl relay agent-status");
    }
    if cfg.auth_token.is_some() {
        println!();
        println!("Note: --auth-token is stored in the plist, which is readable by your user.");
    }
    println!();
    println!("Remove it with: claudectl relay uninstall-agent");
    Ok(())
}

/// `relay uninstall-agent` (#438).
fn cmd_uninstall_agent(json_mode: bool) -> io::Result<()> {
    let (was_present, note) = super::agent::uninstall().map_err(io::Error::other)?;

    if json_mode {
        let json = serde_json::json!({
            "removed": was_present,
            "note": note,
        });
        println!(
            "{}",
            serde_json::to_string_pretty(&json).unwrap_or_default()
        );
        return Ok(());
    }

    if was_present {
        println!("Removed the relay agent. It will not come back at login.");
    } else {
        println!("No relay agent was installed — nothing to remove.");
    }
    if let Some(n) = note {
        // The plist is gone either way; this only explains why unloading was
        // noisy, usually "it was not running".
        println!();
        println!("(launchctl said: {n})");
        println!("The plist was removed regardless, so nothing will restart it.");
    }
    Ok(())
}

/// `relay agent-status` (#438).
fn cmd_agent_status(json_mode: bool) -> io::Result<()> {
    let st = super::agent::status();
    let (out_log, err_log) = super::agent::log_paths();

    if json_mode {
        let json = serde_json::json!({
            "plist_exists": st.plist_exists,
            "loaded": st.loaded,
            "plist": st.plist.display().to_string(),
            "label": super::agent::AGENT_LABEL,
            "stdout_log": out_log.display().to_string(),
            "stderr_log": err_log.display().to_string(),
        });
        println!(
            "{}",
            serde_json::to_string_pretty(&json).unwrap_or_default()
        );
        return Ok(());
    }

    match (st.plist_exists, st.loaded) {
        (true, true) => {
            println!("Relay agent: running.");
            println!("  plist: {}", st.plist.display());
            println!("  logs:  {}", out_log.display());
        }
        (true, false) => {
            println!("Relay agent: installed but NOT running.");
            println!("  plist: {}", st.plist.display());
            println!("  error log: {}", err_log.display());
            println!();
            println!("Re-run `claudectl relay install-agent` to reload it.");
        }
        (false, true) => {
            // Orphaned service with no plist — the state that makes a relay
            // keep coming back after someone deleted the file by hand.
            println!("Relay agent: loaded in launchd, but its plist is gone.");
            println!("Clean it up with: claudectl relay uninstall-agent");
        }
        (false, false) => {
            println!("Relay agent: not installed.");
            println!();
            println!("Nothing keeps `relay serve` alive across logout. Install it with:");
            println!("  claudectl relay install-agent");
        }
    }
    Ok(())
}

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

    // A machine that only connects out still advertises its own sessions, so
    // the fleet view is symmetric regardless of which side dialled.
    let mut local_feed = super::advertise::LocalSessionFeed::new();

    while running.load(std::sync::atomic::Ordering::Relaxed) {
        std::thread::sleep(std::time::Duration::from_secs(1));
        let collected = local_feed.collect_if_due(std::time::Instant::now());
        if let Ok(mut reg) = registry.lock() {
            let messages = reg.drain_messages();
            for (peer_id, msg) in &messages {
                match msg.msg_type {
                    super::MessageType::Heartbeat => {
                        reg.handle_heartbeat(peer_id, &msg.payload);
                    }
                    // #434: the owner of a `join_policy: ask` hive usually
                    // approves long after `hive join` exited, so this is where
                    // most approvals actually land.
                    #[cfg(feature = "hive")]
                    super::MessageType::KnowledgeRejected => {
                        eprintln!(
                            "[{}] {} refused our knowledge: {}",
                            crate::logger::timestamp_now(),
                            peer_id,
                            super::hivejoin::rejection_reason(&msg.payload)
                        );
                    }
                    #[cfg(feature = "hive")]
                    super::MessageType::HiveJoinResult => {
                        match crate::hive::cli::apply_join_result(peer_id.as_str(), &msg.payload) {
                            Ok(Some(line)) => {
                                println!("[{}] {line}", crate::logger::timestamp_now())
                            }
                            Ok(None) => {}
                            Err(e) => eprintln!(
                                "[{}] could not record hive join result: {e}",
                                crate::logger::timestamp_now()
                            ),
                        }
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
            let events = reg.tick(identity.as_str(), Some(local_feed.sessions()));
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

            // Publish the fleet snapshot for the local TUI, same as `serve`.
            if collected {
                super::advertise::publish_snapshot(identity.as_str(), &reg);
            }
        }
    }
    println!("\nDisconnected.");
}

/// Try to connect using a specific PSK. Returns Ok(registry) on success.
/// May we exchange hive knowledge with this peer? (#434)
///
/// `named_hive` is whether this machine runs a named hive. If it does not, there
/// is no membership to check and everything is permitted exactly as it was
/// before #434 — naming a hive is what turns the gate on, so nobody's existing
/// setup goes quiet on upgrade.
///
/// This is deliberately the *only* place the question is asked, and it is asked
/// on both directions: gating what we send without gating what we accept would
/// let an unapproved peer still push units into the hive.
/// May this peer be *sent* hive knowledge?
///
/// Any member, readers included — receiving without contributing is the whole
/// point of a reader (#435, §7.5).
#[cfg(feature = "hive")]
fn hive_may_receive(roster: Option<&crate::hive::membership::Roster>, peer_id: &str) -> bool {
    match roster {
        None => true,
        Some(r) => r.may_receive(peer_id),
    }
}

/// May this peer *contribute* hive knowledge?
///
/// Members whose role is contributor. A reader is refused here and allowed in
/// `hive_may_receive`, and that asymmetry is the feature.
#[cfg(feature = "hive")]
fn hive_may_contribute(roster: Option<&crate::hive::membership::Roster>, peer_id: &str) -> bool {
    match roster {
        None => true,
        Some(r) => r.may_contribute(peer_id),
    }
}

/// The subset of `connected` that may receive hive knowledge.
#[cfg(feature = "hive")]
fn hive_gossip_targets(
    roster: Option<&crate::hive::membership::Roster>,
    connected: Vec<super::PeerId>,
) -> Vec<super::PeerId> {
    let Some(r) = roster else {
        return connected;
    };
    connected
        .into_iter()
        .filter(|p| r.may_receive(p.as_str()))
        .collect()
}

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
pub fn send_message_to_peer(
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

/// `claudectl relay fleet`
/// Show every session running across the cluster — this machine plus every
/// peer that has reported in. Reads the snapshot `relay serve` publishes, so it
/// needs a relay running locally; without one it says so rather than printing
/// an empty table that looks like an idle cluster.
fn cmd_fleet(json_mode: bool) -> io::Result<()> {
    use claudectl_core::fleet;
    use claudectl_core::helpers::truncate_cell;

    let identity = load_or_create_identity();
    let snapshot = fleet::read_snapshot();
    let local = fleet::collect_local_sessions();

    if json_mode {
        let mut workers = vec![serde_json::json!({
            "worker_id": identity.as_str(),
            "local": true,
            "sessions": local,
        })];
        if let Some(snap) = &snapshot {
            for w in snap.live_workers() {
                workers.push(serde_json::json!({
                    "worker_id": w.worker_id,
                    "local": false,
                    "sessions": w.sessions,
                }));
            }
        }
        let output = serde_json::json!({
            "identity": identity.as_str(),
            "relay_running": snapshot.is_some(),
            "workers": workers,
        });
        println!("{}", serde_json::to_string_pretty(&output).unwrap());
        return Ok(());
    }

    // Rows and counts both come from `live_workers`, so a peer that stopped
    // reporting disappears from the table instead of inflating it.
    let live: Vec<fleet::FleetWorker> = snapshot
        .as_ref()
        .map(|s| s.live_workers().into_iter().cloned().collect())
        .unwrap_or_default();
    let remote_count: usize = live.iter().map(|w| w.sessions.len()).sum();
    let total = local.len() + remote_count;

    println!(
        "Fleet: {} session(s) across {} machine(s)",
        total,
        1 + live.len()
    );
    println!();
    println!("{:<20} {:<28} {:<14} COST", "MACHINE", "PROJECT", "STATUS");
    println!("{}", "─".repeat(72));

    for value in &local {
        // Peer ids are long enough that a trailing "(local)" would be the part
        // the column truncates away, so mark this machine with a star instead.
        print_fleet_row(&format!("{}*", truncate_cell(identity.as_str(), 18)), value);
    }
    for w in &live {
        for value in &w.sessions {
            print_fleet_row(&w.worker_id, value);
        }
    }

    if total == 0 {
        println!("(no sessions running)");
    } else {
        println!();
        println!("* = this machine");
    }

    if snapshot.is_none() {
        println!();
        println!("No relay snapshot found — showing local sessions only.");
        println!("Start a relay on each machine to see the whole cluster:");
        println!("  claudectl relay serve");
    }

    Ok(())
}

/// One row of the fleet table, tolerant of a peer running an older build that
/// omits a field.
fn print_fleet_row(machine: &str, value: &serde_json::Value) {
    use claudectl_core::helpers::truncate_cell;

    let project = value
        .get("project")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");
    let status = value
        .get("status")
        .and_then(|v| v.as_str())
        .unwrap_or("Unknown");
    let cost = match value.get("cost_usd").and_then(|v| v.as_f64()) {
        Some(c) => format!("${c:.2}"),
        None => "-".to_string(),
    };
    println!(
        "{:<20} {:<28} {:<14} {}",
        truncate_cell(machine, 19),
        truncate_cell(project, 27),
        status,
        cost
    );
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

// ────────────────────────────────────────────────────────────────────────────
// Hive invite and join (#434)
// ────────────────────────────────────────────────────────────────────────────

/// Is anything actually listening on our relay port?
///
/// `hive invite` hands out an address someone else is going to dial, so an
/// invite minted while nothing serves is an invite that cannot be redeemed. A
/// loopback connect is direct evidence; `relay agent-status` only knows what
/// launchd was told, which is not the same question.
#[cfg(feature = "hive")]
fn relay_is_listening(port: u16) -> bool {
    use std::net::{Ipv4Addr, SocketAddr, TcpStream};
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    TcpStream::connect_timeout(&addr, std::time::Duration::from_millis(300)).is_ok()
}

/// `claudectl hive invite`
///
/// A hive invite is a peer invite plus the hive's name — the holder still has to
/// reach this machine, so the address and PSK are exactly what `relay invite`
/// mints. Only the link can carry the hive id: the relay code and word phrase
/// spend all nine of their bytes on the address and PSK, which is why they are
/// printed with the hive named on a second line instead.
#[cfg(feature = "hive")]
pub fn cmd_hive_invite(show_qr: bool, show_words: bool, json_mode: bool) -> io::Result<()> {
    let hive = match crate::hive::identity::load() {
        Ok(Some(h)) => h,
        Ok(None) => {
            return Err(io::Error::other(
                "this machine has no named hive — run `claudectl hive identity set --name <name>` first",
            ));
        }
        Err(e) => return Err(io::Error::other(e)),
    };

    let identity = load_or_create_identity();
    let cfg = crate::config::Config::load();
    let relay_cfg = cfg.relay.unwrap_or_default();

    let ip = detect_local_ip().unwrap_or_else(|| "127.0.0.1".to_string());
    let port = relay_cfg.listen_port;
    let addr: std::net::SocketAddr = format!("{ip}:{port}")
        .parse()
        .map_err(|e| io::Error::other(format!("invalid addr: {e}")))?;

    let raw_psk = crypto::generate_psk();
    let code = crypto::format_psk(&raw_psk);
    let canonical_psk = crypto::parse_psk(&code).expect("just-generated code must parse");

    // The *effective* policy, so an unconfirmed `open` is never advertised as
    // open — the same gate #432 put on the stored record and #433 put on the
    // LAN datagram.
    let effective = hive.effective_join_policy();
    let hive_link = super::invite::build_hive_invite_link(
        &hive.hive_id,
        identity.as_str(),
        &addr,
        &canonical_psk,
        Some(&hive.name),
        Some(effective.as_str()),
    );
    let relay_code = super::invite::encode_relay_code(&addr, &canonical_psk);
    let word_phrase = super::invite::encode_words(&addr, &canonical_psk);
    let listening = relay_is_listening(port);

    if json_mode {
        let output = serde_json::json!({
            "identity": identity.as_str(),
            "hive_id": hive.hive_id,
            "hive_name": hive.name,
            "join_policy": effective.as_str(),
            "hive_link": hive_link,
            "relay_code": relay_code,
            "word_phrase": word_phrase,
            "addr": addr.to_string(),
            "relay_listening": listening,
        });
        println!("{}", serde_json::to_string_pretty(&output).unwrap());
    } else {
        println!(
            "Inviting to hive \"{}\" ({}), join_policy={}",
            hive.name,
            hive.hive_id,
            effective.as_str()
        );
        if effective == crate::hive::identity::JoinPolicy::Ask {
            println!("  Each join will wait for you to approve it (claudectl hive requests).");
        }
        println!();
        println!("  HIVE LINK:   {hive_link}");
        println!();
        println!("  RELAY CODE:  {relay_code}");
        if show_words {
            println!("  WORD PHRASE: {word_phrase}");
        }
        println!("    (the code and the phrase pair them with this machine; only");
        println!("     the link names the hive — either way they end up asking to join)");
        println!();
        println!("They run:");
        println!();
        println!("  claudectl hive join {hive_link}");
        println!("  claudectl hive join {relay_code}");
        if show_words {
            println!("  claudectl hive join {word_phrase}");
        }
        println!();
        if show_qr {
            println!("QR Code (scan to join):");
            println!();
            println!("{}", super::invite::render_qr(&hive_link));
        }
        if !listening {
            println!(
                "warning: nothing is listening on port {port}, so this invite cannot be\n\
                 redeemed yet. Start the relay with `claudectl relay serve`, or install it\n\
                 to survive logout with `claudectl relay install-agent`."
            );
        }
    }

    // The serve side claims this on first contact, exactly as `relay invite`.
    let pending_path = super::peers_dir().join("_pending.key");
    let _ = std::fs::create_dir_all(super::peers_dir());
    let _ = std::fs::write(&pending_path, crypto::hex_encode(&canonical_psk));

    Ok(())
}

/// How long `hive join` waits for the host to answer before giving up.
///
/// The host answers from its serve loop, which ticks once a second, so this is
/// generous rather than tight — and a silence here is reported as a silence, not
/// as a refusal.
#[cfg(feature = "hive")]
const JOIN_REPLY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(12);

/// `claudectl hive join <link|code|phrase>`
///
/// Pairs with the host, asks to join its hive, and reports what the host said.
/// Deliberately short-lived: it does not stay in the gossip loop, because
/// membership is a stored fact rather than a property of this process.
#[cfg(feature = "hive")]
pub fn cmd_hive_join(input: &[String], grant: Option<&str>) -> io::Result<()> {
    if input.is_empty() {
        eprintln!("Usage: claudectl hive join <hive-link | relay-code | word-phrase>");
        return Err(io::Error::other("missing argument"));
    }

    let input = input.join(" ");
    let identity = load_or_create_identity();

    // All three formats, plus a plain peer link — someone handed a peer invite
    // and told to join a hive should get the hive, not a parse error.
    let (addr, psk, expected_identity, asked_hive, asked_name) =
        if super::invite::is_hive_invite_link(&input) {
            let inv = super::invite::parse_hive_invite_link(&input)
                .map_err(|e| io::Error::other(format!("invalid hive link: {e}")))?;
            println!(
                "Hive \"{}\" ({}){}",
                inv.name.as_deref().unwrap_or("?"),
                inv.hive_id,
                inv.policy
                    .as_deref()
                    .map(|p| format!(", join_policy={p}"))
                    .unwrap_or_default()
            );
            (
                inv.addr,
                inv.psk,
                Some(inv.identity),
                Some(inv.hive_id),
                inv.name,
            )
        } else if input.starts_with("cctl://") {
            let (id, addr, psk) = super::invite::parse_invite_link(&input)
                .map_err(|e| io::Error::other(format!("invalid invite link: {e}")))?;
            (addr, psk, Some(id), None, None)
        } else if input.contains('-')
            && input
                .split('-')
                .all(|w| w.len() <= 5 && w.chars().all(|c| c.is_ascii_alphabetic()))
        {
            let (addr, psk) = super::invite::decode_words(&input)
                .map_err(|e| io::Error::other(format!("invalid word phrase: {e}")))?;
            (addr, psk, None, None, None)
        } else {
            let (addr, psk) = super::invite::decode_relay_code(&input)
                .map_err(|e| io::Error::other(format!("invalid relay code: {e}")))?;
            (addr, psk, None, None, None)
        };

    println!("Connecting to {addr}...");
    let (remote_id, registry) = try_connect(addr, &psk, &identity)
        .map_err(|e| io::Error::other(format!("connection failed: {e}")))?;

    if let Some(ref expected) = expected_identity {
        if remote_id != *expected {
            println!("Warning: expected peer '{expected}' but connected to '{remote_id}'");
        }
    }
    println!("Paired with {remote_id} ({addr})");

    // Pairing is worth keeping even if the hive request goes nowhere — it is
    // what `relay connect` will use next time.
    let _ = save_peer_psk(&remote_id, &psk);
    let _ = super::save_peer_meta(&remote_id, &addr.to_string());

    let request = super::hivejoin::build_join_request(
        identity.as_str(),
        asked_hive.as_deref(),
        Some(identity.as_str()),
        grant,
    );
    {
        let reg = registry
            .lock()
            .map_err(|_| io::Error::other("registry lock poisoned"))?;
        reg.send_to(remote_id.as_str(), &request)
            .map_err(|e| io::Error::other(format!("could not send the join request: {e}")))?;
    }
    if grant.is_some() {
        println!("Presenting a hive.read grant — asking to join as a reader.");
    }
    println!(
        "Asking to join{}...",
        asked_name
            .as_deref()
            .map(|n| format!(" \"{n}\""))
            .unwrap_or_default()
    );

    // Wait for the answer. A dropped connection here is the interesting case:
    // an older host errors on an unknown message type and closes, so silence
    // plus a disconnect is reported as a possible version mismatch rather than
    // as a refusal.
    let deadline = std::time::Instant::now() + JOIN_REPLY_TIMEOUT;
    let mut answered = false;
    while std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(200));
        let drained = {
            match registry.lock() {
                Ok(mut reg) => reg.drain_messages(),
                Err(_) => break,
            }
        };
        for (peer, msg) in drained {
            if msg.msg_type != super::MessageType::HiveJoinResult {
                continue;
            }
            match crate::hive::cli::apply_join_result(peer.as_str(), &msg.payload) {
                Ok(Some(line)) => println!("{line}"),
                Ok(None) => {}
                Err(e) => return Err(io::Error::other(e)),
            }
            answered = true;
        }
        if answered {
            break;
        }
    }

    if !answered {
        return Err(io::Error::other(format!(
            "paired with {remote_id}, but it never answered the join request.\n\
             Either it is not running a named hive, or it is an older claudectl that \
             does not understand hive membership (before v0.66.0) — a host that cannot \
             parse the request closes the connection rather than replying.\n\
             The pairing is saved, so `claudectl relay connect {addr}` still works."
        )));
    }

    // Membership is stored; gossip is someone else's loop. `relay serve` only
    // redials peers it has already lost, so the joiner is the side that has to
    // dial out for knowledge to actually flow.
    println!();
    println!("To start exchanging knowledge, connect to the hive:");
    println!();
    println!("  claudectl relay connect {addr}");
    println!();
    println!("Check where you stand any time with `claudectl hive status`.");

    Ok(())
}

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

    // Claim the pending key *before* the `--json` early return below. It used to
    // happen only at the end of the human-readable path, so `relay invite --json`
    // printed a perfectly valid invite that the serve side had no key for — every
    // scripted pairing failed with "unknown peer".
    let pending_path = super::peers_dir().join("_pending.key");
    let _ = std::fs::create_dir_all(super::peers_dir());
    let _ = std::fs::write(&pending_path, crypto::hex_encode(&canonical_psk));

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

    println!(
        "Scanning LAN for claudectl instances ({} seconds)...",
        super::lan::SCAN_DURATION.as_secs()
    );
    println!();

    let peers = super::lan::scan_lan(super::lan::SCAN_DURATION, identity.as_str());

    if json_mode {
        let json_peers: Vec<serde_json::Value> = peers
            .iter()
            .map(|p| {
                serde_json::json!({
                    "identity": p.identity,
                    "addr": p.relay_addr().to_string(),
                    "version": p.version,
                    // Null rather than omitted, so a consumer can tell "no hive"
                    // from "field this build does not emit".
                    "hive": p.hive.as_ref().map(|h| serde_json::json!({
                        "id": h.id,
                        "name": h.name,
                        "join_policy": h.join_policy,
                        "peers": h.peers,
                        "units": h.units,
                    })),
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
        println!(
            "  {:<20} {:<22} {:<16} VERSION",
            "IDENTITY", "ADDRESS", "HIVE"
        );
        println!("  {}", "─".repeat(70));
        for peer in &peers {
            let paired = if load_peer_psk(&peer.identity).is_some() {
                " (paired)"
            } else {
                ""
            };
            // An em-dash for a machine whose hive is unnamed, which is the
            // default and not a problem.
            let hive = peer
                .hive
                .as_ref()
                .map(|h| claudectl_core::helpers::truncate_cell(&h.name, 16))
                .unwrap_or_else(|| "—".to_string());
            println!(
                "  {:<20} {:<22} {:<16} {}{}",
                peer.identity,
                peer.relay_addr().to_string(),
                hive,
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

// ────────────────────────────────────────────────────────────────────────────
// Tests
// ────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::join_host_port;
    use std::net::SocketAddr;

    #[test]
    fn ipv4_host_joins_plainly() {
        assert_eq!(join_host_port("127.0.0.1", 9876), "127.0.0.1:9876");
        assert_eq!(join_host_port("0.0.0.0", 9876), "0.0.0.0:9876");
    }

    #[test]
    fn bare_ipv6_literal_gets_brackets() {
        assert_eq!(join_host_port("::1", 9876), "[::1]:9876");
        assert_eq!(join_host_port("::", 9876), "[::]:9876");
    }

    #[test]
    fn already_bracketed_ipv6_is_left_alone() {
        assert_eq!(join_host_port("[::1]", 9876), "[::1]:9876");
    }

    #[test]
    fn every_form_parses_as_a_socket_addr() {
        for host in ["127.0.0.1", "0.0.0.0", "::1", "::", "[::1]"] {
            let joined = join_host_port(host, 9876);
            assert!(
                joined.parse::<SocketAddr>().is_ok(),
                "{host} joined to {joined} should parse"
            );
        }
    }
}

/// The hive membership gate (#434).
///
/// These are the teeth of `join_policy`: a peer that is only *pending* must
/// neither receive knowledge nor be able to push any. Policy correctness is
/// tested in `relay::hivejoin`; what is tested here is that the gate the serve
/// loop actually calls agrees with the roster.
#[cfg(all(test, feature = "hive"))]
mod hive_gate_tests {
    use super::*;
    use crate::hive::membership::{Admission, Roster};

    fn peers(ids: &[&str]) -> Vec<super::super::PeerId> {
        ids.iter()
            .map(|s| super::super::PeerId(s.to_string()))
            .collect()
    }

    fn names(v: &[super::super::PeerId]) -> Vec<&str> {
        v.iter().map(|p| p.as_str()).collect()
    }

    #[test]
    fn an_unnamed_hive_gates_nothing() {
        // The no-regression case: everyone who never named a hive keeps the
        // pre-#434 behaviour exactly.
        let connected = peers(&["a-1", "b-2"]);
        assert_eq!(
            names(&hive_gossip_targets(None, connected.clone())),
            vec!["a-1", "b-2"]
        );
        assert!(hive_may_contribute(None, "a-1"));
        assert!(hive_may_receive(None, "a-1"));
        assert!(hive_may_contribute(None, "nobody-ever-heard-of"));
    }

    #[test]
    fn a_named_hive_sends_only_to_members() {
        let tmp = tempfile::tempdir().unwrap();
        let r = Roster::at(tmp.path());
        r.admit("a-1", "hv_1", Admission::Policy).unwrap();

        let connected = peers(&["a-1", "b-2", "c-3"]);
        assert_eq!(
            names(&hive_gossip_targets(Some(&r), connected)),
            vec!["a-1"],
            "only the admitted peer is a sync target"
        );
    }

    #[test]
    fn a_pending_peer_is_gated_in_both_directions() {
        let tmp = tempfile::tempdir().unwrap();
        let r = Roster::at(tmp.path());
        r.record_request("b-2", "hv_1", None).unwrap();

        // Outbound: not a target.
        assert!(
            hive_gossip_targets(Some(&r), peers(&["b-2"])).is_empty(),
            "a pending peer must not be sent knowledge"
        );
        // Inbound: its units are dropped. Gating only one direction would let an
        // unapproved peer poison the hive while receiving nothing.
        assert!(
            !hive_may_contribute(Some(&r), "b-2"),
            "a pending peer must not be able to contribute either"
        );
    }

    #[test]
    fn approving_opens_the_gate_the_serve_loop_reads() {
        let tmp = tempfile::tempdir().unwrap();
        let r = Roster::at(tmp.path());
        let entry = r.record_request("b-2", "hv_1", None).unwrap();
        assert!(!hive_may_contribute(Some(&r), "b-2"));

        r.admit(&entry.peer_id, "hv_1", Admission::Approved)
            .unwrap();

        assert!(hive_may_contribute(Some(&r), "b-2"));
        assert_eq!(
            names(&hive_gossip_targets(Some(&r), peers(&["b-2"]))),
            vec!["b-2"]
        );
    }

    #[test]
    fn a_denied_peer_stays_gated() {
        let tmp = tempfile::tempdir().unwrap();
        let r = Roster::at(tmp.path());
        let entry = r.record_request("b-2", "hv_1", None).unwrap();
        r.deny(&entry).unwrap();
        assert!(!hive_may_contribute(Some(&r), "b-2"));
        assert!(hive_gossip_targets(Some(&r), peers(&["b-2"])).is_empty());
    }

    #[test]
    fn a_reader_receives_but_may_not_contribute() {
        let tmp = tempfile::tempdir().unwrap();
        let r = Roster::at(tmp.path());
        r.admit_as(
            "reader-1",
            "hv_1",
            crate::hive::membership::Admission::Grant,
            crate::hive::membership::Role::Reader,
            Some("g_abc".into()),
        )
        .unwrap();

        // This asymmetry is §7.5: participation without symmetry.
        assert!(
            hive_may_receive(Some(&r), "reader-1"),
            "a reader must receive knowledge — that is what it is for"
        );
        assert!(
            !hive_may_contribute(Some(&r), "reader-1"),
            "a reader must never be able to contribute"
        );
        // And it is a sync target, unlike a pending peer.
        assert_eq!(
            names(&hive_gossip_targets(Some(&r), peers(&["reader-1"]))),
            vec!["reader-1"]
        );
    }

    #[test]
    fn a_contributor_and_a_reader_are_both_targets_but_only_one_may_push() {
        let tmp = tempfile::tempdir().unwrap();
        let r = Roster::at(tmp.path());
        r.admit(
            "writer-1",
            "hv_1",
            crate::hive::membership::Admission::Policy,
        )
        .unwrap();
        r.admit_as(
            "reader-1",
            "hv_1",
            crate::hive::membership::Admission::Grant,
            crate::hive::membership::Role::Reader,
            None,
        )
        .unwrap();

        let selected = hive_gossip_targets(Some(&r), peers(&["writer-1", "reader-1", "stranger"]));
        let mut targets = names(&selected);
        targets.sort();
        assert_eq!(targets, vec!["reader-1", "writer-1"]);

        assert!(hive_may_contribute(Some(&r), "writer-1"));
        assert!(!hive_may_contribute(Some(&r), "reader-1"));
    }

    #[test]
    fn an_unknown_peer_is_gated_by_a_named_hive() {
        // A peer may be paired without ever having asked to join — pairing is
        // not membership.
        let tmp = tempfile::tempdir().unwrap();
        let r = Roster::at(tmp.path());
        assert!(!hive_may_contribute(Some(&r), "never-asked"));
        assert!(!hive_may_receive(Some(&r), "never-asked"));
    }
}
