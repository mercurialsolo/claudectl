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

    if proof != expected_hex {
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

/// Client: send a handshake response with the HMAC proof.
pub fn send_handshake(
    stream: &mut TcpStream,
    identity: &str,
    nonce: &str,
    psk: &[u8; 32],
) -> io::Result<()> {
    let proof = compute_proof(nonce, psk);
    let msg = RelayMessage {
        id: gen_msg_id(),
        msg_type: MessageType::Handshake,
        from_peer: identity.to_string(),
        timestamp: epoch_ms(),
        payload: serde_json::json!({
            "proof": proof,
            "version": env!("CARGO_PKG_VERSION"),
        }),
    };
    write_message(stream, &msg)
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

/// Build a heartbeat message.
pub fn heartbeat_message(identity: &str) -> RelayMessage {
    RelayMessage {
        id: gen_msg_id(),
        msg_type: MessageType::Heartbeat,
        from_peer: identity.to_string(),
        timestamp: epoch_ms(),
        payload: serde_json::json!({}),
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
}
