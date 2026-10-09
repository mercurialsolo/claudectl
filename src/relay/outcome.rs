// What a delegated `claude --print` run actually cost and produced (#493).
//
// Until now the worker reported zero for both. `WorkerTask::tokens_used` and
// `cost_usd` were initialised to zero and only ever read, so every delegated
// task looked free — on a tool whose whole premise is cost tracking.
//
// `claude --print --output-format json` answers all of it directly. That is
// deliberately preferred over re-deriving the numbers from the session's
// transcript with `monitor.rs`: this is Claude Code's own accounting, so the
// two cannot disagree, and there is no session id to discover or pricing table
// to keep current. The pricing table has already been wrong once.

/// The outcome of a delegated run, read from `--output-format json`.
#[derive(Debug, Clone, PartialEq)]
pub struct Outcome {
    /// Did the run fail? From `is_error`.
    pub failed: bool,
    /// On success the session's output; on failure the reason. Both come from
    /// `result`, which is the only place the reason appears — stderr is empty
    /// for an ordinary failure such as not being logged in.
    pub summary: String,
    pub total_cost_usd: f64,
    pub total_tokens: u64,
    /// Which model actually ran. Otherwise unknowable from the delegating side.
    pub model: Option<String>,
    /// The `usage` object verbatim, so the token breakdown is not lost.
    pub usage: serde_json::Value,
}

/// How much of `result` to carry. It crosses the wire and lands in every
/// `relay status` row, and a long answer is the normal case for a Claude
/// session, not the edge.
pub const SUMMARY_CAP: usize = 2048;

/// Keep a summary to something a table can hold and a message should carry.
pub fn cap_summary(s: &str) -> String {
    let trimmed = s.trim();
    if trimmed.len() <= SUMMARY_CAP {
        return trimmed.to_string();
    }
    // Cut on a character boundary, not a byte one.
    let mut end = SUMMARY_CAP;
    while end > 0 && !trimmed.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &trimmed[..end])
}

/// Read the JSON a finished `claude --print --output-format json` left behind.
///
/// `None` when the output is not that JSON at all — claude died before writing
/// it, or wrote something else entirely — which is the caller's signal to fall
/// back to the exit status.
pub fn parse(stdout: &str) -> Option<Outcome> {
    let v: serde_json::Value = serde_json::from_str(stdout.trim()).ok()?;
    // `total_cost_usd` is the field this exists for; without it this is some
    // other JSON and we should not guess at its shape.
    let total_cost_usd = v.get("total_cost_usd")?.as_f64()?;

    // `subtype` reads "success" even when the run failed, so `is_error` is the
    // only trustworthy signal here.
    let failed = v.get("is_error").and_then(|b| b.as_bool()).unwrap_or(false);

    let usage = v.get("usage").cloned().unwrap_or(serde_json::json!({}));
    let n = |k: &str| usage.get(k).and_then(|x| x.as_u64()).unwrap_or(0);
    // Same convention as `monitor.rs`: everything the turn was billed for, so
    // a delegated task's token count means the same as a local one's.
    let total_tokens = n("input_tokens")
        + n("cache_read_input_tokens")
        + n("cache_creation_input_tokens")
        + n("output_tokens");

    // `modelUsage` is keyed by model id. More than one appears when a cheaper
    // model handled part of the work, so take the costliest as the one that
    // ran the task.
    let model = v
        .get("modelUsage")
        .and_then(|m| m.as_object())
        .and_then(|m| {
            m.iter()
                .max_by(|a, b| {
                    let cost = |x: &serde_json::Value| {
                        x.get("costUSD").and_then(|c| c.as_f64()).unwrap_or(0.0)
                    };
                    cost(a.1).total_cmp(&cost(b.1))
                })
                .map(|(k, _)| k.clone())
        });

    Some(Outcome {
        failed,
        summary: cap_summary(v.get("result").and_then(|r| r.as_str()).unwrap_or("")),
        total_cost_usd,
        total_tokens,
        model,
        usage,
    })
}

#[cfg(test)]
mod parsing {
    use super::*;

    /// Trimmed from a real `claude --print --output-format json` run.
    const SUCCESS: &str = r#"{
      "duration_api_ms": 6059,
      "session_id": "7cabcd29-f695-481c-8bb8-e48bfd43c6a2",
      "total_cost_usd": 0.32897649999999995,
      "usage": {
        "input_tokens": 2,
        "cache_creation_input_tokens": 32109,
        "cache_read_input_tokens": 13381,
        "output_tokens": 9,
        "cache_creation": {"ephemeral_1h_input_tokens": 32109, "ephemeral_5m_input_tokens": 0}
      },
      "modelUsage": {
        "claude-haiku-4-5-20251001": {"costUSD": 0.000961, "canonicalModel": "claude-haiku-4-5"},
        "claude-opus-5[1m]": {"costUSD": 0.32801549999999996, "canonicalModel": "claude-opus-5"}
      },
      "is_error": false,
      "num_turns": 1,
      "subtype": "success",
      "result": "json-probe-ok",
      "type": "result"
    }"#;

    /// A real not-logged-in run. Note `subtype` still says "success".
    const NOT_LOGGED_IN: &str = r#"{
      "session_id": "71f82f5f-b594-46de-b852-1bdd236798e1",
      "total_cost_usd": 0,
      "usage": {"input_tokens": 0, "cache_creation_input_tokens": 0,
                "cache_read_input_tokens": 0, "output_tokens": 0},
      "is_error": true,
      "subtype": "success",
      "terminal_reason": "api_error",
      "result": "Not logged in · Please run /login",
      "type": "result"
    }"#;

    #[test]
    fn a_successful_run_reports_its_cost_and_output() {
        let o = parse(SUCCESS).expect("parses");
        assert!(!o.failed);
        assert_eq!(o.total_cost_usd, 0.32897649999999995);
        assert_eq!(o.summary, "json-probe-ok");
        // 2 + 13381 + 32109 + 9
        assert_eq!(o.total_tokens, 45501);
    }

    #[test]
    fn the_model_that_did_the_work_is_the_costliest_one() {
        let o = parse(SUCCESS).expect("parses");
        assert_eq!(o.model.as_deref(), Some("claude-opus-5[1m]"));
    }

    #[test]
    fn the_usage_breakdown_is_kept_rather_than_flattened() {
        let o = parse(SUCCESS).expect("parses");
        assert_eq!(
            o.usage
                .get("cache_creation")
                .and_then(|c| c.get("ephemeral_1h_input_tokens"))
                .and_then(|v| v.as_u64()),
            Some(32109),
            "the 1h cache tier must survive, since it is priced differently"
        );
    }

    // The failure the worker is most likely to hit, and the one that bit me in
    // a fixture: a worker whose HOME has no credentials.
    #[test]
    fn a_failed_run_carries_the_reason_a_person_can_act_on() {
        let o = parse(NOT_LOGGED_IN).expect("parses");
        assert!(o.failed);
        assert_eq!(o.summary, "Not logged in · Please run /login");
        assert_eq!(o.total_cost_usd, 0.0);
        assert_eq!(o.total_tokens, 0);
    }

    // `subtype` is "success" on that failure, so anything reading it instead
    // of `is_error` would call a failed run successful.
    #[test]
    fn subtype_is_not_the_success_signal() {
        let v: serde_json::Value = serde_json::from_str(NOT_LOGGED_IN).unwrap();
        assert_eq!(v.get("subtype").and_then(|s| s.as_str()), Some("success"));
        assert!(parse(NOT_LOGGED_IN).expect("parses").failed);
    }

    // Anything that is not this JSON means claude never got as far as writing
    // it, so the caller has to fall back to the exit status.
    #[test]
    fn output_that_is_not_the_result_json_does_not_parse() {
        for bad in [
            "",
            "   ",
            "not json at all",
            "{}",
            r#"{"result": "hi"}"#,
            r#"{"total_cost_usd": "free"}"#,
            "Killed: 9",
        ] {
            assert!(parse(bad).is_none(), "{bad:?} must not parse");
        }
    }

    #[test]
    fn a_run_with_no_usage_reports_zero_rather_than_a_guess() {
        let o = parse(r#"{"total_cost_usd": 0.0, "is_error": false}"#).expect("parses");
        assert_eq!(o.total_tokens, 0);
        assert_eq!(o.summary, "");
        assert_eq!(o.model, None);
    }

    #[test]
    fn a_long_answer_is_capped_rather_than_sent_whole() {
        let long = "x".repeat(SUMMARY_CAP * 3);
        let json = format!(r#"{{"total_cost_usd": 1.0, "result": "{long}"}}"#);
        let o = parse(&json).expect("parses");
        assert!(o.summary.len() <= SUMMARY_CAP + 4, "capped");
        assert!(o.summary.ends_with('…'), "and marked as truncated");
    }

    #[test]
    fn capping_does_not_split_a_character() {
        // A multi-byte character straddling the cap must not be cut in half.
        let s = "é".repeat(SUMMARY_CAP);
        let capped = cap_summary(&s);
        assert!(capped.len() <= SUMMARY_CAP + 4);
        assert!(capped.ends_with('…'));
    }
}
