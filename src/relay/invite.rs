// Invite system: compact relay codes, invite links, word encoding, QR rendering.
//
// A "relay code" encodes IP + port + PSK-seed into a short, human-speakable string.
// Format: 9 bytes (4 IP + 1 port-delta + 4 PSK-seed) → 15 base32 chars → XXX-XXX-XXX-XXX-XXX
// No raw IPs visible. Speakable over a phone call.

use std::net::{Ipv4Addr, SocketAddr};

use super::crypto;

// ────────────────────────────────────────────────────────────────────────────
// Relay code: compact encoding of connection info
// ────────────────────────────────────────────────────────────────────────────

const DEFAULT_PORT: u16 = 9847;

/// Encode connection info into a compact relay code.
/// Format: 9 bytes → base32 → grouped with dashes.
pub fn encode_relay_code(addr: &SocketAddr, psk: &[u8; 32]) -> String {
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

    let mut buf = [0u8; 9];
    buf[0..4].copy_from_slice(&ip.octets());
    buf[4] = port_delta;
    buf[5..9].copy_from_slice(&psk[..4]); // PSK seed (first 4 bytes)

    let encoded = base32_encode(&buf);
    // Group into 3-char chunks with dashes: XXX-XXX-XXX-XXX-XX
    format_grouped(&encoded, 3)
}

/// Decode a relay code back into address + PSK.
pub fn decode_relay_code(code: &str) -> Result<(SocketAddr, [u8; 32]), String> {
    let clean: String = code.chars().filter(|c| c.is_ascii_alphanumeric()).collect();
    let bytes = base32_decode(&clean)?;

    if bytes.len() < 9 {
        return Err(format!("relay code too short: {} bytes", bytes.len()));
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

    // Derive full 32-byte PSK from 4-byte seed
    let seed = &bytes[5..9];
    let mut psk = crypto::sha256(seed);
    psk[..4].copy_from_slice(seed);

    Ok((addr, psk))
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
// Word-based encoding: memorable phrases
// ────────────────────────────────────────────────────────────────────────────

/// Encode a relay code as a word phrase (e.g., "brave-tiger-quiet-river-bold").
/// Uses a 256-word list (8 bits per word), so 9 bytes = 9 words.
pub fn encode_words(addr: &SocketAddr, psk: &[u8; 32]) -> String {
    let ip = match addr.ip() {
        std::net::IpAddr::V4(v4) => v4,
        std::net::IpAddr::V6(_) => Ipv4Addr::new(127, 0, 0, 1),
    };

    let port_delta = if addr.port() == DEFAULT_PORT {
        128u8
    } else {
        let delta = addr.port() as i32 - DEFAULT_PORT as i32;
        delta.clamp(0, 255) as u8
    };

    let mut buf = [0u8; 9];
    buf[0..4].copy_from_slice(&ip.octets());
    buf[4] = port_delta;
    buf[5..9].copy_from_slice(&psk[..4]);

    buf.iter()
        .map(|&b| WORD_LIST[b as usize])
        .collect::<Vec<_>>()
        .join("-")
}

/// Decode a word phrase back into address + PSK.
pub fn decode_words(phrase: &str) -> Result<(SocketAddr, [u8; 32]), String> {
    let words: Vec<&str> = phrase.split('-').collect();
    if words.len() < 9 {
        return Err(format!(
            "word phrase too short: {} words, need 9",
            words.len()
        ));
    }

    let mut buf = [0u8; 9];
    for (i, word) in words.iter().take(9).enumerate() {
        let lower = word.to_lowercase();
        let idx = WORD_LIST
            .iter()
            .position(|&w| w == lower)
            .ok_or_else(|| format!("unknown word: '{word}'"))?;
        buf[i] = idx as u8;
    }

    let ip = Ipv4Addr::new(buf[0], buf[1], buf[2], buf[3]);
    let port = if buf[4] == 128 {
        DEFAULT_PORT
    } else {
        (DEFAULT_PORT as i32 + buf[4] as i32) as u16
    };

    let addr: SocketAddr = format!("{ip}:{port}")
        .parse()
        .map_err(|e| format!("invalid address: {e}"))?;

    let seed = &buf[5..9];
    let mut psk = crypto::sha256(seed);
    psk[..4].copy_from_slice(seed);

    Ok((addr, psk))
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

        // Should be 9 words separated by dashes
        assert_eq!(phrase.split('-').count(), 9);

        let (decoded_addr, decoded_psk) = decode_words(&phrase).unwrap();
        assert_eq!(decoded_addr, addr);
        assert_eq!(&decoded_psk[..4], &psk[..4]);
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
}
