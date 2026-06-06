#[cfg(target_os = "macos")]
mod apple;
#[cfg(target_os = "macos")]
mod ghostty;
mod gnome_terminal;
#[cfg(target_os = "macos")]
mod iterm2;
mod kitty;
mod tmux;
#[cfg(target_os = "macos")]
mod warp;
mod wezterm;
mod windows_terminal;

use crate::session::ClaudeSession;
use std::path::PathBuf;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TerminalAction {
    Launch,
    Switch,
    Input,
    Approve,
}

impl TerminalAction {
    fn label(&self) -> &'static str {
        match self {
            TerminalAction::Launch => "Launch new session",
            TerminalAction::Switch => "Switch to session terminal",
            TerminalAction::Input => "Send input to session",
            TerminalAction::Approve => "Approve prompt",
        }
    }

    fn summary_name(&self) -> &'static str {
        match self {
            TerminalAction::Launch => "launch",
            TerminalAction::Switch => "switch",
            TerminalAction::Input => "input",
            TerminalAction::Approve => "approve",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DoctorStatus {
    Ready,
    Blocked,
    Unsupported,
}

impl DoctorStatus {
    fn label(&self) -> &'static str {
        match self {
            DoctorStatus::Ready => "ok",
            DoctorStatus::Blocked => "blocked",
            DoctorStatus::Unsupported => "n/a",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DoctorCheck {
    pub name: &'static str,
    pub status: DoctorStatus,
    pub detail: String,
    pub fix: Option<String>,
}

impl DoctorCheck {
    fn ready(name: &'static str, detail: impl Into<String>) -> Self {
        Self {
            name,
            status: DoctorStatus::Ready,
            detail: detail.into(),
            fix: None,
        }
    }

    fn blocked(
        name: &'static str,
        detail: impl Into<String>,
        fix: impl Into<Option<String>>,
    ) -> Self {
        Self {
            name,
            status: DoctorStatus::Blocked,
            detail: detail.into(),
            fix: fix.into(),
        }
    }

    fn unsupported(name: &'static str, detail: impl Into<String>) -> Self {
        Self {
            name,
            status: DoctorStatus::Unsupported,
            detail: detail.into(),
            fix: None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DoctorReport {
    pub terminal: String,
    pub platform: String,
    pub actions: Vec<DoctorCheck>,
    pub prerequisites: Vec<DoctorCheck>,
    pub notes: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Terminal {
    Gnome,
    Ghostty,
    Warp,
    ITerm2,
    Kitty,
    WezTerm,
    WindowsTerm,
    Apple,
    Tmux,
    Unknown(String),
}

fn terminal_name(t: &Terminal) -> &str {
    match t {
        Terminal::Gnome => "GNOME Terminal",
        Terminal::Ghostty => "Ghostty",
        Terminal::Warp => "Warp",
        Terminal::ITerm2 => "iTerm2",
        Terminal::Kitty => "Kitty",
        Terminal::WezTerm => "WezTerm",
        Terminal::WindowsTerm => "Windows Terminal",
        Terminal::Apple => "Apple Terminal",
        Terminal::Tmux => "tmux",
        Terminal::Unknown(name) => name,
    }
}

fn platform_label(os: &str, is_wsl: bool) -> String {
    if is_wsl && os == "linux" {
        "linux (WSL)".to_string()
    } else {
        os.to_string()
    }
}

fn platform_name() -> String {
    platform_label(std::env::consts::OS, is_wsl())
}

fn environment_notes(is_wsl: bool, has_windows_terminal_bridge: bool) -> Vec<String> {
    if !is_wsl {
        return Vec::new();
    }

    let mut notes = vec![
        "WSL detected. Linux session discovery should work normally inside the distro."
            .to_string(),
        "For reliable switch, input, and approval automation in WSL today, prefer tmux or Kitty inside WSL."
            .to_string(),
    ];

    if has_windows_terminal_bridge {
        notes.push(
            "Windows Terminal launch is available from WSL through `cmd.exe /c wt.exe`, but tab switching and input automation still rely on tmux or Kitty."
                .to_string(),
        );
    } else {
        notes.push(
            "Windows Terminal launch is not available from this WSL shell, so claudectl currently relies on Linux-native terminals inside WSL."
                .to_string(),
        );
    }

    notes
}

fn windows_terminal_bridge_ready() -> bool {
    command_ready("cmd.exe") && command_ready("wt.exe")
}

fn wsl_interop_check(is_wsl: bool) -> Option<DoctorCheck> {
    if !is_wsl {
        return None;
    }

    if windows_terminal_bridge_ready() {
        Some(DoctorCheck::ready(
            "Windows Terminal interop",
            "`cmd.exe /c wt.exe` is reachable from WSL.",
        ))
    } else if !command_ready("cmd.exe") {
        Some(DoctorCheck::blocked(
            "Windows Terminal interop",
            "`cmd.exe` is not on PATH from this WSL environment.",
            Some(
                "Enable WSL Windows interop or reopen this distro from a normal WSL shell."
                    .to_string(),
            ),
        ))
    } else {
        Some(DoctorCheck::blocked(
            "Windows Terminal interop",
            "`wt.exe` is not on PATH from this WSL environment.",
            Some(
                "Install Windows Terminal or enable WSL interop, then reopen the shell."
                    .to_string(),
            ),
        ))
    }
}

fn is_wsl() -> bool {
    #[cfg(target_os = "linux")]
    {
        if std::env::var_os("WSL_DISTRO_NAME").is_some()
            || std::env::var_os("WSL_INTEROP").is_some()
        {
            return true;
        }

        for path in ["/proc/sys/kernel/osrelease", "/proc/version"] {
            let Ok(contents) = std::fs::read_to_string(path) else {
                continue;
            };

            if contents.to_ascii_lowercase().contains("microsoft") {
                return true;
            }
        }
    }

    false
}

fn supported_actions(terminal: &Terminal) -> Vec<TerminalAction> {
    match terminal {
        Terminal::Gnome | Terminal::WindowsTerm => vec![TerminalAction::Launch],
        Terminal::Kitty | Terminal::Tmux => vec![
            TerminalAction::Launch,
            TerminalAction::Switch,
            TerminalAction::Input,
            TerminalAction::Approve,
        ],
        Terminal::WezTerm => vec![TerminalAction::Launch, TerminalAction::Switch],
        #[cfg(target_os = "macos")]
        Terminal::Ghostty | Terminal::Warp | Terminal::ITerm2 | Terminal::Apple => vec![
            TerminalAction::Switch,
            TerminalAction::Input,
            TerminalAction::Approve,
        ],
        Terminal::Unknown(_) => Vec::new(),
        #[cfg(not(target_os = "macos"))]
        _ => Vec::new(),
    }
}

pub(crate) fn build_claude_args(prompt: Option<&str>, resume: Option<&str>) -> Vec<String> {
    let mut args = Vec::new();
    if let Some(resume_id) = resume {
        args.push("--resume".to_string());
        args.push(resume_id.to_string());
    }
    if let Some(prompt_text) = prompt {
        args.push("-p".to_string());
        args.push(prompt_text.to_string());
    }
    args
}

pub(crate) fn shell_escape(arg: &str) -> String {
    format!("'{}'", arg.replace('\'', "'\"'\"'"))
}

pub fn detect_terminal() -> Terminal {
    if std::env::var("TMUX").is_ok() {
        return Terminal::Tmux;
    }

    if std::env::var("GNOME_TERMINAL_SERVICE").is_ok()
        || std::env::var("GNOME_TERMINAL_SCREEN").is_ok()
        || ancestor_process_contains("gnome-terminal")
    {
        return Terminal::Gnome;
    }

    if is_wsl() && std::env::var_os("WT_SESSION").is_some() {
        return Terminal::WindowsTerm;
    }

    // Terminal-specific env vars that don't rely on TERM_PROGRAM.
    // Some terminals (notably kitty on Linux) don't set TERM_PROGRAM at all.
    if let Some(term) = detect_by_native_env() {
        return term;
    }

    match std::env::var("TERM_PROGRAM").as_deref() {
        Ok("ghostty") => Terminal::Ghostty,
        Ok("WarpTerminal") => Terminal::Warp,
        Ok("iTerm.app") => Terminal::ITerm2,
        Ok("kitty") => Terminal::Kitty,
        Ok("WezTerm") => Terminal::WezTerm,
        Ok("Apple_Terminal") => Terminal::Apple,
        Ok(other) => Terminal::Unknown(other.to_string()),
        Err(_) => Terminal::Unknown("unknown".to_string()),
    }
}

/// Detect terminal from native env vars that each terminal sets unconditionally,
/// without relying on TERM_PROGRAM (which some terminals don't set on Linux).
fn detect_by_native_env() -> Option<Terminal> {
    // Kitty: KITTY_WINDOW_ID is set unconditionally per-window.
    // TERM=xterm-kitty is also reliable but can be inherited by child shells.
    if std::env::var_os("KITTY_WINDOW_ID").is_some() {
        return Some(Terminal::Kitty);
    }

    // WezTerm: WEZTERM_EXECUTABLE is set on all platforms.
    if std::env::var_os("WEZTERM_EXECUTABLE").is_some() {
        return Some(Terminal::WezTerm);
    }

    // Ghostty: GHOSTTY_RESOURCES_DIR is set on all platforms.
    if std::env::var_os("GHOSTTY_RESOURCES_DIR").is_some() {
        return Some(Terminal::Ghostty);
    }

    // TERM=xterm-kitty as last resort (weaker signal — can be inherited through ssh/tmux)
    if std::env::var("TERM").as_deref() == Ok("xterm-kitty") {
        return Some(Terminal::Kitty);
    }

    None
}

fn ancestor_process_contains(needle: &str) -> bool {
    let mut pid = unsafe { libc::getppid() } as u32;
    let needle = needle.to_ascii_lowercase();

    for _ in 0..8 {
        if pid == 0 {
            break;
        }

        let output = match std::process::Command::new("ps")
            .args(["-o", "ppid=,comm=", "-p", &pid.to_string()])
            .output()
        {
            Ok(output) => output,
            Err(_) => return false,
        };

        let stdout = String::from_utf8_lossy(&output.stdout);
        let line = stdout.trim();
        if line.is_empty() {
            break;
        }

        let mut parts = line.split_whitespace();
        let parent = parts
            .next()
            .and_then(|value| value.parse::<u32>().ok())
            .unwrap_or(0);
        let command = parts.collect::<Vec<_>>().join(" ").to_ascii_lowercase();
        if command.contains(&needle) {
            return true;
        }
        pid = parent;
    }

    false
}

pub fn can_launch_session() -> bool {
    supported_actions(&detect_terminal()).contains(&TerminalAction::Launch)
}

pub fn help_capability_summary() -> String {
    help_capability_summary_for(&detect_terminal())
}

fn help_capability_summary_for(terminal: &Terminal) -> String {
    let actions = supported_actions(terminal);
    if actions.is_empty() {
        format!(
            "Current terminal: {} (monitor-only)",
            terminal_name(terminal)
        )
    } else {
        let summary = actions
            .iter()
            .map(TerminalAction::summary_name)
            .collect::<Vec<_>>()
            .join(", ");
        format!("Current terminal: {} ({summary})", terminal_name(terminal))
    }
}

fn find_command_path(name: &str) -> Option<PathBuf> {
    if name.contains(std::path::MAIN_SEPARATOR) {
        let path = PathBuf::from(name);
        return path.is_file().then_some(path);
    }

    let path_var = std::env::var_os("PATH")?;
    std::env::split_paths(&path_var)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
}

fn binary_check(name: &'static str) -> DoctorCheck {
    match find_command_path(name) {
        Some(path) => DoctorCheck::ready(name, format!("Found at {}", path.display())),
        None => DoctorCheck::blocked(
            name,
            format!("`{name}` is not on PATH."),
            Some(format!("Install `{name}` or add it to PATH.")),
        ),
    }
}

fn command_ready(name: &'static str) -> bool {
    find_command_path(name).is_some()
}

fn output_message(output: &std::process::Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    if !stderr.is_empty() {
        return stderr;
    }
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if !stdout.is_empty() {
        return stdout;
    }
    format!("Command exited with status {}", output.status)
}

fn probe_kitty_remote_control() -> Result<(), String> {
    let output = std::process::Command::new("kitty")
        .args(["@", "ls"])
        .output()
        .map_err(|e| format!("kitty @ ls failed: {e}"))?;

    if output.status.success() {
        Ok(())
    } else {
        Err(output_message(&output))
    }
}

fn probe_tmux_connectivity() -> Result<(), String> {
    let output = std::process::Command::new("tmux")
        .args(["list-panes", "-a", "-F", "#{pane_tty}"])
        .output()
        .map_err(|e| format!("tmux list-panes failed: {e}"))?;

    if output.status.success() {
        Ok(())
    } else {
        Err(output_message(&output))
    }
}

fn probe_wezterm_cli() -> Result<(), String> {
    let output = std::process::Command::new("wezterm")
        .args(["cli", "list", "--format", "json"])
        .output()
        .map_err(|e| format!("wezterm cli list failed: {e}"))?;

    if output.status.success() {
        Ok(())
    } else {
        Err(output_message(&output))
    }
}

#[cfg(target_os = "macos")]
fn probe_system_events_access() -> Result<(), String> {
    let script = r#"tell application "System Events" to return UI elements enabled"#;
    let output = std::process::Command::new("osascript")
        .args(["-e", script])
        .output()
        .map_err(|e| format!("osascript probe failed: {e}"))?;

    if output.status.success() {
        Ok(())
    } else {
        Err(output_message(&output))
    }
}

fn action_check(
    action: TerminalAction,
    status: DoctorStatus,
    detail: impl Into<String>,
    fix: impl Into<Option<String>>,
) -> DoctorCheck {
    match status {
        DoctorStatus::Ready => DoctorCheck::ready(action.label(), detail),
        DoctorStatus::Blocked => DoctorCheck::blocked(action.label(), detail, fix),
        DoctorStatus::Unsupported => DoctorCheck::unsupported(action.label(), detail.into()),
    }
}

pub fn doctor_report() -> DoctorReport {
    doctor_report_for(detect_terminal())
}

fn doctor_report_for(terminal: Terminal) -> DoctorReport {
    let terminal_label = terminal_name(&terminal).to_string();
    let is_wsl = is_wsl();
    let mut prerequisites = vec![binary_check("claude")];
    if let Some(wsl_check) = wsl_interop_check(is_wsl) {
        prerequisites.push(wsl_check);
    }
    let mut actions = Vec::new();
    let mut notes = vec![
        "Run `claudectl --doctor` inside the same terminal family that launches Claude."
            .to_string(),
        "`n` and `--new` use the same launch capability shown here.".to_string(),
    ];
    notes.extend(environment_notes(is_wsl, windows_terminal_bridge_ready()));

    match terminal {
        Terminal::Gnome => {
            let gnome_check = binary_check("gnome-terminal");
            let gnome_ready = gnome_check.status == DoctorStatus::Ready;
            prerequisites.push(gnome_check);

            let launch_status = if gnome_ready {
                DoctorStatus::Ready
            } else {
                DoctorStatus::Blocked
            };
            let launch_detail = if gnome_ready {
                "GNOME Terminal can launch visible Claude sessions with `gnome-terminal --window`."
            } else {
                "GNOME Terminal CLI is unavailable, so visible launch cannot run."
            };
            let launch_fix =
                Some("Install GNOME Terminal and ensure `gnome-terminal` is on PATH.".to_string());
            actions.push(action_check(
                TerminalAction::Launch,
                launch_status,
                launch_detail,
                launch_fix.clone(),
            ));

            for action in [
                TerminalAction::Switch,
                TerminalAction::Input,
                TerminalAction::Approve,
            ] {
                actions.push(action_check(
                    action,
                    DoctorStatus::Unsupported,
                    "GNOME Terminal launch is supported, but reliable remote focus/input automation is not currently available.",
                    Some(
                        "Use tmux or Kitty when you need remote switching, input, or approval from claudectl."
                            .to_string(),
                    ),
                ));
            }

            notes.push(
                "GNOME Terminal launch works on Linux and was smoke-tested under Docker/X11. Remote focus/input automation is intentionally disabled until window targeting is reliable."
                    .to_string(),
            );
        }
        Terminal::WindowsTerm => {
            let cmd_check = binary_check("cmd.exe");
            let cmd_ready = cmd_check.status == DoctorStatus::Ready;
            prerequisites.push(cmd_check);

            let wt_check = binary_check("wt.exe");
            let wt_ready = wt_check.status == DoctorStatus::Ready;
            prerequisites.push(wt_check);

            let launch_status = if cmd_ready && wt_ready {
                DoctorStatus::Ready
            } else {
                DoctorStatus::Blocked
            };
            let launch_detail = if launch_status == DoctorStatus::Ready {
                "Windows Terminal can open a new WSL tab in the current window and run `claude` there."
            } else {
                "Windows Terminal launch needs both `cmd.exe` and `wt.exe` reachable from this WSL shell."
            };
            let launch_fix = Some(
                "Enable WSL Windows interop, ensure Windows Terminal is installed, then rerun `claudectl --doctor`."
                    .to_string(),
            );
            actions.push(action_check(
                TerminalAction::Launch,
                launch_status,
                launch_detail,
                launch_fix.clone(),
            ));

            for action in [
                TerminalAction::Switch,
                TerminalAction::Input,
                TerminalAction::Approve,
            ] {
                actions.push(action_check(
                    action,
                    DoctorStatus::Unsupported,
                    "Windows Terminal launch works from WSL, but remote tab switching and input automation are not implemented there yet.",
                    Some(
                        "Use tmux or Kitty inside WSL when you need switch/input/approve automation."
                            .to_string(),
                    ),
                ));
            }

            notes.push(
                "Windows Terminal support is WSL-only and currently covers visible launch into a new tab, not remote control of existing tabs."
                    .to_string(),
            );
        }
        Terminal::Kitty => {
            let kitty_check = binary_check("kitty");
            let kitty_ready = kitty_check.status == DoctorStatus::Ready;
            prerequisites.push(kitty_check);

            let remote_check = if kitty_ready {
                match probe_kitty_remote_control() {
                    Ok(()) => DoctorCheck::ready(
                        "kitty remote control",
                        "`kitty @` is reachable from this shell.",
                    ),
                    Err(err) => DoctorCheck::blocked(
                        "kitty remote control",
                        format!("`kitty @` is unavailable: {err}"),
                        Some(
                            "Set `allow_remote_control yes` or `allow_remote_control socket-only` in kitty.conf, then restart Kitty."
                                .to_string(),
                        ),
                    ),
                }
            } else {
                DoctorCheck::blocked(
                    "kitty remote control",
                    "Kitty CLI is unavailable, so `kitty @` cannot be used.",
                    Some("Install Kitty and ensure `kitty` is on PATH.".to_string()),
                )
            };
            let remote_ready = remote_check.status == DoctorStatus::Ready;
            prerequisites.push(remote_check);

            let action_status = if kitty_ready && remote_ready {
                DoctorStatus::Ready
            } else {
                DoctorStatus::Blocked
            };
            let detail = if action_status == DoctorStatus::Ready {
                "Kitty can focus tabs and send text through `kitty @`."
            } else {
                "Kitty support is configured, but remote control is not currently available."
            };
            let fix = Some(
                "Enable Kitty remote control in kitty.conf and rerun `claudectl --doctor`."
                    .to_string(),
            );

            for action in supported_actions(&Terminal::Kitty) {
                actions.push(action_check(action, action_status, detail, fix.clone()));
            }
        }
        Terminal::Tmux => {
            let tmux_check = binary_check("tmux");
            let tmux_ready = tmux_check.status == DoctorStatus::Ready;
            prerequisites.push(tmux_check);

            let session_check = if tmux_ready {
                match probe_tmux_connectivity() {
                    Ok(()) => DoctorCheck::ready(
                        "tmux session access",
                        "`tmux list-panes` can see the active server.",
                    ),
                    Err(err) => DoctorCheck::blocked(
                        "tmux session access",
                        format!("tmux is installed, but pane discovery failed: {err}"),
                        Some("Run claudectl from inside the tmux session that owns the Claude panes.".to_string()),
                    ),
                }
            } else {
                DoctorCheck::blocked(
                    "tmux session access",
                    "tmux is unavailable, so pane discovery cannot run.",
                    Some("Install tmux and rerun `claudectl --doctor`.".to_string()),
                )
            };
            let session_ready = session_check.status == DoctorStatus::Ready;
            prerequisites.push(session_check);

            let action_status = if tmux_ready && session_ready {
                DoctorStatus::Ready
            } else {
                DoctorStatus::Blocked
            };
            let detail = if action_status == DoctorStatus::Ready {
                "tmux can open windows, locate panes by TTY, and send keys."
            } else {
                "tmux support needs a reachable tmux server from this shell."
            };
            let fix = Some(
                "Run claudectl inside tmux or connect it to the same tmux server.".to_string(),
            );

            for action in supported_actions(&Terminal::Tmux) {
                actions.push(action_check(action, action_status, detail, fix.clone()));
            }
        }
        Terminal::WezTerm => {
            let wezterm_check = binary_check("wezterm");
            let wezterm_ready = wezterm_check.status == DoctorStatus::Ready;
            prerequisites.push(wezterm_check);

            let cli_check = if wezterm_ready {
                match probe_wezterm_cli() {
                    Ok(()) => DoctorCheck::ready(
                        "wezterm cli",
                        "`wezterm cli` can query panes from this shell.",
                    ),
                    Err(err) => DoctorCheck::blocked(
                        "wezterm cli",
                        format!("WezTerm CLI is installed, but pane discovery failed: {err}"),
                        Some(
                            "Run claudectl inside WezTerm with a reachable mux server.".to_string(),
                        ),
                    ),
                }
            } else {
                DoctorCheck::blocked(
                    "wezterm cli",
                    "WezTerm CLI is unavailable, so pane discovery cannot run.",
                    Some("Install WezTerm and ensure `wezterm` is on PATH.".to_string()),
                )
            };
            let cli_ready = cli_check.status == DoctorStatus::Ready;
            prerequisites.push(cli_check);

            let action_status = if wezterm_ready && cli_ready {
                DoctorStatus::Ready
            } else {
                DoctorStatus::Blocked
            };
            let detail = if action_status == DoctorStatus::Ready {
                "WezTerm supports visible launch and pane activation through `wezterm cli`."
            } else {
                "WezTerm support needs a reachable mux server from this shell."
            };
            let fix = Some(
                "Start claudectl from the same WezTerm environment that owns the Claude panes."
                    .to_string(),
            );

            for action in [TerminalAction::Launch, TerminalAction::Switch] {
                actions.push(action_check(action, action_status, detail, fix.clone()));
            }
            for action in [TerminalAction::Input, TerminalAction::Approve] {
                actions.push(action_check(
                    action,
                    DoctorStatus::Unsupported,
                    "WezTerm integration currently supports launch and pane focus only.",
                    None::<String>,
                ));
            }
            notes.push("WezTerm input injection is not implemented yet.".to_string());
        }
        #[cfg(target_os = "macos")]
        Terminal::Ghostty => {
            let apple_script_check = binary_check("osascript");
            let apple_script_ready = apple_script_check.status == DoctorStatus::Ready;
            prerequisites.push(apple_script_check);

            let detail = if apple_script_ready {
                "Ghostty exposes switch/input/approve through its AppleScript API."
            } else {
                "Ghostty support requires `osascript`."
            };
            let status = if apple_script_ready {
                DoctorStatus::Ready
            } else {
                DoctorStatus::Blocked
            };
            let fix = Some(
                "Ensure macOS automation tools are available and Ghostty is running normally."
                    .to_string(),
            );

            for action in supported_actions(&Terminal::Ghostty) {
                actions.push(action_check(action, status, detail, fix.clone()));
            }
            actions.push(action_check(
                TerminalAction::Launch,
                DoctorStatus::Unsupported,
                "Visible launch is only implemented for tmux, Kitty, and WezTerm.",
                None::<String>,
            ));
            notes.push("Ghostty does not need Kitty-style remote control setup, but macOS may still prompt for automation access.".to_string());
        }
        #[cfg(target_os = "macos")]
        Terminal::Warp | Terminal::ITerm2 | Terminal::Apple => {
            let apple_script_check = binary_check("osascript");
            let apple_script_ready = apple_script_check.status == DoctorStatus::Ready;
            prerequisites.push(apple_script_check);

            let system_events_check = if apple_script_ready {
                match probe_system_events_access() {
                    Ok(()) => DoctorCheck::ready(
                        "System Events access",
                        "AppleScript can talk to System Events from this shell.",
                    ),
                    Err(err) => DoctorCheck::blocked(
                        "System Events access",
                        format!("macOS UI scripting is not currently available: {err}"),
                        Some(
                            "Grant Automation/Accessibility access in System Settings > Privacy & Security, then rerun `claudectl --doctor`."
                                .to_string(),
                        ),
                    ),
                }
            } else {
                DoctorCheck::blocked(
                    "System Events access",
                    "`osascript` is unavailable, so macOS UI scripting cannot run.",
                    Some(
                        "Ensure `/usr/bin/osascript` is available and rerun the doctor."
                            .to_string(),
                    ),
                )
            };
            let system_events_ready = system_events_check.status == DoctorStatus::Ready;
            prerequisites.push(system_events_check);

            actions.push(action_check(
                TerminalAction::Launch,
                DoctorStatus::Unsupported,
                "Visible launch is only implemented for tmux, Kitty, and WezTerm.",
                None::<String>,
            ));

            let status = if apple_script_ready && system_events_ready {
                DoctorStatus::Ready
            } else {
                DoctorStatus::Blocked
            };
            let detail = format!(
                "{} uses AppleScript and System Events for focus and input control.",
                terminal_name(&terminal)
            );
            let fix = Some(
                "Grant Automation/Accessibility permissions to the terminal and rerun `claudectl --doctor`."
                    .to_string(),
            );
            for action in [
                TerminalAction::Switch,
                TerminalAction::Input,
                TerminalAction::Approve,
            ] {
                actions.push(action_check(action, status, &detail, fix.clone()));
            }
        }
        Terminal::Unknown(name) => {
            for action in [
                TerminalAction::Launch,
                TerminalAction::Switch,
                TerminalAction::Input,
                TerminalAction::Approve,
            ] {
                actions.push(action_check(
                    action,
                    DoctorStatus::Unsupported,
                    format!(
                        "No integration is configured for `{name}`. Supported terminals: GNOME Terminal, Windows Terminal on WSL, tmux, Kitty, WezTerm, Ghostty, Warp, iTerm2, Terminal.app."
                    ),
                    None::<String>,
                ));
            }
            notes.push(
                "Monitoring still works in unsupported terminals, but control actions stay manual."
                    .to_string(),
            );
        }
        #[cfg(not(target_os = "macos"))]
        Terminal::Ghostty | Terminal::Warp | Terminal::ITerm2 | Terminal::Apple => {
            for action in [
                TerminalAction::Launch,
                TerminalAction::Switch,
                TerminalAction::Input,
                TerminalAction::Approve,
            ] {
                actions.push(action_check(
                    action,
                    DoctorStatus::Unsupported,
                    format!(
                        "{} control hooks are currently only implemented on macOS.",
                        terminal_name(&terminal)
                    ),
                    None::<String>,
                ));
            }
            notes.push(
                "Monitoring still works in unsupported terminals, but control actions stay manual."
                    .to_string(),
            );
        }
    }

    if !command_ready("claude") {
        notes.push("Launching a new session will fail until `claude` is on PATH.".to_string());
    }

    DoctorReport {
        terminal: terminal_label,
        platform: platform_name(),
        actions,
        prerequisites,
        notes,
    }
}

pub fn format_doctor_report(report: &DoctorReport) -> String {
    let mut lines = vec![
        "claudectl doctor".to_string(),
        String::new(),
        format!("Platform: {}", report.platform),
        format!("Detected terminal: {}", report.terminal),
        String::new(),
        "Prerequisites".to_string(),
    ];

    for check in &report.prerequisites {
        lines.push(format!(
            "  [{}] {}: {}",
            check.status.label(),
            check.name,
            check.detail
        ));
        if let Some(fix) = &check.fix {
            lines.push(format!("      fix: {fix}"));
        }
    }

    lines.push(String::new());
    lines.push("Capabilities".to_string());
    for action in &report.actions {
        lines.push(format!(
            "  [{}] {}: {}",
            action.status.label(),
            action.name,
            action.detail
        ));
        if let Some(fix) = &action.fix {
            lines.push(format!("      fix: {fix}"));
        }
    }

    if !report.notes.is_empty() {
        lines.push(String::new());
        lines.push("Notes".to_string());
        for note in &report.notes {
            lines.push(format!("  - {note}"));
        }
    }

    lines.join("\n")
}

pub fn launch_session(
    cwd: &str,
    prompt: Option<&str>,
    resume: Option<&str>,
) -> Result<String, String> {
    let terminal = detect_terminal();
    match terminal {
        Terminal::Gnome => gnome_terminal::launch(cwd, prompt, resume),
        Terminal::Kitty => kitty::launch(cwd, prompt, resume),
        Terminal::Tmux => tmux::launch(cwd, prompt, resume),
        Terminal::WezTerm => wezterm::launch(cwd, prompt, resume),
        Terminal::WindowsTerm => windows_terminal::launch(cwd, prompt, resume),
        other => Err(format!(
            "Visible session launch is not supported in {}. Start `claude` manually, use tmux/Kitty/WezTerm/GNOME Terminal/Windows Terminal on WSL, or run `claudectl --doctor` for setup guidance.",
            terminal_name(&other)
        )),
    }
}

pub fn switch_to_terminal(session: &ClaudeSession) -> Result<(), String> {
    let terminal = detect_terminal();

    // Only require a TTY for terminals that match sessions by TTY name.
    // Kitty, Ghostty, and Warp use their own IPC (PID/cwd matching) and don't need it.
    let needs_tty = matches!(
        terminal,
        Terminal::Tmux | Terminal::WezTerm | Terminal::Apple | Terminal::ITerm2
    );
    if needs_tty && session.tty.is_empty() {
        return Err("No TTY associated with this session".into());
    }
    crate::logger::log(
        "DEBUG",
        &format!(
            "terminal switch: {} (tty={}) via {:?}",
            session.display_name(),
            session.tty,
            terminal_name(&terminal)
        ),
    );

    match terminal {
        Terminal::Gnome => gnome_terminal::switch(session),
        Terminal::Kitty => kitty::switch(session),
        Terminal::WezTerm => wezterm::switch(session),
        Terminal::Tmux => tmux::switch(session),
        Terminal::WindowsTerm => Err(
            "Windows Terminal currently supports WSL launch only. Use tmux or Kitty inside WSL for session switching."
                .into(),
        ),
        #[cfg(target_os = "macos")]
        Terminal::Ghostty => ghostty::switch(session),
        #[cfg(target_os = "macos")]
        Terminal::Warp => warp::switch(session),
        #[cfg(target_os = "macos")]
        Terminal::ITerm2 => iterm2::switch(session),
        #[cfg(target_os = "macos")]
        Terminal::Apple => apple::switch(session),
        Terminal::Unknown(name) => Err(format!(
            "Unsupported terminal: {name}. Supported: GNOME Terminal, Windows Terminal on WSL (launch only), Ghostty, Warp, iTerm2, Kitty, WezTerm, Terminal.app, tmux. Run `claudectl --doctor` for details."
        )),
        #[cfg(not(target_os = "macos"))]
        _ => Err("Terminal switching not supported on this platform. Run `claudectl --doctor` for details.".into()),
    }
}

pub fn send_input(session: &ClaudeSession, text: &str) -> Result<(), String> {
    match detect_terminal() {
        Terminal::Gnome => gnome_terminal::send_input(session, text),
        #[cfg(target_os = "macos")]
        Terminal::Ghostty => ghostty::send_input(session, text),
        Terminal::Kitty => kitty::send_input(session, text),
        Terminal::Tmux => tmux::send_input(session, text),
        Terminal::WindowsTerm => Err(
            "Windows Terminal currently supports WSL launch only. Use tmux or Kitty inside WSL for session input automation."
                .into(),
        ),
        #[cfg(target_os = "macos")]
        Terminal::Warp => warp::send_input(session, text),
        #[cfg(target_os = "macos")]
        _ => {
            // iTerm2, Apple Terminal, etc: switch + System Events keystroke
            switch_to_terminal(session)?;
            std::thread::sleep(std::time::Duration::from_millis(300));
            let escaped = text.replace('\\', "\\\\").replace('"', "\\\"");
            run_osascript(&format!(
                r#"tell application "System Events" to keystroke "{escaped}""#,
            ))
        }
        #[cfg(not(target_os = "macos"))]
        _ => Err("Input injection not supported for this terminal. Run `claudectl --doctor` for details.".into()),
    }
}

pub fn approve_session(session: &ClaudeSession) -> Result<(), String> {
    match detect_terminal() {
        Terminal::Gnome => gnome_terminal::approve(session),
        #[cfg(target_os = "macos")]
        Terminal::Ghostty => ghostty::approve(session),
        Terminal::Kitty => kitty::approve(session),
        Terminal::Tmux => tmux::send_input(session, "\r"),
        Terminal::WindowsTerm => Err(
            "Windows Terminal currently supports WSL launch only. Use tmux or Kitty inside WSL for approval automation."
                .into(),
        ),
        #[cfg(target_os = "macos")]
        Terminal::Warp => warp::approve(session),
        #[cfg(target_os = "macos")]
        _ => {
            // iTerm2, Apple Terminal, etc: switch + press Enter
            switch_to_terminal(session)?;
            std::thread::sleep(std::time::Duration::from_millis(300));
            run_osascript(r#"tell application "System Events" to key code 36"#)
        }
        #[cfg(not(target_os = "macos"))]
        _ => Err("Input injection not supported for this terminal. Run `claudectl --doctor` for details.".into()),
    }
}

#[cfg(target_os = "macos")]
pub fn run_osascript(script: &str) -> Result<(), String> {
    let output = std::process::Command::new("osascript")
        .args(["-e", script])
        .output()
        .map_err(|e| format!("Failed to run osascript: {e}"))?;

    if output.status.success() {
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        Err(format!("AppleScript error: {}", stderr.trim()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn help_summary_lists_kitty_actions() {
        let summary = help_capability_summary_for(&Terminal::Kitty);
        assert_eq!(
            summary,
            "Current terminal: Kitty (launch, switch, input, approve)"
        );
    }

    #[test]
    fn help_summary_marks_unknown_terminal_monitor_only() {
        let summary = help_capability_summary_for(&Terminal::Unknown("foot".into()));
        assert_eq!(summary, "Current terminal: foot (monitor-only)");
    }

    #[test]
    fn help_summary_mentions_gnome_terminal() {
        let summary = help_capability_summary_for(&Terminal::Gnome);
        assert!(summary.starts_with("Current terminal: GNOME Terminal"));
    }

    #[test]
    fn help_summary_lists_windows_terminal_launch() {
        let summary = help_capability_summary_for(&Terminal::WindowsTerm);
        assert_eq!(summary, "Current terminal: Windows Terminal (launch)");
    }

    #[test]
    fn doctor_report_for_unknown_terminal_marks_actions_unsupported() {
        let report = doctor_report_for(Terminal::Unknown("foot".into()));
        assert_eq!(report.actions.len(), 4);
        assert!(
            report
                .actions
                .iter()
                .all(|action| action.status == DoctorStatus::Unsupported)
        );
    }

    #[test]
    fn platform_label_marks_wsl_explicitly() {
        assert_eq!(platform_label("linux", true), "linux (WSL)");
        assert_eq!(platform_label("macos", false), "macos");
    }

    #[test]
    fn environment_notes_describe_wsl_interop_state() {
        let notes = environment_notes(true, true);
        assert!(notes.iter().any(|note| note.contains("WSL detected")));
        assert!(notes.iter().any(|note| note.contains("cmd.exe /c wt.exe")));
    }

    #[test]
    fn wsl_interop_check_reports_when_available() {
        let check = wsl_interop_check(true).unwrap();
        assert_eq!(check.name, "Windows Terminal interop");
    }

    // Native env var detection tests.
    // These mutate env vars and must be serialized.
    use std::sync::Mutex;
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    /// Helper: clear all terminal-related env vars, run f(), then restore.
    fn with_clean_env<F: FnOnce() -> R, R>(f: F) -> R {
        let _guard = ENV_LOCK.lock().unwrap();

        let keys = [
            "KITTY_WINDOW_ID",
            "KITTY_PID",
            "WEZTERM_EXECUTABLE",
            "GHOSTTY_RESOURCES_DIR",
            "TERM",
            "TERM_PROGRAM",
            "TMUX",
            "GNOME_TERMINAL_SERVICE",
            "GNOME_TERMINAL_SCREEN",
            "WT_SESSION",
        ];
        let saved: Vec<(&str, Option<String>)> =
            keys.iter().map(|k| (*k, std::env::var(k).ok())).collect();

        for key in &keys {
            unsafe { std::env::remove_var(key) };
        }

        let result = f();

        for (key, val) in saved {
            match val {
                Some(v) => unsafe { std::env::set_var(key, v) },
                None => unsafe { std::env::remove_var(key) },
            }
        }

        result
    }

    #[test]
    fn detect_kitty_via_kitty_window_id() {
        with_clean_env(|| {
            unsafe { std::env::set_var("KITTY_WINDOW_ID", "49") };
            assert_eq!(detect_by_native_env(), Some(Terminal::Kitty));
        });
    }

    #[test]
    fn detect_kitty_via_term_xterm_kitty() {
        with_clean_env(|| {
            unsafe { std::env::set_var("TERM", "xterm-kitty") };
            assert_eq!(detect_by_native_env(), Some(Terminal::Kitty));
        });
    }

    #[test]
    fn detect_wezterm_via_wezterm_executable() {
        with_clean_env(|| {
            unsafe { std::env::set_var("WEZTERM_EXECUTABLE", "/usr/bin/wezterm") };
            assert_eq!(detect_by_native_env(), Some(Terminal::WezTerm));
        });
    }

    #[test]
    fn detect_ghostty_via_ghostty_resources_dir() {
        with_clean_env(|| {
            unsafe { std::env::set_var("GHOSTTY_RESOURCES_DIR", "/usr/share/ghostty") };
            assert_eq!(detect_by_native_env(), Some(Terminal::Ghostty));
        });
    }

    #[test]
    fn detect_native_env_returns_none_when_clean() {
        with_clean_env(|| {
            assert_eq!(detect_by_native_env(), None);
        });
    }

    #[test]
    fn kitty_window_id_takes_priority_over_term_xterm_kitty() {
        // Both set — KITTY_WINDOW_ID should match first (stronger signal)
        with_clean_env(|| {
            unsafe {
                std::env::set_var("KITTY_WINDOW_ID", "1");
                std::env::set_var("TERM", "xterm-kitty");
            }
            assert_eq!(detect_by_native_env(), Some(Terminal::Kitty));
        });
    }
}
