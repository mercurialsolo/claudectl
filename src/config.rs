use std::fs;
use std::path::PathBuf;

/// Configuration loaded from TOML files, merged with CLI flags.
/// Priority: CLI flags > project config > global config > defaults.
#[derive(Debug, Clone)]
pub struct Config {
    pub interval: u64,
    pub notify: bool,
    pub debug: bool,
    pub grouped: bool,
    pub sort: Option<String>,
    pub budget: Option<f64>,
    pub kill_on_budget: bool,
    pub webhook: Option<String>,
    pub webhook_on: Option<Vec<String>>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            interval: 2000,
            notify: false,
            debug: false,
            grouped: false,
            sort: None,
            budget: None,
            kill_on_budget: false,
            webhook: None,
            webhook_on: None,
        }
    }
}

/// Raw TOML representation — all fields optional for partial overrides.
#[derive(Debug, Default)]
struct RawConfig {
    interval: Option<u64>,
    notify: Option<bool>,
    debug: Option<bool>,
    grouped: Option<bool>,
    sort: Option<String>,
    budget: Option<f64>,
    kill_on_budget: Option<bool>,
    webhook_url: Option<String>,
    webhook_events: Option<Vec<String>>,
}

impl Config {
    /// Load configuration from global and project config files.
    pub fn load() -> Self {
        let mut config = Config::default();

        // Layer 1: Global config
        if let Some(global) = global_config_path() {
            if let Some(raw) = parse_config_file(&global) {
                config.apply(raw);
            }
        }

        // Layer 2: Project config (.claudectl.toml in cwd)
        if let Some(raw) = parse_config_file(&PathBuf::from(".claudectl.toml")) {
            config.apply(raw);
        }

        config
    }

    /// Apply a raw config layer on top, overriding only set fields.
    fn apply(&mut self, raw: RawConfig) {
        if let Some(v) = raw.interval {
            self.interval = v;
        }
        if let Some(v) = raw.notify {
            self.notify = v;
        }
        if let Some(v) = raw.debug {
            self.debug = v;
        }
        if let Some(v) = raw.grouped {
            self.grouped = v;
        }
        if let Some(v) = raw.sort {
            self.sort = Some(v);
        }
        if let Some(v) = raw.budget {
            self.budget = Some(v);
        }
        if let Some(v) = raw.kill_on_budget {
            self.kill_on_budget = v;
        }
        if let Some(v) = raw.webhook_url {
            self.webhook = Some(v);
        }
        if let Some(v) = raw.webhook_events {
            self.webhook_on = Some(v);
        }
    }

    /// Show resolved config and file locations (for `claudectl config`).
    pub fn print_resolved(&self) {
        println!("Resolved configuration:");
        println!();

        if let Some(p) = global_config_path() {
            if p.exists() {
                println!("  Global config: {}", p.display());
            } else {
                println!("  Global config: {} (not found)", p.display());
            }
        }

        let project_path = PathBuf::from(".claudectl.toml");
        if project_path.exists() {
            println!("  Project config: {}", project_path.display());
        } else {
            println!("  Project config: .claudectl.toml (not found)");
        }

        println!();
        println!("  interval:       {}ms", self.interval);
        println!("  notify:         {}", self.notify);
        println!("  debug:          {}", self.debug);
        println!("  grouped:        {}", self.grouped);
        println!(
            "  sort:           {}",
            self.sort.as_deref().unwrap_or("default")
        );
        println!(
            "  budget:         {}",
            self.budget
                .map(|b| format!("${b:.2}"))
                .unwrap_or_else(|| "none".into())
        );
        println!("  kill_on_budget: {}", self.kill_on_budget);
        println!(
            "  webhook:        {}",
            self.webhook.as_deref().unwrap_or("none")
        );
        println!(
            "  webhook_on:     {}",
            self.webhook_on
                .as_ref()
                .map(|v| v.join(", "))
                .unwrap_or_else(|| "all".into())
        );
    }
}

fn global_config_path() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|home| {
        PathBuf::from(home)
            .join(".config")
            .join("claudectl")
            .join("config.toml")
    })
}

/// Minimal TOML parser — avoids adding a toml crate dependency.
/// Supports: key = value pairs, [sections], # comments, strings, numbers, booleans, arrays.
fn parse_config_file(path: &PathBuf) -> Option<RawConfig> {
    let content = fs::read_to_string(path).ok()?;
    let mut raw = RawConfig::default();
    let mut section = String::new();

    for line in content.lines() {
        let line = line.trim();

        // Skip empty lines and comments
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        // Section headers
        if line.starts_with('[') && line.ends_with(']') {
            section = line[1..line.len() - 1].trim().to_string();
            continue;
        }

        // Key = value
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        let value = value.trim();

        // Strip inline comments
        let value = value.split('#').next().unwrap_or(value).trim();

        match (section.as_str(), key) {
            ("" | "defaults", "interval") => {
                raw.interval = value.parse().ok();
            }
            ("" | "defaults", "notify") => {
                raw.notify = parse_bool(value);
            }
            ("" | "defaults", "debug") => {
                raw.debug = parse_bool(value);
            }
            ("" | "defaults", "grouped") => {
                raw.grouped = parse_bool(value);
            }
            ("" | "defaults", "sort") => {
                raw.sort = Some(unquote(value));
            }
            ("" | "defaults", "budget") => {
                raw.budget = value.parse().ok();
            }
            ("" | "defaults", "kill_on_budget") => {
                raw.kill_on_budget = parse_bool(value);
            }
            ("webhook", "url") => {
                raw.webhook_url = Some(unquote(value));
            }
            ("webhook", "events") => {
                raw.webhook_events = Some(parse_string_array(value));
            }
            _ => {} // Ignore unknown keys
        }
    }

    Some(raw)
}

/// Load hooks from global and project config files.
pub fn load_hooks() -> crate::hooks::HookRegistry {
    let mut registry = crate::hooks::HookRegistry::new();

    if let Some(global) = global_config_path() {
        parse_hooks_from_file(&global, &mut registry);
    }
    parse_hooks_from_file(&PathBuf::from(".claudectl.toml"), &mut registry);

    registry
}

fn parse_hooks_from_file(path: &PathBuf, registry: &mut crate::hooks::HookRegistry) {
    let content = match fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => return,
    };

    let mut section = String::new();

    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        if line.starts_with('[') && line.ends_with(']') {
            section = line[1..line.len() - 1].trim().to_string();
            continue;
        }

        // Only process hooks sections
        if !section.starts_with("hooks.") {
            continue;
        }

        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        let value = value.trim();
        let value = value.split('#').next().unwrap_or(value).trim();

        if key == "run" {
            if let Some(event) = crate::hooks::HookEvent::from_section(&section) {
                registry.add(event, unquote(value));
            }
        }
    }
}

fn parse_bool(s: &str) -> Option<bool> {
    match s {
        "true" => Some(true),
        "false" => Some(false),
        _ => None,
    }
}

fn unquote(s: &str) -> String {
    s.trim_matches('"').trim_matches('\'').to_string()
}

fn parse_string_array(s: &str) -> Vec<String> {
    let s = s.trim_start_matches('[').trim_end_matches(']');
    s.split(',')
        .map(|item| unquote(item.trim()))
        .filter(|item| !item.is_empty())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_bool() {
        assert_eq!(parse_bool("true"), Some(true));
        assert_eq!(parse_bool("false"), Some(false));
        assert_eq!(parse_bool("yes"), None);
    }

    #[test]
    fn test_unquote() {
        assert_eq!(unquote("\"hello\""), "hello");
        assert_eq!(unquote("'hello'"), "hello");
        assert_eq!(unquote("hello"), "hello");
    }

    #[test]
    fn test_parse_string_array() {
        let result = parse_string_array("[\"NeedsInput\", \"Finished\"]");
        assert_eq!(result, vec!["NeedsInput", "Finished"]);
    }

    #[test]
    fn test_parse_config_file() {
        use std::io::Write;
        let mut file = tempfile::NamedTempFile::new().unwrap();
        writeln!(
            file,
            r#"
# Global claudectl config
[defaults]
interval = 1000
notify = true
grouped = true
sort = "cost"
budget = 5.00
kill_on_budget = false

[webhook]
url = "https://hooks.slack.com/test"
events = ["NeedsInput", "Finished"]
"#
        )
        .unwrap();
        file.flush().unwrap();

        let raw = parse_config_file(&file.path().to_path_buf()).unwrap();
        assert_eq!(raw.interval, Some(1000));
        assert_eq!(raw.notify, Some(true));
        assert_eq!(raw.grouped, Some(true));
        assert_eq!(raw.sort, Some("cost".into()));
        assert_eq!(raw.budget, Some(5.0));
        assert_eq!(raw.kill_on_budget, Some(false));
        assert_eq!(raw.webhook_url, Some("https://hooks.slack.com/test".into()));
        assert_eq!(
            raw.webhook_events,
            Some(vec!["NeedsInput".into(), "Finished".into()])
        );
    }

    #[test]
    fn test_config_layering() {
        let mut config = Config::default();
        assert_eq!(config.interval, 2000);
        assert!(!config.notify);

        // Apply global config
        config.apply(RawConfig {
            interval: Some(1000),
            notify: Some(true),
            budget: Some(5.0),
            ..RawConfig::default()
        });
        assert_eq!(config.interval, 1000);
        assert!(config.notify);
        assert_eq!(config.budget, Some(5.0));

        // Apply project config — overrides some fields
        config.apply(RawConfig {
            budget: Some(10.0),
            grouped: Some(true),
            ..RawConfig::default()
        });
        assert_eq!(config.interval, 1000); // Unchanged
        assert!(config.notify); // Unchanged
        assert_eq!(config.budget, Some(10.0)); // Overridden
        assert!(config.grouped); // New
    }
}
