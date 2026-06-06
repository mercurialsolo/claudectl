//! Bind `SessionSource` to the binary's live discovery + monitor pipeline.

use claudectl_core::discovery;
use claudectl_core::runtime::{SessionSnapshot, SessionSource};

pub struct LiveSessionSource;

impl SessionSource for LiveSessionSource {
    fn list(&self) -> Vec<SessionSnapshot> {
        let mut sessions = discovery::scan_sessions();
        discovery::resolve_jsonl_paths(&mut sessions);
        sessions.into_iter().map(snapshot_from_live).collect()
    }
}

fn snapshot_from_live(s: claudectl_core::session::ClaudeSession) -> SessionSnapshot {
    SessionSnapshot {
        session_id: s.session_id,
        pid: s.pid,
        cwd: s.cwd,
        project_name: s.project_name,
        status: s.status.to_string(),
        cost_usd: s.cost_usd,
        context_tokens: s.context_tokens,
        context_max: s.context_max,
        last_message_ts: s.last_message_ts,
    }
}
