//! `claudectl bus …` subcommand.
//!
//! Exposes the bus surface to the shell:
//!
//! * `claudectl bus stdio` — run the MCP server on stdin/stdout. This is what
//!   the Claude Code plugin registers in `.mcp.json`.
//! * `claudectl bus role bind <NAME> <CWD>` — register or update a role.
//! * `claudectl bus role list` — list registered roles.
//! * `claudectl bus send <ROLE> <BODY>` — quick directed send for ops debug.
//! * `claudectl bus inbox <ROLE>` — drain a role's mailbox (CLI form of the
//!   `read_inbox` MCP tool).

use std::path::PathBuf;

use clap::Subcommand;
use serde::Serialize;

use super::mcp;
use super::policy::{self, DEFAULT_MAX_BODY_BYTES};
use super::roles::{self, RoleResolution};
use super::stop_hook;
use super::store::{self, MessageRow};
use super::suggest;

#[derive(Subcommand)]
pub enum BusCommand {
    /// Run the agent-bus MCP server on stdio (used by the Claude Code plugin).
    Stdio,
    /// Manage roles.
    Role {
        #[command(subcommand)]
        command: RoleCommand,
    },
    /// Send a directed message from the CLI (debug helper).
    Send {
        /// Recipient role.
        to: String,
        /// Message body. A leading "/" is neutralized at the boundary.
        body: String,
        /// Subject (defaults to "task.created").
        #[arg(long, default_value = "task.created")]
        subject: String,
        /// Message type: task | result | question | status | handoff.
        #[arg(long, default_value = "task")]
        msg_type: String,
        /// Sender role label.
        #[arg(long)]
        from: Option<String>,
        /// Priority: high | normal | low.
        #[arg(long, default_value = "normal")]
        priority: String,
    },
    /// Drain pending directed messages for a role.
    Inbox {
        /// Role to drain. Defaults to cwd inference.
        #[arg(long)]
        role: Option<String>,
        /// Emit a machine-readable JSON payload instead of human text. Used by
        /// the Stop hook (Trigger A); soft-fails on unbound/ambiguous cwds so
        /// the hook never blocks a session.
        #[arg(long)]
        json: bool,
    },
    /// Report which role the current cwd resolves to.
    Whoami {
        #[arg(long)]
        role: Option<String>,
        /// Emit a machine-readable JSON payload instead of human text.
        #[arg(long)]
        json: bool,
    },
    /// Stop-hook driver for the Claude Code plugin (Trigger A). Drains the
    /// caller's mailbox; when there's mail, emits Stop-hook output so the
    /// agent picks the work up in-turn. Silent + exit 0 in every other case
    /// so a missing role / empty inbox / unbound cwd never blocks a session.
    StopHook,
}

#[derive(Subcommand)]
pub enum RoleCommand {
    /// Bind (create or update) a role to a cwd selector and/or process id.
    Bind {
        /// Role name.
        name: String,
        /// cwd selector (literal path prefix or trailing `*` wildcard).
        /// Optional when `--self` is set; otherwise required.
        cwd: Option<String>,
        /// Optional session_id to bind initially.
        #[arg(long)]
        session_id: Option<String>,
        /// Process ID to bind this role to (#307). When set, the resolver
        /// prefers this binding over cwd-inference for any caller whose
        /// ancestor chain contains this pid.
        #[arg(long, conflicts_with = "self_bind")]
        pid: Option<u32>,
        /// Bind from inside a Claude Code session: auto-detect Claude's pid
        /// by walking the caller's ancestor chain, and capture the current
        /// cwd. Used by the plugin `/bind` slash command (#310).
        #[arg(long = "self", conflicts_with = "cwd")]
        self_bind: bool,
    },
    /// List registered roles.
    List,
    /// Suggest role names by analyzing a session's transcript (#309).
    /// Pure analysis — never writes a binding, never queries the LLM.
    Suggest {
        /// PID of the session to analyze. When omitted, walks the caller's
        /// ancestor chain to find a Claude process (same logic as `--self`).
        #[arg(long)]
        pid: Option<u32>,
        /// Maximum suggestions to return, ranked by score descending.
        #[arg(long, default_value_t = 3)]
        top: usize,
        /// Emit JSON instead of human text. Used by the TUI / plugin to
        /// prefill the bind prompt.
        #[arg(long)]
        json: bool,
    },
}

pub fn dispatch_command(cmd: &BusCommand, _json_mode: bool) -> std::io::Result<()> {
    dispatch(cmd).map_err(std::io::Error::other)
}

pub fn dispatch(cmd: &BusCommand) -> Result<(), String> {
    match cmd {
        BusCommand::Stdio => mcp::run_stdio(),
        BusCommand::Role { command } => dispatch_role(command),
        BusCommand::Send {
            to,
            body,
            subject,
            msg_type,
            from,
            priority,
        } => dispatch_send(to, body, subject, msg_type, from.as_deref(), priority),
        BusCommand::Inbox { role, json } => dispatch_inbox(role.as_deref(), *json),
        BusCommand::Whoami { role, json } => dispatch_whoami(role.as_deref(), *json),
        BusCommand::StopHook => dispatch_stop_hook(),
    }
}

fn dispatch_role(cmd: &RoleCommand) -> Result<(), String> {
    let conn = store::open()?;
    match cmd {
        RoleCommand::Bind {
            name,
            cwd,
            session_id,
            pid,
            self_bind,
        } => {
            // Resolve (cwd, pid) up front. --self auto-detects both from the
            // caller's process tree; the explicit form takes the positional
            // CWD and any --pid override.
            let (cwd_resolved, pid_resolved) = if *self_bind {
                let detected = roles::find_claude_ancestor_pid().ok_or_else(|| {
                    "--self: could not find a Claude Code process in the ancestor chain. \
                     Run this from inside a Claude session, or pass --pid explicitly."
                        .to_string()
                })?;
                let cwd = std::env::current_dir()
                    .map_err(|e| format!("--self: read current dir: {e}"))?
                    .to_string_lossy()
                    .into_owned();
                (cwd, Some(detected))
            } else {
                let cwd = cwd
                    .clone()
                    .ok_or_else(|| "cwd argument required (or pass --self)".to_string())?;
                (cwd, *pid)
            };
            store::upsert_role(
                &conn,
                name,
                &cwd_resolved,
                session_id.as_deref(),
                pid_resolved,
            )?;
            match pid_resolved {
                Some(p) => println!("bound role {name} -> {cwd_resolved} (pid={p})"),
                None => println!("bound role {name} -> {cwd_resolved}"),
            }
            Ok(())
        }
        RoleCommand::List => {
            let rows = store::list_roles(&conn)?;
            if rows.is_empty() {
                println!("(no roles bound — run `claudectl bus role bind <name> <cwd>`)");
                return Ok(());
            }
            for r in rows {
                println!(
                    "{name:<20} {sel:<40} pid={pid} last_seen={ts} session={sess}",
                    name = r.role,
                    sel = r.cwd_selector,
                    pid = r.pid.map(|p| p.to_string()).unwrap_or_else(|| "-".into()),
                    ts = r.last_seen,
                    sess = r.last_session_id.unwrap_or_else(|| "-".into()),
                );
            }
            Ok(())
        }
        RoleCommand::Suggest { pid, top, json } => dispatch_suggest(*pid, *top, *json),
    }
}

/// Resolve the target session by pid (or ancestor walk), read its
/// transcript and cwd, and emit ranked role-name candidates. The TUI's
/// Ctrl+R prompt and the plugin's `/bind` command will call this with
/// `--json` to prefill the bind input.
fn dispatch_suggest(pid: Option<u32>, top: usize, json: bool) -> Result<(), String> {
    use claudectl_core::discovery;

    let target_pid = match pid {
        Some(p) => p,
        None => roles::find_claude_ancestor_pid().ok_or_else(|| {
            "no --pid given and no Claude process found in ancestor chain".to_string()
        })?,
    };

    let mut sessions = discovery::scan_sessions();
    discovery::resolve_jsonl_paths(&mut sessions);
    let session = sessions
        .iter()
        .find(|s| s.pid == target_pid)
        .ok_or_else(|| format!("no running Claude session with pid {target_pid}"))?;

    let suggestions = suggest::suggest_for_session(
        session.jsonl_path.as_deref(),
        std::path::Path::new(&session.cwd),
        top,
    );

    if json {
        println!(
            "{}",
            serde_json::to_string(&suggestions).map_err(|e| format!("encode json: {e}"))?
        );
        return Ok(());
    }

    if suggestions.is_empty() {
        println!("(no role candidates inferred — session is too new or transcript empty)");
        return Ok(());
    }
    for (i, s) in suggestions.iter().enumerate() {
        println!(
            "{idx}. {name}  (score={score})",
            idx = i + 1,
            name = s.name,
            score = s.score
        );
        for reason in &s.reasons {
            println!("     · {reason}");
        }
    }
    Ok(())
}

fn dispatch_send(
    to: &str,
    body: &str,
    subject: &str,
    msg_type: &str,
    from: Option<&str>,
    priority: &str,
) -> Result<(), String> {
    policy::validate_subject(subject).map_err(|e| e.to_string())?;
    policy::validate_type(msg_type).map_err(|e| e.to_string())?;
    policy::validate_body(body, DEFAULT_MAX_BODY_BYTES).map_err(|e| e.to_string())?;
    let sanitized = policy::sanitize_body(body);
    let conn = store::open()?;
    let id = store::insert_message(
        &conn,
        subject,
        msg_type,
        from,
        Some(to),
        None,
        &sanitized,
        priority,
    )?;
    println!("queued {id} -> {to}");
    Ok(())
}

// ---------------- Inbox -----------------------------------------------------

/// Result of a single drain pass — the resolved role (if any), why the role
/// couldn't be resolved (if applicable), and the drained messages. Built once,
/// rendered as either human text or JSON.
struct InboxOutcome {
    role: Option<String>,
    note: Option<String>,
    messages: Vec<MessageRow>,
}

#[derive(Serialize)]
struct InboxOutcomeJson {
    role: Option<String>,
    note: Option<String>,
    messages: Vec<InboxMessageJson>,
}

#[derive(Serialize)]
struct InboxMessageJson {
    id: String,
    subject: String,
    #[serde(rename = "type")]
    msg_type: String,
    sender_role: Option<String>,
    thread_id: Option<String>,
    body: String,
    priority: String,
    created_at: String,
}

impl From<MessageRow> for InboxMessageJson {
    fn from(m: MessageRow) -> Self {
        Self {
            id: m.id,
            subject: m.subject,
            msg_type: m.msg_type,
            sender_role: m.sender_role,
            thread_id: m.thread_id,
            body: m.body,
            priority: m.priority,
            created_at: m.created_at,
        }
    }
}

/// Resolve the caller's role and drain its mailbox. Soft-fails: unbound and
/// ambiguous cwds produce a `note` rather than an error, so JSON callers (the
/// Stop hook) can no-op cleanly without aborting a Claude Code session.
fn fetch_inbox(role: Option<&str>) -> Result<InboxOutcome, String> {
    let mut conn = store::open()?;
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
    let resolution = roles::resolve(&conn, role, &cwd)?;
    let (role, note) = match resolution {
        RoleResolution::Resolved(r) => (Some(r.name), None),
        RoleResolution::Ambiguous { candidates } => {
            (None, Some(format!("ambiguous: {}", candidates.join(", "))))
        }
        RoleResolution::Unbound { cwd } => (None, Some(format!("unbound (cwd={cwd})"))),
    };
    let messages = if let Some(name) = role.as_deref() {
        store::drain_inbox(&mut conn, name, None)?
    } else {
        Vec::new()
    };
    Ok(InboxOutcome {
        role,
        note,
        messages,
    })
}

fn dispatch_inbox(role: Option<&str>, json: bool) -> Result<(), String> {
    let outcome = fetch_inbox(role)?;

    if json {
        let payload = InboxOutcomeJson {
            role: outcome.role,
            note: outcome.note,
            messages: outcome
                .messages
                .into_iter()
                .map(InboxMessageJson::from)
                .collect(),
        };
        println!(
            "{}",
            serde_json::to_string(&payload).map_err(|e| format!("encode json: {e}"))?
        );
        return Ok(());
    }

    // Human renderer: ambiguous / unbound are interactive errors, not silent.
    let Some(name) = outcome.role else {
        return Err(outcome.note.unwrap_or_else(|| "no role resolved".into()));
    };

    if outcome.messages.is_empty() {
        println!("(inbox empty for role {name})");
        return Ok(());
    }
    for m in outcome.messages {
        println!(
            "[{prio}] {subject} from={sender} thread={thread}\n  {body}",
            prio = m.priority,
            subject = m.subject,
            sender = m.sender_role.as_deref().unwrap_or("-"),
            thread = m.thread_id.as_deref().unwrap_or("-"),
            body = m.body.replace('\n', "\n  ")
        );
    }
    Ok(())
}

// ---------------- Whoami ----------------------------------------------------

#[derive(Serialize)]
struct WhoamiJson {
    role: Option<String>,
    cwd_selector: Option<String>,
    last_session_id: Option<String>,
    /// PID this role is bound to (#307). `None` for cwd-only bindings.
    pid: Option<u32>,
    note: Option<String>,
}

// ---------------- Stop hook -------------------------------------------------

/// Drive the Claude Code Stop-hook protocol. Silent + exit 0 on any failure
/// (missing DB, unbound role, ambiguous cwd, drain error) so the hook never
/// blocks a session because of a bus problem.
fn dispatch_stop_hook() -> Result<(), String> {
    let outcome = match fetch_inbox(None) {
        Ok(o) => o,
        Err(_) => return Ok(()),
    };
    let Some(role) = outcome.role else {
        return Ok(());
    };
    if let Some(response) = stop_hook::build_response(&role, &outcome.messages) {
        let json =
            serde_json::to_string(&response).map_err(|e| format!("encode stop-hook json: {e}"))?;
        println!("{json}");
    }
    Ok(())
}

fn dispatch_whoami(role: Option<&str>, json: bool) -> Result<(), String> {
    let conn = store::open()?;
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
    let resolution = roles::resolve(&conn, role, &cwd)?;

    if json {
        let payload = match resolution {
            RoleResolution::Resolved(r) => WhoamiJson {
                role: Some(r.name),
                cwd_selector: Some(r.cwd_selector),
                last_session_id: r.last_session_id,
                pid: r.pid,
                note: None,
            },
            RoleResolution::Ambiguous { candidates } => WhoamiJson {
                role: None,
                cwd_selector: None,
                last_session_id: None,
                pid: None,
                note: Some(format!("ambiguous: {}", candidates.join(", "))),
            },
            RoleResolution::Unbound { cwd } => WhoamiJson {
                role: None,
                cwd_selector: None,
                last_session_id: None,
                pid: None,
                note: Some(format!("unbound (cwd={cwd})")),
            },
        };
        println!(
            "{}",
            serde_json::to_string(&payload).map_err(|e| format!("encode json: {e}"))?
        );
        return Ok(());
    }

    match resolution {
        RoleResolution::Resolved(r) => {
            println!(
                "role={name} cwd_selector={sel} last_session_id={sess}",
                name = r.name,
                sel = r.cwd_selector,
                sess = r.last_session_id.unwrap_or_else(|| "-".into())
            );
        }
        RoleResolution::Ambiguous { candidates } => {
            println!("ambiguous: {}", candidates.join(", "));
        }
        RoleResolution::Unbound { cwd } => {
            println!("unbound (cwd={cwd})");
        }
    }
    Ok(())
}
