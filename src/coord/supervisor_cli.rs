// Allow dead_code: the `Drain` knob is wired through to a flag the
// reconciler reads but doesn't yet act on (it's the bridge surface PR8
// will use for graceful shutdown).
#![allow(dead_code)]
//! `claudectl supervisor` subcommand surface (#349 / RFC §10).
//!
//! Operator-facing verbs over the coord task ledger. Every command
//! reads `~/.claudectl/coord/coord.db` directly — the bus MCP server
//! isn't required for these — so an operator can poke at fleet state
//! even when the headless daemon is down.

use clap::Subcommand;
use std::io;
use std::path::PathBuf;

use super::store;
use super::tasks::{self, NewTask, TaskState};

#[derive(Debug, Subcommand)]
pub enum SupervisorCommand {
    /// Submit one or more tasks from a TOML file (`--run` alias). Each
    /// `[[task]]` block becomes a `tasks` row in PENDING. The supervisor
    /// reconciler picks them up on its next tick.
    Run {
        /// Path to `tasks.toml` (RFC §4 shape).
        file: PathBuf,
        /// Print task ids without inserting them.
        #[arg(long)]
        dry_run: bool,
    },
    /// Submit a single task inline. Useful for one-shot scripts that
    /// don't want to author a TOML file.
    Submit {
        #[arg(long)]
        name: String,
        #[arg(long)]
        cwd: String,
        #[arg(long)]
        prompt: String,
        #[arg(long)]
        role: Option<String>,
        #[arg(long)]
        model: Option<String>,
        #[arg(long)]
        budget_usd: Option<f64>,
        #[arg(long)]
        max_retries: Option<u32>,
        #[arg(long)]
        timeout_min: Option<u32>,
    },
    /// Compact task table: state, attempt count, role, age. Optional
    /// `--state` filter.
    Status {
        #[arg(long)]
        state: Option<String>,
    },
    /// Detailed view of one task: every transition + every verifier
    /// verdict.
    Logs { task_id: String },
    /// Move a task to CANCELLED. Idempotent — already-terminal tasks
    /// are left alone.
    Cancel { task_id: String },
    /// Set a "drain" marker so the reconciler stops issuing new
    /// assignments while keeping running tasks alive. The marker is a
    /// sentinel file at `~/.claudectl/coord/drain`. Remove with
    /// `claudectl supervisor undrain`.
    Drain,
    /// Clear the drain marker so the reconciler resumes new
    /// assignments.
    Undrain,
}

pub fn dispatch(cmd: &SupervisorCommand) -> io::Result<()> {
    match cmd {
        SupervisorCommand::Run { file, dry_run } => run_from_file(file, *dry_run),
        SupervisorCommand::Submit {
            name,
            cwd,
            prompt,
            role,
            model,
            budget_usd,
            max_retries,
            timeout_min,
        } => submit_inline(
            name,
            cwd,
            prompt,
            role.as_deref(),
            model.as_deref(),
            *budget_usd,
            *max_retries,
            *timeout_min,
        ),
        SupervisorCommand::Status { state } => render_status(state.as_deref()),
        SupervisorCommand::Logs { task_id } => render_logs(task_id),
        SupervisorCommand::Cancel { task_id } => cancel_task(task_id),
        SupervisorCommand::Drain => set_drain(true),
        SupervisorCommand::Undrain => set_drain(false),
    }
}

#[allow(clippy::too_many_arguments)]
fn submit_inline(
    name: &str,
    cwd: &str,
    prompt: &str,
    role: Option<&str>,
    model: Option<&str>,
    budget_usd: Option<f64>,
    max_retries: Option<u32>,
    timeout_min: Option<u32>,
) -> io::Result<()> {
    let conn = store::open().map_err(io::Error::other)?;
    let new_task = NewTask {
        name: name.into(),
        role: role.map(String::from),
        cwd: cwd.into(),
        prompt: prompt.into(),
        model: model.map(String::from),
        budget_usd,
        max_retries,
        timeout_min,
        depends_on: vec![],
        policy: None,
        verifiers: vec![],
    };
    let id = tasks::insert_task(&conn, &new_task).map_err(io::Error::other)?;
    println!("submitted {id}");
    Ok(())
}

/// Parse a `tasks.toml` file and insert each `[[task]]` block. The
/// schema mirrors RFC §4 and accepts a `verifiers` array on each task.
fn run_from_file(path: &PathBuf, dry_run: bool) -> io::Result<()> {
    let body = std::fs::read_to_string(path)?;
    let parsed: TaskFile = toml_parse(&body).map_err(io::Error::other)?;
    if dry_run {
        for task in &parsed.task {
            println!("[dry-run] would submit: {}", task.name);
        }
        return Ok(());
    }
    let conn = store::open().map_err(io::Error::other)?;
    let mut inserted = 0usize;
    for entry in &parsed.task {
        let new_task = NewTask {
            name: entry.name.clone(),
            role: entry.role.clone(),
            cwd: entry.cwd.clone(),
            prompt: entry.prompt.clone(),
            model: entry.model.clone(),
            budget_usd: entry.budget_usd,
            max_retries: entry.max_retries,
            timeout_min: entry.timeout_min,
            depends_on: entry.depends_on.clone().unwrap_or_default(),
            policy: None,
            verifiers: entry.verify.clone().unwrap_or_default(),
        };
        let id = tasks::insert_task(&conn, &new_task).map_err(io::Error::other)?;
        println!("submitted {id} ({})", entry.name);
        inserted += 1;
    }
    println!("{inserted} task(s) submitted");
    Ok(())
}

fn render_status(filter: Option<&str>) -> io::Result<()> {
    let conn = store::open().map_err(io::Error::other)?;
    let state = filter.and_then(TaskState::parse);
    let rows = tasks::list_tasks(&conn, state).map_err(io::Error::other)?;
    if rows.is_empty() {
        println!("(no tasks)");
        return Ok(());
    }
    for row in rows {
        let attempts = tasks::attempt_count(&conn, &row.id).unwrap_or(0);
        println!(
            "{id:<28}  {state:<10}  role={role}  attempts={attempts}  cwd={cwd}",
            id = row.id,
            state = row.state.as_str(),
            role = row.role.as_deref().unwrap_or("-"),
            cwd = row.cwd,
        );
    }
    Ok(())
}

fn render_logs(task_id: &str) -> io::Result<()> {
    let conn = store::open().map_err(io::Error::other)?;
    let task = tasks::get_task(&conn, task_id)
        .map_err(io::Error::other)?
        .ok_or_else(|| io::Error::other(format!("task {task_id} not found")))?;
    println!(
        "task {id}  name={name}  state={state}",
        id = task.id,
        name = task.name,
        state = task.state.as_str()
    );
    println!("  cwd={}", task.cwd);
    println!("  prompt={}", task.prompt);
    println!("transitions:");
    let trans = tasks::list_transitions(&conn, task_id).map_err(io::Error::other)?;
    for (from, to, cause, at) in trans {
        println!("  {at}  {from} → {to}  ({cause})");
    }
    Ok(())
}

fn cancel_task(task_id: &str) -> io::Result<()> {
    let mut conn = store::open().map_err(io::Error::other)?;
    let task = tasks::get_task(&conn, task_id)
        .map_err(io::Error::other)?
        .ok_or_else(|| io::Error::other(format!("task {task_id} not found")))?;
    if task.state.is_terminal() {
        println!("{task_id} already terminal ({})", task.state.as_str());
        return Ok(());
    }
    tasks::transition(
        &mut conn,
        task_id,
        task.state,
        TaskState::Cancelled,
        "operator-cancel",
    )
    .map_err(io::Error::other)?;
    println!("cancelled {task_id}");
    Ok(())
}

pub fn drain_marker_path() -> PathBuf {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    home.join(".claudectl").join("coord").join("drain")
}

pub fn is_draining() -> bool {
    drain_marker_path().exists()
}

fn set_drain(enabled: bool) -> io::Result<()> {
    let path = drain_marker_path();
    if enabled {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, b"draining\n")?;
        println!("drain marker set at {}", path.display());
    } else {
        match std::fs::remove_file(&path) {
            Ok(()) => println!("drain marker cleared"),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                println!("(no drain marker)");
            }
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

// ---------- Minimal TOML decoder for `tasks.toml` ----------------------------

/// Toml parsing target. Lives here rather than as a re-export so the
/// `tasks.toml` schema stays under the supervisor's control instead of
/// drifting with whatever upstream defines.
#[derive(Debug, serde::Deserialize)]
struct TaskFile {
    task: Vec<TaskEntry>,
}

#[derive(Debug, serde::Deserialize)]
struct TaskEntry {
    name: String,
    cwd: String,
    prompt: String,
    #[serde(default)]
    role: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    budget_usd: Option<f64>,
    #[serde(default)]
    max_retries: Option<u32>,
    #[serde(default)]
    timeout_min: Option<u32>,
    #[serde(default)]
    depends_on: Option<Vec<String>>,
    #[serde(default)]
    verify: Option<Vec<super::verify::Verifier>>,
}

/// Tiny hand-rolled TOML reader to avoid pulling the `toml` crate just
/// for the supervisor CLI. Supports the subset RFC §4 declares:
/// repeated `[[task]]` headers with key=value entries on bare values
/// (strings, numbers) plus inline `[[task.verify]]` blocks.
///
/// For anything more elaborate (escaped strings with newlines, nested
/// inline tables) the caller should hand-author JSON via the `submit`
/// verb instead. The intent of `tasks.toml` is readable per-task
/// declarations, not arbitrary TOML.
fn toml_parse(input: &str) -> Result<TaskFile, String> {
    let mut tasks: Vec<TaskEntry> = Vec::new();
    let mut current: Option<TaskEntry> = None;
    let mut current_verify: Vec<super::verify::Verifier> = Vec::new();
    let mut verify_open = false;
    let mut verify_buf: Vec<(String, String)> = Vec::new();

    fn finalize_verify(
        verify_buf: &[(String, String)],
        out: &mut Vec<super::verify::Verifier>,
    ) -> Result<(), String> {
        if verify_buf.is_empty() {
            return Ok(());
        }
        let mut map: std::collections::HashMap<&str, &str> = std::collections::HashMap::new();
        for (k, v) in verify_buf {
            map.insert(k.as_str(), v.as_str());
        }
        if let Some(cmd) = map.get("run") {
            out.push(super::verify::Verifier::Run {
                command: cmd.to_string(),
            });
        } else if let Some(prompt) = map.get("brain") {
            out.push(super::verify::Verifier::Brain {
                prompt: prompt.to_string(),
            });
        } else if let Some(prompt) = map.get("agent") {
            out.push(super::verify::Verifier::Agent {
                prompt: prompt.to_string(),
                model: map.get("model").map(|s| s.to_string()),
                budget_usd: map.get("budget_usd").and_then(|s| s.parse::<f64>().ok()),
            });
        } else {
            return Err("[[task.verify]] block missing run/brain/agent key".into());
        }
        Ok(())
    }

    for raw in input.lines() {
        let line = raw.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        if line == "[[task]]" {
            if verify_open {
                finalize_verify(&verify_buf, &mut current_verify)?;
                verify_buf.clear();
                verify_open = false;
            }
            if let Some(mut t) = current.take() {
                if !current_verify.is_empty() {
                    t.verify = Some(std::mem::take(&mut current_verify));
                }
                tasks.push(t);
            }
            current = Some(TaskEntry {
                name: String::new(),
                cwd: String::new(),
                prompt: String::new(),
                role: None,
                model: None,
                budget_usd: None,
                max_retries: None,
                timeout_min: None,
                depends_on: None,
                verify: None,
            });
            continue;
        }
        if line == "[[task.verify]]" {
            if verify_open {
                finalize_verify(&verify_buf, &mut current_verify)?;
                verify_buf.clear();
            }
            verify_open = true;
            continue;
        }
        // key = value
        let Some((k, v)) = line.split_once('=') else {
            return Err(format!("unparseable TOML line: {line}"));
        };
        let key = k.trim().to_string();
        let val = v.trim().trim_matches('"').to_string();
        if verify_open {
            verify_buf.push((key, val));
            continue;
        }
        let Some(t) = current.as_mut() else {
            return Err(format!("key={key} outside any [[task]] block"));
        };
        match key.as_str() {
            "name" => t.name = val,
            "cwd" => t.cwd = val,
            "prompt" => t.prompt = val,
            "role" => t.role = Some(val),
            "model" => t.model = Some(val),
            "budget_usd" => t.budget_usd = val.parse().ok(),
            "max_retries" => t.max_retries = val.parse().ok(),
            "timeout_min" => t.timeout_min = val.parse().ok(),
            "depends_on" => {
                let parts: Vec<String> = val
                    .trim_matches(|c: char| c == '[' || c == ']')
                    .split(',')
                    .map(|s| s.trim().trim_matches('"').to_string())
                    .filter(|s| !s.is_empty())
                    .collect();
                t.depends_on = Some(parts);
            }
            other => return Err(format!("unknown key in [[task]]: {other}")),
        }
    }
    if verify_open {
        finalize_verify(&verify_buf, &mut current_verify)?;
    }
    if let Some(mut t) = current.take() {
        if !current_verify.is_empty() {
            t.verify = Some(current_verify);
        }
        tasks.push(t);
    }
    Ok(TaskFile { task: tasks })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn toml_parse_handles_full_task_block() {
        let body = r#"
[[task]]
name = "auth-middleware"
cwd = "./services"
prompt = "Add JWT auth middleware to all API routes"
role = "backend"
model = "sonnet"
budget_usd = 3.00
max_retries = 2
timeout_min = 45

  [[task.verify]]
  run = "cargo test --all-targets"

  [[task.verify]]
  brain = "Review the diff for auth-coverage gaps. PASS or FAIL with reasons."

  [[task.verify]]
  agent = "Adversarial review: find a request that bypasses the middleware."
  model = "haiku"
  budget_usd = 0.25
"#;
        let parsed = toml_parse(body).expect("parse");
        assert_eq!(parsed.task.len(), 1);
        let t = &parsed.task[0];
        assert_eq!(t.name, "auth-middleware");
        assert_eq!(t.cwd, "./services");
        assert_eq!(t.role.as_deref(), Some("backend"));
        assert_eq!(t.budget_usd, Some(3.0));
        let verify = t.verify.as_ref().expect("verifiers");
        assert_eq!(verify.len(), 3);
        match &verify[2] {
            super::super::verify::Verifier::Agent {
                model, budget_usd, ..
            } => {
                assert_eq!(model.as_deref(), Some("haiku"));
                assert_eq!(*budget_usd, Some(0.25));
            }
            other => panic!("expected Agent, got {other:?}"),
        }
    }

    #[test]
    fn toml_parse_multiple_tasks() {
        let body = r#"
[[task]]
name = "first"
cwd = "/a"
prompt = "do a"

[[task]]
name = "second"
cwd = "/b"
prompt = "do b"
"#;
        let parsed = toml_parse(body).expect("parse");
        assert_eq!(parsed.task.len(), 2);
        assert_eq!(parsed.task[0].name, "first");
        assert_eq!(parsed.task[1].name, "second");
    }

    #[test]
    fn toml_parse_rejects_unknown_key() {
        let body = r#"
[[task]]
name = "x"
cwd = "/x"
prompt = "do"
flavour = "vanilla"
"#;
        assert!(toml_parse(body).is_err());
    }
}
