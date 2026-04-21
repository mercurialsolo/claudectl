#![allow(dead_code)]

use std::fs;
use std::path::PathBuf;

/// Prompt template names.
pub const ADVISORY: &str = "advisory";
pub const ORCHESTRATION: &str = "orchestration";
pub const SUMMARIZE: &str = "summarize";
pub const DECOMPOSITION: &str = "decomposition";

/// Load a prompt template by name. Checks user overrides first, falls back to built-in.
pub fn load(name: &str) -> String {
    // Check user override: ~/.claudectl/brain/prompts/{name}.md
    if let Some(path) = user_prompt_path(name) {
        if let Ok(content) = fs::read_to_string(&path) {
            if !content.trim().is_empty() {
                return content;
            }
        }
    }

    // Fall back to built-in default
    builtin(name).to_string()
}

/// Expand template variables in a prompt string.
pub fn expand(template: &str, vars: &[(&str, &str)]) -> String {
    let mut result = template.to_string();
    for (key, value) in vars {
        result = result.replace(&format!("{{{{{key}}}}}"), value);
    }
    result
}

/// Get the user override path for a prompt.
fn user_prompt_path(name: &str) -> Option<PathBuf> {
    let home = std::env::var("HOME").ok()?;
    Some(
        PathBuf::from(home)
            .join(".claudectl")
            .join("brain")
            .join("prompts")
            .join(format!("{name}.md")),
    )
}

/// Return the built-in default prompt for a given name.
fn builtin(name: &str) -> &'static str {
    match name {
        ADVISORY => ADVISORY_PROMPT,
        ORCHESTRATION => ORCHESTRATION_PROMPT,
        SUMMARIZE => SUMMARIZE_PROMPT,
        DECOMPOSITION => DECOMPOSITION_PROMPT,
        _ => {
            "Respond with JSON: {\"action\": \"deny\", \"reasoning\": \"unknown prompt\", \"confidence\": 0.0}"
        }
    }
}

/// List all available prompt names and their source (builtin vs user override).
pub fn list_prompts() -> Vec<(String, String)> {
    let names = [ADVISORY, ORCHESTRATION, SUMMARIZE, DECOMPOSITION];
    names
        .iter()
        .map(|name| {
            let source = if user_prompt_path(name).as_ref().is_some_and(|p| p.exists()) {
                "user override"
            } else {
                "built-in"
            };
            (name.to_string(), source.to_string())
        })
        .collect()
}

// ────────────────────────────────────────────────────────────────────────────
// Built-in prompt templates
// ────────────────────────────────────────────────────────────────────────────

const ADVISORY_PROMPT: &str = r#"You are a session supervisor for Claude Code. Analyze the session state and recent conversation to decide what action to take. Consider the state of other active sessions when making decisions.

## Session State
{{session_summary}}{{git_context}}{{global_session_map}}{{coordination_context}}

## Recent Conversation
{{recent_transcript}}{{few_shot_examples}}

## Decision
{{decision_prompt}}"#;

const ORCHESTRATION_PROMPT: &str = r#"You are a session orchestrator for Claude Code. You have {{session_count}} active sessions.

## Active Sessions
{{session_map}}

## Orchestration Decision
Analyze all sessions and decide if any cross-session action should be taken:
- "spawn": launch a new session to handle decomposed work (provide spawn_prompt and spawn_cwd)
- "route": send summarized output from one session to another (provide target_pid)
- "terminate": kill a redundant or stuck session
- "deny": no action needed right now

Consider: Are sessions doing redundant work? Could work be parallelized? Is a session stuck? Has one session produced output another needs?

Respond with JSON: {"action": "spawn"|"route"|"terminate"|"deny", "target_pid": <pid if route>, "spawn_prompt": "...", "spawn_cwd": ".", "reasoning": "...", "confidence": 0.0-1.0}"#;

const DECOMPOSITION_PROMPT: &str = r#"Analyze this task prompt and determine if it can be split into independent parallel sub-tasks.

Task prompt:
{{prompt}}

Working directory: {{cwd}}

Rules:
- Only split if parts are truly independent (can run in parallel without file conflicts)
- Each sub-task must be self-contained with a clear, actionable prompt
- Keep the number of sub-tasks between 2 and {{max_tasks}}
- If the task is already focused/atomic, set decomposable to false
- Name each sub-task with a short slug (lowercase, hyphens)

Respond with JSON:
{"decomposable": true/false, "reasoning": "why or why not", "tasks": [{"name": "short-name", "prompt": "full prompt text", "depends_on": ["other-task-name"]}]}"#;

const SUMMARIZE_PROMPT: &str = r#"Summarize this output from session '{{source_project}}' for another Claude Code session working on: {{target_task}}

Keep ONLY what's relevant to the target task. Be concise — this will be injected into another session's context. Max 500 words.

Output to summarize:
{{source_output}}"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_advisory_exists() {
        let prompt = builtin(ADVISORY);
        assert!(prompt.contains("session supervisor"));
        assert!(prompt.contains("{{session_summary}}"));
    }

    #[test]
    fn builtin_orchestration_exists() {
        let prompt = builtin(ORCHESTRATION);
        assert!(prompt.contains("orchestrator"));
        assert!(prompt.contains("{{session_map}}"));
    }

    #[test]
    fn builtin_summarize_exists() {
        let prompt = builtin(SUMMARIZE);
        assert!(prompt.contains("Summarize"));
        assert!(prompt.contains("{{source_output}}"));
    }

    #[test]
    fn expand_replaces_variables() {
        let template = "Hello {{name}}, you have {{count}} items.";
        let result = expand(template, &[("name", "Alice"), ("count", "3")]);
        assert_eq!(result, "Hello Alice, you have 3 items.");
    }

    #[test]
    fn expand_no_variables_unchanged() {
        let template = "No variables here.";
        let result = expand(template, &[]);
        assert_eq!(result, "No variables here.");
    }

    #[test]
    fn load_falls_back_to_builtin() {
        // No user override exists, should return built-in
        let prompt = load(ADVISORY);
        assert!(prompt.contains("session supervisor"));
    }

    #[test]
    fn list_prompts_returns_all() {
        let prompts = list_prompts();
        assert_eq!(prompts.len(), 4);
        assert!(prompts.iter().any(|(n, _)| n == ADVISORY));
        assert!(prompts.iter().any(|(n, _)| n == ORCHESTRATION));
        assert!(prompts.iter().any(|(n, _)| n == SUMMARIZE));
        assert!(prompts.iter().any(|(n, _)| n == DECOMPOSITION));
    }

    #[test]
    fn load_user_override() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.md");
        fs::write(&path, "Custom prompt for {{name}}").unwrap();

        let content = fs::read_to_string(&path).unwrap();
        let result = expand(&content, &[("name", "testing")]);
        assert_eq!(result, "Custom prompt for testing");
    }
}
