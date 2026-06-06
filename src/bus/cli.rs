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

use super::mcp;
use super::policy::{self, DEFAULT_MAX_BODY_BYTES};
use super::roles::{self, RoleResolution};
use super::store;

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
    },
    /// Report which role the current cwd resolves to.
    Whoami {
        #[arg(long)]
        role: Option<String>,
    },
}

#[derive(Subcommand)]
pub enum RoleCommand {
    /// Bind (create or update) a role to a cwd selector.
    Bind {
        /// Role name.
        name: String,
        /// cwd selector (literal path prefix or trailing `*` wildcard).
        cwd: String,
        /// Optional session_id to bind initially.
        #[arg(long)]
        session_id: Option<String>,
    },
    /// List registered roles.
    List,
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
        BusCommand::Inbox { role } => dispatch_inbox(role.as_deref()),
        BusCommand::Whoami { role } => dispatch_whoami(role.as_deref()),
    }
}

fn dispatch_role(cmd: &RoleCommand) -> Result<(), String> {
    let conn = store::open()?;
    match cmd {
        RoleCommand::Bind {
            name,
            cwd,
            session_id,
        } => {
            store::upsert_role(&conn, name, cwd, session_id.as_deref())?;
            println!("bound role {name} -> {cwd}");
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
                    "{name:<20} {sel:<40} last_seen={ts} session={sess}",
                    name = r.role,
                    sel = r.cwd_selector,
                    ts = r.last_seen,
                    sess = r.last_session_id.unwrap_or_else(|| "-".into()),
                );
            }
            Ok(())
        }
    }
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

fn dispatch_inbox(role: Option<&str>) -> Result<(), String> {
    let mut conn = store::open()?;
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
    let resolved = match roles::resolve(&conn, role, &cwd)? {
        RoleResolution::Resolved(r) => r.name,
        RoleResolution::Ambiguous { candidates } => {
            return Err(format!(
                "cwd matches multiple roles: {}. Pass --role explicitly.",
                candidates.join(", ")
            ));
        }
        RoleResolution::Unbound { cwd } => {
            return Err(format!(
                "no role bound for cwd {cwd}. Bind one with `claudectl bus role bind`."
            ));
        }
    };
    let drained = store::drain_inbox(&mut conn, &resolved, None)?;
    if drained.is_empty() {
        println!("(inbox empty for role {resolved})");
        return Ok(());
    }
    for m in drained {
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

fn dispatch_whoami(role: Option<&str>) -> Result<(), String> {
    let conn = store::open()?;
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
    match roles::resolve(&conn, role, &cwd)? {
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
