//! Protocol version and feature negotiation (#559).
//!
//! Before this, the handshake carried `version: CARGO_PKG_VERSION` and nothing
//! read it, so the only way to add to the protocol was to hope the other side
//! ignored what it didn't know. It didn't: an unknown `msg_type` ended the
//! reader loop. Now the dialler says which protocol it speaks and which
//! features it has, the acceptor answers with the features both sides have,
//! and anything new is sent only on a link that agreed to it.
//!
//! - `proto` is bumped only for a change an older peer cannot survive.
//!   Everything else is a feature string.
//! - A peer that sends no `proto` key predates this module. It gets proto 0
//!   and no features, which is exactly the protocol it already speaks.

use std::collections::BTreeSet;

/// The protocol version this build speaks.
pub const PROTO_VERSION: u64 = 1;

/// This build drops frames whose `msg_type` it doesn't know instead of
/// closing the link. A sender may rely on it to probe; nothing else does yet.
pub const UNKNOWN_OK: &str = "unknown-ok";

/// Every feature this build supports.
const LOCAL: &[&str] = &[UNKNOWN_OK];

/// A peer controls this list, so it's bounded and filtered before it's kept.
const MAX_FEATURES: usize = 64;
const MAX_FEATURE_LEN: usize = 32;

/// The features agreed for one link: what both ends advertised.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Features(BTreeSet<String>);

impl Features {
    /// What this build supports.
    pub fn local() -> Self {
        Features(LOCAL.iter().map(|f| (*f).to_string()).collect())
    }

    /// Whether this link agreed to `feature`. Gate every new message type
    /// and every new payload key on this.
    #[allow(
        dead_code,
        reason = "the gate for #560-#564; `unknown-ok` itself needs no check on send"
    )]
    pub fn has(&self, feature: &str) -> bool {
        self.0.contains(feature)
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Sorted, for the wire and for logs.
    pub fn names(&self) -> Vec<String> {
        self.0.iter().cloned().collect()
    }

    /// One line for the relay's output when a link comes up.
    pub fn describe(&self) -> String {
        if self.is_empty() {
            "none (the peer predates protocol negotiation)".to_string()
        } else {
            self.names().join(", ")
        }
    }

    /// The features both sides have.
    pub fn agree(&self, other: &Features) -> Features {
        Features(self.0.intersection(&other.0).cloned().collect())
    }

    /// Read `proto` and `features` from a handshake or ack payload.
    ///
    /// `None` when there is no `proto` key: a peer from before #559, which
    /// must be treated as proto 0 with no features. Feature names outside
    /// `[a-z0-9-]`, empty or longer than 32 bytes are dropped, and at most 64
    /// are kept, so a peer can't make this side hold arbitrary data.
    pub fn from_payload(payload: &serde_json::Value) -> Option<(u64, Features)> {
        let proto = payload.get("proto")?.as_u64()?;
        let names = payload
            .get("features")
            .and_then(|v| v.as_array())
            .map(|list| {
                list.iter()
                    .filter_map(|v| v.as_str())
                    .filter(|s| is_valid_name(s))
                    .take(MAX_FEATURES)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        Some((proto, Features(names)))
    }
}

fn is_valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_FEATURE_LEN
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// What the acceptor answers to a handshake: the protocol both can speak and
/// the features both have. `None` for a peer from before #559, whose ack must
/// stay as it was.
pub fn negotiate(handshake_payload: &serde_json::Value) -> Option<(u64, Features)> {
    let (their_proto, theirs) = Features::from_payload(handshake_payload)?;
    Some((
        their_proto.min(PROTO_VERSION),
        Features::local().agree(&theirs),
    ))
}

/// What the dialler keeps from the ack. An acceptor from before #559 sends no
/// `proto`, which means no features. Whatever the acceptor says, the result
/// never includes a feature this build doesn't have.
pub fn from_ack(ack_payload: &serde_json::Value) -> Features {
    match Features::from_payload(ack_payload) {
        Some((_, granted)) => Features::local().agree(&granted),
        None => Features::default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_peer_from_before_559_negotiates_nothing() {
        assert_eq!(negotiate(&json!({"proof": "x", "version": "0.80.0"})), None);
        assert!(from_ack(&json!({"status": "ok"})).is_empty());
    }

    #[test]
    fn both_sides_get_only_what_both_have() {
        let (proto, agreed) =
            negotiate(&json!({"proto": 1, "features": ["unknown-ok", "frame-bin"]})).unwrap();
        assert_eq!(proto, 1);
        assert_eq!(agreed.names(), vec!["unknown-ok".to_string()]);
    }

    #[test]
    fn a_newer_peer_is_answered_in_this_builds_protocol() {
        let (proto, _) = negotiate(&json!({"proto": 7, "features": []})).unwrap();
        assert_eq!(proto, PROTO_VERSION);
    }

    #[test]
    fn an_acceptor_cannot_grant_a_feature_this_build_lacks() {
        let got =
            from_ack(&json!({"status": "ok", "proto": 1, "features": ["unknown-ok", "made-up"]}));
        assert!(got.has(UNKNOWN_OK));
        assert!(!got.has("made-up"));
    }

    #[test]
    fn malformed_feature_names_are_dropped() {
        let (_, f) = Features::from_payload(&json!({
            "proto": 1,
            "features": ["ok-1", "UPPER", "", "has space", 5, "x".repeat(33)],
        }))
        .unwrap();
        assert_eq!(f.names(), vec!["ok-1".to_string()]);
    }

    #[test]
    fn the_feature_list_is_bounded() {
        let many: Vec<String> = (0..500).map(|i| format!("f{i}")).collect();
        let (_, f) = Features::from_payload(&json!({"proto": 1, "features": many})).unwrap();
        assert_eq!(f.names().len(), MAX_FEATURES);
    }
}
