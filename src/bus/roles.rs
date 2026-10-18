//! Role addressing & resolution. See spec §5.
//!
//! A role is a persistent address ("planner", "impl-frontend"). Sessions are
//! ephemeral; roles outlive process death and re-bind on restart.
//!
//! Resolution order, given a caller's current working directory:
//!
//! 1. **Explicit** — the caller set `CLAUDECTL_BUS_ROLE` at session launch.
//! 2. **cwd-inferred** — a single registered role's `cwd_selector` is a
//!    prefix of (or equal to) the caller's cwd.
//! 3. **Ambiguous** — multiple roles match; surface an `Ambiguous` resolution
//!    so the caller is asked to pass `--role` explicitly.
//! 4. **Unbound** — no match. The caller is told so; nothing is registered
//!    on its behalf.
//!
//! The same selectors double as an *ownership map* (#596): a role bound to
//! a subtree owns the work under it. `owner_of_path` picks the role with the
//! most specific selector enclosing a path, so `bus role bind relay
//! <repo>/src/relay` routes relay work to `relay` while a role bound at the
//! repo root takes everything else. Unlike cwd inference, nesting is not
//! ambiguity here; only two roles on the *same* selector are.

use std::path::Path;

use rusqlite::Connection;
use serde::{Deserialize, Serialize};

use super::store::{self, RoleRow};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Role {
    pub name: String,
    pub cwd_selector: String,
    pub last_session_id: Option<String>,
    pub last_seen: String,
    pub subscriptions: Vec<String>,
    /// PID this role is bound to, if any (#307).
    pub pid: Option<u32>,
    /// Which agent holds the role (`claude`, `codex`), when it was bound with
    /// `--self`. `None` for a role bound by directory only.
    pub agent: Option<String>,
}

impl From<RoleRow> for Role {
    fn from(r: RoleRow) -> Self {
        Self {
            name: r.role,
            cwd_selector: r.cwd_selector,
            last_session_id: r.last_session_id,
            last_seen: r.last_seen,
            subscriptions: r.subscriptions,
            pid: r.pid,
            agent: r.agent,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RoleResolution {
    Resolved(Role),
    Ambiguous { candidates: Vec<String> },
    Unbound { cwd: String },
}

/// Who owns a path (#596). Its own type rather than a `RoleResolution`,
/// whose `Unbound { cwd }` names a caller's directory: here the thing asked
/// about is a path, often a file, and the wire shape should say so.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Ownership {
    /// Exactly one role is bound to the most specific enclosing selector.
    Owned(Role),
    /// Several roles are bound to that same selector; name one.
    Shared { candidates: Vec<String> },
    /// No selector encloses the path.
    Unowned { path: String },
}

pub const ROLE_ENV: &str = "CLAUDECTL_BUS_ROLE";

/// Resolve the caller's role. Order:
/// 1. `explicit` (CLI `--role`)
/// 2. `CLAUDECTL_BUS_ROLE` env
/// 3. **PID-binding** (#307) — any role bound to a pid in the caller's
///    ancestor chain (starting at the caller's parent — for the bus stdio
///    server, that's the Claude Code process).
/// 4. cwd-inference (literal-prefix match against bound `cwd_selector`s)
pub fn resolve(
    conn: &Connection,
    explicit: Option<&str>,
    cwd: &Path,
) -> Result<RoleResolution, String> {
    if let Some(name) = explicit {
        return resolve_by_name(conn, name, cwd);
    }
    if let Ok(name) = std::env::var(ROLE_ENV) {
        if !name.trim().is_empty() {
            return resolve_by_name(conn, name.trim(), cwd);
        }
    }
    if let Some(role) = resolve_by_pid_chain(conn, &ancestor_pids())? {
        return Ok(RoleResolution::Resolved(role));
    }
    resolve_by_cwd(conn, cwd)
}

/// Pick the first role bound to any pid in `chain`. Split out from
/// `ancestor_pids()` so tests can drive it with a synthetic chain.
fn resolve_by_pid_chain(conn: &Connection, chain: &[u32]) -> Result<Option<Role>, String> {
    for pid in chain {
        if let Some(row) = store::get_role_by_pid(conn, *pid)? {
            return Ok(Some(row.into()));
        }
    }
    Ok(None)
}

/// Caller's parent pid chain, capped at depth 8 so we don't walk to init.
/// Depth 8 comfortably covers `claude → bash → mcp-stdio-server` plus
/// nested shells, tmux, etc.
fn ancestor_pids() -> Vec<u32> {
    ancestor_chain_of(unsafe { libc::getppid() } as u32)
}

/// `start` and its parents, capped at depth 8 like `ancestor_pids`. For a
/// session this is the chain a `--pid` binding on its shell or tmux pane
/// is found in (#596).
fn ancestor_chain_of(start: u32) -> Vec<u32> {
    let mut out = Vec::new();
    let mut pid = start;
    for _ in 0..8 {
        if pid <= 1 {
            break;
        }
        out.push(pid);
        let parent = proc_info(pid)
            .map(|i| i.ppid)
            .or_else(|| parent_pid_of(pid));
        pid = match parent {
            Some(p) if p > 1 => p,
            _ => break,
        };
    }
    out
}

/// A process as the bus needs it: its parent, and the name the kernel
/// records for it.
struct ProcInfo {
    ppid: u32,
    name: String,
}

/// Read a process from the kernel rather than by running `ps`. A Codex
/// sandbox forbids running `ps` at all ("Operation not permitted"), which
/// left `bind --self` with no way to find the session it was run from
/// (#527); the kernel call is allowed there. It also spares a `ps` process
/// per ancestor on every lookup. `None` falls back to `ps`.
#[cfg(target_os = "macos")]
fn proc_info(pid: u32) -> Option<ProcInfo> {
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
    // SAFETY: `info` is a zeroed proc_bsdinfo of exactly `size` bytes, which
    // is what PROC_PIDTBSDINFO writes into.
    let n = unsafe {
        libc::proc_pidinfo(
            pid as libc::c_int,
            libc::PROC_PIDTBSDINFO,
            0,
            (&mut info as *mut libc::proc_bsdinfo).cast(),
            size,
        )
    };
    if n != size {
        return None;
    }
    // `pbi_name` is the full name (up to 32); `pbi_comm` is truncated to 16.
    let field: &[libc::c_char] = if info.pbi_name[0] != 0 {
        &info.pbi_name
    } else {
        &info.pbi_comm
    };
    let bytes: Vec<u8> = field
        .iter()
        .take_while(|c| **c != 0)
        .map(|c| *c as u8)
        .collect();
    Some(ProcInfo {
        ppid: info.pbi_ppid,
        name: String::from_utf8_lossy(&bytes).into_owned(),
    })
}

#[cfg(target_os = "linux")]
fn proc_info(pid: u32) -> Option<ProcInfo> {
    // `pid (name) state ppid ...`; the name may itself contain spaces or
    // parentheses, so it's everything between the first `(` and the last `)`.
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let (open, close) = (stat.find('(')?, stat.rfind(')')?);
    let ppid = stat[close + 1..].split_whitespace().nth(1)?.parse().ok()?;
    Some(ProcInfo {
        ppid,
        name: stat[open + 1..close].to_string(),
    })
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn proc_info(_pid: u32) -> Option<ProcInfo> {
    None
}

/// Which agent a process is: by its kernel name, or else by its `ps` command
/// line. The kernel names a process after its executable file, which isn't
/// always the program: Claude Code's native installer runs a file named for
/// its version (`~/.local/share/claude/versions/2.1.301`), and a `node`
/// launcher is `node`. The command line starts with what it was launched as.
fn agent_of_process(pid: u32) -> Option<&'static str> {
    proc_info(pid)
        .and_then(|info| agent_program_of(&info.name))
        .or_else(|| process_command_for(pid).and_then(|c| agent_program_of(&c)))
}

/// The coding agents a role can be bound to, by the program name `ps`
/// shows for them.
pub const AGENT_PROGRAMS: &[&str] = &["claude", "codex"];

/// Which agent a `ps` command line is, if it's one: the program's own name,
/// or the script a `node`/`bun`/`deno` launcher runs. Not "the command line
/// contains claude": the shell that runs `claudectl bus role bind --self`
/// matches that, so `--self` bound roles to a shell that exited a moment
/// later, in Claude Code as much as anywhere (#527).
pub fn agent_program_of(command: &str) -> Option<&'static str> {
    let mut words = command.split_whitespace();
    let base = |w: &str| w.rsplit('/').next().unwrap_or(w).to_ascii_lowercase();
    let mut program = base(words.next()?);
    if matches!(program.as_str(), "node" | "bun" | "deno") {
        program = base(words.find(|w| !w.starts_with('-'))?);
        program = program.trim_end_matches(".js").to_string();
    }
    AGENT_PROGRAMS.iter().copied().find(|a| *a == program)
}

/// The nearest coding-agent process above the caller, and which agent it is.
/// Used by `bus role bind --self` to attach a role to the session it's run
/// from without making the operator look up its pid (#310, #527).
pub fn find_agent_ancestor() -> Option<(u32, &'static str)> {
    ancestor_pids()
        .into_iter()
        .find_map(|pid| agent_of_process(pid).map(|a| (pid, a)))
}

/// Fetch `ps -o command=` for a pid. Returns `None` when the pid is gone.
fn process_command_for(pid: u32) -> Option<String> {
    let output = std::process::Command::new("ps")
        .args(["-o", "command=", "-p", &pid.to_string()])
        .env_clear()
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8(output.stdout).ok()?.trim().to_string())
}

/// Look up `ppid` for `pid` via `ps`. Returns `None` when the pid is gone
/// or `ps` fails. Native `ps` keeps us off the sysinfo crate.
fn parent_pid_of(pid: u32) -> Option<u32> {
    let output = std::process::Command::new("ps")
        .args(["-o", "ppid=", "-p", &pid.to_string()])
        .env_clear()
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout)
        .ok()?
        .trim()
        .parse::<u32>()
        .ok()
}

/// The role of *another* session, given its pid and cwd (#596). `resolve`
/// answers for the calling process: it reads `CLAUDECTL_BUS_ROLE` and walks
/// the caller's own ancestor chain, which for the bus server is the session
/// hosting it, so applying it to every session in `list_agents` labels them
/// all with the host's role. This looks for a binding on the session's own
/// pid or any of its ancestors (a `--pid` bind on its shell or pane counts,
/// as it does for `whoami` inside that session), then at its cwd. Another
/// session's `CLAUDECTL_BUS_ROLE` is not readable from here and is not
/// guessed.
pub fn resolve_for_session(
    conn: &Connection,
    pid: u32,
    cwd: &Path,
) -> Result<RoleResolution, String> {
    if let Some(role) = resolve_by_pid_chain(conn, &ancestor_chain_of(pid))? {
        return Ok(RoleResolution::Resolved(role));
    }
    resolve_by_cwd(conn, cwd)
}

fn resolve_by_name(conn: &Connection, name: &str, _cwd: &Path) -> Result<RoleResolution, String> {
    match store::get_role(conn, name)? {
        Some(r) => Ok(RoleResolution::Resolved(r.into())),
        None => Ok(RoleResolution::Unbound {
            cwd: name.to_string(),
        }),
    }
}

fn resolve_by_cwd(conn: &Connection, cwd: &Path) -> Result<RoleResolution, String> {
    let all = store::list_roles(conn)?;
    let cwd_canon = canonicalize_for_match(cwd);
    let cwd_str = cwd_canon.to_string_lossy();
    let mut matches: Vec<Role> = all
        .into_iter()
        .filter(|r| {
            let (stem, wildcard) = selector_stem(&r.cwd_selector);
            stem_matches(&stem, wildcard, &cwd_str)
        })
        .map(Role::from)
        .collect();
    match matches.len() {
        0 => Ok(RoleResolution::Unbound {
            cwd: cwd_str.into_owned(),
        }),
        1 => Ok(RoleResolution::Resolved(matches.remove(0))),
        _ => Ok(RoleResolution::Ambiguous {
            candidates: matches.into_iter().map(|r| r.name).collect(),
        }),
    }
}

/// The role that owns `path`: the one whose selector is the longest prefix
/// of it (#596). Several roles on that same longest selector are `Shared`;
/// no enclosing selector is `Unowned`. A role bound by pid still has a
/// selector (the cwd it was bound from), so it takes part.
///
/// `path` may be a file or a directory, and need not exist. It is compared
/// as spelled and in canonical form, so a file reached through a symlink
/// that leaves the bound tree (`/repo/config/prod.toml → /etc/app/…`) is
/// still owned by the role bound to `/repo/config`.
pub fn owner_of_path(conn: &Connection, path: &Path) -> Result<Ownership, String> {
    use claudectl_core::paths::{canonical_spellings, normalize_lexically};
    let all = store::list_roles(conn)?;
    let stems: Vec<(String, bool, RoleRow)> = all
        .into_iter()
        .map(|r| {
            let (stem, bare) = selector_stem(&r.cwd_selector);
            (stem, bare, r)
        })
        .collect();
    // The literal spelling first (`.` and `..` folded so a path cannot
    // escape its root through a string prefix); the on-disk forms only when
    // it matched no selector, since they cost a canonicalize walk each.
    let normalized = normalize_lexically(path);
    let literal = normalized.to_string_lossy().into_owned();
    let mut best = best_owners(&stems, std::slice::from_ref(&literal));
    if best.is_empty() {
        best = best_owners(&stems, &canonical_spellings(&normalized));
    }
    match best.len() {
        0 => Ok(Ownership::Unowned { path: literal }),
        1 => Ok(Ownership::Owned(best.remove(0))),
        _ => Ok(Ownership::Shared {
            candidates: best.into_iter().map(|r| r.name).collect(),
        }),
    }
}

/// The roles on the longest selector stem that covers any of `spellings`.
fn best_owners(stems: &[(String, bool, RoleRow)], spellings: &[String]) -> Vec<Role> {
    let mut best_len = 0usize;
    let mut best: Vec<Role> = Vec::new();
    for (stem, bare, r) in stems {
        if !spellings.iter().any(|p| stem_matches(stem, *bare, p)) {
            continue;
        }
        let len = stem.len();
        if len > best_len {
            best_len = len;
            best.clear();
        }
        if len == best_len {
            best.push(Role::from(r.clone()));
        }
    }
    best
}

/// Whether `selector` covers `path`, by the same rule the resolvers use.
pub fn selector_covers(selector: &str, path: &Path) -> bool {
    let (stem, bare) = selector_stem(selector);
    let p = canonicalize_for_match(path);
    stem_matches(&stem, bare, &p.to_string_lossy())
}

/// A selector's comparable stem and whether it is a *bare* prefix. Phase-1
/// selectors are literal path prefixes: `/work/proj` covers itself and
/// everything under it, on a `/` boundary, so `/work/project` is not
/// covered. A trailing `*` after a partial name (`/work/proj-*`) is a bare
/// prefix match, so `/work/proj-impl` is covered too; `/work/*` means the
/// directory and its children, exactly like `/work`. Glob support proper is
/// a phase-3 setup-wizard concern.
fn selector_stem(selector: &str) -> (String, bool) {
    let raw = selector.trim_end_matches('*');
    let bare = selector.ends_with('*') && !raw.ends_with('/') && !raw.is_empty();
    let canon = canonicalize_for_match(Path::new(raw));
    let stem = canon.to_string_lossy();
    (stem.trim_end_matches('/').to_string(), bare)
}

fn stem_matches(stem: &str, bare: bool, path: &str) -> bool {
    if stem.is_empty() {
        return false;
    }
    if bare {
        return path.starts_with(stem);
    }
    path == stem || path.starts_with(&format!("{stem}/"))
}

/// Resolve a path to its real on-disk form. macOS in particular
/// canonicalizes symlinked roots like `/tmp` → `/private/tmp`, which would
/// otherwise make cwd inference miss roles bound with the un-canonicalized
/// form. A path that does not exist yet takes the canonical form of its
/// longest existing ancestor (#596), so a file about to be created under a
/// bound directory still has an owner.
fn canonicalize_for_match(p: &Path) -> std::path::PathBuf {
    claudectl_core::paths::canonicalize_deep(p)
}

#[cfg(test)]
mod agent_programs {
    use super::agent_program_of;

    #[test]
    fn the_agent_is_the_program_not_any_mention_of_it() {
        assert_eq!(agent_program_of("claude"), Some("claude"));
        assert_eq!(
            agent_program_of("/opt/homebrew/bin/claude --resume x"),
            Some("claude")
        );
        assert_eq!(
            agent_program_of("/opt/homebrew/bin/codex exec -C /w hi"),
            Some("codex")
        );
        assert_eq!(
            agent_program_of("node /usr/local/lib/node_modules/@openai/codex/bin/codex.js"),
            Some("codex")
        );
        assert_eq!(
            agent_program_of("node --no-warnings /usr/local/bin/claude"),
            Some("claude")
        );
        // What --self used to bind: the shell running the command.
        assert_eq!(
            agent_program_of("/bin/zsh -c claudectl bus role bind x --self"),
            None
        );
        assert_eq!(agent_program_of("claudectl bus stdio"), None);
        assert_eq!(agent_program_of("/Users/me/codexbus/run.sh"), None);
        assert_eq!(agent_program_of(""), None);
    }

    /// The kernel read agrees with the process itself about its parent, and
    /// names it, so `ps` is only the fallback (#527).
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn the_kernel_names_a_process_and_its_parent() {
        let me = std::process::id();
        let info = super::proc_info(me).expect("own process is readable");
        assert_eq!(info.ppid, unsafe { libc::getppid() } as u32);
        assert!(!info.name.is_empty());
        assert!(super::proc_info(u32::MAX - 1).is_none(), "no such process");
    }

    /// Claude Code's native installer launches `~/.local/bin/claude`, a
    /// symlink to a file named for its version, so the kernel calls the
    /// process `2.1.301` and only the name it was launched as says claude.
    /// `sleep` launched as `claude` is the same shape.
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn an_agent_is_found_by_the_name_it_was_launched_as() {
        use std::os::unix::process::CommandExt;
        let mut child = std::process::Command::new("/bin/sleep")
            .arg0("/Users/me/.local/bin/claude")
            .arg("30")
            .spawn()
            .expect("spawn sleep");
        let agent = super::agent_of_process(child.id());
        let _ = child.kill();
        let _ = child.wait();
        assert_eq!(agent, Some("claude"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bus::store::{open_memory, upsert_role};
    use std::path::PathBuf;

    #[test]
    fn cwd_inference_picks_single_match() {
        let conn = open_memory();
        upsert_role(&conn, "planner", "/work/proj-plan", None, None).unwrap();
        upsert_role(&conn, "impl", "/work/proj-impl", None, None).unwrap();
        let r = resolve(&conn, None, &PathBuf::from("/work/proj-impl/src")).unwrap();
        match r {
            RoleResolution::Resolved(role) => assert_eq!(role.name, "impl"),
            other => panic!("expected Resolved, got {other:?}"),
        }
    }

    #[test]
    fn shared_cwd_is_ambiguous() {
        let conn = open_memory();
        upsert_role(&conn, "planner", "/shared/repo", None, None).unwrap();
        upsert_role(&conn, "impl", "/shared/repo", None, None).unwrap();
        let r = resolve(&conn, None, &PathBuf::from("/shared/repo")).unwrap();
        match r {
            RoleResolution::Ambiguous { candidates } => assert_eq!(candidates.len(), 2),
            other => panic!("expected Ambiguous, got {other:?}"),
        }
    }

    #[test]
    fn explicit_role_overrides_cwd() {
        let conn = open_memory();
        upsert_role(&conn, "planner", "/work/proj", None, None).unwrap();
        upsert_role(&conn, "impl", "/other/place", None, None).unwrap();
        let r = resolve(&conn, Some("impl"), &PathBuf::from("/work/proj")).unwrap();
        assert!(matches!(r, RoleResolution::Resolved(role) if role.name == "impl"));
    }

    #[test]
    fn unbound_cwd_reports_unbound() {
        let conn = open_memory();
        let r = resolve(&conn, None, &PathBuf::from("/nowhere")).unwrap();
        assert!(matches!(r, RoleResolution::Unbound { .. }));
    }

    // ── ownership (#596) ──────────────────────────────────────────────────

    #[test]
    fn a_wildcard_selector_covers_siblings_sharing_its_stem() {
        let conn = open_memory();
        upsert_role(&conn, "worker", "/work/proj-*", None, None).unwrap();
        assert!(matches!(
            owner(&conn, "/work/proj-7/src/x.rs"),
            Ownership::Owned(r) if r.name == "worker"
        ));
        assert!(matches!(
            resolve(&conn, None, &PathBuf::from("/work/proj-impl")).unwrap(),
            RoleResolution::Resolved(r) if r.name == "worker"
        ));
        assert!(matches!(
            owner(&conn, "/work/other"),
            Ownership::Unowned { .. }
        ));
    }

    #[test]
    fn a_wildcard_after_a_directory_keeps_the_slash_boundary() {
        // `/work/*` is the directory and its children, not every path that
        // happens to start with `/work`.
        let conn = open_memory();
        upsert_role(&conn, "worker", "/work/*", None, None).unwrap();
        assert!(matches!(
            owner(&conn, "/work/app/x.rs"),
            Ownership::Owned(r) if r.name == "worker"
        ));
        assert!(matches!(owner(&conn, "/work"), Ownership::Owned(_)));
        assert!(matches!(
            owner(&conn, "/workspace/app/x.rs"),
            Ownership::Unowned { .. }
        ));
        assert!(matches!(
            resolve(&conn, None, &PathBuf::from("/workspace/app")).unwrap(),
            RoleResolution::Unbound { .. }
        ));
    }

    #[test]
    fn a_file_reached_through_a_symlink_that_leaves_the_tree_is_still_owned() {
        let tmp = std::env::temp_dir().join(format!("owner-link-{}", std::process::id()));
        let config = tmp.join("config");
        let real = tmp.join("elsewhere").join("prod.toml");
        std::fs::create_dir_all(&config).unwrap();
        std::fs::create_dir_all(real.parent().unwrap()).unwrap();
        std::fs::write(&real, "x = 1\n").unwrap();
        let link = config.join("prod.toml");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let conn = open_memory();
        upsert_role(&conn, "ops", &config.to_string_lossy(), None, None).unwrap();
        assert!(matches!(
            owner_of_path(&conn, &link).unwrap(),
            Ownership::Owned(r) if r.name == "ops"
        ));
        assert!(matches!(
            owner_of_path(&conn, &real).unwrap(),
            Ownership::Unowned { .. }
        ));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn selector_covers_agrees_with_the_resolvers() {
        assert!(selector_covers("/repo", Path::new("/repo/src/a.rs")));
        assert!(selector_covers("/repo", Path::new("/repo")));
        assert!(!selector_covers("/repo", Path::new("/repository")));
        assert!(selector_covers("/work/proj-*", Path::new("/work/proj-7")));
    }

    #[test]
    fn a_file_that_does_not_exist_yet_under_a_symlinked_selector_has_an_owner() {
        // The selector canonicalizes (macOS `/var/folders` → `/private/var/…`);
        // a not-yet-created file spelled through the symlink must still
        // match. Where temp_dir is not a symlink both spellings agree and
        // the test holds trivially.
        let tmp = std::env::temp_dir().join(format!("owner-sym-{}", std::process::id()));
        std::fs::create_dir_all(tmp.join("src")).unwrap();
        let conn = open_memory();
        upsert_role(
            &conn,
            "impl",
            &tmp.join("src").to_string_lossy(),
            None,
            None,
        )
        .unwrap();
        let new_file = tmp.join("src").join("new.rs");
        assert!(!new_file.exists());
        assert!(matches!(
            owner_of_path(&conn, &new_file).unwrap(),
            Ownership::Owned(r) if r.name == "impl"
        ));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn a_sessions_role_is_its_own_pid_binding_then_its_cwd() {
        let conn = open_memory();
        upsert_role(&conn, "host", "/repo", None, Some(std::process::id())).unwrap();
        upsert_role(&conn, "worker", "/repo", None, Some(4_000_001)).unwrap();
        upsert_role(&conn, "docs", "/docs", None, None).unwrap();
        // The session bound by pid gets its own role, not the host's.
        assert!(matches!(
            resolve_for_session(&conn, 4_000_001, &PathBuf::from("/repo")).unwrap(),
            RoleResolution::Resolved(r) if r.name == "worker"
        ));
        // A binding on an ancestor of the session (its shell, its pane) is
        // found the way `whoami` inside the session would find it. The
        // test process's parent is such an ancestor of the test itself.
        let parent = unsafe { libc::getppid() } as u32;
        upsert_role(&conn, "pane", "/repo", None, Some(parent)).unwrap();
        assert!(
            matches!(
                resolve_for_session(&conn, std::process::id(), &PathBuf::from("/repo")).unwrap(),
                RoleResolution::Resolved(r) if r.name == "host"
            ),
            "the session's own pid wins over an ancestor's"
        );
        // Remove the own-pid row so the ancestor binding is what resolves.
        conn.execute("DELETE FROM roles WHERE role = 'host'", [])
            .unwrap();
        assert!(matches!(
            resolve_for_session(&conn, std::process::id(), &PathBuf::from("/elsewhere")).unwrap(),
            RoleResolution::Resolved(r) if r.name == "pane"
        ));
        // An unbound pid falls back to that session's cwd.
        assert!(matches!(
            resolve_for_session(&conn, 4_000_002, &PathBuf::from("/docs/guide")).unwrap(),
            RoleResolution::Resolved(r) if r.name == "docs"
        ));
        assert!(matches!(
            resolve_for_session(&conn, 4_000_002, &PathBuf::from("/nowhere")).unwrap(),
            RoleResolution::Unbound { .. }
        ));
    }

    fn owner(conn: &Connection, path: &str) -> Ownership {
        owner_of_path(conn, &PathBuf::from(path)).unwrap()
    }

    #[test]
    fn the_most_specific_selector_owns_the_path() {
        let conn = open_memory();
        upsert_role(&conn, "impl", "/repo", None, None).unwrap();
        upsert_role(&conn, "relay", "/repo/src/relay", None, None).unwrap();
        assert!(matches!(
            owner(&conn, "/repo/src/relay/peer.rs"),
            Ownership::Owned(r) if r.name == "relay"
        ));
        assert!(matches!(
            owner(&conn, "/repo/src/bus/mcp.rs"),
            Ownership::Owned(r) if r.name == "impl"
        ));
        assert!(
            matches!(owner(&conn, "/repo/src/relay"), Ownership::Owned(r) if r.name == "relay"),
            "the subtree directory itself belongs to its owner"
        );
    }

    #[test]
    fn nesting_is_not_ambiguity_but_a_shared_selector_is() {
        let conn = open_memory();
        upsert_role(&conn, "a", "/repo", None, None).unwrap();
        upsert_role(&conn, "b", "/repo", None, None).unwrap();
        upsert_role(&conn, "relay", "/repo/src/relay", None, None).unwrap();
        match owner(&conn, "/repo/README.md") {
            Ownership::Shared { candidates } => {
                assert_eq!(candidates.len(), 2);
                assert!(!candidates.contains(&"relay".to_string()));
            }
            other => panic!("expected Shared, got {other:?}"),
        }
        assert!(matches!(
            owner(&conn, "/repo/src/relay/peer.rs"),
            Ownership::Owned(r) if r.name == "relay"
        ));
        // cwd inference keeps its own rule: nested selectors are ambiguous there.
        assert!(matches!(
            resolve(&conn, None, &PathBuf::from("/repo/src/relay")).unwrap(),
            RoleResolution::Ambiguous { .. }
        ));
    }

    #[test]
    fn a_path_that_escapes_the_tree_through_dotdot_is_not_owned() {
        let conn = open_memory();
        upsert_role(&conn, "impl", "/repo", None, None).unwrap();
        assert!(matches!(
            owner(&conn, "/repo/../other/x.rs"),
            Ownership::Unowned { path } if path == "/other/x.rs"
        ));
        assert!(matches!(
            owner(&conn, "/repo/./src/../src/x.rs"),
            Ownership::Owned(r) if r.name == "impl"
        ));
    }

    #[test]
    fn a_path_under_no_selector_has_no_owner() {
        let conn = open_memory();
        upsert_role(&conn, "relay", "/repo/src/relay", None, None).unwrap();
        assert!(matches!(
            owner(&conn, "/repo/src/relayx/peer.rs"),
            Ownership::Unowned { path } if path == "/repo/src/relayx/peer.rs"
        ));
        assert!(matches!(
            owner(&conn, "/elsewhere"),
            Ownership::Unowned { .. }
        ));
    }

    #[test]
    fn pid_binding_takes_precedence_over_cwd_match() {
        // #307: a role bound to pid 12345 should win over a role whose
        // cwd_selector matches the caller's cwd, when 12345 is in the
        // caller's ancestor chain.
        let conn = open_memory();
        upsert_role(&conn, "cwd-role", "/work/proj", None, None).unwrap();
        upsert_role(&conn, "pid-role", "/work/proj", None, Some(12345)).unwrap();
        let role = resolve_by_pid_chain(&conn, &[99999, 12345, 1])
            .unwrap()
            .expect("expected pid match");
        assert_eq!(role.name, "pid-role");
        assert_eq!(role.pid, Some(12345));
    }

    #[test]
    fn pid_chain_with_no_match_falls_through() {
        // Resolver returns None so the caller falls back to cwd inference.
        let conn = open_memory();
        upsert_role(&conn, "cwd-role", "/work/proj", None, Some(11111)).unwrap();
        assert!(
            resolve_by_pid_chain(&conn, &[22222, 33333])
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn upsert_keeps_existing_pid_when_rebinding_without_one() {
        // Re-binding a role for a new session_id shouldn't clobber the pid
        // — otherwise the TUI's resume flow would silently lose the binding.
        let conn = open_memory();
        upsert_role(&conn, "frontend", "/work/proj", None, Some(7777)).unwrap();
        upsert_role(&conn, "frontend", "/work/proj", Some("sess_42"), None).unwrap();
        let row = crate::bus::store::get_role(&conn, "frontend")
            .unwrap()
            .unwrap();
        assert_eq!(row.pid, Some(7777));
        assert_eq!(row.last_session_id.as_deref(), Some("sess_42"));
    }
}
