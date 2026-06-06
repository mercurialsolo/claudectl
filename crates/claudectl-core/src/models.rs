use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ModelProfile {
    pub input_per_m: f64,
    pub output_per_m: f64,
    pub cache_read_per_m: f64,
    pub cache_write_per_m: f64,
    pub context_max: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ModelOverride {
    pub name: String,
    pub profile: ModelProfile,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelProfileSource {
    BuiltIn,
    Override,
    Fallback,
}

impl ModelProfileSource {
    pub fn label(self) -> &'static str {
        match self {
            Self::BuiltIn => "built-in",
            Self::Override => "override",
            Self::Fallback => "fallback",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedModelProfile {
    pub key: String,
    pub profile: ModelProfile,
    pub source: ModelProfileSource,
}

static MODEL_OVERRIDES: OnceLock<Mutex<HashMap<String, ModelProfile>>> = OnceLock::new();

pub fn shorten_model(model: &str) -> String {
    if model.contains("opus") {
        if model.contains("4-6") {
            "opus-4.6".into()
        } else {
            "opus".into()
        }
    } else if model.contains("sonnet") {
        if model.contains("4-6") {
            "sonnet-4.6".into()
        } else {
            "sonnet".into()
        }
    } else if model.contains("haiku") {
        "haiku".into()
    } else {
        model.to_string()
    }
}

pub fn set_overrides(overrides: Vec<ModelOverride>) {
    let store = MODEL_OVERRIDES.get_or_init(|| Mutex::new(HashMap::new()));
    let Ok(mut guard) = store.lock() else {
        return;
    };
    guard.clear();
    for override_ in overrides {
        let raw = override_.name.trim().to_lowercase();
        let shortened = shorten_model(&override_.name).to_lowercase();
        guard.insert(raw, override_.profile);
        guard.insert(shortened, override_.profile);
    }
}

pub fn resolve(model: &str) -> ResolvedModelProfile {
    let empty = HashMap::new();
    let store = MODEL_OVERRIDES.get_or_init(|| Mutex::new(HashMap::new()));
    let guard = store.lock().ok();
    let overrides = guard.as_deref().unwrap_or(&empty);
    resolve_with_overrides(model, overrides)
}

pub(crate) fn resolve_with_overrides(
    model: &str,
    overrides: &HashMap<String, ModelProfile>,
) -> ResolvedModelProfile {
    let raw_key = model.trim().to_lowercase();
    let short_key = shorten_model(model).to_lowercase();

    if let Some(profile) = overrides
        .get(&raw_key)
        .or_else(|| overrides.get(&short_key))
        .copied()
    {
        return ResolvedModelProfile {
            key: if raw_key.is_empty() {
                short_key
            } else {
                raw_key
            },
            profile,
            source: ModelProfileSource::Override,
        };
    }

    if let Some(profile) = built_in_profile(&short_key) {
        return ResolvedModelProfile {
            key: short_key,
            profile,
            source: ModelProfileSource::BuiltIn,
        };
    }

    ResolvedModelProfile {
        key: if short_key.is_empty() {
            "unknown".into()
        } else {
            short_key
        },
        profile: fallback_profile(),
        source: ModelProfileSource::Fallback,
    }
}

fn built_in_profile(key: &str) -> Option<ModelProfile> {
    match key {
        "opus-4.6" | "opus" => Some(ModelProfile {
            input_per_m: 15.0,
            output_per_m: 75.0,
            cache_read_per_m: 1.875,
            cache_write_per_m: 18.75,
            context_max: 1_000_000,
        }),
        "sonnet-4.6" | "sonnet" => Some(ModelProfile {
            input_per_m: 3.0,
            output_per_m: 15.0,
            cache_read_per_m: 0.375,
            cache_write_per_m: 3.75,
            context_max: 200_000,
        }),
        "haiku" => Some(ModelProfile {
            input_per_m: 0.80,
            output_per_m: 4.0,
            cache_read_per_m: 0.10,
            cache_write_per_m: 1.0,
            context_max: 200_000,
        }),
        _ => None,
    }
}

fn fallback_profile() -> ModelProfile {
    ModelProfile {
        input_per_m: 15.0,
        output_per_m: 75.0,
        cache_read_per_m: 1.875,
        cache_write_per_m: 18.75,
        context_max: 200_000,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_builtin_profile() {
        let resolved = resolve_with_overrides("claude-opus-4-6-20260401", &HashMap::new());
        assert_eq!(resolved.source, ModelProfileSource::BuiltIn);
        assert_eq!(resolved.profile.context_max, 1_000_000);
    }

    #[test]
    fn resolve_override_profile() {
        let mut overrides = HashMap::new();
        overrides.insert(
            "gpt-4o".into(),
            ModelProfile {
                input_per_m: 1.0,
                output_per_m: 2.0,
                cache_read_per_m: 0.5,
                cache_write_per_m: 1.5,
                context_max: 128_000,
            },
        );
        let resolved = resolve_with_overrides("gpt-4o", &overrides);
        assert_eq!(resolved.source, ModelProfileSource::Override);
        assert_eq!(resolved.profile.context_max, 128_000);
    }

    #[test]
    fn resolve_fallback_profile() {
        let resolved = resolve_with_overrides("mystery-model", &HashMap::new());
        assert_eq!(resolved.source, ModelProfileSource::Fallback);
        assert_eq!(resolved.profile.context_max, 200_000);
    }
}
