//! MCP server (`claudectl bus stdio`). Exposes the bus surface to Claude Code
//! sessions over stdio JSON-RPC.
//!
//! Tools implemented (spec §3):
//!
//! * `whoami` — returns the calling session's role binding (or Ambiguous /
//!   Unbound resolution).
//! * `list_agents` — snapshot of every running Claude Code session with its
//!   role (if any), cwd, status, and last-seen timestamp.
//! * `publish` — append a message to the recipient role's mailbox. Directed
//!   send only in phase 4; subject pub/sub + claim protocol come in phase 7.
//!   `addressed_to_path` routes to whichever role owns a path (#596).
//! * `read_inbox` — drain pending directed messages for the caller's role.
//! * `owner_of` — which role owns a path: the one bound to the most specific
//!   enclosing directory (#596).
//!
//! The tools share one in-process bus state struct so the SQLite
//! connection is opened exactly once per server lifetime.

use std::path::PathBuf;
use std::sync::Mutex;

use rmcp::ErrorData as McpError;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::tool::Parameters;
use rmcp::handler::server::wrapper::Json;
use rmcp::model::{ServerCapabilities, ServerInfo};
use rmcp::serde_json::json;
use rmcp::transport::io::stdio;
use rmcp::{ServerHandler, ServiceExt, tool, tool_handler, tool_router};
use rusqlite::Connection;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::policy::{self, DEFAULT_MAX_BODY_BYTES, DEFAULT_MAX_HOPS};
use super::roles::{self, Ownership, RoleResolution};
use super::store::{self, MessageRow};
use crate::rate_limit::RateLimiter;

// -------------------- Tool argument & result types ---------------------------

#[derive(Debug, Default, Deserialize, JsonSchema)]
pub struct WhoamiArgs {
    /// Optional explicit role. Wins over cwd inference. Falls back to the
    /// `CLAUDECTL_BUS_ROLE` env var, then to cwd matching.
    #[serde(default)]
    pub role: Option<String>,
    /// Optional override cwd. Defaults to the process's actual cwd; provided
    /// for tests and rare cases where the calling Claude Code session sets a
    /// per-tool working directory.
    #[serde(default)]
    pub cwd: Option<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct WhoamiResult {
    pub resolution: serde_json::Value,
}

#[derive(Debug, Default, Deserialize, JsonSchema)]
pub struct ListAgentsArgs {}

#[derive(Debug, Serialize, JsonSchema)]
pub struct ListAgentsResult {
    pub agents: Vec<AgentSnapshot>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct AgentSnapshot {
    pub session_id: String,
    pub pid: u32,
    pub cwd: String,
    pub project: String,
    pub status: String,
    pub role: Option<String>,
    /// Where in its worktree the session's recent edits concentrate (#596):
    /// a directory relative to the root, `.` for the root itself, absent
    /// when the transcript shows no in-tree edits.
    pub focus: Option<String>,
    pub last_seen_ts: u64,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct PublishArgs {
    pub subject: String,
    pub body: String,
    #[serde(rename = "type")]
    pub msg_type: String,
    #[serde(default)]
    pub addressed_to: Option<String>,
    /// Address the message to whichever role owns this path (#596): the
    /// role bound to the most specific selector enclosing it. Refused when
    /// no role owns the path or two roles share its selector. Exclusive
    /// with `addressed_to`.
    #[serde(default)]
    pub addressed_to_path: Option<String>,
    /// The directory a relative `addressed_to_path` is taken against.
    /// Defaults to the server's cwd, which is where the hosting session
    /// started; a session that has moved should pass its own.
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub thread_id: Option<String>,
    /// "high" | "normal" | "low". Defaults to "normal".
    #[serde(default)]
    pub priority: Option<String>,
    /// Optional sender role override; otherwise inferred from caller cwd.
    #[serde(default)]
    pub sender_role: Option<String>,
    /// Parent hop count (#344). The publish handler stores `parent_hop + 1`
    /// and refuses to write rows above `policy::DEFAULT_MAX_HOPS`. Omitted
    /// or zero means "originating message"; supervisor-routed forwards must
    /// carry the inherited hop from the message that triggered them.
    #[serde(default)]
    pub parent_hop: Option<u32>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct PublishResult {
    pub message_id: String,
    pub sanitized: bool,
    /// The hop count stored on the new row (`parent_hop + 1`).
    pub hop_count: u32,
    /// The role the message was queued for, once `addressed_to_path` has
    /// been resolved (#596).
    pub addressed_to: Option<String>,
    /// Set when the recipient is a name no role has ever bound (#596). The
    /// message is queued all the same and waits until one does; reserved
    /// roles (`supervisor`, `operator`) can never be bound and never warn.
    pub warning: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct OwnerOfArgs {
    /// A file or directory, absolute or relative to `cwd`.
    pub path: String,
    /// The directory a relative `path` is taken against. Defaults to the
    /// server's cwd, which is where the hosting session started.
    #[serde(default)]
    pub cwd: Option<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct OwnerOfResult {
    pub resolution: serde_json::Value,
}

#[derive(Debug, Default, Deserialize, JsonSchema)]
pub struct ReadInboxArgs {
    #[serde(default)]
    pub role: Option<String>,
    /// ISO timestamp. Only messages created after this are returned.
    #[serde(default)]
    pub since: Option<String>,
    /// Non-destructive read (#344). When true, returns the same rows
    /// `drain` would but leaves their status `pending` so a subsequent
    /// drain still hands them out. Supervisor uses this for exactly-once
    /// assignment.
    #[serde(default)]
    pub peek: bool,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct ReadInboxResult {
    pub role: Option<String>,
    pub messages: Vec<InboxEntry>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct InboxEntry {
    pub id: String,
    pub subject: String,
    #[serde(rename = "type")]
    pub msg_type: String,
    pub sender_role: Option<String>,
    pub thread_id: Option<String>,
    pub body: String,
    pub priority: String,
    pub created_at: String,
    /// Hop count carried with the message (#344). Recipients reading via
    /// peek can decide whether forwarding the message would exceed the
    /// cap without taking the drain side-effect.
    pub hop_count: u32,
}

// -------------------- Supervisor MCP tool types (#345) -----------------------

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SubmitTaskArgs {
    pub name: String,
    pub cwd: String,
    pub prompt: String,
    /// Target role mailbox. When unset, the supervisor spawns a fresh
    /// session in `cwd` rather than routing via mailbox.
    #[serde(default)]
    pub role: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub budget_usd: Option<f64>,
    #[serde(default)]
    pub max_retries: Option<u32>,
    #[serde(default)]
    pub timeout_min: Option<u32>,
    #[serde(default)]
    pub depends_on: Option<Vec<String>>,
    /// Per-task policy JSON (force_manual overrides, model routing, …).
    /// Whatever lands here flows through to the brain-gate hook via the
    /// session-policy file the supervisor writes at assignment time.
    #[serde(default)]
    pub policy: Option<serde_json::Value>,
    /// Verifier list (#347). Expects an array of `{kind: "run"|"brain"|
    /// "agent", ...}` objects; serde flattens to `Vec<Verifier>` inside
    /// the handler. Empty / missing ⇒ the task graduates without gates.
    #[serde(default)]
    pub verifiers: Option<serde_json::Value>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct SubmitTaskResult {
    pub task_id: String,
    pub state: String,
}

#[derive(Debug, Default, Deserialize, JsonSchema)]
pub struct ListTasksArgs {
    /// Filter by task state. One of PENDING/READY/ASSIGNED/RUNNING/
    /// VERIFYING/DONE/RETRYING/RESUMING/NEEDS_HUMAN/CANCELLED.
    #[serde(default)]
    pub state: Option<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct ListTasksResult {
    pub tasks: Vec<TaskSummary>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct TaskSummary {
    pub id: String,
    pub name: String,
    pub state: String,
    pub role: Option<String>,
    pub cwd: String,
    pub model: Option<String>,
    pub budget_usd: Option<f64>,
    pub created_at: String,
    pub updated_at: String,
}

impl From<crate::coord::tasks::TaskRow> for TaskSummary {
    fn from(t: crate::coord::tasks::TaskRow) -> Self {
        Self {
            id: t.id,
            name: t.name,
            state: t.state.as_str().to_string(),
            role: t.role,
            cwd: t.cwd,
            model: t.model,
            budget_usd: t.budget_usd,
            created_at: t.created_at,
            updated_at: t.updated_at,
        }
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct TaskStatusArgs {
    pub task_id: String,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct TaskStatusResult {
    pub task: TaskSummary,
    pub transitions: Vec<TransitionEntry>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct TransitionEntry {
    pub from_state: String,
    pub to_state: String,
    pub cause: String,
    pub at: String,
}

impl From<MessageRow> for InboxEntry {
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
            hop_count: m.hop_count,
        }
    }
}

// -------------------- Server state -------------------------------------------

pub struct BusServer {
    conn: Mutex<Connection>,
    rate_limiter: RateLimiter,
    tool_router: ToolRouter<Self>,
}

impl BusServer {
    pub fn new(conn: Connection) -> Self {
        Self {
            conn: Mutex::new(conn),
            rate_limiter: RateLimiter::with_defaults(),
            tool_router: Self::tool_router(),
        }
    }

    fn caller_cwd(override_cwd: Option<&str>) -> PathBuf {
        if let Some(c) = override_cwd {
            return PathBuf::from(c);
        }
        std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"))
    }

    fn resolution_to_json(r: RoleResolution) -> serde_json::Value {
        serde_json::to_value(r).unwrap_or_else(|_| json!({ "kind": "unbound" }))
    }

    fn ownership_to_json(o: Ownership) -> serde_json::Value {
        serde_json::to_value(o).unwrap_or_else(|_| json!({ "kind": "unowned" }))
    }
}

#[tool_router]
impl BusServer {
    #[tool(description = "Identify the calling session's role binding.")]
    async fn whoami(
        &self,
        Parameters(args): Parameters<WhoamiArgs>,
    ) -> Result<Json<WhoamiResult>, McpError> {
        let conn = self.conn.lock().expect("bus db mutex poisoned");
        let cwd = Self::caller_cwd(args.cwd.as_deref());
        let res = roles::resolve(&conn, args.role.as_deref(), &cwd)
            .map_err(|e| McpError::internal_error(e, None))?;
        Ok(Json(WhoamiResult {
            resolution: Self::resolution_to_json(res),
        }))
    }

    #[tool(description = "List every running Claude Code session and its role binding.")]
    async fn list_agents(
        &self,
        Parameters(_): Parameters<ListAgentsArgs>,
    ) -> Result<Json<ListAgentsResult>, McpError> {
        let mut sessions = crate::discovery::scan_sessions();
        crate::discovery::resolve_jsonl_paths(&mut sessions);
        crate::discovery::resolve_worktree_ids(&mut sessions);
        // Transcript reads happen before the DB lock: a focus can cost tens
        // of MB of parsing per session, and `publish` / `read_inbox` from
        // other sessions must not wait behind it.
        let focuses: Vec<Option<String>> = sessions
            .iter()
            .map(|s| {
                let root = s.worktree_id.as_deref().unwrap_or(&s.cwd);
                claudectl_core::focus::session_focus(
                    s.jsonl_path.as_deref(),
                    std::path::Path::new(root),
                )
                .map(|f| f.dir)
            })
            .collect();
        let conn = self.conn.lock().expect("bus db mutex poisoned");
        let mut out = Vec::with_capacity(sessions.len());
        for (s, focus) in sessions.into_iter().zip(focuses) {
            let cwd_path = PathBuf::from(&s.cwd);
            // That session's own binding, not the host session's (#596):
            // `resolve` reads the env and the *server's* ancestor pids.
            let role = match roles::resolve_for_session(&conn, s.pid, &cwd_path) {
                Ok(RoleResolution::Resolved(r)) => Some(r.name),
                _ => None,
            };
            out.push(AgentSnapshot {
                session_id: s.session_id.clone(),
                pid: s.pid,
                cwd: s.cwd.clone(),
                project: s.project_name.clone(),
                status: s.status.to_string(),
                role,
                focus,
                last_seen_ts: s.last_message_ts,
            });
        }
        Ok(Json(ListAgentsResult { agents: out }))
    }

    #[tool(description = "Publish a message to a recipient role's mailbox.")]
    async fn publish(
        &self,
        Parameters(args): Parameters<PublishArgs>,
    ) -> Result<Json<PublishResult>, McpError> {
        policy::validate_subject(&args.subject)
            .map_err(|e| McpError::invalid_params(e.to_string(), None))?;
        policy::validate_type(&args.msg_type)
            .map_err(|e| McpError::invalid_params(e.to_string(), None))?;
        policy::validate_body(&args.body, DEFAULT_MAX_BODY_BYTES)
            .map_err(|e| McpError::invalid_params(e.to_string(), None))?;

        // Hop guard (#344). `parent_hop` carries the hop count of the
        // message that triggered this publish — 0 for originating messages,
        // inherited for forwards. The outgoing hop is `parent_hop + 1`.
        // Rejecting `outgoing > DEFAULT_MAX_HOPS` means the eighth forward
        // is the last; the ninth attempt is what RFC v2 §9 calls "supervisor
        // escalation expected."
        let outgoing_hop = args.parent_hop.unwrap_or(0).saturating_add(1);
        policy::validate_hop_count(outgoing_hop, DEFAULT_MAX_HOPS)
            .map_err(|e| McpError::invalid_params(e.to_string(), None))?;

        // The sender is the caller's own role unless it names one (#527).
        // The field always said "otherwise inferred from caller cwd", and
        // nothing inferred it, so a reply from any agent that didn't pass
        // it arrived with no sender to answer.
        // The recipient is resolved here, with the other argument checks
        // and before the rate limiter, so a refused `addressed_to_path`
        // does not spend a token on a message that was never queued (#596).
        // `args.cwd` is only for the relative `addressed_to_path`; the
        // sender is still inferred from where the server runs, so passing a
        // cwd to route by cannot change who a message is signed as.
        let (sender_role, recipient) = {
            let conn = self.conn.lock().expect("bus db mutex poisoned");
            let sender = sender_for(&conn, args.sender_role.clone(), &Self::caller_cwd(None));
            let recipient = recipient_for(
                &conn,
                args.addressed_to.as_deref(),
                args.addressed_to_path.as_deref(),
                &Self::caller_cwd(args.cwd.as_deref()),
            )
            .map_err(|e| match e {
                RecipientError::Invalid(msg) => McpError::invalid_params(msg, None),
                RecipientError::Store(msg) => McpError::internal_error(msg, None),
            })?;
            (sender, recipient)
        };

        // Rate limit per sender role. Anonymous sends (no role) skip the
        // limiter — that path is rare and not the vector RFC v2 §9 calls
        // out; the reserved-role guard handles the privileged endpoints.
        if let Some(sender) = sender_role.as_deref() {
            if !self
                .rate_limiter
                .try_acquire(sender, std::time::Instant::now())
            {
                return Err(McpError::invalid_params(
                    format!(
                        "rate limit exceeded for sender role '{sender}'; retry after the bucket refills"
                    ),
                    None,
                ));
            }
        }

        // Past the limiter: now the probes that cost a session scan.
        let recipient = recipient.check_presence(&LivePresence);

        let sanitized_body = policy::sanitize_body(&args.body);
        let sanitized = sanitized_body != args.body;
        let priority = args.priority.as_deref().unwrap_or("normal");

        let conn = self.conn.lock().expect("bus db mutex poisoned");
        let id = store::insert_message(
            &conn,
            &args.subject,
            &args.msg_type,
            sender_role.as_deref(),
            recipient.role.as_deref(),
            args.thread_id.as_deref(),
            &sanitized_body,
            priority,
            outgoing_hop,
        )
        .map_err(|e| McpError::internal_error(e, None))?;
        Ok(Json(PublishResult {
            message_id: id,
            sanitized,
            hop_count: outgoing_hop,
            addressed_to: recipient.role,
            warning: recipient.warning,
        }))
    }

    #[tool(
        description = "Which role owns a path (#596): the role bound to the most specific directory enclosing it. Use it to pick a recipient for work on that part of the tree; publish's addressed_to_path does the same in one step."
    )]
    async fn owner_of(
        &self,
        Parameters(args): Parameters<OwnerOfArgs>,
    ) -> Result<Json<OwnerOfResult>, McpError> {
        let conn = self.conn.lock().expect("bus db mutex poisoned");
        let path = absolute_path(&args.path, &Self::caller_cwd(args.cwd.as_deref()));
        let res =
            roles::owner_of_path(&conn, &path).map_err(|e| McpError::internal_error(e, None))?;
        Ok(Json(OwnerOfResult {
            resolution: Self::ownership_to_json(res),
        }))
    }

    #[tool(
        description = "File a task for the supervisor to schedule. Lands as a coord `tasks` row; the supervisor's reconciler picks it up on its next tick. Equivalent to publishing a `task.created` message — agents can use either path."
    )]
    async fn submit_task(
        &self,
        Parameters(args): Parameters<SubmitTaskArgs>,
    ) -> Result<Json<SubmitTaskResult>, McpError> {
        let coord = crate::coord::store::open().map_err(|e| McpError::internal_error(e, None))?;
        let verifiers =
            args.verifiers
                .as_ref()
                .map(|raw| {
                    serde_json::from_value::<Vec<crate::coord::verify::Verifier>>(raw.clone())
                        .map_err(|e| {
                            McpError::invalid_params(
                                format!("verifiers must be a Verifier array: {e}"),
                                None,
                            )
                        })
                })
                .transpose()?
                .unwrap_or_default();
        let new_task = crate::coord::tasks::NewTask {
            name: args.name,
            role: args.role,
            cwd: args.cwd,
            prompt: args.prompt,
            model: args.model,
            budget_usd: args.budget_usd,
            max_retries: args.max_retries,
            timeout_min: args.timeout_min,
            depends_on: args.depends_on.unwrap_or_default(),
            policy: args.policy,
            verifiers,
            // Worktree placement (#570) isn't on the MCP surface yet; a task
            // submitted here runs in its cwd, as before.
            git_ref: None,
            branch: None,
            permission_mode: None,
            requires: vec![],
            peer: None,
            paths: vec![],
        };
        let task_id = crate::coord::tasks::insert_task(&coord, &new_task)
            .map_err(|e| McpError::internal_error(e, None))?;
        Ok(Json(SubmitTaskResult {
            task_id,
            state: "PENDING".into(),
        }))
    }

    #[tool(
        description = "List supervisor tasks. Filter by state with `state=\"RUNNING\"` etc; omit for all."
    )]
    async fn list_tasks(
        &self,
        Parameters(args): Parameters<ListTasksArgs>,
    ) -> Result<Json<ListTasksResult>, McpError> {
        let coord = crate::coord::store::open().map_err(|e| McpError::internal_error(e, None))?;
        let state_filter = args
            .state
            .as_deref()
            .and_then(crate::coord::tasks::TaskState::parse);
        let rows = crate::coord::tasks::list_tasks(&coord, state_filter)
            .map_err(|e| McpError::internal_error(e, None))?;
        Ok(Json(ListTasksResult {
            tasks: rows.into_iter().map(TaskSummary::from).collect(),
        }))
    }

    #[tool(
        description = "Detailed status of one task: current state, attempt history, verifier verdicts, transition log."
    )]
    async fn task_status(
        &self,
        Parameters(args): Parameters<TaskStatusArgs>,
    ) -> Result<Json<TaskStatusResult>, McpError> {
        let coord = crate::coord::store::open().map_err(|e| McpError::internal_error(e, None))?;
        let task = crate::coord::tasks::get_task(&coord, &args.task_id)
            .map_err(|e| McpError::internal_error(e, None))?
            .ok_or_else(|| {
                McpError::invalid_params(format!("task {} not found", args.task_id), None)
            })?;
        let transitions = crate::coord::tasks::list_transitions(&coord, &args.task_id)
            .map_err(|e| McpError::internal_error(e, None))?;
        Ok(Json(TaskStatusResult {
            task: TaskSummary::from(task),
            transitions: transitions
                .into_iter()
                .map(|(from, to, cause, at)| TransitionEntry {
                    from_state: from,
                    to_state: to,
                    cause,
                    at,
                })
                .collect(),
        }))
    }

    #[tool(
        description = "Read pending directed messages addressed to the caller's role. Drains by default; pass peek=true to read without marking delivered."
    )]
    async fn read_inbox(
        &self,
        Parameters(args): Parameters<ReadInboxArgs>,
    ) -> Result<Json<ReadInboxResult>, McpError> {
        let mut conn = self.conn.lock().expect("bus db mutex poisoned");
        let cwd = Self::caller_cwd(None);
        let resolved = match roles::resolve(&conn, args.role.as_deref(), &cwd)
            .map_err(|e| McpError::internal_error(e, None))?
        {
            RoleResolution::Resolved(r) => r.name,
            _ => {
                return Ok(Json(ReadInboxResult {
                    role: None,
                    messages: vec![],
                }));
            }
        };
        let rows = if args.peek {
            store::peek_inbox(&conn, &resolved, args.since.as_deref())
                .map_err(|e| McpError::internal_error(e, None))?
        } else {
            store::drain_inbox(&mut conn, &resolved, args.since.as_deref())
                .map_err(|e| McpError::internal_error(e, None))?
        };
        Ok(Json(ReadInboxResult {
            role: Some(resolved),
            messages: rows.into_iter().map(InboxEntry::from).collect(),
        }))
    }
}

#[tool_handler]
impl ServerHandler for BusServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            instructions: Some(
                "claudectl agent bus. Use list_agents to discover peers (each with the \
                 directory its edits concentrate in, `focus`), whoami for the caller's \
                 role, publish to send a directed message (addressed_to a role, or \
                 addressed_to_path to whoever owns that part of the tree), owner_of to \
                 ask who owns a path, read_inbox to drain your mailbox. See \
                 docs/AGENT_BUS.md."
                    .into(),
            ),
            capabilities: ServerCapabilities::builder().enable_tools().build(),
            ..Default::default()
        }
    }
}

/// Who a message is for, and whether anyone is there to read it (#596).
#[derive(Debug)]
pub struct Recipient {
    pub role: Option<String>,
    pub warning: Option<String>,
    /// The pid binding behind the role, kept so `check_presence` can run
    /// after the rate limiter has admitted the message.
    binding: Option<Binding>,
}

#[derive(Debug)]
struct Binding {
    role: String,
    pid: u32,
    selector: String,
}

impl Recipient {
    /// Add the nobody-home warning: the binding names a pid that is no
    /// longer running *and* no running session sits under its selector.
    /// Roles are durable and the message waits for the re-bind, but a
    /// `/role` session that exited still owns its selector, and silence
    /// there is the failure #596 exists to remove. A role re-bound by
    /// directory keeps its old pid (`upsert_role` coalesces), which is why a
    /// dead pid alone is not enough: a live session in the bound directory
    /// drains that inbox by cwd inference. Separate from `recipient_for` so
    /// the session scan runs only for a message the limiter has admitted.
    pub fn check_presence(mut self, presence: &dyn Presence) -> Self {
        if let Some(b) = self.binding.take() {
            if self.warning.is_none() && !presence.pid_alive(b.pid) {
                let someone_there = presence
                    .session_cwds()
                    .iter()
                    .any(|c| roles::selector_covers(&b.selector, std::path::Path::new(c)));
                if !someone_there {
                    self.warning = Some(format!(
                        "role '{}' is bound to pid {}, which is not running, and no session is in {}; the message waits until one binds",
                        b.role, b.pid, b.selector
                    ));
                }
            }
        }
        self
    }
}

/// Why a recipient could not be resolved. `Invalid` is the caller's
/// arguments (an MCP `invalid_params`); `Store` is the bus database (an
/// `internal_error`), so a locked DB is never reported as a bad request.
#[derive(Debug, PartialEq, Eq)]
pub enum RecipientError {
    Invalid(String),
    Store(String),
}

impl std::fmt::Display for RecipientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Invalid(m) | Self::Store(m) => f.write_str(m),
        }
    }
}

/// Resolve a message's recipient from `addressed_to` or `addressed_to_path`.
///
/// A path is routed to its owner and *refused* when it has none or two
/// roles share its selector: the sender asked to be routed, so a message
/// nobody would read is an error, not a queued row. A name is accepted as
/// it always was, with a warning when no role has ever bound it; the
/// reserved roles are addressable without being bindable and never warn.
/// The reserved check is exact-case, because the mailbox is keyed on the
/// exact name: `Operator` can never be bound *or* drained, so it gets the
/// unbound warning rather than silence. Naming both is an error.
///
/// This only touches the database. The liveness and session probes run in
/// [`Recipient::check_presence`], which the caller invokes once the message
/// has passed the rate limiter.
pub fn recipient_for(
    conn: &rusqlite::Connection,
    addressed_to: Option<&str>,
    addressed_to_path: Option<&str>,
    cwd: &std::path::Path,
) -> Result<Recipient, RecipientError> {
    let store_err = RecipientError::Store;
    let binding_of = |role: &str, pid: Option<u32>, selector: &str| {
        pid.map(|pid| Binding {
            role: role.to_string(),
            pid,
            selector: selector.to_string(),
        })
    };
    match (addressed_to, addressed_to_path) {
        (Some(_), Some(_)) => Err(RecipientError::Invalid(
            "pass addressed_to or addressed_to_path, not both".into(),
        )),
        (None, Some(path)) => {
            let abs = absolute_path(path, cwd);
            match roles::owner_of_path(conn, &abs).map_err(store_err)? {
                Ownership::Owned(r) => Ok(Recipient {
                    binding: binding_of(&r.name, r.pid, &r.cwd_selector),
                    role: Some(r.name),
                    warning: None,
                }),
                Ownership::Shared { candidates } => Err(RecipientError::Invalid(format!(
                    "no single owner for {}: {} are bound to the same selector; address one of them",
                    abs.display(),
                    candidates.join(", ")
                ))),
                Ownership::Unowned { path } => Err(RecipientError::Invalid(format!(
                    "no role owns {path}: bind one to an enclosing directory with `claudectl bus role bind <name> <dir>`"
                ))),
            }
        }
        (Some(name), None) => {
            let reserved = policy::RESERVED_ROLES.contains(&name);
            let (warning, binding) = if reserved {
                (None, None)
            } else {
                match store::get_role(conn, name).map_err(store_err)? {
                    Some(row) => (None, binding_of(&row.role, row.pid, &row.cwd_selector)),
                    None => (
                        Some(format!(
                            "no role '{name}' is bound; the message waits until one binds"
                        )),
                        None,
                    ),
                }
            };
            Ok(Recipient {
                role: Some(name.to_string()),
                warning,
                binding,
            })
        }
        (None, None) => Ok(Recipient {
            role: None,
            warning: None,
            binding: None,
        }),
    }
}

/// What `check_presence` asks about the machine, injected so tests decide
/// what is running.
pub trait Presence {
    fn pid_alive(&self, pid: u32) -> bool;
    /// The cwd of every running agent session.
    fn session_cwds(&self) -> Vec<String>;
}

/// The real machine: `kill(pid, 0)` and a read-only session scan. Read-only
/// because `scan_sessions` also sweeps stale session files, and a `bus
/// send` must not delete state the TUI reads.
pub struct LivePresence;

impl Presence for LivePresence {
    fn pid_alive(&self, pid: u32) -> bool {
        claudectl_core::process::pid_alive(pid)
    }
    fn session_cwds(&self) -> Vec<String> {
        crate::discovery::scan_sessions_readonly()
            .into_iter()
            .map(|s| s.cwd)
            .collect()
    }
}

/// A caller's path as an absolute path: relative ones are taken against
/// its cwd, since the server's own cwd is where it was launched.
pub fn absolute_path(path: &str, cwd: &std::path::Path) -> PathBuf {
    let p = std::path::Path::new(path);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        cwd.join(p)
    }
}

/// Who a `publish` is from: the role it names, or else the caller's own
/// role when that resolves to exactly one (#527). Ambiguous or unbound stays
/// anonymous rather than guessing.
fn sender_for(
    conn: &rusqlite::Connection,
    explicit: Option<String>,
    cwd: &std::path::Path,
) -> Option<String> {
    explicit.or_else(|| match roles::resolve(conn, None, cwd) {
        Ok(RoleResolution::Resolved(r)) => Some(r.name),
        _ => None,
    })
}

/// Run the bus MCP server on stdio until the peer disconnects.
pub fn run_stdio() -> Result<(), String> {
    let conn = store::open()?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("build tokio runtime: {e}"))?;
    runtime.block_on(async move {
        let server = BusServer::new(conn);
        let running = server
            .serve(stdio())
            .await
            .map_err(|e| format!("serve stdio: {e}"))?;
        running
            .waiting()
            .await
            .map_err(|e| format!("server loop: {e}"))?;
        Ok::<(), String>(())
    })
}

#[cfg(test)]
mod sender_tests {
    use super::sender_for;
    use crate::bus::store;
    use std::path::Path;

    /// A reply from an agent that didn't name itself used to arrive with no
    /// sender at all; it's the caller's own role now (#527).
    #[test]
    fn an_unnamed_sender_is_the_callers_role() {
        let conn = store::open_memory();
        store::upsert_role(&conn, "codex-worker", "/work/codex", None, None).unwrap();
        assert_eq!(
            sender_for(&conn, None, Path::new("/work/codex/sub")).as_deref(),
            Some("codex-worker")
        );
        assert_eq!(
            sender_for(&conn, Some("explicit".into()), Path::new("/work/codex")).as_deref(),
            Some("explicit"),
            "a named sender wins"
        );
        assert_eq!(sender_for(&conn, None, Path::new("/elsewhere")), None);
    }

    #[test]
    fn an_ambiguous_caller_stays_anonymous() {
        let conn = store::open_memory();
        store::upsert_role(&conn, "a", "/shared", None, None).unwrap();
        store::upsert_role(&conn, "b", "/shared", None, None).unwrap();
        assert_eq!(sender_for(&conn, None, Path::new("/shared")), None);
    }
}

#[cfg(test)]
mod recipient_tests {
    use super::recipient_for;
    use crate::bus::store;
    use std::path::Path;

    fn db() -> rusqlite::Connection {
        let conn = store::open_memory();
        store::upsert_role(&conn, "impl", "/repo", None, None).unwrap();
        store::upsert_role(&conn, "relay", "/repo/src/relay", None, None).unwrap();
        conn
    }

    #[test]
    fn a_path_is_routed_to_its_owner() {
        let conn = db();
        let r =
            recipient_for(&conn, None, Some("/repo/src/relay/peer.rs"), Path::new("/")).unwrap();
        assert_eq!(r.role.as_deref(), Some("relay"));
        assert_eq!(r.warning, None);
        let r = recipient_for(&conn, None, Some("src/bus/mcp.rs"), Path::new("/repo")).unwrap();
        assert_eq!(
            r.role.as_deref(),
            Some("impl"),
            "a relative path is taken against the caller's cwd"
        );
    }

    #[test]
    fn a_path_nobody_owns_is_refused_not_queued() {
        let conn = db();
        let err = recipient_for(&conn, None, Some("/elsewhere/x.rs"), Path::new("/"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("no role owns /elsewhere/x.rs"), "{err}");
        assert!(
            err.contains("bus role bind"),
            "the refusal says how to fix it: {err}"
        );
    }

    #[test]
    fn a_path_with_two_equal_owners_names_them() {
        let conn = db();
        store::upsert_role(&conn, "impl2", "/repo", None, None).unwrap();
        let err = recipient_for(&conn, None, Some("/repo/README.md"), Path::new("/"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("impl") && err.contains("impl2"), "{err}");
        // The more specific owner is still unambiguous.
        let r = recipient_for(&conn, None, Some("/repo/src/relay/a.rs"), Path::new("/")).unwrap();
        assert_eq!(r.role.as_deref(), Some("relay"));
    }

    #[test]
    fn a_never_bound_name_is_queued_with_a_warning() {
        let conn = db();
        let r = recipient_for(&conn, Some("ghost"), None, Path::new("/")).unwrap();
        assert_eq!(r.role.as_deref(), Some("ghost"), "still queued");
        assert!(
            r.warning
                .as_deref()
                .unwrap()
                .contains("no role 'ghost' is bound")
        );
    }

    struct Fake {
        alive: bool,
        cwds: Vec<&'static str>,
    }

    impl super::Presence for Fake {
        fn pid_alive(&self, _pid: u32) -> bool {
            self.alive
        }
        fn session_cwds(&self) -> Vec<String> {
            self.cwds.iter().map(|c| c.to_string()).collect()
        }
    }

    #[test]
    fn a_role_with_nobody_home_warns_by_name_and_by_path() {
        let conn = db();
        store::upsert_role(&conn, "gone", "/repo/src/gone", None, Some(4_000_001)).unwrap();
        let empty = Fake {
            alive: false,
            cwds: vec!["/other/project"],
        };
        let by_name = recipient_for(&conn, Some("gone"), None, Path::new("/"))
            .unwrap()
            .check_presence(&empty);
        assert_eq!(by_name.role.as_deref(), Some("gone"), "still queued");
        assert!(
            by_name
                .warning
                .as_deref()
                .unwrap()
                .contains("pid 4000001, which is not running"),
            "{:?}",
            by_name.warning
        );
        let by_path = recipient_for(&conn, None, Some("/repo/src/gone/a.rs"), Path::new("/"))
            .unwrap()
            .check_presence(&empty);
        assert_eq!(by_path.role.as_deref(), Some("gone"));
        assert!(
            by_path.warning.is_some(),
            "a dead owner warns on the routed path too"
        );
        // Before the presence check there is no warning: that step is
        // deferred until the limiter has admitted the message.
        let before = recipient_for(&conn, Some("gone"), None, Path::new("/")).unwrap();
        assert_eq!(before.warning, None);
        // The same binding with its process alive is silent.
        let live = Fake {
            alive: true,
            cwds: vec![],
        };
        let ok = recipient_for(&conn, Some("gone"), None, Path::new("/"))
            .unwrap()
            .check_presence(&live);
        assert_eq!(ok.warning, None);
        // A cwd-only binding has no pid to be dead.
        let cwd_only = recipient_for(&conn, Some("relay"), None, Path::new("/"))
            .unwrap()
            .check_presence(&empty);
        assert_eq!(cwd_only.warning, None);
    }

    #[test]
    fn a_stale_pid_does_not_warn_while_a_session_sits_in_the_bound_directory() {
        // `/role impl` bound pid 4321 and exited; the operator then re-bound
        // `impl` by directory, which keeps the old pid. A session working
        // in that directory drains the inbox by cwd inference, so there is
        // someone home and no warning.
        let conn = db();
        store::upsert_role(&conn, "impl2", "/repo/src/bus", None, Some(4321)).unwrap();
        let someone = Fake {
            alive: false,
            cwds: vec!["/repo/src/bus/deep"],
        };
        let r = recipient_for(&conn, Some("impl2"), None, Path::new("/"))
            .unwrap()
            .check_presence(&someone);
        assert_eq!(r.warning, None);
        let nobody = Fake {
            alive: false,
            cwds: vec!["/repo/src/busy"],
        };
        let r = recipient_for(&conn, Some("impl2"), None, Path::new("/"))
            .unwrap()
            .check_presence(&nobody);
        assert!(
            r.warning.is_some(),
            "a sibling directory is not the bound one"
        );
    }

    #[test]
    fn a_bound_or_reserved_name_does_not_warn() {
        let conn = db();
        for name in ["relay", "supervisor", "operator"] {
            let r = recipient_for(&conn, Some(name), None, Path::new("/")).unwrap();
            assert_eq!(r.role.as_deref(), Some(name));
            assert_eq!(r.warning, None, "{name} should not warn");
        }
        // The mailbox is keyed on the exact name and `validate_role_name`
        // refuses every casing, so `Operator` can be neither bound nor
        // drained: it is the unbound case, not the reserved one.
        let r = recipient_for(&conn, Some("Operator"), None, Path::new("/")).unwrap();
        assert!(
            r.warning
                .as_deref()
                .unwrap()
                .contains("no role 'Operator' is bound")
        );
    }

    #[test]
    fn naming_both_is_an_error_and_neither_is_a_broadcast() {
        let conn = db();
        assert!(recipient_for(&conn, Some("impl"), Some("/repo"), Path::new("/")).is_err());
        let r = recipient_for(&conn, None, None, Path::new("/")).unwrap();
        assert_eq!(r.role, None);
        assert_eq!(r.warning, None);
    }

    #[test]
    fn the_refusal_names_the_same_path_owner_of_reports() {
        // Both surfaces answer "who owns this"; they must name the path the
        // same way. `..` is folded in both.
        let conn = db();
        let err = recipient_for(&conn, None, Some("/repo/../elsewhere/x.rs"), Path::new("/"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("no role owns /elsewhere/x.rs"), "{err}");
        assert!(matches!(
            crate::bus::roles::owner_of_path(&conn, Path::new("/repo/../elsewhere/x.rs")).unwrap(),
            crate::bus::roles::Ownership::Unowned { path } if path == "/elsewhere/x.rs"
        ));
    }
}
