//! A peer's advertised headroom (#571), as the heartbeat carries it and as
//! this machine keeps it for placement.
//!
//! On disk at `peers/<id>.headroom` because the supervisor runs in its own
//! process and can't reach the live registry, like `.caps`. Unlike `.caps`,
//! it records **when it was received**, by this machine's clock: placement
//! must not trust a reading from a peer that has gone quiet, and a sender's
//! clock can't be compared with ours.

use claudectl_core::headroom::Headroom;
use serde::{Deserialize, Serialize};

/// What a heartbeat's `headroom` object holds.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Advertised {
    #[serde(flatten)]
    pub headroom: Headroom,
    /// User-defined capability tags (`[relay] tags`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
}

/// A peer's last advertisement and when it arrived.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Received {
    pub peer: String,
    /// Epoch ms on this machine when the heartbeat carrying it arrived.
    pub received_ms: u64,
    #[serde(flatten)]
    pub advertised: Advertised,
}

/// A peer controls its tags, so they're bounded before they're kept.
const MAX_TAGS: usize = 32;
const MAX_TAG_LEN: usize = 40;

/// A tag is what a task's `requires` names: short, and nothing a version
/// constraint or `:auth` suffix could be confused with.
pub fn valid_tag(t: &str) -> bool {
    !t.is_empty()
        && t.len() <= MAX_TAG_LEN
        && t.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
}

/// This machine's headroom as a heartbeat carries it.
pub fn local(tags: &[String], disk_path: &std::path::Path, running_tasks: u32) -> Advertised {
    Advertised {
        headroom: claudectl_core::headroom::probe(disk_path, running_tasks),
        tags: tags.iter().filter(|t| valid_tag(t)).cloned().collect(),
    }
}

fn path(peer_id: &str) -> Option<std::path::PathBuf> {
    super::is_valid_peer_id(peer_id).then(|| super::peers_dir().join(format!("{peer_id}.headroom")))
}

/// Record the `headroom` object from a peer's heartbeat payload, if it sent
/// one. Malformed is ignored, like an older peer that sends none.
pub fn record(peer_id: &str, payload: &serde_json::Value, now_ms: u64) -> Result<bool, String> {
    let Some(value) = payload.get("headroom") else {
        return Ok(false);
    };
    let Ok(mut advertised) = serde_json::from_value::<Advertised>(value.clone()) else {
        return Ok(false);
    };
    advertised.tags.retain(|t| valid_tag(t));
    advertised.tags.truncate(MAX_TAGS);
    let path = path(peer_id).ok_or_else(|| format!("invalid peer id: {peer_id}"))?;
    let rec = Received {
        peer: peer_id.to_string(),
        received_ms: now_ms,
        advertised,
    };
    std::fs::create_dir_all(super::peers_dir()).map_err(|e| format!("create peers dir: {e}"))?;
    let json = serde_json::to_string(&rec).map_err(|e| format!("serialize: {e}"))?;
    // Atomic: the supervisor reads this file on every tick.
    let tmp = path.with_extension("headroom.tmp");
    std::fs::write(&tmp, json).map_err(|e| format!("write headroom: {e}"))?;
    std::fs::rename(&tmp, &path).map_err(|e| format!("rename headroom: {e}"))?;
    Ok(true)
}

/// Every peer's last advertisement, whatever its age. The reader decides
/// what's too old.
pub fn load_all() -> Vec<Received> {
    let Ok(entries) = std::fs::read_dir(super::peers_dir()) else {
        return Vec::new();
    };
    let mut out: Vec<Received> = entries
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "headroom"))
        .filter_map(|e| std::fs::read_to_string(e.path()).ok())
        .filter_map(|s| serde_json::from_str::<Received>(&s).ok())
        .filter(|r| super::is_valid_peer_id(&r.peer))
        .collect();
    out.sort_by(|a, b| a.peer.cmp(&b.peer));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_wire_form_is_headroom_fields_plus_tags() {
        let a = Advertised {
            headroom: Headroom {
                mem_free_mb: Some(4096),
                load_per_core: Some(0.5),
                disk_free_mb: Some(9000),
                running_tasks: 1,
            },
            tags: vec!["docker".into()],
        };
        let v = serde_json::to_value(&a).unwrap();
        assert_eq!(
            v,
            serde_json::json!({"mem_free_mb": 4096, "load_per_core": 0.5,
                "disk_free_mb": 9000, "running_tasks": 1, "tags": ["docker"]})
        );
        assert_eq!(serde_json::from_value::<Advertised>(v).unwrap(), a);
    }

    #[test]
    fn tags_are_short_plain_words() {
        for ok in ["docker", "gpu", "tunnel-prod", "cuda_12", "py3.11"] {
            assert!(valid_tag(ok), "{ok}");
        }
        for bad in ["", "node>=22", "claude:auth", "a b", &"x".repeat(41)] {
            assert!(!valid_tag(bad), "{bad}");
        }
    }
}
