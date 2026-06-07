//! Bind `HiveActions` (knowledge-store + relay reads/writes) to the binary's
//! real subsystems. When the `hive` or `relay` features are off the impl
//! returns empty/no-op values so the TUI's render paths stay branchless.

use std::collections::HashSet;

use claudectl_core::runtime::{HiveActions, HiveViewSnapshot};
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
}
