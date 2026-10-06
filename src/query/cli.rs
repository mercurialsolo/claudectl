//! `claudectl query` — serve the read-only query surface (#429).

use std::io;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use clap::Subcommand;

use crate::access::{self, GrantStore, scope};
use crate::context::{self, ContextIndex};

use super::core::QueryCore;
use super::http::QueryServer;

#[derive(Subcommand)]
pub enum QueryCommand {
    /// Serve read-only project queries over HTTP for grant holders.
    ///
    /// One process serves one project — the repository this is started in.
    /// The index is built once at startup, so restart to pick up new commits.
    Serve {
        /// Address to bind. Loopback by default: this is plaintext HTTP, so
        /// anything reachable off-machine belongs behind a TLS terminator.
        #[arg(long, default_value = "127.0.0.1")]
        addr: String,

        /// Port to bind. 0 picks a free one and prints it.
        #[arg(long, default_value_t = 8787)]
        port: u16,

        /// The project name grants are scoped to. Defaults to the repository
        /// root's directory name.
        #[arg(long)]
        project: Option<String>,
    },

    /// Expose the query surface to a local Claude as MCP tools over stdio.
    #[cfg(feature = "bus")]
    Stdio {
        /// A capability token minted by `claudectl access grant`.
        #[arg(long)]
        token: String,

        /// The project name the token is scoped to. Defaults to the
        /// repository root's directory name.
        #[arg(long)]
        project: Option<String>,
    },
}

pub fn dispatch_command(command: &QueryCommand, json_mode: bool) -> io::Result<()> {
    dispatch(command, json_mode).map_err(io::Error::other)
}

fn dispatch(command: &QueryCommand, json_mode: bool) -> Result<(), String> {
    match command {
        QueryCommand::Serve {
            addr,
            port,
            project,
        } => serve(addr, *port, project.as_deref(), json_mode),

        #[cfg(feature = "bus")]
        QueryCommand::Stdio { token, project } => {
            let core = build_core(project.as_deref())?;
            super::mcp::run_stdio(Arc::new(core), token.clone())
        }
    }
}

/// Decide the project name this process will answer to.
///
/// `--project` wins. Otherwise the repository root's directory name, which is
/// the same thing `session.project_name` uses and so the name an operator will
/// reach for when minting a grant.
///
/// The default can legitimately fail. A scope qualifier is restricted to
/// `[A-Za-z0-9._-]`, and plenty of real directory names are not — every
/// worktree this repo's own tooling creates is called something like
/// `feat+readonly-query-surface`, and `+` is not in that set. Refusing to
/// start with a pointer to `--project` is the honest answer: silently
/// sanitizing the name would produce a server answering to a name no grant was
/// ever minted for.
pub fn resolve_project(start: &Path, requested: Option<&str>) -> Result<String, String> {
    if let Some(name) = requested {
        scope::validate_qualifier(name)
            .map_err(|e| format!("--project {name:?} is not a usable project name: {e}"))?;
        return Ok(name.to_string());
    }
    let root = context::git::repo_root(start).map_err(|e| e.to_string())?;
    let basename = root
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| format!("cannot read a project name from {}", root.display()))?;
    scope::validate_qualifier(basename).map_err(|e| {
        format!(
            "the repository directory is named {basename:?}, which cannot be a scope \
             qualifier ({e}). Pass --project <name> with the name your grants are \
             scoped to."
        )
    })?;
    Ok(basename.to_string())
}

/// Build the index and the grant store this process will serve from.
fn build_core(project: Option<&str>) -> Result<QueryCore, String> {
    let cwd = std::env::current_dir().map_err(|e| format!("cwd: {e}"))?;
    let project = resolve_project(&cwd, project)?;
    let index = ContextIndex::build(&cwd).map_err(|e| e.to_string())?;
    let store = GrantStore::open_default()?;
    let secret = access::token::load_or_create_secret(store.root())?;
    Ok(QueryCore::new(project, Arc::new(index), store, secret))
}

fn serve(addr: &str, port: u16, project: Option<&str>, json_mode: bool) -> Result<(), String> {
    let core = build_core(project)?;
    let stats = core.index().stats.clone();
    let project = core.project().to_string();
    let fingerprint = core.index().fingerprint();
    let empty = core.index().is_empty();
    let mode = context::exposure::mode_from_config();

    let bind: SocketAddr = format!("{addr}:{port}")
        .parse()
        .map_err(|_| format!("cannot parse {addr}:{port} as an address"))?;

    // Same warning the coordinator and the metrics exporter give. Plaintext
    // HTTP carrying a bearer token has no transport confidentiality, and this
    // surface has no rate limiting until #431.
    if claudectl_core::helpers::is_exposed_bind(&bind) {
        eprintln!(
            "warning: query surface bound to {bind}, which is reachable off this machine. \
             This is plaintext HTTP with a bearer token and no rate limiting — put it behind \
             a TLS terminator, or bind 127.0.0.1 and tunnel."
        );
    }

    // Held for the life of the process: dropping it stops the accept loop.
    let server = QueryServer::start(bind, Arc::new(core)).map_err(|e| format!("bind: {e}"))?;
    let listening = server.addr;

    if json_mode {
        let summary = serde_json::json!({
            "project": project,
            "addr": listening.to_string(),
            "fingerprint": fingerprint,
            "share_mode": mode.label(),
            "empty": empty,
            "tracked_files": stats.tracked_files,
            "denied": stats.denied,
            "unreadable": stats.unreadable,
            "categories_hidden": stats.categories_hidden,
        });
        println!("{summary}");
    } else {
        println!("query surface for {project:?} listening on http://{listening}");
        println!("  index: {fingerprint}");
        println!(
            "  {} tracked files, {} denied, {} unreadable",
            stats.tracked_files, stats.denied, stats.unreadable
        );
        println!("  share mode: {}", mode.label());
        if !stats.categories_hidden.is_empty() {
            println!(
                "  hidden by exposure: {}",
                stats.categories_hidden.join(", ")
            );
        }
        if empty {
            // An empty index answers every question with nothing, which looks
            // identical to a broken grant from the caller's side. Say so here
            // rather than letting the operator debug it from the far end.
            eprintln!(
                "warning: the index is empty — every question will return no spans. \
                 Check the share mode and the hidden categories above."
            );
        }
        println!("  POST /api/v1/project/{project}/query");
        println!("  GET  /api/v1/project/{project}/topics");
        println!("  POST /api/v1/project/{project}/doc");
        println!("The index is a startup snapshot; restart to pick up new commits.");
    }

    // The accept loop owns the work; this thread only keeps the process — and
    // so `server` — alive. Exiting is the operator's signal, as with
    // `relay serve` and the metrics exporter.
    loop {
        std::thread::sleep(Duration::from_secs(60));
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Tests
// ────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_explicit_project_is_validated_not_sanitized() {
        let here = std::env::current_dir().unwrap();
        assert_eq!(
            resolve_project(&here, Some("claudectl")).unwrap(),
            "claudectl"
        );
        let err = resolve_project(&here, Some("feat+branch")).unwrap_err();
        assert!(err.contains("not a usable project name"), "got: {err}");
    }

    #[test]
    fn the_default_project_is_the_repository_directory_name() {
        let Some((_dir, root)) = crate::context::tests_support::git_fixture(&[("a.md", "# a\n")])
        else {
            eprintln!("skipping: git unavailable");
            return;
        };
        let expected = root.file_name().unwrap().to_str().unwrap().to_string();
        assert_eq!(resolve_project(&root, None).unwrap(), expected);
    }

    #[test]
    fn a_directory_name_that_cannot_be_a_scope_qualifier_demands_an_explicit_project() {
        let Some(parent) = tempfile::tempdir().ok() else {
            return;
        };
        let root = parent.path().join("feat+readonly-query-surface");
        std::fs::create_dir_all(&root).unwrap();
        if !crate::context::tests_support::git_init(&root) {
            eprintln!("skipping: git unavailable");
            return;
        }
        let err = resolve_project(&root, None).unwrap_err();
        assert!(
            err.contains("--project"),
            "the error must name the way out, got: {err}"
        );
        assert!(err.contains("feat+readonly-query-surface"), "got: {err}");
    }
}
