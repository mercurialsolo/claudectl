// Allow dead_code at the module level: the retry-prompt builder and a few
// helpers are consumed by the actuator's Verifying → Retrying / Done path
// (also in this PR). The standalone `Verifier`/`Verdict` types are public
// so PR6's resume protocol can read verifier history into recovery context.
#![allow(dead_code)]
//! Verifier gates (#345, RFC §5).
//!
//! A task is never `DONE` because the agent said so. Three verifier kinds,
//! in ascending cost, run in declared order with short-circuit on first
//! FAIL:
//!
//! 1. **`run`** — shell command in the task's `cwd`. Exit code is the
//!    verdict; stdout + stderr is captured (truncated) so the retry
//!    prompt can carry the failure context.
//! 2. **`brain`** — prompt routed to the already-running local model.
//!    Verdict parsed from a leading `PASS` / `FAIL:` token. Zero
//!    marginal cost — the right tier for "is the diff plausibly what
//!    was asked."
//! 3. **`agent`** — short-lived headless `claude -p` with the diff +
//!    prompt. Own model + budget cap. Same verdict-parsing convention
//!    as `brain`. For gates that need frontier judgment.
//!
//! Verifier output is the gradient. On `FAIL` the retry prompt is the
//! original prompt + the verifier output + "previous attempt failed
//! verification for these reasons; fix them." No DSL, no plugin ABI —
//! these three cover ~95% of real gates and stay inspectable.

use std::path::Path;
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// One verifier definition. Stored on `tasks.verifiers` as JSON so a
/// task's gate list survives crash + restart untouched.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Verifier {
    /// Shell exit code is the verdict.
    Run { command: String },
    /// Routed to the local LLM via the existing brain client.
    Brain { prompt: String },
    /// Short-lived `claude -p` with its own budget cap.
    Agent {
        prompt: String,
        #[serde(default)]
        model: Option<String>,
        #[serde(default)]
        budget_usd: Option<f64>,
    },
}

impl Verifier {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Run { .. } => "run",
            Self::Brain { .. } => "brain",
            Self::Agent { .. } => "agent",
        }
    }

    /// Surface text recorded in `task_verifications.command`. We store
    /// the actual shell command / LLM prompt so an operator looking at
    /// the ledger sees what was tried, not just the kind.
    pub fn command_text(&self) -> &str {
        match self {
            Self::Run { command } => command,
            Self::Brain { prompt } | Self::Agent { prompt, .. } => prompt,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum VerdictKind {
    Pass,
    Fail,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Verdict {
    pub verdict: VerdictKind,
    /// Truncated output. Shell verifiers capture stdout+stderr;
    /// brain/agent verifiers capture the model's full reply.
    pub output: String,
    /// Cost in USD. Always 0 for `run` (no LLM call) and `brain`
    /// (local model, no marginal cost); populated for `agent`.
    pub cost_usd: f64,
}

/// Truncation cap for verifier output. 64KB is the issue spec; we honor
/// it here so the actuator and the brain-prompt builder agree on what
/// fits in `task_verifications.output`.
pub const OUTPUT_MAX_BYTES: usize = 64 * 1024;

pub fn truncate_output(s: &str) -> String {
    if s.len() <= OUTPUT_MAX_BYTES {
        return s.to_string();
    }
    let mut head = s[..OUTPUT_MAX_BYTES].to_string();
    head.push_str("\n[truncated]");
    head
}

/// Parse the leading `PASS` / `FAIL[:...]` marker brain and agent
/// verifiers must emit. Tolerant of leading whitespace and case so a
/// model that says "  fail: missing JWT" still parses. Defaults to
/// FAIL when the marker is missing — gates fail closed, not open.
pub fn parse_brain_verdict(reply: &str) -> VerdictKind {
    let trimmed = reply.trim_start();
    let lower = trimmed.to_ascii_lowercase();
    // Default to FAIL — RFC §5: gates fail closed, not open. A model
    // that omits the marker is treated the same as one that emits
    // FAIL, so neither path returns Pass.
    if lower.starts_with("pass") {
        VerdictKind::Pass
    } else {
        VerdictKind::Fail
    }
}

/// Side-effect surface for the verifier runner. Splits out so tests can
/// drive every kind without spawning shells / LLM calls. The production
/// impl lives in the binary's `commands.rs` next to `LiveSideEffects`.
pub trait VerifierBackend {
    /// Run a shell command in `cwd`. Returns combined stdout+stderr and
    /// the exit code. Output is truncated to `OUTPUT_MAX_BYTES` by the
    /// caller.
    fn run_shell(
        &self,
        cwd: &Path,
        command: &str,
        timeout: Duration,
    ) -> Result<ShellResult, String>;

    /// Query the local brain LLM with `prompt`. Returns the model's
    /// reply text; verdict parsing happens in `run_verifier`.
    fn query_brain(&self, prompt: &str) -> Result<String, String>;

    /// Run a headless `claude -p` agent with `prompt`. Honor the budget
    /// cap by killing the process if it exceeds spend; report the cost
    /// regardless so the ledger reflects what was actually spent.
    fn run_agent(
        &self,
        prompt: &str,
        model: Option<&str>,
        budget_usd: Option<f64>,
    ) -> Result<AgentResult, String>;
}

pub struct ShellResult {
    pub exit_code: i32,
    pub combined_output: String,
}

pub struct AgentResult {
    pub reply: String,
    pub cost_usd: f64,
}

/// Default per-verifier timeout. Shell verifiers blow past this if the
/// command misbehaves; the runner sends SIGTERM via `Command::kill`.
pub const DEFAULT_RUN_TIMEOUT: Duration = Duration::from_secs(15 * 60);

/// Execute one verifier and return the verdict + output. The actuator
/// records this as a `task_verifications` row before deciding whether
/// to move on to the next verifier (PASS) or short-circuit (FAIL).
pub fn run_verifier(
    backend: &dyn VerifierBackend,
    cwd: &Path,
    verifier: &Verifier,
) -> Result<Verdict, String> {
    match verifier {
        Verifier::Run { command } => {
            let r = backend.run_shell(cwd, command, DEFAULT_RUN_TIMEOUT)?;
            let verdict = if r.exit_code == 0 {
                VerdictKind::Pass
            } else {
                VerdictKind::Fail
            };
            Ok(Verdict {
                verdict,
                output: truncate_output(&r.combined_output),
                cost_usd: 0.0,
            })
        }
        Verifier::Brain { prompt } => {
            let reply = backend.query_brain(prompt)?;
            let verdict = parse_brain_verdict(&reply);
            Ok(Verdict {
                verdict,
                output: truncate_output(&reply),
                cost_usd: 0.0,
            })
        }
        Verifier::Agent {
            prompt,
            model,
            budget_usd,
        } => {
            let r = backend.run_agent(prompt, model.as_deref(), *budget_usd)?;
            let verdict = parse_brain_verdict(&r.reply);
            Ok(Verdict {
                verdict,
                output: truncate_output(&r.reply),
                cost_usd: r.cost_usd,
            })
        }
    }
}

/// Build the retry prompt fed back to the agent (RFC §5). Verifier
/// output is the gradient — the failing verifier's output is what
/// teaches the next attempt what to fix.
pub fn build_retry_prompt(
    original_prompt: &str,
    failed_verifier_kind: &str,
    failed_output: &str,
) -> String {
    let trimmed = original_prompt.trim_end();
    format!(
        "{trimmed}\n\nPrevious attempt failed verification:\n\
         {failed_verifier_kind}: {failed_output}\n\n\
         Fix these issues and try again."
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::path::PathBuf;

    struct StubBackend {
        shell_exit: i32,
        shell_output: String,
        brain_reply: String,
        agent_reply: String,
        agent_cost: f64,
        calls: RefCell<Vec<String>>,
    }

    impl Default for StubBackend {
        fn default() -> Self {
            Self {
                shell_exit: 0,
                shell_output: "ok".into(),
                brain_reply: "PASS".into(),
                agent_reply: "PASS".into(),
                agent_cost: 0.0,
                calls: RefCell::new(Vec::new()),
            }
        }
    }

    impl VerifierBackend for StubBackend {
        fn run_shell(
            &self,
            _cwd: &Path,
            command: &str,
            _timeout: Duration,
        ) -> Result<ShellResult, String> {
            self.calls.borrow_mut().push(format!("shell:{command}"));
            Ok(ShellResult {
                exit_code: self.shell_exit,
                combined_output: self.shell_output.clone(),
            })
        }
        fn query_brain(&self, prompt: &str) -> Result<String, String> {
            self.calls.borrow_mut().push(format!("brain:{prompt}"));
            Ok(self.brain_reply.clone())
        }
        fn run_agent(
            &self,
            prompt: &str,
            _model: Option<&str>,
            _budget_usd: Option<f64>,
        ) -> Result<AgentResult, String> {
            self.calls.borrow_mut().push(format!("agent:{prompt}"));
            Ok(AgentResult {
                reply: self.agent_reply.clone(),
                cost_usd: self.agent_cost,
            })
        }
    }

    #[test]
    fn run_verifier_exit_zero_is_pass() {
        let backend = StubBackend::default();
        let v = run_verifier(
            &backend,
            &PathBuf::from("/x"),
            &Verifier::Run {
                command: "cargo test".into(),
            },
        )
        .unwrap();
        assert_eq!(v.verdict, VerdictKind::Pass);
        assert_eq!(v.cost_usd, 0.0);
    }

    #[test]
    fn run_verifier_nonzero_exit_is_fail_with_output_captured() {
        let backend = StubBackend {
            shell_exit: 101,
            shell_output: "FAIL: assertion failed in tests/auth.rs".into(),
            ..Default::default()
        };
        let v = run_verifier(
            &backend,
            &PathBuf::from("/x"),
            &Verifier::Run {
                command: "cargo test".into(),
            },
        )
        .unwrap();
        assert_eq!(v.verdict, VerdictKind::Fail);
        assert!(v.output.contains("assertion failed"));
    }

    #[test]
    fn brain_verifier_parses_pass_and_fail_markers() {
        let backend = StubBackend {
            brain_reply: "PASS — looks clean".into(),
            ..Default::default()
        };
        let v = run_verifier(
            &backend,
            &PathBuf::from("/x"),
            &Verifier::Brain {
                prompt: "Review the diff for JWT coverage".into(),
            },
        )
        .unwrap();
        assert_eq!(v.verdict, VerdictKind::Pass);
        assert_eq!(v.cost_usd, 0.0, "brain gates are zero marginal cost");

        let backend2 = StubBackend {
            brain_reply: "FAIL: missing JWT check on /admin route".into(),
            ..Default::default()
        };
        let v2 = run_verifier(
            &backend2,
            &PathBuf::from("/x"),
            &Verifier::Brain {
                prompt: "Review the diff".into(),
            },
        )
        .unwrap();
        assert_eq!(v2.verdict, VerdictKind::Fail);
        assert!(v2.output.contains("missing JWT check"));
    }

    #[test]
    fn brain_verdict_defaults_to_fail_when_marker_missing() {
        // Models that don't emit a leading PASS/FAIL get treated as
        // FAIL — gates fail closed, not open. RFC §5 calls this out.
        assert_eq!(parse_brain_verdict("looks fine to me"), VerdictKind::Fail);
        assert_eq!(parse_brain_verdict(""), VerdictKind::Fail);
    }

    #[test]
    fn agent_verifier_carries_cost_through_to_verdict() {
        let backend = StubBackend {
            agent_reply: "PASS — could not find bypass".into(),
            agent_cost: 0.24,
            ..Default::default()
        };
        let v = run_verifier(
            &backend,
            &PathBuf::from("/x"),
            &Verifier::Agent {
                prompt: "Adversarial review: find a bypass".into(),
                model: Some("haiku".into()),
                budget_usd: Some(0.25),
            },
        )
        .unwrap();
        assert_eq!(v.verdict, VerdictKind::Pass);
        assert_eq!(v.cost_usd, 0.24);
    }

    #[test]
    fn retry_prompt_appends_failure_context() {
        let p = build_retry_prompt(
            "Add JWT middleware to all API routes",
            "run",
            "test failures:\n  assertion failed",
        );
        assert!(p.starts_with("Add JWT middleware"));
        assert!(p.contains("Previous attempt failed verification:"));
        assert!(p.contains("run: test failures:"));
        assert!(p.ends_with("Fix these issues and try again."));
    }

    #[test]
    fn truncate_output_respects_cap() {
        let s = "x".repeat(OUTPUT_MAX_BYTES + 1000);
        let out = truncate_output(&s);
        assert!(out.len() <= OUTPUT_MAX_BYTES + "\n[truncated]".len());
        assert!(out.ends_with("[truncated]"));
    }

    #[test]
    fn truncate_output_preserves_short_input() {
        let s = "small reply";
        let out = truncate_output(s);
        assert_eq!(out, s);
    }
}
