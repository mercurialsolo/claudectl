// CLI dispatch for relay subcommands.

use std::io;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use clap::Subcommand;
use claudectl_core::runtime::PeerLiveness;

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
        /// Also install the headless service: the supervisor reconciler and
        /// (with --brain) the brain, which otherwise only run while a terminal
        /// is open (#546).
        #[arg(long)]
        headless: bool,
        /// Enable the local-LLM brain in the headless service. Off by default;
        /// with no reachable endpoint the service warns and runs on without it.
        #[arg(long, requires = "headless")]
        brain: bool,
        /// Let the headless brain execute its suggestions without asking.
        /// Implies --brain.
        #[arg(long, requires = "headless")]
        auto_run: bool,
    },

    /// Remove the background services — relay and headless (#438, #546)
    UninstallAgent,

    /// Whether the background services are installed and running (#438, #546)
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

    /// Forget paired peers that have not been heard from in a while. Lists
    /// them unless --yes is given
    Prune {
        /// How long unheard before a peer goes: 30m, 12h, 7d, 2w. A peer that
        /// never connected counts from when it was paired
        #[arg(long, default_value = "7d")]
        older_than: String,
        /// Forget them. Without this, only list what would go
        #[arg(long)]
        yes: bool,
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
        /// Refuse before sending if the peer does not advertise these, e.g.
        /// `cargo>=1.88,claude:auth` (#536).
        #[arg(long)]
        requires: Option<String>,
    },

    /// Move a conversation to a paired peer, so it can be resumed there
    SendSession {
        /// Target peer ID
        peer: String,
        /// Session ID to send (the one `claude` reports as `session_id`)
        session: String,
        /// Working directory the session belongs to. Defaults to the current
        /// directory, which is where its transcript is looked for.
        #[arg(long)]
        cwd: Option<String>,
        /// Where the receiving host should place it. Defaults to `--cwd`,
        /// which will not exist on a peer running another OS.
        #[arg(long)]
        remote_cwd: Option<String>,
        /// Read this transcript instead of deriving its location from `--cwd`
        /// and the session id. The session id then comes from the file's own
        /// name. The TUI passes the path session discovery already resolved,
        /// which is right in cases the slug derivation misses.
        #[arg(long)]
        transcript: Option<std::path::PathBuf>,
    },

    /// Show conversations this host has sent to peers
    Sessions,

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
            headless,
            brain,
            auto_run,
        } => cmd_install_agent(
            super::agent::AgentConfig {
                port: *port,
                http_port: *http_port,
                http_addr: http_addr.clone(),
                auth_token: auth_token.clone(),
            },
            headless.then(|| super::agent::HeadlessConfig {
                brain: *brain,
                auto_run: *auto_run,
            }),
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
        RelayCommand::Prune { older_than, yes } => cmd_prune(older_than, *yes, json_mode),
        RelayCommand::Identity => cmd_identity(json_mode),
        RelayCommand::Delegate {
            peer,
            prompt,
            cwd,
            git_ref,
            requires,
        } => cmd_delegate(
            peer,
            prompt,
            cwd.as_deref(),
            git_ref.clone(),
            requires.as_deref(),
            json_mode,
        ),
        RelayCommand::SendSession {
            peer,
            session,
            cwd,
            remote_cwd,
            transcript,
        } => cmd_send_session(
            peer,
            session,
            cwd.as_deref(),
            remote_cwd.as_deref(),
            transcript.as_deref(),
            json_mode,
        ),
        RelayCommand::Sessions => cmd_sessions(json_mode),
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
        identity.as_str(),
    )));
    // #536: probe this machine once and advertise it on every heartbeat, so a
    // delegator can refuse work this host cannot run before spending anything.
    if let Ok(mut reg) = registry.lock() {
        let caps = super::capabilities::local();
        println!("Capabilities: {}", caps.summary());
        if let Ok(value) = serde_json::to_value(&caps) {
            reg.set_local_capabilities(value);
        }
        // #541: where we can be reached, so a peer that loses the path it has
        // can try another instead of retrying a dead address forever.
        let addrs = super::netif::advertised_addrs(port);
        if !addrs.is_empty() {
            println!("Reachable at: {}", addrs.join(", "));
        }
        reg.set_local_addrs(addrs);
    }
    // #571: room for another task, measured on every heartbeat (not once, like
    // the capabilities) because placement acts on it. Running tasks are the
    // supervisor's sessions plus delegated work this daemon is running.
    let delegated_running = Arc::new(std::sync::atomic::AtomicU32::new(0));
    if let Ok(mut reg) = registry.lock() {
        let tags = relay_cfg.tags.clone();
        let delegated = Arc::clone(&delegated_running);
        reg.set_headroom_source(Box::new(move || {
            let running =
                delegated.load(std::sync::atomic::Ordering::Relaxed) + supervisor_running();
            let disk = super::relay_dir();
            serde_json::to_value(super::headroom::local(
                &tags,
                &disk,
                running,
                super::worker::local_max_tasks(),
            ))
            .unwrap_or(serde_json::Value::Null)
        }));
    }
    // Write handles for in-flight one-shot deliveries (#511). Beside the
    // registry, never in it.
    let replies = super::transient::TransientReplies::new();
    let listener = RelayListener::start(
        addr,
        Arc::clone(&registry),
        identity.clone(),
        relay_cfg.max_peers,
        replies.clone(),
    )?;

    println!("Relay listening on {} as {}", listener.addr, identity);

    // Fleet leases (#569): whether this relay answers lease requests.
    #[cfg(feature = "coord")]
    let lease_authority =
        super::lease::Authority::new(relay_cfg.lease_authority.as_deref(), identity.as_str());
    #[cfg(feature = "coord")]
    match relay_cfg.lease_authority.as_deref() {
        Some(a) if lease_authority.is_authority() => {
            println!("Fleet leases: this machine is the lease authority ({a})");
        }
        Some(a) => println!("Fleet leases: the lease authority is {a}"),
        None => {}
    }

    // #464: listening is not enough. Two machines that both only serve would
    // each wait for the other forever, so dial the peers we already know.
    // Advertise our listening port so peers can dial us back (#484), but
    // only when we are bound to a wildcard address. Bound to one specific
    // address, the port is not reachable at the address a peer observes us
    // from, and advertising it would hand out an address that cannot work.
    let advertised_port = if addr.ip().is_unspecified() {
        Some(listener.addr.port())
    } else {
        None
    };
    spawn_startup_dials(Arc::clone(&registry), identity.clone(), advertised_port);

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

    // The distillation signal. `set_broadcast_channel` installs a
    // process-global sender, so only this loop may create one — the dialling
    // loop gets its lower-latency equivalent from the periodic tick instead.
    #[cfg(feature = "hive")]
    let broadcast_rx = crate::hive::is_active(Some(&hive_cfg)).then(|| {
        let (tx, rx) = std::sync::mpsc::channel::<u32>();
        crate::hive::set_broadcast_channel(tx);
        rx
    });

    // Block on Ctrl+C
    let running = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let r = Arc::clone(&running);
    let _ = ctrlc::set_handler(move || {
        r.store(false, std::sync::atomic::Ordering::Relaxed);
    });

    // LAN discovery. `relay discover` has always *scanned* for announcements,
    // but nothing ever sent one — `start_announcer` had no callers anywhere, and
    // the module-level `#![allow(dead_code)]` that used to sit on
    // `relay/mod.rs` kept that quiet. So `discover` returned "no instances
    // found" even with a relay running next to it, while telling the operator
    // to start the thing that was already running (#433). That allow is gone
    // as of #465, so the next one of these is a build failure.
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
    // Only ever stored to and read under `hive`, unlike `hive_peers` above,
    // which the serve loop updates in every build.
    #[cfg(feature = "hive")]
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
    // Store, engine and both membership gates. The gate is on only while a
    // hive is named: unnamed means every paired peer is treated exactly as it
    // was before #434, so nobody's existing setup goes quiet on upgrade.
    #[cfg(feature = "hive")]
    let mut hive =
        super::hivesync::HiveSync::new(&hive_cfg, identity.as_str(), hive_identity.is_some());

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

    // #506: listen as well as announce. `start_announcer` tells the LAN where
    // we are; without a listener this machine never hears anyone else move, so
    // a paired peer that changed network stayed unreachable until a human ran
    // `relay discover` at the right moment. Started whenever announcing is on,
    // since a machine unwilling to say where it is has no claim on others.
    let lan_seen = if relay_cfg.lan_announce {
        match super::lan::start_listener(identity.0.clone(), true) {
            Ok(seen) => Some(seen),
            Err(e) => {
                // Announcing still works without it; say so rather than
                // serving with a listener that silently never heard anyone.
                eprintln!(
                    "LAN discovery: cannot listen on UDP {}: {e} — peers that move will not be re-found",
                    super::lan::LAN_PORT
                );
                None
            }
        }
    } else {
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
            #[cfg(feature = "hive")]
            let connected_now = reg.connected_peers();
            for (from_peer, msg) in messages {
                // #455: the gossip arms live in one module both loops call.
                // Keeping a copy in each is what let the dialling loop quietly
                // have none at all.
                #[cfg(feature = "hive")]
                if let Some(out) =
                    hive.handle_inbound(&from_peer, &msg, identity.as_str(), &connected_now)
                {
                    for (target, out_msg) in out {
                        let _ = reg.send_to(target.as_str(), &out_msg);
                    }
                    continue;
                }
                // Fleet leases (#569). A request arrives on a one-shot
                // connection, so the answer goes back on that connection,
                // with the registry as the fallback for a lasting link.
                #[cfg(feature = "coord")]
                if super::lease::is_request(msg.msg_type) {
                    let answer =
                        lease_authority.answer(&from_peer, &msg, local_feed.sessions(), &reg);
                    let reply = super::lease::result_message(identity.as_str(), answer);
                    if let Err(e) = replies
                        .reply(&from_peer, &reply)
                        .or_else(|_| reg.send_to(from_peer.as_str(), &reply))
                    {
                        println!(
                            "[{}] could not answer a lease request from {from_peer}: {e}",
                            crate::logger::timestamp_now()
                        );
                    }
                    continue;
                }
                match msg.msg_type {
                    super::MessageType::Heartbeat => {
                        reg.handle_heartbeat(&from_peer, &msg.payload);
                        persist_peer_capabilities(from_peer.as_str(), &msg.payload);
                        let _ = super::headroom::record(
                            from_peer.as_str(),
                            &msg.payload,
                            super::epoch_ms(),
                        );
                        record_advertised_addrs(from_peer.as_str(), &msg.payload);
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
                                        if let Err(e) = reg.send_to(from_peer.as_str(), &status_msg)
                                        {
                                            eprintln!("  Could not acknowledge {task_id}: {e}");
                                        }
                                    }
                                    Err(e) => {
                                        // #491: this used to print and send
                                        // nothing, so a task that could not
                                        // start — an unreadable cwd, `claude`
                                        // missing from PATH — left the
                                        // delegating host waiting forever, with
                                        // the reason on the wrong machine's
                                        // console. Every other outcome travels
                                        // back over the relay; this one must too.
                                        eprintln!("  Failed to accept task: {e}");
                                        let msg = super::delegation::build_report_message(
                                            &task_id,
                                            &super::delegation::TaskReport {
                                                failed: true,
                                                summary: super::outcome::cap_summary(&format!(
                                                    "Could not start the task: {e}"
                                                )),
                                                total_cost_usd: 0.0,
                                                total_tokens: 0,
                                                model: None,
                                                usage: serde_json::json!({}),
                                                branch: None,
                                                verifications: Vec::new(),
                                                refused: true,
                                            },
                                            identity.as_str(),
                                        );
                                        if let Err(e) = reg.send_to(from_peer.as_str(), &msg) {
                                            eprintln!(
                                                "  Could not report the failure of {task_id}: {e}"
                                            );
                                        }
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
                                if let Some(resp) = worker.handle_interrupt(
                                    &task_id,
                                    &itype,
                                    &reason,
                                    from_peer.as_str(),
                                ) {
                                    let _ = reg.send_to(from_peer.as_str(), &resp);
                                }
                            }
                            Err(e) => eprintln!("  Bad TaskInterrupt message: {e}"),
                        }
                    }
                    super::MessageType::TaskStatus | super::MessageType::TaskHandoff => {
                        // Until #490 this arm printed the message and dropped
                        // it, so a delegated task's outcome reached a console
                        // and nothing else.
                        let is_handoff = msg.msg_type == super::MessageType::TaskHandoff;
                        let ts = crate::logger::timestamp_now();
                        match super::tasks::parse_report(is_handoff, &msg.payload) {
                            Some((task_id, report)) => {
                                let what = match super::tasks::apply(&task_id, &report) {
                                    super::tasks::Applied::Updated => {
                                        super::tasks::describe(&report)
                                    }
                                    // A settled task must not reopen: the mesh
                                    // dedups by message id, but a reconnect can
                                    // still replay one.
                                    super::tasks::Applied::AlreadySettled => {
                                        "already settled, ignored".to_string()
                                    }
                                    // Only tasks this host delegated belong in
                                    // the ledger; the sender may be stale.
                                    super::tasks::Applied::Unknown => {
                                        "not a task we delegated, ignored".to_string()
                                    }
                                };
                                println!("[{ts}] {task_id} from {from_peer}: {what}");
                            }
                            None => {
                                println!(
                                    "[{ts}] {:?} from {from_peer} carried no task id, ignored",
                                    msg.msg_type
                                );
                            }
                        }
                    }
                    // #478 item 3: a transcript arrives in chunks, because a
                    // median one is 5.5 MB against a 1 MiB frame cap.
                    super::MessageType::SessionTransfer => {
                        let ts = crate::logger::timestamp_now();
                        match super::transfer::parse_chunk(&msg.payload) {
                            Some(chunk) => {
                                // Placement decides whether the transcript can
                                // be resumed at all, so a missing target
                                // directory is reported, not guessed at. Same
                                // wording the worker uses for a delegated task
                                // with a cwd this host does not have.
                                if !std::path::Path::new(&chunk.cwd).is_dir() {
                                    println!(
                                        "[{ts}] session {} from {from_peer}: working directory does not exist on this host: {} (sender can pass --remote-cwd)",
                                        chunk.session_id, chunk.cwd
                                    );
                                } else {
                                    match super::transfer::accept_chunk(&chunk, &chunk.cwd) {
                                        Ok(super::transfer::Accepted::More { have, of }) => {
                                            println!(
                                                "[{ts}] session {} from {from_peer}: chunk {have}/{of}",
                                                chunk.session_id
                                            );
                                        }
                                        Ok(super::transfer::Accepted::Placed(path))
                                        | Ok(super::transfer::Accepted::AlreadyPresent(path)) => {
                                            println!(
                                                "[{ts}] session {} from {from_peer}: placed at {}",
                                                chunk.session_id,
                                                path.display()
                                            );
                                            println!(
                                                "       resume with: {}",
                                                super::transfer::resume_hint(
                                                    &chunk.cwd,
                                                    &chunk.session_id
                                                )
                                            );
                                            // The sender has no other way to
                                            // learn where it landed.
                                            //
                                            // A one-shot delivery is not in
                                            // the registry (#487), so the
                                            // registry cannot answer it —
                                            // that is what left every
                                            // transfer unacknowledged until
                                            // #511. Try the connection it
                                            // actually arrived on first, and
                                            // fall back to the registry for a
                                            // peer that holds a lasting link.
                                            let ack = super::transfer::build_received_message(
                                                &chunk.session_id,
                                                &path,
                                                &chunk.cwd,
                                                identity.as_str(),
                                            );
                                            let sent_ack = replies
                                                .reply(&from_peer, &ack)
                                                .or_else(|_| reg.send_to(from_peer.as_str(), &ack));
                                            if let Err(e) = sent_ack {
                                                println!(
                                                    "[{ts}] could not acknowledge session {} to {from_peer}: {e}",
                                                    chunk.session_id
                                                );
                                            }
                                        }
                                        Err(e) => {
                                            println!(
                                                "[{ts}] session {} from {from_peer}: {e}",
                                                chunk.session_id
                                            );
                                        }
                                    }
                                }
                            }
                            None => {
                                println!(
                                    "[{ts}] SessionTransfer from {from_peer} was malformed, ignored"
                                );
                            }
                        }
                    }
                    super::MessageType::SessionReceived => {
                        let ts = crate::logger::timestamp_now();
                        match super::transfer::parse_received(&msg.payload) {
                            Some((session_id, path, resume)) => {
                                if super::transfer::record_received(&session_id, &path, &resume) {
                                    println!(
                                        "[{ts}] {from_peer} has session {session_id} at {path}"
                                    );
                                    println!("       resume there with: {resume}");
                                } else {
                                    println!(
                                        "[{ts}] {from_peer} acknowledged session {session_id}, which this host did not send — ignored"
                                    );
                                }
                            }
                            None => {
                                println!(
                                    "[{ts}] SessionReceived from {from_peer} carried no session id, ignored"
                                );
                            }
                        }
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

            // Tick worker — send status updates back to controllers.
            // A discarded error here is how a lost handoff stayed invisible:
            // the task finished, the reply went nowhere, and nothing said so.
            let worker_msgs = worker.tick();
            delegated_running.store(worker.running_count(), std::sync::atomic::Ordering::Relaxed);
            for (target_peer, msg) in worker_msgs {
                if let Err(e) = reg.send_to(&target_peer, &msg) {
                    eprintln!(
                        "  Could not deliver {:?} to {target_peer}: {e}",
                        msg.msg_type
                    );
                }
            }

            // #465: settled tasks were never dropped from the worker's map, so
            // `relay serve` grew by one `WorkerTask` — strings, paths and a
            // `Child` handle — for every task it had ever run. An hour is well
            // past the point where a reply has been sent and the on-disk
            // ledger in `relay/tasks` has taken over as the record.
            worker.cleanup_finished(WORKER_TASK_RETENTION_SECS);

            // Brain distillation just produced something: say it now rather
            // than waiting out the tick below.
            #[cfg(feature = "hive")]
            if let Some(rx) = broadcast_rx.as_ref() {
                let mut distilled = false;
                while rx.try_recv().is_ok() {
                    distilled = true;
                }
                if distilled {
                    hive.refresh_membership();
                    for (target, sync_msg) in hive.sync(reg.connected_peers()) {
                        let _ = reg.send_to(target.as_str(), &sync_msg);
                    }
                }
            }

            // #455: and offer every eligible peer whatever it has not been sent
            // yet, on a timer. The engine is incremental, so this is a no-op
            // when there is nothing new — which is what lets one plain interval
            // stand in for sync-on-connect, catch-up after a reconnect, and the
            // units a peer missed while it was still waiting to be approved.
            #[cfg(feature = "hive")]
            for (target, sync_msg) in
                hive.sync_if_due(std::time::Instant::now(), reg.connected_peers())
            {
                let _ = reg.send_to(target.as_str(), &sync_msg);
            }

            // #506: a paired peer that moved announces itself from its new
            // address; record it so `reconnect_peer`'s existing fallback (#484)
            // has somewhere to go. Cheap per tick — the map is small, and only
            // a genuinely new address writes anything.
            // The listener thread publishes what it hears on its own clock, so
            // `relay discover` reads that instead of binding beside us. See the
            // note in lan.rs.
            if let Some(seen) = lan_seen.as_ref() {
                relearn_moved_peers(&seen.peers);
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
                        match reconnect_peer(&mut reg, &id, addr, &identity, advertised_port) {
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
                // Renew fleet leases whose sessions are still reported, and
                // expire the rest (#569). A no-op unless this is the authority.
                #[cfg(feature = "coord")]
                lease_authority.sweep(local_feed.sessions(), &reg);
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
        if let Some(store) = hive.store() {
            hive_units.store(store.len() as u32, std::sync::atomic::Ordering::Relaxed);
        }
    }

    listener.stop();
    // Stop the LAN listener and wait for it, so `discovered.json` is retracted
    // before this process is gone (a quarter second at most).
    if let Some(seen) = lan_seen {
        seen.stop();
    }
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
/// `relay install-agent` (#438, and the headless service in #546).
///
/// The relay is always installed; `headless` adds the second service. Both are
/// idempotent — re-running after an upgrade or a port change is the supported
/// way to update them.
fn cmd_install_agent(
    cfg: super::agent::AgentConfig,
    headless: Option<super::agent::HeadlessConfig>,
    json_mode: bool,
) -> io::Result<()> {
    use super::agent::{Service, ServiceConfig};

    let relay = ServiceConfig::Relay(cfg.clone());
    let path = super::agent::install(&relay).map_err(io::Error::other)?;
    let (out_log, err_log) = super::agent::log_paths(Service::Relay);
    let loaded = super::agent::is_loaded(Service::Relay);

    // The headless install is reported but not fatal: a relay that installed
    // cleanly should not be rolled back because the second service failed.
    let headless_result = headless.as_ref().map(|h| {
        let cfg = ServiceConfig::Headless(h.clone());
        super::agent::install(&cfg).map(|p| {
            (
                p,
                super::agent::is_loaded(Service::Headless),
                super::agent::log_paths(Service::Headless),
            )
        })
    });

    if json_mode {
        let mut json = serde_json::json!({
            "installed": true,
            "loaded": loaded,
            "plist": path.display().to_string(),
            "label": super::agent::AGENT_LABEL,
            "port": cfg.port,
            "stdout_log": out_log.display().to_string(),
            "stderr_log": err_log.display().to_string(),
        });
        if let Some(result) = &headless_result {
            json["headless"] = match result {
                Ok((p, loaded, (out, err))) => serde_json::json!({
                    "installed": true,
                    "loaded": loaded,
                    "service": p.display().to_string(),
                    "label": super::agent::HEADLESS_LABEL,
                    "brain": headless.as_ref().map(|h| h.brain || h.auto_run),
                    "auto_run": headless.as_ref().map(|h| h.auto_run),
                    "stdout_log": out.display().to_string(),
                    "stderr_log": err.display().to_string(),
                    "heartbeat": claudectl_core::heartbeat::path().display().to_string(),
                }),
                Err(e) => serde_json::json!({"installed": false, "error": e}),
            };
        }
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

    match (&headless_result, headless.as_ref()) {
        (Some(Ok((p, hl_loaded, (hl_out, hl_err)))), Some(h)) => {
            println!();
            println!(
                "Installed the headless service (supervisor reconciler{}).",
                if h.brain || h.auto_run {
                    " + brain"
                } else {
                    ""
                }
            );
            println!();
            println!("  service:   {}", p.display());
            println!("  label:     {}", super::agent::HEADLESS_LABEL);
            println!("  logs:      {}", hl_out.display());
            println!("             {}", hl_err.display());
            println!(
                "  heartbeat: {}",
                claudectl_core::heartbeat::path().display()
            );
            println!();
            if *hl_loaded {
                println!("It is running now and ticks the supervisor even with no terminal open.");
            } else {
                println!("The service is written but is not reported as loaded.");
                println!("Check the error log above, then: claudectl relay agent-status");
            }
            if h.auto_run {
                println!();
                println!("Note: --auto-run lets the brain act without asking you first.");
            }
        }
        (Some(Err(e)), _) => {
            println!();
            println!("The relay agent installed, but the headless service did not: {e}");
            println!("Re-run with --headless once that is fixed; the relay is unaffected.");
        }
        _ => {}
    }

    println!();
    println!("Remove them with: claudectl relay uninstall-agent");
    Ok(())
}

/// `relay uninstall-agent` (#438, both services since #546).
///
/// Removes every service rather than taking a name: the command's job is "stop
/// claudectl from coming back at login", and leaving the second service
/// installed would not do that.
fn cmd_uninstall_agent(json_mode: bool) -> io::Result<()> {
    use super::agent::Service;

    let mut removed: Vec<(Service, bool, Option<String>)> = Vec::new();
    for service in Service::ALL {
        let (was_present, note) = super::agent::uninstall(service).map_err(io::Error::other)?;
        removed.push((service, was_present, note));
    }
    let any = removed.iter().any(|(_, present, _)| *present);

    if json_mode {
        // `removed` and `note` keep their old shape and meaning for the relay,
        // so anything scripted against #438 still reads the same answer.
        let relay = removed
            .iter()
            .find(|(s, _, _)| *s == Service::Relay)
            .expect("relay is in Service::ALL");
        let headless = removed
            .iter()
            .find(|(s, _, _)| *s == Service::Headless)
            .expect("headless is in Service::ALL");
        let json = serde_json::json!({
            "removed": relay.1,
            "note": relay.2,
            "headless": {"removed": headless.1, "note": headless.2},
            "any_removed": any,
        });
        println!(
            "{}",
            serde_json::to_string_pretty(&json).unwrap_or_default()
        );
        return Ok(());
    }

    if !any {
        println!("No background services were installed — nothing to remove.");
        return Ok(());
    }

    for (service, was_present, note) in &removed {
        if *was_present {
            println!(
                "Removed {}. It will not come back at login.",
                service.description()
            );
        }
        if let Some(n) = note {
            // The service file is gone either way; this only explains why
            // unloading was noisy, usually "it was not running".
            println!("  (service manager said: {n})");
            println!("  It was removed regardless, so nothing will restart it.");
        }
    }
    Ok(())
}

/// `relay agent-status` (#438, both services since #546).
fn cmd_agent_status(json_mode: bool) -> io::Result<()> {
    use super::agent::Service;

    let st = super::agent::status(Service::Relay);
    let (out_log, err_log) = super::agent::log_paths(Service::Relay);
    let hl = super::agent::status(Service::Headless);
    let (hl_out, hl_err) = super::agent::log_paths(Service::Headless);
    // Loaded answers "is it installed"; the heartbeat answers "is it working".
    // #546 was filed because only the first question was being asked.
    let hl_beat = claudectl_core::heartbeat::read();

    if json_mode {
        let json = serde_json::json!({
            "service_exists": st.service_exists,
            "loaded": st.loaded,
            "plist": st.service.display().to_string(),
            "label": super::agent::AGENT_LABEL,
            "stdout_log": out_log.display().to_string(),
            "stderr_log": err_log.display().to_string(),
            "headless": {
                "service_exists": hl.service_exists,
                "loaded": hl.loaded,
                "service": hl.service.display().to_string(),
                "label": super::agent::HEADLESS_LABEL,
                "stdout_log": hl_out.display().to_string(),
                "stderr_log": hl_err.display().to_string(),
                "heartbeat": claudectl_core::heartbeat::path().display().to_string(),
                "publishing": hl_beat.is_some(),
                "pid": hl_beat.as_ref().map(|b| b.pid),
            },
        });
        println!(
            "{}",
            serde_json::to_string_pretty(&json).unwrap_or_default()
        );
        return Ok(());
    }

    match (st.service_exists, st.loaded) {
        (true, true) => {
            println!("Relay agent: running.");
            println!("  plist: {}", st.service.display());
            println!("  logs:  {}", out_log.display());
        }
        (true, false) => {
            println!("Relay agent: installed but NOT running.");
            println!("  plist: {}", st.service.display());
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

    println!();
    match (hl.service_exists, hl.loaded) {
        (true, true) if hl_beat.is_some() => {
            println!("Headless service: running.");
            println!("  service: {}", hl.service.display());
            if let Some(b) = &hl_beat {
                println!("  pid:     {}", b.pid);
            }
            println!("  logs:    {}", hl_out.display());
        }
        (true, true) => {
            // Loaded but not publishing — alive and doing nothing, which is
            // the case a service manager cannot see.
            println!("Headless service: loaded, but it has stopped publishing.");
            println!(
                "  heartbeat: {}",
                claudectl_core::heartbeat::path().display()
            );
            println!("  error log: {}", hl_err.display());
            println!();
            println!("That is wedged, not running. Check the log, then re-install it.");
        }
        (true, false) => {
            println!("Headless service: installed but NOT running.");
            println!("  service:   {}", hl.service.display());
            println!("  error log: {}", hl_err.display());
        }
        (false, true) => {
            println!("Headless service: loaded, but its service file is gone.");
            println!("Clean it up with: claudectl relay uninstall-agent");
        }
        (false, false) => {
            println!("Headless service: not installed.");
            println!();
            println!("The supervisor and brain only run while a terminal is open. To keep");
            println!("them up: claudectl relay install-agent --headless");
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
    // #455: this loop used to handle heartbeats and join results and nothing
    // else, so `relay join` built a connection that could not carry knowledge
    // in either direction. It gets the same gossip module `relay serve` uses.
    #[cfg(feature = "hive")]
    let mut hive = {
        let cfg = crate::config::Config::load().hive.unwrap_or_default();
        let named = matches!(crate::hive::identity::load(), Ok(Some(_)));
        super::hivesync::HiveSync::new(&cfg, identity, named)
    };
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
            #[cfg(feature = "hive")]
            let connected_now = reg.connected_peers();
            for (peer_id, msg) in &messages {
                #[cfg(feature = "hive")]
                if let Some(out) =
                    hive.handle_inbound(peer_id, msg, identity.as_str(), &connected_now)
                {
                    for (target, out_msg) in out {
                        let _ = reg.send_to(target.as_str(), &out_msg);
                    }
                    continue;
                }
                match msg.msg_type {
                    super::MessageType::Heartbeat => {
                        reg.handle_heartbeat(peer_id, &msg.payload);
                        persist_peer_capabilities(peer_id.as_str(), &msg.payload);
                        record_advertised_addrs(peer_id.as_str(), &msg.payload);
                    }
                    // #434: the owner of a `join_policy: ask` hive usually
                    // approves long after `hive join` exited, so this is where
                    // most approvals actually land.
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
            // #455: the dialling side's only sync trigger. A distillation here
            // reaches the host on the next tick rather than immediately, which
            // is the cost of the broadcast channel being process-global.
            #[cfg(feature = "hive")]
            for (target, sync_msg) in
                hive.sync_if_due(std::time::Instant::now(), reg.connected_peers())
            {
                let _ = reg.send_to(target.as_str(), &sync_msg);
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
                        match reconnect_peer(&mut reg, &id, addr, &identity, None) {
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
pub(super) fn try_connect_with(
    addr: SocketAddr,
    psk: &[u8; 32],
    identity: &super::PeerId,
    intent: &super::protocol::DialIntent,
) -> Result<(String, Arc<Mutex<PeerRegistry>>), String> {
    let registry = Arc::new(Mutex::new(PeerRegistry::new(30, identity.as_str())));
    let tx = {
        let reg = registry.lock().unwrap();
        reg.message_tx()
    };

    let conn = PeerConnection::connect(addr, psk, identity, tx, intent)?;
    let remote_id = conn.peer_id.0.clone();
    if let Ok(mut reg) = registry.lock() {
        reg.add_peer(conn);
    }
    Ok((remote_id, registry))
}

/// Refuse a delegation the target cannot run, before anything goes on the wire
/// (#536).
///
/// Checked here rather than on the worker because the whole point is to spend
/// nothing: no message, no authenticated connection, no `claude` session whose
/// only output is "Not logged in".
///
/// Two cases deliberately pass: no `--requires` at all, and a peer that has
/// never advertised a manifest. The second is what keeps an older peer usable —
/// unknown is not the same as missing, and a refusal here would break setups
/// that work today. The failure mode is therefore one-directional: this can let
/// through work that then fails the old way, never block work that would have
/// succeeded.
fn check_requirements(peer_id: &str, requires: Option<&str>, json_mode: bool) -> io::Result<()> {
    let Some(spec) = requires else {
        return Ok(());
    };
    let reqs = super::capabilities::Requirement::parse_list(spec)
        .map_err(|e| io::Error::other(format!("invalid --requires: {e}")))?;
    if reqs.is_empty() {
        return Ok(());
    }
    let Some(caps) = super::capabilities::load_peer_capabilities(peer_id) else {
        if !json_mode {
            eprintln!("note: {peer_id} has not advertised its capabilities yet.");
            eprintln!("      Delegating without checking --requires.");
        }
        return Ok(());
    };
    let mut unmet = caps.unmet(&reqs);
    // #571: a name that isn't a probed tool is a tag. It used to pass
    // silently, as "never probed"; now it's checked against the tags the
    // peer advertises. A peer that advertises no headroom at all is an older
    // build, and stays let through, as above.
    let tags = super::headroom::load_all()
        .into_iter()
        .find(|r| r.peer == peer_id)
        .map(|r| r.advertised.tags);
    for req in &reqs {
        if super::capabilities::PROBED_TOOLS.contains(&req.tool.as_str()) {
            continue;
        }
        match &tags {
            Some(tags) if !tags.contains(&req.tool) => {
                unmet.push(format!(
                    "{} does not advertise the tag `{}`",
                    peer_id, req.tool
                ));
            }
            Some(_) => {}
            None if !json_mode => {
                eprintln!(
                    "note: {peer_id} advertises no tags; not checking `{}`.",
                    req.tool
                );
            }
            None => {}
        }
    }
    if unmet.is_empty() {
        return Ok(());
    }
    if json_mode {
        println!(
            "{}",
            serde_json::json!({
                "error": "requirements not met",
                "peer": peer_id,
                "unmet": unmet,
            })
        );
    } else {
        eprintln!("{peer_id} cannot run this task:");
        for miss in &unmet {
            eprintln!("  - {miss}");
        }
        eprintln!(
            "
Nothing was sent. `claudectl relay peers` shows what each peer advertises."
        );
    }
    Err(io::Error::other(format!(
        "{peer_id} does not meet: {}",
        unmet.join("; ")
    )))
}

/// Record a new address for any *already paired* peer heard on the LAN (#506).
///
/// Pairing is the filter: an announcement is unauthenticated, so anyone on the
/// network can claim any identity. Writing an address for a peer we hold no PSK
/// for would let a stranger seed candidates we later dial. For a paired peer the
/// cost of a wrong address is a failed dial and the next candidate, because the
/// PSK still has to check out — so this widens where we look, never who we will
/// talk to.
///
/// Only genuinely new addresses are written; `save_peer_meta` merges and keeps
/// the most recent at the front (#484).
fn relearn_moved_peers(
    seen: &Arc<Mutex<std::collections::HashMap<String, super::lan::DiscoveredPeer>>>,
) {
    let Ok(map) = seen.lock() else {
        return;
    };
    for (peer_id, found) in map.iter() {
        if found.is_stale() {
            continue;
        }
        if load_peer_psk(peer_id).is_none() {
            continue; // not paired — see above
        }
        let addr = found.relay_addr().to_string();
        let known = super::peer_addrs(load_peer_meta(peer_id).as_ref());
        if known.iter().any(|a| a.addr == addr) {
            continue;
        }
        match super::save_peer_meta(peer_id, &addr) {
            Ok(()) => println!("Learned new address for {peer_id}: {addr}"),
            Err(e) => eprintln!("warning: could not record {addr} for {peer_id}: {e}"),
        }
    }
}

/// Record the addresses a peer says it can be reached at (#541).
///
/// Only addresses that parse as a literal `address:port` are kept: there is no
/// resolver on the dial path, so a hostname is not something we could use, and
/// an unparseable entry from a peer should never reach the record.
///
/// Best-effort, like the manifest beside it: a heartbeat that cannot be
/// recorded must not disturb the loop that received it.
fn record_advertised_addrs(peer_id: &str, payload: &serde_json::Value) {
    let Some(list) = payload.get("listen_addrs").and_then(|v| v.as_array()) else {
        return;
    };
    let addrs: Vec<String> = list
        .iter()
        .filter_map(|v| v.as_str())
        .filter(|s| s.parse::<SocketAddr>().is_ok())
        .map(str::to_string)
        .collect();
    match super::merge_advertised_addrs(peer_id, &addrs) {
        Ok(0) => {}
        Ok(n) => println!("Learned {n} more address(es) for {peer_id}"),
        Err(e) => eprintln!("warning: could not record addresses for {peer_id}: {e}"),
    }
}

/// Write a peer's advertised manifest to disk so the standalone CLI commands
/// can see it (#536).
///
/// Best-effort by construction: a heartbeat that cannot be recorded must never
/// take down the loop that received it, and the only cost of a failed write is
/// a `delegate` that does not pre-check.
/// Supervisor sessions running on this machine. Zero when coord is not built
/// in, or there's no coord database, or it can't be read: headroom is
/// advisory, and a relay must not fail to heartbeat over it. Never creates
/// or migrates the database (see `open_existing_read_only`).
fn supervisor_running() -> u32 {
    #[cfg(feature = "coord")]
    {
        supervisor_running_in(&crate::coord::store::db_file())
    }
    #[cfg(not(feature = "coord"))]
    {
        0
    }
}

#[cfg(feature = "coord")]
fn supervisor_running_in(db: &std::path::Path) -> u32 {
    crate::coord::store::open_existing_read_only(db)
        .and_then(|c| crate::coord::tasks::running_spawned_attempts(&c).ok())
        .map_or(0, |a| a.len() as u32)
}

fn persist_peer_capabilities(peer_id: &str, payload: &serde_json::Value) {
    let Some(caps) = payload.get("capabilities") else {
        return;
    };
    if let Ok(parsed) = serde_json::from_value::<super::capabilities::Capabilities>(caps.clone()) {
        let _ = super::capabilities::save_peer_capabilities(peer_id, &parsed);
    }
}

/// What a successful invite dial yields: the resolved peer id, the registry
/// holding the live connection, and the PSK that actually authenticated.
type InviteDial = (String, Arc<Mutex<PeerRegistry>>, [u8; 32]);

/// Dial an invite, trying each PSK the host might accept.
///
/// Returns the PSK that authenticated alongside the connection, because the
/// caller must persist *that* one: saving the invite's key after connecting on
/// an existing pairing would overwrite a working pairing with one the host no
/// longer accepts, turning a recoverable refusal into a permanent one.
fn connect_with_invite(
    addr: SocketAddr,
    invite_psk: &[u8; 32],
    peer_id: Option<&str>,
    identity: &super::PeerId,
) -> Result<InviteDial, String> {
    let existing = super::existing_pairing_psk(peer_id, &addr.to_string());
    let candidates = super::invite_psk_candidates(invite_psk, existing);
    let mut last = String::new();
    for psk in &candidates {
        match try_connect(addr, psk, identity) {
            Ok((id, reg)) => return Ok((id, reg, *psk)),
            Err(e) => last = e,
        }
    }
    if candidates.len() > 1 {
        // Both the stored pairing and the invite were refused, so this is not
        // the shadowing case — say so, rather than sending the owner to
        // `relay forget` for a problem it will not fix.
        Err(format!(
            "{last} (tried the existing pairing for this host and the invite's own key)"
        ))
    } else {
        Err(last)
    }
}

/// Dial a peer for a lasting link. `relay connect` and `relay join` both go
/// on to run a loop on the connection, so neither is a one-shot.
fn try_connect(
    addr: SocketAddr,
    psk: &[u8; 32],
    identity: &super::PeerId,
) -> Result<(String, Arc<Mutex<PeerRegistry>>), String> {
    // No listener in either command, so there is no port to teach the peer; it
    // learns an address for us from our own `serve` dials instead.
    try_connect_with(
        addr,
        psk,
        identity,
        &super::protocol::DialIntent::peer_link(None),
    )
}

/// Open a one-shot authenticated connection to `peer_id`, send `msg`, and close
/// (#378). This is how `relay delegate`/`interrupt` actually reach a peer from a
/// standalone CLI process — it connects directly to the peer using the stored
/// PSK + address, independent of any running `relay serve` daemon. Returns the
/// resolved remote id on success; an `Err` here must surface as a non-zero exit
/// so scripts never mistake a built-but-unsent message for a delivered one.
/// Move a conversation to a peer (#478 item 3).
///
/// One-shot like `relay delegate`: this process sends and exits. The
/// receiver's acknowledgement — which carries the path it chose and the
/// command to resume there — arrives at whatever process is serving, so it is
/// written to the ledger rather than printed here. `relay sessions` reads it.
/// Everything `send-session` can establish without touching the network: that
/// the transcript exists and names a usable session id, that the peer is
/// paired, and that there is somewhere to dial it.
///
/// Extracted so the TUI's send hotkey (#510) gets these three failures
/// immediately and in the operator's own words. The hotkey detaches the
/// transfer itself — megabytes over the wire cannot run on the render thread —
/// and a detached child that exits 1 reports nothing back, so a spawn that
/// "succeeded" would otherwise be indistinguishable from a transfer that
/// worked. Returns the session id the bytes will travel under, which for an
/// explicit path is the file's own stem rather than the caller's belief.
pub(crate) fn send_session_preflight(
    peer: &str,
    transcript: Option<&std::path::Path>,
    source_cwd: &str,
    session: &str,
) -> Result<(String, std::path::PathBuf), String> {
    let (session_id, path) = super::transfer::resolve_source(transcript, source_cwd, session)?;
    if load_peer_psk(peer).is_none() {
        return Err(format!(
            "peer '{peer}' is not paired — run `claudectl relay pair` first"
        ));
    }
    if dial_candidates(peer).is_empty() {
        return Err(format!(
            "no stored address for peer '{peer}' — pair or connect first"
        ));
    }
    Ok((session_id, path))
}

fn cmd_send_session(
    peer: &str,
    session: &str,
    cwd: Option<&str>,
    remote_cwd: Option<&str>,
    transcript: Option<&std::path::Path>,
    json_mode: bool,
) -> io::Result<()> {
    let identity = load_or_create_identity();

    // Where the transcript is on this host.
    let source_cwd = match cwd {
        Some(c) => c.to_string(),
        None => std::env::current_dir()
            .map(|p| p.display().to_string())
            .map_err(|e| io::Error::other(format!("current directory: {e}")))?,
    };

    // One place owns these three refusals, so the hotkey and the command say
    // the same thing. `session` is what the caller asked for; `session_id` is
    // what the bytes actually are.
    let (session_id, path) =
        send_session_preflight(peer, transcript, &source_cwd, session).map_err(io::Error::other)?;
    let session = session_id.as_str();

    let contents = std::fs::read(&path).map_err(|e| {
        io::Error::other(format!(
            "no transcript for session '{session}' at {}: {e}",
            path.display()
        ))
    })?;

    // Where it should land over there. The sender's own path is the default
    // and is usually wrong across operating systems, hence --remote-cwd.
    let target_cwd = remote_cwd.unwrap_or(&source_cwd).to_string();

    let chunks =
        super::transfer::split(session, &target_cwd, &contents).map_err(io::Error::other)?;

    // On disk before the wire: the acknowledgement lands at a different
    // process, and has nothing to attach to if this was never recorded (#490).
    super::transfer::record_sent(session, peer, &target_cwd, contents.len() as u64)
        .map_err(io::Error::other)?;

    let psk = load_peer_psk(peer).ok_or_else(|| {
        io::Error::other(format!(
            "peer '{peer}' is not paired — run `claudectl relay pair` first"
        ))
    })?;
    let candidates = dial_candidates(peer);
    if candidates.is_empty() {
        return Err(io::Error::other(format!(
            "no stored address for peer '{peer}' — pair or connect first"
        )));
    }

    let intent = super::protocol::DialIntent::one_shot();
    let mut last_err = String::new();
    for addr in candidates {
        let (remote_id, registry) = match try_connect_with(addr, &psk, &identity, &intent) {
            Ok(v) => v,
            Err(e) => {
                last_err = e;
                continue;
            }
        };
        if remote_id != peer {
            return Err(io::Error::other(format!(
                "remote identity mismatch at {addr}: expected {peer}, got {remote_id}"
            )));
        }

        for chunk in &chunks {
            let msg = super::transfer::build_chunk_message(chunk, identity.as_str());
            registry
                .lock()
                .map_err(|_| io::Error::other("registry lock poisoned"))?
                .send_to(&remote_id, &msg)
                .map_err(|e| {
                    io::Error::other(format!(
                        "sending chunk {} of {}: {e}",
                        chunk.seq + 1,
                        chunk.total
                    ))
                })?;
        }

        // A written frame is not a read one. Half-close and wait for the peer
        // to drain the whole stream; a fixed sleep cannot stand in for this
        // when the payload is megabytes (#487 is the same mistake at one
        // frame). Allow a second per megabyte, floor of ten.
        let budget = std::time::Duration::from_secs(10 + (contents.len() as u64 / 1_048_576));
        let drained = registry
            .lock()
            .map_err(|_| io::Error::other("registry lock poisoned"))?
            .get_peer(&remote_id)
            .map(|c| c.finish_sending(budget));

        // Wait for the receiver to say where it landed (#511). It answers on
        // this same connection and writes the ack *before* dropping its
        // handle, so the ack is queued ahead of the EOF that ends our reader
        // — by the time the drain above returns, it is normally already here.
        //
        // An ack is a stronger signal than the drain: it means placed and
        // resumable, not merely read off the socket. So it is checked even
        // when the drain timed out, and only its absence falls back to
        // treating the drain as the verdict.
        let confirmed = await_session_ack(&registry, session, std::time::Duration::from_secs(5));

        if confirmed.is_none() {
            if let Some(Err(e)) = drained {
                return Err(io::Error::other(format!(
                    "peer '{peer}' did not take the whole transcript: {e}"
                )));
            }
        }

        let bytes = contents.len();
        if json_mode {
            println!(
                "{}",
                serde_json::json!({
                    "session_id": session,
                    "peer": remote_id,
                    "bytes": bytes,
                    "chunks": chunks.len(),
                    "remote_cwd": target_cwd,
                })
            );
        } else {
            println!(
                "Sent session {session} to {remote_id}: {:.2} MB in {} chunk(s)",
                bytes as f64 / 1_048_576.0,
                chunks.len()
            );
            match &confirmed {
                Some(ack) => {
                    println!("Placed on {remote_id} at {}", ack.path);
                    println!("Resume it there with: {}", ack.resume);
                }
                None => {
                    println!("Target directory on {remote_id}: {target_cwd}");
                    println!(
                        "Not acknowledged within 5s — the transfer may still have landed. \
                         Check `claudectl relay sessions` here, or the receiver's log."
                    );
                }
            }
        }
        return Ok(());
    }
    Err(io::Error::other(last_err))
}

/// Where the receiver put a transcript, as it reported back.
struct SessionAck {
    path: String,
    resume: String,
}

/// Wait for the receiver's acknowledgement of `session`, recording it in the
/// ledger when it lands (#511).
///
/// Polls rather than blocking on the channel because `PeerRegistry` owns the
/// `Receiver` and hands out only `try_recv_message` — the serve loop's drain
/// stays the one place that consumes in a loop.
///
/// Messages for other sessions are discarded: this is a one-shot command whose
/// connection carried exactly one transcript, and nothing else will read them.
fn await_session_ack(
    registry: &Arc<Mutex<PeerRegistry>>,
    session: &str,
    budget: std::time::Duration,
) -> Option<SessionAck> {
    let deadline = std::time::Instant::now() + budget;
    while std::time::Instant::now() < deadline {
        let next = registry.lock().ok().and_then(|r| r.try_recv_message());
        let Some((_, msg)) = next else {
            std::thread::sleep(std::time::Duration::from_millis(50));
            continue;
        };
        if msg.msg_type != super::MessageType::SessionReceived {
            continue;
        }
        let Some((acked, path, resume)) = super::transfer::parse_received(&msg.payload) else {
            continue;
        };
        if acked != session {
            continue;
        }
        super::transfer::record_received(&acked, &path, &resume);
        return Some(SessionAck { path, resume });
    }
    None
}

/// What this host has sent, and what came back (#478 item 3).
fn cmd_sessions(json_mode: bool) -> io::Result<()> {
    let records = super::transfer::list();
    if json_mode {
        println!("{}", serde_json::json!({ "transfers": records }));
        return Ok(());
    }
    if records.is_empty() {
        println!("No conversations sent from this host.");
        return Ok(());
    }
    println!(
        "{:<38} {:<16} {:<10} {:>10}  WHERE",
        "SESSION", "PEER", "STATE", "SIZE"
    );
    for r in &records {
        let get = |k: &str| r.get(k).and_then(|v| v.as_str()).unwrap_or("-").to_string();
        let bytes = r.get("bytes").and_then(|v| v.as_u64()).unwrap_or(0);
        println!(
            "{:<38} {:<16} {:<10} {:>9.2}M  {}",
            truncate(&get("session_id"), 37),
            truncate(&get("peer"), 15),
            get("state"),
            bytes as f64 / 1_048_576.0,
            r.get("remote_path")
                .and_then(|v| v.as_str())
                .unwrap_or(&get("cwd"))
        );
    }
    for r in &records {
        if let Some(cmd) = r.get("resume").and_then(|v| v.as_str()) {
            println!("\nResume on the peer: {cmd}");
            break;
        }
    }
    Ok(())
}

pub fn send_message_to_peer(
    peer_id: &str,
    identity: &super::PeerId,
    msg: &RelayMessage,
) -> Result<String, String> {
    send_message_requiring(peer_id, identity, msg, None)
}

/// `send_message_to_peer`, refusing before sending when the link didn't agree
/// `feature`: a peer that would ignore what the message relies on (#571).
pub fn send_message_requiring(
    peer_id: &str,
    identity: &super::PeerId,
    msg: &RelayMessage,
    feature: Option<&str>,
) -> Result<String, String> {
    let psk = load_peer_psk(peer_id).ok_or_else(|| {
        format!("peer '{peer_id}' is not paired — run `claudectl relay pair` first")
    })?;
    // Every address we have seen the peer at, not just the newest (#484):
    // sending to a peer that has moved should not fail when a working address
    // is on record.
    let candidates = dial_candidates(peer_id);
    if candidates.is_empty() {
        return Err(format!(
            "no stored address for peer '{peer_id}' — pair or connect first"
        ));
    }

    // A one-shot: this connection carries `msg` and closes. Saying so keeps the
    // acceptor from treating it as a rival to an existing peer link and closing
    // it before reading the message, which is #487.
    let intent = super::protocol::DialIntent::one_shot();

    let mut last_err = String::new();
    for addr in candidates {
        let (remote_id, registry) = match try_connect_with(addr, &psk, identity, &intent) {
            Ok(v) => v,
            Err(e) => {
                last_err = e;
                continue;
            }
        };
        if remote_id != peer_id {
            return Err(format!(
                "remote identity mismatch at {addr}: expected {peer_id}, got {remote_id}"
            ));
        }
        if let Some(f) = feature {
            let agreed = registry
                .lock()
                .map_err(|_| "registry lock poisoned".to_string())?
                .get_peer(&remote_id)
                .is_some_and(|c| c.features.has(f));
            if !agreed {
                return Err(format!(
                    "peer '{peer_id}' runs a claudectl without `{f}`; upgrade it"
                ));
            }
        }
        registry
            .lock()
            .map_err(|_| "registry lock poisoned".to_string())?
            .send_to(&remote_id, msg)?;

        // Let the frame flush before the connection drops at scope end.
        std::thread::sleep(std::time::Duration::from_millis(250));

        // A written frame is not a delivered one. If the peer closed on us in
        // that window, it did not read the message — which is exactly how #487
        // stayed invisible: the write succeeded, the peer hung up, and the
        // caller was told the task had been delegated.
        let delivered = registry
            .lock()
            .map(|reg| reg.get_peer(&remote_id).is_none_or(|c| c.reader_alive()))
            .unwrap_or(true);
        if !delivered {
            return Err(format!(
                "peer '{peer_id}' closed the connection without accepting the message"
            ));
        }
        return Ok(remote_id);
    }
    Err(last_err)
}

/// Every address we could dial this peer at, most-recently-seen first (#484).
///
/// A peer appears on more than one address — a LAN address and a VPN address,
/// say — and which of them works depends on where *we* are. Trying them in
/// order is what lets a pair survive one of them going away.
pub(super) fn dial_candidates(peer_id: &str) -> Vec<SocketAddr> {
    parse_candidates(super::load_peer_meta(peer_id).as_ref())
}

/// The dialable addresses in a peer record, in the order they are recorded.
///
/// Split from `dial_candidates` so it can be tested without a HOME to read
/// from. Anything that is not a literal `address:port` is dropped: there is no
/// resolver on this path, so a hostname is not something we could dial.
fn parse_candidates(meta: Option<&serde_json::Value>) -> Vec<SocketAddr> {
    let mut out: Vec<SocketAddr> = Vec::new();
    for known in super::peer_addrs(meta) {
        if let Ok(addr) = known.addr.parse::<SocketAddr>() {
            if !out.contains(&addr) {
                out.push(addr);
            }
        }
    }
    out
}

/// Dial every peer we have both a PSK and a stored address for (#464).
///
/// Without this, two machines that both run only `relay serve` never connect:
/// each listens, neither dials, and the registry's `ReconnectNeeded` event
/// cannot fire because a fresh process has no peers in it yet.
///
/// Runs on its own thread. `PeerConnection::connect` carries a 10s connect
/// timeout plus a blocking handshake, so dialling inline would hold the banner
/// for 10s per powered-off peer. The hive sync tick picks up whatever
/// connections appear, so nothing needs to be notified when this finishes.
///
/// Each peer's addresses are tried in turn, newest first, stopping at the
/// first that answers as the peer we expect (#484). A peer that has moved
/// networks fails on its old address and may answer on an older one we still
/// hold, which is the point of keeping more than one.
///
/// No dial/wait tie-break: both ends having an address is now the normal case,
/// since the listener records one too, so both dial and both accept.
/// `PeerRegistry::add_peer` resolves that collision deterministically (#480),
/// keeping the connection opened by the lower peer id.
fn spawn_startup_dials(
    registry: Arc<Mutex<PeerRegistry>>,
    identity: super::PeerId,
    listen_port: Option<u16>,
) {
    let targets: Vec<(String, Vec<SocketAddr>)> = list_known_peers()
        .into_iter()
        .map(|id| {
            let addrs = dial_candidates(&id);
            (id, addrs)
        })
        .filter(|(_, addrs)| !addrs.is_empty())
        .collect();
    if targets.is_empty() {
        return;
    }

    std::thread::spawn(move || {
        for (id, addrs) in targets {
            let peer_id = super::PeerId(id.clone());

            // Dial unconditionally, even if the peer appears to be connected
            // already. Checking first looks like a cheap way to avoid a
            // redundant socket, but it is a race: the peer is doing the same
            // thing at the same time, and both sides skipping on what the
            // other just did leaves the pair with no agreed connection.
            // `add_peer` resolves a genuine collision deterministically, so
            // the worst case here is one extra handshake per peer at startup.
            let tx = match registry.lock() {
                Ok(reg) => reg.message_tx(),
                Err(_) => return,
            };

            let psk = match load_peer_psk(&id) {
                Some(psk) => psk,
                None => {
                    println!("Peer {id} has no pairing key on disk; not dialling");
                    continue;
                }
            };

            // Try each address in turn, stopping at the first that answers as
            // the peer we expect. Connecting with the lock released: it would
            // otherwise be held for the whole 10s timeout per address,
            // stalling the serve loop's tick.
            let mut connected = None;
            let mut addr_used = None;
            for addr in &addrs {
                let conn = match PeerConnection::connect(
                    *addr,
                    &psk,
                    &identity,
                    tx.clone(),
                    &super::protocol::DialIntent::peer_link(listen_port),
                ) {
                    Ok(c) => c,
                    Err(e) => {
                        // Name the address as one we saw the peer at, or a
                        // peer that has simply moved is indistinguishable
                        // from a host that is off.
                        println!("Peer {id} not reachable at {addr}: {e}");
                        continue;
                    }
                };
                if conn.peer_id != peer_id {
                    println!(
                        "Peer {id} not dialled: {addr} answered as {} — that address \
                         now belongs to another host",
                        conn.peer_id
                    );
                    conn.shutdown();
                    continue;
                }
                addr_used = Some(*addr);
                connected = Some(conn);
                break;
            }
            let (Some(conn), Some(addr)) = (connected, addr_used) else {
                // #543: every address failed. Leave a placeholder so the
                // reconnect loop keeps trying — including any address learned
                // after startup (#506, #541) — instead of forgetting this peer
                // until the process restarts. A relay is usually a launchd
                // agent running for weeks, so "restart it" is not a fix.
                if let (Ok(mut reg), Some(first)) = (registry.lock(), addrs.first()) {
                    let mut pending = PeerConnection::pending(peer_id.clone(), Some(*first));
                    // Back off from the start rather than redialling on the
                    // very next tick, which for an unreachable host is a dial
                    // storm against a 10s connect timeout.
                    pending.schedule_reconnect();
                    reg.add_peer(pending);
                    println!("Peer {id} unreachable for now; will keep retrying");
                }
                continue;
            };

            let agreed = conn.features.describe();
            match registry.lock() {
                Ok(mut reg) => {
                    if reg.add_peer(conn) {
                        println!("Connected to {id} ({addr}; features: {agreed})");
                    } else {
                        // We and the peer dialled each other at the same
                        // moment and the rule kept theirs. Still connected,
                        // just not over our socket.
                        println!("Connected to {id} (it dialled us at the same moment)");
                    }
                }
                Err(_) => return,
            }
        }
    });
}

/// Reconnect an existing peer in a registry using its stored PSK.
fn reconnect_peer(
    reg: &mut PeerRegistry,
    peer_id: &super::PeerId,
    addr: Option<SocketAddr>,
    identity: &super::PeerId,
    listen_port: Option<u16>,
) -> Result<(), String> {
    let psk = load_peer_psk(peer_id.as_str()).ok_or("missing peer PSK")?;

    // The address the connection was last up on first, then every other
    // address we have seen this peer at (#484). A peer that moved networks
    // will fail on the first and may well answer on a later one.
    let mut candidates: Vec<SocketAddr> = addr.into_iter().collect();
    for known in dial_candidates(peer_id.as_str()) {
        if !candidates.contains(&known) {
            candidates.push(known);
        }
    }
    if candidates.is_empty() {
        return Err("no known address for peer".into());
    }

    let tx = reg.message_tx();
    let mut last_err = String::new();
    for addr in candidates {
        let conn = match PeerConnection::connect(
            addr,
            &psk,
            identity,
            tx.clone(),
            &super::protocol::DialIntent::peer_link(listen_port),
        ) {
            Ok(c) => c,
            Err(e) => {
                last_err = e;
                continue;
            }
        };
        if conn.peer_id != *peer_id {
            last_err = format!(
                "remote identity mismatch at {addr}: expected {}, got {}",
                peer_id, conn.peer_id
            );
            conn.shutdown();
            continue;
        }
        reg.add_peer(conn);
        return Ok(());
    }
    Err(last_err)
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
    let now = super::epoch_ms();
    let peers = peers_by_liveness(list_known_peers(), now);
    // #571: each peer's last advertised headroom, by peer id.
    let headroom: std::collections::HashMap<String, super::headroom::Received> =
        super::headroom::load_all()
            .into_iter()
            .map(|r| (r.peer.clone(), r))
            .collect();

    if json_mode {
        let peers: Vec<serde_json::Value> = peers
            .iter()
            .map(|(id, liveness)| {
                let meta = load_peer_meta(id).unwrap_or(serde_json::json!({}));
                serde_json::json!({
                    "peer_id": id,
                    "addr": meta.get("addr").and_then(|v| v.as_str()).unwrap_or("unknown"),
                    "last_seen": meta.get("last_seen").and_then(|v| v.as_u64()).unwrap_or(0),
                    "status": liveness.status(),
                    "last_heard_secs_ago": match liveness {
                        PeerLiveness::Live { secs_ago } | PeerLiveness::Stale { secs_ago } => {
                            serde_json::json!(secs_ago)
                        }
                        PeerLiveness::NeverConnected => serde_json::Value::Null,
                    },
                    "has_psk": load_peer_psk(id).is_some(),
                    "headroom": headroom.get(id.as_str()),
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
        if peers.is_empty() {
            println!("No paired peers. Run 'claudectl relay pair' to get started.");
        } else {
            // Identities run to `<host>-<8 hex>`, past any fixed width.
            let w = peers
                .iter()
                .map(|(id, _)| id.len())
                .max()
                .unwrap_or(4)
                .max(4);
            println!(
                "{:<w$}  {:<6}  {:<16}  ADDRESS",
                "PEER", "STATUS", "LAST HEARD"
            );
            println!("{}", "─".repeat(w + 50));
            for (id, liveness) in &peers {
                let meta = load_peer_meta(id).unwrap_or(serde_json::json!({}));
                let addr = meta
                    .get("addr")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown");
                println!(
                    "{:<w$}  {:<6}  {:<16}  {}",
                    id,
                    liveness.status(),
                    liveness.last_heard_cell(),
                    addr
                );
                // #536: what that peer says it can run. Indented under the row
                // rather than given a column, because the manifest is long and
                // the table is already at the width of a terminal.
                if let Some(caps) = super::capabilities::load_peer_capabilities(id) {
                    println!("  └ {} ({}/{})", caps.summary(), caps.os, caps.arch);
                }
                if let Some(h) = headroom.get(id.as_str()) {
                    println!("  └ {}", headroom_line(h, now));
                }
            }
            let quiet = peers.iter().filter(|(_, l)| !l.is_live()).count();
            if quiet > 0 {
                println!();
                println!(
                    "{quiet} not heard from in {}s: the supervisor won't place work on them, \
                     and `delegate` / `send-session` will fail to reach them.",
                    claudectl_core::runtime::PEER_STALE_AFTER_SECS
                );
                println!(
                    "`claudectl relay prune` lists the long-gone ones; `relay forget <peer>` removes one."
                );
            }
        }
    }
    Ok(())
}

/// Paired peers with their liveness, live first, then by id.
fn peers_by_liveness(ids: Vec<String>, now_ms: u64) -> Vec<(String, PeerLiveness)> {
    let mut peers: Vec<(String, PeerLiveness)> = ids
        .into_iter()
        .map(|id| {
            let l = super::peer_liveness(&id, now_ms);
            (id, l)
        })
        .collect();
    peers.sort_by(|a, b| (!a.1.is_live(), &a.0).cmp(&(!b.1.is_live(), &b.0)));
    peers
}

/// One line for `relay peers`: free memory, load, disk, tasks, tags, age.
fn headroom_line(r: &super::headroom::Received, now_ms: u64) -> String {
    let h = &r.advertised.headroom;
    let or_dash = |v: Option<String>| v.unwrap_or_else(|| "-".into());
    let mut s = format!(
        "{} MB free · load {}/core · {} MB disk · {} running",
        or_dash(h.mem_free_mb.map(|m| m.to_string())),
        or_dash(h.load_per_core.map(|l| format!("{l:.2}"))),
        or_dash(h.disk_free_mb.map(|d| d.to_string())),
        h.running_tasks
    );
    if !r.advertised.tags.is_empty() {
        s.push_str(&format!(" · tags {}", r.advertised.tags.join(", ")));
    }
    s.push_str(&format!(
        " ({}s ago)",
        now_ms.saturating_sub(r.received_ms) / 1000
    ));
    s
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

/// A paired peer `relay prune` looked at, and how long it has been quiet.
#[derive(Debug, Clone, PartialEq)]
struct PruneCandidate {
    id: String,
    liveness: PeerLiveness,
    /// Seconds since its pairing key was written, for a peer that never
    /// connected. `None` when the key's age can't be read.
    paired_secs_ago: Option<u64>,
}

/// What `relay prune` would do: the peers to forget, and the quiet ones kept
/// because the hive depends on them, with the reason. Pure, so the rule is
/// testable without a peer store.
fn prune_plan(
    peers: &[PruneCandidate],
    older_than_secs: u64,
    protected: &std::collections::BTreeMap<String, String>,
) -> (Vec<String>, Vec<(String, String)>) {
    let mut forget = Vec::new();
    let mut kept = Vec::new();
    for p in peers {
        let quiet_secs = match p.liveness {
            PeerLiveness::Live { .. } => continue,
            PeerLiveness::Stale { secs_ago } => Some(secs_ago),
            // Never heard from: date it by the pairing. A key we can't date
            // is left alone rather than guessed old.
            PeerLiveness::NeverConnected => p.paired_secs_ago,
        };
        match quiet_secs {
            Some(q) if q >= older_than_secs => match protected.get(&p.id) {
                Some(why) => kept.push((p.id.clone(), why.clone())),
                None => forget.push(p.id.clone()),
            },
            _ => {}
        }
    }
    (forget, kept)
}

/// Peers the hive depends on, with why. Forgetting the machine you joined a
/// hive through, or one you admitted to yours, leaves the membership record
/// pointing at a peer this machine can no longer reach; that is `hive`'s to
/// undo, not a side effect of tidying the relay.
fn hive_protected_peers() -> io::Result<std::collections::BTreeMap<String, String>> {
    let mut out = std::collections::BTreeMap::new();
    // Unreadable membership fails the prune: guessing it empty could forget
    // the hive's host.
    let joined = crate::hive::membership::load_membership()
        .map_err(|e| io::Error::other(format!("read hive membership: {e}")))?;
    if let Some(m) = joined {
        let name = m.name.clone().unwrap_or_else(|| m.hive_id.clone());
        out.insert(
            m.joined_via.clone(),
            format!("you joined hive \"{name}\" through it"),
        );
    }
    for member in crate::hive::membership::list_members() {
        out.entry(member.peer_id)
            .or_insert_with(|| "a member of your hive".to_string());
    }
    Ok(out)
}

/// Seconds since `peer_id`'s pairing key was written.
fn paired_secs_ago(peer_id: &str) -> Option<u64> {
    let path = super::peers_dir().join(format!("{peer_id}.key"));
    let modified = std::fs::metadata(path).ok()?.modified().ok()?;
    std::time::SystemTime::now()
        .duration_since(modified)
        .ok()
        .map(|d| d.as_secs())
}

/// `claudectl relay prune [--older-than 7d] [--yes]`
fn cmd_prune(older_than: &str, yes: bool, json_mode: bool) -> io::Result<()> {
    let older_than_secs = claudectl_core::history::parse_duration(older_than).ok_or_else(|| {
        io::Error::other(format!(
            "invalid --older-than {older_than:?}: use a number and s, m, h, d or w (7d)"
        ))
    })?;
    let now = super::epoch_ms();
    let candidates: Vec<PruneCandidate> = peers_by_liveness(list_known_peers(), now)
        .into_iter()
        .map(|(id, liveness)| PruneCandidate {
            paired_secs_ago: paired_secs_ago(&id),
            id,
            liveness,
        })
        .collect();
    let protected = hive_protected_peers()?;
    let (forget, kept) = prune_plan(&candidates, older_than_secs, &protected);
    let quiet = |id: &str| {
        candidates
            .iter()
            .find(|c| c.id == id)
            .map(|c| c.liveness.last_heard())
            .unwrap_or_default()
    };

    if yes {
        for id in &forget {
            super::forget_peer(id);
        }
    }

    if json_mode {
        let out = serde_json::json!({
            "older_than_secs": older_than_secs,
            "forgotten": yes,
            "prune": forget.iter().map(|id| serde_json::json!({
                "peer_id": id, "last_heard": quiet(id),
            })).collect::<Vec<_>>(),
            "kept": kept.iter().map(|(id, why)| serde_json::json!({
                "peer_id": id, "last_heard": quiet(id), "reason": why,
            })).collect::<Vec<_>>(),
        });
        println!("{}", serde_json::to_string_pretty(&out).unwrap());
        return Ok(());
    }

    if forget.is_empty() && kept.is_empty() {
        println!("No paired peer has been quiet for {older_than} or longer.");
        return Ok(());
    }
    if !forget.is_empty() {
        println!(
            "{} {} quiet for {older_than} or longer:",
            if yes { "Forgot" } else { "Would forget" },
            if forget.len() == 1 {
                "1 peer"
            } else {
                "these peers"
            },
        );
        for id in &forget {
            println!("  {id}  ({})", quiet(id));
        }
    }
    if !kept.is_empty() {
        println!("Kept, though quiet as long:");
        for (id, why) in &kept {
            println!("  {id}  ({}): {why}", quiet(id));
        }
    }
    if !yes && !forget.is_empty() {
        println!();
        println!(
            "Run again with --yes to forget them. A forgotten peer needs a new invite to come back."
        );
    }
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
    requires: Option<&str>,
    json_mode: bool,
) -> io::Result<()> {
    let identity = load_or_create_identity();
    check_requirements(peer_id, requires, json_mode)?;
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

    // Record it only once it is away (#490). A task we could not hand over is
    // not a task in flight, and this process exits long before any reply
    // arrives, so the ledger on disk is the only thing that can connect the
    // two.
    if let Ok(remote) = &sent {
        if let Err(e) = super::tasks::record_delegated(&task_id, remote, prompt, cwd) {
            eprintln!("Warning: task {task_id} was delegated but not recorded: {e}");
        }
    }

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
///
/// This used to print `active_delegated_tasks: 0` and a note saying live status
/// needed `relay serve` — a constant, and so wrong in every state (#490). The
/// records are written by `relay delegate` and updated by the serve loop as
/// reports come back, so reading them here needs no live connection: that is
/// the whole point of the ledger being on disk.
fn cmd_task_status(json_mode: bool) -> io::Result<()> {
    let identity = load_or_create_identity();
    let tasks = super::tasks::list();
    let active = tasks
        .iter()
        .filter(|t| {
            !super::tasks::is_settled(t.get("state").and_then(|v| v.as_str()).unwrap_or_default())
        })
        .count();

    if json_mode {
        let output = serde_json::json!({
            "identity": identity.as_str(),
            "active_delegated_tasks": active,
            "tasks": tasks,
        });
        println!("{}", serde_json::to_string_pretty(&output).unwrap());
        return Ok(());
    }

    println!("Relay identity: {}", identity);
    println!();
    if tasks.is_empty() {
        println!("No delegated tasks on record.");
        println!("Delegate one with `claudectl relay delegate <peer> \"<prompt>\"`.");
        return Ok(());
    }

    println!("{:<28} {:<10} {:<22} DETAIL", "TASK", "STATE", "PEER");
    println!("{}", "─".repeat(92));
    for t in &tasks {
        let str_of = |k: &str| t.get(k).and_then(|v| v.as_str()).unwrap_or("").to_string();
        let state = str_of("state");
        // A settled task's summary says what happened; an open one's prompt is
        // the only thing there is to show.
        let detail = if super::tasks::is_settled(&state) {
            let cost = t
                .get("total_cost_usd")
                .and_then(|v| v.as_f64())
                .unwrap_or(0.0);
            let summary = str_of("summary");
            if cost > 0.0 {
                format!("{summary} (${cost:.4})")
            } else {
                summary
            }
        } else {
            str_of("prompt")
        };
        println!(
            "{:<28} {:<10} {:<22} {}",
            truncate(&str_of("task_id"), 28),
            state,
            truncate(&str_of("peer"), 22),
            truncate(&detail, 36)
        );
    }
    println!();
    println!("{active} active, {} on record.", tasks.len());

    Ok(())
}

/// Clip a cell so one long prompt cannot break the table.
fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let keep: String = s.chars().take(max.saturating_sub(1)).collect();
    format!("{keep}…")
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

/// How long a settled task stays in the worker's in-memory map before it is
/// dropped. The durable record is `relay/tasks/<id>.json` (#490), so this
/// only bounds memory.
const WORKER_TASK_RETENTION_SECS: u64 = 3600;

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
        } else if super::invite::looks_like_word_phrase(&input) {
            let (addr, psk) = super::invite::decode_words(&input)
                .map_err(|e| io::Error::other(format!("invalid word phrase: {e}")))?;
            (addr, psk, None, None, None)
        } else {
            let (addr, psk) = super::invite::decode_relay_code(&input)
                .map_err(|e| io::Error::other(format!("invalid relay code: {e}")))?;
            (addr, psk, None, None, None)
        };

    println!("Connecting to {addr}...");
    let (remote_id, registry, used_psk) =
        connect_with_invite(addr, &psk, expected_identity.as_deref(), &identity)
            .map_err(|e| io::Error::other(format!("connection failed: {e}")))?;

    if let Some(ref expected) = expected_identity {
        if remote_id != *expected {
            println!("Warning: expected peer '{expected}' but connected to '{remote_id}'");
        }
    }
    println!("Paired with {remote_id} ({addr})");

    // Pairing is worth keeping even if the hive request goes nowhere — it is
    // what `relay connect` will use next time. `used_psk`, not the invite's:
    // see `connect_with_invite`.
    let _ = save_peer_psk(&remote_id, &used_psk);
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
    } else if super::invite::looks_like_word_phrase(&input) {
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
    let (remote_id, registry, used_psk) =
        connect_with_invite(addr, &psk, remote_identity.as_deref(), &identity)
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

    // Save PSK and metadata. `used_psk` rather than the invite's: see
    // `connect_with_invite`.
    let _ = save_peer_psk(&remote_id, &used_psk);
    let _ = super::save_peer_meta(&remote_id, &addr.to_string());

    // Run the connection loop
    run_connect_loop(&registry, identity.as_str());

    Ok(())
}

/// `claudectl relay discover`
fn cmd_discover(json_mode: bool) -> io::Result<()> {
    let identity = load_or_create_identity();

    // A process on this machine that holds the discovery port publishes what
    // it hears, and a scan bound beside it would see half the LAN
    // (`SO_REUSEPORT` splits datagrams rather than copying them). The
    // arbitration lives in `lan::discover_once`, shared with `hive discover`.
    let peers = super::lan::discover_once(identity.as_str(), |published| {
        if json_mode {
            return;
        }
        if published {
            println!("Peers seen by the claudectl process listening on this machine:");
        } else {
            println!(
                "Scanning LAN for claudectl instances ({} seconds)...",
                super::lan::SCAN_DURATION.as_secs()
            );
        }
        println!();
    });

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

/// #571: the relay counts supervisor tasks on every heartbeat, on every
/// machine. It must never create or upgrade a coord database to do it.
#[cfg(all(test, feature = "coord"))]
mod heartbeat_never_touches_coord {
    use super::supervisor_running_in;

    fn scratch(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("cctl-hb-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn no_database_is_zero_and_creates_nothing() {
        let dir = scratch("none");
        assert_eq!(
            supervisor_running_in(&dir.join("coord").join("coord.db")),
            0
        );
        assert!(!dir.exists(), "the relay created {}", dir.display());
    }

    #[test]
    fn an_older_or_newer_database_is_read_as_zero_and_left_as_it_was() {
        let dir = scratch("versions");
        std::fs::create_dir_all(&dir).unwrap();
        for version in [3u32, crate::coord::store::EXPECTED_COORD_SCHEMA_VERSION + 1] {
            let db = dir.join(format!("v{version}.db"));
            let c = rusqlite::Connection::open(&db).unwrap();
            c.pragma_update(None, "user_version", version).unwrap();
            drop(c);
            assert_eq!(supervisor_running_in(&db), 0);
            let after: u32 = rusqlite::Connection::open(&db)
                .unwrap()
                .query_row("PRAGMA user_version", [], |r| r.get(0))
                .unwrap();
            assert_eq!(after, version, "the relay migrated a coord database");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}

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

#[cfg(test)]
mod startup_dial_targets {
    use super::parse_candidates;
    use serde_json::json;

    fn strs(meta: serde_json::Value) -> Vec<String> {
        parse_candidates(Some(&meta))
            .iter()
            .map(|a| a.to_string())
            .collect()
    }

    // A record written before #484 has one bare `addr` and no list. It must
    // still be dialled, or this change would strand every existing pairing.
    #[test]
    fn a_pre_list_record_is_still_dialled() {
        assert_eq!(
            strs(json!({"addr": "10.33.226.19:7777", "last_seen": 1})),
            vec!["10.33.226.19:7777"]
        );
    }

    // A peer seen on a LAN and over a VPN: both, in recorded order, so the
    // most recent is tried first and the other remains as a fallback.
    #[test]
    fn every_recorded_address_is_dialled_in_order() {
        assert_eq!(
            strs(json!({
                "addr": "100.85.22.1:9847",
                "addrs": [
                    {"addr": "100.85.22.1:9847", "last_seen": 20},
                    {"addr": "192.168.4.24:9847", "last_seen": 10},
                ]
            })),
            vec!["100.85.22.1:9847", "192.168.4.24:9847"]
        );
    }

    // A peer with no address at all — a `.key` and nothing else, which is what
    // the acceptor used to be left with. Skipped, not guessed at.
    #[test]
    fn a_record_without_an_address_is_not_dialled() {
        assert!(parse_candidates(Some(&json!({"last_seen": 1}))).is_empty());
        assert!(parse_candidates(None).is_empty());
    }

    // Nothing on this path resolves names, so a hostname is not dialable here.
    #[test]
    fn unusable_addresses_are_dropped_without_dropping_the_rest() {
        assert_eq!(
            strs(json!({"addrs": [
                {"addr": "mini.local:7777"},
                {"addr": "10.33.226.19"},
                {"addr": 7777},
                {"addr": "192.168.4.39:9847"},
            ]})),
            vec!["192.168.4.39:9847"],
            "one bad entry must not cost us the good one behind it"
        );
    }

    #[test]
    fn a_repeated_address_is_dialled_once() {
        assert_eq!(
            strs(json!({"addrs": [
                {"addr": "192.168.4.39:9847", "last_seen": 20},
                {"addr": "192.168.4.39:9847", "last_seen": 10},
            ]})),
            vec!["192.168.4.39:9847"]
        );
    }

    // An `addrs` key holding nothing usable must not hide a good `addr`.
    #[test]
    fn an_empty_list_falls_back_to_the_single_address() {
        assert_eq!(
            strs(json!({"addr": "192.168.4.39:9847", "addrs": []})),
            vec!["192.168.4.39:9847"]
        );
    }

    #[test]
    fn an_ipv6_address_is_dialled() {
        assert_eq!(strs(json!({"addr": "[::1]:7777"})), vec!["[::1]:7777"]);
    }
}

#[cfg(test)]
mod prune {
    use super::{PruneCandidate, prune_plan};
    use claudectl_core::runtime::PeerLiveness;
    use std::collections::BTreeMap;

    const WEEK: u64 = 7 * 86_400;

    fn peer(id: &str, liveness: PeerLiveness, paired_secs_ago: Option<u64>) -> PruneCandidate {
        PruneCandidate {
            id: id.into(),
            liveness,
            paired_secs_ago,
        }
    }

    #[test]
    fn a_live_peer_is_never_pruned_however_old_its_pairing() {
        let peers = [peer(
            "mini",
            PeerLiveness::Live { secs_ago: 5 },
            Some(10 * WEEK),
        )];
        assert_eq!(prune_plan(&peers, WEEK, &BTreeMap::new()), (vec![], vec![]));
    }

    #[test]
    fn a_peer_stale_past_the_threshold_is_pruned_and_a_recent_one_is_not() {
        let peers = [
            peer(
                "gone",
                PeerLiveness::Stale {
                    secs_ago: 8 * 86_400,
                },
                None,
            ),
            peer("asleep", PeerLiveness::Stale { secs_ago: 3600 }, None),
        ];
        let (forget, kept) = prune_plan(&peers, WEEK, &BTreeMap::new());
        assert_eq!(forget, vec!["gone".to_string()]);
        assert!(kept.is_empty());
    }

    #[test]
    fn a_never_connected_peer_is_dated_by_its_pairing() {
        let peers = [
            peer("old-invite", PeerLiveness::NeverConnected, Some(2 * WEEK)),
            peer("todays-invite", PeerLiveness::NeverConnected, Some(600)),
        ];
        let (forget, _) = prune_plan(&peers, WEEK, &BTreeMap::new());
        assert_eq!(forget, vec!["old-invite".to_string()]);
    }

    #[test]
    fn a_pairing_that_cannot_be_dated_is_left_alone() {
        let peers = [peer("unknown", PeerLiveness::NeverConnected, None)];
        assert_eq!(prune_plan(&peers, 0, &BTreeMap::new()), (vec![], vec![]));
    }

    #[test]
    fn a_peer_the_hive_depends_on_is_kept_with_the_reason() {
        let peers = [peer(
            "host",
            PeerLiveness::Stale { secs_ago: 3 * WEEK },
            None,
        )];
        let protected = BTreeMap::from([(
            "host".to_string(),
            "you joined hive \"homelab\" through it".to_string(),
        )]);
        let (forget, kept) = prune_plan(&peers, WEEK, &protected);
        assert!(forget.is_empty());
        assert_eq!(
            kept,
            vec![(
                "host".to_string(),
                "you joined hive \"homelab\" through it".to_string()
            )]
        );
    }
}
