use std::fs;
use std::path::PathBuf;

use crate::brain::agents::AgentConfig;
use crate::models::{ModelOverride, ModelProfile};
use crate::rules::{AutoRule, RuleAction};

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
    pub daily_limit: Option<f64>,
    pub weekly_limit: Option<f64>,
    pub context_warn_threshold: u8, // 0-100, fires on_context_high when context % crosses this
    pub model_overrides: Vec<ModelOverride>,
    pub rules: Vec<AutoRule>,
    pub health: HealthThresholds,
    pub file_conflicts: bool, // Detect file-level conflicts across sessions
    pub auto_deny_file_conflicts: bool, // Auto-deny writes to conflicting files
    pub brain: Option<BrainConfig>,
    pub agents: Vec<AgentConfig>,
}

/// Configurable thresholds for session health checks.
/// All thresholds have sensible defaults; users only need to override what they want.
#[derive(Debug, Clone)]
pub struct HealthThresholds {
    pub cache_critical_pct: f64, // Cache hit ratio below this = critical (default 10%)
    pub cache_warning_pct: f64,  // Cache hit ratio below this = warning (default 30%)
    pub cache_min_tokens: u64,   // Ignore cache check until this many input tokens (default 10k)
    pub cost_spike_critical: f64, // Burn rate > Nx average = critical (default 5.0)
    pub cost_spike_warning: f64, // Burn rate > Nx average = warning (default 2.5)
    pub loop_max_calls: u32,     // Tool calls with errors to trigger loop warning (default 10)
    pub stall_min_cost: f64,     // Min cost in USD to trigger stall check (default 5.0)
    pub stall_min_minutes: u64,  // Min minutes with no edits to trigger stall (default 10)
    pub context_critical_pct: f64, // Context usage above this = critical (default 90%)
    pub context_warning_pct: f64, // Context usage above this = warning (default 80%)
}

impl Default for HealthThresholds {
    fn default() -> Self {
        Self {
            cache_critical_pct: 10.0,
            cache_warning_pct: 30.0,
            cache_min_tokens: 10_000,
            cost_spike_critical: 5.0,
            cost_spike_warning: 2.5,
            loop_max_calls: 10,
            stall_min_cost: 5.0,
            stall_min_minutes: 10,
            context_critical_pct: 90.0,
            context_warning_pct: 80.0,
        }
    }
}

/// Raw TOML representation for health thresholds — all fields optional.
#[derive(Debug, Default)]
struct RawHealthThresholds {
    cache_critical_pct: Option<f64>,
    cache_warning_pct: Option<f64>,
    cache_min_tokens: Option<u64>,
    cost_spike_critical: Option<f64>,
    cost_spike_warning: Option<f64>,
    loop_max_calls: Option<u32>,
    stall_min_cost: Option<f64>,
    stall_min_minutes: Option<u64>,
    context_critical_pct: Option<f64>,
    context_warning_pct: Option<f64>,
}

/// Configuration for the optional local LLM brain.
/// When `None`, brain is completely disabled with zero overhead.
#[derive(Debug, Clone)]
pub struct BrainConfig {
    pub enabled: bool,
    pub endpoint: String,
    pub model: String,
    pub auto_mode: bool,
    pub timeout_ms: u64,
    pub max_context_tokens: u32,
    pub few_shot_count: usize,
    pub max_sessions: usize,
    pub orchestrate: bool,
    pub orchestrate_interval_secs: u64,
}

impl Default for BrainConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            endpoint: "http://localhost:11434/api/generate".into(),
            model: "gemma4:e4b".into(),
            auto_mode: false,
            timeout_ms: 5000,
            max_context_tokens: 4000,
            few_shot_count: 5,
            max_sessions: 10,
            orchestrate: false,
            orchestrate_interval_secs: 30,
        }
    }
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
            daily_limit: None,
            weekly_limit: None,
            context_warn_threshold: 75,
            model_overrides: Vec::new(),
            rules: Vec::new(),
            health: HealthThresholds::default(),
            file_conflicts: true,
            auto_deny_file_conflicts: false,
            brain: None,
            agents: Vec::new(),
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
    daily_limit: Option<f64>,
    weekly_limit: Option<f64>,
    context_warn_threshold: Option<u8>,
    model_overrides: Vec<ModelOverride>,
    rules: Vec<AutoRule>,
    health: Option<RawHealthThresholds>,
    file_conflicts: Option<bool>,
    auto_deny_file_conflicts: Option<bool>,
    brain: Option<BrainConfig>,
    agents: Vec<AgentConfig>,
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
        if let Some(v) = raw.daily_limit {
            self.daily_limit = Some(v);
        }
        if let Some(v) = raw.weekly_limit {
            self.weekly_limit = Some(v);
        }
        if let Some(v) = raw.context_warn_threshold {
            self.context_warn_threshold = v.min(100);
        }
        if let Some(h) = raw.health {
            if let Some(v) = h.cache_critical_pct {
                self.health.cache_critical_pct = v;
            }
            if let Some(v) = h.cache_warning_pct {
                self.health.cache_warning_pct = v;
            }
            if let Some(v) = h.cache_min_tokens {
                self.health.cache_min_tokens = v;
            }
            if let Some(v) = h.cost_spike_critical {
                self.health.cost_spike_critical = v;
            }
            if let Some(v) = h.cost_spike_warning {
                self.health.cost_spike_warning = v;
            }
            if let Some(v) = h.loop_max_calls {
                self.health.loop_max_calls = v;
            }
            if let Some(v) = h.stall_min_cost {
                self.health.stall_min_cost = v;
            }
            if let Some(v) = h.stall_min_minutes {
                self.health.stall_min_minutes = v;
            }
            if let Some(v) = h.context_critical_pct {
                self.health.context_critical_pct = v;
            }
            if let Some(v) = h.context_warning_pct {
                self.health.context_warning_pct = v;
            }
        }
        if let Some(v) = raw.file_conflicts {
            self.file_conflicts = v;
        }
        if let Some(v) = raw.auto_deny_file_conflicts {
            self.auto_deny_file_conflicts = v;
        }
        for override_ in raw.model_overrides {
            upsert_model_override(&mut self.model_overrides, override_);
        }
        for rule in raw.rules {
            // Replace rule with same name, or append
            if let Some(pos) = self.rules.iter().position(|r| r.name == rule.name) {
                self.rules[pos] = rule;
            } else {
                self.rules.push(rule);
            }
        }
        if let Some(brain) = raw.brain {
            self.brain = Some(brain);
        }
        for agent in raw.agents {
            if let Some(pos) = self.agents.iter().position(|a| a.name == agent.name) {
                self.agents[pos] = agent;
            } else {
                self.agents.push(agent);
            }
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
        println!(
            "  daily_limit:    {}",
            self.daily_limit
                .map(|b| format!("${b:.2}"))
                .unwrap_or_else(|| "none".into())
        );
        println!(
            "  weekly_limit:   {}",
            self.weekly_limit
                .map(|b| format!("${b:.2}"))
                .unwrap_or_else(|| "none".into())
        );
        println!("  context_warn: {}%", self.context_warn_threshold);
        println!();
        println!("  [orchestrate]");
        println!("  file_conflicts:           {}", self.file_conflicts);
        println!(
            "  auto_deny_file_conflicts: {}",
            self.auto_deny_file_conflicts
        );
        println!();
        println!("  [health]");
        println!(
            "  cache:    critical <{:.0}%, warning <{:.0}%, min {}",
            self.health.cache_critical_pct,
            self.health.cache_warning_pct,
            self.health.cache_min_tokens,
        );
        println!(
            "  cost:     critical >{:.1}x, warning >{:.1}x",
            self.health.cost_spike_critical, self.health.cost_spike_warning,
        );
        println!("  loop:     {} calls", self.health.loop_max_calls);
        println!(
            "  stall:    >${:.0} and >{}min",
            self.health.stall_min_cost, self.health.stall_min_minutes,
        );
        println!(
            "  context:  critical >{:.0}%, warning >{:.0}%",
            self.health.context_critical_pct, self.health.context_warning_pct,
        );
        if self.model_overrides.is_empty() {
            println!("  model_overrides: none");
        } else {
            println!("  model_overrides:");
            for override_ in &self.model_overrides {
                println!(
                    "    {} => in ${:.2}/M, out ${:.2}/M, ctx {}",
                    override_.name,
                    override_.profile.input_per_m,
                    override_.profile.output_per_m,
                    override_.profile.context_max
                );
            }
        }
    }

    /// Print an annotated default config template to stdout.
    pub fn print_template() {
        print!(
            r#"# claudectl configuration
# Place this file at:
#   Project: .claudectl.toml (in your project root)
#   Global:  ~/.config/claudectl/config.toml
#
# Priority: CLI flags > project config > global config > defaults
# Only set values you want to override — unset keys use defaults.

# ── General ─────────────────────────────────────────────────────────

[defaults]
# Refresh interval in milliseconds
# interval = 2000

# Enable desktop notifications on NeedsInput transitions
# notify = false

# Show debug timing metrics in the footer
# debug = false

# Group sessions by project in the table view
# grouped = false

# Default sort column: "Status", "Context", "Cost", "$/hr", "Elapsed"
# sort = "Status"

# Per-session budget in USD (alert at 80%, optionally kill at 100%)
# budget = 10.00

# Auto-kill sessions that exceed the budget (requires budget)
# kill_on_budget = false

# ── Webhook ─────────────────────────────────────────────────────────

[webhook]
# POST JSON on status changes
# url = "https://hooks.slack.com/services/..."

# Only fire on these status transitions (omit for all)
# events = ["NeedsInput", "Finished"]

# ── Budget Limits ───────────────────────────────────────────────────

[budget]
# Daily spending limit in USD
# daily_limit = 50.00

# Weekly spending limit in USD
# weekly_limit = 200.00

# ── Context ─────────────────────────────────────────────────────────

[context]
# Fire on_context_high hook when context usage crosses this percentage
# warn_threshold = 75

# ── Orchestration ───────────────────────────────────────────────────

[orchestrate]
# Detect file-level conflicts when multiple sessions edit the same file
# file_conflicts = true

# Auto-deny writes to files being edited by another session
# auto_deny_file_conflicts = false

# ── Health Check Thresholds ─────────────────────────────────────────

[health]
# Cache hit ratio thresholds (percentage, 0-100)
# cache_critical_pct = 10.0
# cache_warning_pct = 30.0
# cache_min_tokens = 10000

# Cost spike detection (multiplier of session average burn rate)
# cost_spike_critical = 5.0
# cost_spike_warning = 2.5

# Loop detection (tool call count threshold when errors are present)
# loop_max_calls = 10

# Stall detection (minimum cost in USD and minutes with no file edits)
# stall_min_cost = 5.0
# stall_min_minutes = 10

# Context saturation thresholds (percentage, 0-100)
# context_critical_pct = 90.0
# context_warning_pct = 80.0

# ── Model Pricing Overrides ─────────────────────────────────────────
# Override built-in pricing for specific models.
#
# [models."my-custom-model"]
# input_per_m = 3.00
# output_per_m = 15.00
# cache_read_per_m = 0.30
# cache_write_per_m = 3.75
# context_max = 200000

# ── Auto-Rules ──────────────────────────────────────────────────────
# Match sessions by status/tool/command/project/cost, then take action.
# Deny rules always take precedence regardless of order.
#
# [rules.approve_reads]
# match_status = ["Needs Input"]
# match_tool = ["Read", "Glob", "Grep"]
# action = "approve"
#
# [rules.deny_destructive]
# match_tool = ["Bash"]
# match_command = ["rm -rf", "git push --force"]
# action = "deny"
#
# [rules.kill_runaway]
# match_cost_above = 20.0
# action = "terminate"
#
# [rules.auto_continue]
# match_status = ["Waiting"]
# action = "send"
# message = "continue"

# ── Event Hooks ─────────────────────────────────────────────────────
# Run shell commands on session events.
#
# [hooks.on_needs_input]
# run = "notify-send 'Session needs input'"
#
# [hooks.on_finished]
# run = "say 'Session finished'"
#
# Available events: on_needs_input, on_finished, on_budget_80,
#   on_budget_exceeded, on_context_high, on_status_change

# ── Brain (Local LLM) ──────────────────────────────────────────────

# [brain]
# enabled = true
# endpoint = "http://localhost:11434/api/generate"
# model = "gemma4:e4b"
# auto = false
# timeout_ms = 5000
# max_context_tokens = 4000
# few_shot_count = 5
# max_sessions = 10
# orchestrate = false
# orchestrate_interval = 30

# ── External Agents ─────────────────────────────────────────────────
#
# [agents.codex]
# type = "codex"
# command = "codex --quiet"
# capabilities = ["code-review", "refactoring"]
# cwd = "/path/to/project"
"#
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
            ("budget", "daily_limit") => {
                raw.daily_limit = value.parse().ok();
            }
            ("budget", "weekly_limit") => {
                raw.weekly_limit = value.parse().ok();
            }
            ("context", "warn_threshold") => {
                raw.context_warn_threshold = value.parse().ok();
            }
            ("orchestrate", "file_conflicts") => {
                raw.file_conflicts = parse_bool(value);
            }
            ("orchestrate", "auto_deny_file_conflicts") => {
                raw.auto_deny_file_conflicts = parse_bool(value);
            }
            ("health", key) => {
                let h = raw.health.get_or_insert_with(RawHealthThresholds::default);
                match key {
                    "cache_critical_pct" => h.cache_critical_pct = value.parse().ok(),
                    "cache_warning_pct" => h.cache_warning_pct = value.parse().ok(),
                    "cache_min_tokens" => h.cache_min_tokens = value.parse().ok(),
                    "cost_spike_critical" => h.cost_spike_critical = value.parse().ok(),
                    "cost_spike_warning" => h.cost_spike_warning = value.parse().ok(),
                    "loop_max_calls" => h.loop_max_calls = value.parse().ok(),
                    "stall_min_cost" => h.stall_min_cost = value.parse().ok(),
                    "stall_min_minutes" => h.stall_min_minutes = value.parse().ok(),
                    "context_critical_pct" => h.context_critical_pct = value.parse().ok(),
                    "context_warning_pct" => h.context_warning_pct = value.parse().ok(),
                    _ => {}
                }
            }
            _ if parse_model_section(&section).is_some() => {
                let Some(model_name) = parse_model_section(&section) else {
                    continue;
                };
                let profile = ensure_model_override(&mut raw.model_overrides, &model_name);
                match key {
                    "input_per_m" => {
                        profile.input_per_m = value.parse().unwrap_or(profile.input_per_m);
                    }
                    "output_per_m" => {
                        profile.output_per_m = value.parse().unwrap_or(profile.output_per_m);
                    }
                    "cache_read_per_m" => {
                        profile.cache_read_per_m =
                            value.parse().unwrap_or(profile.cache_read_per_m);
                    }
                    "cache_write_per_m" => {
                        profile.cache_write_per_m =
                            value.parse().unwrap_or(profile.cache_write_per_m);
                    }
                    "context_max" => {
                        profile.context_max = value.parse().unwrap_or(profile.context_max);
                    }
                    _ => {}
                }
            }
            _ if parse_rule_section(&section).is_some() => {
                let Some(rule_name) = parse_rule_section(&section) else {
                    continue;
                };
                let rule = ensure_rule(&mut raw.rules, &rule_name);
                match key {
                    "match_status" => rule.match_status = parse_string_array(value),
                    "match_tool" => rule.match_tool = parse_string_array(value),
                    "match_command" => rule.match_command = parse_string_array(value),
                    "match_project" => rule.match_project = parse_string_array(value),
                    "match_cost_above" => rule.match_cost_above = value.parse().ok(),
                    "match_last_error" => rule.match_last_error = parse_bool(value),
                    "match_file_conflict" => rule.match_file_conflict = parse_bool(value),
                    "action" => {
                        if let Some(a) = RuleAction::parse(&unquote(value)) {
                            rule.action = a;
                        }
                    }
                    "message" => rule.message = Some(unquote(value)),
                    _ => {}
                }
            }
            ("brain", _) => {
                let brain = raw.brain.get_or_insert_with(BrainConfig::default);
                match key {
                    "enabled" => {
                        if let Some(v) = parse_bool(value) {
                            brain.enabled = v;
                        }
                    }
                    "endpoint" => brain.endpoint = unquote(value),
                    "model" => brain.model = unquote(value),
                    "auto" => {
                        if let Some(v) = parse_bool(value) {
                            brain.auto_mode = v;
                        }
                    }
                    "timeout_ms" => {
                        if let Ok(v) = value.parse() {
                            brain.timeout_ms = v;
                        }
                    }
                    "max_context_tokens" => {
                        if let Ok(v) = value.parse() {
                            brain.max_context_tokens = v;
                        }
                    }
                    "few_shot_count" => {
                        if let Ok(v) = value.parse() {
                            brain.few_shot_count = v;
                        }
                    }
                    "max_sessions" => {
                        if let Ok(v) = value.parse() {
                            brain.max_sessions = v;
                        }
                    }
                    "orchestrate" => {
                        if let Some(v) = parse_bool(value) {
                            brain.orchestrate = v;
                        }
                    }
                    "orchestrate_interval" => {
                        if let Ok(v) = value.parse() {
                            brain.orchestrate_interval_secs = v;
                        }
                    }
                    _ => {}
                }
            }
            _ if parse_agent_section(&section).is_some() => {
                let Some(agent_name) = parse_agent_section(&section) else {
                    continue;
                };
                let agent = ensure_agent(&mut raw.agents, &agent_name);
                match key {
                    "type" => agent.agent_type = unquote(value),
                    "command" => agent.command = unquote(value),
                    "capabilities" => agent.capabilities = parse_string_array(value),
                    "cwd" => agent.cwd = unquote(value),
                    _ => {}
                }
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

fn parse_model_section(section: &str) -> Option<String> {
    section.strip_prefix("models.").map(unquote)
}

fn ensure_model_override<'a>(
    overrides: &'a mut Vec<ModelOverride>,
    model_name: &str,
) -> &'a mut ModelProfile {
    if let Some(index) = overrides.iter().position(|item| item.name == model_name) {
        return &mut overrides[index].profile;
    }

    overrides.push(ModelOverride {
        name: model_name.to_string(),
        profile: ModelProfile {
            input_per_m: 0.0,
            output_per_m: 0.0,
            cache_read_per_m: 0.0,
            cache_write_per_m: 0.0,
            context_max: 0,
        },
    });

    &mut overrides
        .last_mut()
        .expect("override was just pushed")
        .profile
}

fn upsert_model_override(overrides: &mut Vec<ModelOverride>, incoming: ModelOverride) {
    if let Some(existing) = overrides.iter_mut().find(|item| item.name == incoming.name) {
        *existing = incoming;
    } else {
        overrides.push(incoming);
    }
}

fn parse_rule_section(section: &str) -> Option<String> {
    section.strip_prefix("rules.").map(unquote)
}

fn ensure_rule<'a>(rules: &'a mut Vec<AutoRule>, name: &str) -> &'a mut AutoRule {
    if let Some(index) = rules.iter().position(|r| r.name == name) {
        return &mut rules[index];
    }
    rules.push(AutoRule::new(name.to_string(), RuleAction::Approve));
    rules.last_mut().expect("rule was just pushed")
}

fn parse_agent_section(section: &str) -> Option<String> {
    section.strip_prefix("agents.").map(unquote)
}

fn ensure_agent<'a>(agents: &'a mut Vec<AgentConfig>, name: &str) -> &'a mut AgentConfig {
    if let Some(index) = agents.iter().position(|a| a.name == name) {
        return &mut agents[index];
    }
    agents.push(AgentConfig::new(name.to_string()));
    agents.last_mut().expect("agent was just pushed")
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

[models."gpt-4o"]
input_per_m = 1.25
output_per_m = 5.0
cache_read_per_m = 0.15
cache_write_per_m = 0.9
context_max = 128000
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
        assert_eq!(raw.model_overrides.len(), 1);
        assert_eq!(raw.model_overrides[0].name, "gpt-4o");
        assert_eq!(raw.model_overrides[0].profile.context_max, 128_000);
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

    #[test]
    fn test_parse_rules_from_config() {
        use std::io::Write;
        let mut file = tempfile::NamedTempFile::new().unwrap();
        writeln!(
            file,
            r#"
[rules.approve_reads]
match_status = ["Needs Input"]
match_tool = ["Read", "Glob", "Grep"]
action = "approve"

[rules.deny_destructive]
match_status = ["Needs Input"]
match_tool = ["Bash"]
match_command = ["rm -rf", "git push --force"]
action = "deny"

[rules.auto_continue]
match_status = ["Waiting"]
action = "send"
message = "continue"

[rules.kill_expensive]
match_cost_above = 10.0
action = "terminate"
"#
        )
        .unwrap();
        file.flush().unwrap();

        let raw = parse_config_file(&file.path().to_path_buf()).unwrap();
        assert_eq!(raw.rules.len(), 4);

        let r0 = &raw.rules[0];
        assert_eq!(r0.name, "approve_reads");
        assert_eq!(r0.match_tool, vec!["Read", "Glob", "Grep"]);
        assert_eq!(r0.action, RuleAction::Approve);

        let r1 = &raw.rules[1];
        assert_eq!(r1.name, "deny_destructive");
        assert_eq!(r1.match_command, vec!["rm -rf", "git push --force"]);
        assert_eq!(r1.action, RuleAction::Deny);

        let r2 = &raw.rules[2];
        assert_eq!(r2.name, "auto_continue");
        assert_eq!(r2.action, RuleAction::Send);
        assert_eq!(r2.message, Some("continue".into()));

        let r3 = &raw.rules[3];
        assert_eq!(r3.name, "kill_expensive");
        assert_eq!(r3.match_cost_above, Some(10.0));
        assert_eq!(r3.action, RuleAction::Terminate);
    }

    #[test]
    fn test_parse_brain_config() {
        use std::io::Write;
        let mut file = tempfile::NamedTempFile::new().unwrap();
        writeln!(
            file,
            r#"
[brain]
enabled = true
endpoint = "http://localhost:8080/v1/chat"
model = "llama3:8b"
auto = true
timeout_ms = 3000
max_context_tokens = 8000
"#
        )
        .unwrap();
        file.flush().unwrap();

        let raw = parse_config_file(&file.path().to_path_buf()).unwrap();
        let brain = raw.brain.expect("brain config should be parsed");
        assert!(brain.enabled);
        assert_eq!(brain.endpoint, "http://localhost:8080/v1/chat");
        assert_eq!(brain.model, "llama3:8b");
        assert!(brain.auto_mode);
        assert_eq!(brain.timeout_ms, 3000);
        assert_eq!(brain.max_context_tokens, 8000);
    }

    #[test]
    fn test_no_brain_config_returns_none() {
        use std::io::Write;
        let mut file = tempfile::NamedTempFile::new().unwrap();
        writeln!(file, "[defaults]\ninterval = 1000").unwrap();
        file.flush().unwrap();

        let raw = parse_config_file(&file.path().to_path_buf()).unwrap();
        assert!(raw.brain.is_none());
    }

    #[test]
    fn test_parse_agents_from_config() {
        use std::io::Write;
        let mut file = tempfile::NamedTempFile::new().unwrap();
        writeln!(
            file,
            r#"
[agents.codex]
type = "codex"
command = "codex --quiet"
capabilities = ["code-review", "refactoring"]
cwd = "/tmp/project"

[agents.aider]
type = "aider"
command = "aider --yes"
capabilities = ["implementation", "debugging"]
"#
        )
        .unwrap();
        file.flush().unwrap();

        let raw = parse_config_file(&file.path().to_path_buf()).unwrap();
        assert_eq!(raw.agents.len(), 2);

        let codex = &raw.agents[0];
        assert_eq!(codex.name, "codex");
        assert_eq!(codex.agent_type, "codex");
        assert_eq!(codex.command, "codex --quiet");
        assert_eq!(codex.capabilities, vec!["code-review", "refactoring"]);
        assert_eq!(codex.cwd, "/tmp/project");

        let aider = &raw.agents[1];
        assert_eq!(aider.name, "aider");
        assert_eq!(aider.command, "aider --yes");
    }

    #[test]
    fn test_parse_health_thresholds() {
        use std::io::Write;
        let mut file = tempfile::NamedTempFile::new().unwrap();
        writeln!(
            file,
            r#"
[health]
cache_critical_pct = 5.0
cache_warning_pct = 20.0
cache_min_tokens = 50000
cost_spike_critical = 8.0
cost_spike_warning = 3.0
loop_max_calls = 15
stall_min_cost = 10.0
stall_min_minutes = 20
context_critical_pct = 95.0
context_warning_pct = 85.0
"#
        )
        .unwrap();
        file.flush().unwrap();

        let raw = parse_config_file(&file.path().to_path_buf()).unwrap();
        let h = raw.health.expect("health config should be parsed");
        assert_eq!(h.cache_critical_pct, Some(5.0));
        assert_eq!(h.cache_warning_pct, Some(20.0));
        assert_eq!(h.cache_min_tokens, Some(50000));
        assert_eq!(h.cost_spike_critical, Some(8.0));
        assert_eq!(h.cost_spike_warning, Some(3.0));
        assert_eq!(h.loop_max_calls, Some(15));
        assert_eq!(h.stall_min_cost, Some(10.0));
        assert_eq!(h.stall_min_minutes, Some(20));
        assert_eq!(h.context_critical_pct, Some(95.0));
        assert_eq!(h.context_warning_pct, Some(85.0));
    }

    #[test]
    fn test_health_thresholds_layering() {
        let mut config = Config::default();
        assert_eq!(config.health.cache_critical_pct, 10.0); // default

        config.apply(RawConfig {
            health: Some(RawHealthThresholds {
                cache_critical_pct: Some(5.0),
                ..RawHealthThresholds::default()
            }),
            ..RawConfig::default()
        });
        assert_eq!(config.health.cache_critical_pct, 5.0); // overridden
        assert_eq!(config.health.cache_warning_pct, 30.0); // unchanged default
    }

    #[test]
    fn test_parse_orchestrate_config() {
        use std::io::Write;
        let mut file = tempfile::NamedTempFile::new().unwrap();
        writeln!(
            file,
            r#"
[orchestrate]
file_conflicts = true
auto_deny_file_conflicts = true
"#
        )
        .unwrap();
        file.flush().unwrap();

        let raw = parse_config_file(&file.path().to_path_buf()).unwrap();
        assert_eq!(raw.file_conflicts, Some(true));
        assert_eq!(raw.auto_deny_file_conflicts, Some(true));
    }

    #[test]
    fn test_orchestrate_defaults() {
        let config = Config::default();
        assert!(config.file_conflicts); // on by default
        assert!(!config.auto_deny_file_conflicts); // off by default
    }
}
