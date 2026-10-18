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
/// hard way (#506): a second socket bound beside a running `relay serve` does
/// not get a copy of each datagram, `SO_REUSEPORT` splits them, and both ends
/// then see half the LAN. So while a local relay publishes `discovered.json`
/// this handle reads that file and holds no socket. It binds its own only
/// when no relay is publishing, and re-checks on every snapshot, because the
/// tab's `h` key starts a relay mid-session: the moment the file goes fresh
/// the socket is dropped, and when the file goes stale it is bound again.
#[cfg(feature = "relay")]
mod discovery {
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};

    use claudectl_core::runtime::{
        DiscoveredHive, DiscoveredMachine, DiscoveryListener, DiscoverySnapshot, DiscoverySource,
    };

    use crate::relay::lan::{self, DiscoveredPeer, HiveAd};

    /// Our own socket on the discovery port, held only while no local relay
    /// publishes. Dropping it tells the listener thread to exit; the thread's
    /// receive timeout is 500ms, so the port is free again within that.
    struct OwnListener {
        shutdown: Arc<AtomicBool>,
        map: Arc<Mutex<HashMap<String, DiscoveredPeer>>>,
    }

    impl Drop for OwnListener {
        fn drop(&mut self) {
            self.shutdown.store(true, Ordering::Relaxed);
        }
    }

    pub struct LiveDiscovery {
        own_identity: String,
        listener: Option<OwnListener>,
    }

    impl LiveDiscovery {
        pub fn new(own_identity: String) -> Self {
            Self {
                own_identity,
                listener: None,
            }
        }
    }

    impl DiscoveryListener for LiveDiscovery {
        fn snapshot(&mut self) -> DiscoverySnapshot {
            if let Some(seen) = lan::read_seen() {
                // A relay on this machine holds the port. Never bind beside it.
                self.listener = None;
                let mut machines: Vec<DiscoveredMachine> = seen
                    .peers
                    .iter()
                    .filter(|p| p.identity != self.own_identity)
                    .map(|p| machine_from_peer(&DiscoveredPeer::from_seen(p)))
                    .collect();
                sort_machines(&mut machines);
                return DiscoverySnapshot {
                    source: DiscoverySource::LocalRelay,
                    machines,
                };
            }

            let own_identity = self.own_identity.clone();
            let own = self.listener.get_or_insert_with(|| {
                let shutdown = Arc::new(AtomicBool::new(false));
                let map = lan::start_listener(own_identity, Arc::clone(&shutdown));
                OwnListener { shutdown, map }
            });
            let mut machines: Vec<DiscoveredMachine> = own
                .map
                .lock()
                .map(|m| {
                    m.values()
                        .filter(|p| !p.is_stale())
                        .map(machine_from_peer)
                        .collect()
                })
                .unwrap_or_default();
            sort_machines(&mut machines);
            DiscoverySnapshot {
                source: DiscoverySource::Listener,
                machines,
            }
        }
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
