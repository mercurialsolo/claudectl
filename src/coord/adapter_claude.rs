#![allow(dead_code)]

use super::adapter::*;
use crate::discovery;

/// Claude Code adapter -- wraps existing claudectl discovery, monitoring, and terminal integration.
pub struct ClaudeCodeAdapter;

impl AgentAdapter for ClaudeCodeAdapter {
    fn family(&self) -> AgentFamily {
        AgentFamily::ClaudeCode
    }

    fn capabilities(&self) -> AdapterCapabilities {
        AdapterCapabilities {
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

    fn discover_sessions(&self) -> Vec<AgentIdentity> {
        let sessions = discovery::scan_sessions();
        sessions
            .into_iter()
            .map(|s| AgentIdentity {
                agent_family: "claude-code".into(),
                session_id: s.session_id,
                cwd: s.cwd,
                branch: None, // Resolved later by resolve_worktree_ids
                pid: Some(s.pid),
            })
            .collect()
    }

    fn get_state(&self, session_id: &str) -> Option<AgentState> {
        let sessions = discovery::scan_sessions();
        let session = sessions.iter().find(|s| s.session_id == session_id)?;

        let context_pressure = if session.context_max > 0 {
            Some(session.context_tokens as f64 / session.context_max as f64)
        } else {
            None
        };

        Some(AgentState {
            status: session.status.to_string(),
            context_pressure,
            pending_tool: session.pending_tool_name.clone(),
            last_output: None,
            cost_usd: Some(session.cost_usd),
        })
    }

    fn send_input(&self, session_id: &str, text: &str) -> Result<(), String> {
        let sessions = discovery::scan_sessions();
        let session = sessions
            .iter()
            .find(|s| s.session_id == session_id)
            .ok_or_else(|| format!("Session not found: {session_id}"))?;
        crate::terminals::send_input(session, text)
    }

    fn pause(&self, session_id: &str) -> Result<(), String> {
        // Send a compact/pause instruction to the session
        self.send_input(session_id, "/compact")
    }

    fn resume(&self, session_id: &str) -> Result<(), String> {
        // Claude Code sessions resume automatically when given input
        self.send_input(session_id, "continue")
    }

    fn terminate(&self, session_id: &str) -> Result<(), String> {
        let sessions = discovery::scan_sessions();
        let session = sessions
            .iter()
            .find(|s| s.session_id == session_id)
            .ok_or_else(|| format!("Session not found: {session_id}"))?;

        let pid = session.pid;
        let output = std::process::Command::new("kill")
            .arg(pid.to_string())
            .output()
            .map_err(|e| format!("kill failed: {e}"))?;

        if output.status.success() {
            Ok(())
        } else {
            Err(format!("kill returned: {}", output.status))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_adapter_family() {
        let adapter = ClaudeCodeAdapter;
        assert_eq!(adapter.family(), AgentFamily::ClaudeCode);
    }

    #[test]
    fn claude_adapter_capabilities() {
        let adapter = ClaudeCodeAdapter;
        let caps = adapter.capabilities();
        assert!(caps.discover_sessions);
        assert!(caps.monitor_state);
        assert!(caps.send_input);
        assert!(caps.deliver_interrupt);
        assert!(caps.terminate);
        assert!(!caps.request_checkpoint);
        assert!(!caps.request_compaction);
    }

    #[test]
    fn claude_adapter_discover_returns_vec() {
        let adapter = ClaudeCodeAdapter;
        // Should not panic, even if no sessions exist
        let sessions = adapter.discover_sessions();
        for s in &sessions {
            assert_eq!(s.agent_family, "claude-code");
            assert!(s.pid.is_some());
        }
    }
}
