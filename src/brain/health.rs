//! Brain reachability probe — the single source of truth for "will the brain
//! actually do anything right now?".
//!
//! The value proposition ("a local-LLM brain that learns from you") is silently
//! gated behind a running local LLM. The most common activation failure isn't a
//! dead endpoint — it's a *live* endpoint with the model never pulled (`ollama
//! serve` running, but no `ollama pull`). A plain reachability check reports that
//! as healthy, so the brain fails on its first inference with no explanation.
//!
//! This module distinguishes the three states that need different fixes:
//! endpoint down, endpoint up but model missing, and ready. `doctor` and the
//! CLI status paths call `probe`; the pure `classify`/`model_present` helpers are
//! unit-tested without touching the network.

use crate::config::BrainConfig;
use std::process::Command;

/// The three actionable states of the local-LLM brain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BrainHealth {
    /// Nothing is listening on the configured endpoint.
    EndpointDown { endpoint: String, model: String },
    /// The endpoint answers, but the configured model has not been pulled.
    ModelMissing {
        endpoint: String,
        model: String,
        available: Vec<String>,
    },
    /// Endpoint reachable and the model is present (or, for OpenAI-compatible
    /// endpoints, reachable and assumed present — those can't be listed uniformly).
    Ready { endpoint: String, model: String },
}

/// What a network probe observed. Kept separate from `BrainHealth` so the
/// decision logic (`classify`) stays pure and testable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Observation {
    /// The probe request failed or returned a non-success status.
    Unreachable,
    /// Ollama `/api/tags` returned this list of installed model names.
    ModelList(Vec<String>),
    /// Endpoint answered but its models can't be listed (OpenAI-compatible).
    ReachableNoList,
}

impl BrainHealth {
    /// True only when the brain can serve an inference right now.
    pub fn is_ready(&self) -> bool {
        matches!(self, BrainHealth::Ready { .. })
    }

    /// One-line status suitable for `doctor` rows and CLI banners.
    pub fn headline(&self) -> String {
        match self {
            BrainHealth::Ready { endpoint, model } => {
                format!("brain ready — {model} at {endpoint}")
            }
            BrainHealth::ModelMissing { model, .. } => {
                format!("endpoint up, but model '{model}' is not pulled")
            }
            BrainHealth::EndpointDown { endpoint, .. } => {
                format!("no local-LLM endpoint reachable at {endpoint}")
            }
        }
    }

    /// The exact next command to make the brain work, or `None` when ready.
    pub fn fix_hint(&self) -> Option<String> {
        match self {
            BrainHealth::Ready { .. } => None,
            BrainHealth::ModelMissing {
                model, available, ..
            } => {
                let have = if available.is_empty() {
                    "none installed".to_string()
                } else {
                    available.join(", ")
                };
                Some(format!(
                    "Pull the configured model: `ollama pull {model}` (installed: {have})."
                ))
            }
            BrainHealth::EndpointDown { model, .. } => Some(format!(
                "Start a local LLM: `brew install ollama && ollama serve &`, then `ollama pull {model}`."
            )),
        }
    }
}

/// Whether the OpenAI-compatible chat/completions path is configured. Mirrors
/// `client::is_openai_compatible` (kept local to avoid widening that fn's
/// visibility for one caller).
fn is_openai_compatible(endpoint: &str) -> bool {
    endpoint.contains("/v1/chat") || endpoint.contains("/v1/completions")
}

/// Scheme+host prefix of an endpoint URL, dropping any path. Returns `None` when
/// the URL has no `scheme://host` shape we can build a sibling path from.
fn base_url(endpoint: &str) -> Option<String> {
    let scheme_end = endpoint.find("://")? + 3;
    let host = endpoint[scheme_end..].split('/').next().unwrap_or("");
    if host.is_empty() {
        return None;
    }
    Some(format!("{}{host}", &endpoint[..scheme_end]))
}

/// The ollama `/api/tags` URL for a `/api/generate`-style endpoint. `None` for
/// OpenAI-compatible endpoints, whose models we don't list.
pub fn tags_url_for(endpoint: &str) -> Option<String> {
    if is_openai_compatible(endpoint) {
        return None;
    }
    Some(format!("{}/api/tags", base_url(endpoint)?))
}

/// Extract installed model names from an ollama `/api/tags` JSON body. Unknown
/// or malformed shapes yield an empty list (treated downstream as "no models").
pub fn parse_model_list(tags_json: &str) -> Vec<String> {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(tags_json) else {
        return Vec::new();
    };
    value
        .get("models")
        .and_then(|m| m.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|m| m.get("name").and_then(|n| n.as_str()))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// A model reference with no explicit tag means `:latest` in ollama.
fn normalize_model(model: &str) -> String {
    if model.contains(':') {
        model.to_string()
    } else {
        format!("{model}:latest")
    }
}

/// Whether `wanted` is among `available`, honoring the implicit `:latest` tag.
pub fn model_present(available: &[String], wanted: &str) -> bool {
    let want = normalize_model(wanted);
    available.iter().any(|a| normalize_model(a) == want)
}

/// Pure decision: given the configured endpoint/model and what a probe saw,
/// which actionable state are we in?
pub fn classify(endpoint: &str, model: &str, observed: Observation) -> BrainHealth {
    match observed {
        Observation::Unreachable => BrainHealth::EndpointDown {
            endpoint: endpoint.to_string(),
            model: model.to_string(),
        },
        Observation::ReachableNoList => BrainHealth::Ready {
            endpoint: endpoint.to_string(),
            model: model.to_string(),
        },
        Observation::ModelList(available) => {
            if model_present(&available, model) {
                BrainHealth::Ready {
                    endpoint: endpoint.to_string(),
                    model: model.to_string(),
                }
            } else {
                BrainHealth::ModelMissing {
                    endpoint: endpoint.to_string(),
                    model: model.to_string(),
                    available,
                }
            }
        }
    }
}

/// `curl -sS <url>` with a short timeout, returning the body on success.
fn curl_get(url: &str, timeout_secs: u64) -> Option<String> {
    let output = Command::new("curl")
        .args(["-sS", "--max-time", &timeout_secs.to_string(), url])
        .output()
        .ok()?;
    if !output.status.success() || output.stdout.is_empty() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Probe the configured brain endpoint and classify its health. Does real IO
/// (one short `curl`); the classification is delegated to the pure `classify`.
pub fn probe(config: &BrainConfig) -> BrainHealth {
    // Keep the probe snappy regardless of the (possibly long) inference timeout.
    let timeout_secs = (config.timeout_ms / 1000).clamp(1, 3);
    let observed = match tags_url_for(&config.endpoint) {
        Some(tags_url) => match curl_get(&tags_url, timeout_secs) {
            Some(body) => Observation::ModelList(parse_model_list(&body)),
            None => Observation::Unreachable,
        },
        // OpenAI-compatible: we can't list models, so a reachable base means ready.
        None => match base_url(&config.endpoint).and_then(|b| curl_get(&b, timeout_secs)) {
            Some(_) => Observation::ReachableNoList,
            None => Observation::Unreachable,
        },
    };
    classify(&config.endpoint, &config.model, observed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tags_url_derived_from_ollama_generate_endpoint() {
        assert_eq!(
            tags_url_for("http://localhost:11434/api/generate").as_deref(),
            Some("http://localhost:11434/api/tags")
        );
    }

    #[test]
    fn tags_url_none_for_openai_endpoint() {
        assert_eq!(
            tags_url_for("http://localhost:8080/v1/chat/completions"),
            None
        );
    }

    #[test]
    fn base_url_strips_path_keeps_port() {
        assert_eq!(
            base_url("http://192.168.1.5:11434/api/generate").as_deref(),
            Some("http://192.168.1.5:11434")
        );
    }

    #[test]
    fn parse_model_list_extracts_names() {
        let body = r#"{"models":[{"name":"gemma4:e4b","size":1},{"name":"llama3:latest"}]}"#;
        assert_eq!(parse_model_list(body), vec!["gemma4:e4b", "llama3:latest"]);
    }

    #[test]
    fn parse_model_list_handles_empty_and_garbage() {
        assert!(parse_model_list(r#"{"models":[]}"#).is_empty());
        assert!(parse_model_list("not json").is_empty());
        assert!(parse_model_list(r#"{"other":1}"#).is_empty());
    }

    #[test]
    fn model_present_honors_implicit_latest_tag() {
        let have = vec!["llama3:latest".to_string()];
        assert!(model_present(&have, "llama3"));
        assert!(model_present(&have, "llama3:latest"));
        assert!(!model_present(&have, "llama3:8b"));
    }

    #[test]
    fn model_present_exact_tag_match() {
        let have = vec!["gemma4:e4b".to_string()];
        assert!(model_present(&have, "gemma4:e4b"));
        assert!(!model_present(&have, "gemma4"));
    }

    #[test]
    fn classify_unreachable_is_endpoint_down() {
        let h = classify(
            "http://localhost:11434/api/generate",
            "gemma4:e4b",
            Observation::Unreachable,
        );
        assert!(matches!(h, BrainHealth::EndpointDown { .. }));
        assert!(!h.is_ready());
        assert!(h.fix_hint().unwrap().contains("ollama serve"));
    }

    #[test]
    fn classify_endpoint_up_model_missing() {
        let observed = Observation::ModelList(vec!["other:latest".to_string()]);
        let h = classify(
            "http://localhost:11434/api/generate",
            "gemma4:e4b",
            observed,
        );
        match &h {
            BrainHealth::ModelMissing { available, .. } => assert_eq!(available, &["other:latest"]),
            other => panic!("expected ModelMissing, got {other:?}"),
        }
        // The fix hint names the exact pull command for the configured model.
        assert!(h.fix_hint().unwrap().contains("ollama pull gemma4:e4b"));
    }

    #[test]
    fn classify_model_present_is_ready() {
        let observed = Observation::ModelList(vec!["gemma4:e4b".to_string()]);
        let h = classify(
            "http://localhost:11434/api/generate",
            "gemma4:e4b",
            observed,
        );
        assert!(h.is_ready());
        assert!(h.fix_hint().is_none());
    }

    #[test]
    fn classify_openai_reachable_is_ready() {
        let h = classify(
            "http://localhost:8080/v1/chat/completions",
            "gpt",
            Observation::ReachableNoList,
        );
        assert!(h.is_ready());
    }
}
