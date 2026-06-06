use std::io;
use std::path::{Path, PathBuf};

/// The hooks we install into Claude Code's settings.json.
///
/// We use `PostToolUse` with a wildcard matcher so claudectl sees every tool
/// completion, and `Stop` to catch session endings. The commands call
/// `claudectl --json` which is a lightweight, non-TUI snapshot that the
/// brain / hooks system can consume.
///
/// We also wire up `PreToolUse` for Bash commands so claudectl's rule engine
/// can evaluate deny rules before execution.
struct HookSpec {
    event: &'static str,
    matcher: &'static str,
    command: &'static str,
    timeout: u32,
}

const HOOKS: &[HookSpec] = &[
    HookSpec {
        event: "PreToolUse",
        matcher: "Bash",
        command: "claudectl --json 2>/dev/null || true",
        timeout: 5,
    },
    HookSpec {
        event: "PostToolUse",
        matcher: "*",
        command: "claudectl --json 2>/dev/null || true",
        timeout: 5,
    },
    HookSpec {
        event: "Stop",
        matcher: "",
        command: "claudectl --json 2>/dev/null || true",
        timeout: 5,
    },
];

fn settings_path(project: bool) -> PathBuf {
    if project {
        PathBuf::from(".claude/settings.local.json")
    } else {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/tmp"));
        home.join(".claude/settings.json")
    }
}

/// Convenience for the init wizard's state-detection path. Returns the
/// global (user-scope) settings file location.
pub fn user_settings_path() -> PathBuf {
    settings_path(false)
}

/// Probe a settings.json on disk for claudectl-managed hooks. Returns
/// `Some(true)` when present, `Some(false)` when absent, and `None` when the
/// file doesn't exist or can't be parsed. Used by `init::state` to keep
/// detection consistent with what `run_init` writes.
pub fn settings_contain_claudectl_hooks(path: &Path) -> Option<bool> {
    let raw = std::fs::read_to_string(path).ok()?;
    let value: serde_json::Value = serde_json::from_str(&raw).ok()?;
    Some(has_claudectl_hooks(&value))
}

fn build_hooks_value() -> serde_json::Value {
    let mut hooks_map = serde_json::Map::new();

    for spec in HOOKS {
        let hook_entry = serde_json::json!({
            "type": "command",
            "command": spec.command,
            "timeout": spec.timeout,
        });

        let matcher_entry = serde_json::json!({
            "matcher": spec.matcher,
            "hooks": [hook_entry],
        });

        let array = hooks_map
            .entry(spec.event)
            .or_insert_with(|| serde_json::Value::Array(Vec::new()));
        if let serde_json::Value::Array(arr) = array {
            arr.push(matcher_entry);
        }
    }

    serde_json::Value::Object(hooks_map)
}

/// Check if claudectl hooks are already present in existing settings.
fn has_claudectl_hooks(existing: &serde_json::Value) -> bool {
    if let Some(hooks) = existing.get("hooks") {
        if let Some(obj) = hooks.as_object() {
            for (_event, matchers) in obj {
                if let Some(arr) = matchers.as_array() {
                    for matcher_entry in arr {
                        if let Some(inner_hooks) = matcher_entry.get("hooks") {
                            if let Some(inner_arr) = inner_hooks.as_array() {
                                for hook in inner_arr {
                                    if let Some(cmd) = hook.get("command") {
                                        if let Some(s) = cmd.as_str() {
                                            if s.contains("claudectl") {
                                                return true;
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    false
}

/// Merge claudectl hooks into existing settings, preserving all other keys
/// and any non-claudectl hooks already defined.
fn merge_hooks(existing: &mut serde_json::Value) {
    let new_hooks = build_hooks_value();

    let hooks_obj = existing
        .as_object_mut()
        .expect("settings must be an object")
        .entry("hooks")
        .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));

    if let (Some(target), Some(source)) = (hooks_obj.as_object_mut(), new_hooks.as_object()) {
        for (event, new_matchers) in source {
            let event_arr = target
                .entry(event)
                .or_insert_with(|| serde_json::Value::Array(Vec::new()));
            if let (Some(arr), Some(new_arr)) = (event_arr.as_array_mut(), new_matchers.as_array())
            {
                for new_matcher in new_arr {
                    arr.push(new_matcher.clone());
                }
            }
        }
    }
}

/// Remove claudectl hooks from a matcher entry's inner hooks array.
/// Returns true if any hooks remain after filtering.
fn filter_claudectl_hooks(matcher_entry: &mut serde_json::Value) -> bool {
    if let Some(inner_hooks) = matcher_entry.get_mut("hooks") {
        if let Some(arr) = inner_hooks.as_array_mut() {
            arr.retain(|hook| {
                hook.get("command")
                    .and_then(|c| c.as_str())
                    .is_none_or(|s| !s.contains("claudectl"))
            });
            return !arr.is_empty();
        }
    }
    true
}

/// Remove all claudectl hook entries from settings, preserving everything else.
/// Returns the number of hook entries removed.
fn remove_claudectl_hooks(settings: &mut serde_json::Value) -> usize {
    let mut removed = 0;

    let Some(hooks) = settings.get_mut("hooks") else {
        return 0;
    };
    let Some(hooks_obj) = hooks.as_object_mut() else {
        return 0;
    };

    // For each event, filter out matcher entries that contain claudectl commands
    let mut empty_events = Vec::new();
    for (event, matchers) in hooks_obj.iter_mut() {
        if let Some(arr) = matchers.as_array_mut() {
            let before = arr.len();
            arr.retain_mut(filter_claudectl_hooks);
            removed += before - arr.len();
            if arr.is_empty() {
                empty_events.push(event.clone());
            }
        }
    }

    // Remove event keys that are now empty
    for event in empty_events {
        hooks_obj.remove(&event);
    }

    // Remove the hooks key entirely if it's now empty
    if hooks_obj.is_empty() {
        if let Some(obj) = settings.as_object_mut() {
            obj.remove("hooks");
        }
    }

    removed
}

/// Run the uninit command: remove claudectl hooks from settings.json.
pub fn run_uninit(project: bool) -> io::Result<()> {
    let path = settings_path(project);

    if !path.exists() {
        println!(
            "No settings file at {} — nothing to remove.",
            path.display()
        );
        return Ok(());
    }

    let content = std::fs::read_to_string(&path)?;
    let mut settings = match serde_json::from_str::<serde_json::Value>(&content) {
        Ok(v) if v.is_object() => v,
        _ => {
            eprintln!(
                "Error: {} is not valid JSON — refusing to modify.",
                path.display()
            );
            std::process::exit(1);
        }
    };

    if !has_claudectl_hooks(&settings) {
        println!(
            "No claudectl hooks found in {} — nothing to remove.",
            path.display()
        );
        return Ok(());
    }

    let removed = remove_claudectl_hooks(&mut settings);

    // If the settings object is now empty (only had hooks), remove the file
    let is_empty = settings.as_object().is_some_and(|obj| obj.is_empty());

    if is_empty {
        std::fs::remove_file(&path)?;
        println!(
            "Removed {removed} claudectl hook(s) — {} was empty and has been deleted.",
            path.display()
        );
    } else {
        let json = serde_json::to_string_pretty(&settings)
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
        std::fs::write(&path, format!("{json}\n"))?;
        println!(
            "Removed {removed} claudectl hook(s) from {}",
            path.display()
        );
    }

    Ok(())
}

/// Run the init command: write Claude Code hooks into settings.json.
pub fn run_init(project: bool, dry_run: bool) -> io::Result<()> {
    let path = settings_path(project);

    // Read existing settings or start fresh
    let mut settings = if path.exists() {
        let content = std::fs::read_to_string(&path)?;
        match serde_json::from_str::<serde_json::Value>(&content) {
            Ok(v) if v.is_object() => v,
            Ok(_) => {
                eprintln!(
                    "Error: {} exists but is not a JSON object — refusing to overwrite.",
                    path.display()
                );
                std::process::exit(1);
            }
            Err(e) => {
                eprintln!(
                    "Error: {} contains invalid JSON: {} — refusing to overwrite.",
                    path.display(),
                    e
                );
                std::process::exit(1);
            }
        }
    } else {
        serde_json::json!({})
    };

    // Check for existing claudectl hooks
    if has_claudectl_hooks(&settings) {
        println!("claudectl hooks already configured in {}", path.display());
        println!("To re-initialize, run `claudectl init --remove` first.");
        return Ok(());
    }

    // Merge hooks into settings
    merge_hooks(&mut settings);

    if dry_run {
        // Show what would be written without actually writing
        let json = serde_json::to_string_pretty(&settings)
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
        println!("Would write to {}:", path.display());
        println!();
        println!("{json}");
        return Ok(());
    }

    // Ensure parent directory exists
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    // Write
    let json = serde_json::to_string_pretty(&settings)
        .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
    std::fs::write(&path, format!("{json}\n"))?;

    print_success(&path);

    Ok(())
}

fn print_success(path: &Path) {
    println!("Initialized claudectl hooks in {}", path.display());
    println!();
    println!("Hooks installed:");
    println!("  PreToolUse (Bash)  — lets claudectl observe commands before execution");
    println!("  PostToolUse (*)    — notifies claudectl after every tool completion");
    println!("  Stop               — notifies claudectl when a session ends");
    println!();
    println!("Claude Code will now notify claudectl on each tool use.");
    println!("Run `claudectl` to start the dashboard.");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_hooks_value() {
        let hooks = build_hooks_value();
        let obj = hooks.as_object().unwrap();

        // Should have entries for PreToolUse, PostToolUse, and Stop
        assert!(obj.contains_key("PreToolUse"));
        assert!(obj.contains_key("PostToolUse"));
        assert!(obj.contains_key("Stop"));

        // Each event should have an array of matcher entries
        for (_event, matchers) in obj {
            let arr = matchers.as_array().unwrap();
            assert!(!arr.is_empty());
            for entry in arr {
                assert!(entry.get("matcher").is_some());
                assert!(entry.get("hooks").is_some());
                let inner = entry["hooks"].as_array().unwrap();
                assert_eq!(inner[0]["type"], "command");
                assert!(inner[0]["command"].as_str().unwrap().contains("claudectl"));
            }
        }
    }

    #[test]
    fn test_has_claudectl_hooks_empty() {
        let settings = serde_json::json!({});
        assert!(!has_claudectl_hooks(&settings));
    }

    #[test]
    fn test_has_claudectl_hooks_present() {
        let settings = serde_json::json!({
            "hooks": {
                "PostToolUse": [{
                    "matcher": "*",
                    "hooks": [{
                        "type": "command",
                        "command": "claudectl --json 2>/dev/null || true",
                        "timeout": 5
                    }]
                }]
            }
        });
        assert!(has_claudectl_hooks(&settings));
    }

    #[test]
    fn test_has_claudectl_hooks_other_hooks_only() {
        let settings = serde_json::json!({
            "hooks": {
                "PreToolUse": [{
                    "matcher": "Bash",
                    "hooks": [{
                        "type": "command",
                        "command": "echo hello",
                        "timeout": 5
                    }]
                }]
            }
        });
        assert!(!has_claudectl_hooks(&settings));
    }

    #[test]
    fn test_merge_hooks_empty() {
        let mut settings = serde_json::json!({});
        merge_hooks(&mut settings);

        assert!(settings.get("hooks").is_some());
        let hooks = settings["hooks"].as_object().unwrap();
        assert!(hooks.contains_key("PreToolUse"));
        assert!(hooks.contains_key("PostToolUse"));
        assert!(hooks.contains_key("Stop"));
    }

    #[test]
    fn test_merge_hooks_preserves_existing() {
        let mut settings = serde_json::json!({
            "allowedTools": ["Bash", "Read"],
            "hooks": {
                "PreToolUse": [{
                    "matcher": "Write",
                    "hooks": [{
                        "type": "command",
                        "command": "echo validate-write",
                        "timeout": 10
                    }]
                }]
            }
        });

        merge_hooks(&mut settings);

        // Existing allowedTools preserved
        assert_eq!(
            settings["allowedTools"],
            serde_json::json!(["Bash", "Read"])
        );

        // Existing PreToolUse Write hook preserved
        let pre = settings["hooks"]["PreToolUse"].as_array().unwrap();
        assert_eq!(pre.len(), 2); // original Write + new Bash
        assert_eq!(pre[0]["matcher"], "Write");
        assert_eq!(pre[1]["matcher"], "Bash");

        // New hooks added
        assert!(settings["hooks"]["PostToolUse"].is_array());
        assert!(settings["hooks"]["Stop"].is_array());
    }

    #[test]
    fn test_run_init_creates_file() {
        let dir = tempfile::tempdir().unwrap();
        let settings_file = dir.path().join(".claude/settings.local.json");

        // Temporarily override HOME so settings_path uses our temp dir
        // We test the file-writing logic directly instead
        let parent = settings_file.parent().unwrap();
        std::fs::create_dir_all(parent).unwrap();

        let mut settings = serde_json::json!({});
        merge_hooks(&mut settings);

        let json = serde_json::to_string_pretty(&settings).unwrap();
        std::fs::write(&settings_file, format!("{json}\n")).unwrap();

        // Verify the file was created and is valid JSON
        let content = std::fs::read_to_string(&settings_file).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert!(parsed.get("hooks").is_some());
        assert!(has_claudectl_hooks(&parsed));
    }

    #[test]
    fn test_settings_path_global() {
        let path = settings_path(false);
        let path_str = path.to_string_lossy();
        assert!(path_str.ends_with(".claude/settings.json"));
    }

    #[test]
    fn test_settings_path_project() {
        let path = settings_path(true);
        assert_eq!(path, PathBuf::from(".claude/settings.local.json"));
    }

    #[test]
    fn test_remove_claudectl_hooks_all() {
        let mut settings = serde_json::json!({});
        merge_hooks(&mut settings);
        assert!(has_claudectl_hooks(&settings));

        let removed = remove_claudectl_hooks(&mut settings);
        assert_eq!(removed, 3); // PreToolUse, PostToolUse, Stop
        assert!(!has_claudectl_hooks(&settings));
        // hooks key removed entirely when empty
        assert!(settings.get("hooks").is_none());
    }

    #[test]
    fn test_remove_claudectl_hooks_preserves_others() {
        let mut settings = serde_json::json!({
            "allowedTools": ["Bash"],
            "hooks": {
                "PreToolUse": [
                    {
                        "matcher": "Write",
                        "hooks": [{
                            "type": "command",
                            "command": "echo validate-write",
                            "timeout": 10
                        }]
                    },
                    {
                        "matcher": "Bash",
                        "hooks": [{
                            "type": "command",
                            "command": "claudectl --json 2>/dev/null || true",
                            "timeout": 5
                        }]
                    }
                ],
                "PostToolUse": [{
                    "matcher": "*",
                    "hooks": [{
                        "type": "command",
                        "command": "claudectl --json 2>/dev/null || true",
                        "timeout": 5
                    }]
                }]
            }
        });

        let removed = remove_claudectl_hooks(&mut settings);
        assert_eq!(removed, 2); // Bash from PreToolUse + PostToolUse entry

        // Write hook in PreToolUse preserved
        let pre = settings["hooks"]["PreToolUse"].as_array().unwrap();
        assert_eq!(pre.len(), 1);
        assert_eq!(pre[0]["matcher"], "Write");

        // PostToolUse event removed entirely (was only claudectl)
        assert!(settings["hooks"].get("PostToolUse").is_none());

        // allowedTools untouched
        assert_eq!(settings["allowedTools"], serde_json::json!(["Bash"]));
    }

    #[test]
    fn test_remove_claudectl_hooks_noop_when_absent() {
        let mut settings = serde_json::json!({
            "hooks": {
                "PreToolUse": [{
                    "matcher": "Bash",
                    "hooks": [{
                        "type": "command",
                        "command": "echo hello",
                        "timeout": 5
                    }]
                }]
            }
        });

        let removed = remove_claudectl_hooks(&mut settings);
        assert_eq!(removed, 0);
        // Original hook still present
        assert!(settings["hooks"]["PreToolUse"].as_array().unwrap().len() == 1);
    }

    #[test]
    fn test_remove_then_no_hooks_key() {
        // Settings that only had claudectl hooks — hooks key should be removed entirely
        let mut settings = serde_json::json!({ "permissions": {} });
        merge_hooks(&mut settings);
        remove_claudectl_hooks(&mut settings);

        assert!(settings.get("hooks").is_none());
        // Other keys preserved
        assert!(settings.get("permissions").is_some());
    }

    #[test]
    fn test_init_uninit_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let settings_file = dir.path().join("settings.json");

        // Start with existing settings
        let original = serde_json::json!({
            "allowedTools": ["Read", "Glob"],
            "hooks": {
                "SessionStart": [{
                    "matcher": "*",
                    "hooks": [{
                        "type": "command",
                        "command": "echo started",
                        "timeout": 5
                    }]
                }]
            }
        });
        let json = serde_json::to_string_pretty(&original).unwrap();
        std::fs::write(&settings_file, &json).unwrap();

        // Init: merge claudectl hooks in
        let content = std::fs::read_to_string(&settings_file).unwrap();
        let mut settings: serde_json::Value = serde_json::from_str(&content).unwrap();
        merge_hooks(&mut settings);
        let json = serde_json::to_string_pretty(&settings).unwrap();
        std::fs::write(&settings_file, &json).unwrap();
        assert!(has_claudectl_hooks(&settings));

        // Uninit: remove claudectl hooks
        let content = std::fs::read_to_string(&settings_file).unwrap();
        let mut settings: serde_json::Value = serde_json::from_str(&content).unwrap();
        remove_claudectl_hooks(&mut settings);

        // Back to original state
        assert!(!has_claudectl_hooks(&settings));
        assert_eq!(
            settings["allowedTools"],
            serde_json::json!(["Read", "Glob"])
        );
        let session_start = settings["hooks"]["SessionStart"].as_array().unwrap();
        assert_eq!(session_start.len(), 1);
        assert_eq!(session_start[0]["hooks"][0]["command"], "echo started");
    }
}
