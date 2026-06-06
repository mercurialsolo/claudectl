use crate::session::ClaudeSession;

pub fn launch(cwd: &str, prompt: Option<&str>, resume: Option<&str>) -> Result<String, String> {
    let mut cmd = std::process::Command::new("wezterm");
    cmd.args(["cli", "spawn", "--cwd", cwd, "--", "claude"]);
    for arg in super::build_claude_args(prompt, resume) {
        cmd.arg(arg);
    }

    let output = cmd
        .output()
        .map_err(|e| format!("wezterm cli spawn failed: {e}"))?;

    if output.status.success() {
        Ok("wezterm pane".into())
    } else {
        Err(String::from_utf8_lossy(&output.stderr).trim().to_string())
    }
}

pub fn switch(session: &ClaudeSession) -> Result<(), String> {
    // WezTerm has `wezterm cli list` and `wezterm cli activate-pane`.
    // `wezterm cli list --format json` shows all panes with their cwd and tty.
    let output = std::process::Command::new("wezterm")
        .args(["cli", "list", "--format", "json"])
        .output()
        .map_err(|e| format!("wezterm cli failed: {e}"))?;

    if !output.status.success() {
        return Err("wezterm cli list failed".into());
    }

    let json: serde_json::Value = serde_json::from_slice(&output.stdout)
        .map_err(|e| format!("Failed to parse wezterm output: {e}"))?;

    // Find pane matching our cwd or tty
    if let Some(panes) = json.as_array() {
        for pane in panes {
            let pane_cwd = pane.get("cwd").and_then(|v| v.as_str()).unwrap_or("");
            let pane_tty = pane.get("tty_name").and_then(|v| v.as_str()).unwrap_or("");

            if pane_cwd.contains(&session.project_name) || pane_tty.contains(&session.tty) {
                if let Some(pane_id) = pane.get("pane_id").and_then(|v| v.as_u64()) {
                    let result = std::process::Command::new("wezterm")
                        .args(["cli", "activate-pane", "--pane-id", &pane_id.to_string()])
                        .output()
                        .map_err(|e| format!("wezterm activate-pane failed: {e}"))?;

                    if result.status.success() {
                        return Ok(());
                    }
                }
            }
        }
    }

    Err("Session not found in WezTerm pane list".into())
}
