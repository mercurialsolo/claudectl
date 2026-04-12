use std::collections::HashMap;
use std::process::Command;

use crate::session::ClaudeSession;

/// Event types that can trigger hooks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HookEvent {
    SessionStart,
    StatusChange,
    NeedsInput,
    Finished,
    BudgetWarning,
    BudgetExceeded,
    Idle,
}

impl HookEvent {
    pub fn from_section(s: &str) -> Option<Self> {
        match s {
            "hooks.on_session_start" => Some(Self::SessionStart),
            "hooks.on_status_change" => Some(Self::StatusChange),
            "hooks.on_needs_input" => Some(Self::NeedsInput),
            "hooks.on_finished" => Some(Self::Finished),
            "hooks.on_budget_warning" => Some(Self::BudgetWarning),
            "hooks.on_budget_exceeded" => Some(Self::BudgetExceeded),
            "hooks.on_idle" => Some(Self::Idle),
            _ => None,
        }
    }

    pub fn name(&self) -> &'static str {
        match self {
            Self::SessionStart => "on_session_start",
            Self::StatusChange => "on_status_change",
            Self::NeedsInput => "on_needs_input",
            Self::Finished => "on_finished",
            Self::BudgetWarning => "on_budget_warning",
            Self::BudgetExceeded => "on_budget_exceeded",
            Self::Idle => "on_idle",
        }
    }
}

/// Registry of all configured hooks.
#[derive(Debug, Clone, Default)]
pub struct HookRegistry {
    hooks: HashMap<HookEvent, Vec<String>>,
}

impl HookRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add(&mut self, event: HookEvent, command: String) {
        self.hooks.entry(event).or_default().push(command);
    }

    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.hooks.is_empty()
    }

    /// Fire all hooks for an event with session context.
    pub fn fire(&self, event: HookEvent, session: &ClaudeSession) {
        let Some(commands) = self.hooks.get(&event) else {
            return;
        };

        for template in commands {
            let cmd = expand_template(template, session);

            crate::logger::log("DEBUG", &format!("hook {}: {}", event.name(), cmd));

            // Spawn async — don't block the TUI
            let _ = Command::new("sh")
                .args(["-c", &cmd])
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn();
        }
    }

    /// Fire hooks with just a status string (for events without a specific session, like StatusChange).
    pub fn fire_with_status(
        &self,
        event: HookEvent,
        session: &ClaudeSession,
        old_status: &str,
        new_status: &str,
    ) {
        let Some(commands) = self.hooks.get(&event) else {
            return;
        };

        for template in commands {
            let cmd = expand_template(template, session)
                .replace("{old_status}", old_status)
                .replace("{new_status}", new_status);

            crate::logger::log("DEBUG", &format!("hook {}: {}", event.name(), cmd));

            let _ = Command::new("sh")
                .args(["-c", &cmd])
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn();
        }
    }

    /// List all configured hooks (for `claudectl --hooks`).
    pub fn print_list(&self) {
        if self.hooks.is_empty() {
            println!("No hooks configured.");
            println!();
            println!("Add hooks in ~/.config/claudectl/config.toml:");
            println!();
            println!("  [hooks.on_needs_input]");
            println!("  run = \"say 'Claude needs input'\"");
            return;
        }

        println!("Configured hooks:");
        println!();
        for (event, commands) in &self.hooks {
            for cmd in commands {
                let display = if cmd.len() > 60 {
                    format!("{}...", &cmd[..57])
                } else {
                    cmd.clone()
                };
                println!("  {:<22} {}", event.name(), display);
            }
        }
    }
}

/// Replace template placeholders with session data.
fn expand_template(template: &str, session: &ClaudeSession) -> String {
    template
        .replace("{pid}", &session.pid.to_string())
        .replace("{project}", session.display_name())
        .replace("{status}", &session.status.to_string())
        .replace("{cost}", &format!("{:.2}", session.cost_usd))
        .replace("{model}", &session.model)
        .replace("{cwd}", &session.cwd)
        .replace("{tokens_in}", &session.total_input_tokens.to_string())
        .replace("{tokens_out}", &session.total_output_tokens.to_string())
        .replace("{elapsed}", &session.format_elapsed())
        .replace("{session_id}", &session.session_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{ClaudeSession, RawSession};

    fn make_session() -> ClaudeSession {
        let raw = RawSession {
            pid: 12345,
            session_id: "abc-def-123".into(),
            cwd: "/Users/test/projects/my-app".into(),
            started_at: 0,
        };
        let mut s = ClaudeSession::from_raw(raw);
        s.model = "opus-4.6".into();
        s.cost_usd = 3.45;
        s.total_input_tokens = 500_000;
        s.total_output_tokens = 50_000;
        s
    }

    #[test]
    fn test_expand_template() {
        let s = make_session();
        let result = expand_template("echo {pid} {project} ${cost}", &s);
        assert_eq!(result, "echo 12345 my-app $3.45");
    }

    #[test]
    fn test_expand_all_vars() {
        let s = make_session();
        let result = expand_template(
            "{pid}|{project}|{status}|{cost}|{model}|{cwd}|{tokens_in}|{tokens_out}|{session_id}",
            &s,
        );
        assert!(result.contains("12345"));
        assert!(result.contains("my-app"));
        assert!(result.contains("opus-4.6"));
        assert!(result.contains("/Users/test/projects/my-app"));
        assert!(result.contains("500000"));
        assert!(result.contains("50000"));
        assert!(result.contains("abc-def-123"));
    }

    #[test]
    fn test_hook_event_from_section() {
        assert_eq!(
            HookEvent::from_section("hooks.on_needs_input"),
            Some(HookEvent::NeedsInput)
        );
        assert_eq!(
            HookEvent::from_section("hooks.on_finished"),
            Some(HookEvent::Finished)
        );
        assert_eq!(HookEvent::from_section("hooks.unknown"), None);
        assert_eq!(HookEvent::from_section("defaults"), None);
    }

    #[test]
    fn test_registry_add_and_fire() {
        let mut reg = HookRegistry::new();
        assert!(reg.is_empty());
        reg.add(HookEvent::NeedsInput, "echo test".into());
        assert!(!reg.is_empty());
    }
}
