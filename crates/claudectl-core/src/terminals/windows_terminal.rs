use std::path::Path;

fn launch_title(cwd: &str, resume: Option<&str>) -> String {
    if let Some(session_id) = resume.filter(|value| !value.trim().is_empty()) {
        return format!("claude resume {session_id}");
    }

    let project = Path::new(cwd)
        .file_name()
        .and_then(|value| value.to_str())
        .filter(|value| !value.is_empty())
        .unwrap_or("claude");

    format!("claude: {project}")
}

fn build_wsl_command(cwd: &str, prompt: Option<&str>, resume: Option<&str>) -> String {
    let mut parts = vec![
        format!("cd {}", super::shell_escape(cwd)),
        "&&".to_string(),
        "exec".to_string(),
        "claude".to_string(),
    ];
    parts.extend(
        super::build_claude_args(prompt, resume)
            .into_iter()
            .map(|arg| super::shell_escape(&arg)),
    );
    parts.join(" ")
}

fn build_cmd_args(
    cwd: &str,
    prompt: Option<&str>,
    resume: Option<&str>,
    distro: Option<&str>,
) -> Vec<String> {
    let mut args = vec![
        "/c".to_string(),
        "wt.exe".to_string(),
        "-w".to_string(),
        "0".to_string(),
        "new-tab".to_string(),
        "--title".to_string(),
        launch_title(cwd, resume),
        "wsl.exe".to_string(),
    ];

    if let Some(distro_name) = distro.filter(|value| !value.trim().is_empty()) {
        args.push("-d".to_string());
        args.push(distro_name.to_string());
    }

    args.push("bash".to_string());
    args.push("-lc".to_string());
    args.push(build_wsl_command(cwd, prompt, resume));
    args
}

pub fn launch(cwd: &str, prompt: Option<&str>, resume: Option<&str>) -> Result<String, String> {
    let distro = std::env::var("WSL_DISTRO_NAME").ok();
    let output = std::process::Command::new("cmd.exe")
        .args(build_cmd_args(cwd, prompt, resume, distro.as_deref()))
        .output()
        .map_err(|e| format!("cmd.exe /c wt.exe new-tab failed: {e}"))?;

    if output.status.success() {
        Ok("Windows Terminal tab".into())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        if stderr.is_empty() {
            Err(String::from_utf8_lossy(&output.stdout).trim().to_string())
        } else {
            Err(stderr)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_wsl_command_shell_escapes_cwd_and_args() {
        let command = build_wsl_command("/tmp/ship it", Some("say 'hi'"), Some("session-7"));

        assert_eq!(
            command,
            "cd '/tmp/ship it' && exec claude '--resume' 'session-7' '-p' 'say '\"'\"'hi'\"'\"''"
        );
    }

    #[test]
    fn build_cmd_args_targets_current_window_and_distro() {
        let args = build_cmd_args("/work/repo", None, None, Some("Ubuntu"));

        assert_eq!(
            args,
            vec![
                "/c",
                "wt.exe",
                "-w",
                "0",
                "new-tab",
                "--title",
                "claude: repo",
                "wsl.exe",
                "-d",
                "Ubuntu",
                "bash",
                "-lc",
                "cd '/work/repo' && exec claude",
            ]
        );
    }

    #[test]
    fn launch_title_prefers_resume_id_when_present() {
        assert_eq!(
            launch_title("/tmp/project", Some("session-123")),
            "claude resume session-123"
        );
    }
}
