//! Content & flow guardrails (spec §9, §10).
//!
//! Phase-4 scope is **non-optional command sanitization at the injection
//! boundary** (§9) plus a hard body-size cap. The richer policy surface from
//! §10 (rate limits, hop caps, loop detection, ACLs) is deferred to phase 8.

/// Default body-size ceiling matching the spec's example policy (§10).
pub const DEFAULT_MAX_BODY_BYTES: usize = 8192;

/// Allowed `type` values. Untyped/unknown types are rejected (§10).
pub const ALLOWED_TYPES: &[&str] = &["task", "result", "question", "status", "handoff"];

#[derive(Debug)]
pub enum PolicyError {
    BadType(String),
    BodyTooLarge { max: usize },
    BadSubject,
}

impl std::fmt::Display for PolicyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BadType(t) => write!(f, "message type not allowed: {t}"),
            Self::BodyTooLarge { max } => write!(f, "body exceeds {max} bytes"),
            Self::BadSubject => f.write_str("subject must be a non-empty dot-delimited identifier"),
        }
    }
}

impl std::error::Error for PolicyError {}

pub fn validate_type(msg_type: &str) -> Result<(), PolicyError> {
    if ALLOWED_TYPES.contains(&msg_type) {
        Ok(())
    } else {
        Err(PolicyError::BadType(msg_type.to_string()))
    }
}

pub fn validate_subject(subject: &str) -> Result<(), PolicyError> {
    if subject.is_empty() {
        return Err(PolicyError::BadSubject);
    }
    for ch in subject.chars() {
        if !(ch.is_ascii_alphanumeric() || ch == '.' || ch == '_' || ch == '-' || ch == '*') {
            return Err(PolicyError::BadSubject);
        }
    }
    Ok(())
}

pub fn validate_body(body: &str, max: usize) -> Result<(), PolicyError> {
    if body.len() > max {
        Err(PolicyError::BodyTooLarge { max })
    } else {
        Ok(())
    }
}

/// Neutralize command semantics in a message body before it can be delivered
/// to a Claude Code session (§9). A leading `/` becomes a leading space so the
/// recipient reads the line as content rather than a slash command. Other
/// control-character classes that could move the cursor or rewrite the prompt
/// are stripped.
pub fn sanitize_body(body: &str) -> String {
    let mut out = String::with_capacity(body.len());
    let mut chars = body.chars().peekable();

    // Defang a leading "/command" so it lands as plain text.
    if let Some(&first) = chars.peek() {
        if first == '/' {
            out.push(' ');
            chars.next();
        }
    }

    for ch in chars {
        // Allow common whitespace; drop the rest of C0 / DEL.
        let keep = match ch {
            '\n' | '\r' | '\t' => true,
            c if (c as u32) < 0x20 => false,
            '\u{007F}' => false,
            _ => true,
        };
        if keep {
            out.push(ch);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_leading_slash() {
        assert_eq!(sanitize_body("/loop run"), " loop run");
        assert_eq!(sanitize_body("//doubled"), " /doubled");
        assert_eq!(sanitize_body("hello /loop"), "hello /loop");
    }

    #[test]
    fn drops_control_chars() {
        let raw = "ok\x1b[2Joops\x07";
        let cleaned = sanitize_body(raw);
        assert!(!cleaned.contains('\x1b'));
        assert!(!cleaned.contains('\x07'));
        assert!(cleaned.contains("ok"));
        assert!(cleaned.contains("oops"));
    }

    #[test]
    fn keeps_whitespace() {
        let raw = "line one\nline two\tindented";
        assert_eq!(sanitize_body(raw), raw);
    }

    #[test]
    fn validates_subject_grammar() {
        assert!(validate_subject("task.created").is_ok());
        assert!(validate_subject("task.*").is_ok());
        assert!(validate_subject("").is_err());
        assert!(validate_subject("task created").is_err());
        assert!(validate_subject("task/created").is_err());
    }

    #[test]
    fn rejects_unknown_types() {
        assert!(validate_type("task").is_ok());
        assert!(validate_type("brain-dump").is_err());
    }
}
