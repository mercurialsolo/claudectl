use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ModelProfile {
    pub input_per_m: f64,
    pub output_per_m: f64,
    /// Price of a cache *hit*. Tabled rather than derived because the
    /// multiplier is per-model: 0.1x of base input for most models, but 0.05x
    /// on Opus 5.5 / Sonnet 5.5 and 0.025x on Fable 5.1.
    pub cache_read_per_m: f64,
    /// Price of a **5-minute** cache write, the documented default. The
    /// 1-hour rate is derived — see [`ModelProfile::cache_write_1h_per_m`].
    pub cache_write_per_m: f64,
    pub context_max: u64,
}

impl ModelProfile {
    /// Price of a **1-hour** cache write.
    ///
    /// Unlike the read multiplier this one is universal — "1-hour cache write
    /// tokens are 2 times the base input tokens price" — so it is computed
    /// instead of occupying a column that would have to be kept in step with
    /// `input_per_m` in every row.
    pub fn cache_write_1h_per_m(&self) -> f64 {
        self.input_per_m * 2.0
    }
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

/// Reduce an API model id to a pricing key, e.g. `claude-opus-5` ->
/// `opus-5`, `claude-haiku-4-5-20251001` -> `haiku-4.5`.
///
/// This used to collapse anything it did not recognise to the bare family
/// name, so `claude-opus-5` became `opus`, matched the `"opus"` arm of the
/// price table, and was billed at retired **Opus 4.1** rates — three times
/// its real price. A key with no version therefore no longer matches any
/// built-in profile; it falls through to [`fallback_profile`] and is reported
/// as unverified, because guessing a sibling's rates is the whole bug.
pub fn shorten_model(model: &str) -> String {
    let lower = model.trim().to_lowercase();
    let family = ["opus", "sonnet", "haiku", "fable", "mythos"]
        .into_iter()
        .find(|f| lower.contains(f));
    let Some(family) = family else {
        return model.to_string();
    };

    // The version is the digit groups after the family name, stopping at the
    // release date: `opus-4-6-20260401` -> 4.6, `opus-5` -> 5, `fable-5-1` ->
    // 5.1. An already-shortened key (`opus-4.6`) carries its version dotted,
    // and has to round-trip, because callers pass pricing keys back in.
    let tail = &lower[lower.find(family).unwrap_or(0) + family.len()..];
    let mut parts: Vec<String> = Vec::new();
    for seg in tail.split('-') {
        if seg.is_empty() {
            continue;
        }
        // An 8-digit group is a release date, which ends the version.
        if seg.len() >= 8 && seg.chars().all(|c| c.is_ascii_digit()) {
            break;
        }
        if seg.contains('.') {
            if seg.chars().all(|c| c.is_ascii_digit() || c == '.') {
                parts = seg
                    .split('.')
                    .filter(|x| !x.is_empty())
                    .map(String::from)
                    .collect();
            }
            break;
        }
        if !seg.chars().all(|c| c.is_ascii_digit()) {
            break;
        }
        parts.push(seg.to_string());
        if parts.len() == 2 {
            break;
        }
    }

    if parts.is_empty() {
        family.to_string()
    } else {
        format!("{family}-{}", parts.join("."))
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

/// Prices per million tokens, from
/// <https://platform.claude.com/docs/en/about-claude/pricing> as of 2026-10-08.
///
/// `cache_write_per_m` is the **5-minute** rate (1.25x base input); the 1-hour
/// rate is derived. Retired models are kept because a transcript recorded
/// before their retirement still has to be priced as it was billed.
///
/// Haiku 5.5 is deliberately absent: it is priced by prompt length, which this
/// one-row-per-model shape cannot express, so it resolves as unverified rather
/// than wrong.
fn built_in_profile(key: &str) -> Option<ModelProfile> {
    let p = |input_per_m, output_per_m, cache_read_per_m, cache_write_per_m, context_max| {
        Some(ModelProfile {
            input_per_m,
            output_per_m,
            cache_read_per_m,
            cache_write_per_m,
            context_max,
        })
    };
    match key {
        // Claude 4.6 and later carry the full 1M window at standard pricing.
        "fable-5.1" | "mythos-5.1" => p(10.0, 50.0, 0.25, 12.50, 1_000_000),
        "fable-5" | "mythos-5" => p(10.0, 50.0, 1.00, 12.50, 1_000_000),
        "opus-5.5" => p(4.0, 20.0, 0.20, 5.00, 1_000_000),
        "opus-5" | "opus-4.8" | "opus-4.7" | "opus-4.6" => p(5.0, 25.0, 0.50, 6.25, 1_000_000),
        "opus-4.5" => p(5.0, 25.0, 0.50, 6.25, 200_000),
        // Retired, but still the correct price for an older transcript.
        "opus-4.1" | "opus-4" => p(15.0, 75.0, 1.50, 18.75, 200_000),
        "sonnet-5.5" => p(2.0, 10.0, 0.10, 2.50, 1_000_000),
        "sonnet-5" => p(2.0, 10.0, 0.20, 2.50, 1_000_000),
        "sonnet-4.6" => p(3.0, 15.0, 0.30, 3.75, 1_000_000),
        "sonnet-4.5" | "sonnet-4" => p(3.0, 15.0, 0.30, 3.75, 200_000),
        "haiku-4.5" => p(1.0, 5.0, 0.10, 1.25, 200_000),
        "haiku-3.5" => p(0.80, 4.0, 0.08, 1.00, 200_000),
        _ => None,
    }
}

/// Used when the model id matches no row above. Reported as
/// `ModelProfileSource::Fallback`, which surfaces as `verified: false`.
///
/// Set to current Opus 5 rates rather than the retired Opus 4.1 rates it used
/// to carry: an unknown *Claude* id is far likelier to be a current model than
/// a 2025 one, and the old value overstated an unknown model's cost threefold.
fn fallback_profile() -> ModelProfile {
    ModelProfile {
        input_per_m: 5.0,
        output_per_m: 25.0,
        cache_read_per_m: 0.50,
        cache_write_per_m: 6.25,
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
