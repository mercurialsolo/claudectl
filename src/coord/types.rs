use serde::{Deserialize, Serialize};

// -- Event Types ---------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventType {
    SessionObserved,
    TaskCreated,
    LeaseAcquired,
    LeaseReleased,
    MemoryWritten,
    InterruptRaised,
    InterruptDelivered,
    InterruptAcknowledged,
    HandoffCreated,
    HandoffAccepted,
    BlockerOpened,
    BlockerResolved,
}

impl EventType {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::SessionObserved => "session_observed",
            Self::TaskCreated => "task_created",
            Self::LeaseAcquired => "lease_acquired",
            Self::LeaseReleased => "lease_released",
            Self::MemoryWritten => "memory_written",
            Self::InterruptRaised => "interrupt_raised",
            Self::InterruptDelivered => "interrupt_delivered",
            Self::InterruptAcknowledged => "interrupt_acknowledged",
            Self::HandoffCreated => "handoff_created",
            Self::HandoffAccepted => "handoff_accepted",
            Self::BlockerOpened => "blocker_opened",
            Self::BlockerResolved => "blocker_resolved",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "session_observed" => Some(Self::SessionObserved),
            "task_created" => Some(Self::TaskCreated),
            "lease_acquired" => Some(Self::LeaseAcquired),
            "lease_released" => Some(Self::LeaseReleased),
            "memory_written" => Some(Self::MemoryWritten),
            "interrupt_raised" => Some(Self::InterruptRaised),
            "interrupt_delivered" => Some(Self::InterruptDelivered),
            "interrupt_acknowledged" => Some(Self::InterruptAcknowledged),
            "handoff_created" => Some(Self::HandoffCreated),
            "handoff_accepted" => Some(Self::HandoffAccepted),
            "blocker_opened" => Some(Self::BlockerOpened),
            "blocker_resolved" => Some(Self::BlockerResolved),
            _ => None,
        }
    }
}

impl std::fmt::Display for EventType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CoordEvent {
    pub id: Option<i64>,
    pub event_type: EventType,
    pub timestamp: String,
    pub session_id: Option<String>,
    pub payload: serde_json::Value,
}

// -- Lease ---------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LeaseMode {
    Exclusive,
    SharedRead,
    SharedAppend,
    Advisory,
}

impl LeaseMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Exclusive => "exclusive",
            Self::SharedRead => "shared_read",
            Self::SharedAppend => "shared_append",
            Self::Advisory => "advisory",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "exclusive" => Some(Self::Exclusive),
            "shared_read" => Some(Self::SharedRead),
            "shared_append" => Some(Self::SharedAppend),
            "advisory" => Some(Self::Advisory),
            _ => None,
        }
    }
}

impl std::fmt::Display for LeaseMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LeaseStatus {
    Active,
    Released,
    Expired,
}

impl LeaseStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Released => "released",
            Self::Expired => "expired",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "active" => Some(Self::Active),
            "released" => Some(Self::Released),
            "expired" => Some(Self::Expired),
            _ => None,
        }
    }
}

impl std::fmt::Display for LeaseStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Lease {
    pub id: String,
    pub owner_session_id: String,
    pub owner_agent: String,
    pub resource_kind: String,
    pub resource_value: String,
    pub mode: LeaseMode,
    pub reason: String,
    pub acquired_at: String,
    pub expires_at: Option<String>,
    pub status: LeaseStatus,
}

// -- Blocker -------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BlockerStatus {
    Open,
    Resolved,
}

impl BlockerStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Resolved => "resolved",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "open" => Some(Self::Open),
            "resolved" => Some(Self::Resolved),
            _ => None,
        }
    }
}

impl std::fmt::Display for BlockerStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Blocker {
    pub id: String,
    pub task_id: String,
    pub depends_on: Option<String>,
    pub waiting_for: String,
    pub status: BlockerStatus,
    pub owner_session_id: String,
    pub created_at: String,
    pub resolved_at: Option<String>,
}

// -- Handoff -------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HandoffState {
    pub goal: String,
    pub artifacts: Vec<String>,
    pub attempted: Vec<String>,
    pub next_steps: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Handoff {
    pub id: String,
    pub from_session_id: String,
    pub to_session_id: Option<String>,
    pub task_id: String,
    pub summary: String,
    pub state: HandoffState,
    pub priority: String,
    pub created_at: String,
    pub acknowledged_at: Option<String>,
}

// -- Interrupt -----------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InterruptType {
    Nudge,
    RequestInput,
    Pause,
    Compact,
    Reroute,
    ReleaseOwnership,
    Stop,
    Resume,
    DependencyUnblocked,
    HandoffReady,
}

impl InterruptType {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Nudge => "nudge",
            Self::RequestInput => "request_input",
            Self::Pause => "pause",
            Self::Compact => "compact",
            Self::Reroute => "reroute",
            Self::ReleaseOwnership => "release_ownership",
            Self::Stop => "stop",
            Self::Resume => "resume",
            Self::DependencyUnblocked => "dependency_unblocked",
            Self::HandoffReady => "handoff_ready",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "nudge" => Some(Self::Nudge),
            "request_input" => Some(Self::RequestInput),
            "pause" => Some(Self::Pause),
            "compact" => Some(Self::Compact),
            "reroute" => Some(Self::Reroute),
            "release_ownership" => Some(Self::ReleaseOwnership),
            "stop" => Some(Self::Stop),
            "resume" => Some(Self::Resume),
            "dependency_unblocked" => Some(Self::DependencyUnblocked),
            "handoff_ready" => Some(Self::HandoffReady),
            _ => None,
        }
    }
}

impl std::fmt::Display for InterruptType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InterruptState {
    Pending,
    Delivered,
    Acknowledged,
    Resolved,
    Expired,
    Dismissed,
}

impl InterruptState {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Delivered => "delivered",
            Self::Acknowledged => "acknowledged",
            Self::Resolved => "resolved",
            Self::Expired => "expired",
            Self::Dismissed => "dismissed",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "pending" => Some(Self::Pending),
            "delivered" => Some(Self::Delivered),
            "acknowledged" => Some(Self::Acknowledged),
            "resolved" => Some(Self::Resolved),
            "expired" => Some(Self::Expired),
            "dismissed" => Some(Self::Dismissed),
            _ => None,
        }
    }
}

impl std::fmt::Display for InterruptState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Interrupt {
    pub id: String,
    pub interrupt_type: InterruptType,
    pub priority: String,
    pub target_session_id: String,
    pub reason: String,
    pub payload: Option<serde_json::Value>,
    pub delivery_mode: String,
    pub max_retries: u32,
    pub expires_at: Option<String>,
    pub dedupe_key: Option<String>,
    pub state: InterruptState,
    pub created_at: String,
    pub delivered_at: Option<String>,
    pub acknowledged_at: Option<String>,
}

// -- Memory Record -------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Subject {
    pub kind: String,
    pub value: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryRecord {
    pub id: String,
    pub mem_type: String,
    pub scope: serde_json::Value,
    pub subjects: Vec<Subject>,
    pub summary: String,
    pub evidence: Vec<Subject>,
    pub source: Option<serde_json::Value>,
    pub confidence: f64,
    pub created_at: String,
    pub updated_at: String,
    pub expires_at: Option<String>,
    pub tags: Vec<String>,
}

// -- Tests ---------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_type_serde_roundtrip() {
        let val = EventType::LeaseAcquired;
        let json = serde_json::to_string(&val).unwrap();
        assert_eq!(json, "\"lease_acquired\"");
        let back: EventType = serde_json::from_str(&json).unwrap();
        assert_eq!(back, val);
    }

    #[test]
    fn event_type_all_variants_roundtrip() {
        let variants = [
            EventType::SessionObserved,
            EventType::TaskCreated,
            EventType::LeaseAcquired,
            EventType::LeaseReleased,
            EventType::MemoryWritten,
            EventType::InterruptRaised,
            EventType::InterruptDelivered,
            EventType::InterruptAcknowledged,
            EventType::HandoffCreated,
            EventType::HandoffAccepted,
            EventType::BlockerOpened,
            EventType::BlockerResolved,
        ];
        for v in variants {
            let s = v.as_str();
            assert_eq!(EventType::parse(s), Some(v));
            let json = serde_json::to_string(&v).unwrap();
            let back: EventType = serde_json::from_str(&json).unwrap();
            assert_eq!(back, v);
        }
    }

    #[test]
    fn lease_mode_serde_roundtrip() {
        let val = LeaseMode::SharedRead;
        let json = serde_json::to_string(&val).unwrap();
        assert_eq!(json, "\"shared_read\"");
        let back: LeaseMode = serde_json::from_str(&json).unwrap();
        assert_eq!(back, val);
    }

    #[test]
    fn interrupt_type_display() {
        assert_eq!(
            InterruptType::DependencyUnblocked.to_string(),
            "dependency_unblocked"
        );
        assert_eq!(
            InterruptType::ReleaseOwnership.to_string(),
            "release_ownership"
        );
    }

    #[test]
    fn interrupt_state_lifecycle_ordering() {
        // Verify all lifecycle states parse correctly
        let states = [
            "pending",
            "delivered",
            "acknowledged",
            "resolved",
            "expired",
            "dismissed",
        ];
        for s in states {
            assert!(InterruptState::parse(s).is_some(), "failed to parse: {s}");
        }
    }

    #[test]
    fn unknown_strings_return_none() {
        assert_eq!(EventType::parse("bogus"), None);
        assert_eq!(LeaseMode::parse("bogus"), None);
        assert_eq!(LeaseStatus::parse("bogus"), None);
        assert_eq!(BlockerStatus::parse("bogus"), None);
        assert_eq!(InterruptType::parse("bogus"), None);
        assert_eq!(InterruptState::parse("bogus"), None);
    }

    #[test]
    fn coord_event_json_roundtrip() {
        let event = CoordEvent {
            id: Some(1),
            event_type: EventType::HandoffCreated,
            timestamp: "2026-04-20T10:00:00Z".into(),
            session_id: Some("sess_1".into()),
            payload: serde_json::json!({"task": "fix_tests"}),
        };
        let json = serde_json::to_string(&event).unwrap();
        let back: CoordEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(back.event_type, EventType::HandoffCreated);
        assert_eq!(back.session_id.as_deref(), Some("sess_1"));
    }
}
