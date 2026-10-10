// Moving a conversation between hosts (#478 item 3).
//
// A Claude Code session is a single file: `~/.claude/projects/<slug of
// cwd>/<session_id>.jsonl`. Three things about it decide this module's shape,
// each established by running it rather than reading the code:
//
//  1. A transcript copied into a *different* project directory resumes, and
//     recalls what was said, even though every record inside still names the
//     original `cwd`. Resume keys on the file's location and name, not on the
//     `cwd` recorded within.
//  2. Resume looks *only* in the project directory derived from the current
//     working directory — there is no search. So placement has to be exactly
//     right; a transcript in the wrong directory is not resumable at all.
//     (That is why `cwd_to_slug` had to be fixed first, #501.)
//  3. The `<session_id>/` sidecar directory — subagent transcripts, tool
//     result overflow — is *not* needed. A session whose subagent returned a
//     word still recalls that word with the sidecar left behind, because the
//     subagent's result is in the main transcript as a tool result.
//
// So the payload is one file. Measured on a real machine: median 5.45 MB, p90
// 35 MB, max 198 MB across 319 transcripts. `protocol.rs` caps a frame at
// 1 MiB, so it is chunked; see `MAX_TRANSCRIPT_BYTES` for why it is not
// compressed.

use std::fs;
use std::path::{Path, PathBuf};

use super::{crypto, relay_dir};

/// Raw bytes per chunk, before base64.
///
/// Base64 expands by exactly 4/3, so 512 KiB becomes ~699 KB, which leaves
/// roughly 30% of `protocol::MAX_LINE_SIZE` for the JSON envelope. Picking the
/// raw size rather than the encoded one keeps that headroom predictable:
/// embedding the JSONL as a JSON string instead would expand by an amount that
/// depends on its contents and can approach 2x.
pub const CHUNK_RAW_BYTES: usize = 512 * 1024;

/// The largest transcript this will send, and the reason it has a limit at
/// all: nothing in the dependency tree compresses, and adding the first
/// compression crate is a decision for the maintainer, not for this module
/// (#503 carries the measurements). 64 MB covers ~93% of real transcripts;
/// past that the sender refuses and says the measured size.
///
/// It also bounds memory. The receiver's reader thread drains the socket into
/// a channel as fast as it can parse, so without a cap the channel could hold
/// an entire 198 MB transfer before the serve loop writes a single byte.
pub const MAX_TRANSCRIPT_BYTES: u64 = 64 * 1024 * 1024;

// ────────────────────────────────────────────────────────────────────────────
// Base64 (RFC 4648 §4, standard alphabet, padded)
// ────────────────────────────────────────────────────────────────────────────
//
// Hand-rolled for the same reason `crypto.rs` hand-rolls SHA-256 and HMAC: the
// `base64` crate is in the tree only transitively under `bus`, and `relay`
// must not depend on `bus` (#482).

const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

pub fn b64_encode(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for group in data.chunks(3) {
        let b = [
            group[0],
            group.get(1).copied().unwrap_or(0),
            group.get(2).copied().unwrap_or(0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        out.push(ALPHABET[(n >> 18) as usize & 63] as char);
        out.push(ALPHABET[(n >> 12) as usize & 63] as char);
        out.push(if group.len() > 1 {
            ALPHABET[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if group.len() > 2 {
            ALPHABET[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

fn b64_value(c: u8) -> Option<u32> {
    match c {
        b'A'..=b'Z' => Some(u32::from(c - b'A')),
        b'a'..=b'z' => Some(u32::from(c - b'a') + 26),
        b'0'..=b'9' => Some(u32::from(c - b'0') + 52),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    }
}

pub fn b64_decode(text: &str) -> Result<Vec<u8>, String> {
    let bytes = text.as_bytes();
    if bytes.len() % 4 != 0 {
        return Err(format!(
            "base64 length {} is not a multiple of 4",
            bytes.len()
        ));
    }
    let mut out = Vec::with_capacity(bytes.len() / 4 * 3);
    for (i, quad) in bytes.chunks(4).enumerate() {
        let last = i == bytes.len() / 4 - 1;
        let pad = if last {
            quad.iter().filter(|&&c| c == b'=').count()
        } else {
            if quad.contains(&b'=') {
                return Err("base64 padding before the final group".into());
            }
            0
        };
        if pad > 2 {
            return Err("base64 group has more than two padding characters".into());
        }
        let mut n = 0u32;
        for &c in quad {
            let v = if c == b'=' {
                0
            } else {
                b64_value(c).ok_or_else(|| format!("invalid base64 character {:?}", c as char))?
            };
            n = (n << 6) | v;
        }
        out.push((n >> 16) as u8);
        if pad < 2 {
            out.push((n >> 8) as u8);
        }
        if pad < 1 {
            out.push(n as u8);
        }
    }
    Ok(out)
}

// ────────────────────────────────────────────────────────────────────────────
// Placement
// ────────────────────────────────────────────────────────────────────────────

/// Where a transcript for `session_id` must land so that `claude --resume`
/// run from `cwd` will find it.
pub fn transcript_path(cwd: &str, session_id: &str) -> Option<PathBuf> {
    if !is_valid_session_id(session_id) {
        return None;
    }
    Some(claudectl_core::discovery::project_dir_for(cwd).join(format!("{session_id}.jsonl")))
}

/// Which transcript `send-session` should read, and the id that travels with
/// its bytes.
///
/// Two callers, two situations. The CLI is given a `--cwd` and a session id
/// and derives the path, which is a slug computation that misses: #501 took
/// the match rate from 275 to 296 of the 316 transcripts on one machine, and
/// session discovery papers over the remainder with a full project scan
/// (`discovery::resolve_jsonl_paths` priority 4). The TUI already holds that
/// scan's answer in `ClaudeSession.jsonl_path`, so it passes the path and
/// skips the derivation entirely.
///
/// When a path is given, the session id is its **file stem**, not what the
/// caller believes the session is called. Discovery's priorities 2 and 3 can
/// return a transcript belonging to a different id — a `--resume` uuid, or
/// simply the newest file in the project directory — and labelling those
/// bytes with the caller's id would resume the wrong conversation on the far
/// side.
pub fn resolve_source(
    explicit: Option<&Path>,
    cwd: &str,
    session_id: &str,
) -> Result<(String, PathBuf), String> {
    let Some(path) = explicit else {
        let path = transcript_path(cwd, session_id)
            .ok_or_else(|| format!("'{session_id}' is not a usable session id"))?;
        return Ok((session_id.to_string(), path));
    };

    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .ok_or_else(|| format!("{} has no readable file name", path.display()))?;
    if !is_valid_session_id(stem) {
        return Err(format!("'{stem}' is not a usable session id"));
    }
    if !path.is_file() {
        return Err(format!("no transcript at {}", path.display()));
    }
    Ok((stem.to_string(), path.to_path_buf()))
}

/// A session id becomes a filename, so it must not be able to escape the
/// directory or name something else. Same reasoning as `is_valid_peer_id` and
/// `tasks::is_valid_task_id`; Claude Code's ids are UUIDs, so this is loose
/// enough to accept them and nothing with a separator in it.
pub fn is_valid_session_id(session_id: &str) -> bool {
    !session_id.is_empty()
        && session_id.len() <= 128
        && session_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

// ────────────────────────────────────────────────────────────────────────────
// Chunking
// ────────────────────────────────────────────────────────────────────────────

/// One chunk of a transcript, as it goes on the wire.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chunk {
    pub session_id: String,
    /// The working directory the sender had. The receiver uses it only as the
    /// default placement; `--remote-cwd` overrides it, and on a Linux
    /// receiver a sender's `/Users/...` path will essentially never exist.
    pub cwd: String,
    pub seq: u32,
    pub total: u32,
    /// Hex SHA-256 of the *whole* transcript, identical on every chunk. It
    /// names the partial file, verifies the result, and makes a re-send
    /// idempotent.
    pub digest: String,
    pub data: Vec<u8>,
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Split a transcript into chunks. Returns an error rather than a huge
/// transfer when the file exceeds `MAX_TRANSCRIPT_BYTES`.
pub fn split(session_id: &str, cwd: &str, contents: &[u8]) -> Result<Vec<Chunk>, String> {
    if !is_valid_session_id(session_id) {
        return Err(format!("not a usable session id: {session_id:?}"));
    }
    if contents.len() as u64 > MAX_TRANSCRIPT_BYTES {
        return Err(format!(
            "transcript is {:.1} MB, over the {:.0} MB limit — raise it with --max-size",
            contents.len() as f64 / 1_048_576.0,
            MAX_TRANSCRIPT_BYTES as f64 / 1_048_576.0,
        ));
    }
    let digest = hex(&crypto::sha256(contents));
    // An empty transcript still sends one chunk, so the receiver has
    // something to verify and place rather than nothing at all.
    let groups: Vec<&[u8]> = if contents.is_empty() {
        vec![&[]]
    } else {
        contents.chunks(CHUNK_RAW_BYTES).collect()
    };
    let total = groups.len() as u32;
    Ok(groups
        .into_iter()
        .enumerate()
        .map(|(i, data)| Chunk {
            session_id: session_id.to_string(),
            cwd: cwd.to_string(),
            seq: i as u32,
            total,
            digest: digest.clone(),
            data: data.to_vec(),
        })
        .collect())
}

// ────────────────────────────────────────────────────────────────────────────
// Reassembly
// ────────────────────────────────────────────────────────────────────────────

/// Where partial transfers accumulate.
fn transfers_dir() -> PathBuf {
    relay_dir().join("transfers")
}

/// What accepting a chunk did.
#[derive(Debug, PartialEq, Eq)]
pub enum Accepted {
    /// Took the chunk; more are expected.
    More { have: u32, of: u32 },
    /// The last chunk arrived, the digest matched, and the transcript is now
    /// at this path.
    Placed(PathBuf),
    /// The transcript was already there, byte-identical. A re-send is not an
    /// error.
    AlreadyPresent(PathBuf),
}

/// Accept one chunk against an explicit directory root.
///
/// `transfers_root` holds the partial file; `target` is where the finished
/// transcript goes. Both are parameters so a test drives the real code in a
/// temporary directory — see #468 for why this is not done with `HOME`.
pub fn accept_chunk_in(
    transfers_root: &Path,
    target: &Path,
    chunk: &Chunk,
) -> Result<Accepted, String> {
    if chunk.total == 0 || chunk.seq >= chunk.total {
        return Err(format!(
            "chunk {} of {} is not a possible position",
            chunk.seq, chunk.total
        ));
    }
    if chunk.digest.len() != 64 || !chunk.digest.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(format!("not a sha-256 digest: {:?}", chunk.digest));
    }

    // An identical transcript already in place: idempotent success. A
    // *different* one is refused below rather than overwritten.
    if let Ok(existing) = fs::read(target) {
        if hex(&crypto::sha256(&existing)) == chunk.digest {
            return Ok(Accepted::AlreadyPresent(target.to_path_buf()));
        }
    }

    fs::create_dir_all(transfers_root).map_err(|e| format!("create transfers dir: {e}"))?;
    let part = transfers_root.join(format!("{}.part", chunk.digest));

    // Chunks travel over one TCP connection, so they arrive in order. Rather
    // than buffer out-of-order chunks, insist on the order and say so — a gap
    // means something is wrong that silently filling it would hide.
    let have = fs::metadata(&part).map(|m| m.len()).unwrap_or(0);
    let expected_offset = u64::from(chunk.seq) * CHUNK_RAW_BYTES as u64;
    if chunk.seq == 0 {
        let _ = fs::remove_file(&part);
    } else if have != expected_offset {
        return Err(format!(
            "chunk {} expects {} bytes already written, found {}",
            chunk.seq, expected_offset, have
        ));
    }

    {
        use std::io::Write;
        let mut f = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&part)
            .map_err(|e| format!("open partial transfer: {e}"))?;
        f.write_all(&chunk.data)
            .map_err(|e| format!("write partial transfer: {e}"))?;
    }

    if chunk.seq + 1 < chunk.total {
        return Ok(Accepted::More {
            have: chunk.seq + 1,
            of: chunk.total,
        });
    }

    // Last chunk: the digest is the whole point of carrying it.
    let whole = fs::read(&part).map_err(|e| format!("read partial transfer: {e}"))?;
    let got = hex(&crypto::sha256(&whole));
    if got != chunk.digest {
        let _ = fs::remove_file(&part);
        return Err(format!(
            "transfer digest mismatch: expected {}, got {} — discarded",
            chunk.digest, got
        ));
    }

    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("create project dir: {e}"))?;
    }
    if target.exists() {
        // Present but a different digest, or the earlier read failed.
        let _ = fs::remove_file(&part);
        return Err(format!(
            "{} already holds a different transcript; refusing to overwrite it",
            target.display()
        ));
    }
    // Rename rather than copy, so the transcript appears whole or not at all.
    fs::rename(&part, target).map_err(|e| format!("place transcript: {e}"))?;
    Ok(Accepted::Placed(target.to_path_buf()))
}

/// Accept one chunk into the real relay store, placing the transcript where
/// `claude --resume` run from `cwd` will find it.
pub fn accept_chunk(chunk: &Chunk, cwd: &str) -> Result<Accepted, String> {
    let target = transcript_path(cwd, &chunk.session_id)
        .ok_or_else(|| format!("not a usable session id: {:?}", chunk.session_id))?;
    accept_chunk_in(&transfers_dir(), &target, chunk)
}

/// What to tell the sender, and what the person there has to type. Placement
/// is load-bearing and the receiver is the only side that knows where the
/// transcript actually landed.
pub fn resume_hint(cwd: &str, session_id: &str) -> String {
    format!("cd {cwd} && claude --resume {session_id} --fork-session")
}

// ────────────────────────────────────────────────────────────────────────────
// Wire format
// ────────────────────────────────────────────────────────────────────────────

/// Build the message carrying one chunk.
pub fn build_chunk_message(chunk: &Chunk, identity: &str) -> super::RelayMessage {
    super::RelayMessage {
        id: super::gen_msg_id(),
        msg_type: super::MessageType::SessionTransfer,
        from_peer: identity.to_string(),
        timestamp: super::epoch_ms(),
        payload: serde_json::json!({
            "session_id": chunk.session_id,
            "cwd": chunk.cwd,
            "seq": chunk.seq,
            "total": chunk.total,
            "digest": chunk.digest,
            "data": b64_encode(&chunk.data),
        }),
    }
}

/// Read a chunk back off the wire. `None` for anything malformed — a peer
/// cannot be trusted to send well-formed chunks, and a partial transfer that
/// silently accepts a garbled one would fail the digest much later with no
/// clue where it went wrong.
pub fn parse_chunk(payload: &serde_json::Value) -> Option<Chunk> {
    let session_id = payload.get("session_id")?.as_str()?.to_string();
    if !is_valid_session_id(&session_id) {
        return None;
    }
    let seq = u32::try_from(payload.get("seq")?.as_u64()?).ok()?;
    let total = u32::try_from(payload.get("total")?.as_u64()?).ok()?;
    let data = b64_decode(payload.get("data")?.as_str()?).ok()?;
    Some(Chunk {
        session_id,
        cwd: payload.get("cwd")?.as_str()?.to_string(),
        seq,
        total,
        digest: payload.get("digest")?.as_str()?.to_string(),
        data,
    })
}

/// Build the receiver's acknowledgement: where the transcript landed and the
/// command that resumes it.
pub fn build_received_message(
    session_id: &str,
    path: &Path,
    cwd: &str,
    identity: &str,
) -> super::RelayMessage {
    super::RelayMessage {
        id: super::gen_msg_id(),
        msg_type: super::MessageType::SessionReceived,
        from_peer: identity.to_string(),
        timestamp: super::epoch_ms(),
        payload: serde_json::json!({
            "session_id": session_id,
            "path": path.display().to_string(),
            "cwd": cwd,
            "resume": resume_hint(cwd, session_id),
        }),
    }
}

/// `(session_id, path, resume command)` from an acknowledgement.
pub fn parse_received(payload: &serde_json::Value) -> Option<(String, String, String)> {
    Some((
        payload.get("session_id")?.as_str()?.to_string(),
        payload.get("path")?.as_str()?.to_string(),
        payload.get("resume")?.as_str()?.to_string(),
    ))
}

// ────────────────────────────────────────────────────────────────────────────
// Sender's ledger
// ────────────────────────────────────────────────────────────────────────────
//
// `relay send-session` is a one-shot process like `relay delegate`: it sends
// and exits. The acknowledgement arrives at whatever process is serving, which
// is never the same one, so it has to land on disk to be seen at all (#490).

fn ledger_path(session_id: &str) -> Option<PathBuf> {
    if !is_valid_session_id(session_id) {
        return None;
    }
    Some(transfers_dir().join(format!("{session_id}.json")))
}

/// Record that this host sent a transcript, before it sends it.
pub fn record_sent(session_id: &str, peer: &str, cwd: &str, bytes: u64) -> Result<(), String> {
    let path = ledger_path(session_id)
        .ok_or_else(|| format!("not a usable session id: {session_id:?}"))?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("create transfers dir: {e}"))?;
    }
    let record = serde_json::json!({
        "session_id": session_id,
        "peer": peer,
        "cwd": cwd,
        "bytes": bytes,
        "state": "sent",
        "sent_at": super::epoch_ms(),
    });
    fs::write(
        &path,
        serde_json::to_string_pretty(&record).unwrap_or_default(),
    )
    .map_err(|e| format!("write transfer record: {e}"))
}

/// Record the receiver's acknowledgement against a transfer this host sent.
/// Returns false when there is no such record, so the serve loop can say so
/// rather than inventing one.
pub fn record_received(session_id: &str, path: &str, resume: &str) -> bool {
    let Some(ledger) = ledger_path(session_id) else {
        return false;
    };
    let Ok(text) = fs::read_to_string(&ledger) else {
        return false;
    };
    let Ok(mut record) = serde_json::from_str::<serde_json::Value>(&text) else {
        return false;
    };
    if let Some(obj) = record.as_object_mut() {
        obj.insert("state".into(), "received".into());
        obj.insert("remote_path".into(), path.into());
        obj.insert("resume".into(), resume.into());
        obj.insert("received_at".into(), super::epoch_ms().into());
    }
    fs::write(
        &ledger,
        serde_json::to_string_pretty(&record).unwrap_or_default(),
    )
    .is_ok()
}

/// Every transfer this host has sent, newest first.
pub fn list() -> Vec<serde_json::Value> {
    let Ok(entries) = fs::read_dir(transfers_dir()) else {
        return Vec::new();
    };
    let mut out: Vec<serde_json::Value> = entries
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
        .filter_map(|e| fs::read_to_string(e.path()).ok())
        .filter_map(|t| serde_json::from_str(&t).ok())
        .collect();
    out.sort_by_key(|r| std::cmp::Reverse(r.get("sent_at").and_then(|v| v.as_u64()).unwrap_or(0)));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── base64 ──────────────────────────────────────────────────────────

    /// The RFC 4648 §10 test vectors, which is the point of hand-rolling it:
    /// a round-trip test against itself would pass with a wrong alphabet.
    #[test]
    fn base64_matches_rfc4648_vectors() {
        for (plain, encoded) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(b64_encode(plain.as_bytes()), encoded, "encoding {plain:?}");
            assert_eq!(
                b64_decode(encoded).unwrap(),
                plain.as_bytes(),
                "decoding {encoded:?}"
            );
        }
    }

    /// Every byte value, so the `+` and `/` end of the alphabet is covered.
    #[test]
    fn base64_round_trips_all_byte_values() {
        let all: Vec<u8> = (0..=255).collect();
        assert_eq!(b64_decode(&b64_encode(&all)).unwrap(), all);
    }

    #[test]
    fn base64_rejects_malformed_input() {
        assert!(b64_decode("Zg=").is_err(), "length not a multiple of 4");
        assert!(b64_decode("Zm9v!!!!").is_err(), "invalid character");
        assert!(b64_decode("Zg==Zg==").is_err(), "padding before the end");
    }

    // ── chunking ────────────────────────────────────────────────────────

    fn body(n: usize) -> Vec<u8> {
        // Varying bytes, so a chunk reassembled in the wrong order fails.
        (0..n).map(|i| (i % 251) as u8).collect()
    }

    #[test]
    fn split_covers_the_whole_transcript_when_not_a_chunk_multiple() {
        let data = body(CHUNK_RAW_BYTES * 2 + 7);
        let chunks = split("s-1", "/tmp/p", &data).unwrap();
        assert_eq!(chunks.len(), 3, "two full chunks and a 7-byte remainder");
        assert_eq!(chunks[2].data.len(), 7);
        assert!(chunks.iter().all(|c| c.total == 3));
        assert_eq!(
            chunks
                .iter()
                .flat_map(|c| c.data.clone())
                .collect::<Vec<_>>(),
            data,
            "concatenated chunks must equal the original"
        );
        let one = chunks[0].digest.clone();
        assert!(
            chunks.iter().all(|c| c.digest == one),
            "every chunk carries the whole file's digest"
        );
    }

    #[test]
    fn split_refuses_a_transcript_over_the_limit() {
        // One byte past the cap, actually allocated: the point is that `split`
        // produces the refusal, so the test must not produce it itself.
        let oversized = vec![0u8; MAX_TRANSCRIPT_BYTES as usize + 1];
        let msg = split("s-1", "/tmp/p", &oversized).unwrap_err();
        assert!(msg.contains("over the 64 MB limit"), "got: {msg}");
        assert!(msg.contains("--max-size"), "must say how to raise it");

        // And exactly at the cap it does not refuse.
        let at_limit = vec![0u8; MAX_TRANSCRIPT_BYTES as usize];
        assert!(
            split("s-1", "/tmp/p", &at_limit).is_ok(),
            "the cap is inclusive"
        );
    }

    #[test]
    fn split_rejects_a_session_id_that_is_not_a_safe_filename() {
        for bad in ["", "../escape", "a/b", "with space"] {
            assert!(split(bad, "/tmp/p", b"x").is_err(), "accepted {bad:?}");
        }
    }

    // ── reassembly ──────────────────────────────────────────────────────

    fn roundtrip(len: usize) {
        let tmp = tempfile::tempdir().unwrap();
        let data = body(len);
        let chunks = split("sess-abc", "/tmp/proj", &data).unwrap();
        let target = tmp.path().join("projects").join("sess-abc.jsonl");

        let mut placed = None;
        for c in &chunks {
            match accept_chunk_in(&tmp.path().join("transfers"), &target, c).unwrap() {
                Accepted::More { have, of } => {
                    assert_eq!(of, chunks.len() as u32);
                    assert!(have < of);
                }
                Accepted::Placed(p) => placed = Some(p),
                Accepted::AlreadyPresent(p) => placed = Some(p),
            }
        }
        assert_eq!(placed.as_deref(), Some(target.as_path()));
        assert_eq!(fs::read(&target).unwrap(), data, "bytes survived the trip");
        assert!(
            fs::read_dir(tmp.path().join("transfers"))
                .map(|d| d.count() == 0)
                .unwrap_or(true),
            "the partial file is consumed, not left behind"
        );
    }

    #[test]
    fn a_single_chunk_transcript_round_trips() {
        roundtrip(1024);
    }

    #[test]
    fn a_multi_chunk_transcript_round_trips_on_a_non_multiple_size() {
        roundtrip(CHUNK_RAW_BYTES * 2 + 123);
    }

    #[test]
    fn an_exact_chunk_multiple_round_trips() {
        roundtrip(CHUNK_RAW_BYTES * 2);
    }

    #[test]
    fn a_corrupted_chunk_is_caught_by_the_digest_and_nothing_is_placed() {
        let tmp = tempfile::tempdir().unwrap();
        let data = body(CHUNK_RAW_BYTES + 10);
        let mut chunks = split("sess-abc", "/tmp/proj", &data).unwrap();
        let target = tmp.path().join("sess-abc.jsonl");
        let transfers = tmp.path().join("transfers");

        chunks[1].data[0] ^= 0xff;
        accept_chunk_in(&transfers, &target, &chunks[0]).unwrap();
        let err = accept_chunk_in(&transfers, &target, &chunks[1]).unwrap_err();

        assert!(err.contains("digest mismatch"), "got: {err}");
        assert!(!target.exists(), "a failed transfer places nothing");
        assert!(
            fs::read_dir(&transfers)
                .map(|d| d.count() == 0)
                .unwrap_or(true),
            "the bad partial is discarded, not left to be resumed"
        );
    }

    #[test]
    fn an_out_of_order_chunk_is_refused_rather_than_silently_filled() {
        let tmp = tempfile::tempdir().unwrap();
        let data = body(CHUNK_RAW_BYTES * 3);
        let chunks = split("sess-abc", "/tmp/proj", &data).unwrap();
        let target = tmp.path().join("sess-abc.jsonl");
        let transfers = tmp.path().join("transfers");

        accept_chunk_in(&transfers, &target, &chunks[0]).unwrap();
        let err = accept_chunk_in(&transfers, &target, &chunks[2]).unwrap_err();
        assert!(err.contains("expects"), "got: {err}");
        assert!(!target.exists());
    }

    #[test]
    fn re_sending_an_already_placed_transcript_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let data = body(2048);
        let chunks = split("sess-abc", "/tmp/proj", &data).unwrap();
        let target = tmp.path().join("sess-abc.jsonl");
        let transfers = tmp.path().join("transfers");

        accept_chunk_in(&transfers, &target, &chunks[0]).unwrap();
        let again = accept_chunk_in(&transfers, &target, &chunks[0]).unwrap();

        assert_eq!(again, Accepted::AlreadyPresent(target.clone()));
        assert_eq!(fs::read(&target).unwrap(), data, "unchanged");
    }

    #[test]
    fn a_different_transcript_under_the_same_name_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("sess-abc.jsonl");
        fs::write(&target, b"a conversation that is already here").unwrap();
        let transfers = tmp.path().join("transfers");

        let chunks = split("sess-abc", "/tmp/proj", &body(512)).unwrap();
        let err = accept_chunk_in(&transfers, &target, &chunks[0]).unwrap_err();

        assert!(err.contains("refusing to overwrite"), "got: {err}");
        assert_eq!(
            fs::read(&target).unwrap(),
            b"a conversation that is already here",
            "the existing transcript is untouched"
        );
    }

    #[test]
    fn an_impossible_chunk_position_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("s.jsonl");
        let mut c = split("s", "/tmp/p", b"x").unwrap().remove(0);
        c.seq = 5;
        assert!(accept_chunk_in(tmp.path(), &target, &c).is_err());
        c.seq = 0;
        c.total = 0;
        assert!(accept_chunk_in(tmp.path(), &target, &c).is_err());
    }

    // ── placement ───────────────────────────────────────────────────────

    #[test]
    fn transcript_path_lands_in_the_slugged_project_dir() {
        let p = transcript_path("/Users/x/Sandbox/my_app", "sess-1").unwrap();
        assert!(
            p.ends_with("-Users-x-Sandbox-my-app/sess-1.jsonl"),
            "got {}",
            p.display()
        );
    }

    #[test]
    fn transcript_path_refuses_an_unsafe_session_id() {
        assert!(transcript_path("/tmp/p", "../../escape").is_none());
    }

    #[test]
    fn the_resume_hint_is_the_command_to_type() {
        assert_eq!(
            resume_hint("/home/dev/proj", "abc-123"),
            "cd /home/dev/proj && claude --resume abc-123 --fork-session"
        );
    }

    // ── wire format ─────────────────────────────────────────────────────

    /// A chunk survives the message envelope with its bytes intact. Built and
    /// parsed through the real functions the serve loop uses, and the data is
    /// non-UTF-8 so base64 is actually doing something.
    #[test]
    fn a_chunk_survives_the_wire_envelope() {
        let raw: Vec<u8> = (0u8..=255).rev().collect();
        let chunk = Chunk {
            session_id: "sess-1".into(),
            cwd: "/Users/x/proj".into(),
            seq: 2,
            total: 5,
            digest: hex(&crypto::sha256(&raw)),
            data: raw.clone(),
        };
        let msg = build_chunk_message(&chunk, "peer-a");
        assert_eq!(msg.msg_type, super::super::MessageType::SessionTransfer);
        assert_eq!(parse_chunk(&msg.payload).unwrap(), chunk);
    }

    #[test]
    fn parse_chunk_rejects_malformed_payloads() {
        let good = build_chunk_message(&split("sess-1", "/p", b"hello").unwrap()[0], "peer-a");
        assert!(parse_chunk(&good.payload).is_some(), "control");

        for (field, value) in [
            ("session_id", serde_json::json!("../escape")),
            ("data", serde_json::json!("not valid base64!")),
            ("seq", serde_json::json!("two")),
        ] {
            let mut p = good.payload.clone();
            p[field] = value;
            assert!(parse_chunk(&p).is_none(), "accepted a bad {field}");
        }

        let mut missing = good.payload.clone();
        missing.as_object_mut().unwrap().remove("digest");
        assert!(parse_chunk(&missing).is_none(), "accepted a missing digest");
    }

    #[test]
    fn the_acknowledgement_carries_the_path_and_the_command() {
        let msg = build_received_message(
            "sess-9",
            Path::new("/home/dev/.claude/projects/-home-dev-proj/sess-9.jsonl"),
            "/home/dev/proj",
            "mini",
        );
        assert_eq!(msg.msg_type, super::super::MessageType::SessionReceived);
        let (id, path, resume) = parse_received(&msg.payload).unwrap();
        assert_eq!(id, "sess-9");
        assert!(path.ends_with("sess-9.jsonl"));
        assert_eq!(
            resume,
            "cd /home/dev/proj && claude --resume sess-9 --fork-session"
        );
    }

    // ── resolve_source ──────────────────────────────────────────────────

    /// An explicit path is authoritative, and the id comes from the file
    /// stem rather than from what the caller believed the session was
    /// called. Discovery can hand back a transcript belonging to another id
    /// (a `--resume` uuid, or just the newest file in the directory), and
    /// labelling those bytes with the wrong id resumes the wrong
    /// conversation on the far side.
    #[test]
    fn explicit_path_takes_its_id_from_the_file_stem() {
        let dir = std::env::temp_dir().join(format!("cctl-src-{}", std::process::id()));
        fs::create_dir_all(&dir).expect("create temp dir");
        let path = dir.join("11111111-2222-3333-4444-555555555555.jsonl");
        fs::write(&path, b"{}\n").expect("write transcript");

        let (id, resolved) =
            resolve_source(Some(&path), "/does/not/matter", "a-different-id").expect("resolves");

        assert_eq!(id, "11111111-2222-3333-4444-555555555555");
        assert_eq!(resolved, path);
        fs::remove_dir_all(&dir).ok();
    }

    /// A stem that is not a usable session id must be refused rather than
    /// put on the wire, because the receiver turns it straight back into a
    /// filename.
    #[test]
    fn explicit_path_with_an_unusable_stem_is_refused() {
        let dir = std::env::temp_dir().join(format!("cctl-bad-{}", std::process::id()));
        fs::create_dir_all(&dir).expect("create temp dir");
        let path = dir.join("not a session id.jsonl");
        fs::write(&path, b"{}\n").expect("write transcript");

        let err = resolve_source(Some(&path), "/cwd", "sess-1").expect_err("refuses");

        assert!(err.contains("not a usable session id"), "got: {err}");
        fs::remove_dir_all(&dir).ok();
    }

    /// A path that does not exist is refused before any peer is dialled —
    /// the whole point of the preflight.
    #[test]
    fn explicit_path_that_is_missing_is_refused() {
        let missing = std::env::temp_dir().join("cctl-definitely-absent-9bd2.jsonl");
        let err = resolve_source(Some(&missing), "/cwd", "sess-1").expect_err("refuses");
        assert!(err.contains("no transcript"), "got: {err}");
    }

    /// With no explicit path the slug derivation still applies, so the CLI
    /// keeps the behaviour it shipped with.
    #[test]
    fn without_a_path_the_id_and_cwd_derive_the_location() {
        let (id, path) = resolve_source(None, "/home/dev/proj", "sess-9").expect("resolves");
        assert_eq!(id, "sess-9");
        assert_eq!(path, transcript_path("/home/dev/proj", "sess-9").unwrap());
    }

    #[test]
    fn without_a_path_an_unusable_id_is_refused() {
        let err = resolve_source(None, "/home/dev/proj", "../escape").expect_err("refuses");
        assert!(err.contains("not a usable session id"), "got: {err}");
    }
}
