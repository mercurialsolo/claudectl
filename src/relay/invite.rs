// Invite system: compact relay codes, invite links, word encoding, QR rendering.
//
// A "relay code" encodes IP + port + PSK into a short, human-speakable string.
// Format: 13 bytes (4 IP + 1 port-delta + 8 PSK) → 21 base32 chars → seven groups of 3.
// No raw IPs visible. Speakable over a phone call.
//
// All three formats — code, word phrase and cctl:// link — carry the same 8 PSK
// bytes and rebuild the full key with the same `crypto::parse_psk`. They used
// not to: codes and phrases packed 4 bytes and derived their own key, so they
// produced a key neither the link nor the inviter'"'"'s stored `_pending.key` would
// ever match, and no code or phrase could be redeemed.

use std::net::{Ipv4Addr, SocketAddr};

use super::crypto;

// ────────────────────────────────────────────────────────────────────────────
// Relay code: compact encoding of connection info
// ────────────────────────────────────────────────────────────────────────────

const DEFAULT_PORT: u16 = 9847;

/// Bytes an invite payload packs into: 4 IPv4 octets, 1 port delta, 8 PSK bytes.
///
/// The PSK half is **8** bytes, not 4, because that is what the invite *link*
/// carries and what [`crypto::parse_psk`] rebuilds a full key from. The relay
/// code and the word phrase used to pack only 4 and then derive the key
/// themselves with `sha256(seed4)` — a different 32-byte key than the link
/// produces, and a different one than the inviter stores in `_pending.key`. So
/// codes and phrases could never authenticate; only links could. The fix is not
/// really the width, it is that all three formats now go through one derivation.
const INVITE_PAYLOAD_LEN: usize = 13;

/// Pack an address and PSK into the bytes a code or phrase carries.
fn pack_invite(addr: &SocketAddr, psk: &[u8; 32]) -> [u8; INVITE_PAYLOAD_LEN] {
    let ip = match addr.ip() {
        std::net::IpAddr::V4(v4) => v4,
        std::net::IpAddr::V6(_) => Ipv4Addr::new(127, 0, 0, 1), // fallback
    };

    let port_delta = if addr.port() == DEFAULT_PORT {
        128u8 // sentinel for "default port"
    } else {
        // Encode port as offset from default, clamped to u8 range
        let delta = addr.port() as i32 - DEFAULT_PORT as i32;
        delta.clamp(0, 255) as u8
    };

    let mut buf = [0u8; INVITE_PAYLOAD_LEN];
    buf[0..4].copy_from_slice(&ip.octets());
    buf[4] = port_delta;
    buf[5..13].copy_from_slice(&psk[..8]);
    buf
}

/// Unpack what [`pack_invite`] produced.
///
/// The PSK comes back through `crypto::parse_psk`, the same function the link
/// path uses, so every invite format yields the identical canonical key.
fn unpack_invite(bytes: &[u8]) -> Result<(SocketAddr, [u8; 32]), String> {
    if bytes.len() != INVITE_PAYLOAD_LEN {
        // Loud rather than lenient: a 9-byte payload is a code minted by a
        // claudectl whose codes could not authenticate anyway, and decoding a
        // short buffer would just produce a key nothing matches.
        return Err(format!(
            "invite payload is {} bytes, expected {INVITE_PAYLOAD_LEN} — a code or phrase \
             this short was minted by claudectl 0.65.0 or earlier, whose codes could not \
             be redeemed at all. Ask for a new one.",
            bytes.len()
        ));
    }

    let ip = Ipv4Addr::new(bytes[0], bytes[1], bytes[2], bytes[3]);
    let port = if bytes[4] == 128 {
        DEFAULT_PORT
    } else {
        (DEFAULT_PORT as i32 + bytes[4] as i32) as u16
    };

    let addr: SocketAddr = format!("{ip}:{port}")
        .parse()
        .map_err(|e| format!("invalid address: {e}"))?;

    let psk = crypto::parse_psk(&crypto::hex_encode(&bytes[5..13]))?;
    Ok((addr, psk))
}

/// Encode connection info into a compact relay code.
/// Format: 13 bytes -> 21 base32 chars -> XXX-XXX-XXX-XXX-XXX-XXX-XXX
pub fn encode_relay_code(addr: &SocketAddr, psk: &[u8; 32]) -> String {
    let encoded = base32_encode(&pack_invite(addr, psk));
    // 13 bytes is 21 base32 chars, which groups into 7 chunks of 3 exactly.
    format_grouped(&encoded, 3)
}

/// Decode a relay code back into address + PSK.
pub fn decode_relay_code(code: &str) -> Result<(SocketAddr, [u8; 32]), String> {
    let clean: String = code.chars().filter(|c| c.is_ascii_alphanumeric()).collect();
    let bytes = base32_decode(&clean)?;
    unpack_invite(&bytes)
}

// ────────────────────────────────────────────────────────────────────────────
// Invite link: cctl:// URL format
// ────────────────────────────────────────────────────────────────────────────

/// Build an invite link: cctl://<identity>@<host>:<port>/k/<psk-code>
pub fn build_invite_link(identity: &str, addr: &SocketAddr, psk: &[u8; 32]) -> String {
    let psk_code = crypto::format_psk(psk).replace('-', "");
    format!("cctl://{identity}@{addr}/k/{psk_code}")
}

/// Parse an invite link back into components.
pub fn parse_invite_link(link: &str) -> Result<(String, SocketAddr, [u8; 32]), String> {
    let stripped = link
        .strip_prefix("cctl://")
        .ok_or("invite link must start with cctl://")?;

    let (identity_host, psk_part) = stripped
        .split_once("/k/")
        .ok_or("missing /k/ in invite link")?;

    let (identity, host_port) = identity_host
        .split_once('@')
        .ok_or("missing @ in invite link")?;

    let addr: SocketAddr = host_port
        .parse()
        .map_err(|e| format!("invalid address '{host_port}': {e}"))?;

    // Re-insert dashes into the PSK code for parse_psk
    let psk_hex = psk_part.trim();
    if psk_hex.len() != 16 {
        return Err(format!(
            "invalid PSK code length: expected 16, got {}",
            psk_hex.len()
        ));
    }
    let dashed = format!(
        "{}-{}-{}-{}",
        &psk_hex[0..4],
        &psk_hex[4..8],
        &psk_hex[8..12],
        &psk_hex[12..16]
    );
    let psk = crypto::parse_psk(&dashed)?;

    Ok((identity.to_string(), addr, psk))
}

// ────────────────────────────────────────────────────────────────────────────
// Hive invite links (#434)
// ────────────────────────────────────────────────────────────────────────────

/// Everything a hive invite link carries.
///
/// `name` and `policy` are advisory — what the holder is told they are joining,
/// so `hive join` can print it before connecting. The host re-decides both from
/// its own identity file; a link cannot talk its way into a policy.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(
    not(feature = "hive"),
    allow(dead_code, reason = "the hive invite flow is the only caller")
)]
pub struct HiveInvite {
    pub hive_id: String,
    pub identity: String,
    pub addr: SocketAddr,
    pub psk: [u8; 32],
    pub name: Option<String>,
    pub policy: Option<String>,
}

/// Build a hive invite link.
///
/// The spec (§7.4) wrote this as `cctl://hive/<hive_id>?k=<psk>&n=<name>`, which
/// cannot be used: it names a hive but no machine, so a holder has nothing to
/// connect to. The address is therefore carried in `a=`, exactly the
/// `identity@host:port` the peer link already puts before `/k/`. The path shape
/// is the spec's, so a hive link is still recognisable at a glance.
#[cfg_attr(
    not(feature = "hive"),
    allow(dead_code, reason = "the hive invite flow is the only caller")
)]
pub fn build_hive_invite_link(
    hive_id: &str,
    identity: &str,
    addr: &SocketAddr,
    psk: &[u8; 32],
    name: Option<&str>,
    policy: Option<&str>,
) -> String {
    let psk_code = crypto::format_psk(psk).replace('-', "");
    let mut link = format!("cctl://hive/{hive_id}?a={identity}@{addr}&k={psk_code}");
    if let Some(name) = name {
        link.push_str(&format!("&n={name}"));
    }
    if let Some(policy) = policy {
        link.push_str(&format!("&p={policy}"));
    }
    link
}

/// True when `link` is a hive invite rather than a peer invite.
///
/// The two are unambiguous: a peer link always has `<identity>@` before its
/// `/k/`, and a hive link's first path segment is the literal `hive`.
#[cfg_attr(
    not(feature = "hive"),
    allow(dead_code, reason = "the hive invite flow is the only caller")
)]
pub fn is_hive_invite_link(link: &str) -> bool {
    link.strip_prefix("cctl://")
        .is_some_and(|rest| rest.starts_with("hive/"))
}

/// Parse a hive invite link.
#[cfg_attr(
    not(feature = "hive"),
    allow(dead_code, reason = "the hive invite flow is the only caller")
)]
pub fn parse_hive_invite_link(link: &str) -> Result<HiveInvite, String> {
    let rest = link
        .strip_prefix("cctl://hive/")
        .ok_or("a hive invite link must start with cctl://hive/")?;

    let (hive_id, query) = rest.split_once('?').ok_or(
        "hive invite link has no parameters — it must carry at least \
         a=<identity>@<host:port> and k=<psk>",
    )?;

    if hive_id.is_empty() {
        return Err("hive invite link has no hive id".into());
    }

    let mut addr_part = None;
    let mut psk_part = None;
    let mut name = None;
    let mut policy = None;

    for field in query.split('&') {
        if field.is_empty() {
            continue;
        }
        let (key, value) = field
            .split_once('=')
            .ok_or_else(|| format!("malformed parameter '{field}' — expected key=value"))?;
        match key {
            "a" => addr_part = Some(value),
            "k" => psk_part = Some(value),
            "n" => name = Some(value.to_string()),
            "p" => policy = Some(value.to_string()),
            // Unknown keys are ignored rather than rejected, so a future field
            // does not make today's binary refuse an otherwise valid link.
            _ => {}
        }
    }

    let addr_part = addr_part.ok_or(
        "hive invite link is missing a=<identity>@<host:port>, so there is nothing to connect to",
    )?;
    let (identity, host_port) = addr_part
        .split_once('@')
        .ok_or("the a= parameter must be <identity>@<host:port>")?;
    if identity.is_empty() {
        return Err("the a= parameter has an empty identity".into());
    }
    let addr: SocketAddr = host_port
        .parse()
        .map_err(|e| format!("invalid address '{host_port}': {e}"))?;

    let psk_code = psk_part
        .ok_or("hive invite link is missing k=<psk>")?
        .trim();
    if psk_code.len() != 16 {
        return Err(format!(
            "invalid PSK code length: expected 16, got {}",
            psk_code.len()
        ));
    }
    // parse_psk filters to hex digits itself, so the undashed form is fine.
    let psk = crypto::parse_psk(psk_code)?;

    Ok(HiveInvite {
        hive_id: hive_id.to_string(),
        identity: identity.to_string(),
        addr,
        psk,
        name,
        policy,
    })
}

// ────────────────────────────────────────────────────────────────────────────
// Word-based encoding: memorable phrases
// ────────────────────────────────────────────────────────────────────────────

/// Encode a relay code as a word phrase (e.g., "brave-tiger-quiet-river-bold").
/// Uses a 256-word list (8 bits per word), so 9 bytes = 9 words.
pub fn encode_words(addr: &SocketAddr, psk: &[u8; 32]) -> String {
    pack_invite(addr, psk)
        .iter()
        .map(|&b| WORD_LIST[b as usize])
        .collect::<Vec<_>>()
        .join("-")
}

/// Is this input a word phrase rather than a relay code?
///
/// Both formats are dash-separated and fixed-length, and the two lengths differ
/// — a phrase is one word per payload byte, a code is seven groups of three
/// base32 characters — so the question has an exact answer and does not need a
/// guess.
///
/// It used to be guessed: "every segment is short and alphabetic". A base32 code
/// contains only `A`–`Z` and `2`–`7`, so a code that happens to draw no digits
/// satisfies that and was read as a phrase, then failed to decode. At 21
/// characters that is `(26/32)^21`, about **1 in 78** invites; before #453
/// widened the payload it was 15 characters, or about 1 in 18. Either way the
/// code was unredeemable and the error blamed the phrase decoder.
pub fn looks_like_word_phrase(input: &str) -> bool {
    let segments: Vec<&str> = input.split('-').collect();
    segments.len() == INVITE_PAYLOAD_LEN
        && segments
            .iter()
            .all(|w| !w.is_empty() && w.chars().all(|c| c.is_ascii_alphabetic()))
}

/// Decode a word phrase back into address + PSK.
pub fn decode_words(phrase: &str) -> Result<(SocketAddr, [u8; 32]), String> {
    let words: Vec<&str> = phrase.split('-').collect();
    if words.len() != INVITE_PAYLOAD_LEN {
        return Err(format!(
            "word phrase has {} words, need {INVITE_PAYLOAD_LEN}",
            words.len()
        ));
    }

    let mut buf = [0u8; INVITE_PAYLOAD_LEN];
    for (i, word) in words.iter().enumerate() {
        let lower = word.to_lowercase();
        let idx = WORD_LIST
            .iter()
            .position(|&w| w == lower)
            .ok_or_else(|| format!("unknown word: '{word}'"))?;
        buf[i] = idx as u8;
    }

    unpack_invite(&buf)
}

// ────────────────────────────────────────────────────────────────────────────
// QR code rendering (via qrencode CLI or fallback)
// ────────────────────────────────────────────────────────────────────────────

/// Render a QR code in the terminal for the given text.
/// Tries `qrencode` CLI first, falls back to a text box.
pub fn render_qr(text: &str) -> String {
    // Try qrencode if available
    if let Ok(output) = std::process::Command::new("qrencode")
        .args(["-t", "UTF8", "-m", "1", text])
        .output()
    {
        if output.status.success() {
            return String::from_utf8_lossy(&output.stdout).to_string();
        }
    }

    // Fallback: render a simple bordered text box with the code
    let mut lines = Vec::new();
    lines.push(format!("  ╔{}╗", "═".repeat(text.len() + 2)));
    lines.push(format!("  ║ {} ║", text));
    lines.push(format!("  ╚{}╝", "═".repeat(text.len() + 2)));
    lines.push(String::new());
    lines.push("  (Install 'qrencode' for a scannable QR code)".to_string());
    lines.join("\n")
}

// ────────────────────────────────────────────────────────────────────────────
// Base32 encoding (RFC 4648, no padding)
// ────────────────────────────────────────────────────────────────────────────

const BASE32_ALPHABET: &[u8; 32] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";

fn base32_encode(data: &[u8]) -> String {
    let mut result = String::new();
    let mut buffer: u64 = 0;
    let mut bits: u32 = 0;

    for &byte in data {
        buffer = (buffer << 8) | byte as u64;
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            let idx = ((buffer >> bits) & 0x1F) as usize;
            result.push(BASE32_ALPHABET[idx] as char);
        }
    }
    if bits > 0 {
        let idx = ((buffer << (5 - bits)) & 0x1F) as usize;
        result.push(BASE32_ALPHABET[idx] as char);
    }

    result
}

fn base32_decode(encoded: &str) -> Result<Vec<u8>, String> {
    let mut buffer: u64 = 0;
    let mut bits: u32 = 0;
    let mut result = Vec::new();

    for ch in encoded.chars() {
        let upper = ch.to_ascii_uppercase();
        let val = match upper {
            'A'..='Z' => upper as u64 - 'A' as u64,
            '2'..='7' => upper as u64 - '2' as u64 + 26,
            _ => return Err(format!("invalid base32 character: '{ch}'")),
        };
        buffer = (buffer << 5) | val;
        bits += 5;
        if bits >= 8 {
            bits -= 8;
            result.push(((buffer >> bits) & 0xFF) as u8);
        }
    }

    Ok(result)
}

fn format_grouped(s: &str, group_size: usize) -> String {
    s.as_bytes()
        .chunks(group_size)
        .map(|chunk| std::str::from_utf8(chunk).unwrap_or("???"))
        .collect::<Vec<_>>()
        .join("-")
}

// ────────────────────────────────────────────────────────────────────────────
// Word list (256 common, short, distinct English words)
// ────────────────────────────────────────────────────────────────────────────

const WORD_LIST: [&str; 256] = [
    "ace", "act", "age", "aid", "aim", "air", "ale", "ant", "ape", "arc", "ark", "arm", "art",
    "ash", "axe", "bay", "bed", "bee", "bet", "bid", "big", "bit", "bow", "box", "bud", "bug",
    "bus", "cab", "cap", "car", "cat", "cob", "cod", "cog", "cop", "cow", "cry", "cub", "cup",
    "cut", "dam", "day", "den", "dew", "dig", "dim", "dip", "dog", "dot", "dry", "dub", "dug",
    "dun", "duo", "dye", "ear", "eat", "eel", "egg", "elk", "elm", "emu", "end", "era", "eve",
    "ewe", "eye", "fan", "far", "fat", "fax", "fed", "few", "fig", "fin", "fir", "fit", "fix",
    "fly", "fog", "for", "fox", "fry", "fun", "fur", "gag", "gap", "gas", "gem", "get", "gin",
    "gnu", "god", "got", "gum", "gun", "gut", "guy", "gym", "had", "ham", "has", "hat", "hay",
    "hen", "her", "hid", "him", "hip", "hit", "hog", "hop", "hot", "how", "hub", "hue", "hug",
    "hum", "hut", "ice", "ill", "imp", "ink", "inn", "ion", "ire", "ivy", "jab", "jag", "jam",
    "jar", "jaw", "jay", "jet", "jig", "job", "jog", "joy", "jug", "jut", "keg", "ken", "key",
    "kid", "kin", "kit", "lab", "lad", "lag", "lap", "law", "lay", "lea", "led", "leg", "let",
    "lid", "lip", "lit", "log", "lot", "low", "lug", "mad", "man", "map", "mar", "mat", "may",
    "men", "met", "mid", "mix", "mob", "mod", "mop", "mow", "mud", "mug", "nab", "nag", "nap",
    "net", "new", "nil", "nip", "nit", "nod", "nor", "not", "now", "nun", "nut", "oak", "oar",
    "oat", "odd", "ode", "off", "oft", "ohm", "oil", "old", "one", "opt", "orb", "ore", "our",
    "out", "owe", "owl", "own", "pad", "pal", "pan", "paw", "pay", "pea", "peg", "pen", "per",
    "pet", "pie", "pig", "pin", "pit", "ply", "pod", "pop", "pot", "pro", "pry", "pub", "pug",
    "pun", "pup", "put", "ram", "ran", "rap", "rat", "raw", "ray", "red", "rib", "rid", "rig",
    "rim", "rip", "rob", "rod", "rot", "row", "rub", "rug", "rum",
];

// ────────────────────────────────────────────────────────────────────────────
// Tests
// ────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn test_addr() -> SocketAddr {
        "192.168.1.50:9847".parse().unwrap()
    }

    fn test_psk() -> [u8; 32] {
        let mut psk = [0u8; 32];
        psk[0] = 0xAB;
        psk[1] = 0xCD;
        psk[2] = 0xEF;
        psk[3] = 0x01;
        psk
    }

    #[test]
    fn relay_code_roundtrip() {
        let addr = test_addr();
        let psk = test_psk();
        let code = encode_relay_code(&addr, &psk);

        // Should be grouped with dashes
        assert!(code.contains('-'));
        // Should be all uppercase alphanumeric + dashes
        assert!(code.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'));

        let (decoded_addr, decoded_psk) = decode_relay_code(&code).unwrap();
        assert_eq!(decoded_addr, addr);
        assert_eq!(&decoded_psk[..4], &psk[..4]); // first 4 bytes match (seed)
    }

    #[test]
    fn relay_code_non_default_port() {
        let addr: SocketAddr = "10.0.0.1:9900".parse().unwrap();
        let psk = test_psk();
        let code = encode_relay_code(&addr, &psk);
        let (decoded_addr, _) = decode_relay_code(&code).unwrap();
        assert_eq!(decoded_addr.ip(), addr.ip());
        assert_eq!(decoded_addr.port(), addr.port());
    }

    #[test]
    fn relay_code_default_port_sentinel() {
        let addr: SocketAddr = "172.16.0.1:9847".parse().unwrap();
        let psk = test_psk();
        let code = encode_relay_code(&addr, &psk);
        let (decoded_addr, _) = decode_relay_code(&code).unwrap();
        assert_eq!(decoded_addr.port(), 9847);
    }

    #[test]
    fn invite_link_roundtrip() {
        let addr = test_addr();
        let psk = crypto::generate_psk();
        let canonical = crypto::parse_psk(&crypto::format_psk(&psk)).unwrap();
        let link = build_invite_link("laptop-a3f2", &addr, &canonical);

        assert!(link.starts_with("cctl://"));
        assert!(link.contains("laptop-a3f2@"));
        assert!(link.contains("/k/"));

        let (identity, decoded_addr, decoded_psk) = parse_invite_link(&link).unwrap();
        assert_eq!(identity, "laptop-a3f2");
        assert_eq!(decoded_addr, addr);
        assert_eq!(decoded_psk, canonical);
    }

    #[test]
    fn invite_link_parse_errors() {
        assert!(parse_invite_link("http://example.com").is_err());
        assert!(parse_invite_link("cctl://no-k-segment").is_err());
        assert!(parse_invite_link("cctl://no-at-sign/k/abcd1234abcd1234").is_err());
    }

    #[test]
    fn word_encoding_roundtrip() {
        let addr = test_addr();
        let psk = test_psk();
        let phrase = encode_words(&addr, &psk);

        // 13 words: 4 IP + 1 port-delta + 8 PSK bytes, one word per byte.
        assert_eq!(phrase.split('-').count(), 13);

        let (decoded_addr, decoded_psk) = decode_words(&phrase).unwrap();
        assert_eq!(decoded_addr, addr);
        // The carried half must survive exactly. `test_psk` is a raw key rather
        // than a canonical one, so the derived tail legitimately differs; the
        // full-key agreement is asserted in
        // `a_word_phrase_yields_the_key_the_inviter_stored`, which starts from
        // the canonical form an inviter actually stores.
        assert_eq!(&decoded_psk[..8], &psk[..8]);
    }

    #[test]
    fn word_phrase_too_short() {
        assert!(decode_words("ace-act-age").is_err());
    }

    #[test]
    fn word_unknown_word() {
        assert!(decode_words("ace-act-age-aid-aim-air-ale-ant-zzzzz").is_err());
    }

    #[test]
    fn base32_roundtrip() {
        let data = [0xDE, 0xAD, 0xBE, 0xEF, 0x01, 0x23, 0x45, 0x67, 0x89];
        let encoded = base32_encode(&data);
        let decoded = base32_decode(&encoded).unwrap();
        assert_eq!(&decoded[..data.len()], &data);
    }

    #[test]
    fn base32_known_value() {
        // "Hello" in base32 = "JBSWY3DP" (RFC 4648)
        let encoded = base32_encode(b"Hello");
        assert_eq!(encoded, "JBSWY3DP");
    }

    #[test]
    fn format_grouped_works() {
        assert_eq!(format_grouped("ABCDEFGH", 3), "ABC-DEF-GH");
        assert_eq!(format_grouped("ABCDEF", 3), "ABC-DEF");
        assert_eq!(format_grouped("AB", 3), "AB");
    }

    #[test]
    fn word_list_has_256_unique_entries() {
        let mut seen = std::collections::HashSet::new();
        for word in &WORD_LIST {
            assert!(seen.insert(word), "duplicate word: {word}");
        }
        assert_eq!(WORD_LIST.len(), 256);
    }

    #[test]
    fn qr_fallback_renders_box() {
        // qrencode likely not available in test env — tests the fallback
        let output = render_qr("test-data");
        assert!(output.contains("test-data") || output.contains("qrencode"));
    }

    // ── Hive invite links (#434) ───────────────────────────────────────────

    fn sample_psk() -> [u8; 32] {
        // Round-tripped through the code form, because that is the only part a
        // link carries: `format_psk` keeps 8 bytes and `parse_psk` derives the
        // rest. Comparing against a raw random PSK would compare the wrong thing.
        crypto::parse_psk("a1b2-c3d4-e5f6-0789").unwrap()
    }

    #[test]
    fn a_hive_link_round_trips_every_field() {
        let psk = sample_psk();
        let addr: SocketAddr = "192.168.1.50:9847".parse().unwrap();
        let link = build_hive_invite_link(
            "hv_3a9f21",
            "laptop-a3f2",
            &addr,
            &psk,
            Some("barrys-hive"),
            Some("ask"),
        );
        let back = parse_hive_invite_link(&link).unwrap();
        assert_eq!(back.hive_id, "hv_3a9f21");
        assert_eq!(back.identity, "laptop-a3f2");
        assert_eq!(back.addr, addr);
        assert_eq!(back.psk, psk);
        assert_eq!(back.name.as_deref(), Some("barrys-hive"));
        assert_eq!(back.policy.as_deref(), Some("ask"));
    }

    #[test]
    fn a_hive_link_carries_the_address_the_spec_left_out() {
        // §7.4 wrote `cctl://hive/<id>?k=<psk>&n=<name>`, which names a hive but
        // no machine. Such a link must be refused with a reason rather than
        // parsed into something unusable.
        let err = parse_hive_invite_link("cctl://hive/hv_3a9f21?k=a1b2c3d4e5f60789&n=barrys-hive")
            .unwrap_err();
        assert!(err.contains("nothing to connect to"), "got: {err}");
    }

    #[test]
    fn the_optional_fields_are_optional() {
        let psk = sample_psk();
        let addr: SocketAddr = "10.0.0.2:9999".parse().unwrap();
        let link = build_hive_invite_link("hv_1", "box-1", &addr, &psk, None, None);
        assert!(!link.contains("&n="));
        assert!(!link.contains("&p="));
        let back = parse_hive_invite_link(&link).unwrap();
        assert_eq!(back.name, None);
        assert_eq!(back.policy, None);
        assert_eq!(back.addr, addr);
    }

    #[test]
    fn hive_and_peer_links_are_told_apart() {
        let psk = sample_psk();
        let addr: SocketAddr = "192.168.1.50:9847".parse().unwrap();
        let hive = build_hive_invite_link("hv_1", "laptop-a3f2", &addr, &psk, None, None);
        let peer = build_invite_link("laptop-a3f2", &addr, &psk);

        assert!(is_hive_invite_link(&hive));
        assert!(!is_hive_invite_link(&peer));
        // And each parser refuses the other's format rather than half-reading it.
        assert!(parse_hive_invite_link(&peer).is_err());
        assert!(parse_invite_link(&hive).is_err());
    }

    #[test]
    fn a_malformed_hive_link_says_what_is_wrong() {
        for (link, want) in [
            ("cctl://laptop@1.2.3.4:1/k/aaaa", "must start with"),
            ("cctl://hive/hv_1", "no parameters"),
            (
                "cctl://hive/?a=x@1.2.3.4:1&k=a1b2c3d4e5f60789",
                "no hive id",
            ),
            (
                "cctl://hive/hv_1?a=laptop@nonsense&k=a1b2c3d4e5f60789",
                "invalid address",
            ),
            (
                "cctl://hive/hv_1?a=laptop-a3f2-1.2.3.4:1&k=a1b2c3d4e5f60789",
                "<identity>@<host:port>",
            ),
            (
                "cctl://hive/hv_1?a=@1.2.3.4:1&k=a1b2c3d4e5f60789",
                "empty identity",
            ),
            ("cctl://hive/hv_1?a=laptop@1.2.3.4:1", "missing k="),
            (
                "cctl://hive/hv_1?a=laptop@1.2.3.4:1&k=tooshort",
                "expected 16",
            ),
            (
                "cctl://hive/hv_1?a=laptop@1.2.3.4:1&k=a1b2c3d4e5f60789&oops",
                "expected key=value",
            ),
        ] {
            let err = parse_hive_invite_link(link).unwrap_err();
            assert!(
                err.contains(want),
                "link {link:?}\n  wanted {want:?}\n  got    {err:?}"
            );
        }
    }

    #[test]
    fn an_unknown_parameter_does_not_make_a_valid_link_unusable() {
        // A field added by a later version must not make this build refuse a
        // link whose required parts it understands perfectly well.
        let link = "cctl://hive/hv_1?a=laptop@1.2.3.4:9847&k=a1b2c3d4e5f60789&n=x&z=future";
        let back = parse_hive_invite_link(link).unwrap();
        assert_eq!(back.hive_id, "hv_1");
        assert_eq!(back.name.as_deref(), Some("x"));
    }

    #[test]
    fn a_hive_link_survives_qr_rendering() {
        // The link is the one format that carries the hive id, so it is also the
        // one that gets scanned. An empty render would be a silent failure.
        let psk = sample_psk();
        let addr: SocketAddr = "192.168.1.50:9847".parse().unwrap();
        let link = build_hive_invite_link(
            "hv_3a9f21",
            "laptop-a3f2",
            &addr,
            &psk,
            Some("barrys-hive"),
            Some("invite"),
        );
        let qr = render_qr(&link);
        assert!(!qr.trim().is_empty(), "QR render produced nothing");
    }

    // ── All three invite formats must agree on the key ────────────────────
    //
    // The round-trip tests above check each codec against itself, which is
    // exactly the vacuous shape that let the real bug hide: the code and the
    // phrase round-tripped perfectly while producing a key the link and the
    // inviter's `_pending.key` never matched. These compare the three formats
    // against *each other*, and against the canonical key the inviter stores.

    /// The key an inviter actually stores: `parse_psk(format_psk(raw))`.
    fn canonical() -> [u8; 32] {
        let raw = [
            0x93u8, 0x4b, 0xb9, 0x1f, 0xa5, 0x39, 0x41, 0x19, 0xde, 0xad, 0xbe, 0xef, 0x00, 0x11,
            0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff,
            0x01, 0x02, 0x03, 0x04,
        ];
        crypto::parse_psk(&crypto::format_psk(&raw)).unwrap()
    }

    #[test]
    fn a_relay_code_yields_the_key_the_inviter_stored() {
        let psk = canonical();
        let addr: SocketAddr = "192.168.4.24:9910".parse().unwrap();
        let (back_addr, back_psk) = decode_relay_code(&encode_relay_code(&addr, &psk)).unwrap();
        assert_eq!(back_addr, addr);
        assert_eq!(
            back_psk, psk,
            "a code must rebuild the same key the inviter stored, or it can never authenticate"
        );
    }

    #[test]
    fn a_word_phrase_yields_the_key_the_inviter_stored() {
        let psk = canonical();
        let addr: SocketAddr = "10.1.2.3:9850".parse().unwrap();
        let (back_addr, back_psk) = decode_words(&encode_words(&addr, &psk)).unwrap();
        assert_eq!(back_addr, addr);
        assert_eq!(back_psk, psk);
    }

    #[test]
    fn code_phrase_and_link_all_agree_on_the_key() {
        let psk = canonical();
        let addr: SocketAddr = "192.168.4.24:9910".parse().unwrap();

        let (_, from_code) = decode_relay_code(&encode_relay_code(&addr, &psk)).unwrap();
        let (_, from_words) = decode_words(&encode_words(&addr, &psk)).unwrap();
        let (_, _, from_link) = parse_invite_link(&build_invite_link("x", &addr, &psk)).unwrap();
        let from_hive = parse_hive_invite_link(&build_hive_invite_link(
            "hv_1", "x", &addr, &psk, None, None,
        ))
        .unwrap()
        .psk;

        assert_eq!(from_code, psk);
        assert_eq!(from_words, psk);
        assert_eq!(from_link, psk);
        assert_eq!(from_hive, psk);
    }

    #[test]
    fn a_legacy_nine_byte_code_is_refused_with_a_reason() {
        // Rather than decoding a short buffer into a key nothing matches.
        let legacy = base32_encode(&[192u8, 168, 4, 24, 63, 0x93, 0x4b, 0xb9, 0x1f]);
        let err = decode_relay_code(&format_grouped(&legacy, 3)).unwrap_err();
        assert!(err.contains("expected 13"), "got: {err}");
        assert!(err.contains("0.65.0"), "should say which versions: {err}");
    }

    #[test]
    fn a_phrase_of_the_old_length_is_refused() {
        let nine: Vec<&str> = WORD_LIST.iter().take(9).copied().collect();
        let err = decode_words(&nine.join("-")).unwrap_err();
        assert!(err.contains("need 13"), "got: {err}");
    }

    #[test]
    fn a_relay_code_groups_without_a_dangling_chunk() {
        let psk = canonical();
        let addr: SocketAddr = "192.168.4.24:9910".parse().unwrap();
        let code = encode_relay_code(&addr, &psk);
        let groups: Vec<&str> = code.split('-').collect();
        assert_eq!(groups.len(), 7, "code: {code}");
        assert!(
            groups.iter().all(|g| g.len() == 3),
            "every group should be 3 chars: {code}"
        );
    }

    #[test]
    fn a_word_phrase_is_thirteen_words() {
        let psk = canonical();
        let addr: SocketAddr = "192.168.4.24:9910".parse().unwrap();
        let phrase = encode_words(&addr, &psk);
        assert_eq!(phrase.split('-').count(), 13, "phrase: {phrase}");
    }

    #[test]
    fn a_code_of_only_letters_is_not_mistaken_for_a_word_phrase() {
        // The bug this replaced: base32 is A-Z plus 2-7, so a code that draws no
        // digits used to satisfy "short alphabetic segments" and be sent to the
        // phrase decoder, which could not read it.
        let all_letters = format_grouped("ABCDEFGHIJKLMNOPQRSTU", 3);
        assert_eq!(all_letters.split('-').count(), 7);
        assert!(
            !looks_like_word_phrase(&all_letters),
            "{all_letters} is a relay code, not a phrase"
        );
    }

    #[test]
    fn a_real_phrase_is_recognised_and_a_real_code_is_not() {
        let psk = canonical();
        let addr: SocketAddr = "192.168.4.24:9910".parse().unwrap();
        assert!(looks_like_word_phrase(&encode_words(&addr, &psk)));
        assert!(!looks_like_word_phrase(&encode_relay_code(&addr, &psk)));
    }

    #[test]
    fn every_generated_code_is_told_apart_from_a_phrase() {
        // Sweep the PSK space rather than trusting one sample: with the old
        // heuristic roughly one in seventy-eight of these was misread.
        let addr: SocketAddr = "192.168.4.24:9910".parse().unwrap();
        for seed in 0u32..600 {
            let hex = format!("{:016x}", seed.wrapping_mul(2_654_435_761));
            let psk = crypto::parse_psk(&hex).unwrap();
            let code = encode_relay_code(&addr, &psk);
            assert!(
                !looks_like_word_phrase(&code),
                "code {code} misread as a phrase"
            );
            assert!(
                looks_like_word_phrase(&encode_words(&addr, &psk)),
                "phrase for seed {seed} not recognised"
            );
        }
    }

    #[test]
    fn neither_a_link_nor_junk_looks_like_a_phrase() {
        assert!(!looks_like_word_phrase(
            "cctl://x@1.2.3.4:1/k/aaaabbbbccccdddd"
        ));
        assert!(!looks_like_word_phrase(""));
        assert!(!looks_like_word_phrase("one-two-three"));
        // Right count, but an empty segment is not a word.
        assert!(!looks_like_word_phrase(
            &"a-".repeat(12).to_string().replace("a-a", "a--a")
        ));
    }
}
