//! Bind `HiveActions` (knowledge-store + relay reads/writes) to the binary's
//! real subsystems. When the `hive` or `relay` features are off the impl
//! returns empty/no-op values so the TUI's render paths stay branchless.

use std::collections::HashSet;

use claudectl_core::runtime::{DiscoveryListener, HiveActions, HiveViewSnapshot};
use claudectl_core::skills::DiscoveredSkill;

pub struct LiveHiveActions;

impl HiveActions for LiveHiveActions {
    fn shared_skill_keys(&self) -> HashSet<String> {
        #[cfg(feature = "hive")]
        {
            let store = crate::hive::store::HiveStore::load();
            let mut out = HashSet::new();
            for unit in store.all_units() {
                if let crate::hive::KnowledgeContent::Skill { name, .. } = &unit.content {
                    out.insert(format!("skill:{}", name.to_lowercase().replace(' ', "-")));
                }
            }
            out
        }
        #[cfg(not(feature = "hive"))]
        {
            HashSet::new()
        }
    }

    fn share_skill(&self, skill: &DiscoveredSkill) -> Result<String, String> {
        #[cfg(feature = "hive")]
        {
            let path_str = skill.path.to_string_lossy().to_string();
            crate::hive::cli::share_artifact_from_path("skill", &path_str, "universal")
                .map(|(unit_id, _summary)| unit_id)
                .map_err(|e| e.to_string())
        }
        #[cfg(not(feature = "hive"))]
        {
            let _ = skill;
            Err("hive feature not compiled in".into())
        }
    }

    fn hive_view_snapshot(&self) -> HiveViewSnapshot {
        #[cfg(feature = "relay")]
        {
            let identity = Some(crate::relay::load_or_create_identity().as_str().to_string());
            let peers = crate::relay::list_known_peers()
                .into_iter()
                .map(|id| {
                    let addr = crate::relay::load_peer_meta(&id).and_then(|v| {
                        v.get("addr")
                            .and_then(|a| a.as_str())
                            .map(|s| s.to_string())
                    });
                    (id, addr)
                })
                .collect();
            HiveViewSnapshot { identity, peers }
        }
        #[cfg(not(feature = "relay"))]
        {
            HiveViewSnapshot::default()
        }
    }

    fn start_discovery(&self) -> Option<Box<dyn DiscoveryListener>> {
        #[cfg(feature = "relay")]
        {
            Some(Box::new(discovery::LiveDiscovery::new(
                crate::relay::load_or_create_identity().as_str().to_string(),
            )))
        }
        #[cfg(not(feature = "relay"))]
        {
            None
        }
    }
}

/// The Hive tab's live listener over `relay::lan`.
///
/// The one rule here is the port-sharing rule `relay discover` learned the
/// hard way (#506): a second socket bound beside a long-lived holder of the
/// discovery port does not get a copy of each datagram, `SO_REUSEPORT` splits
/// them, and both ends then see half the LAN. The rule in `lan.rs` is
/// "whoever holds the port publishes `discovered.json`, and everyone else
/// reads it". So this handle:
///
/// - reads the file and holds no socket while another process publishes it
///   (a relay, or another TUI on this machine);
/// - otherwise binds the port itself; the listener thread then publishes in
///   turn on its own clock and retracts the file when it exits, so a
///   `relay discover` or a second TUI reads instead of binding beside it and
///   scans the moment the tab is gone;
/// - re-checks on every snapshot, because the tab's `h` key starts a relay
///   mid-session: the moment someone else's write shows up the socket is
///   dropped, and when their file goes stale it is bound again.
///
/// Its own writes are told apart by `writer_pid` (`lan::published_by_other`),
/// otherwise it would read its own publication, conclude a relay had started,
/// and drop the socket.
#[cfg(feature = "relay")]
mod discovery {
    use std::time::{Duration, Instant};

    use claudectl_core::runtime::{
        DiscoveredHive, DiscoveredMachine, DiscoveryListener, DiscoverySnapshot, DiscoverySource,
    };

    use crate::relay::lan::{self, DiscoveredPeer, HiveAd};

    /// How long to leave a port that refused to bind before trying again.
    /// Trying on every tick would be a syscall and a log line every two
    /// seconds for as long as the tab is open.
    const BIND_RETRY: Duration = Duration::from_secs(15);

    pub struct LiveDiscovery {
        own_identity: String,
        /// Our own socket on the discovery port, held only while nobody else
        /// on this machine publishes. Dropping it stops the thread and waits
        /// for it, so the port is free and the file retracted by the time the
        /// tab has gone.
        listener: Option<lan::Listener>,
        /// When the last bind failed, for the retry backoff; the log line is
        /// written once per failure streak, not once per tick.
        bind_failed_at: Option<Instant>,
    }

    impl LiveDiscovery {
        pub fn new(own_identity: String) -> Self {
            Self {
                own_identity,
                listener: None,
                bind_failed_at: None,
            }
        }

        fn cannot_listen() -> DiscoverySnapshot {
            DiscoverySnapshot {
                source: DiscoverySource::BindFailed,
                machines: Vec::new(),
            }
        }
    }

    impl DiscoveryListener for LiveDiscovery {
        fn snapshot(&mut self) -> DiscoverySnapshot {
            if let Some(seen) = lan::published_by_other(lan::read_seen(), std::process::id()) {
                // Another process holds the port. Never bind beside it.
                self.listener = None;
                let peers = lan::peers_from_seen(&seen, &self.own_identity);
                return DiscoverySnapshot {
                    source: DiscoverySource::Published,
                    machines: machines_from(&peers),
                };
            }

            if self.listener.is_none() {
                if let Some(failed_at) = self.bind_failed_at {
                    if failed_at.elapsed() < BIND_RETRY {
                        return Self::cannot_listen();
                    }
                }
                match lan::start_listener(self.own_identity.clone(), true) {
                    Ok(listener) => {
                        self.listener = Some(listener);
                        self.bind_failed_at = None;
                    }
                    Err(e) => {
                        if self.bind_failed_at.is_none() {
                            crate::logger::log("LAN", &format!("Hive tab cannot listen: {e}"));
                        }
                        self.bind_failed_at = Some(Instant::now());
                        return Self::cannot_listen();
                    }
                }
            }
            // Clone out from under the lock: the UDP thread takes the same
            // lock for every announcement. A poisoned lock means that thread
            // panicked, so nothing is listening or publishing any more: drop
            // the handle and say so, rather than render an empty map as if it
            // were the LAN.
            let peers: Result<Vec<DiscoveredPeer>, ()> = {
                let Some(own) = self.listener.as_ref() else {
                    unreachable!("listener was just bound")
                };
                own.peers
                    .lock()
                    .map(|m| m.values().filter(|p| !p.is_stale()).cloned().collect())
                    .map_err(|_| ())
            };
            let Ok(peers) = peers else {
                crate::logger::log("LAN", "Hive tab listener thread died; will rebind");
                self.listener = None;
                return Self::cannot_listen();
            };
            DiscoverySnapshot {
                source: DiscoverySource::Listener,
                machines: machines_from(&peers),
            }
        }
    }

    fn machines_from(peers: &[DiscoveredPeer]) -> Vec<DiscoveredMachine> {
        let mut machines: Vec<DiscoveredMachine> = peers.iter().map(machine_from_peer).collect();
        sort_machines(&mut machines);
        machines
    }

    fn machine_from_peer(p: &DiscoveredPeer) -> DiscoveredMachine {
        machine_from_parts(p, crate::relay::load_peer_psk(&p.identity).is_some())
    }

    /// The conversion without the PSK lookup, so a test can cover it with no
    /// relay directory on the machine running the suite.
    fn machine_from_parts(p: &DiscoveredPeer, paired: bool) -> DiscoveredMachine {
        DiscoveredMachine {
            identity: p.identity.clone(),
            addr: p.relay_addr().to_string(),
            version: p.version.clone(),
            hive: p.hive.as_ref().map(hive_from_ad),
            paired,
        }
    }

    fn hive_from_ad(h: &HiveAd) -> DiscoveredHive {
        DiscoveredHive {
            id: h.id.clone(),
            name: h.name.clone(),
            join_policy: h.join_policy.clone(),
            peers: h.peers,
            units: h.units,
        }
    }

    /// Named hives first, grouped by name, then by identity, so the listing
    /// holds still between ticks while announcements arrive in any order.
    fn sort_machines(machines: &mut [DiscoveredMachine]) {
        machines.sort_by(|a, b| {
            let ka = (a.hive.is_none(), a.hive.as_ref().map(|h| h.name.as_str()));
            let kb = (b.hive.is_none(), b.hive.as_ref().map(|h| h.name.as_str()));
            ka.cmp(&kb).then_with(|| a.identity.cmp(&b.identity))
        });
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::net::SocketAddr;
        use std::time::Instant;

        fn peer(identity: &str, hive: Option<&str>) -> DiscoveredPeer {
            DiscoveredPeer {
                identity: identity.into(),
                addr: "192.168.1.20:40000".parse::<SocketAddr>().unwrap(),
                relay_port: 9847,
                version: "0.90.2".into(),
                last_seen: Instant::now(),
                hive: hive.map(|name| HiveAd {
                    id: format!("hv_{name}"),
                    name: name.into(),
                    join_policy: "ask".into(),
                    peers: 2,
                    units: 212,
                }),
            }
        }

        #[test]
        fn machine_carries_relay_addr_and_hive() {
            let m = machine_from_parts(&peer("mini-9f2a", Some("my-hive")), true);
            assert_eq!(m.identity, "mini-9f2a");
            assert_eq!(m.addr, "192.168.1.20:9847");
            assert!(m.paired);
            let h = m.hive.expect("hive block");
            assert_eq!(h.name, "my-hive");
            assert_eq!(h.join_policy, "ask");
            assert_eq!((h.peers, h.units), (2, 212));
        }

        #[test]
        fn named_hives_sort_first_then_by_identity() {
            let mut machines = vec![
                machine_from_parts(&peer("zed", None), false),
                machine_from_parts(&peer("bob", Some("ops")), false),
                machine_from_parts(&peer("amy", None), false),
                machine_from_parts(&peer("cat", Some("dev")), false),
                machine_from_parts(&peer("ann", Some("ops")), false),
            ];
            sort_machines(&mut machines);
            let ids: Vec<&str> = machines.iter().map(|m| m.identity.as_str()).collect();
            assert_eq!(ids, ["cat", "ann", "bob", "amy", "zed"]);
        }
    }
}
