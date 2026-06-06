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

use super::policy::{self, DEFAULT_MAX_BODY_BYTES};
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
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct PublishResult {
    pub message_id: String,
    pub sanitized: bool,
}

#[derive(Debug, Default, Deserialize, JsonSchema)]
pub struct ReadInboxArgs {
    #[serde(default)]
    pub role: Option<String>,
    /// ISO timestamp. Only messages created after this are returned.
    #[serde(default)]
    pub since: Option<String>,
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
        }
    }
}

// -------------------- Server state -------------------------------------------

pub struct BusServer {
    conn: Mutex<Connection>,
    tool_router: ToolRouter<Self>,
}

impl BusServer {
    pub fn new(conn: Connection) -> Self {
        Self {
            conn: Mutex::new(conn),
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
        )
        .map_err(|e| McpError::internal_error(e, None))?;
        Ok(Json(PublishResult {
            message_id: id,
            sanitized,
        }))
    }

    #[tool(description = "Drain pending directed messages addressed to the caller's role.")]
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
        let drained = store::drain_inbox(&mut conn, &resolved, args.since.as_deref())
            .map_err(|e| McpError::internal_error(e, None))?;
        Ok(Json(ReadInboxResult {
            role: Some(resolved),
            messages: drained.into_iter().map(InboxEntry::from).collect(),
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
