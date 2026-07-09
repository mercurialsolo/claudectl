//! Cross-teardown registry of live Claude Code sessions running inside an
//! `sbx` agent sandbox.
//!
//! Sandbox sessions (launched via `sc`) run inside an ephemeral `sbx` microVM,
//! but their Claude Code transcripts (`~/.claude`) and this registry
//! (`~/.local/share/claudectl`) live on host-shared bind mounts, so both
//! survive `sbx rm`. On every `SessionStart` / `SessionEnd` hook that fires
//! *inside a sandbox*, `hook_state::record_hook_event` upserts / removes the
//! session here, keyed by the sandbox name.
//!
//! The payoff: an abrupt `sbx rm` never fires `SessionEnd`, so that sandbox's
//! slice stays frozen at its last live state — exactly the set of sessions
//! `claudectl --restore-sessions` brings back, one `sc --resume <id>` window
//! each.
//!
//! Writes are serialized with an advisory `flock` and committed via
//! temp-file + atomic rename, so concurrent hook processes (many sessions
//! starting/ending at once) never corrupt or tear the file.

use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io;
use std::os::unix::io::AsRawFd;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Env var `sc` sets inside the sandbox to mark that Claude is running there.
/// Its mere presence — not its value — gates registry writes, mirroring the
/// `var_os(...).is_some()` convention the sandbox launcher uses elsewhere.
pub const ENV_SANDBOX_MARKER: &str = "LINERA_SANDBOX";
/// Env var carrying the sandbox's name (`sbx` container). Matches `sc`'s
/// `SANDBOX_NAME`, which defaults to `linera-agent` for the shared sandbox.
pub const ENV_SANDBOX_NAME: &str = "SANDBOX_NAME";
/// Default when `SANDBOX_NAME` is unset — kept in sync with `sc`.
const DEFAULT_SANDBOX_NAME: &str = "linera-agent";

fn current_version() -> u32 {
    1
}

/// One resumable session recorded in the registry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionEntry {
    /// Claude Code session id — the argument to `sc --resume <id>`.
    pub session_id: String,
    /// Host working directory the session was launched from, so restore can
    /// reopen it in the right place. Empty if the hook payload omitted `cwd`.
    #[serde(default)]
    pub cwd: String,
    /// Absolute path to the session's JSONL transcript on the shared
    /// `~/.claude` mount. Restore skips entries whose transcript no longer
    /// exists (unresumable). Empty if the payload omitted it.
    #[serde(default)]
    pub transcript: String,
    /// Unix epoch milliseconds at `SessionStart`. Zero if unknown.
    #[serde(default)]
    pub started_at_ms: u64,
}

/// The on-disk registry: a map of sandbox name -> its live sessions.
#[derive(Debug, Serialize, Deserialize)]
pub struct Registry {
    #[serde(default = "current_version")]
    pub version: u32,
    #[serde(default)]
    pub sandboxes: BTreeMap<String, Vec<SessionEntry>>,
}

impl Default for Registry {
    // Hand-written (not derived): a derived `Default` would set `version` to 0,
    // which `upsert` would then persist — the `serde(default)` only fills the
    // field when it's *absent* on read, not for `Default::default()`.
    fn default() -> Self {
        Registry {
            version: current_version(),
            sandboxes: BTreeMap::new(),
        }
    }
}

/// The sandbox this process is running inside, or `None` on the host.
///
/// Returns `Some(name)` only when the sandbox marker env var is present, so
/// host Claude sessions (which fire the same hooks) never touch the registry.
pub fn current_sandbox() -> Option<String> {
    std::env::var_os(ENV_SANDBOX_MARKER)?;
    let name = std::env::var(ENV_SANDBOX_NAME)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| DEFAULT_SANDBOX_NAME.to_string());
    Some(name)
}

/// Path to the shared registry file. Honors `CLAUDECTL_SANDBOX_REGISTRY`
/// (used by tests to avoid stomping the real file); otherwise the
/// host-shared `~/.local/share/claudectl` mount.
pub fn registry_path() -> PathBuf {
    if let Some(path) = std::env::var_os("CLAUDECTL_SANDBOX_REGISTRY") {
        return PathBuf::from(path);
    }
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    home.join(".local/share/claudectl/sandbox-sessions.json")
}

/// Read the registry. A missing or unparseable file yields an empty registry —
/// callers treat "no registry" and "empty registry" identically, and a
/// corrupt file should never block a restore attempt or a hook.
pub fn load() -> Registry {
    match fs::read(registry_path()) {
        Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_default(),
        Err(_) => Registry::default(),
    }
}

/// Add or replace a session under `sandbox` (idempotent on `session_id`).
pub fn upsert(sandbox: &str, entry: SessionEntry) -> io::Result<()> {
    with_lock(|| {
        let mut registry = load();
        let slice = registry.sandboxes.entry(sandbox.to_string()).or_default();
        slice.retain(|existing| existing.session_id != entry.session_id);
        slice.push(entry.clone());
        write_atomic(&registry)
    })
}

/// Remove a session by id from every sandbox slice. We key removal on the id
/// alone (not the current sandbox name) so a `SessionEnd` whose env drifted
/// from its `SessionStart` still cleans up correctly. Empty slices are pruned.
pub fn remove_session(session_id: &str) -> io::Result<()> {
    with_lock(|| {
        let mut registry = load();
        registry.sandboxes.retain(|_, slice| {
            slice.retain(|entry| entry.session_id != session_id);
            !slice.is_empty()
        });
        write_atomic(&registry)
    })
}

/// Serialize `registry` and commit it via temp-file + atomic rename, so a
/// reader never observes a half-written file.
fn write_atomic(registry: &Registry) -> io::Result<()> {
    let path = registry_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut bytes = serde_json::to_vec_pretty(registry)?;
    bytes.push(b'\n');
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, &bytes)?;
    fs::rename(&tmp, &path)
}

/// Run `body` while holding an exclusive advisory lock on a sidecar lock file,
/// so concurrent hook processes serialize their read-modify-write cycles.
/// The lock releases when the file descriptor closes at the end of scope.
fn with_lock<T>(body: impl FnOnce() -> io::Result<T>) -> io::Result<T> {
    let path = registry_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let lock_path = path.with_extension("lock");
    let lock_file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&lock_path)?;
    let fd = lock_file.as_raw_fd();
    // SAFETY: `fd` is a valid, open descriptor owned by `lock_file` for the
    // duration of this call; `flock` only reads it. LOCK_EX blocks until the
    // lock is acquired.
    if unsafe { libc::flock(fd, libc::LOCK_EX) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let result = body();
    // Best-effort unlock; dropping `lock_file` releases it regardless.
    unsafe { libc::flock(fd, libc::LOCK_UN) };
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, MutexGuard};

    /// Serializes every test that mutates process env vars — set_var/remove_var
    /// are process-global, and Rust runs tests on parallel threads. Recovers
    /// from poisoning so one panicking test doesn't cascade into the rest.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn env_guard() -> MutexGuard<'static, ()> {
        ENV_LOCK.lock().unwrap_or_else(|poison| poison.into_inner())
    }

    /// Point the registry at a throwaway file for the duration of a test, while
    /// holding the env lock so no other test observes our `CLAUDECTL_*` vars.
    struct TempRegistry {
        dir: std::path::PathBuf,
        _lock: MutexGuard<'static, ()>,
    }

    impl TempRegistry {
        fn new(tag: &str) -> Self {
            let lock = env_guard();
            // Include the pid: `cargo test` runs the lib and bin test binaries
            // as separate processes in parallel, and a tag-only path would let
            // them race on the same temp files. `ENV_LOCK` only serializes
            // within one process.
            let dir = std::env::temp_dir()
                .join(format!("claudectl-reg-test-{}-{tag}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            // SAFETY: env access here is serialized by the held `ENV_LOCK`.
            unsafe { std::env::set_var("CLAUDECTL_SANDBOX_REGISTRY", dir.join("registry.json")) };
            TempRegistry { dir, _lock: lock }
        }
    }

    impl Drop for TempRegistry {
        fn drop(&mut self) {
            // SAFETY: still holding `ENV_LOCK` via `_lock`.
            unsafe { std::env::remove_var("CLAUDECTL_SANDBOX_REGISTRY") };
            let _ = fs::remove_dir_all(&self.dir);
        }
    }

    fn entry(id: &str, cwd: &str) -> SessionEntry {
        SessionEntry {
            session_id: id.to_string(),
            cwd: cwd.to_string(),
            transcript: format!("/tmp/{id}.jsonl"),
            started_at_ms: 42,
        }
    }

    #[test]
    fn missing_file_loads_empty() {
        let _guard = TempRegistry::new("missing");
        let registry = load();
        assert!(registry.sandboxes.is_empty());
    }

    #[test]
    fn upsert_then_load_roundtrips() {
        let _guard = TempRegistry::new("roundtrip");
        upsert("linera-agent", entry("aaa", "/work/a")).unwrap();
        let registry = load();
        let slice = registry.sandboxes.get("linera-agent").unwrap();
        assert_eq!(slice.len(), 1);
        assert_eq!(slice[0], entry("aaa", "/work/a"));
        assert_eq!(registry.version, 1);
    }

    #[test]
    fn upsert_is_idempotent_on_session_id() {
        let _guard = TempRegistry::new("idempotent");
        upsert("linera-agent", entry("aaa", "/work/old")).unwrap();
        upsert("linera-agent", entry("aaa", "/work/new")).unwrap();
        let registry = load();
        let slice = registry.sandboxes.get("linera-agent").unwrap();
        assert_eq!(slice.len(), 1, "same id must not duplicate");
        assert_eq!(slice[0].cwd, "/work/new", "latest wins");
    }

    #[test]
    fn sandboxes_keep_independent_slices() {
        let _guard = TempRegistry::new("slices");
        upsert("linera-agent", entry("aaa", "/a")).unwrap();
        upsert("pm-task", entry("bbb", "/b")).unwrap();
        let registry = load();
        assert_eq!(registry.sandboxes.get("linera-agent").unwrap().len(), 1);
        assert_eq!(registry.sandboxes.get("pm-task").unwrap().len(), 1);
    }

    #[test]
    fn remove_deletes_by_id_and_prunes_empty_sandbox() {
        let _guard = TempRegistry::new("remove");
        upsert("linera-agent", entry("aaa", "/a")).unwrap();
        upsert("linera-agent", entry("bbb", "/b")).unwrap();
        remove_session("aaa").unwrap();
        let registry = load();
        let slice = registry.sandboxes.get("linera-agent").unwrap();
        assert_eq!(slice.len(), 1);
        assert_eq!(slice[0].session_id, "bbb");

        remove_session("bbb").unwrap();
        let registry = load();
        assert!(
            !registry.sandboxes.contains_key("linera-agent"),
            "sandbox with no sessions is pruned"
        );
    }

    #[test]
    fn remove_missing_id_is_a_noop() {
        let _guard = TempRegistry::new("remove-missing");
        upsert("linera-agent", entry("aaa", "/a")).unwrap();
        remove_session("does-not-exist").unwrap();
        assert_eq!(load().sandboxes.get("linera-agent").unwrap().len(), 1);
    }

    #[test]
    fn current_sandbox_gated_on_marker() {
        let _lock = env_guard();
        // SAFETY: env access is serialized by the held `ENV_LOCK`.
        unsafe {
            std::env::remove_var(ENV_SANDBOX_MARKER);
            std::env::remove_var(ENV_SANDBOX_NAME);
            assert_eq!(current_sandbox(), None);

            std::env::set_var(ENV_SANDBOX_MARKER, "1");
            assert_eq!(current_sandbox(), Some(DEFAULT_SANDBOX_NAME.to_string()));

            std::env::set_var(ENV_SANDBOX_NAME, "pm-task");
            assert_eq!(current_sandbox(), Some("pm-task".to_string()));

            std::env::remove_var(ENV_SANDBOX_MARKER);
            std::env::remove_var(ENV_SANDBOX_NAME);
        }
    }
}
