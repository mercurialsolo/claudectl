#![allow(dead_code)]

use super::adapter::*;

/// Codex adapter -- stub implementation demonstrating the adapter pattern.
/// Future versions will implement full discovery and interaction.
pub struct CodexAdapter;

impl AgentAdapter for CodexAdapter {
    fn family(&self) -> AgentFamily {
        AgentFamily::Codex
    }

    fn capabilities(&self) -> AdapterCapabilities {
        AdapterCapabilities::minimal()
    }

    fn discover_sessions(&self) -> Vec<AgentIdentity> {
        // Stub: Codex session discovery not yet implemented.
        // Future: scan for codex processes, parse their state files.
        Vec::new()
    }

    fn get_state(&self, _session_id: &str) -> Option<AgentState> {
        None
    }

    fn send_input(&self, _session_id: &str, _text: &str) -> Result<(), String> {
        Err("Codex adapter: send_input not yet implemented".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codex_adapter_family() {
        let adapter = CodexAdapter;
        assert_eq!(adapter.family(), AgentFamily::Codex);
    }

    #[test]
    fn codex_adapter_minimal_capabilities() {
        let adapter = CodexAdapter;
        let caps = adapter.capabilities();
        assert!(caps.discover_sessions);
        assert!(!caps.send_input);
        assert!(!caps.terminate);
        assert_eq!(caps.count(), 1);
    }

    #[test]
    fn codex_adapter_discover_is_empty() {
        let adapter = CodexAdapter;
        assert!(adapter.discover_sessions().is_empty());
    }

    #[test]
    fn codex_adapter_send_input_returns_err() {
        let adapter = CodexAdapter;
        assert!(adapter.send_input("sess", "hello").is_err());
    }
}
