//! Sandbox terminal bridge client.
//!
//! When claudectl runs inside the agent-sandbox microVM and the host runs a
//! Linux desktop terminal (kitty / tmux / wezterm), the in-sandbox process
//! cannot drive the host terminal directly: the kitty/tmux/wezterm binaries
//! and their unix sockets live on the host, not in the sandbox.
//!
//! The host daemon `sandbox-terminal-bridge.sh` (shipped from
//! linera-infra/tools/agent-sandbox/install.sh) watches a shared bind-mounted
//! directory under `~/.cache/sandbox-terminal-bridge/` and runs the matching
//! kitty / tmux / wezterm CLI on the host, then writes the result back.
//!
//! This module is the in-sandbox client: it serializes a request JSON,
//! atomically drops it into `requests/<id>.json`, and polls
//! `responses/<id>.out` for the result.
//!
//! The protocol is intentionally identical in shape to the older
//! `sandbox-osa-bridge` (see `run_osascript` in `terminals/mod.rs`) so the
//! same atomic-write + poll-with-timeout pattern is reused. The wire format
//! itself is JSON instead of a raw AppleScript blob because the Linux side
//! has to multiplex three terminals and four verbs.
//!
//! See linera-infra issue #986 for the cross-repo design.

use serde::Serialize;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Default poll deadline for the response file. Picked to match the
/// osa-bridge default (3s) plus headroom for the multiplexed Linux backend.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(5);

/// Polling interval while waiting for the response file. 25ms keeps focus
/// latency imperceptible without burning CPU.
const POLL_INTERVAL: Duration = Duration::from_millis(25);

/// Bridge protocol caps `send_text` payloads at 1024 bytes (enforced by the
/// host daemon to keep request files bounded). We mirror the limit on the
/// client so the user gets a clear error before a request is dropped.
pub const MAX_SEND_TEXT_BYTES: usize = 1024;

/// Which host terminal the request targets. The string form is the wire value
/// the daemon dispatches on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BridgeTerminal {
    Kitty,
    Tmux,
    WezTerm,
}

impl BridgeTerminal {
    fn as_wire(self) -> &'static str {
        match self {
            BridgeTerminal::Kitty => "kitty",
            BridgeTerminal::Tmux => "tmux",
            BridgeTerminal::WezTerm => "wezterm",
        }
    }
}

/// Per-(verb, terminal) target identification. Each variant carries exactly
/// the fields the host daemon needs to reach the right window/pane.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(untagged)]
pub enum BridgeTarget {
    /// kitty: `socket` is `KITTY_LISTEN_ON` (e.g. `unix:/tmp/mykitty`),
    /// `match` is a kitty match expression like `id:42`.
    /// Kitty docs: https://sw.kovidgoyal.net/kitty/remote-control/
    Kitty {
        socket: String,
        #[serde(rename = "match")]
        match_expr: String,
    },
    /// tmux: `socket` is the tmux server socket path (typically derived from
    /// `TMUX` env), `pane` is a target like `%17`.
    /// tmux docs: https://man.openbsd.org/tmux.1
    Tmux { socket: String, pane: String },
    /// wezterm: `pane_id` is the integer from `WEZTERM_PANE`, optional
    /// `unix_socket` overrides the default mux socket if set.
    /// wezterm docs: https://wezfurlong.org/wezterm/cli/cli/index.html
    WezTerm {
        pane_id: u64,
        unix_socket: Option<String>,
    },
}

/// Verb + per-verb args. `args` is JSON-flattened on the wire so each verb
/// only carries the fields it needs.
///
/// The protocol also defines a `list_windows` verb on the host side; the
/// claudectl client does not consume it yet, so it is intentionally not
/// modeled here. Add it when a doctor or matcher actually needs the data.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BridgeVerb {
    /// Bring the target window/tab/pane to the foreground.
    FocusTab,
    /// Inject UTF-8 text into the target. Capped at MAX_SEND_TEXT_BYTES.
    SendText { text: String },
    /// Inject a single named key. Allowed values match the protocol spec
    /// (Enter, Return, Escape, Tab, BackSpace, Up, Down, Left, Right, F1..F12).
    SendKey { key: String },
}

/// Outcome of a successful bridge round-trip. `payload` is the response
/// body minus the exit-code line — for `list_windows` this is stdout; for
/// the other verbs it is stderr (typically empty on success).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BridgeOutput {
    pub payload: String,
}

/// Distinct error shapes so the caller can surface actionable messages.
#[derive(Debug)]
pub enum BridgeError {
    /// `~/.cache/sandbox-terminal-bridge/` (or its `requests/` subdir) is
    /// missing — the host daemon is not installed or the bind mount is not
    /// active. Suggest running install.sh on the host.
    BridgeUnavailable { path: PathBuf, reason: String },
    /// Local I/O failed (write, rename, read). Carries the underlying error
    /// text for diagnosis.
    Io(String),
    /// Request payload was too large for the protocol.
    PayloadTooLarge { size: usize, max: usize },
    /// Response did not arrive within the deadline. The daemon may be stuck
    /// or not running; we surface the bridge dir + timeout so the user can
    /// debug.
    Timeout {
        bridge_dir: PathBuf,
        waited: Duration,
    },
    /// Daemon ran but returned a non-zero exit code. `code` is parsed from
    /// the first line of the response file; `stderr` is the rest.
    Daemon { code: i32, stderr: String },
    /// Response file was unparseable (missing exit-code line, etc).
    Malformed(String),
}

impl std::fmt::Display for BridgeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BridgeError::BridgeUnavailable { path, reason } => write!(
                f,
                "sandbox-terminal-bridge not available at {} ({reason}). Install the host daemon by running tools/agent-sandbox/install.sh on the host, then retry.",
                path.display(),
            ),
            BridgeError::Io(msg) => write!(f, "sandbox-terminal-bridge I/O error: {msg}"),
            BridgeError::PayloadTooLarge { size, max } => write!(
                f,
                "sandbox-terminal-bridge: payload {size} bytes exceeds protocol max of {max}",
            ),
            BridgeError::Timeout { bridge_dir, waited } => write!(
                f,
                "sandbox-terminal-bridge timed out after {:?} (bridge dir: {}). Is sandbox-terminal-bridge.sh running on the host?",
                waited,
                bridge_dir.display(),
            ),
            BridgeError::Daemon { code, stderr } => {
                if stderr.is_empty() {
                    write!(f, "sandbox-terminal-bridge: host returned exit code {code}")
                } else {
                    write!(
                        f,
                        "sandbox-terminal-bridge: host returned exit code {code}: {}",
                        stderr.trim()
                    )
                }
            }
            BridgeError::Malformed(msg) => {
                write!(f, "sandbox-terminal-bridge: malformed response — {msg}")
            }
        }
    }
}

impl std::error::Error for BridgeError {}

/// Default bridge dir: `$HOME/.cache/sandbox-terminal-bridge`. The host
/// install.sh creates this and bind-mounts it RW into the sandbox at the
/// same path so $HOME-relative resolution lines up on both sides.
pub fn default_bridge_dir() -> Result<PathBuf, BridgeError> {
    let home = std::env::var("HOME").map_err(|_| BridgeError::BridgeUnavailable {
        path: PathBuf::from("~/.cache/sandbox-terminal-bridge"),
        reason: "$HOME is unset".into(),
    })?;
    Ok(PathBuf::from(home).join(".cache/sandbox-terminal-bridge"))
}

/// One-shot dispatch using the default bridge dir + timeout. Used by the
/// in-sandbox terminal arms in `terminals/mod.rs`.
pub fn dispatch(
    terminal: BridgeTerminal,
    target: BridgeTarget,
    verb: BridgeVerb,
) -> Result<BridgeOutput, BridgeError> {
    let bridge = default_bridge_dir()?;
    dispatch_with(&bridge, terminal, target, verb, DEFAULT_TIMEOUT)
}

/// Test-friendly variant: takes an explicit bridge dir + timeout so the
/// integration tests can spin up a fake daemon under tempfile.
pub fn dispatch_with(
    bridge_dir: &Path,
    terminal: BridgeTerminal,
    target: BridgeTarget,
    verb: BridgeVerb,
    timeout: Duration,
) -> Result<BridgeOutput, BridgeError> {
    if let BridgeVerb::SendText { text } = &verb
        && text.len() > MAX_SEND_TEXT_BYTES
    {
        return Err(BridgeError::PayloadTooLarge {
            size: text.len(),
            max: MAX_SEND_TEXT_BYTES,
        });
    }

    let requests_dir = bridge_dir.join("requests");
    let responses_dir = bridge_dir.join("responses");
    if !requests_dir.is_dir() {
        return Err(BridgeError::BridgeUnavailable {
            path: bridge_dir.to_path_buf(),
            reason: format!("{} does not exist", requests_dir.display()),
        });
    }
    // The daemon creates responses/ on startup; if it is missing, treat the
    // bridge as unavailable rather than fail later on a read.
    if !responses_dir.is_dir() {
        return Err(BridgeError::BridgeUnavailable {
            path: bridge_dir.to_path_buf(),
            reason: format!("{} does not exist", responses_dir.display()),
        });
    }

    let id = make_request_id();
    let req_path = requests_dir.join(format!("{id}.json"));
    let resp_path = responses_dir.join(format!("{id}.out"));
    let tmp_path = requests_dir.join(format!("{id}.json.tmp"));

    let body = serde_json::to_vec(&Request {
        verb: verb.wire_name(),
        terminal: terminal.as_wire(),
        target,
        args: VerbArgs::from_verb(&verb),
    })
    .map_err(|e| BridgeError::Io(format!("serialize request: {e}")))?;

    // Atomic write: stream into .tmp, then rename. The daemon's inotify /
    // poll loop only acts on the renamed final name, so a partial write is
    // never observed. Same idiom as run_osascript.
    {
        let mut f = std::fs::File::create(&tmp_path)
            .map_err(|e| BridgeError::Io(format!("create {}: {e}", tmp_path.display())))?;
        f.write_all(&body)
            .map_err(|e| BridgeError::Io(format!("write {}: {e}", tmp_path.display())))?;
    }
    std::fs::rename(&tmp_path, &req_path).map_err(|e| {
        // Best-effort cleanup: if rename failed the .tmp is still there.
        let _ = std::fs::remove_file(&tmp_path);
        BridgeError::Io(format!("rename request file: {e}"))
    })?;

    // Poll for the response. The daemon also uses rename-into-place, so
    // existence implies the file is complete.
    let deadline = Instant::now() + timeout;
    while !resp_path.is_file() {
        if Instant::now() >= deadline {
            // Best-effort: drop our orphan request so the daemon does not
            // act on it after we have already given up.
            let _ = std::fs::remove_file(&req_path);
            return Err(BridgeError::Timeout {
                bridge_dir: bridge_dir.to_path_buf(),
                waited: timeout,
            });
        }
        std::thread::sleep(POLL_INTERVAL);
    }

    let raw = std::fs::read_to_string(&resp_path)
        .map_err(|e| BridgeError::Io(format!("read response: {e}")))?;
    // Best-effort cleanup; if it fails the daemon will GC.
    let _ = std::fs::remove_file(&resp_path);

    parse_response(&raw)
}

#[derive(Serialize)]
struct Request<'a> {
    verb: &'static str,
    terminal: &'static str,
    target: BridgeTarget,
    args: VerbArgs<'a>,
}

/// Per-verb args, serialized as a JSON object. We can't use the verb enum
/// directly because the wire shape splits `verb` (string) and `args`
/// (object), and `serde(tag/content)` would put the verb name inside `args`.
#[derive(Serialize)]
#[serde(untagged)]
enum VerbArgs<'a> {
    Empty {},
    Text { text: &'a str },
    Key { key: &'a str },
}

impl<'a> VerbArgs<'a> {
    fn from_verb(v: &'a BridgeVerb) -> Self {
        match v {
            BridgeVerb::FocusTab => VerbArgs::Empty {},
            BridgeVerb::SendText { text } => VerbArgs::Text { text },
            BridgeVerb::SendKey { key } => VerbArgs::Key { key },
        }
    }
}

impl BridgeVerb {
    fn wire_name(&self) -> &'static str {
        match self {
            BridgeVerb::FocusTab => "focus_tab",
            BridgeVerb::SendText { .. } => "send_text",
            BridgeVerb::SendKey { .. } => "send_key",
        }
    }
}

/// Generate a request id unique enough for serialized writes from a single
/// claudectl process. Same shape as run_osascript's id.
fn make_request_id() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{}-{}", std::process::id(), nanos)
}

/// Response wire format:
///     <exit-code>\n
///     <stderr-or-stdout-payload>
fn parse_response(raw: &str) -> Result<BridgeOutput, BridgeError> {
    let (exit_line, payload) = raw
        .split_once('\n')
        .map(|(a, b)| (a.trim(), b))
        .unwrap_or((raw.trim(), ""));
    let code: i32 = exit_line.parse().map_err(|_| {
        BridgeError::Malformed(format!(
            "first line of response is not an integer exit code: {exit_line:?}",
        ))
    })?;
    if code == 0 {
        Ok(BridgeOutput {
            payload: payload.to_string(),
        })
    } else {
        Err(BridgeError::Daemon {
            code,
            stderr: payload.to_string(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::thread;
    use tempfile::TempDir;

    fn make_bridge() -> TempDir {
        let dir = TempDir::new().expect("tempdir");
        std::fs::create_dir_all(dir.path().join("requests")).unwrap();
        std::fs::create_dir_all(dir.path().join("responses")).unwrap();
        dir
    }

    /// Spawn a fake daemon thread that watches `requests/`, validates the
    /// payload via the supplied closure, and writes a fixed response. Returns
    /// a channel receiver carrying the parsed request JSON for assertions.
    fn fake_daemon(
        bridge: PathBuf,
        response_body: &'static str,
    ) -> mpsc::Receiver<serde_json::Value> {
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let requests = bridge.join("requests");
            let responses = bridge.join("responses");
            let deadline = Instant::now() + Duration::from_secs(2);
            loop {
                if Instant::now() >= deadline {
                    return;
                }
                let entries = match std::fs::read_dir(&requests) {
                    Ok(e) => e,
                    Err(_) => {
                        thread::sleep(Duration::from_millis(10));
                        continue;
                    }
                };
                let mut found = None;
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.extension().and_then(|s| s.to_str()) == Some("json") {
                        found = Some(path);
                        break;
                    }
                }
                let Some(req_path) = found else {
                    thread::sleep(Duration::from_millis(10));
                    continue;
                };
                let body = match std::fs::read_to_string(&req_path) {
                    Ok(b) => b,
                    Err(_) => continue,
                };
                let parsed: serde_json::Value =
                    serde_json::from_str(&body).expect("daemon: parse request");
                let id = req_path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap()
                    .to_string();
                let _ = std::fs::remove_file(&req_path);
                let resp_tmp = responses.join(format!("{id}.out.tmp"));
                let resp_final = responses.join(format!("{id}.out"));
                std::fs::write(&resp_tmp, response_body).unwrap();
                std::fs::rename(&resp_tmp, &resp_final).unwrap();
                tx.send(parsed).unwrap();
                return;
            }
        });
        rx
    }

    #[test]
    fn dispatch_focus_tab_round_trip() {
        let bridge = make_bridge();
        let rx = fake_daemon(bridge.path().to_path_buf(), "0\n");

        let out = dispatch_with(
            bridge.path(),
            BridgeTerminal::Kitty,
            BridgeTarget::Kitty {
                socket: "unix:/tmp/mykitty".into(),
                match_expr: "id:42".into(),
            },
            BridgeVerb::FocusTab,
            Duration::from_secs(1),
        )
        .expect("dispatch ok");
        assert_eq!(out.payload, "");

        let req = rx
            .recv_timeout(Duration::from_secs(1))
            .expect("daemon saw request");
        assert_eq!(req["verb"], "focus_tab");
        assert_eq!(req["terminal"], "kitty");
        assert_eq!(req["target"]["socket"], "unix:/tmp/mykitty");
        assert_eq!(req["target"]["match"], "id:42");
        assert!(req["args"].is_object());
    }

    #[test]
    fn dispatch_send_text_serializes_text_arg() {
        let bridge = make_bridge();
        let rx = fake_daemon(bridge.path().to_path_buf(), "0\n");

        dispatch_with(
            bridge.path(),
            BridgeTerminal::Tmux,
            BridgeTarget::Tmux {
                socket: "/tmp/tmux-501/default".into(),
                pane: "%7".into(),
            },
            BridgeVerb::SendText {
                text: "hello\n".into(),
            },
            Duration::from_secs(1),
        )
        .expect("dispatch ok");

        let req = rx.recv_timeout(Duration::from_secs(1)).unwrap();
        assert_eq!(req["verb"], "send_text");
        assert_eq!(req["terminal"], "tmux");
        assert_eq!(req["args"]["text"], "hello\n");
        assert_eq!(req["target"]["pane"], "%7");
    }

    #[test]
    fn dispatch_send_key_serializes_key_arg() {
        let bridge = make_bridge();
        let rx = fake_daemon(bridge.path().to_path_buf(), "0\n");

        dispatch_with(
            bridge.path(),
            BridgeTerminal::WezTerm,
            BridgeTarget::WezTerm {
                pane_id: 13,
                unix_socket: None,
            },
            BridgeVerb::SendKey {
                key: "Enter".into(),
            },
            Duration::from_secs(1),
        )
        .expect("dispatch ok");

        let req = rx.recv_timeout(Duration::from_secs(1)).unwrap();
        assert_eq!(req["verb"], "send_key");
        assert_eq!(req["terminal"], "wezterm");
        assert_eq!(req["args"]["key"], "Enter");
        assert_eq!(req["target"]["pane_id"], 13);
        // unix_socket=None must serialize to JSON null (we don't strip it
        // because the daemon needs to know "use default" vs "key absent").
        assert!(req["target"]["unix_socket"].is_null());
    }

    #[test]
    fn dispatch_returns_response_payload_to_caller() {
        // Even for void verbs, the daemon may return diagnostic stdout/stderr
        // after the exit-code line; the client must surface it via payload so
        // doctor and matcher heuristics can use it later.
        let bridge = make_bridge();
        let _rx = fake_daemon(bridge.path().to_path_buf(), "0\nfocused\n");

        let out = dispatch_with(
            bridge.path(),
            BridgeTerminal::Kitty,
            BridgeTarget::Kitty {
                socket: "unix:/tmp/mykitty".into(),
                match_expr: "id:1".into(),
            },
            BridgeVerb::FocusTab,
            Duration::from_secs(1),
        )
        .expect("dispatch ok");
        assert_eq!(out.payload, "focused\n");
    }

    #[test]
    fn dispatch_propagates_daemon_error() {
        let bridge = make_bridge();
        let _rx = fake_daemon(bridge.path().to_path_buf(), "2\nkitty: no such window\n");

        let err = dispatch_with(
            bridge.path(),
            BridgeTerminal::Kitty,
            BridgeTarget::Kitty {
                socket: "unix:/tmp/mykitty".into(),
                match_expr: "id:99".into(),
            },
            BridgeVerb::FocusTab,
            Duration::from_secs(1),
        )
        .expect_err("daemon failure");
        match err {
            BridgeError::Daemon { code, stderr } => {
                assert_eq!(code, 2);
                assert!(stderr.contains("no such window"));
            }
            other => panic!("expected Daemon, got {other:?}"),
        }
    }

    #[test]
    fn dispatch_times_out_when_no_daemon() {
        let bridge = make_bridge();
        // No daemon spawned.
        let err = dispatch_with(
            bridge.path(),
            BridgeTerminal::Kitty,
            BridgeTarget::Kitty {
                socket: "unix:/tmp/mykitty".into(),
                match_expr: "id:1".into(),
            },
            BridgeVerb::FocusTab,
            Duration::from_millis(150),
        )
        .expect_err("must time out");
        assert!(matches!(err, BridgeError::Timeout { .. }), "got {err}");
    }

    #[test]
    fn dispatch_reports_missing_bridge_dir() {
        let dir = TempDir::new().unwrap();
        // No requests/ subdir: simulates uninstalled host daemon.
        let err = dispatch_with(
            dir.path(),
            BridgeTerminal::Kitty,
            BridgeTarget::Kitty {
                socket: "unix:/tmp/mykitty".into(),
                match_expr: "id:1".into(),
            },
            BridgeVerb::FocusTab,
            Duration::from_millis(150),
        )
        .expect_err("must report missing");
        assert!(
            matches!(err, BridgeError::BridgeUnavailable { .. }),
            "got {err}",
        );
    }

    #[test]
    fn send_text_payload_too_large_rejected_locally() {
        let bridge = make_bridge();
        let big = "x".repeat(MAX_SEND_TEXT_BYTES + 1);
        let err = dispatch_with(
            bridge.path(),
            BridgeTerminal::Kitty,
            BridgeTarget::Kitty {
                socket: "unix:/tmp/mykitty".into(),
                match_expr: "id:1".into(),
            },
            BridgeVerb::SendText { text: big },
            Duration::from_millis(150),
        )
        .expect_err("must reject");
        assert!(
            matches!(err, BridgeError::PayloadTooLarge { .. }),
            "got {err}"
        );
    }

    #[test]
    fn parse_response_rejects_non_integer_exit_code() {
        let err = parse_response("oops\nstuff\n").expect_err("must fail");
        assert!(matches!(err, BridgeError::Malformed(_)), "got {err}");
    }
}
