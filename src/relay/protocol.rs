// Wire framing (NDJSON, and binary frames under `frame-bin`) and HMAC
// challenge-response authentication.

use std::io::{self, BufReader, Read, Write};
use std::net::TcpStream;

use super::crypto;
use super::features::{self, Features};
use super::{MessageType, RelayMessage, epoch_ms, gen_msg_id};

/// Maximum line size: 1 MB.
const MAX_LINE_SIZE: usize = 1_048_576;

// ────────────────────────────────────────────────────────────────────────────
// NDJSON framing
// ────────────────────────────────────────────────────────────────────────────

/// Write a RelayMessage as a single JSON line to the stream.
pub fn write_message<W: Write>(stream: &mut W, msg: &RelayMessage) -> io::Result<()> {
    let json =
        serde_json::to_string(msg).map_err(|e| io::Error::other(format!("serialize: {e}")))?;
    let line = format!("{json}\n");
    stream.write_all(line.as_bytes())?;
    stream.flush()
}

/// Read one RelayMessage from a buffered reader. Returns None on EOF.
///
/// A frame whose `msg_type` this build doesn't know is logged and skipped,
/// and the next frame is read (#559). Before, it failed to parse and every
/// caller treated that as a dead connection, so a newer peer adding any
/// message type disconnected every older one.
pub fn read_message<R: io::Read>(reader: &mut BufReader<R>) -> io::Result<Option<RelayMessage>> {
    loop {
        let Some(line) = read_line(reader)? else {
            return Ok(None);
        };
        let msg = parse_message(&line)?;
        if msg.msg_type != MessageType::Unknown {
            return Ok(Some(msg));
        }
        note_unknown(&line, &msg.from_peer);
    }
}

/// Parse one frame. Split out so the unknown-type path is testable without a
/// socket.
fn parse_message(line: &[u8]) -> io::Result<RelayMessage> {
    let text = std::str::from_utf8(line).map_err(|e| io::Error::other(format!("utf8: {e}")))?;
    serde_json::from_str(text.trim()).map_err(|e| io::Error::other(format!("parse: {e}")))
}

/// Log a skipped frame once per `(peer, msg_type)` for the life of the
/// process, so a newer peer sending one every heartbeat doesn't flood the log.
fn note_unknown(line: &[u8], from_peer: &str) {
    use std::collections::HashSet;
    use std::sync::{Mutex, OnceLock};
    static SEEN: OnceLock<Mutex<HashSet<(String, String)>>> = OnceLock::new();

    let name = serde_json::from_slice::<serde_json::Value>(line)
        .ok()
        .and_then(|v| {
            v.get("msg_type")
                .and_then(|t| t.as_str())
                .map(str::to_string)
        })
        .unwrap_or_default();
    // A peer chooses both strings; keep what's logged short and printable.
    let clip = |s: &str| -> String {
        s.chars()
            .filter(|c| c.is_ascii_graphic())
            .take(64)
            .collect()
    };
    let key = (clip(from_peer), clip(&name));
    // Bounded by count as well as length: a peer that invents a new type name
    // per frame would otherwise grow this for the life of the process.
    const MAX_SEEN: usize = 256;
    let first = SEEN
        .get_or_init(|| Mutex::new(HashSet::new()))
        .lock()
        .map(|mut seen| {
            if seen.contains(&key) {
                false
            } else if seen.len() < MAX_SEEN {
                seen.insert(key.clone())
            } else {
                if seen.len() == MAX_SEEN {
                    // One sentinel entry, so this line prints once.
                    seen.insert((String::new(), String::new()));
                    eprintln!(
                        "[{}] further unknown message types are skipped without logging",
                        crate::logger::timestamp_now()
                    );
                }
                false
            }
        })
        .unwrap_or(false);
    // stderr, like the relay's other operational warnings: `logger::log` is
    // silent unless a log file is configured, and this is the one sign that a
    // peer is sending something this build drops.
    if first {
        eprintln!(
            "[{}] skipping unknown message type '{}' from {}: the peer is newer than this build",
            crate::logger::timestamp_now(),
            key.1,
            key.0
        );
    }
}

/// Read one `\n`-terminated line, byte by byte up to `MAX_LINE_SIZE` so a
/// peer can't make this side buffer without bound. `None` on EOF before any
/// byte.
fn read_line<R: io::Read>(reader: &mut BufReader<R>) -> io::Result<Option<Vec<u8>>> {
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

    // A blank line has always read as EOF; keep that.
    if line.is_empty() {
        return Ok(None);
    }
    Ok(Some(line))
}

// ────────────────────────────────────────────────────────────────────────────
// Binary framing (feature `frame-bin`, #562)
// ────────────────────────────────────────────────────────────────────────────
//
//   ┌──────────────┬───────────┬──────────────────────────┐
//   │ len: u32 BE  │ flags: u8 │ body: len - 1 bytes      │
//   └──────────────┴───────────┴──────────────────────────┘
//   flags bit 0: body is `raw_len: u32 LE` + an LZ4 block of the JSON
//   flags bits 1-7: reserved, must be 0
//   body, decompressed if bit 0 is set: one RelayMessage as JSON
//
// The challenge, handshake and ack are always NDJSON. A link switches only
// after both ends agreed `frame-bin`, so an older peer never sees a binary
// byte. No base64 anywhere: it would cost back a third of what LZ4 saves.

/// Compress a frame only when its JSON is at least this long. Bus messages
/// and acks are tens of bytes, and LZ4 makes those bigger.
///
/// Part of what `frame-bin` means: a receiver rejects a compressed frame
/// below it, so changing it needs a new feature string.
pub const COMPRESS_THRESHOLD: usize = 1024;

/// The most a frame may carry on the wire (`len`), matching the NDJSON line cap.
pub const MAX_FRAME_LEN: usize = MAX_LINE_SIZE;

/// The most a compressed frame may expand to. Checked against the declared
/// size before anything is allocated, so a small frame can't expand into a
/// memory exhaustion. Part of what `frame-bin` means.
pub const MAX_RAW_LEN: usize = 4 * 1024 * 1024;

const FLAG_LZ4: u8 = 0b0000_0001;

/// How one connection's bytes are framed after its handshake.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Framing {
    /// One JSON object per line: every peer from before #562.
    Lines,
    /// Length-prefixed frames, LZ4 above the threshold.
    Binary,
}

impl Framing {
    /// The framing a link uses, from what its handshake agreed.
    pub fn for_link(features: &Features) -> Self {
        if features.has(features::FRAME_BIN) {
            Framing::Binary
        } else {
            Framing::Lines
        }
    }
}

/// Write one message in this connection's framing.
///
/// A binary frame is built whole and written with one `write_all`, so its
/// header can't interleave with another write on a cloned handle.
pub fn write_frame<W: Write>(w: &mut W, msg: &RelayMessage, framing: Framing) -> io::Result<()> {
    match framing {
        Framing::Lines => write_message(w, msg),
        Framing::Binary => {
            let frame = encode_frame(msg)?;
            w.write_all(&frame)?;
            w.flush()
        }
    }
}

/// Read one message in this connection's framing. `None` on a clean EOF.
/// Unknown message types are skipped in both framings (#559).
pub fn read_frame<R: Read>(
    reader: &mut BufReader<R>,
    framing: Framing,
) -> io::Result<Option<RelayMessage>> {
    match framing {
        Framing::Lines => read_message(reader),
        Framing::Binary => loop {
            let Some(json) = read_binary_body(reader)? else {
                return Ok(None);
            };
            let msg = parse_message(&json)?;
            if msg.msg_type != MessageType::Unknown {
                return Ok(Some(msg));
            }
            note_unknown(&json, &msg.from_peer);
        },
    }
}

/// Encode one message as a binary frame. Compressed only at or above the
/// threshold, and only when that actually makes it smaller.
pub fn encode_frame(msg: &RelayMessage) -> io::Result<Vec<u8>> {
    let json = serde_json::to_vec(msg).map_err(|e| io::Error::other(format!("serialize: {e}")))?;
    if json.len() > MAX_RAW_LEN {
        return Err(io::Error::other(format!(
            "message is {} bytes; the most a frame can carry is {MAX_RAW_LEN}",
            json.len()
        )));
    }

    let mut flags = 0u8;
    let mut body = json;
    if body.len() >= COMPRESS_THRESHOLD {
        let block = lz4_flex::block::compress(&body);
        if block.len() + 4 < body.len() {
            let mut packed = Vec::with_capacity(4 + block.len());
            packed.extend_from_slice(&(body.len() as u32).to_le_bytes());
            packed.extend_from_slice(&block);
            body = packed;
            flags |= FLAG_LZ4;
        }
    }

    let len = 1 + body.len();
    if len > MAX_FRAME_LEN {
        return Err(io::Error::other(format!(
            "frame is {len} bytes after compression; the most the wire allows is {MAX_FRAME_LEN}"
        )));
    }
    let mut frame = Vec::with_capacity(4 + len);
    frame.extend_from_slice(&(len as u32).to_be_bytes());
    frame.push(flags);
    frame.extend_from_slice(&body);
    Ok(frame)
}

/// Read one binary frame and return its JSON, decompressed. Every bound is
/// checked before the allocation it guards.
fn read_binary_body<R: Read>(reader: &mut R) -> io::Result<Option<Vec<u8>>> {
    let mut header = [0u8; 4];
    if !fill(reader, &mut header, false)? {
        return Ok(None);
    }
    let len = u32::from_be_bytes(header) as usize;
    if len == 0 || len > MAX_FRAME_LEN {
        return Err(frame_error(format!("frame length {len} out of range")));
    }

    let mut frame = vec![0u8; len];
    fill(reader, &mut frame, true)?;
    let flags = frame[0];
    if flags & !FLAG_LZ4 != 0 {
        return Err(frame_error(format!(
            "reserved flag bits set: {flags:#010b}"
        )));
    }
    let body = &frame[1..];
    if flags & FLAG_LZ4 == 0 {
        return Ok(Some(body.to_vec()));
    }

    if body.len() < 4 {
        return Err(frame_error("compressed frame has no size prefix".into()));
    }
    let raw_len = u32::from_le_bytes([body[0], body[1], body[2], body[3]]) as usize;
    // A sender never compresses below the threshold, so a frame that claims
    // to has been built by something that isn't following `frame-bin`.
    if !(COMPRESS_THRESHOLD..=MAX_RAW_LEN).contains(&raw_len) {
        return Err(frame_error(format!(
            "compressed frame claims {raw_len} bytes"
        )));
    }
    let json = lz4_flex::block::decompress(&body[4..], raw_len)
        .map_err(|e| frame_error(format!("lz4: {e}")))?;
    if json.len() != raw_len {
        return Err(frame_error(format!(
            "compressed frame claimed {raw_len} bytes and held {}",
            json.len()
        )));
    }
    Ok(Some(json))
}

fn frame_error(msg: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg)
}

/// Fill `buf` completely. `Ok(false)` only for EOF before the first byte of a
/// frame, which is a clean close.
///
/// A read timeout before the first byte is returned so the reader loop can
/// treat it as an idle link, as it always has. After the first byte it's
/// retried instead: giving up mid-frame would leave the next read starting in
/// the middle of this one, and every frame after it would be garbage.
fn fill<R: Read>(reader: &mut R, buf: &mut [u8], mut started: bool) -> io::Result<bool> {
    let mut filled = 0;
    while filled < buf.len() {
        match reader.read(&mut buf[filled..]) {
            Ok(0) if filled == 0 && !started => return Ok(false),
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "connection closed mid-frame",
                ));
            }
            Ok(n) => {
                filled += n;
                started = true;
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e)
                if started
                    && matches!(
                        e.kind(),
                        io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
                    ) => {}
            Err(e) => return Err(e),
        }
    }
    Ok(true)
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
    write_message(stream, &handshake_ack(identity, status, None))
}

/// Server: accept a handshake, answering with the protocol and features both
/// sides have (#559). Returns what was agreed, to store on the connection. A
/// dialler from before #559 gets the same ack it always did, and no features.
pub fn send_handshake_ack_negotiated<W: Write>(
    stream: &mut W,
    identity: &str,
    handshake: &RelayMessage,
) -> io::Result<Features> {
    let negotiated = features::negotiate(&handshake.payload);
    write_message(stream, &handshake_ack(identity, "ok", negotiated.as_ref()))?;
    Ok(negotiated.map(|(_, f)| f).unwrap_or_default())
}

fn handshake_ack(
    identity: &str,
    status: &str,
    negotiated: Option<&(u64, Features)>,
) -> RelayMessage {
    let mut payload = serde_json::json!({ "status": status });
    if let Some((proto, agreed)) = negotiated {
        payload["proto"] = serde_json::json!(proto);
        payload["features"] = serde_json::json!(agreed.names());
    }
    RelayMessage {
        id: gen_msg_id(),
        msg_type: MessageType::HandshakeAck,
        from_peer: identity.to_string(),
        timestamp: epoch_ms(),
        payload,
    }
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
        "proto": features::PROTO_VERSION,
        "features": Features::local().names(),
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

/// Client: wait for and parse the handshake ack. Returns the acceptor's peer
/// id and the features agreed for this link (#559); an acceptor from before
/// #559 means none.
pub fn await_handshake_ack(
    reader: &mut BufReader<TcpStream>,
) -> Result<(String, Features), String> {
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
        Ok((msg.from_peer, features::from_ack(&msg.payload)))
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
/// A heartbeat carrying sessions and, when the caller has probed them, this
/// machine's capabilities (#536).
///
/// `capabilities` is omitted rather than sent empty when absent: a peer running
/// an older build sends no such key, and the receiver has to tell "did not say"
/// from "said nothing is installed". Conflating them would make every older
/// peer look unusable.
pub fn heartbeat_full(
    identity: &str,
    sessions: &[serde_json::Value],
    capabilities: Option<&serde_json::Value>,
    listen_addrs: &[String],
) -> RelayMessage {
    let mut payload = serde_json::json!({
        "worker_id": identity,
        "timestamp": epoch_ms(),
        "sessions": sessions,
    });
    if let (Some(caps), Some(obj)) = (capabilities, payload.as_object_mut()) {
        obj.insert("capabilities".into(), caps.clone());
    }
    // #541: where this machine can be reached, so a peer that loses the path it
    // has can try another. Omitted when empty for the same reason
    // `capabilities` is: an older peer sends no such key, and "did not say"
    // must stay distinguishable from "has no addresses".
    if !listen_addrs.is_empty() {
        if let Some(obj) = payload.as_object_mut() {
            obj.insert("listen_addrs".into(), serde_json::json!(listen_addrs));
        }
    }
    RelayMessage {
        id: gen_msg_id(),
        msg_type: MessageType::Heartbeat,
        from_peer: identity.to_string(),
        timestamp: epoch_ms(),
        payload,
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Tests
// ────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn frames(lines: &[&str]) -> BufReader<std::io::Cursor<Vec<u8>>> {
        let mut bytes = Vec::new();
        for line in lines {
            bytes.extend_from_slice(line.as_bytes());
            bytes.push(b'\n');
        }
        BufReader::new(std::io::Cursor::new(bytes))
    }

    const FUTURE_FRAME: &str = r#"{"id":"1","msg_type":"bus_forward","from_peer":"newer","timestamp":0,"payload":{"to":"x"}}"#;
    const HEARTBEAT_FRAME: &str =
        r#"{"id":"2","msg_type":"heartbeat","from_peer":"newer","timestamp":0,"payload":{}}"#;

    /// The #559 regression: a frame from a newer peer used to fail to parse,
    /// and every caller read that as a dead link.
    #[test]
    fn an_unknown_message_type_is_skipped_not_fatal() {
        let mut reader = frames(&[FUTURE_FRAME, HEARTBEAT_FRAME]);
        let msg = read_message(&mut reader).unwrap().unwrap();
        assert_eq!(msg.msg_type, MessageType::Heartbeat);
        assert_eq!(msg.id, "2");
    }

    #[test]
    fn only_unknown_frames_then_eof_reads_as_eof() {
        let mut reader = frames(&[FUTURE_FRAME, FUTURE_FRAME]);
        assert!(read_message(&mut reader).unwrap().is_none());
    }

    #[test]
    fn malformed_json_is_still_an_error() {
        let mut reader = frames(&["{not json"]);
        assert!(read_message(&mut reader).is_err());
    }

    #[test]
    fn a_blank_line_still_reads_as_eof() {
        let mut reader = frames(&[""]);
        assert!(read_message(&mut reader).unwrap().is_none());
    }

    #[test]
    fn an_oversized_line_is_refused() {
        let big = "x".repeat(MAX_LINE_SIZE + 1);
        let mut reader = frames(&[&big]);
        assert!(read_message(&mut reader).is_err());
    }

    /// Every type this build sends must parse back as itself. A variant that
    /// serialised to a name the reader mapped to `Unknown` would be silently
    /// dropped by every peer, including one on this same build.
    #[test]
    fn every_message_type_round_trips() {
        use MessageType::*;
        // Matching exhaustively means a new variant fails to compile here
        // until it's added to the list below.
        let all = [
            Challenge,
            Handshake,
            HandshakeAck,
            Heartbeat,
            Ack,
            DelegateTask,
            TaskStatus,
            TaskHandoff,
            TaskInterrupt,
            KnowledgeSync,
            KnowledgeRequest,
            KnowledgeSnapshot,
            HiveJoinRequest,
            HiveJoinResult,
            KnowledgeRejected,
            SessionTransfer,
            SessionReceived,
        ];
        for t in all {
            match t {
                Challenge | Handshake | HandshakeAck | Heartbeat | Ack | DelegateTask
                | TaskStatus | TaskHandoff | TaskInterrupt | KnowledgeSync | KnowledgeRequest
                | KnowledgeSnapshot | HiveJoinRequest | HiveJoinResult | KnowledgeRejected
                | SessionTransfer | SessionReceived => {}
                Unknown => unreachable!("never sent"),
            }
            let msg = RelayMessage {
                id: "x".into(),
                msg_type: t,
                from_peer: "p".into(),
                timestamp: 0,
                payload: serde_json::json!({}),
            };
            let line = serde_json::to_string(&msg).unwrap();
            let mut reader = frames(&[&line]);
            let back = read_message(&mut reader).unwrap().unwrap();
            assert_eq!(back.msg_type, t, "{t:?} did not survive the wire");

            // And through binary frames (#562), both below the compression
            // threshold and padded well past it.
            for payload in [
                serde_json::json!({}),
                serde_json::json!({"pad": "session data ".repeat(400)}),
            ] {
                let msg = RelayMessage {
                    payload: payload.clone(),
                    ..msg.clone()
                };
                let back = binary_round_trip(&msg);
                assert_eq!(back.msg_type, t, "{t:?} did not survive a binary frame");
                assert_eq!(
                    back.payload, payload,
                    "{t:?} payload changed in a binary frame"
                );
            }
        }
    }

    // ── Binary framing (#562) ───────────────────────────────────────────

    fn msg_with(payload: serde_json::Value) -> RelayMessage {
        RelayMessage {
            id: "id-1".into(),
            msg_type: MessageType::Heartbeat,
            from_peer: "peer-a".into(),
            timestamp: 7,
            payload,
        }
    }

    fn binary_round_trip(msg: &RelayMessage) -> RelayMessage {
        let mut wire = Vec::new();
        write_frame(&mut wire, msg, Framing::Binary).unwrap();
        let mut reader = BufReader::new(std::io::Cursor::new(wire));
        read_frame(&mut reader, Framing::Binary).unwrap().unwrap()
    }

    fn read_binary(bytes: Vec<u8>) -> io::Result<Option<RelayMessage>> {
        read_frame(
            &mut BufReader::new(std::io::Cursor::new(bytes)),
            Framing::Binary,
        )
    }

    /// A frame with an arbitrary header and body, for the malformed cases.
    fn raw_frame(flags: u8, body: &[u8]) -> Vec<u8> {
        let mut f = ((1 + body.len()) as u32).to_be_bytes().to_vec();
        f.push(flags);
        f.extend_from_slice(body);
        f
    }

    fn lz4_body(raw_len: u32, block: &[u8]) -> Vec<u8> {
        let mut b = raw_len.to_le_bytes().to_vec();
        b.extend_from_slice(block);
        b
    }

    /// Text that LZ4 can't shrink: a deterministic byte soup.
    fn incompressible(n: usize) -> String {
        let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
        (0..n)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                char::from(b'!' + (x % 90) as u8)
            })
            .collect()
    }

    #[test]
    fn a_small_frame_goes_uncompressed() {
        let frame = encode_frame(&msg_with(serde_json::json!({"a": 1}))).unwrap();
        assert_eq!(frame[4] & FLAG_LZ4, 0);
        let json_len = serde_json::to_vec(&msg_with(serde_json::json!({"a": 1})))
            .unwrap()
            .len();
        assert!(json_len < COMPRESS_THRESHOLD);
        assert_eq!(frame.len(), 4 + 1 + json_len);
    }

    #[test]
    fn a_large_frame_is_compressed_and_smaller() {
        let msg = msg_with(serde_json::json!({"sessions": "status Idle cost 1.23 ".repeat(500)}));
        let json_len = serde_json::to_vec(&msg).unwrap().len();
        let frame = encode_frame(&msg).unwrap();
        assert_eq!(frame[4] & FLAG_LZ4, FLAG_LZ4);
        assert!(frame.len() * 3 < json_len, "{} vs {json_len}", frame.len());
        assert_eq!(binary_round_trip(&msg).payload, msg.payload);
    }

    #[test]
    fn a_large_frame_that_wont_shrink_goes_uncompressed() {
        let msg = msg_with(serde_json::json!({"noise": incompressible(4000)}));
        let frame = encode_frame(&msg).unwrap();
        assert_eq!(frame[4] & FLAG_LZ4, 0);
        assert_eq!(binary_round_trip(&msg).payload, msg.payload);
    }

    #[test]
    fn a_frame_at_exactly_the_threshold_round_trips() {
        // Pad until the JSON is exactly COMPRESS_THRESHOLD bytes.
        let base = serde_json::to_vec(&msg_with(serde_json::json!({"p": ""})))
            .unwrap()
            .len();
        let msg = msg_with(serde_json::json!({"p": "a".repeat(COMPRESS_THRESHOLD - base)}));
        assert_eq!(serde_json::to_vec(&msg).unwrap().len(), COMPRESS_THRESHOLD);
        assert_eq!(binary_round_trip(&msg).payload, msg.payload);
    }

    #[test]
    fn several_frames_in_a_row_each_read_whole() {
        let mut wire = Vec::new();
        let msgs = [
            msg_with(serde_json::json!({"n": 1})),
            msg_with(serde_json::json!({"n": "big ".repeat(1000)})),
            msg_with(serde_json::json!({"n": 3})),
        ];
        for m in &msgs {
            write_frame(&mut wire, m, Framing::Binary).unwrap();
        }
        let mut reader = BufReader::new(std::io::Cursor::new(wire));
        for m in &msgs {
            let back = read_frame(&mut reader, Framing::Binary).unwrap().unwrap();
            assert_eq!(back.payload, m.payload);
        }
        assert!(read_frame(&mut reader, Framing::Binary).unwrap().is_none());
    }

    #[test]
    fn an_unknown_type_in_a_binary_frame_is_skipped() {
        let mut wire = raw_frame(0, FUTURE_FRAME.as_bytes());
        wire.extend(raw_frame(0, HEARTBEAT_FRAME.as_bytes()));
        let back = read_binary(wire).unwrap().unwrap();
        assert_eq!(back.msg_type, MessageType::Heartbeat);
    }

    #[test]
    fn eof_at_a_frame_boundary_is_a_clean_close() {
        assert!(read_binary(Vec::new()).unwrap().is_none());
    }

    #[test]
    fn eof_inside_a_frame_is_an_error() {
        let mut frame = encode_frame(&msg_with(serde_json::json!({"a": 1}))).unwrap();
        frame.truncate(frame.len() - 3);
        let err = read_binary(frame).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
        // Inside the header too.
        let err = read_binary(vec![0, 0]).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
    }

    #[test]
    fn a_zero_length_frame_is_refused() {
        assert!(read_binary(0u32.to_be_bytes().to_vec()).is_err());
    }

    #[test]
    fn an_oversized_length_is_refused_before_reading_it() {
        let header = ((MAX_FRAME_LEN + 1) as u32).to_be_bytes().to_vec();
        let err = read_binary(header).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn reserved_flag_bits_are_refused() {
        for bit in 1..8 {
            let err = read_binary(raw_frame(1 << bit, HEARTBEAT_FRAME.as_bytes())).unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::InvalidData, "bit {bit}");
        }
    }

    #[test]
    fn a_compressed_frame_without_a_size_prefix_is_refused() {
        assert!(read_binary(raw_frame(FLAG_LZ4, &[1, 2])).is_err());
    }

    #[test]
    fn a_compressed_frame_claiming_less_than_the_threshold_is_refused() {
        let json = HEARTBEAT_FRAME.as_bytes();
        let block = lz4_flex::block::compress(json);
        let body = lz4_body(json.len() as u32, &block);
        assert!(json.len() < COMPRESS_THRESHOLD);
        let err = read_binary(raw_frame(FLAG_LZ4, &body)).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    /// The decompression bomb: a tiny frame that says it expands past the cap
    /// is refused on the declared size, before any allocation.
    #[test]
    fn a_compressed_frame_claiming_more_than_the_cap_is_refused() {
        let block = lz4_flex::block::compress(&vec![b'a'; 64 * 1024]);
        let body = lz4_body((MAX_RAW_LEN + 1) as u32, &block);
        let err = read_binary(raw_frame(FLAG_LZ4, &body)).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn a_size_prefix_that_disagrees_with_the_block_is_refused() {
        let json =
            serde_json::to_vec(&msg_with(serde_json::json!({"p": "x".repeat(3000)}))).unwrap();
        let block = lz4_flex::block::compress(&json);
        // Claims more than the block holds.
        let body = lz4_body(json.len() as u32 + 10, &block);
        assert!(read_binary(raw_frame(FLAG_LZ4, &body)).is_err());
        // Claims less than the block holds.
        let body = lz4_body(json.len() as u32 - 10, &block);
        assert!(read_binary(raw_frame(FLAG_LZ4, &body)).is_err());
    }

    #[test]
    fn a_corrupted_block_is_refused_not_panicked_on() {
        let json =
            serde_json::to_vec(&msg_with(serde_json::json!({"p": "y".repeat(3000)}))).unwrap();
        let mut block = lz4_flex::block::compress(&json);
        // Flip bytes through the block; every variant must error or produce
        // something that fails to parse, never panic.
        for i in (0..block.len()).step_by(7) {
            block[i] ^= 0xA5;
            let body = lz4_body(json.len() as u32, &block);
            let _ = read_binary(raw_frame(FLAG_LZ4, &body));
            block[i] ^= 0xA5;
        }
        // A block that's pure garbage.
        let body = lz4_body(json.len() as u32, &[0xFF; 32]);
        assert!(read_binary(raw_frame(FLAG_LZ4, &body)).is_err());
    }

    #[test]
    fn a_valid_frame_holding_invalid_json_is_refused() {
        assert!(read_binary(raw_frame(0, b"{not json")).is_err());
    }

    #[test]
    fn a_message_too_big_for_a_frame_is_refused_at_send() {
        let msg = msg_with(serde_json::json!({"p": incompressible(MAX_RAW_LEN)}));
        assert!(encode_frame(&msg).is_err());
        // And one that fits the raw cap but can't compress under the wire cap.
        let msg = msg_with(serde_json::json!({"p": incompressible(MAX_FRAME_LEN + 10)}));
        assert!(encode_frame(&msg).is_err());
    }

    #[test]
    fn framing_follows_the_agreed_features() {
        assert_eq!(Framing::for_link(&Features::default()), Framing::Lines);
        let (_, agreed) =
            features::negotiate(&serde_json::json!({"proto": 1, "features": ["frame-bin"]}))
                .unwrap();
        assert_eq!(Framing::for_link(&agreed), Framing::Binary);
    }

    /// The mini on 0.80 sends no `proto`. Its link must stay on NDJSON, or
    /// this release would disconnect every peer that hasn't upgraded.
    #[test]
    fn a_peer_from_before_559_stays_on_lines() {
        let agreed = features::negotiate(&serde_json::json!({"proof": "x", "version": "0.80.0"}))
            .map(|(_, f)| f)
            .unwrap_or_default();
        assert_eq!(Framing::for_link(&agreed), Framing::Lines);
    }

    /// A read timeout part-way through a frame must not desync the stream:
    /// the reader keeps going until the frame is whole (#562).
    #[test]
    fn a_frame_split_across_a_read_timeout_still_arrives_whole() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let mut writer = TcpStream::connect(addr).unwrap();
        let (server, _) = listener.accept().unwrap();
        server
            .set_read_timeout(Some(std::time::Duration::from_millis(10)))
            .unwrap();

        let msg = msg_with(serde_json::json!({"p": "split ".repeat(400)}));
        let frame = encode_frame(&msg).unwrap();
        let (a, b) = frame.split_at(3);
        let rest = b.to_vec();
        writer.write_all(a).unwrap();
        let t = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(120));
            writer.write_all(&rest).unwrap();
            writer
        });

        let mut reader = BufReader::new(server);
        let back = read_frame(&mut reader, Framing::Binary).unwrap().unwrap();
        assert_eq!(back.payload, msg.payload);
        drop(t.join().unwrap());
    }

    /// Before the first byte, a timeout is still returned, so the reader
    /// loop can treat an idle link as idle, as it always has.
    #[test]
    fn a_timeout_before_any_byte_is_returned_to_the_caller() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let _writer = TcpStream::connect(addr).unwrap();
        let (server, _) = listener.accept().unwrap();
        server
            .set_read_timeout(Some(std::time::Duration::from_millis(10)))
            .unwrap();
        let err = read_frame(&mut BufReader::new(server), Framing::Binary).unwrap_err();
        assert!(
            matches!(
                err.kind(),
                io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
            ),
            "{err:?}"
        );
    }

    #[test]
    fn a_handshake_advertises_protocol_and_features() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let mut client = TcpStream::connect(addr).unwrap();
        let (server, _) = listener.accept().unwrap();
        let psk = crypto::generate_psk();
        send_handshake(
            &mut client,
            "me",
            "nonce",
            &psk,
            &DialIntent::peer_link(None),
        )
        .unwrap();
        let msg = read_message(&mut BufReader::new(server)).unwrap().unwrap();
        assert_eq!(
            msg.payload["proto"],
            serde_json::json!(features::PROTO_VERSION)
        );
        assert!(
            msg.payload["features"]
                .as_array()
                .unwrap()
                .contains(&serde_json::json!(features::UNKNOWN_OK))
        );
        // The proof still verifies with the new keys present.
        assert_eq!(verify_handshake(&msg, "nonce", &psk).unwrap(), "me");
    }

    #[test]
    fn negotiation_over_a_socket_agrees_on_both_ends() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let mut client = TcpStream::connect(addr).unwrap();
        let (mut server, _) = listener.accept().unwrap();
        let psk = crypto::generate_psk();

        send_handshake(
            &mut client,
            "dialler",
            "n",
            &psk,
            &DialIntent::peer_link(None),
        )
        .unwrap();
        let mut server_reader = BufReader::new(server.try_clone().unwrap());
        let handshake = read_message(&mut server_reader).unwrap().unwrap();
        let server_side =
            send_handshake_ack_negotiated(&mut server, "acceptor", &handshake).unwrap();

        let (peer, client_side) = await_handshake_ack(&mut BufReader::new(client)).unwrap();
        assert_eq!(peer, "acceptor");
        assert_eq!(server_side, client_side);
        assert!(client_side.has(features::UNKNOWN_OK));
    }

    /// A 0.80 dialler sends no `proto`. Its ack must look exactly as it did,
    /// and the link gets no features.
    #[test]
    fn a_pre_negotiation_dialler_gets_the_old_ack() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let client = TcpStream::connect(addr).unwrap();
        let (mut server, _) = listener.accept().unwrap();
        let old_handshake = RelayMessage {
            id: "h".into(),
            msg_type: MessageType::Handshake,
            from_peer: "old".into(),
            timestamp: 0,
            payload: serde_json::json!({"proof": "p", "version": "0.80.0"}),
        };
        let agreed =
            send_handshake_ack_negotiated(&mut server, "acceptor", &old_handshake).unwrap();
        assert!(agreed.is_empty());
        let ack = read_message(&mut BufReader::new(client)).unwrap().unwrap();
        assert_eq!(ack.payload, serde_json::json!({"status": "ok"}));
    }

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
    fn heartbeat_with_sessions_includes_payload() {
        let sessions = vec![
            serde_json::json!({"pid": 1234, "project": "backend", "status": "Processing"}),
            serde_json::json!({"pid": 5678, "project": "frontend", "status": "Idle"}),
        ];
        let msg = heartbeat_full("worker-01", &sessions, None, &[]);
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
        let msg = heartbeat_full("worker-02", &[], None, &[]);
        let sessions = msg.payload.get("sessions").and_then(|v| v.as_array());
        assert!(sessions.is_some());
        assert_eq!(sessions.unwrap().len(), 0);
    }

    /// A peer older than #536 sends no `capabilities` key, and a peer that has
    /// not probed yet sends none either. Both must stay indistinguishable from
    /// the heartbeat this protocol has always sent, or every older peer in a
    /// hive starts looking like it has nothing installed.
    #[test]
    fn a_heartbeat_without_capabilities_is_byte_identical_to_the_old_one() {
        let msg = heartbeat_full("worker-03", &[], None, &[]);
        assert!(msg.payload.get("capabilities").is_none());
        let keys: Vec<&str> = msg
            .payload
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(keys, vec!["sessions", "timestamp", "worker_id"]);
    }

    #[test]
    fn a_heartbeat_carries_the_manifest_when_one_is_probed() {
        let caps = serde_json::json!({
            "claudectl_version": "0.79.0",
            "os": "linux",
            "arch": "x86_64",
            "tools": {"claude": {"state": "present", "version": "2.1.0"}},
        });
        let msg = heartbeat_full("worker-04", &[], Some(&caps), &[]);
        assert_eq!(msg.payload.get("capabilities"), Some(&caps));
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
