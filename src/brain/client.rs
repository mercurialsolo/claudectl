#![allow(dead_code)]

use std::process::Command;

use crate::config::BrainConfig;
use crate::rules::RuleAction;

/// The brain's suggestion for a session, parsed from the LLM response.
#[derive(Debug, Clone)]
pub struct BrainSuggestion {
    pub action: RuleAction,
    pub message: Option<String>,
    pub reasoning: String,
    pub confidence: f64,
    /// Epoch seconds when this suggestion was created.
    /// Used by time-to-correct analysis to measure user reaction latency.
    pub suggested_at: u64,
}

/// Call the local LLM endpoint via curl and parse the response, using the
/// model configured as the primary (`config.model`).
pub fn infer(config: &BrainConfig, prompt: &str) -> Result<BrainSuggestion, String> {
    infer_with_model(config, prompt, &config.model)
}

/// Whether a primary-model decision is too uncertain and should be re-run on
/// the stronger escalation model. Pure so the routing policy is unit-testable
/// without an LLM. Escalation only happens when a strong model is configured.
pub fn should_escalate(confidence: f64, threshold: f64, has_strong_model: bool) -> bool {
    has_strong_model && confidence < threshold
}

/// Two-tier routed inference (#370): ask the cheap/primary model first; if it
/// returns a low-confidence decision and an `escalation_model` is configured,
/// re-run the prompt on the stronger model and take that answer. With no
/// escalation model this is byte-for-byte `infer`. A failed escalation falls
/// back to the primary suggestion rather than erroring.
pub fn infer_routed(config: &BrainConfig, prompt: &str) -> Result<BrainSuggestion, String> {
    let primary = infer_with_model(config, prompt, &config.model)?;
    let Some(strong) = config.escalation_model.as_deref() else {
        return Ok(primary);
    };
    if !should_escalate(primary.confidence, config.escalation_threshold, true) {
        return Ok(primary);
    }
    crate::logger::log(
        "DEBUG",
        &format!(
            "brain routing: confidence {:.2} < {:.2}, escalating {} -> {strong}",
            primary.confidence, config.escalation_threshold, config.model
        ),
    );
    match infer_with_model(config, prompt, strong) {
        Ok(escalated) => Ok(escalated),
        Err(e) => {
            crate::logger::log(
                "WARN",
                &format!("brain escalation to {strong} failed ({e}); using primary"),
            );
            Ok(primary)
        }
    }
}

/// Call the local LLM endpoint via curl with an explicit `model`, parsing the
/// response. The model is a parameter so the router can target either the
/// primary or the escalation model without cloning the whole config.
fn infer_with_model(
    config: &BrainConfig,
    prompt: &str,
    model: &str,
) -> Result<BrainSuggestion, String> {
    let is_openai = is_openai_compatible(&config.endpoint);

    let payload = if is_openai {
        // OpenAI-compatible format (llama.cpp, vLLM, LM Studio)
        serde_json::json!({
            "model": model,
            "messages": [
                {"role": "user", "content": prompt}
            ],
            "response_format": {"type": "json_object"},
            "stream": false,
        })
    } else {
        // Ollama /api/generate format (default)
        serde_json::json!({
            "model": model,
            "prompt": prompt,
            "stream": false,
            "format": "json",
        })
    };

    let body = serde_json::to_string(&payload).map_err(|e| format!("json error: {e}"))?;
    let timeout_secs = (config.timeout_ms / 1000).max(1);

    let output = Command::new("curl")
        .args([
            "-s",
            "-X",
            "POST",
            "-H",
            "Content-Type: application/json",
            "-d",
            &body,
            "--max-time",
            &timeout_secs.to_string(),
            &config.endpoint,
        ])
        .output()
        .map_err(|e| format!("curl failed: {e}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("curl error (exit {}): {stderr}", output.status));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    if is_openai {
        parse_openai_response(&stdout)
    } else {
        parse_ollama_response(&stdout)
    }
}

/// Result of a task decomposition analysis.
#[derive(Debug, Clone)]
pub struct DecompositionResult {
    pub decomposable: bool,
    pub reasoning: String,
    pub tasks: Vec<DecomposedTask>,
}

/// A single sub-task from decomposition.
#[derive(Debug, Clone)]
pub struct DecomposedTask {
    pub name: String,
    pub prompt: String,
    pub depends_on: Vec<String>,
}

/// Analyze a prompt and determine if it can be split into parallel sub-tasks.
pub fn decompose_prompt(
    config: &BrainConfig,
    prompt: &str,
    cwd: &str,
    max_tasks: usize,
) -> Result<DecompositionResult, String> {
    let template = super::prompts::load(super::prompts::DECOMPOSITION);
    let expanded = super::prompts::expand(
        &template,
        &[
            ("prompt", prompt),
            ("cwd", cwd),
            ("max_tasks", &max_tasks.to_string()),
        ],
    );

    let response = call_llm(config, &expanded)?;
    parse_decomposition_json(&response)
}

/// Parse the decomposition JSON response.
pub fn parse_decomposition_json(text: &str) -> Result<DecompositionResult, String> {
    let json: serde_json::Value = serde_json::from_str(text.trim())
        .map_err(|e| format!("invalid decomposition JSON: {e}"))?;

    let decomposable = json
        .get("decomposable")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let reasoning = json
        .get("reasoning")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    let tasks = if decomposable {
        json.get("tasks")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|t| {
                        Some(DecomposedTask {
                            name: t.get("name")?.as_str()?.to_string(),
                            prompt: t.get("prompt")?.as_str()?.to_string(),
                            depends_on: t
                                .get("depends_on")
                                .and_then(|v| v.as_array())
                                .map(|deps| {
                                    deps.iter()
                                        .filter_map(|d| d.as_str().map(|s| s.to_string()))
                                        .collect()
                                })
                                .unwrap_or_default(),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default()
    } else {
        Vec::new()
    };

    Ok(DecompositionResult {
        decomposable,
        reasoning,
        tasks,
    })
}

/// Detect if the endpoint is OpenAI-compatible based on URL path.
fn is_openai_compatible(endpoint: &str) -> bool {
    endpoint.contains("/v1/chat") || endpoint.contains("/v1/completions")
}

/// Summarize source session output for routing to a target session.
/// Returns a compact summary that won't bloat the target's context.
pub fn summarize_for_routing(
    config: &BrainConfig,
    source_output: &str,
    source_project: &str,
    target_task: &str,
) -> Result<String, String> {
    let template = super::prompts::load(super::prompts::SUMMARIZE);
    let prompt = super::prompts::expand(
        &template,
        &[
            ("source_project", source_project),
            ("target_task", target_task),
            ("source_output", source_output),
        ],
    );

    let response = call_llm(config, &prompt)?;
    Ok(response.trim().to_string())
}

/// Make an LLM API call, auto-detecting ollama vs OpenAI format from the endpoint URL.
fn call_llm(config: &BrainConfig, prompt: &str) -> Result<String, String> {
    let is_openai = is_openai_compatible(&config.endpoint);

    let payload = if is_openai {
        serde_json::json!({
            "model": config.model,
            "messages": [{"role": "user", "content": prompt}],
            "stream": false,
        })
    } else {
        serde_json::json!({
            "model": config.model,
            "prompt": prompt,
            "stream": false,
        })
    };

    let body = serde_json::to_string(&payload).map_err(|e| format!("json error: {e}"))?;
    let timeout_secs = (config.timeout_ms / 1000).max(1);

    let output = Command::new("curl")
        .args([
            "-s",
            "-X",
            "POST",
            "-H",
            "Content-Type: application/json",
            "-d",
            &body,
            "--max-time",
            &timeout_secs.to_string(),
            &config.endpoint,
        ])
        .output()
        .map_err(|e| format!("curl failed: {e}"))?;

    if !output.status.success() {
        return Err(format!(
            "curl error: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: serde_json::Value =
        serde_json::from_str(&stdout).map_err(|e| format!("invalid response: {e}"))?;

    if is_openai {
        // OpenAI: choices[0].message.content
        Ok(json
            .get("choices")
            .and_then(|v| v.as_array())
            .and_then(|arr| arr.first())
            .and_then(|choice| choice.get("message"))
            .and_then(|msg| msg.get("content"))
            .and_then(|v| v.as_str())
            .unwrap_or(&stdout)
            .to_string())
    } else {
        // Ollama: response field
        Ok(json
            .get("response")
            .and_then(|v| v.as_str())
            .unwrap_or(&stdout)
            .to_string())
    }
}

/// Parse the ollama `/api/generate` response format.
fn parse_ollama_response(response: &str) -> Result<BrainSuggestion, String> {
    let json: serde_json::Value =
        serde_json::from_str(response).map_err(|e| format!("invalid JSON response: {e}"))?;

    // Ollama wraps the generated text in a "response" field
    let generated = json
        .get("response")
        .and_then(|v| v.as_str())
        .unwrap_or(response);

    parse_suggestion_json(generated)
}

/// Parse OpenAI-compatible /v1/chat/completions response.
fn parse_openai_response(response: &str) -> Result<BrainSuggestion, String> {
    let json: serde_json::Value =
        serde_json::from_str(response).map_err(|e| format!("invalid JSON response: {e}"))?;

    // OpenAI format: choices[0].message.content
    let content = json
        .get("choices")
        .and_then(|v| v.as_array())
        .and_then(|arr| arr.first())
        .and_then(|choice| choice.get("message"))
        .and_then(|msg| msg.get("content"))
        .and_then(|v| v.as_str())
        .unwrap_or(response);

    parse_suggestion_json(content)
}

/// Parse the structured JSON that the brain LLM is expected to produce.
pub fn parse_suggestion_json(text: &str) -> Result<BrainSuggestion, String> {
    // The LLM should produce JSON like:
    // {"action": "approve", "message": null, "reasoning": "safe command", "confidence": 0.95}
    let json: serde_json::Value =
        serde_json::from_str(text.trim()).map_err(|e| format!("invalid suggestion JSON: {e}"))?;

    let action_str = json
        .get("action")
        .and_then(|v| v.as_str())
        .ok_or("missing 'action' field")?;

    let action = if action_str == "route" {
        let target_pid = json
            .get("target_pid")
            .and_then(|v| v.as_u64())
            .ok_or("route action requires 'target_pid' field")? as u32;
        RuleAction::Route { target_pid }
    } else if action_str == "spawn" {
        let prompt = json
            .get("spawn_prompt")
            .and_then(|v| v.as_str())
            .ok_or("spawn action requires 'spawn_prompt' field")?
            .to_string();
        let cwd = json
            .get("spawn_cwd")
            .and_then(|v| v.as_str())
            .unwrap_or(".")
            .to_string();
        RuleAction::Spawn { prompt, cwd }
    } else if action_str == "delegate" {
        let agent = json
            .get("agent")
            .and_then(|v| v.as_str())
            .ok_or("delegate action requires 'agent' field")?
            .to_string();
        let prompt = json
            .get("delegate_prompt")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        RuleAction::Delegate { agent, prompt }
    } else {
        RuleAction::parse(action_str).ok_or_else(|| format!("unknown action '{action_str}'"))?
    };

    let message = json
        .get("message")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    let reasoning = json
        .get("reasoning")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    let confidence = json
        .get("confidence")
        .and_then(|v| v.as_f64())
        .unwrap_or(0.5);

    Ok(BrainSuggestion {
        action,
        message,
        reasoning,
        confidence: confidence.clamp(0.0, 1.0),
        suggested_at: epoch_secs(),
    })
}

fn epoch_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escalate_only_when_uncertain_and_strong_model_present() {
        // Low confidence + a strong model configured ⇒ escalate.
        assert!(should_escalate(0.5, 0.7, true));
        // Confident enough ⇒ keep the primary answer.
        assert!(!should_escalate(0.7, 0.7, true));
        assert!(!should_escalate(0.95, 0.7, true));
        // No escalation model ⇒ never escalate, even when uncertain.
        assert!(!should_escalate(0.1, 0.7, false));
    }

    #[test]
    fn parse_approve_suggestion() {
        let json = r#"{"action": "approve", "reasoning": "safe read command", "confidence": 0.95}"#;
        let s = parse_suggestion_json(json).unwrap();
        assert_eq!(s.action, RuleAction::Approve);
        assert_eq!(s.reasoning, "safe read command");
        assert!((s.confidence - 0.95).abs() < f64::EPSILON);
        assert!(s.message.is_none());
    }

    #[test]
    fn parse_send_suggestion() {
        let json = r#"{"action": "send", "message": "continue", "reasoning": "task in progress", "confidence": 0.8}"#;
        let s = parse_suggestion_json(json).unwrap();
        assert_eq!(s.action, RuleAction::Send);
        assert_eq!(s.message.as_deref(), Some("continue"));
    }

    #[test]
    fn parse_deny_suggestion() {
        let json = r#"{"action": "deny", "reasoning": "dangerous command", "confidence": 0.99}"#;
        let s = parse_suggestion_json(json).unwrap();
        assert_eq!(s.action, RuleAction::Deny);
    }

    #[test]
    fn parse_terminate_suggestion() {
        let json = r#"{"action": "terminate", "reasoning": "over budget", "confidence": 0.7}"#;
        let s = parse_suggestion_json(json).unwrap();
        assert_eq!(s.action, RuleAction::Terminate);
    }

    #[test]
    fn parse_missing_action_fails() {
        let json = r#"{"reasoning": "no action"}"#;
        assert!(parse_suggestion_json(json).is_err());
    }

    #[test]
    fn parse_unknown_action_fails() {
        let json = r#"{"action": "dance", "reasoning": "invalid"}"#;
        assert!(parse_suggestion_json(json).is_err());
    }

    #[test]
    fn parse_confidence_clamped() {
        let json = r#"{"action": "approve", "reasoning": "test", "confidence": 1.5}"#;
        let s = parse_suggestion_json(json).unwrap();
        assert!((s.confidence - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn parse_ollama_wrapped_response() {
        let ollama_response = r#"{"model":"gemma4","response":"{\"action\":\"approve\",\"reasoning\":\"safe\",\"confidence\":0.9}","done":true}"#;
        let s = parse_ollama_response(ollama_response).unwrap();
        assert_eq!(s.action, RuleAction::Approve);
    }

    #[test]
    fn defaults_on_missing_optional_fields() {
        let json = r#"{"action": "approve"}"#;
        let s = parse_suggestion_json(json).unwrap();
        assert_eq!(s.reasoning, "");
        assert!((s.confidence - 0.5).abs() < f64::EPSILON);
        assert!(s.message.is_none());
    }

    #[test]
    fn parse_openai_wrapped_response() {
        let openai_response = r#"{"choices":[{"message":{"content":"{\"action\":\"deny\",\"reasoning\":\"dangerous\",\"confidence\":0.95}"}}]}"#;
        let s = parse_openai_response(openai_response).unwrap();
        assert_eq!(s.action, RuleAction::Deny);
        assert!((s.confidence - 0.95).abs() < f64::EPSILON);
    }

    #[test]
    fn detect_openai_endpoint() {
        assert!(is_openai_compatible(
            "http://localhost:8080/v1/chat/completions"
        ));
        assert!(is_openai_compatible("http://host/v1/completions"));
        assert!(!is_openai_compatible("http://localhost:11434/api/generate"));
    }

    #[test]
    fn parse_decomposition_decomposable() {
        let json = r#"{"decomposable": true, "reasoning": "two independent modules", "tasks": [{"name": "task-a", "prompt": "update module A", "depends_on": []}, {"name": "task-b", "prompt": "update module B", "depends_on": []}]}"#;
        let result = parse_decomposition_json(json).unwrap();
        assert!(result.decomposable);
        assert_eq!(result.tasks.len(), 2);
        assert_eq!(result.tasks[0].name, "task-a");
        assert_eq!(result.tasks[1].name, "task-b");
        assert!(result.tasks[0].depends_on.is_empty());
    }

    #[test]
    fn parse_decomposition_not_decomposable() {
        let json = r#"{"decomposable": false, "reasoning": "task is atomic", "tasks": []}"#;
        let result = parse_decomposition_json(json).unwrap();
        assert!(!result.decomposable);
        assert!(result.tasks.is_empty());
        assert!(result.reasoning.contains("atomic"));
    }

    #[test]
    fn parse_decomposition_with_dependencies() {
        let json = r#"{"decomposable": true, "reasoning": "pipeline", "tasks": [{"name": "analyze", "prompt": "analyze code", "depends_on": []}, {"name": "fix", "prompt": "fix issues", "depends_on": ["analyze"]}]}"#;
        let result = parse_decomposition_json(json).unwrap();
        assert_eq!(result.tasks[1].depends_on, vec!["analyze"]);
    }

    #[test]
    fn parse_decomposition_invalid_json() {
        assert!(parse_decomposition_json("not json").is_err());
    }
}
