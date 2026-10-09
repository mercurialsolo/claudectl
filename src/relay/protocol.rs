// NDJSON wire protocol and HMAC challenge-response authentication.

use std::io::{self, BufReader, Write};
use std::net::TcpStream;

use super::crypto;
use super::{MessageType, RelayMessage, epoch_ms, gen_msg_id};

/// Maximum line size: 1 MB.
const MAX_LINE_SIZE: usize = 1_048_576;

// ────────────────────────────────────────────────────────────────────────────
// NDJSON framing
// ────────────────────────────────────────────────────────────────────────────

/// Write a RelayMessage as a single JSON line to the stream.
pub fn write_message(stream: &mut TcpStream, msg: &RelayMessage) -> io::Result<()> {
    let json =
        serde_json::to_string(msg).map_err(|e| io::Error::other(format!("serialize: {e}")))?;
    let line = format!("{json}\n");
    stream.write_all(line.as_bytes())?;
    stream.flush()
}

/// Read one RelayMessage from a buffered reader. Returns None on EOF.
/// Reads byte-by-byte up to MAX_LINE_SIZE to prevent OOM from malicious peers.
pub fn read_message(reader: &mut BufReader<TcpStream>) -> io::Result<Option<RelayMessage>> {
    let mut line = Vec::with_capacity(4096);
    let mut byte = [0u8; 1];

    loop {
        use std::io::Read;
        match reader.read(&mut byte) {
            Ok(0) => {
                if line.is_empty() {
                    return Ok(None); // EOF
                }
                break; // EOF mid-line, try to parse what we have
            }
            Ok(_) => {
                if byte[0] == b'\n' {
                    break;
                }
                line.push(byte[0]);
                if line.len() > MAX_LINE_SIZE {
                    return Err(io::Error::other("message exceeds 1MB size limit"));
                }
            }
            Err(e) => return Err(e),
        }
    }

    if line.is_empty() {
        return Ok(None);
    }

    let text = String::from_utf8(line).map_err(|e| io::Error::other(format!("utf8: {e}")))?;
    let msg: RelayMessage =
        serde_json::from_str(text.trim()).map_err(|e| io::Error::other(format!("parse: {e}")))?;
    Ok(Some(msg))
}

// ────────────────────────────────────────────────────────────────────────────
// Server-side authentication
// ────────────────────────────────────────────────────────────────────────────

/// Server: send a challenge nonce to the connecting peer.
/// Returns the nonce for later verification.
pub fn send_challenge(stream: &mut TcpStream) -> io::Result<String> {
    let nonce = crypto::random_hex(32);
    let msg = RelayMessage {
        id: gen_msg_id(),
        msg_type: MessageType::Challenge,
        from_peer: String::new(), // server fills identity later
        timestamp: epoch_ms(),
        payload: serde_json::json!({ "nonce": nonce }),
    };
    write_message(stream, &msg)?;
    Ok(nonce)
}

/// Server: verify a handshake response against the expected nonce and PSK.
/// Returns the peer ID if verification succeeds.
pub fn verify_handshake(msg: &RelayMessage, nonce: &str, psk: &[u8; 32]) -> Result<String, String> {
    if msg.msg_type != MessageType::Handshake {
        return Err("expected handshake message".into());
    }

    let proof = msg
        .payload
        .get("proof")
        .and_then(|v| v.as_str())
        .ok_or("missing proof field")?;

    let expected = crypto::hmac_sha256(psk, nonce.as_bytes());
    let expected_hex = crypto::hex_encode(&expected);

    // Constant-time (#426): the nonce is fresh per connection, so an attacker
    // cannot replay a fixed challenge to walk the MAC — but a short-circuiting
    // compare still leaks the position of the first differing nibble.
    if !crypto::ct_eq(proof.as_bytes(), expected_hex.as_bytes()) {
        return Err("HMAC verification failed".into());
    }

    Ok(msg.from_peer.clone())
}

/// Server: send a handshake acknowledgement.
pub fn send_handshake_ack(stream: &mut TcpStream, identity: &str, status: &str) -> io::Result<()> {
    let msg = RelayMessage {
        id: gen_msg_id(),
        msg_type: MessageType::HandshakeAck,
        from_peer: identity.to_string(),
        timestamp: epoch_ms(),
        payload: serde_json::json!({ "status": status }),
    };
    write_message(stream, &msg)
}

// ────────────────────────────────────────────────────────────────────────────
// Client-side authentication
// ────────────────────────────────────────────────────────────────────────────

/// Client: compute the HMAC proof for a challenge nonce.
pub fn compute_proof(nonce: &str, psk: &[u8; 32]) -> String {
    let mac = crypto::hmac_sha256(psk, nonce.as_bytes());
    crypto::hex_encode(&mac)
}

/// What a dialler tells the acceptor about itself during the handshake.
///
/// Two kinds of connection reach a listener and they are not interchangeable:
/// a lasting link between two peers, and a one-shot that carries a single
/// message and closes. Treating the second as the first is #487 — the registry
/// saw a message delivery as a rival connection and the collision rule, asked
/// a question about the wrong kind of connection, closed it before its frame
/// was read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DialIntent {
    /// The port we accept connections on, so the peer can record an address it
    /// could dial us back at (#484). `None` when we have no listener, and when
    /// we are bound to a specific address rather than a wildcard, since such a
    /// port is not reachable at the address the peer observes us from.
    pub listen_port: Option<u16>,
    /// This connection carries one message and closes. The acceptor must
    /// deliver its messages without registering it as the peer link.
    pub transient: bool,
}

impl DialIntent {
    /// A lasting link between two peers.
    pub fn peer_link(listen_port: Option<u16>) -> Self {
        DialIntent {
            listen_port,
            transient: false,
        }
    }

    /// A connection that carries one message and closes.
    ///
    /// It advertises no listening port: a one-shot sender has no listener, and
    /// nothing should learn an address from a connection that is about to go
    /// away.
    pub fn one_shot() -> Self {
        DialIntent {
            listen_port: None,
            transient: true,
        }
    }
}

/// Client: send a handshake response with the HMAC proof.
///
/// A peer too old to read the extra fields ignores them, so both are additive.
pub fn send_handshake(
    stream: &mut TcpStream,
    identity: &str,
    nonce: &str,
    psk: &[u8; 32],
    intent: &DialIntent,
) -> io::Result<()> {
    let proof = compute_proof(nonce, psk);
    let mut payload = serde_json::json!({
        "proof": proof,
        "version": env!("CARGO_PKG_VERSION"),
    });
    if let Some(port) = intent.listen_port {
        payload["listen_port"] = serde_json::json!(port);
    }
    if intent.transient {
        payload["transient"] = serde_json::json!(true);
    }
    let msg = RelayMessage {
        id: gen_msg_id(),
        msg_type: MessageType::Handshake,
        from_peer: identity.to_string(),
        timestamp: epoch_ms(),
        payload,
    };
    write_message(stream, &msg)
}

/// Server: does the dialling peer say this connection is a one-shot?
///
/// Absent on anything older than 0.74.0, which is why the default is `false`:
/// an older peer's one-shot still looks like a link, as it did before #487.
/// Only an explicit `true` counts, so a malformed value cannot turn a real
/// peer link into a delivery that is never registered.
pub fn handshake_is_transient(msg: &RelayMessage) -> bool {
    msg.payload.get("transient").and_then(|v| v.as_bool()) == Some(true)
}

/// Server: the port the dialling peer says it listens on, if it said.
///
/// Absent on anything older than 0.73.0 and on a dialler with no listener, so
/// `None` means "nothing learned", never an error.
pub fn handshake_listen_port(msg: &RelayMessage) -> Option<u16> {
    let port = msg.payload.get("listen_port")?.as_u64()?;
    if port == 0 || port > u16::MAX as u64 {
        return None;
    }
    Some(port as u16)
}

/// Client: wait for and parse the handshake ack. Returns Ok(()) on success.
pub fn await_handshake_ack(reader: &mut BufReader<TcpStream>) -> Result<String, String> {
    let msg = read_message(reader)
        .map_err(|e| format!("read ack: {e}"))?
        .ok_or("connection closed before ack")?;

    if msg.msg_type != MessageType::HandshakeAck {
        return Err(format!("expected handshake_ack, got {:?}", msg.msg_type));
    }

    let status = msg
        .payload
        .get("status")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");

    if status == "ok" {
        Ok(msg.from_peer)
    } else {
        Err(format!("handshake denied: {status}"))
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Heartbeat helpers
// ────────────────────────────────────────────────────────────────────────────

/// Build a heartbeat message (liveness only, no session data).
pub fn heartbeat_message(identity: &str) -> RelayMessage {
    RelayMessage {
        id: gen_msg_id(),
        msg_type: MessageType::Heartbeat,
        from_peer: identity.to_string(),
        timestamp: epoch_ms(),
        payload: serde_json::json!({}),
    }
}

/// Build a heartbeat message carrying the worker's current session state.
pub fn heartbeat_with_sessions(identity: &str, sessions: &[serde_json::Value]) -> RelayMessage {
    RelayMessage {
        id: gen_msg_id(),
        msg_type: MessageType::Heartbeat,
        from_peer: identity.to_string(),
        timestamp: epoch_ms(),
        payload: serde_json::json!({
            "worker_id": identity,
            "timestamp": epoch_ms(),
            "sessions": sessions,
        }),
    }
}

/// Build an ack message for a received message.
pub fn ack_message(identity: &str, original_id: &str) -> RelayMessage {
    RelayMessage {
        id: gen_msg_id(),
        msg_type: MessageType::Ack,
        from_peer: identity.to_string(),
        timestamp: epoch_ms(),
        payload: serde_json::json!({ "ack_id": original_id }),
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Tests
// ────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn challenge_response_flow() {
        let psk = crypto::generate_psk();
        let nonce = crypto::random_hex(32);

        // Client computes proof
        let proof = compute_proof(&nonce, &psk);

        // Server verifies
        let msg = RelayMessage {
            id: "test".into(),
            msg_type: MessageType::Handshake,
            from_peer: "client-1".into(),
            timestamp: 0,
            payload: serde_json::json!({ "proof": proof, "version": "0.35.0" }),
        };
        let result = verify_handshake(&msg, &nonce, &psk);
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "client-1");
    }

    #[test]
    fn bad_proof_rejected() {
        let psk = crypto::generate_psk();
        let nonce = crypto::random_hex(32);

        let msg = RelayMessage {
            id: "test".into(),
            msg_type: MessageType::Handshake,
            from_peer: "client-1".into(),
            timestamp: 0,
            payload: serde_json::json!({ "proof": "deadbeef", "version": "0.35.0" }),
        };
        let result = verify_handshake(&msg, &nonce, &psk);
        assert!(result.is_err());
    }

    #[test]
    fn wrong_message_type_rejected() {
        let psk = crypto::generate_psk();
        let nonce = crypto::random_hex(32);
        let proof = compute_proof(&nonce, &psk);

        let msg = RelayMessage {
            id: "test".into(),
            msg_type: MessageType::Heartbeat, // wrong type
            from_peer: "client-1".into(),
            timestamp: 0,
            payload: serde_json::json!({ "proof": proof }),
        };
        let result = verify_handshake(&msg, &nonce, &psk);
        assert!(result.is_err());
    }

    #[test]
    fn heartbeat_message_valid() {
        let msg = heartbeat_message("test-peer");
        assert_eq!(msg.msg_type, MessageType::Heartbeat);
        assert_eq!(msg.from_peer, "test-peer");
        assert!(msg.timestamp > 0);
    }

    #[test]
    fn ack_message_valid() {
        let msg = ack_message("test-peer", "msg_123");
        assert_eq!(msg.msg_type, MessageType::Ack);
        assert_eq!(
            msg.payload.get("ack_id").and_then(|v| v.as_str()),
            Some("msg_123")
        );
    }

    #[test]
    fn heartbeat_with_sessions_includes_payload() {
        let sessions = vec![
            serde_json::json!({"pid": 1234, "project": "backend", "status": "Processing"}),
            serde_json::json!({"pid": 5678, "project": "frontend", "status": "Idle"}),
        ];
        let msg = heartbeat_with_sessions("worker-01", &sessions);
        assert_eq!(msg.msg_type, MessageType::Heartbeat);
        assert_eq!(msg.from_peer, "worker-01");
        assert_eq!(
            msg.payload.get("worker_id").and_then(|v| v.as_str()),
            Some("worker-01")
        );
        let payload_sessions = msg.payload.get("sessions").and_then(|v| v.as_array());
        assert!(payload_sessions.is_some());
        assert_eq!(payload_sessions.unwrap().len(), 2);
    }

    #[test]
    fn heartbeat_with_empty_sessions() {
        let msg = heartbeat_with_sessions("worker-02", &[]);
        let sessions = msg.payload.get("sessions").and_then(|v| v.as_array());
        assert!(sessions.is_some());
        assert_eq!(sessions.unwrap().len(), 0);
    }
}

#[cfg(test)]
mod advertised_listen_port {
    use super::super::{MessageType, RelayMessage};
    use super::handshake_listen_port;

    fn handshake(payload: serde_json::Value) -> RelayMessage {
        RelayMessage {
            id: "m1".into(),
            msg_type: MessageType::Handshake,
            from_peer: "peer-1".into(),
            timestamp: 0,
            payload,
        }
    }

    #[test]
    fn a_advertised_port_is_read() {
        assert_eq!(
            handshake_listen_port(&handshake(
                serde_json::json!({"proof": "ab", "version": "0.73.0", "listen_port": 9847})
            )),
            Some(9847)
        );
    }

    // Every peer built before #484 sends only proof and version, and a dialler
    // with no listener sends no port either. Both mean "nothing learned", so
    // neither may look like an error.
    #[test]
    fn an_absent_port_is_not_an_error() {
        assert_eq!(
            handshake_listen_port(&handshake(
                serde_json::json!({"proof": "ab", "version": "0.72.0"})
            )),
            None
        );
    }

    // Port 0 means "any port" to bind(2) and is never something to dial, so it
    // must not be recorded as an address.
    #[test]
    fn port_zero_is_refused() {
        assert_eq!(
            handshake_listen_port(&handshake(serde_json::json!({"listen_port": 0}))),
            None
        );
    }

    #[test]
    fn a_port_outside_the_range_is_refused() {
        assert_eq!(
            handshake_listen_port(&handshake(serde_json::json!({"listen_port": 65536}))),
            None
        );
        assert_eq!(
            handshake_listen_port(&handshake(serde_json::json!({"listen_port": -1}))),
            None
        );
        assert_eq!(
            handshake_listen_port(&handshake(serde_json::json!({"listen_port": "9847"}))),
            None,
            "a string is not a port we should trust into an address"
        );
    }

    #[test]
    fn the_highest_valid_port_is_accepted() {
        assert_eq!(
            handshake_listen_port(&handshake(serde_json::json!({"listen_port": 65535}))),
            Some(65535)
        );
    }
}

#[cfg(test)]
mod one_shot_connections {
    use super::super::{MessageType, RelayMessage};
    use super::{DialIntent, handshake_is_transient};

    fn handshake(payload: serde_json::Value) -> RelayMessage {
        RelayMessage {
            id: "m1".into(),
            msg_type: MessageType::Handshake,
            from_peer: "peer-1".into(),
            timestamp: 0,
            payload,
        }
    }

    #[test]
    fn a_one_shot_says_so_and_advertises_no_port() {
        let intent = DialIntent::one_shot();
        assert!(intent.transient);
        assert_eq!(
            intent.listen_port, None,
            "a connection about to close must not teach an address"
        );
    }

    #[test]
    fn a_peer_link_does_not_claim_to_be_transient() {
        assert!(!DialIntent::peer_link(Some(9847)).transient);
        assert!(!DialIntent::peer_link(None).transient);
    }

    #[test]
    fn the_flag_is_read_when_present() {
        assert!(handshake_is_transient(&handshake(
            serde_json::json!({"proof": "ab", "transient": true})
        )));
    }

    // Every peer built before #487 omits the field. Defaulting to "not
    // transient" keeps such a connection a link, as it was — the safe
    // direction, since the alternative would stop registering real peers.
    #[test]
    fn an_absent_flag_means_a_peer_link() {
        assert!(!handshake_is_transient(&handshake(
            serde_json::json!({"proof": "ab", "version": "0.73.0"})
        )));
    }

    // Only an explicit `true` counts, so a malformed value cannot turn a real
    // peer link into a delivery that is never registered.
    #[test]
    fn anything_other_than_true_means_a_peer_link() {
        for v in [
            serde_json::json!(false),
            serde_json::json!("true"),
            serde_json::json!(1),
            serde_json::json!(null),
        ] {
            assert!(
                !handshake_is_transient(&handshake(serde_json::json!({"transient": v.clone()}))),
                "{v} must not be read as transient"
            );
        }
    }
}
