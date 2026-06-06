use crate::session::ClaudeSession;

pub fn launch(cwd: &str, prompt: Option<&str>, resume: Option<&str>) -> Result<String, String> {
    let mut cmd = std::process::Command::new("gnome-terminal");
    cmd.args(["--window", "--working-directory", cwd, "--", "claude"]);
    for arg in super::build_claude_args(prompt, resume) {
        cmd.arg(arg);
    }

    let output = cmd
        .output()
        .map_err(|e| format!("gnome-terminal launch failed: {e}"))?;

    if output.status.success() {
        Ok("gnome-terminal window".into())
    } else {
        Err(String::from_utf8_lossy(&output.stderr).trim().to_string())
    }
}

pub fn switch(_session: &ClaudeSession) -> Result<(), String> {
    Err(
        "GNOME Terminal launch is supported, but remote focus/input control is not yet reliable. Use tmux or Kitty for session switching and input automation."
            .into(),
    )
}

pub fn send_input(_session: &ClaudeSession, _text: &str) -> Result<(), String> {
    Err(
        "GNOME Terminal launch is supported, but remote focus/input control is not yet reliable. Use tmux or Kitty for session input automation."
            .into(),
    )
}

pub fn approve(_session: &ClaudeSession) -> Result<(), String> {
    Err(
        "GNOME Terminal launch is supported, but remote focus/input control is not yet reliable. Use tmux or Kitty for approval automation."
            .into(),
    )
}
