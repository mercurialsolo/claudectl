#![allow(dead_code)]

use serde::{Deserialize, Serialize};

/// Capabilities an agent adapter supports.
/// The coordination layer checks these before attempting operations.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdapterCapabilities {
    pub discover_sessions: bool,
    pub monitor_state: bool,
    pub send_input: bool,
    pub deliver_interrupt: bool,
    pub request_checkpoint: bool,
    pub request_compaction: bool,
    pub pause: bool,
    pub resume: bool,
    pub terminate: bool,
}

impl AdapterCapabilities {
    /// All capabilities enabled (for fully-supported adapters).
    pub fn full() -> Self {
        Self {
            discover_sessions: true,
            monitor_state: true,
            send_input: true,
            deliver_interrupt: true,
            request_checkpoint: false,
            request_compaction: false,
            pause: true,
            resume: true,
            terminate: true,
        }
    }

    /// Minimal capabilities (for stub/partial adapters).
    pub fn minimal() -> Self {
        Self {
            discover_sessions: true,
            monitor_state: false,
            send_input: false,
            deliver_interrupt: false,
            request_checkpoint: false,
            request_compaction: false,
            pause: false,
            resume: false,
            terminate: false,
        }
    }

    /// Count of enabled capabilities.
    pub fn count(&self) -> usize {
        [
            self.discover_sessions,
            self.monitor_state,
            self.send_input,
            self.deliver_interrupt,
            self.request_checkpoint,
            self.request_compaction,
            self.pause,
            self.resume,
            self.terminate,
        ]
        .iter()
        .filter(|&&v| v)
        .count()
    }
}

/// Identity of a discovered agent session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentIdentity {
    pub agent_family: String,
    pub session_id: String,
    pub cwd: String,
    pub branch: Option<String>,
    pub pid: Option<u32>,
}

/// Current state of an agent session as reported by the adapter.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentState {
    pub status: String,
    pub context_pressure: Option<f64>,
    pub pending_tool: Option<String>,
    pub last_output: Option<String>,
    pub cost_usd: Option<f64>,
}

/// Known agent families.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AgentFamily {
    ClaudeCode,
    Codex,
}

impl AgentFamily {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::ClaudeCode => "claude-code",
            Self::Codex => "codex",
        }
    }

    pub fn all() -> &'static [AgentFamily] {
        &[AgentFamily::ClaudeCode, AgentFamily::Codex]
    }
}

impl std::fmt::Display for AgentFamily {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The agent adapter trait. Each coding agent runtime implements this
/// to participate in the coordination layer.
pub trait AgentAdapter {
    /// The agent family this adapter supports.
    fn family(&self) -> AgentFamily;

    /// Declared capabilities of this adapter.
    fn capabilities(&self) -> AdapterCapabilities;

    /// Discover active sessions for this agent family.
    fn discover_sessions(&self) -> Vec<AgentIdentity>;

    /// Get the current state of a session by ID.
    fn get_state(&self, session_id: &str) -> Option<AgentState>;

    /// Send text input to a session.
    fn send_input(&self, session_id: &str, text: &str) -> Result<(), String>;

    /// Pause a session at a safe boundary.
    fn pause(&self, session_id: &str) -> Result<(), String> {
        let _ = session_id;
        Err("not supported".into())
    }

    /// Resume a paused session.
    fn resume(&self, session_id: &str) -> Result<(), String> {
        let _ = session_id;
        Err("not supported".into())
    }

    /// Terminate a session.
    fn terminate(&self, session_id: &str) -> Result<(), String> {
        let _ = session_id;
        Err("not supported".into())
    }
}

/// Get all registered adapters.
pub fn all_adapters() -> Vec<Box<dyn AgentAdapter>> {
    vec![
        Box::new(super::adapter_claude::ClaudeCodeAdapter),
        Box::new(super::adapter_codex::CodexAdapter),
    ]
}

/// Get an adapter by family name.
pub fn get_adapter(family: &str) -> Option<Box<dyn AgentAdapter>> {
    match family {
        "claude-code" => Some(Box::new(super::adapter_claude::ClaudeCodeAdapter)),
        "codex" => Some(Box::new(super::adapter_codex::CodexAdapter)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capabilities_full_count() {
        let caps = AdapterCapabilities::full();
        // full() has checkpoint and compaction as false
        assert_eq!(caps.count(), 7);
    }

    #[test]
    fn capabilities_minimal_count() {
        let caps = AdapterCapabilities::minimal();
        assert_eq!(caps.count(), 1);
    }

    #[test]
    fn agent_family_display() {
        assert_eq!(AgentFamily::ClaudeCode.to_string(), "claude-code");
        assert_eq!(AgentFamily::Codex.to_string(), "codex");
    }

    #[test]
    fn all_adapters_registered() {
        let adapters = all_adapters();
        assert_eq!(adapters.len(), 2);
        assert_eq!(adapters[0].family(), AgentFamily::ClaudeCode);
        assert_eq!(adapters[1].family(), AgentFamily::Codex);
    }

    #[test]
    fn get_adapter_by_name() {
        assert!(get_adapter("claude-code").is_some());
        assert!(get_adapter("codex").is_some());
        assert!(get_adapter("unknown").is_none());
    }
}
