//! The HTTP half of the query surface — for a human or a script
//! (#429, open-cluster RFC §4.7).
//!
//! Raw TCP HTTP/1.1, no framework, mirroring `src/relay/http.rs` and
//! `src/coord/exporter.rs`. The dependency budget is the reason those two are
//! hand-rolled and it is the reason this one is; the request shapes here are a
//! request line, a header map and a small JSON body, which is not enough to
//! justify a web stack.
//!
//! What is *not* copied from `relay/http.rs` is its auth placement. Relay
//! compares one global bearer token before it has looked at the path, because
//! every route shares the same secret. Here the token is a capability with
//! scopes, and which scope a request needs depends on which operation it is —
//! so the route is parsed first and the token is verified against the
//! operation, through [`QueryCore`], which owns every policy decision. This
//! file maps bytes to calls and calls to statuses, and nothing else.
//!
//! ## Routes
//!
//! ```text
//! POST /api/v1/project/<project>/query     {"question": "...", "limit": 5}
//! GET  /api/v1/project/<project>/topics
//! POST /api/v1/project/<project>/doc       {"path": "docs/x.md"}
//! GET  /api/v1/project/<project>/escalation/<esc_id>
//! ```
//!
//! §4.7 lists only the first two. `doc` is added so the three MCP tools have
//! three HTTP equivalents — without it, the remote MCP client §4.7 sketches
//! would have no endpoint to forward `get_doc` to. It is a POST because its
//! argument is a caller-supplied path-shaped string: carrying that in a JSON
//! body avoids percent-decoding and query-string parsing on the one input that
//! most wants neither.
//!
//! Every route is a read. The only write this surface performs anywhere is the
//! grant's own `use_count` and audit line (see [`QueryCore`]).

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read as IoRead, Write as IoWrite};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crate::access::scope;

use super::core::{self as qcore, QueryCore, QueryError};

/// Maximum request body. Two small JSON objects are the only bodies this
/// surface accepts, so the cap is the question cap plus room for the envelope
/// rather than relay's 1 MB.
const MAX_BODY_SIZE: usize = qcore::MAX_QUESTION_BYTES + 1024;

/// Prefix every route shares.
const ROUTE_PREFIX: &str = "/api/v1/project/";

/// A body identical for every refusal that must not distinguish itself.
///
/// RFC §3.3: a missing scope returns `404`, not `403`, so an unauthorized
/// caller cannot enumerate which projects exist. The same reasoning extends to
/// the route — "wrong project", "no such route" and "denied" are one answer,
/// byte for byte.
const NOT_FOUND: &str = r#"{"error":"not found"}"#;

/// A running query server.
pub struct QueryServer {
    pub addr: SocketAddr,
    shutdown: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl QueryServer {
    /// Bind and start serving `core` on a background thread.
    ///
    /// Binding happens on the calling thread so a port conflict surfaces as an
    /// error from `start` rather than vanishing into a detached thread, and so
    /// `addr` is a real port when the caller passed `:0`.
    pub fn start(addr: SocketAddr, core: Arc<QueryCore>) -> std::io::Result<Self> {
        let listener = TcpListener::bind(addr)?;
        let local_addr = listener.local_addr()?;
        let shutdown = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&shutdown);

        let handle = std::thread::Builder::new()
            .name("query-http".into())
            .spawn(move || {
                let _ = listener.set_nonblocking(true);
                loop {
                    if flag.load(Ordering::Relaxed) {
                        break;
                    }
                    match listener.accept() {
                        Ok((stream, _)) => {
                            let core = Arc::clone(&core);
                            std::thread::spawn(move || handle_connection(stream, &core));
                        }
                        Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(10));
                        }
                        Err(_) => {
                            std::thread::sleep(Duration::from_millis(500));
                        }
                    }
                }
            })?;

        Ok(QueryServer {
            addr: local_addr,
            shutdown,
            handle: Some(handle),
        })
    }

    /// Stop accepting and wait for the accept loop to exit.
    pub fn stop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

impl Drop for QueryServer {
    fn drop(&mut self) {
        self.stop();
    }
}

/// The three operations, as named in a route's last segment.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Route {
    Query,
    Topics,
    Doc,
    /// `GET …/escalation/<id>` — the one route with a path parameter (#446).
    Escalation(String),
}

/// Split a request target into `(project, route)`.
///
/// Returns `None` for anything that is not one of the three routes, including
/// a deeper or shallower path. The query string is discarded: no route takes
/// one, and silently tolerating `?x=y` would make two spellings of one route.
fn parse_route(method: &str, target: &str) -> Option<(String, Route)> {
    let path = target.split('?').next().unwrap_or(target);
    let rest = path.strip_prefix(ROUTE_PREFIX)?;
    let mut segments = rest.split('/');
    let project = segments.next()?;
    let action = segments.next()?;
    // `escalation` is the one route taking a path parameter, so it is the one
    // place a third segment is allowed — and exactly one, still rejecting
    // anything deeper. Every other route stays strictly two-segment.
    let tail = segments.next();
    if segments.next().is_some() {
        return None;
    }
    let route = match (method, action, tail) {
        ("POST", "query", None) => Route::Query,
        ("GET", "topics", None) => Route::Topics,
        ("POST", "doc", None) => Route::Doc,
        ("GET", "escalation", Some(id)) if !id.is_empty() => Route::Escalation(id.to_string()),
        _ => return None,
    };
    Some((project.to_string(), route))
}

/// Pull the bare capability token out an `Authorization: Bearer …` header.
///
/// Returns `None` when the header is missing or not a bearer, which is the one
/// failure answered with `401` rather than `404`: it is decided before the
/// request's project is looked at, so it says nothing about what exists.
fn bearer_token(headers: &HashMap<String, String>) -> Option<&str> {
    let value = headers.get("authorization")?;
    let token = value.strip_prefix("Bearer ")?.trim();
    if token.is_empty() { None } else { Some(token) }
}

fn handle_connection(mut stream: TcpStream, core: &Arc<QueryCore>) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let peer = match stream.try_clone() {
        Ok(s) => s,
        Err(_) => return,
    };
    let mut reader = BufReader::new(peer);

    let mut request_line = String::new();
    if reader.read_line(&mut request_line).is_err() {
        return;
    }
    let parts: Vec<&str> = request_line.split_whitespace().collect();
    if parts.len() < 2 {
        send(&mut stream, 400, r#"{"error":"bad request"}"#);
        return;
    }
    let (method, target) = (parts[0], parts[1]);

    let mut headers: HashMap<String, String> = HashMap::new();
    let mut content_length = 0usize;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).is_err() {
            break;
        }
        let trimmed = line.trim_end();
        if trimmed.is_empty() {
            break;
        }
        if let Some((k, v)) = trimmed.split_once(':') {
            let key = k.trim().to_lowercase();
            let val = v.trim().to_string();
            if key == "content-length" {
                content_length = val.parse().unwrap_or(0);
            }
            headers.insert(key, val);
        }
    }

    // 1. Route shape. The action names are public, so an unmatched route is a
    //    404 that leaks nothing and costs no authorization work.
    let Some((project, route)) = parse_route(method, target) else {
        send(&mut stream, 404, NOT_FOUND);
        return;
    };

    // 2. Credentials, before anything project-specific is decided — so every
    //    unauthenticated request to every path gets the same 401.
    let Some(token) = bearer_token(&headers) else {
        send(&mut stream, 401, r#"{"error":"unauthorized"}"#);
        return;
    };

    // 3. The project segment's charset, before it is compared to anything.
    //    `validate_qualifier` is the same gate that governs a scope's
    //    qualifier, so a name that cannot appear in a scope cannot appear here
    //    either — and it rejects `/`, `:` and `..` outright.
    if scope::validate_qualifier(&project).is_err() {
        send(&mut stream, 400, r#"{"error":"invalid project name"}"#);
        return;
    }

    // 4. Is this the project this process serves? Compared, never resolved.
    //
    //    This runs before authorization deliberately. `QueryCore::authorize`
    //    derives the required scope from the *served* project, so the order
    //    cannot affect who gets in; what it affects is the audit log. Verifying
    //    first would write an `allowed` line and bump `use_count` for a request
    //    that then returned 404, and an audit trail claiming a query was
    //    answered when it was not is worse than one probe going unrecorded.
    if !core.serves(&project) {
        send(&mut stream, 404, NOT_FOUND);
        return;
    }

    let body = if method == "POST" {
        match read_body(&mut reader, content_length) {
            Ok(b) => b,
            Err(msg) => {
                send(&mut stream, 400, &format!(r#"{{"error":"{msg}"}}"#));
                return;
            }
        }
    } else {
        Vec::new()
    };

    let result = match route {
        Route::Query => serve_query(core, token, &body),
        Route::Topics => core.topics(token).and_then(encode).map(|j| (200, j)),
        Route::Doc => serve_doc(core, token, &body),
        Route::Escalation(id) => core
            .poll_escalation(token, &id)
            .and_then(|s| encode(&s).map(|j| (s.http_status(), j))),
    };

    match result {
        Ok((status, json)) => send(&mut stream, status, &json),
        Err(QueryError::Denied) => send(&mut stream, 404, NOT_FOUND),
        Err(QueryError::BadRequest(msg)) => {
            send(
                &mut stream,
                400,
                &format!(r#"{{"error":"{}"}}"#, escape(&msg)),
            );
        }
        Err(QueryError::Internal(_)) => {
            // The detail stays in the operator's logs. An internal message can
            // name a path or a grant id, and this is a third-party endpoint.
            send(&mut stream, 500, r#"{"error":"internal error"}"#);
        }
        Err(QueryError::Throttled {
            message,
            retry_after_secs,
        }) => {
            // 429, and the two causes are distinguishable. The caller has
            // already proved they hold a valid in-scope token, so naming their
            // own limit leaks nothing §3.3 protects — and a grant that goes
            // silently quiet is worse for the holder than one that says why.
            let headers = retry_after_secs.map(|s| format!("Retry-After: {s}\r\n"));
            send_with_headers(
                &mut stream,
                429,
                &format!(r#"{{"error":"{}"}}"#, escape(message)),
                headers.as_deref(),
            );
        }
    }
}

#[derive(serde::Deserialize)]
struct QueryBody {
    question: String,
    #[serde(default)]
    limit: Option<usize>,
}

#[derive(serde::Deserialize)]
struct DocBody {
    path: String,
}

/// `(status, body)`, because an escalated query answers `202` and everything
/// else answers `200`. The discriminant is in the envelope either way — see
/// [`qcore::Answer::http_status`].
fn serve_query(core: &QueryCore, token: &str, body: &[u8]) -> Result<(u16, String), QueryError> {
    let parsed: QueryBody = serde_json::from_slice(body)
        .map_err(|_| QueryError::BadRequest("expected {\"question\": \"...\"}".into()))?;
    let answer = core.ask(token, &parsed.question, parsed.limit)?;
    let status = answer.http_status();
    Ok((status, encode(answer)?))
}

fn serve_doc(core: &QueryCore, token: &str, body: &[u8]) -> Result<(u16, String), QueryError> {
    let parsed: DocBody = serde_json::from_slice(body)
        .map_err(|_| QueryError::BadRequest("expected {\"path\": \"...\"}".into()))?;
    core.get_doc(token, &parsed.path)
        .and_then(encode)
        .map(|j| (200, j))
}

fn encode<T: serde::Serialize>(value: T) -> Result<String, QueryError> {
    serde_json::to_string(&value).map_err(|e| QueryError::Internal(format!("serialize: {e}")))
}

fn read_body(reader: &mut BufReader<TcpStream>, content_length: usize) -> Result<Vec<u8>, String> {
    if content_length == 0 {
        return Err("missing request body".into());
    }
    if content_length > MAX_BODY_SIZE {
        return Err("request body too large".into());
    }
    let mut body = vec![0u8; content_length];
    reader
        .read_exact(&mut body)
        .map(|_| body)
        .map_err(|_| "failed to read body".to_string())
}

/// Minimal escaping for a message interpolated into a JSON error object.
///
/// Error messages here are built from string literals and integers, never from
/// caller input, so this is belt and braces against a future message that is
/// less careful.
fn escape(msg: &str) -> String {
    msg.replace('\\', "\\\\").replace('"', "\\\"")
}

fn send(stream: &mut TcpStream, status: u16, body: &str) {
    send_with_headers(stream, status, body, None);
}

/// [`send`] plus `extra` — already-CRLF-terminated header lines.
///
/// Only `Retry-After` uses it. A separate entry point so the common path stays
/// a three-argument call, and so headers are appended after `Content-Length`
/// rather than spliced near the status line.
fn send_with_headers(stream: &mut TcpStream, status: u16, body: &str, extra: Option<&str>) {
    let reason = match status {
        200 => "OK",
        202 => "Accepted",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        _ => "Error",
    };
    let response = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n{}Connection: close\r\n\r\n{}",
        status,
        reason,
        body.len(),
        extra.unwrap_or(""),
        body
    );
    let _ = stream.write_all(response.as_bytes());
    let _ = stream.flush();
}

// ────────────────────────────────────────────────────────────────────────────
// Tests
// ────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::access::{self, GrantStore, Scope};
    use crate::context::exposure::ExposureState;
    use crate::context::{Category, ContextIndex, IndexExposure, ShareMode};

    /// A server over a fixture repo, plus a real token minted into a temp
    /// grant store. Nothing here touches the operator's `~/.claudectl`.
    struct Fixture {
        server: QueryServer,
        token: String,
        project: String,
        _repo: tempfile::TempDir,
        _store: tempfile::TempDir,
    }

    fn fixture(scopes: Vec<Scope>) -> Option<Fixture> {
        fixture_with_rate_limit(scopes, None)
    }

    /// `fixture`, with the grant's per-minute rate limit overridden.
    fn fixture_with_rate_limit(
        scopes: Vec<Scope>,
        rate_limit_per_min: Option<u32>,
    ) -> Option<Fixture> {
        fixture_full(scopes, rate_limit_per_min, None)
    }

    /// `fixture`, with a classifier wired in (#430).
    fn fixture_classified(
        scopes: Vec<Scope>,
        classifier: crate::query::classify::Classifier,
    ) -> Option<Fixture> {
        fixture_full(scopes, None, Some(classifier))
    }

    fn fixture_full(
        scopes: Vec<Scope>,
        rate_limit_per_min: Option<u32>,
        classifier: Option<crate::query::classify::Classifier>,
    ) -> Option<Fixture> {
        let project = "fixture".to_string();
        let (repo, root) = crate::context::tests_support::git_fixture(&[
            (
                "CLAUDE.md",
                "# claudectl\n\n## Config layering\n\nCLI flags beat TOML.\n",
            ),
            ("docs/terminals.md", "# Terminals\n\nImplement the trait.\n"),
        ])?;
        let mut gate = IndexExposure::all_hidden();
        for c in [Category::ClaudeMd, Category::Docs] {
            gate.set(c, ExposureState::Expose);
        }
        let index = ContextIndex::build_with(&root, &gate, ShareMode::Manual).ok()?;

        let store_dir = tempfile::tempdir().ok()?;
        let store = GrantStore::new(store_dir.path());
        let secret = access::token::load_or_create_secret(store.root()).ok()?;
        let mut grant = access::new_grant(
            "gr_http01".into(),
            "http test".into(),
            scopes,
            access::epoch_ms(),
            access::epoch_ms() + 60_000,
        );
        if let Some(r) = rate_limit_per_min {
            grant.rate_limit_per_min = r;
        }
        store.create(&grant).ok()?;
        let token = access::token::mint(&secret, &grant.grant_id, &grant.scopes, grant.expires_ms);

        let classifier = classifier.unwrap_or_else(|| {
            crate::query::classify::Classifier::inactive("no classifier in this test")
        });
        let core = Arc::new(QueryCore::new(
            project.clone(),
            Arc::new(index),
            store,
            secret,
            classifier,
        ));
        let server = QueryServer::start("127.0.0.1:0".parse().unwrap(), core).ok()?;

        Some(Fixture {
            server,
            token,
            project,
            _repo: repo,
            _store: store_dir,
        })
    }

    /// Send `request` and read until the server closes the connection.
    ///
    /// `relay/http.rs`'s helper reads once into a 4 KB buffer, which silently
    /// truncates anything larger than one packet — and a response carrying
    /// several verbatim spans is routinely larger than that. Reading to close
    /// is correct here because every response sets `Connection: close`.
    ///
    /// The connect is retried until the deadline to close the startup race
    /// where the accept loop has not yet picked the connection up. Safe for
    /// every route on this surface, all of which are reads.
    fn request(port: u16, raw: &str, deadline: Duration) -> String {
        let start = std::time::Instant::now();
        loop {
            if let Ok(mut stream) = TcpStream::connect(("127.0.0.1", port)) {
                let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
                let _ = stream.write_all(raw.as_bytes());
                let _ = stream.flush();
                let mut out = Vec::new();
                let mut buf = [0u8; 4096];
                loop {
                    match stream.read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => out.extend_from_slice(&buf[..n]),
                        Err(_) => break,
                    }
                }
                if !out.is_empty() {
                    return String::from_utf8_lossy(&out).to_string();
                }
            }
            if start.elapsed() >= deadline {
                return String::new();
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn post(port: u16, path: &str, auth: Option<&str>, body: &str) -> String {
        let auth_line = auth.map(|t| format!("Authorization: Bearer {t}\r\n"));
        let raw = format!(
            "POST {} HTTP/1.1\r\nHost: localhost\r\n{}Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            path,
            auth_line.unwrap_or_default(),
            body.len(),
            body
        );
        request(port, &raw, Duration::from_secs(10))
    }

    fn get(port: u16, path: &str, auth: Option<&str>) -> String {
        let auth_line = auth.map(|t| format!("Authorization: Bearer {t}\r\n"));
        let raw = format!(
            "GET {} HTTP/1.1\r\nHost: localhost\r\n{}Connection: close\r\n\r\n",
            path,
            auth_line.unwrap_or_default()
        );
        request(port, &raw, Duration::from_secs(10))
    }

    /// #429's acceptance criterion, end to end over a real socket: a grant
    /// token asks a question and gets cited spans back, a scope mismatch
    /// returns 404, and no route mutates anything.
    #[test]
    fn a_grant_token_asks_a_question_and_gets_cited_spans() {
        let Some(f) = fixture(vec![
            Scope::ProjectQuery("fixture".into()),
            Scope::ProjectDocs("fixture".into()),
        ]) else {
            eprintln!("skipping: git unavailable");
            return;
        };
        let port = f.server.addr.port();
        let path = format!("/api/v1/project/{}/query", f.project);

        let response = post(
            port,
            &path,
            Some(&f.token),
            r#"{"question":"how is config layering done?"}"#,
        );
        assert!(response.contains("200 OK"), "got: {response}");
        assert!(
            response.contains("CLAUDE.md"),
            "expected a citation, got: {response}"
        );
        assert!(
            response.contains("CLI flags beat TOML"),
            "expected the verbatim span, got: {response}"
        );
        assert!(
            response.contains("Config layering"),
            "expected the heading path, got: {response}"
        );
    }

    #[test]
    fn an_escalated_question_answers_202_with_a_pending_id() {
        // The one place the two transports differ: MCP has no status codes, so
        // the envelope carries `status` and HTTP adds `202` on top of it.
        let store_dir = tempfile::tempdir().expect("tempdir");
        let classifier = crate::query::classify::Classifier::with_transport(
            &crate::query::classify::JevSettings::default(),
            store_dir.path(),
            // Middle band: neither confidently answerable nor refusable.
            Box::new(crate::query::classify::test_support::Fake::classifying(
                "structure",
                0.55,
                0.52,
                0.1,
                0.02,
                0.93,
            )),
        );
        let Some(f) = fixture_classified(vec![Scope::ProjectQuery("fixture".into())], classifier)
        else {
            eprintln!("skipping: git unavailable");
            return;
        };
        let response = post(
            f.server.addr.port(),
            &format!("/api/v1/project/{}/query", f.project),
            Some(&f.token),
            r#"{"question":"how does retry work?"}"#,
        );
        assert!(response.starts_with("HTTP/1.1 202 Accepted"), "{response}");
        let json = response.split("\r\n\r\n").nth(1).unwrap_or("").to_string();
        let parsed: serde_json::Value = serde_json::from_str(&json).expect("json");
        assert_eq!(parsed["status"], "pending_review");
        assert!(
            parsed["escalation_id"]
                .as_str()
                .is_some_and(|s| s.starts_with("esc_")),
            "{json}"
        );
        assert!(parsed["spans"].as_array().is_some_and(|a| a.is_empty()));
    }

    #[test]
    fn an_answered_question_carries_the_status_discriminant_too() {
        // Always emitted, including on #429's path: a discriminant that is
        // sometimes absent is worse for a client than one extra field.
        let Some(f) = fixture(vec![Scope::ProjectQuery("fixture".into())]) else {
            eprintln!("skipping: git unavailable");
            return;
        };
        let response = post(
            f.server.addr.port(),
            &format!("/api/v1/project/{}/query", f.project),
            Some(&f.token),
            r#"{"question":"how is config layering done?"}"#,
        );
        assert!(response.starts_with("HTTP/1.1 200 OK"), "{response}");
        let json = response.split("\r\n\r\n").nth(1).unwrap_or("").to_string();
        let parsed: serde_json::Value = serde_json::from_str(&json).expect("json");
        assert_eq!(parsed["status"], "answered");
        // And the fields that only apply to the other outcomes stay absent.
        assert!(parsed.get("declined").is_none());
        assert!(parsed.get("escalation_id").is_none());
    }

    #[test]
    fn topics_and_doc_are_served_over_http() {
        let Some(f) = fixture(vec![
            Scope::ProjectQuery("fixture".into()),
            Scope::ProjectDocs("fixture".into()),
        ]) else {
            eprintln!("skipping: git unavailable");
            return;
        };
        let port = f.server.addr.port();

        let topics = get(
            port,
            &format!("/api/v1/project/{}/topics", f.project),
            Some(&f.token),
        );
        assert!(topics.contains("200 OK"), "got: {topics}");
        assert!(topics.contains("Config layering"), "got: {topics}");
        assert!(
            !topics.contains("CLI flags beat TOML"),
            "topics must carry no bodies, got: {topics}"
        );

        let doc = post(
            port,
            &format!("/api/v1/project/{}/doc", f.project),
            Some(&f.token),
            r#"{"path":"docs/terminals.md"}"#,
        );
        assert!(doc.contains("200 OK"), "got: {doc}");
        assert!(doc.contains("Implement the trait"), "got: {doc}");
    }

    #[test]
    fn a_missing_or_malformed_bearer_is_401() {
        let Some(f) = fixture(vec![Scope::ProjectQuery("fixture".into())]) else {
            eprintln!("skipping: git unavailable");
            return;
        };
        let port = f.server.addr.port();
        let path = format!("/api/v1/project/{}/query", f.project);

        let none = post(port, &path, None, r#"{"question":"x"}"#);
        assert!(none.contains("401"), "got: {none}");

        let empty = post(port, &path, Some(""), r#"{"question":"x"}"#);
        assert!(empty.contains("401"), "got: {empty}");
    }

    #[test]
    fn a_token_scoped_to_another_project_is_404() {
        let Some(f) = fixture(vec![Scope::ProjectQuery("somewhere-else".into())]) else {
            eprintln!("skipping: git unavailable");
            return;
        };
        let port = f.server.addr.port();
        let response = post(
            port,
            &format!("/api/v1/project/{}/query", f.project),
            Some(&f.token),
            r#"{"question":"x"}"#,
        );
        assert!(
            response.contains("404"),
            "a scope mismatch must be 404, not 403 — got: {response}"
        );
        assert!(response.contains("not found"), "got: {response}");
    }

    #[test]
    fn a_route_naming_another_project_is_the_same_404_as_an_unmatched_route() {
        let Some(f) = fixture(vec![Scope::ProjectQuery("fixture".into())]) else {
            eprintln!("skipping: git unavailable");
            return;
        };
        let port = f.server.addr.port();

        let wrong_project = post(
            port,
            "/api/v1/project/other/query",
            Some(&f.token),
            r#"{"question":"x"}"#,
        );
        let unmatched = get(port, "/api/v1/project/fixture/nope", Some(&f.token));
        assert!(wrong_project.contains("404"), "got: {wrong_project}");
        assert!(unmatched.contains("404"), "got: {unmatched}");
        let body_of = |r: &str| r.split("\r\n\r\n").nth(1).unwrap_or("").to_string();
        assert_eq!(
            body_of(&wrong_project),
            body_of(&unmatched),
            "a probe must not tell a wrong project from a nonexistent route"
        );
    }

    #[test]
    fn a_token_without_project_docs_cannot_fetch_a_doc() {
        let Some(f) = fixture(vec![Scope::ProjectQuery("fixture".into())]) else {
            eprintln!("skipping: git unavailable");
            return;
        };
        let port = f.server.addr.port();
        let response = post(
            port,
            &format!("/api/v1/project/{}/doc", f.project),
            Some(&f.token),
            r#"{"path":"docs/terminals.md"}"#,
        );
        assert!(
            response.contains("404"),
            "project.query must not buy verbatim doc retrieval — got: {response}"
        );
    }

    #[test]
    fn no_mutating_method_is_routed() {
        let Some(f) = fixture(vec![Scope::ProjectQuery("fixture".into())]) else {
            eprintln!("skipping: git unavailable");
            return;
        };
        let port = f.server.addr.port();
        for method in ["PUT", "PATCH", "DELETE"] {
            let raw = format!(
                "{} /api/v1/project/{}/query HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                method, f.project, f.token
            );
            let response = request(port, &raw, Duration::from_secs(10));
            assert!(
                response.contains("404"),
                "{method} must not route — got: {response}"
            );
        }
    }

    #[test]
    fn an_invalid_project_name_is_rejected_before_it_is_compared() {
        let Some(f) = fixture(vec![Scope::ProjectQuery("fixture".into())]) else {
            eprintln!("skipping: git unavailable");
            return;
        };
        let port = f.server.addr.port();
        // `..` and `%2e%2e` both fail the qualifier charset, so neither ever
        // reaches a comparison — let alone a path join.
        for name in ["..", "%2e%2e", "a:b"] {
            let response = post(
                port,
                &format!("/api/v1/project/{name}/query"),
                Some(&f.token),
                r#"{"question":"x"}"#,
            );
            assert!(response.contains("400"), "{name} — got: {response}");
        }
    }

    /// `escape` does the JSON quoting, so the message literal must not also
    /// pre-escape — double-escaping ships visible backslashes to the caller.
    #[test]
    fn an_error_message_is_escaped_exactly_once() {
        let body = format!(
            r#"{{"error":"{}"}}"#,
            escape("expected {\"question\": \"...\"}")
        );
        let parsed: serde_json::Value = serde_json::from_str(&body).expect("valid JSON");
        assert_eq!(
            parsed["error"], "expected {\"question\": \"...\"}",
            "got {body}"
        );
    }

    /// #431: the two guardrails are `429`, distinguishable, and carry a
    /// `Retry-After`. The caller has already proved they hold a valid in-scope
    /// token, so naming their own limit leaks nothing §3.3 protects.
    #[test]
    fn exceeding_the_rate_limit_is_429_with_a_retry_after_header() {
        let Some(f) = fixture_with_rate_limit(vec![Scope::ProjectQuery("fixture".into())], Some(1))
        else {
            eprintln!("skipping: git unavailable");
            return;
        };
        let port = f.server.addr.port();
        let path = format!("/api/v1/project/{}/query", f.project);
        let body = r#"{"question":"config layering"}"#;

        let first = post(port, &path, Some(&f.token), body);
        assert!(first.contains("200 OK"), "got: {first}");

        let second = post(port, &path, Some(&f.token), body);
        assert!(
            second.contains("429 Too Many Requests"),
            "a throttled request must be 429, not 404 — got: {second}"
        );
        assert!(
            second.contains("Retry-After:"),
            "expected a Retry-After header, got: {second}"
        );
        assert!(
            second.contains("rate limited"),
            "the holder should be told which limit they hit, got: {second}"
        );
    }

    #[test]
    fn a_throttled_response_is_still_well_formed_json() {
        let Some(f) = fixture_with_rate_limit(vec![Scope::ProjectQuery("fixture".into())], Some(1))
        else {
            eprintln!("skipping: git unavailable");
            return;
        };
        let port = f.server.addr.port();
        let path = format!("/api/v1/project/{}/query", f.project);
        let body = r#"{"question":"config"}"#;
        let _ = post(port, &path, Some(&f.token), body);
        let throttled = post(port, &path, Some(&f.token), body);

        // Adding a header must not corrupt the framing — Content-Length still
        // has to describe the body, and the body still has to parse.
        let json = throttled
            .split("\r\n\r\n")
            .nth(1)
            .expect("a body after the headers");
        let parsed: serde_json::Value =
            serde_json::from_str(json).unwrap_or_else(|e| panic!("{e} in {json:?}"));
        assert_eq!(parsed["error"], "rate limited");
    }

    #[test]
    fn a_malformed_body_is_400() {
        let Some(f) = fixture(vec![Scope::ProjectQuery("fixture".into())]) else {
            eprintln!("skipping: git unavailable");
            return;
        };
        let port = f.server.addr.port();
        let path = format!("/api/v1/project/{}/query", f.project);
        let response = post(port, &path, Some(&f.token), r#"{"not_a_question":1}"#);
        assert!(response.contains("400"), "got: {response}");
    }

    #[test]
    fn parse_route_accepts_only_the_three_routes() {
        assert_eq!(
            parse_route("POST", "/api/v1/project/p/query"),
            Some(("p".into(), Route::Query))
        );
        assert_eq!(
            parse_route("GET", "/api/v1/project/p/topics?x=1"),
            Some(("p".into(), Route::Topics))
        );
        assert_eq!(
            parse_route("POST", "/api/v1/project/p/doc"),
            Some(("p".into(), Route::Doc))
        );
        // Wrong method, extra depth, missing depth, wrong prefix.
        assert_eq!(parse_route("GET", "/api/v1/project/p/query"), None);
        assert_eq!(parse_route("POST", "/api/v1/project/p/query/extra"), None);
        assert_eq!(parse_route("POST", "/api/v1/project/p"), None);
        assert_eq!(parse_route("POST", "/api/v1/projects/p/query"), None);
        assert_eq!(parse_route("POST", "/metrics"), None);
    }

    #[test]
    fn the_escalation_route_takes_one_path_parameter_and_only_one() {
        assert_eq!(
            parse_route("GET", "/api/v1/project/p/escalation/esc_7f2a1b9c4d3e"),
            Some(("p".into(), Route::Escalation("esc_7f2a1b9c4d3e".into())))
        );
        // The query string is still discarded, as on every other route.
        assert_eq!(
            parse_route("GET", "/api/v1/project/p/escalation/esc_abc?x=1"),
            Some(("p".into(), Route::Escalation("esc_abc".into())))
        );
        // Wrong method.
        assert_eq!(
            parse_route("POST", "/api/v1/project/p/escalation/esc_abc"),
            None
        );
        // Depth is exactly three: no id, empty id, or a fourth segment.
        assert_eq!(parse_route("GET", "/api/v1/project/p/escalation"), None);
        assert_eq!(parse_route("GET", "/api/v1/project/p/escalation/"), None);
        assert_eq!(
            parse_route("GET", "/api/v1/project/p/escalation/esc_abc/more"),
            None
        );
        // Allowing a third segment must not have loosened the other routes.
        assert_eq!(parse_route("POST", "/api/v1/project/p/query/esc_abc"), None);
        assert_eq!(parse_route("GET", "/api/v1/project/p/topics/x"), None);
        assert_eq!(parse_route("POST", "/api/v1/project/p/doc/x"), None);
    }

    #[test]
    fn bearer_token_requires_the_scheme_and_a_value() {
        let mut h = HashMap::new();
        assert_eq!(bearer_token(&h), None);
        h.insert("authorization".into(), "cctl_gr_x_y".into());
        assert_eq!(bearer_token(&h), None, "a bare token is not a bearer");
        h.insert("authorization".into(), "Bearer   ".into());
        assert_eq!(bearer_token(&h), None);
        h.insert("authorization".into(), "Bearer cctl_gr_x_y".into());
        assert_eq!(bearer_token(&h), Some("cctl_gr_x_y"));
    }
}
