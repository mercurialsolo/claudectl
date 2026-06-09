#![allow(unknown_lints)]
#![allow(
    clippy::collapsible_if,
    clippy::manual_is_multiple_of,
    clippy::io_other_error
)]

// Foundational modules from claudectl-core (epic #279). Re-aliased so existing
// `crate::session::*` paths still resolve. See `lib.rs` for the rationale.
use claudectl_core::{
    discovery, history, hooks, launch, logger, models, process, rules, session, terminals, theme,
    transcript,
};
// TUI now lives in `claudectl-tui` (issue #275). The `app` + `ui` + TUI
// peripheral modules are imported here so existing `app::App`, `ui::table`,
// `demo::*`, `recorder::*`, `session_recorder::*` paths in main.rs resolve
// unchanged.
use claudectl_tui::{app, demo, recorder, session_recorder, ui};

mod brain;
mod brain_screen;
#[cfg(feature = "bus")]
mod bus;
mod commands;
mod config;
#[cfg(feature = "coord")]
mod coord;
mod doctor;
#[cfg(feature = "hive")]
mod hive;
#[cfg(feature = "coord")]
mod ingest;
mod init;
mod orchestrator;
#[cfg(feature = "relay")]
mod relay;
mod runtime;

use std::io;
use std::time::{Duration, Instant};

use clap::{CommandFactory, Parser, Subcommand};
use clap_complete::Shell;
use crossterm::{
    event::{self, Event},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{Terminal, backend::CrosstermBackend};

use app::{App, FocusFilter, StatusFilter};

#[derive(Clone)]
pub(crate) struct ViewFilters {
    pub(crate) status_filter: StatusFilter,
    pub(crate) focus_filter: FocusFilter,
    pub(crate) search: String,
}

#[derive(Subcommand)]
pub(crate) enum Command {
    #[cfg(feature = "relay")]
    /// Relay: peer-to-peer connections, delegation, and discovery
    Relay {
        #[command(subcommand)]
        command: relay::cli::RelayCommand,
    },

    #[cfg(feature = "hive")]
    /// Hive: shared knowledge store, trust, archive, and distillation
    Hive {
        #[command(subcommand)]
        command: hive::cli::HiveCommand,
    },

    #[cfg(feature = "coord")]
    /// Coordination: events, leases, blockers, handoffs, interrupts, and memory
    Coord {
        #[command(subcommand)]
        command: coord::cli::CoordCommand,
    },

    #[cfg(feature = "bus")]
    /// Agent bus: MCP server, roles, mailboxes (docs/AGENT_BUS.md)
    Bus {
        #[command(subcommand)]
        command: bus::cli::BusCommand,
    },

    #[cfg(feature = "coord")]
    /// Ingest a Claude Code hook payload from stdin into the coord
    /// `hook_events` table (#345, RFC v2 §6). Best-effort by
    /// construction — meant to be called from a bash hook with
    /// `2>/dev/null || true`. JSONL tail + `ps` stay authoritative;
    /// this is a latency optimization for the supervisor's reconciler.
    Ingest {
        /// Which hook is calling. One of `PreToolUse`, `PostToolUse`,
        /// `Stop`, `SessionStart`, `Notification`, `UserPromptSubmit`.
        #[arg(long)]
        hook: String,
    },

    /// Onboarding wizard (budget, brain, hooks, bus, skills). See issue #257.
    Init {
        /// Drift report comparing recorded onboarding against current state.
        #[arg(long, conflicts_with_all = ["reset", "remove", "non_interactive"])]
        check: bool,
        /// Clear the onboarding marker so the next `init` starts fresh.
        #[arg(long, conflicts_with_all = ["check", "remove", "non_interactive"])]
        reset: bool,
        /// Uninstall every claudectl-managed artifact (hooks, marker).
        /// Preserves user data: bus DB roles, brain decision logs, hive
        /// knowledge, relay identity, config file. Pair with `--purge` to
        /// wipe everything.
        #[arg(long, conflicts_with_all = ["check", "reset", "non_interactive", "purge"])]
        remove: bool,
        /// Hard uninstall: `--remove` PLUS delete `~/.claudectl/` entirely
        /// (bus DB, brain decisions, hive knowledge, relay identity, coord
        /// state) and `~/.config/claudectl/config.toml`. Use to start over
        /// from a clean slate. Requires `--yes` to proceed without prompt.
        #[arg(long, conflicts_with_all = ["check", "reset", "non_interactive", "remove"])]
        purge: bool,
        /// Skip the confirmation prompt for `--purge`.
        #[arg(long)]
        yes: bool,
        /// Install (or re-install) just the embedded plugin + hooks (#325).
        /// Skip every other phase. Useful for users who already configured
        /// budget / brain / bus and just want to refresh the plugin files
        /// after `brew upgrade claudectl`.
        #[arg(
            long,
            conflicts_with_all = ["check", "reset", "remove", "purge", "non_interactive"]
        )]
        plugin_only: bool,
        /// Re-sync everything the previous `init` wrote to match the
        /// running binary (#327): hook entries, embedded plugin files,
        /// DB schema migrations, and the onboarding marker version. Use
        /// after `brew upgrade claudectl` / `cargo install ... --force`.
        #[arg(
            long,
            conflicts_with_all = ["check", "reset", "remove", "purge", "plugin_only", "non_interactive"]
        )]
        upgrade: bool,
        /// Run every phase without prompting. Combine with the per-phase
        /// flags below (`--budget`, `--brain-url`, `--bus-role`, etc.).
        #[arg(long)]
        non_interactive: bool,

        /// Weekly budget cap in USD (used with --non-interactive).
        #[arg(long)]
        budget: Option<f64>,
        /// Skip the budget phase (used with --non-interactive).
        #[arg(long)]
        skip_budget: bool,

        /// Local-LLM endpoint URL for the brain phase.
        #[arg(long)]
        brain_url: Option<String>,
        /// Skip the brain phase.
        #[arg(long)]
        skip_brain: bool,

        /// Install Claude Code hooks (default in --non-interactive).
        #[arg(long)]
        install_plugin: bool,
        /// Skip the plugin phase.
        #[arg(long)]
        skip_plugin: bool,

        /// Bind this role for the bus phase (used with --non-interactive).
        #[arg(long)]
        bus_role: Option<String>,
        /// cwd to bind the bus role to. Defaults to the process cwd.
        #[arg(long)]
        bus_cwd: Option<String>,
        /// Skip the bus phase.
        #[arg(long)]
        skip_bus: bool,

        /// Skip the skills phase.
        #[arg(long)]
        skip_skills: bool,
    },

    /// Print a shell completion script for the given shell (bash, zsh, fish, …) to stdout
    Completions {
        /// Shell to emit completions for
        #[arg(value_enum)]
        shell: Shell,
    },

    /// Print a roff-formatted man page to stdout
    Man,

    /// Install + runtime health check. Answers "is everything wired up?"
    /// in one command — PATH, hooks, plugin files, brain endpoint, bus
    /// feature, bus DB, session discovery, terminal integration.
    /// Exits non-zero on any failure; advisories don't affect exit code.
    Doctor {
        /// Emit JSON instead of human-readable output.
        #[arg(long)]
        json: bool,
    },
}

#[derive(Parser)]
#[command(
    name = "claudectl",
    version,
    about = "Orchestrate a swarm of Claude Code agents with a local-LLM brain that learns from you."
)]
pub(crate) struct Cli {
    // ── Dashboard ───────────────────────────────────────────────────────
    /// Refresh interval in milliseconds
    #[arg(short, long, default_value_t = 2000, help_heading = "Dashboard")]
    pub(crate) interval: u64,

    /// Color theme: dark, light, or none (respects NO_COLOR env var)
    #[arg(long, help_heading = "Dashboard")]
    pub(crate) theme: Option<String>,

    /// Enable debug mode: show timing metrics in the footer
    #[arg(long, help_heading = "Dashboard")]
    pub(crate) debug: bool,

    /// Run with deterministic fake sessions for screenshots and recordings
    #[arg(long, help_heading = "Dashboard")]
    pub(crate) demo: bool,

    // ── Output Modes ───────────────────────────────────────────────────
    /// Print session list to stdout and exit (no TUI)
    #[arg(short, long, help_heading = "Output Modes")]
    pub(crate) list: bool,

    /// Print JSON array of sessions and exit
    #[arg(long, help_heading = "Output Modes")]
    pub(crate) json: bool,

    /// Stream status changes to stdout (no TUI). Only prints when status changes.
    #[arg(short, long, help_heading = "Output Modes")]
    pub(crate) watch: bool,

    /// Run headless with brain, coordination, and context rot prevention active (no TUI).
    /// Attach a dashboard with `claudectl` in another terminal.
    #[arg(long, help_heading = "Output Modes")]
    pub(crate) headless: bool,

    /// Output format for watch mode. Placeholders: {pid}, {project}, {status}, {cost}, {context}
    #[arg(
        long,
        default_value = "{pid} {project}: {status} (${cost}, ctx {context}%)",
        help_heading = "Output Modes"
    )]
    pub(crate) format: String,

    /// Show summary of session activity and exit
    #[arg(long, help_heading = "Output Modes")]
    pub(crate) summary: bool,

    /// Time window for --summary, --history, --stats (e.g., "8h", "24h", "30m")
    #[arg(long, default_value = "24h", help_heading = "Output Modes")]
    pub(crate) since: String,

    // ── Filtering ──────────────────────────────────────────────────────
    /// Filter sessions by status (e.g., "NeedsInput", "Processing", "Finished")
    #[arg(long, help_heading = "Filtering")]
    pub(crate) filter_status: Option<String>,

    /// Focus on a high-signal subset: attention, over-budget, high-context, unknown-telemetry, conflict
    #[arg(long, help_heading = "Filtering")]
    pub(crate) focus: Option<String>,

    /// Search project/model/session text
    #[arg(long, help_heading = "Filtering")]
    pub(crate) search: Option<String>,

    // ── Session Management ─────────────────────────────────────────────
    /// Launch a new Claude Code session in the given directory
    #[arg(long = "new", help_heading = "Session Management")]
    pub(crate) new_session: bool,

    /// Working directory for the new session (used with --new)
    #[arg(long, default_value = ".", help_heading = "Session Management")]
    pub(crate) cwd: String,

    /// Prompt to send to the new session (used with --new)
    #[arg(long, help_heading = "Session Management")]
    pub(crate) prompt: Option<String>,

    /// Resume a session by ID (used with --new)
    #[arg(long, help_heading = "Session Management")]
    pub(crate) resume: Option<String>,

    // ── Budget & Notifications ─────────────────────────────────────────
    /// Per-session budget in USD. Alert at 80%, optionally kill at 100%.
    #[arg(long, help_heading = "Budget & Notifications")]
    pub(crate) budget: Option<f64>,

    /// Auto-kill sessions that exceed the budget (requires --budget)
    #[arg(long, help_heading = "Budget & Notifications")]
    pub(crate) kill_on_budget: bool,

    /// Enable desktop notifications on NeedsInput transitions
    #[arg(long, help_heading = "Budget & Notifications")]
    pub(crate) notify: bool,

    /// Webhook URL to POST JSON on status changes
    #[arg(long, help_heading = "Budget & Notifications")]
    pub(crate) webhook: Option<String>,

    /// Only fire webhook on these status transitions (comma-separated, e.g. "NeedsInput,Finished")
    #[arg(long, help_heading = "Budget & Notifications")]
    pub(crate) webhook_on: Option<String>,

    // ── Brain (Local LLM) ──────────────────────────────────────────────
    /// Enable local LLM brain for session advisory (requires ollama or compatible endpoint)
    #[arg(long, help_heading = "Brain (Local LLM)")]
    pub(crate) brain: bool,

    /// Auto-execute brain suggestions without confirmation (requires --brain)
    #[arg(long, help_heading = "Brain (Local LLM)")]
    pub(crate) auto_run: bool,

    /// LLM endpoint URL for brain (requires --brain)
    #[arg(long, help_heading = "Brain (Local LLM)")]
    pub(crate) url: Option<String>,

    /// Override brain model name (requires --brain)
    #[arg(long, help_heading = "Brain (Local LLM)")]
    pub(crate) brain_model: Option<String>,

    /// Run brain eval scenarios against the local LLM and report results
    #[arg(long, help_heading = "Brain (Local LLM)")]
    pub(crate) brain_eval: bool,

    /// List brain prompt templates and their source (built-in vs user override)
    #[arg(long, help_heading = "Brain (Local LLM)")]
    pub(crate) brain_prompts: bool,

    /// Brain statistics and metrics (subcommands: scorecard, tier, latency, cache, counterfactual, learning-curve, accuracy, baseline, false-approve, help)
    #[arg(long, help_heading = "Brain (Local LLM)")]
    pub(crate) brain_stats: Option<String>,

    /// Interactive review of the highest-value brain decisions. Walks through
    /// counterfactual hits, Critical-tier safety cases, and high-confidence
    /// misses; lets you mark each as canonical (teaching material).
    /// Pass "list" / "queue" to print the queue non-interactively.
    #[arg(long, help_heading = "Brain (Local LLM)")]
    pub(crate) brain_review: Option<Option<String>>,

    /// Mark a specific decision_id as canonical without going through the
    /// interactive review (used by `brain counterfactual` output).
    #[arg(long, help_heading = "Brain (Local LLM)")]
    pub(crate) brain_mark_canonical: Option<String>,

    /// Query the brain for a single tool-call decision and exit (JSON output).
    /// Used by Claude Code plugin hooks for inline approve/deny.
    #[arg(long, help_heading = "Brain (Local LLM)")]
    pub(crate) brain_query: bool,

    /// Tool name for --brain-query (e.g., "Bash", "Write", "Edit")
    #[arg(long, help_heading = "Brain (Local LLM)")]
    pub(crate) tool: Option<String>,

    /// Command or input for --brain-query (e.g., "rm -rf /tmp")
    #[arg(long, help_heading = "Brain (Local LLM)")]
    pub(crate) tool_input: Option<String>,

    /// Project name for --brain-query context (defaults to current directory name)
    #[arg(long, help_heading = "Brain (Local LLM)")]
    pub(crate) project: Option<String>,

    /// Set brain gate mode: on (default), off (disable), auto (full auto-approve).
    /// Controls whether the Claude Code plugin hook queries the brain.
    #[arg(long, help_heading = "Brain (Local LLM)")]
    pub(crate) mode: Option<String>,

    /// Record a tool-call outcome to the pending-outcomes spool.
    /// Used by the Claude Code PostToolUse hook for #220 baselining.
    /// Reads pending-outcome JSON from stdin (preferred) or builds one from
    /// --tool, --tool-input, --project, --exit-code, --duration-ms, --stderr-tail.
    #[arg(long, help_heading = "Brain (Local LLM)")]
    pub(crate) record_outcome: bool,

    /// Tool exit code for --record-outcome (0 = success).
    #[arg(long, help_heading = "Brain (Local LLM)")]
    pub(crate) exit_code: Option<i32>,

    /// Tool wall-clock duration in milliseconds for --record-outcome.
    #[arg(long, help_heading = "Brain (Local LLM)")]
    pub(crate) duration_ms: Option<u64>,

    /// Tail of stderr / tool error output for --record-outcome
    /// (truncated to MAX_STDERR_TAIL_BYTES).
    #[arg(long, help_heading = "Brain (Local LLM)")]
    pub(crate) stderr_tail: Option<String>,

    /// Claude Code session id (passed through hook payload), optional.
    #[arg(long, help_heading = "Brain (Local LLM)")]
    pub(crate) session_id: Option<String>,

    /// Claude Code tool_use_id (passed through hook payload), optional.
    #[arg(long, help_heading = "Brain (Local LLM)")]
    pub(crate) tool_use_id: Option<String>,

    /// Reap pending outcomes: attribute each to a matching decision and
    /// archive orphans older than 24h. Exits with the reap stats as JSON
    /// when --json is set, otherwise human-readable.
    #[arg(long, help_heading = "Brain (Local LLM)")]
    pub(crate) reap_outcomes: bool,

    /// List resolved tool-call outcomes attributed to brain decisions.
    /// Filterable by --tool and --project. Honours --json.
    #[arg(long, help_heading = "Brain (Local LLM)")]
    pub(crate) brain_outcomes: bool,

    /// Rank approaches by outcome data (success_rate * sample_count).
    /// Filterable by --tool and --project. Honours --json.
    #[arg(long, help_heading = "Brain (Local LLM)")]
    pub(crate) brain_baseline: bool,

    /// Limit baseline ranking output to top N rows.
    #[arg(long, help_heading = "Brain (Local LLM)")]
    pub(crate) top: Option<usize>,

    /// Show auto-generated insights, or set mode (on/off/status).
    /// Requires --brain or brain.enabled in config.
    /// Without argument: show current insights.
    /// With argument: set insights mode (on = auto-generate, off = disable).
    #[arg(long, help_heading = "Brain (Local LLM)", num_args = 0..=1, default_missing_value = "")]
    pub(crate) insights: Option<String>,

    /// Propose CLAUDE.md additions from high-confidence brain preferences.
    /// Use --project to scope to a specific project's preferences, and --apply
    /// to write the suggestions to CLAUDE.md.
    #[arg(long, help_heading = "Brain (Local LLM)")]
    pub(crate) brain_garden: bool,

    /// Apply changes alongside actions that propose them
    /// (currently used with --brain-garden).
    #[arg(long, help_heading = "Brain (Local LLM)")]
    pub(crate) apply: bool,

    /// Print a markdown session briefing for the given --project.
    /// Aggregates recent decisions, learned preferences, and known
    /// anti-patterns into context suitable for injection at session start.
    #[arg(long, help_heading = "Brain (Local LLM)")]
    pub(crate) brain_briefing: bool,

    // ── Orchestration ──────────────────────────────────────────────────
    /// Analyze a prompt and suggest parallel sub-tasks (outputs TaskFile JSON)
    #[arg(long, help_heading = "Orchestration")]
    pub(crate) decompose: Option<String>,

    /// Run tasks from a JSON file (e.g., claudectl --run tasks.json)
    #[arg(long, help_heading = "Orchestration")]
    pub(crate) run: Option<String>,

    /// Run independent tasks in parallel (used with --run)
    #[arg(long, help_heading = "Orchestration")]
    pub(crate) parallel: bool,

    // ── Subcommands ──────────────────────────────────────────────────
    #[command(subcommand)]
    command: Option<Command>,

    // ── Recording ──────────────────────────────────────────────────────
    /// Record the TUI session as an asciicast v2 file (e.g., --record demo.cast)
    #[arg(long, help_heading = "Recording")]
    pub(crate) record: Option<String>,

    /// Auto-quit the TUI after this many seconds (useful with --demo --record)
    #[arg(long, help_heading = "Recording")]
    pub(crate) duration: Option<u64>,

    // ── Cleanup ────────────────────────────────────────────────────────
    /// Clean up old session data (JSONL transcripts, session JSON files)
    #[arg(long, help_heading = "Cleanup")]
    pub(crate) clean: bool,

    /// Only clean sessions older than this duration (e.g., "7d", "24h"). Used with --clean.
    #[arg(long, help_heading = "Cleanup")]
    pub(crate) older_than: Option<String>,

    /// Only clean sessions that have finished. Used with --clean.
    #[arg(long, help_heading = "Cleanup")]
    pub(crate) finished: bool,

    /// Show what would be removed without deleting. Used with --clean.
    #[arg(long, help_heading = "Cleanup")]
    pub(crate) dry_run: bool,

    // ── History & Diagnostics ──────────────────────────────────────────
    /// Run post-mortem analysis on a completed session transcript
    #[arg(long, help_heading = "History & Diagnostics")]
    pub(crate) autopsy: bool,

    /// Session ID or JSONL path for --autopsy (defaults to most recent session)
    #[arg(long, help_heading = "History & Diagnostics")]
    pub(crate) session: Option<String>,

    /// Show history of completed sessions and exit
    #[arg(long, help_heading = "History & Diagnostics")]
    pub(crate) history: bool,

    /// Show aggregated session statistics and exit
    #[arg(long, help_heading = "History & Diagnostics")]
    pub(crate) stats: bool,

    /// Show resolved configuration and exit
    #[arg(long, help_heading = "History & Diagnostics")]
    pub(crate) config: bool,

    /// Print an annotated default config template to stdout and exit
    #[arg(long, help_heading = "History & Diagnostics")]
    pub(crate) config_template: bool,

    /// Validate config files and report unknown keys or malformed values
    #[arg(long, help_heading = "History & Diagnostics")]
    pub(crate) config_validate: bool,

    /// Write a sample .claudectl.toml in the current directory
    #[arg(long, help_heading = "Setup")]
    pub(crate) config_init: bool,

    /// List configured event hooks and exit
    #[arg(long, help_heading = "History & Diagnostics")]
    pub(crate) hooks: bool,

    /// Diagnose terminal integration and setup requirements
    #[arg(long, help_heading = "History & Diagnostics")]
    pub(crate) doctor: bool,

    /// Write diagnostic logs to a file (for debugging/bug reports)
    #[arg(long, help_heading = "History & Diagnostics")]
    pub(crate) log: Option<String>,

    // ── Setup ─────────────────────────────────────────────────────────
    /// Wire up Claude Code hooks in .claude/settings.json and exit
    #[arg(long, help_heading = "Setup")]
    pub(crate) init: bool,

    /// Remove claudectl hooks from .claude/settings.json and exit
    #[arg(long, help_heading = "Setup", conflicts_with = "init")]
    pub(crate) uninstall: bool,

    /// Configuration scope: user (global ~/.claude/settings.json) or project (.claude/settings.local.json)
    #[arg(short, long, default_value = "user", help_heading = "Setup")]
    pub(crate) scope: String,
}

fn main() -> io::Result<()> {
    let cli = Cli::parse();
    let is_demo = cli.demo;
    let result = run_main(cli);
    if result.is_ok() {
        maybe_print_star_prompt(is_demo);
    }
    result
}

fn maybe_print_star_prompt(is_demo: bool) {
    let marker = std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from("/tmp"))
        .join(".claudectl/.star-prompted");

    let first_run = !marker.exists();

    if is_demo || first_run {
        eprintln!();
        eprintln!(
            "\u{2b50} If claudectl is useful, star it: https://github.com/mercurialsolo/claudectl"
        );

        if first_run {
            if let Some(parent) = marker.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let _ = std::fs::write(&marker, "");
        }
    }
}

/// First-run detection for the activation nudge (#322). Returns true when
/// the user has neither onboarded (`~/.claudectl/onboarding.json` absent)
/// nor installed Claude Code hooks (`~/.claude/settings.json` lacks any
/// `claudectl` entries). When either is present, we assume the operator
/// knows what they're doing and stay quiet.
fn is_first_run() -> bool {
    let Some(home) = std::env::var_os("HOME").map(std::path::PathBuf::from) else {
        return false;
    };
    let marker = home.join(".claudectl").join("onboarding.json");
    let settings = home.join(".claude").join("settings.json");
    let onboarded = marker.exists();
    let hooked = std::fs::read_to_string(&settings)
        .map(|s| s.contains("claudectl"))
        .unwrap_or(false);
    !onboarded && !hooked
}

/// One-screen banner shown above the empty TUI when the user hasn't
/// onboarded yet (#322). Goes to stderr so it doesn't pollute stdout
/// piping; appears before the alt-screen swap.
fn print_first_run_banner() {
    eprintln!();
    eprintln!("┌─────────────────────────────────────────────────────────────────┐");
    eprintln!("│  Welcome to claudectl.                                          │");
    eprintln!("│                                                                 │");
    eprintln!("│  You haven't onboarded yet — the dashboard will be empty until  │");
    eprintln!("│  Claude Code hooks are installed. Quit and run one of:          │");
    eprintln!("│                                                                 │");
    eprintln!("│    claudectl init        Interactive 5-phase wizard (preferred) │");
    eprintln!("│    claudectl --demo      Explore with fake sessions             │");
    eprintln!("│                                                                 │");
    eprintln!("│  Silence this with CLAUDECTL_SKIP_FIRST_RUN=1.                  │");
    eprintln!("└─────────────────────────────────────────────────────────────────┘");
    eprintln!();
    // Tiny delay so the user actually reads the banner before the TUI
    // grabs the alt-screen and hides it.
    std::thread::sleep(std::time::Duration::from_millis(800));
}

fn run_main(cli: Cli) -> io::Result<()> {
    // Initialize diagnostic logger if --log is set
    if let Some(ref log_path) = cli.log {
        if let Err(e) = logger::init(log_path) {
            eprintln!("Warning: could not open log file {log_path}: {e}");
        }
    }

    // Load config from files, then let CLI flags override
    let mut cfg = config::Config::load();

    // CLI flags override config file values (only override if explicitly set)
    if cli.interval != 2000 {
        cfg.interval = cli.interval;
    }
    if cli.notify {
        cfg.notify = true;
    }
    if cli.debug {
        cfg.debug = true;
    }
    if cli.budget.is_some() {
        cfg.budget = cli.budget;
    }
    if cli.kill_on_budget {
        cfg.kill_on_budget = true;
    }
    if cli.webhook.is_some() {
        cfg.webhook = cli.webhook.clone();
    }
    if cli.webhook_on.is_some() {
        cfg.webhook_on = cli.webhook_on.as_deref().map(|s| {
            s.split(',')
                .map(|t| t.trim().to_string())
                .collect::<Vec<_>>()
        });
    }

    // Brain CLI overrides
    if cli.brain {
        let brain = cfg.brain.get_or_insert_with(config::BrainConfig::default);
        brain.enabled = true;
        if cli.auto_run {
            brain.auto_mode = true;
        }
        if let Some(ref endpoint) = cli.url {
            brain.endpoint = endpoint.clone();
        }
        if let Some(ref model) = cli.brain_model {
            brain.model = model.clone();
        }
    }

    models::set_overrides(cfg.model_overrides.clone());
    let filters = ViewFilters {
        status_filter: commands::parse_status_filter(cli.filter_status.as_deref())?,
        focus_filter: commands::parse_focus_filter(cli.focus.as_deref())?,
        search: cli.search.clone().unwrap_or_default(),
    };

    // Load event hooks from config
    let hook_registry = config::load_hooks();

    if cli.config {
        cfg.print_resolved();
        return Ok(());
    }

    if cli.config_template {
        config::Config::print_template();
        return Ok(());
    }

    if cli.config_validate {
        return commands::validate_config();
    }

    if cli.config_init {
        return commands::write_config_init();
    }

    if cli.hooks {
        hook_registry.print_list();
        return Ok(());
    }

    if cli.doctor {
        eprintln!(
            "note: `--doctor` is deprecated. Use `claudectl doctor` for the new \
             structured checklist (PATH + hooks + plugin + brain + bus + sessions + \
             terminal). The legacy report follows below."
        );
        return commands::print_doctor();
    }

    if cli.init {
        eprintln!(
            "note: `--init` is deprecated and will be removed in a future release. \
             Use `claudectl init` for the full onboarding wizard, or this flag for \
             the hook-only install."
        );
        let project = cli.scope == "project";
        return init::run_init(project, cli.dry_run);
    }

    if cli.uninstall {
        eprintln!(
            "note: `--uninstall` is deprecated and will be removed in a future release. \
             Use `claudectl init --remove` instead."
        );
        let project = cli.scope == "project";
        return init::run_uninit(project);
    }

    if cli.brain_prompts {
        println!("Brain Prompt Templates");
        println!("======================");
        for (name, source) in brain::prompts::list_prompts() {
            println!("  {name}: {source}");
        }
        println!();
        println!("Override: create ~/.claudectl/brain/prompts/<name>.md");
        return Ok(());
    }

    if cli.brain_eval {
        let brain_cfg = cfg.brain.clone().unwrap_or_default();
        println!("Loading eval scenarios...");
        let scenarios = brain::evals::load_scenarios();
        println!(
            "Running {} scenarios against {}...",
            scenarios.len(),
            brain_cfg.endpoint
        );
        println!();
        let results = brain::evals::run_evals(&brain_cfg, &scenarios);
        brain::evals::print_results(&results);
        return Ok(());
    }

    if let Some(ref subcommand) = cli.brain_stats {
        brain::metrics::dispatch(subcommand);
        return Ok(());
    }

    if let Some(ref id) = cli.brain_mark_canonical {
        match brain::review::mark_by_id(id, None) {
            Ok(()) => {
                println!("Marked decision {id} as canonical.");
                return Ok(());
            }
            Err(e) => {
                eprintln!("Could not mark decision {id} canonical: {e}");
                std::process::exit(1);
            }
        }
    }

    if let Some(ref maybe_arg) = cli.brain_review {
        match maybe_arg.as_deref() {
            Some("list") | Some("queue") | Some("print") => {
                brain::review::print_queue();
            }
            _ => {
                brain::review::run_interactive();
            }
        }
        return Ok(());
    }

    if let Some(ref command) = cli.command {
        match command {
            #[cfg(feature = "relay")]
            Command::Relay { command } => return relay::cli::dispatch_command(command, cli.json),

            #[cfg(feature = "hive")]
            Command::Hive { command } => return hive::cli::dispatch_command(command, cli.json),

            #[cfg(feature = "coord")]
            Command::Coord { command } => return coord::cli::dispatch_command(command, cli.json),

            #[cfg(feature = "bus")]
            Command::Bus { command } => return bus::cli::dispatch_command(command, cli.json),

            #[cfg(feature = "coord")]
            Command::Ingest { hook } => {
                let code = ingest::run(hook)?;
                std::process::exit(code);
            }

            Command::Init {
                check,
                reset,
                remove,
                purge,
                yes,
                plugin_only,
                upgrade,
                non_interactive,
                budget,
                skip_budget,
                brain_url,
                skip_brain,
                install_plugin,
                skip_plugin,
                bus_role,
                bus_cwd,
                skip_bus,
                skip_skills,
            } => {
                if *check {
                    return init::run_check();
                }
                if *reset {
                    return init::run_reset();
                }
                if *remove {
                    return init::run_remove();
                }
                if *purge {
                    return init::run_purge(*yes);
                }
                if *upgrade {
                    // #327 — re-sync after `brew upgrade`. Hooks +
                    // plugin + DB migrations + marker version, with a
                    // per-step report.
                    return init::run_upgrade();
                }
                if *plugin_only {
                    // #325 — install just the embedded plugin + hook
                    // entries. The other four wizard phases stay where
                    // the previous run left them (no marker rewrite).
                    return init::phases::install_plugin_now();
                }
                if *non_interactive {
                    let install_plugin_opt = if *skip_plugin {
                        Some(false)
                    } else if *install_plugin {
                        Some(true)
                    } else {
                        None
                    };
                    let answers = init::phases::Answers {
                        budget_weekly_usd: *budget,
                        skip_budget: *skip_budget,
                        brain_url: brain_url.clone(),
                        skip_brain: *skip_brain,
                        install_plugin: install_plugin_opt,
                        bus_role: bus_role.clone(),
                        bus_cwd: bus_cwd.as_ref().map(std::path::PathBuf::from),
                        skip_bus: *skip_bus,
                        skip_skills: *skip_skills,
                    };
                    return init::run_non_interactive(&answers);
                }
                return init::run_wizard();
            }

            Command::Completions { shell } => {
                let mut cmd = Cli::command();
                let name = cmd.get_name().to_string();
                clap_complete::generate(*shell, &mut cmd, name, &mut io::stdout());
                return Ok(());
            }

            Command::Man => {
                let cmd = Cli::command();
                clap_mangen::Man::new(cmd)
                    .render(&mut io::stdout())
                    .map_err(io::Error::other)?;
                return Ok(());
            }

            Command::Doctor { json } => {
                let checks = doctor::run_all_checks();
                if *json {
                    println!("{}", doctor::render_checks_json(&checks)?);
                } else {
                    print!("{}", doctor::render_checks(&checks));
                }
                let code = doctor::exit_code(&checks);
                if code != 0 {
                    // Use a clean process exit so the caller (CI script,
                    // shell pipeline) sees a non-zero status without us
                    // printing a backtrace.
                    std::process::exit(code);
                }
                return Ok(());
            }
        }
    }

    if cli.brain_query {
        return commands::run_brain_query(&cfg, &cli);
    }

    if cli.record_outcome {
        return commands::run_record_outcome(&cli);
    }

    if cli.reap_outcomes {
        return commands::run_reap_outcomes(&cli);
    }

    if cli.brain_outcomes {
        return commands::run_brain_outcomes(&cli);
    }

    if cli.brain_baseline {
        return commands::run_brain_baseline(&cli);
    }

    if let Some(ref mode) = cli.mode {
        return commands::run_brain_mode(mode);
    }

    if let Some(ref insights_arg) = cli.insights {
        return commands::run_insights(&cfg, &cli, insights_arg);
    }

    if cli.brain_garden {
        return commands::run_brain_garden(&cli);
    }

    if cli.brain_briefing {
        return commands::run_brain_briefing(&cli);
    }

    if let Some(ref prompt) = cli.decompose {
        let brain_cfg = cfg.brain.clone().unwrap_or_default();
        if prompt.len() < 200 {
            println!(
                "Prompt is short ({} chars) — decomposition works best with larger, multi-part prompts.",
                prompt.len()
            );
        }
        let cwd = std::env::current_dir()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| ".".into());
        let max_tasks = brain_cfg.max_sessions.min(6);
        eprintln!("Analyzing prompt for decomposition...");
        match brain::client::decompose_prompt(&brain_cfg, prompt, &cwd, max_tasks) {
            Ok(result) => {
                if result.decomposable {
                    let task_file = orchestrator::decomposition_to_task_file(result.tasks, &cwd);
                    let json = serde_json::to_string_pretty(&task_file)
                        .unwrap_or_else(|e| format!("JSON error: {e}"));
                    println!("{json}");
                } else {
                    println!("Not decomposable: {}", result.reasoning);
                }
            }
            Err(e) => {
                eprintln!("Decomposition failed: {e}");
                std::process::exit(1);
            }
        }
        return Ok(());
    }

    if let Some(ref run_file) = cli.run {
        let task_file = orchestrator::load_tasks(run_file)?;
        return orchestrator::run_tasks(task_file, cli.parallel);
    }

    if cli.autopsy {
        return commands::run_autopsy(cli.session.as_deref(), cli.json);
    }

    if cli.clean {
        return commands::run_clean(cli.older_than.as_deref(), cli.finished, cli.dry_run);
    }

    if cli.history {
        history::print_history(&cli.since);
        return Ok(());
    }

    if cli.stats {
        history::print_stats(&cli.since);
        return Ok(());
    }

    if cli.new_session {
        return commands::launch_session(&cli.cwd, cli.prompt.as_deref(), cli.resume.as_deref());
    }

    if cli.summary {
        return commands::print_summary(&cli.since);
    }

    if cli.headless {
        return commands::run_headless(Duration::from_millis(cfg.interval), &cfg, cli.json);
    }

    if cli.json && !cli.watch {
        return commands::print_json(cli.demo, &filters);
    }

    if cli.list {
        return commands::print_list(cli.demo, &filters);
    }

    if cli.watch {
        return commands::run_watch(
            Duration::from_millis(cfg.interval),
            cli.json,
            &cli.format,
            &filters,
        );
    }

    let tick_rate = Duration::from_millis(cfg.interval);
    let theme_mode = theme::ThemeMode::detect(cli.theme.as_deref());
    let app_theme = theme::Theme::from_mode(theme_mode);

    // #322 — first-run nudge. If the user has neither onboarded nor
    // installed hooks, drop a hint above the TUI before launching so
    // they understand why the dashboard is going to be empty. Skipped in
    // --demo (the wizard's whole point is moot there) and when the
    // operator opts out via env.
    if !cli.demo && std::env::var("CLAUDECTL_SKIP_FIRST_RUN").is_err() && is_first_run() {
        print_first_run_banner();
    }

    if let Some(ref record_path) = cli.record {
        // Recording mode: use TeeWriter to capture exact ANSI output
        let term_size = crossterm::terminal::size().unwrap_or((120, 40));
        let mut rec = recorder::Recorder::new(record_path, term_size.0, term_size.1)?;
        let rec_ptr: *mut recorder::Recorder = &mut rec;

        enable_raw_mode()?;
        // SAFETY: rec outlives tee_writer and terminal (both dropped before rec)
        let tee_writer = unsafe { recorder::TeeWriter::new(rec_ptr) };
        execute!(io::stdout(), EnterAlternateScreen)?;
        let backend = CrosstermBackend::new(tee_writer);
        let mut terminal = Terminal::new(backend)?;

        let max_dur = cli.duration.map(Duration::from_secs);

        let result = run_tui(
            &mut terminal,
            tick_rate,
            &cfg,
            app_theme,
            hook_registry,
            cli.demo,
            &filters,
            max_dur,
        );

        disable_raw_mode()?;
        execute!(io::stdout(), LeaveAlternateScreen)?;
        terminal.show_cursor()?;

        match rec.finish() {
            Ok(()) => {
                eprintln!("Saved to {record_path}");
            }
            Err(e) => {
                // For GIF conversion failures, the error message contains instructions
                eprintln!("{e}");
            }
        }

        result
    } else {
        // Normal mode: plain stdout
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        execute!(stdout, EnterAlternateScreen)?;
        let backend = CrosstermBackend::new(stdout);
        let mut terminal = Terminal::new(backend)?;

        let max_dur = cli.duration.map(Duration::from_secs);

        let result = run_tui(
            &mut terminal,
            tick_rate,
            &cfg,
            app_theme,
            hook_registry,
            cli.demo,
            &filters,
            max_dur,
        );

        disable_raw_mode()?;
        execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
        terminal.show_cursor()?;

        result
    }
}

#[allow(clippy::too_many_arguments)]
fn run_tui<W: io::Write>(
    terminal: &mut Terminal<CrosstermBackend<W>>,
    tick_rate: Duration,
    cfg: &config::Config,
    app_theme: theme::Theme,
    hook_registry: hooks::HookRegistry,
    demo_mode: bool,
    filters: &ViewFilters,
    max_duration: Option<Duration>,
) -> io::Result<()> {
    let mut app = App::new();
    // Replace the App's default in-memory MockRuntime with a real one wired
    // to the live brain / coord / bus / discovery subsystems. App::new
    // intentionally uses a mock so its many test call sites stay parameter-
    // free; the production wiring happens here, in main.
    app.runtime = runtime::build_runtime();
    app.notify = cfg.notify;
    app.debug = cfg.debug;
    app.webhook_url = cfg.webhook.clone();
    app.webhook_filter = cfg.webhook_on.clone();
    app.budget_usd = cfg.budget;
    app.kill_on_budget = cfg.kill_on_budget;
    app.grouped_view = cfg.grouped;
    app.theme = app_theme;
    app.hooks = hook_registry;
    app.daily_limit = cfg.daily_limit;
    app.weekly_limit = cfg.weekly_limit;
    app.context_warn_threshold = cfg.context_warn_threshold;
    app.rules = cfg.rules.clone();
    app.health_thresholds = cfg.health.clone();
    app.file_conflicts_enabled = cfg.file_conflicts;
    app.auto_deny_file_conflicts = cfg.auto_deny_file_conflicts;
    app.idle_config = cfg.idle.clone();
    app.brain_config = cfg.brain.clone();
    if let Some(ref brain_cfg) = cfg.brain {
        if brain_cfg.enabled {
            if commands::check_brain_endpoint(&brain_cfg.endpoint, brain_cfg.timeout_ms) {
                app.brain_driver = Some(Box::new(runtime::LiveBrainDriver::new(
                    brain::engine::BrainEngine::new(brain_cfg.clone()),
                )));
                app.status_msg = format!(
                    "Brain: connected to {} ({})",
                    brain_cfg.endpoint, brain_cfg.model
                );
            } else {
                app.status_msg = format!(
                    "Error: Brain endpoint {} not reachable — run `claudectl --doctor` or start ollama",
                    brain_cfg.endpoint
                );
            }
        }
    }
    app.demo_mode = demo_mode;
    commands::apply_filters(&mut app, filters);

    if demo_mode {
        app.daily_limit = Some(50.0);
        app.budget_usd = Some(10.0);
        app.rules = demo::demo_rules();
        // Create a stub brain engine so the status bar can show brain suggestions
        if app.brain_driver.is_none() {
            app.brain_driver = Some(Box::new(runtime::LiveBrainDriver::new(
                brain::engine::BrainEngine::new(config::BrainConfig::default()),
            )));
        }
        // Re-refresh to replace real sessions discovered during App::new()
        app.refresh();

        // Optional: auto-open the Skills & Hive view for recording demo GIFs.
        // `CLAUDECTL_DEMO_SKILLS=1 claudectl --demo --record demo-skills.cast`.
        if std::env::var("CLAUDECTL_DEMO_SKILLS").as_deref() == Ok("1") {
            app.open_skills_overlay();
            // Seed a fake invite so the Hive tab has something to show when
            // we flip to it (don't actually shell out to relay invite).
            app.hive_last_invite = Some(app::HiveInvite {
                relay_code: "MUR7-K2F9-XQ3T".into(),
                word_phrase: "swift-otter-storm-glass-meadow".into(),
                invite_link: "cctl://share?id=demo-mbp-a1b2&addr=192.168.1.42:9847&psk=...".into(),
            });
            app.hive_identity = Some("demo-mbp-a1b2".into());
            app.hive_known_peers = vec![
                ("alice-mbp-f3a1".into(), Some("192.168.1.17:9847".into())),
                ("ci-runner-9d1e".into(), Some("10.4.0.23:9847".into())),
            ];
        }
    }

    let mut last_tick = Instant::now();
    let tui_start = Instant::now();
    let mut sess_recs: std::collections::HashMap<u32, session_recorder::SessionRecorder> =
        std::collections::HashMap::new();
    let term_size = crossterm::terminal::size().unwrap_or((120, 40));

    loop {
        // Auto-quit after --duration seconds (graceful exit, flushes recordings)
        if let Some(max) = max_duration {
            if tui_start.elapsed() >= max {
                for (_, rec) in sess_recs.iter_mut() {
                    let _ = rec.finish();
                }
                if let Some(ref hl) = app.demo_highlight {
                    hl.cleanup();
                }
                return Ok(());
            }
        }
        terminal.draw(|frame| {
            let area = frame.area();

            // Full-screen mode: Skills & Hive takes over the entire frame.
            if app.show_skills {
                ui::skills::render_skills_screen(frame, area, &app);
                return;
            }

            // Full-screen mode: Brain Review (scorecard + review queue).
            if app.show_brain {
                brain_screen::render_brain_screen(frame, area, &app);
                return;
            }

            #[cfg(feature = "relay")]
            let main_area = if app.show_peers_panel {
                let chunks = ratatui::layout::Layout::default()
                    .direction(ratatui::layout::Direction::Vertical)
                    .constraints([
                        ratatui::layout::Constraint::Min(5),
                        ratatui::layout::Constraint::Length(
                            (app.relay_peers.len() as u16 + 2).min(8),
                        ),
                    ])
                    .split(area);
                ui::peers::render_peers_panel(frame, chunks[1], &app.relay_peers, &app.theme);
                chunks[0]
            } else {
                area
            };

            #[cfg(not(feature = "relay"))]
            let main_area = area;

            ui::table::render(frame, main_area, &app);
        })?;

        let timeout = tick_rate
            .checked_sub(last_tick.elapsed())
            .unwrap_or_else(|| Duration::from_secs(0));

        if event::poll(timeout)? {
            if let Event::Key(key) = event::read()? {
                if !app.handle_key(key) {
                    // Finish all session recordings on quit
                    for (_, rec) in sess_recs.iter_mut() {
                        let _ = rec.finish();
                    }
                    if let Some(ref hl) = app.demo_highlight {
                        hl.cleanup();
                    }
                    return Ok(());
                }
            }
        }

        if last_tick.elapsed() >= tick_rate {
            app.tick();
            last_tick = Instant::now();

            // Start recorders for newly added recordings
            for (pid, path) in &app.session_recordings {
                if sess_recs.contains_key(pid) {
                    continue;
                }
                if let Some(session) = app.sessions.iter().find(|s| s.pid == *pid) {
                    if let Some(ref jsonl) = session.jsonl_path {
                        let name = session.display_name();
                        match session_recorder::SessionRecorder::new(
                            jsonl,
                            path,
                            name,
                            term_size.0,
                            term_size.1,
                        ) {
                            Ok(r) => {
                                sess_recs.insert(*pid, r);
                            }
                            Err(e) => {
                                app.status_msg = format!("Record error: {e}");
                            }
                        }
                    }
                }
            }

            // Poll all active recorders
            for (_, rec) in sess_recs.iter_mut() {
                let _ = rec.poll();
            }

            // Finish recorders that were removed from app.session_recordings
            let stopped: Vec<u32> = sess_recs
                .keys()
                .filter(|pid| !app.session_recordings.contains_key(pid))
                .copied()
                .collect();
            for pid in stopped {
                if let Some(mut rec) = sess_recs.remove(&pid) {
                    match rec.finish() {
                        Ok(()) => {}
                        Err(e) => {
                            app.status_msg = format!("{e}");
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod first_run_tests {
    use super::*;
    use std::fs;
    use std::sync::Mutex;

    // is_first_run reads HOME and the filesystem; serialize so concurrent
    // tests don't clobber each other when they set HOME.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn set_home(p: &std::path::Path) {
        // Cargo's test harness shares a process; reset HOME after each test
        // by calling this with the original value (we just leak temp dirs
        // since they're under /tmp anyway).
        // SAFETY: tests are serialized via ENV_LOCK above; nothing else
        // here races on env reads inside the lock window.
        unsafe { std::env::set_var("HOME", p) };
    }

    #[test]
    fn first_run_when_neither_marker_nor_hooks_present() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        set_home(tmp.path());
        assert!(is_first_run(), "fresh home should be first-run");
    }

    #[test]
    fn not_first_run_when_onboarding_marker_present() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir_all(tmp.path().join(".claudectl")).unwrap();
        fs::write(tmp.path().join(".claudectl").join("onboarding.json"), "{}").unwrap();
        set_home(tmp.path());
        assert!(
            !is_first_run(),
            "onboarding marker present should suppress first-run"
        );
    }

    #[test]
    fn not_first_run_when_settings_mentions_claudectl() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir_all(tmp.path().join(".claude")).unwrap();
        fs::write(
            tmp.path().join(".claude").join("settings.json"),
            r#"{"hooks":{"PostToolUse":[{"hooks":[{"command":"claudectl --json"}]}]}}"#,
        )
        .unwrap();
        set_home(tmp.path());
        assert!(
            !is_first_run(),
            "hook install should suppress first-run even without onboarding marker"
        );
    }

    #[test]
    fn not_first_run_when_home_missing() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        // SAFETY: serialized via ENV_LOCK; nothing else reads HOME inside
        // this critical section.
        unsafe { std::env::remove_var("HOME") };
        assert!(
            !is_first_run(),
            "no HOME should be treated as not-first-run (no nudge possible)"
        );
    }
}
