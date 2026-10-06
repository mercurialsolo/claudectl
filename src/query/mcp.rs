//! The MCP half of the query surface — for a Claude (#429, RFC §4.7).
//!
//! Three tools over the same [`QueryCore`] the HTTP surface uses, exposed on
//! stdio exactly as `src/bus/mcp.rs` exposes the agent bus: an rmcp
//! `ToolRouter` held as a field, `#[tool]` methods returning
//! `Result<Json<T>, McpError>`, and a current-thread Tokio runtime built
//! inside [`run_stdio`] so the rest of the binary stays synchronous.
//!
//! ## What this is, and what §4.7 sketches
//!
//! The RFC writes the MCP half as
//! `claudectl query stdio --token … --endpoint <url>` — a *client* running on
//! the third party's machine, forwarding to the owner's HTTP server. That is a
//! separate artifact: it needs an outbound HTTP client, URL handling, and it
//! holds no index and no grant store, because the owner's server is what
//! authorizes.
//!
//! This is the other half of that pair, and the half #429 needs: the tools
//! served locally against the owner's own index, authenticated by the same
//! capability token. It satisfies "a grant token asks a question over both
//! surfaces" without inventing an HTTP client — and it is what the remote
//! client will eventually proxy *to*, through the three HTTP routes in
//! `http.rs`. The `--endpoint` form is a follow-up.
//!
//! ## Why the token, on a local server
//!
//! A local stdio server could skip authorization — whoever can run the binary
//! can read the repository anyway. It does not, for two reasons. The scope
//! check is where `project.docs` is told from `project.query`, so skipping it
//! would make the MCP half strictly more permissive than the HTTP half for the
//! same grant. And the audit trail is the owner's record of what a grant
//! asked; a surface that answers without recording would put a hole in it.

use std::sync::Arc;

use rmcp::ErrorData as McpError;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::tool::Parameters;
use rmcp::handler::server::wrapper::Json;
use rmcp::model::{ServerCapabilities, ServerInfo};
use rmcp::transport::io::stdio;
use rmcp::{ServerHandler, ServiceExt, tool, tool_handler, tool_router};
use schemars::JsonSchema;
use serde::Deserialize;

use super::core::{Answer, Document, QueryCore, QueryError, Topics};

#[derive(Debug, Deserialize, JsonSchema)]
pub struct AskProjectArgs {
    /// The question, in natural language.
    pub question: String,
    /// How many spans to return. Defaults to 5, capped at 20.
    #[serde(default)]
    pub limit: Option<usize>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ListTopicsArgs {}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GetDocArgs {
    /// A work-tree-relative path as `list_topics` reports it.
    pub path: String,
}

/// Translate a core error into an MCP error.
///
/// `Denied` keeps the opaque wording the HTTP surface sends with its `404`:
/// RFC §3.3 does not want a caller able to tell "you lack the scope" from "it
/// does not exist", and that reasoning does not change with the transport.
fn to_mcp(err: QueryError) -> McpError {
    match err {
        QueryError::Denied => McpError::invalid_params("not found".to_string(), None),
        QueryError::BadRequest(msg) => McpError::invalid_params(msg, None),
        QueryError::Internal(msg) => McpError::internal_error(msg, None),
    }
}

/// The MCP server. One project, one grant, for the life of the process.
pub struct QueryMcpServer {
    core: Arc<QueryCore>,
    token: String,
    tool_router: ToolRouter<Self>,
}

impl QueryMcpServer {
    pub fn new(core: Arc<QueryCore>, token: String) -> Self {
        QueryMcpServer {
            core,
            token,
            tool_router: Self::tool_router(),
        }
    }
}

#[tool_router]
impl QueryMcpServer {
    #[tool(
        description = "Ask a natural-language question about the project. Returns verbatim spans \
                       from its published documentation and module map, each with its source path \
                       and heading. Nothing is generated: a span is either in the project's index \
                       or it is not returned."
    )]
    async fn ask_project(
        &self,
        Parameters(args): Parameters<AskProjectArgs>,
    ) -> Result<Json<Answer>, McpError> {
        self.core
            .ask(&self.token, &args.question, args.limit)
            .map(Json)
            .map_err(to_mcp)
    }

    #[tool(
        description = "List everything this project can be asked about — documentation headings, \
                       module paths, published skills — with no bodies. Call this first: it is \
                       the table of contents a question should be aimed at."
    )]
    async fn list_topics(
        &self,
        Parameters(_): Parameters<ListTopicsArgs>,
    ) -> Result<Json<Topics>, McpError> {
        self.core.topics(&self.token).map(Json).map_err(to_mcp)
    }

    #[tool(
        description = "Retrieve every indexed section of one documentation file, verbatim. The \
                       path is looked up against the index, never opened from disk, so a path \
                       that is not published is indistinguishable from one that does not exist."
    )]
    async fn get_doc(
        &self,
        Parameters(args): Parameters<GetDocArgs>,
    ) -> Result<Json<Document>, McpError> {
        self.core
            .get_doc(&self.token, &args.path)
            .map(Json)
            .map_err(to_mcp)
    }
}

#[tool_handler]
impl ServerHandler for QueryMcpServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            instructions: Some(format!(
                "Read-only query access to the project {:?}. Call list_topics first to see what \
                 is answerable, then ask_project. Answers are verbatim spans with citations, \
                 never generated prose — if something is not in the project's published \
                 documentation, it is not available here. No tool on this server can write, \
                 execute, or delegate anything.",
                self.core.project()
            )),
            capabilities: ServerCapabilities::builder().enable_tools().build(),
            ..Default::default()
        }
    }
}

/// Serve the query tools on stdio until the peer disconnects.
///
/// Builds its own current-thread runtime, as `bus::mcp::run_stdio` does: the
/// `tokio` dependency carries no `rt-multi-thread`, and keeping the runtime
/// inside this call is what preserves "the TUI and every other code path
/// remain sync."
pub fn run_stdio(core: Arc<QueryCore>, token: String) -> Result<(), String> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("build tokio runtime: {e}"))?;
    runtime.block_on(async move {
        let server = QueryMcpServer::new(core, token);
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
