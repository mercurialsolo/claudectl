//! Content & flow guardrails (spec §9, §10).
//!
//! Originally phase-4 scope was **non-optional command sanitization at the
//! injection boundary** (§9) plus a hard body-size cap, with the richer
//! policy surface (rate limits, hop caps, loop detection, ACLs) deferred.
//! Supervisor RFC v2 §9 makes that deferred surface a gating dependency:
//! without hop caps and reserved-role guards, the long-running supervisor
//! ships an unbounded ping-pong burn loop overnight (#344).
//!
//! This module now owns:
//! * `validate_subject` / `validate_type` / `validate_body` — content rules.
//! * `sanitize_body` — strip slash-command smuggling at the delivery boundary.
//! * `validate_hop_count` — refuse to forward past `DEFAULT_MAX_HOPS`.
//! * `validate_role_name` — refuse to bind the reserved `supervisor` /
//!   `operator` names. Anyone can still *address* those roles (escalation
//!   must work from any sender); only binding is locked down.
//!
//! Rate limiting lives in its sibling `rate_limit.rs` because it carries
//! per-process state.

/// Default body-size ceiling matching the spec's example policy (§10).
pub const DEFAULT_MAX_BODY_BYTES: usize = 8192;

/// Allowed `type` values. Untyped/unknown types are rejected (§10).
pub const ALLOWED_TYPES: &[&str] = &["task", "result", "question", "status", "handoff"];

/// Maximum hop count for forwarded messages. Counts each `publish` that
/// inherits a non-zero hop from a prior message. RFC v2 §9 anchors the
/// ping-pong burn-loop mitigation here. Claude Code's own Stop-hook 8-block
/// cap is the runtime backstop; this is the application-level shorter leash.
pub const DEFAULT_MAX_HOPS: u32 = 8;

/// Role names the bus reserves for the supervisor escalation path. Sessions
/// cannot bind these names via `bus role bind`, the `whoami` MCP tool, or
/// any other path. Without this guard, a hostile cwd could claim to *be*
/// the supervisor and intercept escalations addressed to it.
///
/// Addressing these roles stays open — escalation from any caller is what
/// makes the supervisor useful — but only one component (the supervisor
/// subsystem itself) ever holds them.
pub const RESERVED_ROLES: &[&str] = &["supervisor", "operator"];

#[derive(Debug)]
pub enum PolicyError {
    BadType(String),
    BodyTooLarge { max: usize },
    BadSubject,
    HopLimitExceeded { max: u32, observed: u32 },
    ReservedRole(String),
}

impl std::fmt::Display for PolicyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BadType(t) => write!(f, "message type not allowed: {t}"),
            Self::BodyTooLarge { max } => write!(f, "body exceeds {max} bytes"),
            Self::BadSubject => f.write_str("subject must be a non-empty dot-delimited identifier"),
            Self::HopLimitExceeded { max, observed } => write!(
                f,
                "hop limit exceeded (hop={observed}, max={max}); supervisor escalation expected"
            ),
            Self::ReservedRole(name) => write!(
                f,
                "role name '{name}' is reserved for the supervisor subsystem and cannot be bound"
            ),
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

/// Reject hop counts at or above `max`. Callers compute the *outgoing* hop
/// (parent hop + 1) before calling this, so passing `max` itself fails — the
/// next forward would be over the limit.
pub fn validate_hop_count(hop: u32, max: u32) -> Result<(), PolicyError> {
    if hop > max {
        Err(PolicyError::HopLimitExceeded { max, observed: hop })
    } else {
        Ok(())
    }
}

/// Reject reserved names at the binding boundary. Case-insensitive so
/// `Supervisor` and `SUPERVISOR` are also blocked — SQLite role lookups
/// are case-sensitive, but human typos shouldn't yield a hostile bind.
pub fn validate_role_name(name: &str) -> Result<(), PolicyError> {
    let lower = name.to_ascii_lowercase();
    if RESERVED_ROLES.iter().any(|r| *r == lower) {
        return Err(PolicyError::ReservedRole(name.to_string()));
    }
    Ok(())
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

    #[test]
    fn hop_count_rejects_over_max() {
        assert!(validate_hop_count(0, DEFAULT_MAX_HOPS).is_ok());
        assert!(validate_hop_count(DEFAULT_MAX_HOPS, DEFAULT_MAX_HOPS).is_ok());
        assert!(validate_hop_count(DEFAULT_MAX_HOPS + 1, DEFAULT_MAX_HOPS).is_err());
    }

    #[test]
    fn reserved_roles_cannot_be_bound() {
        assert!(validate_role_name("supervisor").is_err());
        assert!(validate_role_name("operator").is_err());
        assert!(validate_role_name("Supervisor").is_err());
        assert!(validate_role_name("OPERATOR").is_err());
        assert!(validate_role_name("planner").is_ok());
        assert!(validate_role_name("backend").is_ok());
    }
}
