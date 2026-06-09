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
//! * `read_inbox` — drain pending directed messages for the caller's role.
//!
//! All four tools share one in-process bus state struct so the SQLite
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
use super::rate_limit::RateLimiter;
use super::roles::{self, RoleResolution};
use super::store::{self, MessageRow};

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
        let conn = self.conn.lock().expect("bus db mutex poisoned");
        let mut out = Vec::with_capacity(sessions.len());
        for s in sessions {
            let cwd_path = PathBuf::from(&s.cwd);
            let role = match roles::resolve(&conn, None, &cwd_path) {
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

        // Rate limit per sender role. Anonymous sends (no role) skip the
        // limiter — that path is rare and not the vector RFC v2 §9 calls
        // out; the reserved-role guard handles the privileged endpoints.
        if let Some(sender) = args.sender_role.as_deref() {
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

        let sanitized_body = policy::sanitize_body(&args.body);
        let sanitized = sanitized_body != args.body;
        let priority = args.priority.as_deref().unwrap_or("normal");

        let conn = self.conn.lock().expect("bus db mutex poisoned");
        let id = store::insert_message(
            &conn,
            &args.subject,
            &args.msg_type,
            args.sender_role.as_deref(),
            args.addressed_to.as_deref(),
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
                "claudectl agent bus. Use list_agents to discover peers, whoami for the \
                 caller's role, publish to send a directed message, read_inbox to drain \
                 your mailbox. See docs/AGENT_BUS.md."
                    .into(),
            ),
            capabilities: ServerCapabilities::builder().enable_tools().build(),
            ..Default::default()
        }
    }
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
