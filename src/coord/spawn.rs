//! Running a supervisor task's session (#570), and watching it.
//!
//! A task runs as `claude --print --output-format json` in its workdir: the
//! task's worktree when it has a `git_ref`, its `cwd` otherwise. Three things
//! make that safe for a daemon:
//!
//! - **The session id is chosen before the process starts** (`--session-id`),
//!   and recorded on the attempt first. A daemon that crashes between the
//!   record and the spawn finds an attempt with no pid; one that crashes after
//!   finds the pid alive and adopts it instead of spawning twice.
//! - **The process is not the daemon's child.** It starts through
//!   `sh -c '… &'`, so it's reparented and reaped by the system. A child the
//!   daemon never `wait()`s on would stay a zombie after exiting, and
//!   `kill(pid, 0)` reports a zombie as alive: the task would look RUNNING
//!   forever, and a daemon restart would lose it anyway.
//! - **Output goes to files**, not pipes: `~/.claudectl/supervisor/runs/
//!   <attempt>.{out,err}`. Nothing has to drain them, and the result JSON
//!   is still there after a daemon restart.
//!
//! The sensor reads it back: a complete result means Completed or Failed; no
//! result and a live pid means Running; otherwise Dead, after a grace period
//! for a spawn that hasn't recorded its pid yet.

use std::path::{Path, PathBuf};
use std::process::Command;

use super::supervisor::{ObservedSession, ObservedStatus, Sensors};
use super::tasks::TaskRow;
use claudectl_core::worktree;

/// How long an attempt may go without a recorded pid before it counts as
/// dead: long enough for a worktree checkout and a spawn, short enough that a
/// daemon that died mid-spawn doesn't stall the task.
pub const SPAWN_GRACE_SECS: u64 = 60;

/// Everything one spawn needs.
#[derive(Debug, Clone, PartialEq)]
pub struct SpawnRequest {
    pub task_id: String,
    pub attempt_id: String,
    pub session_id: String,
    pub workdir: PathBuf,
    pub prompt: String,
    pub permission_mode: Option<String>,
    pub model: Option<String>,
}

pub fn runs_dir() -> PathBuf {
    claudectl_core::helpers::dirs_home()
        .join(".claudectl")
        .join("supervisor")
        .join("runs")
}

pub fn out_path(attempt_id: &str) -> PathBuf {
    runs_dir().join(format!("{attempt_id}.out"))
}

pub fn err_path(attempt_id: &str) -> PathBuf {
    runs_dir().join(format!("{attempt_id}.err"))
}

/// A random v4 UUID, for `claude --session-id`, without a crate for it.
pub fn new_session_id() -> String {
    let mut b = [0u8; 16];
    let filled = std::fs::File::open("/dev/urandom")
        .and_then(|mut f| std::io::Read::read_exact(&mut f, &mut b))
        .is_ok();
    if !filled {
        // No /dev/urandom: time and pid are enough for uniqueness here.
        let t = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        b[..16].copy_from_slice(&(t ^ ((std::process::id() as u128) << 64)).to_le_bytes());
    }
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h: Vec<String> = b.iter().map(|x| format!("{x:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        h[0..4].concat(),
        h[4..6].concat(),
        h[6..8].concat(),
        h[8..10].concat(),
        h[10..16].concat()
    )
}

/// The `claude` arguments for a task's session.
pub fn claude_args(req: &SpawnRequest) -> Vec<String> {
    let mut args: Vec<String> = vec![
        "--print".into(),
        "--output-format".into(),
        "json".into(),
        "--session-id".into(),
        req.session_id.clone(),
    ];
    if let Some(m) = &req.permission_mode {
        args.push("--permission-mode".into());
        args.push(m.clone());
    }
    if let Some(m) = &req.model {
        args.push("--model".into());
        args.push(m.clone());
    }
    args.push(req.prompt.clone());
    args
}

/// Start `program args…` in `workdir`, detached, output to the attempt's
/// files. Returns the pid.
pub fn spawn_detached(
    program: &Path,
    args: &[String],
    workdir: &Path,
    out: &Path,
    err: &Path,
) -> Result<u32, String> {
    if let Some(dir) = out.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
    }
    // `sh` backgrounds the command and exits; the command is reparented and
    // reaped by the system, never left a zombie of this process.
    let script = r#"out="$1"; err="$2"; shift 2; "$@" </dev/null >"$out" 2>"$err" & echo $!"#;
    let output = Command::new("/bin/sh")
        .arg("-c")
        .arg(script)
        .arg("sh")
        .arg(out)
        .arg(err)
        .arg(program)
        .args(args)
        .current_dir(workdir)
        .output()
        .map_err(|e| format!("spawn in {}: {e}", workdir.display()))?;
    if !output.status.success() {
        return Err(format!(
            "spawn in {}: {}",
            workdir.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse::<u32>()
        .map_err(|_| "spawn did not report a pid".to_string())
}

/// Start the session a request describes.
pub fn spawn(req: &SpawnRequest) -> Result<u32, String> {
    spawn_detached(
        &claudectl_core::helpers::claude_bin(),
        &claude_args(req),
        &req.workdir,
        &out_path(&req.attempt_id),
        &err_path(&req.attempt_id),
    )
}

/// Where a task's session runs: its worktree when it names a `git_ref`
/// (created or reused), its `cwd` otherwise.
pub fn prepare_workdir(task: &TaskRow) -> Result<PathBuf, String> {
    let cwd = PathBuf::from(&task.cwd);
    let Some(git_ref) = &task.git_ref else {
        return Ok(cwd);
    };
    let branch = task
        .branch
        .clone()
        .unwrap_or_else(|| worktree::default_branch(&task.name, &task.id));
    worktree::ensure(&cwd, git_ref, &branch, &worktree::path_for(&task.id)).map(|w| w.path)
}

/// After DONE: remove the task's worktree if it holds nothing uncommitted.
/// A task without a `git_ref` ran in its `cwd`, which is never removed.
pub fn cleanup_workdir(task: &TaskRow, workdir: &Path) -> Result<worktree::Cleanup, String> {
    if task.git_ref.is_none() || workdir != worktree::path_for(&task.id) {
        return Ok(worktree::Cleanup::Missing);
    }
    worktree::cleanup(workdir)
}

/// What a finished session reported.
#[derive(Debug, Clone, PartialEq)]
pub struct RunResult {
    pub ok: bool,
    pub cost_usd: Option<f64>,
    pub summary: String,
}

/// The session's result, if it has finished writing one. `--output-format
/// json` writes a single object with `is_error` when the run ends; anything
/// short of that (empty, partial, not JSON) means it hasn't finished.
pub fn read_result(attempt_id: &str) -> Option<RunResult> {
    parse_result(&std::fs::read_to_string(out_path(attempt_id)).ok()?)
}

pub fn parse_result(text: &str) -> Option<RunResult> {
    let v: serde_json::Value = serde_json::from_str(text.trim()).ok()?;
    let is_error = v.get("is_error")?.as_bool()?;
    Some(RunResult {
        ok: !is_error,
        cost_usd: v.get("total_cost_usd").and_then(serde_json::Value::as_f64),
        summary: v
            .get("result")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .chars()
            .take(500)
            .collect(),
    })
}

pub fn pid_alive(pid: u32) -> bool {
    pid != 0 && unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
}

/// How an attempt looks right now. The result file is checked before the
/// pid, so a reused pid can't bring a finished session back to life.
pub fn observe(
    result: Option<&RunResult>,
    pid: Option<u32>,
    alive: impl Fn(u32) -> bool,
    secs_since_start: Option<u64>,
) -> ObservedStatus {
    if let Some(r) = result {
        return if r.ok {
            ObservedStatus::Completed
        } else {
            ObservedStatus::Failed
        };
    }
    match pid {
        Some(p) if alive(p) => ObservedStatus::Running,
        Some(_) => ObservedStatus::Dead,
        // Spawn in progress, or the daemon died mid-spawn: give it a moment.
        None if secs_since_start.is_some_and(|s| s < SPAWN_GRACE_SECS) => ObservedStatus::Unknown,
        None => ObservedStatus::Dead,
    }
}

/// The supervisor's live sensor: every RUNNING task's spawned attempt, and
/// the bus roster, built once per pass the first time anything asks.
#[derive(Default)]
pub struct LiveSensors {
    #[cfg_attr(not(feature = "bus"), allow(dead_code))]
    roster: std::cell::OnceCell<Vec<super::supervisor::RoleView>>,
}

impl Sensors for LiveSensors {
    fn last_hook_event_id(&self) -> i64 {
        0
    }

    fn observed_sessions(&self) -> Vec<ObservedSession> {
        let Ok(conn) = super::store::open() else {
            return Vec::new();
        };
        // On a peer (#571): what this machine's relay has heard about it.
        let remote = super::tasks::running_remote_attempts(&conn)
            .unwrap_or_default()
            .into_iter()
            .map(|a| ObservedSession {
                status: super::remote::observe(
                    super::remote::record(&a.attempt_id).as_ref(),
                    super::tasks::age_secs(&a.started_at),
                ),
                session_id: a.attempt_id,
                pid: 0,
                health_alerts: Vec::new(),
            });
        let attempts = super::tasks::running_spawned_attempts(&conn).unwrap_or_default();
        attempts
            .into_iter()
            .map(|a| {
                let result = read_result(&a.attempt_id);
                ObservedSession {
                    status: observe(
                        result.as_ref(),
                        a.pid,
                        pid_alive,
                        super::tasks::age_secs(&a.started_at),
                    ),
                    session_id: a.session_id,
                    pid: a.pid.unwrap_or(0),
                    health_alerts: Vec::new(),
                }
            })
            .chain(remote)
            .collect()
    }

    fn placement(&self) -> Vec<super::placement::PeerView> {
        super::placement::live_views()
    }

    #[cfg(feature = "bus")]
    fn roles(&self) -> Vec<super::supervisor::RoleView> {
        self.roster.get_or_init(live_roster).clone()
    }

    /// How the mailbox message stands (#605): drained, or waiting with
    /// whether a running session answers to its recipient role (the
    /// roster's liveness). No bus database, a store error or a missing
    /// message is `Unknown`: not a reason to spawn.
    #[cfg(feature = "bus")]
    fn assignment_claim(&self, message_id: &str) -> super::supervisor::Claim {
        use super::supervisor::Claim;
        if !crate::bus::store::db_exists() {
            return Claim::Unknown;
        }
        let Ok(conn) = crate::bus::store::open() else {
            return Claim::Unknown;
        };
        match crate::bus::store::message_claim(&conn, message_id) {
            Ok(Some((status, _))) if status == "delivered" => Claim::Drained,
            Ok(Some((_, recipient))) => {
                let roster = self.roster.get_or_init(live_roster);
                Claim::Waiting {
                    recipient_live: recipient
                        .is_some_and(|r| roster.iter().any(|v| v.name == r && v.live)),
                }
            }
            _ => Claim::Unknown,
        }
    }
}

/// Every bound bus role with whether anyone is home and where its sessions
/// are working (#596 follow-up). A role is live when its bound pid runs or
/// a running session answers to it the way `whoami` would inside it
/// (`roles::resolve_for_session`), which is also what lets that session
/// read the mailbox. A session file whose process has exited counts for
/// nothing. A session's focus is read from its
/// transcript, as `list_agents` does, and credited to the role it answers
/// to, but only for roles that share a selector with another live role:
/// that is the one case focus decides. Read-only: the session scan sweeps
/// nothing, and a machine without a bus database gets none created.
#[cfg(feature = "bus")]
fn live_roster() -> Vec<super::supervisor::RoleView> {
    use crate::bus::roles::{self, RoleResolution};
    use claudectl_core::paths::canonicalize_deep;
    use std::collections::{HashMap, HashSet};
    use std::path::Path;
    // A machine that never used the bus has no roles to route to, and asking
    // must not create its database.
    if !crate::bus::store::db_exists() {
        return Vec::new();
    }
    let opened = crate::bus::store::open()
        .and_then(|conn| crate::bus::store::list_roles(&conn).map(|rows| (conn, rows)));
    let (conn, rows) = match opened {
        Ok(x) => x,
        Err(e) => {
            roster_error(Some(format!("bus roles: {e}")));
            return Vec::new();
        }
    };
    // Session files outlive their process for a while; only a running
    // session keeps a role alive or lends it a focus.
    let mut sessions: Vec<_> = crate::discovery::scan_sessions_readonly()
        .into_iter()
        .filter(claudectl_core::discovery::session_running)
        .collect();
    crate::discovery::resolve_jsonl_paths(&mut sessions);
    crate::discovery::resolve_worktree_ids(&mut sessions);
    // Which role each running session answers to, exactly as `whoami`,
    // `read_inbox` and the stop hook resolve it inside that session: its
    // pid binding, else the single role bound over its cwd. A session that
    // those would call ambiguous (two roles on one selector, or a root
    // role and a nested one, neither pid-bound) credits nobody, because it
    // could not drain a mailbox either; `/role` in that session fixes both.
    let mut answers_to: Vec<(usize, String)> = Vec::new();
    let mut first_error: Option<String> = None;
    for (i, s) in sessions.iter().enumerate() {
        let cwd = Path::new(&s.cwd);
        let credited = match roles::resolve_for_session(&conn, s.pid, cwd) {
            Ok(RoleResolution::Resolved(r)) => Ok(Some(r.name)),
            Ok(_) => Ok(None),
            Err(e) => Err(e),
        };
        match credited {
            Ok(Some(name)) => answers_to.push((i, name)),
            Ok(None) => {}
            Err(e) => {
                first_error.get_or_insert(format!("bus roles: resolving session {}: {e}", s.pid));
            }
        }
    }
    roster_error(first_error);
    let live: Vec<bool> = rows
        .iter()
        .map(|r| {
            r.pid.is_some_and(claudectl_core::process::pid_alive)
                || answers_to.iter().any(|(_, name)| *name == r.role)
        })
        .collect();
    let parts: Vec<(String, bool)> = rows
        .iter()
        .map(|r| roles::selector_stem_parts(&r.cwd_selector))
        .collect();
    // Focus only ever decides between live roles on one selector, and it
    // costs a transcript read per session. So it is read only for roles in
    // such a group; a sole owner's sessions are not opened at all.
    let mut live_per_stem: HashMap<&(String, bool), u32> = HashMap::new();
    for (p, &is_live) in parts.iter().zip(&live) {
        if is_live {
            *live_per_stem.entry(p).or_default() += 1;
        }
    }
    let contested: HashSet<&str> = rows
        .iter()
        .zip(&parts)
        .zip(&live)
        .filter(|((_, p), is_live)| **is_live && live_per_stem.get(p).is_some_and(|n| *n > 1))
        .map(|((r, _), _)| r.role.as_str())
        .collect();
    let mut focus_by_role: HashMap<String, Vec<PathBuf>> = HashMap::new();
    for (i, name) in &answers_to {
        if !contested.contains(name.as_str()) {
            continue;
        }
        let s = &sessions[*i];
        let root = Path::new(s.worktree_id.as_deref().unwrap_or(&s.cwd));
        if let Some(f) = claudectl_core::focus::session_focus(s.jsonl_path.as_deref(), root) {
            focus_by_role
                .entry(name.clone())
                .or_default()
                .push(canonicalize_deep(&f.path_under(root)));
        }
    }
    rows.into_iter()
        .zip(parts)
        .zip(live)
        .map(|((r, (stem, bare)), live)| super::supervisor::RoleView {
            focus: focus_by_role.remove(&r.role).unwrap_or_default(),
            name: r.role,
            stem,
            bare,
            live,
        })
        .collect()
}

/// Say a roster problem once per distinct message, not on every pass of a
/// daemon; `None` clears it so the next occurrence is said again.
#[cfg(feature = "bus")]
fn roster_error(error: Option<String>) {
    static LAST_ERROR: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);
    if let Ok(mut last) = LAST_ERROR.lock() {
        if *last != error {
            if let Some(e) = &error {
                eprintln!("supervisor: {e}; routing nothing until it's fixed");
            }
            *last = error;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req() -> SpawnRequest {
        SpawnRequest {
            task_id: "task_1".into(),
            attempt_id: "attempt_1".into(),
            session_id: new_session_id(),
            workdir: PathBuf::from("/tmp"),
            prompt: "do the thing".into(),
            permission_mode: None,
            model: None,
        }
    }

    #[test]
    fn session_ids_are_v4_uuids_and_unique() {
        let a = new_session_id();
        let b = new_session_id();
        assert_ne!(a, b);
        assert_eq!(a.len(), 36);
        let parts: Vec<&str> = a.split('-').collect();
        assert_eq!(
            parts.iter().map(|p| p.len()).collect::<Vec<_>>(),
            vec![8, 4, 4, 4, 12]
        );
        assert!(parts[2].starts_with('4'));
        assert!(matches!(
            parts[3].chars().next(),
            Some('8' | '9' | 'a' | 'b')
        ));
    }

    #[test]
    fn args_name_the_session_and_carry_optional_flags() {
        let mut r = req();
        let args = claude_args(&r);
        assert_eq!(&args[..3], ["--print", "--output-format", "json"]);
        assert!(
            args.windows(2)
                .any(|w| w[0] == "--session-id" && w[1] == r.session_id)
        );
        assert_eq!(args.last().unwrap(), "do the thing");
        assert!(!args.contains(&"--permission-mode".to_string()));
        r.permission_mode = Some("acceptEdits".into());
        r.model = Some("sonnet".into());
        let args = claude_args(&r);
        assert!(
            args.windows(2)
                .any(|w| w[0] == "--permission-mode" && w[1] == "acceptEdits")
        );
        assert!(
            args.windows(2)
                .any(|w| w[0] == "--model" && w[1] == "sonnet")
        );
        assert_eq!(args.last().unwrap(), "do the thing");
    }

    #[test]
    fn a_result_is_read_only_when_complete() {
        let ok = r#"{"type":"result","is_error":false,"total_cost_usd":0.29,"result":"done"}"#;
        assert_eq!(
            parse_result(ok),
            Some(RunResult {
                ok: true,
                cost_usd: Some(0.29),
                summary: "done".into()
            })
        );
        let bad = r#"{"is_error":true,"result":"Not logged in"}"#;
        assert!(!parse_result(bad).unwrap().ok);
        assert_eq!(parse_result(""), None);
        assert_eq!(parse_result(r#"{"type":"result","is_err"#), None);
        assert_eq!(parse_result(r#"{"session_id":"x"}"#), None);
    }

    #[test]
    fn the_result_is_believed_over_the_pid() {
        let done = RunResult {
            ok: true,
            cost_usd: None,
            summary: String::new(),
        };
        // A reused pid that happens to be alive must not resurrect it.
        assert_eq!(
            observe(Some(&done), Some(42), |_| true, Some(5)),
            ObservedStatus::Completed
        );
        let failed = RunResult {
            ok: false,
            ..done.clone()
        };
        assert_eq!(
            observe(Some(&failed), Some(42), |_| true, Some(5)),
            ObservedStatus::Failed
        );
    }

    #[test]
    fn no_result_means_running_or_dead_by_the_pid() {
        assert_eq!(
            observe(None, Some(42), |_| true, Some(5)),
            ObservedStatus::Running
        );
        assert_eq!(
            observe(None, Some(42), |_| false, Some(5)),
            ObservedStatus::Dead
        );
    }

    #[test]
    fn an_attempt_without_a_pid_gets_a_grace_period() {
        assert_eq!(
            observe(None, None, |_| true, Some(3)),
            ObservedStatus::Unknown
        );
        assert_eq!(
            observe(None, None, |_| true, Some(SPAWN_GRACE_SECS + 1)),
            ObservedStatus::Dead
        );
        assert_eq!(observe(None, None, |_| true, None), ObservedStatus::Dead);
    }

    /// The zombie trap: a process the daemon spawned and never waited on
    /// stays "alive" to kill(pid, 0). Spawned detached, it's reaped.
    #[test]
    fn a_detached_process_is_not_left_a_zombie() {
        let dir = std::env::temp_dir().join(format!("cctl-spawn-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let pid = spawn_detached(
            Path::new("/bin/sh"),
            &["-c".into(), "echo hello".into()],
            &dir,
            &dir.join("o"),
            &dir.join("e"),
        )
        .unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while pid_alive(pid) && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(!pid_alive(pid), "exited process {pid} still reads as alive");
        assert_eq!(
            std::fs::read_to_string(dir.join("o")).unwrap().trim(),
            "hello"
        );
    }

    #[test]
    fn the_workdir_is_the_cwd_without_a_ref() {
        let t = TaskRow {
            id: "task_1".into(),
            name: "n".into(),
            state: super::super::tasks::TaskState::Pending,
            role: None,
            cwd: "/tmp".into(),
            prompt: "p".into(),
            model: None,
            budget_usd: None,
            max_retries: 2,
            timeout_min: 45,
            depends_on: vec![],
            policy: None,
            verifiers: vec![],
            created_at: String::new(),
            updated_at: String::new(),
            git_ref: None,
            branch: None,
            permission_mode: None,
            requires: vec![],
            peer: None,
            paths: vec![],
        };
        assert_eq!(prepare_workdir(&t).unwrap(), PathBuf::from("/tmp"));
        // And a task that ran in its cwd never has that cwd removed.
        assert_eq!(
            cleanup_workdir(&t, Path::new("/tmp")).unwrap(),
            worktree::Cleanup::Missing
        );
    }
}
