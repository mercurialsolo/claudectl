# Terminal Support

Run `claudectl doctor` to check your terminal's capabilities along with the rest of the install (PATH, hooks, plugin files, brain endpoint, bus, session discovery). The legacy `claudectl --doctor` flag still works for the terminal-only report.

## Compatibility Matrix

| Terminal | Launch (`--new` / `n`) | Switch | Input | Approve | Notes |
|----------|-------------------------|--------|-------|---------|-------|
| **GNOME Terminal** | Yes | - | - | - | Visible launch via `gnome-terminal --window` on Linux |
| **Ghostty** | - | Yes | Yes | Yes | Native AppleScript API, no Kitty-style remote control setup |
| **Kitty** | Yes | Yes | Yes | Yes | `kitty @` remote control |
| **tmux** | Yes | Yes | Yes | Yes | `tmux` pane/window control |
| **WezTerm** | Yes | Yes | - | - | `wezterm cli` launch + pane activation |
| **Windows Terminal (WSL)** | Yes | - | - | - | Visible launch via `cmd.exe /c wt.exe` into a new WSL tab |
| **Warp** | - | Yes | Yes | Yes | Command Palette + System Events |
| **iTerm2** | - | Yes | Yes | Yes | AppleScript + System Events |
| **Terminal.app** | - | Yes | Yes | Yes | AppleScript + System Events |

## Setup Notes

- **GNOME Terminal**: Launch support is verified on Linux under Docker/X11. Remote switch/input/approve automation is intentionally unsupported.
- **Windows Terminal**: WSL-only, currently covers visible launch, not remote tab control.
- **Kitty**: Requires `allow_remote_control yes` in `~/.config/kitty/kitty.conf`.
- **Warp, iTerm2, Terminal.app**: Require macOS Automation/Accessibility permission in System Settings > Privacy & Security.
- **tmux**: Assumes claudectl can reach the same tmux server as the Claude panes.
