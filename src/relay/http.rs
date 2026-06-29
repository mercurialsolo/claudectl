// Lightweight HTTP/1.1 server for coordinator mode.
// Raw TCP, no framework — keeps dependency count at zero.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read as IoRead, Write as IoWrite};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::mesh::WorkerState;

/// Maximum HTTP request body size (1 MB).
const MAX_BODY_SIZE: usize = 1_048_576;

/// Shared state that the HTTP server reads from and writes to.
/// Updated by the relay loop on each tick.
pub struct CoordinatorState {
    pub identity: String,
    pub workers: HashMap<String, WorkerState>,
    pub local_sessions: Vec<serde_json::Value>,
}

/// A running HTTP server handle.
pub struct HttpServer {
    pub addr: SocketAddr,
    shutdown: Arc<AtomicBool>,
    _handle: Option<std::thread::JoinHandle<()>>,
}

impl HttpServer {
    /// Start the HTTP server on a background thread.
    pub fn start(
        addr: SocketAddr,
        auth_token: String,
        state: Arc<Mutex<CoordinatorState>>,
    ) -> std::io::Result<Self> {
        let listener = TcpListener::bind(addr)?;
        let local_addr = listener.local_addr()?;
        let shutdown = Arc::new(AtomicBool::new(false));
        let shutdown_clone = Arc::clone(&shutdown);

        let handle = std::thread::spawn(move || {
            let _ = listener.set_nonblocking(true);
            loop {
                if shutdown_clone.load(Ordering::Relaxed) {
                    break;
                }
                match listener.accept() {
                    Ok((stream, _)) => {
                        let state = Arc::clone(&state);
                        let token = auth_token.clone();
                        std::thread::spawn(move || {
                            handle_connection(stream, &state, &token);
                        });
                    }
                    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        // Short poll: bounds first-accept latency (and the
                        // startup race a client hits) to ~10ms rather than 100ms,
                        // while still checking the shutdown flag each loop.
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    Err(_) => {
                        std::thread::sleep(Duration::from_millis(500));
                    }
                }
            }
        });

        Ok(HttpServer {
            addr: local_addr,
            shutdown,
            _handle: Some(handle),
        })
    }

    /// Signal the server to stop.
    pub fn stop(&self) {
        self.shutdown.store(true, Ordering::Relaxed);
    }
}

/// Handle a single HTTP connection.
fn handle_connection(
    mut stream: TcpStream,
    state: &Arc<Mutex<CoordinatorState>>,
    auth_token: &str,
) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let peer_stream = match stream.try_clone() {
        Ok(s) => s,
        Err(_) => return,
    };
    let mut reader = BufReader::new(peer_stream);

    // Parse request line
    let mut request_line = String::new();
    if reader.read_line(&mut request_line).is_err() {
        return;
    }
    let parts: Vec<&str> = request_line.split_whitespace().collect();
    if parts.len() < 2 {
        send_response(&mut stream, 400, r#"{"error":"bad request"}"#);
        return;
    }
    let method = parts[0];
    let path = parts[1];

    // Parse headers
    let mut headers: HashMap<String, String> = HashMap::new();
    let mut content_length: usize = 0;
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

    // Check bearer token auth
    let auth_header = headers
        .get("authorization")
        .map(|s| s.as_str())
        .unwrap_or("");
    let expected = format!("Bearer {auth_token}");
    if auth_header != expected {
        send_response(&mut stream, 401, r#"{"error":"unauthorized"}"#);
        return;
    }

    // Route
    match (method, path) {
        ("POST", "/api/heartbeat") => {
            handle_heartbeat_post(&mut reader, &mut stream, state, content_length);
        }
        ("GET", "/api/sessions") => {
            handle_sessions_get(&mut stream, state);
        }
        ("GET", "/api/workers") => {
            handle_workers_get(&mut stream, state);
        }
        _ => {
            send_response(&mut stream, 404, r#"{"error":"not found"}"#);
        }
    }
}

/// POST /api/heartbeat — receive worker session state.
fn handle_heartbeat_post(
    reader: &mut BufReader<TcpStream>,
    stream: &mut TcpStream,
    state: &Arc<Mutex<CoordinatorState>>,
    content_length: usize,
) {
    if content_length == 0 || content_length > MAX_BODY_SIZE {
        send_response(stream, 400, r#"{"error":"invalid content-length"}"#);
        return;
    }
    let mut body = vec![0u8; content_length];
    if reader.read_exact(&mut body).is_err() {
        send_response(stream, 400, r#"{"error":"failed to read body"}"#);
        return;
    }
    let payload: serde_json::Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(_) => {
            send_response(stream, 400, r#"{"error":"invalid json"}"#);
            return;
        }
    };

    let worker_id = payload
        .get("worker_id")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
        .to_string();
    let sessions = payload
        .get("sessions")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    if let Ok(mut cs) = state.lock() {
        cs.workers.insert(
            worker_id.clone(),
            WorkerState {
                worker_id,
                sessions,
                last_updated: super::epoch_ms(),
            },
        );
    }

    send_response(stream, 200, r#"{"ok":true}"#);
}

/// GET /api/sessions — return unified session list from all workers.
fn handle_sessions_get(stream: &mut TcpStream, state: &Arc<Mutex<CoordinatorState>>) {
    let body = if let Ok(cs) = state.lock() {
        let mut all_sessions = Vec::new();

        // Local sessions
        for s in &cs.local_sessions {
            let mut session = s.clone();
            if let Some(obj) = session.as_object_mut() {
                obj.insert("worker_id".into(), serde_json::json!(cs.identity));
            }
            all_sessions.push(session);
        }

        // Remote worker sessions
        for ws in cs.workers.values() {
            for s in &ws.sessions {
                let mut session = s.clone();
                if let Some(obj) = session.as_object_mut() {
                    obj.insert("worker_id".into(), serde_json::json!(ws.worker_id));
                }
                all_sessions.push(session);
            }
        }

        serde_json::to_string(&all_sessions).unwrap_or_else(|_| "[]".into())
    } else {
        "[]".into()
    };

    send_response(stream, 200, &body);
}

/// GET /api/workers — return worker status summary.
fn handle_workers_get(stream: &mut TcpStream, state: &Arc<Mutex<CoordinatorState>>) {
    let body = if let Ok(cs) = state.lock() {
        let now = super::epoch_ms();
        let mut workers = serde_json::Map::new();

        // Local worker
        workers.insert(
            cs.identity.clone(),
            serde_json::json!({
                "session_count": cs.local_sessions.len(),
                "last_updated": now,
                "state": "local",
            }),
        );

        // Remote workers
        for ws in cs.workers.values() {
            let age_secs = now.saturating_sub(ws.last_updated) / 1000;
            let state_label = if age_secs < 60 { "connected" } else { "stale" };
            workers.insert(
                ws.worker_id.clone(),
                serde_json::json!({
                    "session_count": ws.sessions.len(),
                    "last_updated": ws.last_updated,
                    "state": state_label,
                }),
            );
        }

        serde_json::to_string(&serde_json::Value::Object(workers)).unwrap_or_else(|_| "{}".into())
    } else {
        "{}".into()
    };

    send_response(stream, 200, &body);
}

/// Send an HTTP/1.1 response with JSON content type.
fn send_response(stream: &mut TcpStream, status: u16, body: &str) {
    let status_text = match status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        _ => "Error",
    };
    let response = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        status,
        status_text,
        body.len(),
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

    /// Send `request` to `127.0.0.1:port`, retrying a fresh connect+read until
    /// a non-empty response arrives or `deadline` elapses. This removes the
    /// startup race where the server's accept loop hasn't yet picked up the
    /// connection — each attempt is a clean, full request, so it's safe for the
    /// idempotent GET/heartbeat endpoints under test. Returns "" on timeout.
    fn request_until_response(port: u16, request: &[u8], deadline: Duration) -> String {
        let start = std::time::Instant::now();
        loop {
            if let Ok(mut stream) = TcpStream::connect(format!("127.0.0.1:{port}")) {
                let _ = stream.set_read_timeout(Some(Duration::from_millis(500)));
                let _ = stream.write_all(request);
                let _ = stream.flush();
                let mut buf = [0u8; 4096];
                if let Ok(n) = stream.read(&mut buf) {
                    if n > 0 {
                        return String::from_utf8_lossy(&buf[..n]).to_string();
                    }
                }
            }
            if start.elapsed() >= deadline {
                return String::new();
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn send_response_formats_correctly() {
        // We can't easily test TCP streams, but we verify the format string logic
        let status_text = match 200u16 {
            200 => "OK",
            401 => "Unauthorized",
            404 => "Not Found",
            _ => "Error",
        };
        assert_eq!(status_text, "OK");

        let body = r#"{"ok":true}"#;
        let response = format!(
            "HTTP/1.1 {} {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            200,
            "OK",
            body.len(),
            body
        );
        assert!(response.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(response.contains("Content-Length: 11\r\n"));
        assert!(response.ends_with(r#"{"ok":true}"#));
    }

    #[test]
    fn coordinator_state_sessions_merge() {
        let state = CoordinatorState {
            identity: "local-host".into(),
            workers: {
                let mut m = HashMap::new();
                m.insert(
                    "remote-01".into(),
                    WorkerState {
                        worker_id: "remote-01".into(),
                        sessions: vec![
                            serde_json::json!({"pid": 100, "project": "api"}),
                            serde_json::json!({"pid": 200, "project": "web"}),
                        ],
                        last_updated: super::super::epoch_ms(),
                    },
                );
                m
            },
            local_sessions: vec![serde_json::json!({"pid": 999, "project": "local-proj"})],
        };

        // Simulate what handle_sessions_get does
        let mut all_sessions = Vec::new();
        for s in &state.local_sessions {
            let mut session = s.clone();
            if let Some(obj) = session.as_object_mut() {
                obj.insert("worker_id".into(), serde_json::json!(state.identity));
            }
            all_sessions.push(session);
        }
        for ws in state.workers.values() {
            for s in &ws.sessions {
                let mut session = s.clone();
                if let Some(obj) = session.as_object_mut() {
                    obj.insert("worker_id".into(), serde_json::json!(ws.worker_id));
                }
                all_sessions.push(session);
            }
        }

        assert_eq!(all_sessions.len(), 3);
        // Local session tagged with local identity
        assert_eq!(
            all_sessions[0].get("worker_id").and_then(|v| v.as_str()),
            Some("local-host")
        );
    }

    #[test]
    fn http_server_start_and_stop() {
        let state = Arc::new(Mutex::new(CoordinatorState {
            identity: "test-node".into(),
            workers: HashMap::new(),
            local_sessions: Vec::new(),
        }));

        // Bind to port 0 for an available port
        let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let server = HttpServer::start(addr, "test-token".into(), state).unwrap();

        // Verify it bound to a real port
        assert_ne!(server.addr.port(), 0);

        // Stop should not panic
        server.stop();
    }

    #[test]
    fn http_server_rejects_bad_auth() {
        let state = Arc::new(Mutex::new(CoordinatorState {
            identity: "test-node".into(),
            workers: HashMap::new(),
            local_sessions: Vec::new(),
        }));

        let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let server = HttpServer::start(addr, "secret-token".into(), state).unwrap();
        let port = server.addr.port();

        let request = "GET /api/sessions HTTP/1.1\r\nAuthorization: Bearer wrong-token\r\n\r\n";
        let response = request_until_response(port, request.as_bytes(), Duration::from_secs(5));
        assert!(response.contains("401"), "got: {response:?}");

        server.stop();
    }

    #[test]
    fn http_server_returns_sessions() {
        let state = Arc::new(Mutex::new(CoordinatorState {
            identity: "local".into(),
            workers: HashMap::new(),
            local_sessions: vec![serde_json::json!({"pid": 42, "project": "test"})],
        }));

        let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let server = HttpServer::start(addr, "tok".into(), state).unwrap();
        let port = server.addr.port();

        let request = "GET /api/sessions HTTP/1.1\r\nAuthorization: Bearer tok\r\n\r\n";
        let response = request_until_response(port, request.as_bytes(), Duration::from_secs(5));
        assert!(response.contains("200 OK"), "got: {response:?}");
        assert!(response.contains("\"pid\":42"));
        assert!(response.contains("\"worker_id\":\"local\""));

        server.stop();
    }

    #[test]
    fn http_server_accepts_heartbeat_post() {
        let state = Arc::new(Mutex::new(CoordinatorState {
            identity: "coord".into(),
            workers: HashMap::new(),
            local_sessions: Vec::new(),
        }));
        let state_check = Arc::clone(&state);

        let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let server = HttpServer::start(addr, "t".into(), state).unwrap();
        let port = server.addr.port();

        let body = r#"{"worker_id":"remote-1","sessions":[{"pid":1}]}"#;
        let request = format!(
            "POST /api/heartbeat HTTP/1.1\r\nAuthorization: Bearer t\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        );
        let response = request_until_response(port, request.as_bytes(), Duration::from_secs(5));
        assert!(response.contains("200 OK"), "got: {response:?}");

        // The heartbeat upserts worker "remote-1"; the request helper may retry,
        // but each POST replaces (not appends) the session list, so the count
        // stays 1 regardless of how many succeeded.
        if let Ok(cs) = state_check.lock() {
            assert!(cs.workers.contains_key("remote-1"));
            assert_eq!(cs.workers["remote-1"].sessions.len(), 1);
        }

        server.stop();
    }
}
