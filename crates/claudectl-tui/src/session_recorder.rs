use std::fs::File;
use std::io::{BufRead, BufReader, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use claudectl_core::transcript::{TranscriptBlock, TranscriptEvent, TranscriptRole, parse_line};

/// Default lookback buffer: capture recent events before record start (~30s of activity).
const DEFAULT_LOOKBACK_BYTES: u64 = 50_000;
/// Maximum characters of bash output to include in a frame.
const MAX_BASH_OUTPUT: usize = 800;
/// Maximum characters of assistant text to include.
const MAX_ASSISTANT_TEXT: usize = 200;
/// Maximum lines of diff to show for Edit events.
const MAX_DIFF_LINES: usize = 15;
/// Seconds between frames in the highlight reel.
const FRAME_PACE: f64 = 1.2;
/// Seconds to hold tool results before next event.
const RESULT_HOLD: f64 = 2.0;
/// Seconds to hold the title card.
const TITLE_HOLD: f64 = 3.0;

/// Records a single Claude Code session as a highlight reel.
pub struct SessionRecorder {
    jsonl_path: PathBuf,
    offset: u64,
    cast_file: File,
    cast_path: PathBuf,
    final_path: PathBuf,
    is_gif: bool,
    virtual_time: f64, // Synthetic clock for paced playback
    #[allow(dead_code)]
    width: u16,
    #[allow(dead_code)]
    height: u16,
    title_written: bool,
    session_name: String,
    // Running tally for the header
    edits: u32,
    commands: u32,
    errors: u32,
}

/// A parsed event from the JSONL stream.
enum SessionEvent {
    AssistantText(String),
    ToolUse {
        tool: String,
        summary: String,
        diff: Option<String>, // For Edit: the new_string content (abbreviated)
    },
    ToolResult {
        output: String,
        is_error: bool,
    },
}

impl SessionRecorder {
    pub fn new(
        jsonl_path: &Path,
        output_path: &str,
        session_name: &str,
        width: u16,
        height: u16,
    ) -> std::io::Result<Self> {
        let is_gif = output_path.ends_with(".gif");
        let final_path = PathBuf::from(output_path);

        let cast_path = if is_gif {
            let mut tmp = std::env::temp_dir();
            tmp.push(format!("claudectl-sess-{}.cast", std::process::id()));
            tmp
        } else {
            final_path.clone()
        };

        let mut cast_file = File::create(&cast_path)?;

        // Write asciicast v2 header
        let header = serde_json::json!({
            "version": 2,
            "width": width,
            "height": height,
            "timestamp": std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
            "title": format!("claudectl: {session_name}"),
            "env": {
                "SHELL": std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".into()),
                "TERM": "xterm-256color"
            }
        });
        writeln!(cast_file, "{}", header)?;

        // Start with a lookback buffer to capture recent events before record-start.
        // Seek back DEFAULT_LOOKBACK_BYTES, then scan forward to the next newline
        // to avoid starting mid-line in the JSONL stream.
        let file_len = std::fs::metadata(jsonl_path).map(|m| m.len()).unwrap_or(0);
        let initial_offset = if file_len > DEFAULT_LOOKBACK_BYTES {
            let raw_offset = file_len - DEFAULT_LOOKBACK_BYTES;
            // Find next newline after raw_offset to align to a line boundary
            if let Ok(f) = File::open(jsonl_path) {
                let mut reader = BufReader::new(f);
                if reader.seek(SeekFrom::Start(raw_offset)).is_ok() {
                    let mut discard = String::new();
                    if reader.read_line(&mut discard).is_ok() {
                        reader.stream_position().unwrap_or(raw_offset)
                    } else {
                        raw_offset
                    }
                } else {
                    raw_offset
                }
            } else {
                raw_offset
            }
        } else {
            0 // File is smaller than lookback — include everything
        };

        Ok(Self {
            jsonl_path: jsonl_path.to_path_buf(),
            offset: initial_offset,
            cast_file,
            cast_path,
            final_path,
            is_gif,
            virtual_time: 0.0,
            width,
            height,
            title_written: false,
            session_name: session_name.to_string(),
            edits: 0,
            commands: 0,
            errors: 0,
        })
    }

    /// Read new JSONL lines and emit highlight frames.
    pub fn poll(&mut self) -> std::io::Result<bool> {
        let mut file = match File::open(&self.jsonl_path) {
            Ok(f) => f,
            Err(_) => return Ok(false),
        };

        let file_len = file.metadata().map(|m| m.len()).unwrap_or(0);
        if self.offset >= file_len {
            return Ok(false);
        }

        if self.offset > 0 {
            file.seek(SeekFrom::Start(self.offset))?;
        }

        // Write title card on first poll
        if !self.title_written {
            self.write_title_card()?;
            self.title_written = true;
        }

        let reader = BufReader::new(&file);
        let mut had_events = false;

        for line in reader.lines() {
            let line = match line {
                Ok(l) => l,
                Err(_) => break,
            };

            for event in parse_events(&line) {
                if self.emit_highlight(&event)? {
                    had_events = true;
                }
            }
        }

        self.offset = file_len;
        Ok(had_events)
    }

    fn write_frame(&mut self, data: &str) -> std::io::Result<()> {
        let event = serde_json::json!([self.virtual_time, "o", data]);
        writeln!(self.cast_file, "{}", event)
    }

    fn write_title_card(&mut self) -> std::io::Result<()> {
        let name = &self.session_name;
        // Compact title card matching Claude Code's style
        let card = format!(
            "\x1b[2J\x1b[H\r\n\
             \x1b[1;37m  ╭─ {name} ─────────────────────────────╮\x1b[0m\r\n\
             \x1b[1;37m  │                                       │\x1b[0m\r\n\
             \x1b[1;37m  │\x1b[0m  \x1b[36mSession recording\x1b[0m\x1b[1;37m                │\x1b[0m\r\n\
             \x1b[1;37m  │\x1b[0m  \x1b[90mRecorded with claudectl\x1b[0m\x1b[1;37m          │\x1b[0m\r\n\
             \x1b[1;37m  │                                       │\x1b[0m\r\n\
             \x1b[1;37m  ╰───────────────────────────────────────╯\x1b[0m\r\n\r\n"
        );
        self.write_frame(&card)?;
        self.virtual_time += TITLE_HOLD;
        Ok(())
    }

    fn write_separator(&mut self) -> std::io::Result<()> {
        // Thin separator between event groups, no screen clear
        self.write_frame("\r\n")?;
        self.virtual_time += 0.2;
        Ok(())
    }

    /// Emit a frame only for highlight-worthy events. Returns true if emitted.
    fn emit_highlight(&mut self, event: &SessionEvent) -> std::io::Result<bool> {
        match event {
            SessionEvent::AssistantText(text) => {
                // Only show brief planning statements, not verbose explanations
                if text.len() < 30 || text.contains("```") {
                    return Ok(false);
                }
                let truncated = if text.len() > MAX_ASSISTANT_TEXT {
                    format!("{}...", truncate_str(text, MAX_ASSISTANT_TEXT))
                } else {
                    text.clone()
                };
                // Claude Code style: bullet point with text
                self.write_separator()?;
                let frame = format!(
                    "  \x1b[36m●\x1b[0m \x1b[1;37m{}\x1b[0m\r\n\r\n",
                    truncated.replace('\n', "\r\n    ")
                );
                self.write_frame(&frame)?;
                self.virtual_time += FRAME_PACE;
                Ok(true)
            }
            SessionEvent::ToolUse {
                tool,
                summary,
                diff,
            } => {
                // Update tally
                match tool.as_str() {
                    "Edit" | "Write" | "NotebookEdit" => self.edits += 1,
                    "Bash" => self.commands += 1,
                    "Read" | "Grep" | "Glob" => {
                        let frame = format!("  \x1b[90m│ {tool}({summary})\x1b[0m\r\n");
                        self.write_frame(&frame)?;
                        self.virtual_time += 0.4;
                        return Ok(true);
                    }
                    _ => {}
                }

                self.write_separator()?;

                // Claude Code style rendering per tool type
                match tool.as_str() {
                    "Edit" | "Write" | "NotebookEdit" => {
                        let action = if tool == "Write" { "Create" } else { "Update" };
                        let mut frame =
                            format!("  \x1b[32m●\x1b[0m \x1b[1;37m{action}({summary})\x1b[0m\r\n");
                        if let Some(diff_content) = diff {
                            let old_count =
                                diff_content.lines().filter(|l| l.starts_with('-')).count();
                            let new_count =
                                diff_content.lines().filter(|l| l.starts_with('+')).count();
                            frame.push_str(&format!(
                                "    \x1b[90mAdded \x1b[1;37m{new_count}\x1b[0m\x1b[90m lines, removed \x1b[1;37m{old_count}\x1b[0m\x1b[90m lines\x1b[0m\r\n"
                            ));
                            for (line_num, line) in
                                (1u32..).zip(diff_content.lines().take(MAX_DIFF_LINES))
                            {
                                let colored = if let Some(content) = line.strip_prefix('+') {
                                    format!("    \x1b[42;30m{line_num:>3} +{content}\x1b[0m\r\n")
                                } else if let Some(content) = line.strip_prefix('-') {
                                    format!("    \x1b[41;37m{line_num:>3} -{content}\x1b[0m\r\n")
                                } else {
                                    format!("    \x1b[90m{line_num:>3}  {line}\x1b[0m\r\n")
                                };
                                frame.push_str(&colored);
                            }
                            let total = diff_content.lines().count();
                            if total > MAX_DIFF_LINES {
                                frame.push_str(&format!(
                                    "    \x1b[90m... +{} more lines\x1b[0m\r\n",
                                    total - MAX_DIFF_LINES
                                ));
                            }
                        }
                        frame.push_str("\r\n");
                        self.write_frame(&frame)?;
                    }
                    "Bash" => {
                        let frame = format!(
                            "  \x1b[33m●\x1b[0m \x1b[90mRunning\x1b[0m \x1b[1;37m1\x1b[0m \x1b[90mbash command...\x1b[0m\r\n\
                             \x1b[90m    └\x1b[0m \x1b[37m$ {summary}\x1b[0m\r\n\r\n"
                        );
                        self.write_frame(&frame)?;
                    }
                    "Agent" => {
                        let frame = format!(
                            "  \x1b[35m●\x1b[0m \x1b[1;37mAgent\x1b[0m \x1b[90m{summary}\x1b[0m\r\n\r\n"
                        );
                        self.write_frame(&frame)?;
                    }
                    _ => {
                        let frame = format!(
                            "  \x1b[36m●\x1b[0m \x1b[37m{tool}\x1b[0m \x1b[90m{summary}\x1b[0m\r\n\r\n"
                        );
                        self.write_frame(&frame)?;
                    }
                }
                self.virtual_time += FRAME_PACE;
                Ok(true)
            }
            SessionEvent::ToolResult { output, is_error } => {
                if output.is_empty() {
                    return Ok(false);
                }

                if *is_error {
                    self.errors += 1;
                }

                let truncated = if output.len() > MAX_BASH_OUTPUT {
                    format!("{}...", truncate_str(output, MAX_BASH_OUTPUT))
                } else {
                    output.clone()
                };
                let display = truncated.replace('\n', "\r\n    ");
                let frame = if *is_error {
                    format!("    \x1b[1;31m✗ Error:\x1b[0m\r\n    \x1b[31m{display}\x1b[0m\r\n\r\n")
                } else {
                    format!("    \x1b[90m{display}\x1b[0m\r\n\r\n")
                };
                self.write_frame(&frame)?;
                self.virtual_time += RESULT_HOLD;
                Ok(true)
            }
        }
    }

    pub fn finish(&mut self) -> std::io::Result<()> {
        let error_str = if self.errors > 0 {
            format!("  \x1b[31m{} errors\x1b[0m", self.errors)
        } else {
            String::new()
        };
        let summary = format!(
            "\r\n\r\n\
             \x1b[1;37m  ╭─ {} ── complete ──────────────────────╮\x1b[0m\r\n\
             \x1b[1;37m  │                                       │\x1b[0m\r\n\
             \x1b[1;37m  │\x1b[0m  \x1b[32m{} edits\x1b[0m  \x1b[33m{} commands\x1b[0m{error_str}\x1b[1;37m        │\x1b[0m\r\n\
             \x1b[1;37m  │                                       │\x1b[0m\r\n\
             \x1b[1;37m  │\x1b[0m  \x1b[90mclaudectl\x1b[0m\x1b[1;37m                            │\x1b[0m\r\n\
             \x1b[1;37m  ╰───────────────────────────────────────╯\x1b[0m\r\n",
            self.session_name, self.edits, self.commands
        );
        self.write_frame(&summary)?;
        self.virtual_time += TITLE_HOLD;

        self.cast_file.flush()?;

        if self.is_gif {
            return self.convert_to_gif();
        }
        Ok(())
    }

    fn convert_to_gif(&self) -> std::io::Result<()> {
        let cast = self.cast_path.clone();
        let gif = self.final_path.clone();

        // Check if agg exists before spawning
        let has_agg = std::process::Command::new("which")
            .arg("agg")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);

        if !has_agg {
            let fallback = self.final_path.with_extension("cast");
            if self.cast_path != fallback {
                std::fs::rename(&self.cast_path, &fallback)?;
            }
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!(
                    "agg not found — install with: cargo install agg\n\
                     Saved asciicast to {}",
                    fallback.display()
                ),
            ));
        }

        // Spawn agg in the background — don't block the TUI
        std::thread::spawn(move || {
            let result = std::process::Command::new("agg")
                .args([
                    cast.to_string_lossy().as_ref(),
                    gif.to_string_lossy().as_ref(),
                ])
                .output();

            match result {
                Ok(output) if output.status.success() => {
                    let _ = std::fs::remove_file(&cast);
                }
                _ => {
                    // Keep the .cast file as fallback
                    let fallback = gif.with_extension("cast");
                    if cast != fallback {
                        let _ = std::fs::rename(&cast, &fallback);
                    }
                }
            }
        });

        Ok(())
    }
}

/// Parse a JSONL line into zero or more session events.
fn parse_events(line: &str) -> Vec<SessionEvent> {
    let mut events = Vec::new();

    let Some(event) = parse_line(line) else {
        return events;
    };

    let TranscriptEvent::Message(message) = event else {
        return events;
    };

    for block in message.content {
        match block {
            TranscriptBlock::Text(text) if message.role == TranscriptRole::Assistant => {
                let trimmed = text.trim();
                if !trimmed.is_empty() && trimmed.len() > 20 {
                    events.push(SessionEvent::AssistantText(trimmed.to_string()));
                }
            }
            TranscriptBlock::ToolUse { name, input }
                if message.role == TranscriptRole::Assistant =>
            {
                let summary = summarize_tool_use(&name, Some(&input));
                let diff = extract_diff(&name, Some(&input));
                events.push(SessionEvent::ToolUse {
                    tool: name,
                    summary,
                    diff,
                });
            }
            TranscriptBlock::ToolResult { content, is_error }
                if message.role == TranscriptRole::User && !content.is_empty() =>
            {
                events.push(SessionEvent::ToolResult {
                    output: content,
                    is_error,
                });
            }
            _ => {}
        }
    }

    events
}

/// Produce a human-readable summary of a tool use invocation.
fn summarize_tool_use(tool: &str, input: Option<&serde_json::Value>) -> String {
    let input = match input {
        Some(v) => v,
        None => return String::new(),
    };

    match tool {
        "Edit" => {
            let file = input
                .get("file_path")
                .and_then(|p| p.as_str())
                .unwrap_or("?");
            let short = shorten_path(file);
            let old_len = input
                .get("old_string")
                .and_then(|s| s.as_str())
                .map(|s| s.len())
                .unwrap_or(0);
            let new_len = input
                .get("new_string")
                .and_then(|s| s.as_str())
                .map(|s| s.len())
                .unwrap_or(0);
            format!("{short}  ({old_len} → {new_len} chars)")
        }
        "Write" => {
            let file = input
                .get("file_path")
                .and_then(|p| p.as_str())
                .unwrap_or("?");
            let short = shorten_path(file);
            let content_len = input
                .get("content")
                .and_then(|s| s.as_str())
                .map(|s| s.len())
                .unwrap_or(0);
            format!("{short}  ({content_len} chars)")
        }
        "Bash" => {
            let cmd = input.get("command").and_then(|c| c.as_str()).unwrap_or("?");
            if cmd.len() > 80 {
                format!("{}...", truncate_str(cmd, 77))
            } else {
                cmd.to_string()
            }
        }
        "Read" => {
            let file = input
                .get("file_path")
                .and_then(|p| p.as_str())
                .unwrap_or("?");
            shorten_path(file)
        }
        "Grep" => {
            let pattern = input.get("pattern").and_then(|p| p.as_str()).unwrap_or("?");
            format!("/{pattern}/")
        }
        "Glob" => {
            let pattern = input.get("pattern").and_then(|p| p.as_str()).unwrap_or("?");
            pattern.to_string()
        }
        _ => String::new(),
    }
}

/// Truncate a string at a char boundary, never splitting a multi-byte character.
fn truncate_str(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// Extract a simple diff representation for Edit tool events.
fn extract_diff(tool: &str, input: Option<&serde_json::Value>) -> Option<String> {
    let input = input?;
    match tool {
        "Edit" => {
            let old = input
                .get("old_string")
                .and_then(|s| s.as_str())
                .unwrap_or("");
            let new = input
                .get("new_string")
                .and_then(|s| s.as_str())
                .unwrap_or("");
            if old.is_empty() && new.is_empty() {
                return None;
            }
            let mut diff = String::new();
            for line in old.lines() {
                diff.push_str(&format!("-{line}\n"));
            }
            for line in new.lines() {
                diff.push_str(&format!("+{line}\n"));
            }
            Some(diff)
        }
        "Write" => {
            // Show first few lines of the new file content
            let content = input.get("content").and_then(|s| s.as_str())?;
            let preview: String = content
                .lines()
                .take(MAX_DIFF_LINES)
                .map(|l| format!("+{l}\n"))
                .collect();
            Some(preview)
        }
        _ => None,
    }
}

fn shorten_path(path: &str) -> String {
    let parts: Vec<&str> = path.rsplit('/').take(2).collect();
    match parts.len() {
        2 => format!("{}/{}", parts[1], parts[0]),
        1 => parts[0].to_string(),
        _ => path.to_string(),
    }
}
