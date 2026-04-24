// Inline SHA-256 and HMAC-SHA256 implementation.
// No external crate — the algorithms are simple enough to implement directly.

// ────────────────────────────────────────────────────────────────────────────
// SHA-256 constants
// ────────────────────────────────────────────────────────────────────────────

const K: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
    0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
    0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
    0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
    0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
    0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
];

const H_INIT: [u32; 8] = [
    0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19,
];

// ────────────────────────────────────────────────────────────────────────────
// SHA-256
// ────────────────────────────────────────────────────────────────────────────

/// Compute SHA-256 hash of the input.
pub fn sha256(data: &[u8]) -> [u8; 32] {
    let mut h = H_INIT;

    // Pre-processing: pad message
    let bit_len = (data.len() as u64) * 8;
    let mut padded = data.to_vec();
    padded.push(0x80);
    while (padded.len() % 64) != 56 {
        padded.push(0);
    }
    padded.extend_from_slice(&bit_len.to_be_bytes());

    // Process each 64-byte block
    for block in padded.chunks_exact(64) {
        let mut w = [0u32; 64];
        for i in 0..16 {
            w[i] = u32::from_be_bytes([
                block[i * 4],
                block[i * 4 + 1],
                block[i * 4 + 2],
                block[i * 4 + 3],
            ]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }

        let mut a = h[0];
        let mut b = h[1];
        let mut c = h[2];
        let mut d = h[3];
        let mut e = h[4];
        let mut f = h[5];
        let mut g = h[6];
        let mut hh = h[7];

        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let temp1 = hh
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let temp2 = s0.wrapping_add(maj);

            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(temp1);
            d = c;
            c = b;
            b = a;
            a = temp1.wrapping_add(temp2);
        }

        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
        h[5] = h[5].wrapping_add(f);
        h[6] = h[6].wrapping_add(g);
        h[7] = h[7].wrapping_add(hh);
    }

    let mut result = [0u8; 32];
    for (i, val) in h.iter().enumerate() {
        result[i * 4..i * 4 + 4].copy_from_slice(&val.to_be_bytes());
    }
    result
}

// ────────────────────────────────────────────────────────────────────────────
// HMAC-SHA256 (RFC 2104)
// ────────────────────────────────────────────────────────────────────────────

/// Compute HMAC-SHA256(key, message).
pub fn hmac_sha256(key: &[u8], message: &[u8]) -> [u8; 32] {
    const BLOCK_SIZE: usize = 64;

    // Step 1: if key > block size, hash it; if shorter, pad with zeros
    let mut k_prime = [0u8; BLOCK_SIZE];
    if key.len() > BLOCK_SIZE {
        let hashed = sha256(key);
        k_prime[..32].copy_from_slice(&hashed);
    } else {
        k_prime[..key.len()].copy_from_slice(key);
    }

    // Step 2: XOR key with ipad and opad
    let mut i_key_pad = [0u8; BLOCK_SIZE];
    let mut o_key_pad = [0u8; BLOCK_SIZE];
    for i in 0..BLOCK_SIZE {
        i_key_pad[i] = k_prime[i] ^ 0x36;
        o_key_pad[i] = k_prime[i] ^ 0x5c;
    }

    // Step 3: inner hash = SHA256(i_key_pad || message)
    let mut inner = Vec::with_capacity(BLOCK_SIZE + message.len());
    inner.extend_from_slice(&i_key_pad);
    inner.extend_from_slice(message);
    let inner_hash = sha256(&inner);

    // Step 4: outer hash = SHA256(o_key_pad || inner_hash)
    let mut outer = Vec::with_capacity(BLOCK_SIZE + 32);
    outer.extend_from_slice(&o_key_pad);
    outer.extend_from_slice(&inner_hash);
    sha256(&outer)
}

// ────────────────────────────────────────────────────────────────────────────
// PSK generation and formatting
// ────────────────────────────────────────────────────────────────────────────

/// Generate a random 32-byte PSK using /dev/urandom.
pub fn generate_psk() -> [u8; 32] {
    let mut buf = [0u8; 32];

    // Try /dev/urandom first (must use read_exact, not read — the file is infinite)
    if let Ok(mut file) = std::fs::File::open("/dev/urandom") {
        use std::io::Read;
        if file.read_exact(&mut buf).is_ok() {
            return buf;
        }
    }

    // Fallback: seed from timestamps and pid.
    // WARNING: This fallback is NOT cryptographically secure — an attacker who knows
    // the process start time, PID, and thread ID can predict the key. Only used when
    // /dev/urandom is unavailable (sandboxed/exotic environments). On Linux/macOS this
    // path should never be reached.
    let seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let pid = std::process::id() as u128;
    let tid = format!("{:?}", std::thread::current().id());
    let combined = format!("{seed}-{pid}-{tid}");
    buf = sha256(combined.as_bytes());
    buf
}

/// Format a 32-byte PSK as a human-friendly code: `xxxx-xxxx-xxxx-xxxx`.
///
/// SECURITY NOTE: The code only encodes the first 8 bytes (64 bits of entropy).
/// The remaining 24 bytes are derived deterministically via SHA-256 in `parse_psk`.
/// This is acceptable for short-lived pairing codes shared out-of-band on a LAN,
/// but the effective key strength for brute-force is 2^64, not 2^256.
pub fn format_psk(psk: &[u8; 32]) -> String {
    let hex = hex_encode(&psk[..8]); // Use first 8 bytes = 16 hex chars
    // Group into 4-char chunks separated by dashes
    hex.as_bytes()
        .chunks(4)
        .map(|chunk| std::str::from_utf8(chunk).unwrap_or("????"))
        .collect::<Vec<_>>()
        .join("-")
}

/// Parse a human-friendly PSK code back to 32 bytes.
/// The code encodes the first 8 bytes; remaining 24 bytes are derived via SHA-256.
pub fn parse_psk(code: &str) -> Result<[u8; 32], String> {
    let clean: String = code.chars().filter(|c| c.is_ascii_hexdigit()).collect();
    if clean.len() != 16 {
        return Err(format!(
            "invalid PSK code: expected 16 hex chars, got {}",
            clean.len()
        ));
    }
    let prefix = hex_decode(&clean)?;
    // Derive full 32-byte key: SHA-256 of the prefix
    let mut full = sha256(&prefix);
    // Embed the prefix in the first 8 bytes for round-trip consistency
    full[..8].copy_from_slice(&prefix);
    Ok(full)
}

// ────────────────────────────────────────────────────────────────────────────
// Hex encoding / decoding
// ────────────────────────────────────────────────────────────────────────────

/// Hex-encode bytes to a lowercase string.
pub fn hex_encode(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// Hex-decode a string to bytes.
pub fn hex_decode(s: &str) -> Result<Vec<u8>, String> {
    if s.len() % 2 != 0 {
        return Err("hex string must have even length".into());
    }
    (0..s.len())
        .step_by(2)
        .map(|i| {
            u8::from_str_radix(&s[i..i + 2], 16)
                .map_err(|e| format!("invalid hex at position {i}: {e}"))
        })
        .collect()
}

/// Generate `n` random bytes as a hex string.
pub fn random_hex(n: usize) -> String {
    let psk = generate_psk();
    hex_encode(&psk[..n.min(32)])
}

// ────────────────────────────────────────────────────────────────────────────
// Tests
// ────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_empty() {
        // SHA-256("") = e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855
        let hash = sha256(b"");
        assert_eq!(
            hex_encode(&hash),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn sha256_abc() {
        // SHA-256("abc") = ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad
        let hash = sha256(b"abc");
        assert_eq!(
            hex_encode(&hash),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn sha256_longer_message() {
        // SHA-256("abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq")
        let hash = sha256(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq");
        assert_eq!(
            hex_encode(&hash),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
    }

    #[test]
    fn hmac_sha256_rfc4231_test1() {
        // RFC 4231 Test Case 1
        let key = [0x0bu8; 20];
        let data = b"Hi There";
        let mac = hmac_sha256(&key, data);
        assert_eq!(
            hex_encode(&mac),
            "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"
        );
    }

    #[test]
    fn hmac_sha256_rfc4231_test2() {
        // RFC 4231 Test Case 2: key = "Jefe", data = "what do ya want for nothing?"
        let mac = hmac_sha256(b"Jefe", b"what do ya want for nothing?");
        assert_eq!(
            hex_encode(&mac),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
    }

    #[test]
    fn hex_roundtrip() {
        let data = [0xde, 0xad, 0xbe, 0xef, 0x01, 0x23];
        let encoded = hex_encode(&data);
        assert_eq!(encoded, "deadbeef0123");
        let decoded = hex_decode(&encoded).unwrap();
        assert_eq!(decoded, data);
    }

    #[test]
    fn hex_decode_errors() {
        assert!(hex_decode("abc").is_err()); // odd length
        assert!(hex_decode("zzzz").is_err()); // invalid chars
    }

    #[test]
    fn psk_format_roundtrip() {
        let psk = generate_psk();
        let code = format_psk(&psk);
        // Code should be xxxx-xxxx-xxxx-xxxx format
        assert_eq!(code.len(), 19); // 16 hex + 3 dashes
        assert_eq!(code.matches('-').count(), 3);

        let parsed = parse_psk(&code).unwrap();
        // First 8 bytes match the original random PSK
        assert_eq!(&parsed[..8], &psk[..8]);

        // Canonical round-trip: parse_psk(format_psk(x)) is idempotent
        // Both sides of a pairing must use parse_psk to get the canonical key
        let code2 = format_psk(&parsed);
        let parsed2 = parse_psk(&code2).unwrap();
        assert_eq!(parsed, parsed2); // full 32-byte match
    }

    #[test]
    fn psk_parse_rejects_bad_length() {
        assert!(parse_psk("abcd").is_err());
        assert!(parse_psk("").is_err());
    }

    #[test]
    fn generate_psk_nonzero() {
        let psk = generate_psk();
        // Should not be all zeros
        assert!(psk.iter().any(|&b| b != 0));
    }
}
