use std::collections::HashMap;
use std::io::Write;
use std::path::PathBuf;
use std::time::Duration;

use claudectl_core::session::{
    ClaudeSession, RawSession, SessionStatus, TelemetryStatus, ToolStats,
};

/// Fake project definitions for demo mode.
const PROJECTS: &[(&str, &str, &str)] = &[
    ("acme-api", "/Users/dev/projects/acme-api", "opus-4.6"),
    ("acme-api", "/Users/dev/projects/acme-api", "opus-4.6"),
    (
        "web-frontend",
        "/Users/dev/projects/web-frontend",
        "sonnet-4.6",
    ),
    ("ml-pipeline", "/Users/dev/projects/ml-pipeline", "opus-4.6"),
    (
        "ml-pipeline",
        "/Users/dev/worktrees/ml-pipeline-feat",
        "sonnet-4.6",
    ),
    (
        "infra-terraform",
        "/Users/dev/projects/infra-terraform",
        "haiku",
    ),
    ("docs-site", "/Users/dev/projects/docs-site", "sonnet-4.6"),
    ("mobile-app", "/Users/dev/projects/mobile-app", "opus-4.6"),
];

/// Deterministic status progression per session (cycles through these).
const STATUS_SEQUENCES: &[&[SessionStatus]] = &[
    &[
        SessionStatus::Processing,
        SessionStatus::Processing,
        SessionStatus::Processing,
        SessionStatus::NeedsInput,
        SessionStatus::NeedsInput,
        SessionStatus::Processing,
        SessionStatus::Processing,
        SessionStatus::WaitingInput,
    ],
    &[
        SessionStatus::Processing,
        SessionStatus::NeedsInput,
        SessionStatus::NeedsInput,
        SessionStatus::NeedsInput,
        SessionStatus::Processing,
        SessionStatus::Processing,
        SessionStatus::WaitingInput,
        SessionStatus::Processing,
    ],
    &[
        SessionStatus::Processing,
        SessionStatus::Processing,
        SessionStatus::WaitingInput,
        SessionStatus::WaitingInput,
        SessionStatus::Processing,
        SessionStatus::Processing,
        SessionStatus::NeedsInput,
        SessionStatus::Processing,
    ],
    &[
        SessionStatus::Processing,
        SessionStatus::Processing,
        SessionStatus::Processing,
        SessionStatus::Processing,
        SessionStatus::Processing,
        SessionStatus::WaitingInput,
        SessionStatus::Processing,
        SessionStatus::Processing,
    ],
    &[
        SessionStatus::Processing,
        SessionStatus::Processing,
        SessionStatus::Processing,
        SessionStatus::Processing,
        SessionStatus::WaitingInput,
        SessionStatus::Processing,
        SessionStatus::Processing,
        SessionStatus::NeedsInput,
    ],
    &[
        SessionStatus::WaitingInput,
        SessionStatus::Processing,
        SessionStatus::Processing,
        SessionStatus::Processing,
        SessionStatus::Processing,
        SessionStatus::Processing,
        SessionStatus::NeedsInput,
        SessionStatus::Processing,
    ],
    &[
        SessionStatus::Processing,
        SessionStatus::Processing,
        SessionStatus::WaitingInput,
        SessionStatus::Processing,
        SessionStatus::Processing,
        SessionStatus::Processing,
        SessionStatus::WaitingInput,
        SessionStatus::WaitingInput,
    ],
    &[
        SessionStatus::Processing,
        SessionStatus::Processing,
        SessionStatus::NeedsInput,
        SessionStatus::Processing,
        SessionStatus::Processing,
        SessionStatus::Processing,
        SessionStatus::WaitingInput,
        SessionStatus::Processing,
    ],
];

/// Pending tool calls assigned to NeedsInput sessions by index.
/// (tool_name, command/input_summary)
const PENDING_TOOLS: &[(&str, &str)] = &[
    ("Bash", "cargo test --workspace"),
    ("Bash", "cargo clippy -- -D warnings"),
    ("Bash", "npm run build && npm test"),
    ("Bash", "python train.py --epochs 50"),
    ("Bash", "rm -rf /tmp/cache && rm -rf node_modules"),
    ("Bash", "terraform apply -auto-approve"),
    ("Bash", "npm run deploy -- --prod"),
    ("Bash", "git push --force origin main"),
];

/// Generate deterministic fake sessions for demo mode.
pub fn generate_sessions(tick: u32) -> Vec<ClaudeSession> {
    let base_pid = 10000u32;

    PROJECTS
        .iter()
        .enumerate()
        .map(|(i, (name, cwd, model))| {
            let pid = base_pid + (i as u32 * 1111);
            let raw = RawSession {
                pid,
                session_id: format!("demo-{:04x}-{:04x}-{:04x}", i, i * 7, i * 13),
                cwd: cwd.to_string(),
                started_at: 0, // Will be overridden
            };
            let mut s = ClaudeSession::from_raw(raw);
            s.project_name = name.to_string();
            s.model = model.to_string();
            s.telemetry_status = TelemetryStatus::Available;
            s.usage_metrics_available = true;

            // Deterministic status from sequence
            let seq = STATUS_SEQUENCES[i % STATUS_SEQUENCES.len()];
            s.status = seq[(tick as usize) % seq.len()];

            // Deterministic metrics that grow over time
            let base_tokens = (i as u64 + 1) * 50_000 + (tick as u64) * 2_000;
            s.total_input_tokens = base_tokens;
            s.total_output_tokens = base_tokens / 5;
            s.cache_read_tokens = base_tokens / 3;
            s.cache_write_tokens = base_tokens / 10;

            // Context grows over time, different rates per session
            let ctx_rate = 0.3 + (i as f64 * 0.08);
            let ctx_pct = ((tick as f64 * ctx_rate) % 95.0) + 5.0;
            s.context_max = claudectl_core::monitor::model_context_max(model);
            s.context_tokens = (s.context_max as f64 * ctx_pct / 100.0) as u64;

            // Cost grows over time
            s.cost_usd = (i as f64 + 1.0) * 0.15 + (tick as f64) * 0.03 * (i as f64 + 1.0);
            s.burn_rate_per_hr = if matches!(s.status, SessionStatus::Processing) {
                2.0 + (i as f64 * 0.8)
            } else {
                0.0
            };

            // Elapsed time
            let base_elapsed = (i as u64 + 1) * 300 + tick as u64 * 2;
            s.elapsed = Duration::from_secs(base_elapsed);

            // CPU/MEM
            s.cpu_percent = match s.status {
                SessionStatus::Processing => 15.0 + (i as f32 * 3.0),
                SessionStatus::NeedsInput => 0.3,
                _ => 0.8,
            };
            s.mem_mb = 200.0 + (i as f64 * 50.0);

            // Subagents for some sessions
            if i == 0 || i == 3 {
                s.subagent_count = 2 + (tick as usize % 3);
            }

            // Activity sparkline history
            for t in 0..15 {
                let past_tick = if tick > 15 { tick - 15 + t } else { t };
                let past_status = seq[(past_tick as usize) % seq.len()];
                let level = match past_status {
                    SessionStatus::Processing => 7,
                    SessionStatus::NeedsInput => 4,
                    SessionStatus::WaitingInput => 2,
                    SessionStatus::Unknown => 2,
                    SessionStatus::Idle => 1,
                    SessionStatus::Finished => 0,
                };
                s.activity_history.push(level);
            }

            // Worktree IDs — sessions 0 and 1 share same worktree (conflict!)
            // Session 4 is a worktree of project 3's repo (no conflict)
            s.worktree_id = Some(cwd.to_string());

            // Tool usage for detail panel
            if tick > 3 {
                let mut tools = HashMap::new();
                tools.insert(
                    "Bash".to_string(),
                    ToolStats {
                        calls: 12 + (i as u32 * 3),
                    },
                );
                tools.insert(
                    "Read".to_string(),
                    ToolStats {
                        calls: 25 + (i as u32 * 5),
                    },
                );
                tools.insert(
                    "Edit".to_string(),
                    ToolStats {
                        calls: 8 + (i as u32 * 2),
                    },
                );
                tools.insert(
                    "Grep".to_string(),
                    ToolStats {
                        calls: 6 + (i as u32),
                    },
                );
                s.tool_usage = tools;
            }

            // File changes for detail panel
            if tick > 5 {
                let mut files = HashMap::new();
                files.insert(format!("/Users/dev/projects/{name}/src/main.rs"), 3);
                files.insert(format!("/Users/dev/projects/{name}/src/lib.rs"), 1);
                if i % 2 == 0 {
                    files.insert(format!("/Users/dev/projects/{name}/Cargo.toml"), 1);
                }
                s.files_modified = files;
            }

            // ── Health-triggering overrides ──────────────────────────────

            // Session 2 (web-frontend): Low cache hit ratio → 🔥 critical
            if i == 2 {
                s.total_input_tokens = 120_000 + (tick as u64 * 3_000);
                s.cache_read_tokens = 5_000; // ~4% — well under 10% critical threshold
                s.cache_write_tokens = 2_000;
            }

            // Session 3 (ml-pipeline): Context saturation → 🧠 critical
            if i == 3 && tick > 6 {
                let saturation = 0.91 + ((tick as f64 - 6.0) * 0.005).min(0.07);
                s.context_tokens = (s.context_max as f64 * saturation) as u64;
            }

            // Session 5 (infra-terraform): Stalled → 🐌 warning
            // High cost, long elapsed, but NO file edits
            if i == 5 {
                s.cost_usd = 7.50 + (tick as f64) * 0.12;
                s.elapsed = Duration::from_secs(900 + tick as u64 * 5);
                s.files_modified.clear(); // Zero file edits despite spending
            }

            // Session 7 (mobile-app): Cost spike → 💸
            if i == 7 && tick > 4 {
                // Base cost accumulates slowly, then burn rate spikes
                s.cost_usd = 3.0 + (tick as f64) * 0.05;
                let elapsed_hrs = s.elapsed.as_secs_f64() / 3600.0;
                let avg_rate = if elapsed_hrs > 0.01 {
                    s.cost_usd / elapsed_hrs
                } else {
                    1.0
                };
                // Spike burn rate to 6x average
                s.burn_rate_per_hr = avg_rate * 6.0;
            }

            // Session 3 (ml-pipeline): Severe cognitive decay (⊘) — high context + all signals
            if i == 3 && tick > 4 {
                s.baseline_tokens_per_edit = Some(4000.0);
                s.edit_event_count = 15;
                s.total_tokens_at_edit_count = 15 * 12_000; // 12k/edit vs 4k baseline = 3x
                s.error_counts_per_window = vec![0, 1, 1, 3, 5, 6, 8];
                s.baseline_error_rate = Some(0.7);
                s.file_reads_since_edit.insert("src/pipeline.rs".into(), 5);
                s.file_reads_since_edit
                    .insert("src/data_loader.rs".into(), 3);
            }

            // Session 0 (acme-api): Early cognitive decay (◐) — moderate context + some signals
            if i == 0 && tick > 8 {
                s.baseline_tokens_per_edit = Some(5000.0);
                s.edit_event_count = 10;
                s.total_tokens_at_edit_count = 10 * 7_500; // 7.5k/edit vs 5k baseline = 1.5x
                s.error_counts_per_window = vec![1, 1, 2, 2, 3];
                s.baseline_error_rate = Some(1.3);
                s.file_reads_since_edit.insert("src/main.rs".into(), 3);
                // Push context to ~65% for moderate decay
                let ctx_pct = 0.60 + ((tick as f64 - 8.0) * 0.01).min(0.15);
                s.context_tokens = (s.context_max as f64 * ctx_pct) as u64;
            }

            // Session 4 (ml-pipeline worktree): Loop detection → 🔄
            if i == 4 && tick > 5 {
                s.last_tool_error = true;
                s.tool_usage
                    .entry("Bash".to_string())
                    .and_modify(|t| t.calls = 15 + (tick % 5))
                    .or_insert(ToolStats {
                        calls: 15 + (tick % 5),
                    });
            }

            // ── Pending tool info for rules/brain ────────────────────────

            if s.status == SessionStatus::NeedsInput {
                let (tool, cmd) = PENDING_TOOLS[i % PENDING_TOOLS.len()];
                s.pending_tool_name = Some(tool.to_string());
                s.pending_tool_input = Some(cmd.to_string());
            }

            s
        })
        .collect()
}

/// Scripted demo events that simulate rules, brain, and routing actions.
/// Returns a status message for specific ticks, cycling every CYCLE_LEN ticks.
pub fn demo_event(tick: u32) -> Option<DemoEvent> {
    const CYCLE_LEN: u32 = 32;
    let phase = tick % CYCLE_LEN;

    match phase {
        // Brain auto-approve (show the brain working)
        2 => Some(DemoEvent {
            message: "Brain: auto-approved Bash(cargo test --workspace) for acme-api [92%]"
                .into(),
            kind: EventKind::BrainSuggestion,
        }),

        // Rule firing
        4 => Some(DemoEvent {
            message:
                "Rule 'deny-rm-rf': denied ml-pipeline (Bash: rm -rf /tmp/cache)"
                    .into(),
            kind: EventKind::RuleAction,
        }),

        // Brain deny
        6 => Some(DemoEvent {
            message: "Brain: denied Bash(terraform apply -auto-approve) for infra-terraform — destructive without plan review [87%]".into(),
            kind: EventKind::BrainSuggestion,
        }),

        // Cognitive decay alert
        8 => Some(DemoEvent {
            message: "Health: ml-pipeline cognitive decay at 82/100 — session degrading, consider restart".into(),
            kind: EventKind::HealthAlert,
        }),

        // Brain auto-approve
        10 => Some(DemoEvent {
            message: "Brain: auto-approved Bash(npm run build && npm test) for web-frontend [95%]"
                .into(),
            kind: EventKind::BrainSuggestion,
        }),

        // Brain override by deny rule
        12 => Some(DemoEvent {
            message:
                "Brain suggested approve, but deny rule 'deny-force-push' overrides (git push --force)"
                    .into(),
            kind: EventKind::BrainOverride,
        }),

        // Hive: knowledge sync from peer
        14 => Some(DemoEvent {
            message: "Hive: synced 3 knowledge units from ci-runner — [Bash, cargo *] approve (94%), [Bash, docker push] deny (92%)".into(),
            kind: EventKind::HiveSync,
        }),

        // Inter-session routing
        16 => Some(DemoEvent {
            message: "Routed summary from ml-pipeline → docs-site: \"Added training pipeline with checkpoint support\"".into(),
            kind: EventKind::Route,
        }),

        // Stall alert
        18 => Some(DemoEvent {
            message: "Health: infra-terraform stalled — $8.40 spent, 16 min, no file edits".into(),
            kind: EventKind::HealthAlert,
        }),

        // Hive: brain decision influenced by peer knowledge
        20 => Some(DemoEvent {
            message: "Brain: denied Bash(docker push prod:latest) — [hive] ci-runner says deny (92%, 15 decisions)".into(),
            kind: EventKind::HiveInfluence,
        }),

        // Context saturation alert
        22 => Some(DemoEvent {
            message: "Health: ml-pipeline context at 94% — auto-restart checkpoint saved".into(),
            kind: EventKind::HealthAlert,
        }),

        // Hive: trust drift from concordance
        24 => Some(DemoEvent {
            message: "Hive: trust for alice-mbp drifted 0.50 → 0.53 (3 concordant decisions)".into(),
            kind: EventKind::HiveSync,
        }),

        // Brain approve with hive confirmation
        26 => Some(DemoEvent {
            message: "Brain: auto-approved Bash(cargo clippy -- -D warnings) — confirmed by [hive] 3 peers (94%) [96%]".into(),
            kind: EventKind::HiveInfluence,
        }),

        // Hive: new peer joined
        28 => Some(DemoEvent {
            message: "Hive: alice-mbp connected — received snapshot of 12 knowledge units".into(),
            kind: EventKind::HiveSync,
        }),

        // Hive: distillation with knowledge discrimination
        30 => Some(DemoEvent {
            message: "Hive: distilled 5 patterns — 3 shared (best_practice), 2 kept local (personal: time-of-day, cost)".into(),
            kind: EventKind::HiveSync,
        }),

        _ => None,
    }
}

/// A scripted event in the demo timeline.
pub struct DemoEvent {
    pub message: String,
    pub kind: EventKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventKind {
    RuleAction,
    BrainSuggestion,
    BrainOverride,
    Route,
    HealthAlert,
    HiveSync,
    HiveInfluence,
}

/// Generate demo peer display info for the TUI peers panel.
#[cfg(feature = "relay")]
pub fn demo_peers(tick: u32) -> Vec<crate::ui::peers::PeerDisplayInfo> {
    let mut peers = vec![
        crate::ui::peers::PeerDisplayInfo {
            peer_id: "ci-runner-9d1e".into(),
            state: "connected".into(),
            trust: 0.82,
            units_sent: 42,
            units_received: 18,
            session_count: 3,
        },
        crate::ui::peers::PeerDisplayInfo {
            peer_id: "alice-mbp-f3a1".into(),
            state: if tick % 32 < 28 {
                "connecting".into()
            } else {
                "connected".into()
            },
            trust: 0.53,
            units_sent: 0,
            units_received: if tick % 32 >= 28 { 12 } else { 0 },
            session_count: if tick % 32 >= 28 { 2 } else { 0 },
        },
    ];

    // After tick 28, alice is connected and has received knowledge
    if tick % 32 >= 28 {
        peers[1].state = "connected".into();
        peers[1].trust = 0.53 + (tick % 32 - 28) as f64 * 0.01;
    }

    peers
}

/// Generate demo rules for display.
pub fn demo_rules() -> Vec<claudectl_core::rules::AutoRule> {
    use claudectl_core::rules::{AutoRule, RuleAction};

    vec![
        {
            let mut r = AutoRule::new("approve-cargo".into(), RuleAction::Approve);
            r.match_tool = vec!["Bash".into()];
            r.match_command = vec!["cargo".into()];
            r
        },
        {
            let mut r = AutoRule::new("deny-rm-rf".into(), RuleAction::Deny);
            r.match_command = vec!["rm -rf".into()];
            r
        },
        {
            let mut r = AutoRule::new("deny-force-push".into(), RuleAction::Deny);
            r.match_command = vec!["--force".into()];
            r
        },
        {
            let mut r = AutoRule::new("kill-runaway".into(), RuleAction::Terminate);
            r.match_cost_above = Some(20.0);
            r
        },
    ]
}

// ── Demo highlight reel JSONL ──────────────────────────────────────

/// A scripted JSONL transcript simulating a realistic Claude Code session.
/// Each entry is one JSONL line that the SessionRecorder can parse.
/// Tells the story: "Add JWT auth middleware to acme-api".
fn demo_highlight_script() -> Vec<String> {
    let lines: Vec<serde_json::Value> = vec![
        // 1. Assistant reads the existing code
        assistant_msg(
            "Let me read the current auth setup to understand the existing patterns before making changes.",
            Some((
                "Read",
                serde_json::json!({"file_path": "/Users/dev/projects/acme-api/src/auth.rs"}),
            )),
        ),
        // 2. Tool result from Read
        user_tool_result(
            "pub fn check_token(token: &str) -> bool {\n    // TODO: implement real auth\n    token == \"hardcoded-dev-token\"\n}",
            false,
        ),
        // 3. Assistant reads the router
        assistant_msg(
            "Now let me check how routes are set up to see where middleware gets applied.",
            Some((
                "Read",
                serde_json::json!({"file_path": "/Users/dev/projects/acme-api/src/router.rs"}),
            )),
        ),
        // 4. Tool result
        user_tool_result(
            "pub fn routes() -> Router {\n    Router::new()\n        .route(\"/api/users\", get(list_users))\n        .route(\"/api/users/:id\", get(get_user))\n        .route(\"/health\", get(health_check))\n}",
            false,
        ),
        // 5. Grep for existing middleware patterns
        assistant_msg(
            "Checking if there are any existing middleware patterns in the codebase I should follow.",
            Some((
                "Grep",
                serde_json::json!({"pattern": "middleware|layer|Extension", "path": "/Users/dev/projects/acme-api/src"}),
            )),
        ),
        // 6. Grep result
        user_tool_result(
            "src/main.rs:  .layer(CorsLayer::permissive())\nsrc/main.rs:  .layer(TraceLayer::new_for_http())",
            false,
        ),
        // 7. Write the JWT auth module
        assistant_msg(
            "I'll create the JWT middleware module following the existing layer pattern from main.rs.",
            Some((
                "Write",
                serde_json::json!({
                    "file_path": "/Users/dev/projects/acme-api/src/middleware/jwt.rs",
                    "content": "use axum::{extract::Request, middleware::Next, response::Response};\nuse jsonwebtoken::{decode, DecodingKey, Validation};\n\npub async fn require_auth(\n    request: Request,\n    next: Next,\n) -> Result<Response, AuthError> {\n    let token = request\n        .headers()\n        .get(\"Authorization\")\n        .and_then(|v| v.to_str().ok())\n        .and_then(|v| v.strip_prefix(\"Bearer \"))\n        .ok_or(AuthError::MissingToken)?;\n\n    let claims = decode::<Claims>(\n        token,\n        &DecodingKey::from_secret(std::env::var(\"JWT_SECRET\")?.as_ref()),\n        &Validation::default(),\n    ).map_err(|_| AuthError::InvalidToken)?;\n\n    request.extensions_mut().insert(claims.claims);\n    Ok(next.run(request).await)\n}"
                }),
            )),
        ),
        // 8. Write result
        user_tool_result("", false),
        // 9. Edit the router to add middleware
        assistant_msg(
            "Now I'll wire the JWT middleware into the router, keeping /health unprotected.",
            Some((
                "Edit",
                serde_json::json!({
                    "file_path": "/Users/dev/projects/acme-api/src/router.rs",
                    "old_string": "pub fn routes() -> Router {\n    Router::new()\n        .route(\"/api/users\", get(list_users))\n        .route(\"/api/users/:id\", get(get_user))\n        .route(\"/health\", get(health_check))\n}",
                    "new_string": "pub fn routes() -> Router {\n    let protected = Router::new()\n        .route(\"/api/users\", get(list_users))\n        .route(\"/api/users/:id\", get(get_user))\n        .layer(middleware::from_fn(jwt::require_auth));\n\n    let public = Router::new()\n        .route(\"/health\", get(health_check));\n\n    Router::new().merge(protected).merge(public)\n}"
                }),
            )),
        ),
        // 10. Edit result
        user_tool_result("", false),
        // 11. Edit Cargo.toml to add jsonwebtoken dep
        assistant_msg(
            "Adding the jsonwebtoken dependency to Cargo.toml.",
            Some((
                "Edit",
                serde_json::json!({
                    "file_path": "/Users/dev/projects/acme-api/Cargo.toml",
                    "old_string": "[dependencies]\naxum = \"0.7\"",
                    "new_string": "[dependencies]\naxum = \"0.7\"\njsonwebtoken = \"9\""
                }),
            )),
        ),
        // 12. Edit result
        user_tool_result("", false),
        // 13. Run cargo check
        assistant_msg(
            "Let me verify everything compiles before writing tests.",
            Some((
                "Bash",
                serde_json::json!({"command": "cargo check --workspace"}),
            )),
        ),
        // 14. Cargo check result — error!
        user_tool_result(
            "error[E0433]: failed to resolve: use of undeclared type `AuthError`\n  --> src/middleware/jwt.rs:8:37\n   |\n8  | ) -> Result<Response, AuthError> {\n   |                      ^^^^^^^^^ not found in this scope\n\nerror: aborting due to 1 previous error",
            true,
        ),
        // 15. Fix the error
        assistant_msg(
            "I forgot to define the AuthError type. Let me add it to the JWT module.",
            Some((
                "Edit",
                serde_json::json!({
                    "file_path": "/Users/dev/projects/acme-api/src/middleware/jwt.rs",
                    "old_string": "use axum::{extract::Request, middleware::Next, response::Response};",
                    "new_string": "use axum::{\n    extract::Request,\n    http::StatusCode,\n    middleware::Next,\n    response::{IntoResponse, Response},\n};\n\n#[derive(Debug)]\npub enum AuthError {\n    MissingToken,\n    InvalidToken,\n}\n\nimpl IntoResponse for AuthError {\n    fn into_response(self) -> Response {\n        let status = match self {\n            Self::MissingToken => StatusCode::UNAUTHORIZED,\n            Self::InvalidToken => StatusCode::FORBIDDEN,\n        };\n        status.into_response()\n    }\n}"
                }),
            )),
        ),
        // 16. Fix result
        user_tool_result("", false),
        // 17. Cargo check again
        assistant_msg(
            "Checking if the compilation error is fixed.",
            Some((
                "Bash",
                serde_json::json!({"command": "cargo check --workspace"}),
            )),
        ),
        // 18. Success
        user_tool_result(
            "    Finished `dev` profile [unoptimized + debuginfo] target(s) in 2.34s",
            false,
        ),
        // 19. Write tests
        assistant_msg(
            "Compilation passes. Now I'll write tests for the auth middleware.",
            Some((
                "Write",
                serde_json::json!({
                    "file_path": "/Users/dev/projects/acme-api/tests/auth_test.rs",
                    "content": "#[tokio::test]\nasync fn rejects_missing_token() {\n    let app = test_app();\n    let resp = app.get(\"/api/users\").send().await;\n    assert_eq!(resp.status(), 401);\n}\n\n#[tokio::test]\nasync fn rejects_invalid_token() {\n    let app = test_app();\n    let resp = app.get(\"/api/users\")\n        .header(\"Authorization\", \"Bearer invalid\")\n        .send().await;\n    assert_eq!(resp.status(), 403);\n}\n\n#[tokio::test]\nasync fn accepts_valid_token() {\n    let app = test_app();\n    let token = make_test_jwt();\n    let resp = app.get(\"/api/users\")\n        .header(\"Authorization\", format!(\"Bearer {token}\"))\n        .send().await;\n    assert_eq!(resp.status(), 200);\n}\n\n#[tokio::test]\nasync fn health_is_public() {\n    let app = test_app();\n    let resp = app.get(\"/health\").send().await;\n    assert_eq!(resp.status(), 200);\n}"
                }),
            )),
        ),
        // 20. Test write result
        user_tool_result("", false),
        // 21. Run tests
        assistant_msg(
            "Running the full test suite to verify the auth middleware works correctly.",
            Some((
                "Bash",
                serde_json::json!({"command": "cargo test --workspace"}),
            )),
        ),
        // 22. Tests pass
        user_tool_result(
            "running 4 tests\ntest rejects_missing_token ... ok\ntest rejects_invalid_token ... ok\ntest accepts_valid_token ... ok\ntest health_is_public ... ok\n\ntest result: ok. 4 passed; 0 failed; 0 ignored",
            false,
        ),
        // 23. Final summary
        assistant_msg(
            "All 4 tests pass. The JWT auth middleware is working — protected routes require a valid Bearer token, and /health remains public.",
            None,
        ),
    ];

    lines
        .iter()
        .map(|v| serde_json::to_string(v).unwrap())
        .collect()
}

/// Build an assistant message JSONL entry.
fn assistant_msg(text: &str, tool: Option<(&str, serde_json::Value)>) -> serde_json::Value {
    let mut content = vec![serde_json::json!({"type": "text", "text": text})];
    if let Some((name, input)) = tool {
        content.push(serde_json::json!({
            "type": "tool_use",
            "name": name,
            "input": input,
        }));
    }
    serde_json::json!({
        "message": {
            "role": "assistant",
            "model": "claude-opus-4-6-20260401",
            "stop_reason": if content.len() > 1 { "tool_use" } else { "end_turn" },
            "content": content,
        }
    })
}

/// Build a user tool-result JSONL entry.
fn user_tool_result(output: &str, is_error: bool) -> serde_json::Value {
    serde_json::json!({
        "message": {
            "role": "user",
            "content": [{
                "type": "tool_result",
                "content": output,
                "is_error": is_error,
            }]
        }
    })
}

/// State for drip-feeding JSONL events during demo mode.
pub struct DemoHighlightState {
    /// Per-session: (temp JSONL path, next line index to write)
    sessions: HashMap<u32, (PathBuf, usize)>,
    /// The scripted JSONL lines (shared across sessions)
    script: Vec<String>,
}

impl Default for DemoHighlightState {
    fn default() -> Self {
        Self::new()
    }
}

impl DemoHighlightState {
    pub fn new() -> Self {
        Self {
            sessions: HashMap::new(),
            script: demo_highlight_script(),
        }
    }

    /// Ensure a temp JSONL file exists for the given PID. Returns the path.
    pub fn ensure_jsonl(&mut self, pid: u32) -> &PathBuf {
        let entry = self.sessions.entry(pid).or_insert_with(|| {
            let mut path = std::env::temp_dir();
            path.push(format!("claudectl-demo-{pid}.jsonl"));
            // Create empty file (or truncate if leftover from previous run)
            let _ = std::fs::File::create(&path);
            (path, 0)
        });
        &entry.0
    }

    /// Drip-feed the next JSONL event for a session that is being recorded.
    /// Called on each tick. Writes 2 lines per tick (one exchange) for pacing.
    pub fn drip_feed(&mut self, pid: u32) -> bool {
        let Some((path, idx)) = self.sessions.get_mut(&pid) else {
            return false;
        };

        if *idx >= self.script.len() {
            return false; // Script exhausted
        }

        // Write 2 lines per tick (assistant + tool result = one exchange)
        let end = (*idx + 2).min(self.script.len());
        if let Ok(mut f) = std::fs::OpenOptions::new().append(true).open(&*path) {
            for line in &self.script[*idx..end] {
                let _ = writeln!(f, "{}", line);
            }
        }
        *idx = end;
        true
    }

    /// Clean up temp files on exit.
    pub fn cleanup(&self) {
        for (path, _) in self.sessions.values() {
            let _ = std::fs::remove_file(path);
        }
    }
}

#[cfg(test)]
mod highlight_tests {
    use super::*;
    use claudectl_core::transcript::{TranscriptEvent, parse_line};

    #[test]
    fn demo_highlight_generates_valid_jsonl() {
        let script = demo_highlight_script();
        assert!(!script.is_empty(), "script should have events");

        let mut assistant_count = 0;
        let mut tool_result_count = 0;
        let mut error_count = 0;

        for (i, line) in script.iter().enumerate() {
            // Every line must be valid JSON
            let _: serde_json::Value = serde_json::from_str(line)
                .unwrap_or_else(|e| panic!("line {i} is not valid JSON: {e}"));

            // Every line must parse via the transcript parser
            let event = parse_line(line)
                .unwrap_or_else(|| panic!("line {i} failed to parse as TranscriptEvent"));

            if let TranscriptEvent::Message(msg) = event {
                match msg.role {
                    claudectl_core::transcript::TranscriptRole::Assistant => assistant_count += 1,
                    claudectl_core::transcript::TranscriptRole::User => {
                        for block in &msg.content {
                            if let claudectl_core::transcript::TranscriptBlock::ToolResult {
                                is_error,
                                ..
                            } = block
                            {
                                tool_result_count += 1;
                                if *is_error {
                                    error_count += 1;
                                }
                            }
                        }
                    }
                }
            }
        }

        // Sanity checks on script content
        assert!(
            assistant_count >= 10,
            "should have >=10 assistant msgs, got {assistant_count}"
        );
        assert!(
            tool_result_count >= 5,
            "should have >=5 tool results, got {tool_result_count}"
        );
        assert!(error_count >= 1, "should have >=1 error, got {error_count}");
    }

    #[test]
    fn demo_highlight_state_creates_and_cleans_up() {
        let mut state = DemoHighlightState::new();
        let path = state.ensure_jsonl(12345).clone();

        assert!(path.exists(), "temp JSONL file should be created");

        // Drip feed should write events
        assert!(state.drip_feed(12345), "first drip should succeed");
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(!content.is_empty(), "file should have content after drip");

        // Cleanup should remove the file
        state.cleanup();
        assert!(!path.exists(), "temp file should be removed after cleanup");
    }
}
