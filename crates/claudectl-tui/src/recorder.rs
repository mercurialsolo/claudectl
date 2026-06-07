use std::fs::File;
use std::io::{self, Write};
use std::path::PathBuf;
use std::time::Instant;

/// Output format detected from file extension.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum OutputFormat {
    Asciicast, // .cast
    Gif,       // .gif — records as .cast then converts via agg
}

impl OutputFormat {
    pub fn from_path(path: &str) -> Self {
        if path.ends_with(".gif") {
            Self::Gif
        } else {
            Self::Asciicast
        }
    }
}

/// Records terminal output in asciicast v2 format by teeing all writes.
/// See: https://docs.asciinema.org/manual/asciicast/v2/
pub struct Recorder {
    file: File,
    start: Instant,
    buf: Vec<u8>,
    cast_path: PathBuf,  // Where the .cast file lives
    final_path: PathBuf, // What the user asked for
    format: OutputFormat,
}

impl Recorder {
    /// Create a new recorder. If path ends in .gif, records to a temp .cast
    /// and converts on finish.
    pub fn new(path: &str, width: u16, height: u16) -> io::Result<Self> {
        let format = OutputFormat::from_path(path);
        let final_path = PathBuf::from(path);

        // For GIF output, write the .cast to a temp file
        let cast_path = match format {
            OutputFormat::Gif => {
                let mut tmp = std::env::temp_dir();
                tmp.push(format!("claudectl-{}.cast", std::process::id()));
                tmp
            }
            OutputFormat::Asciicast => final_path.clone(),
        };

        let mut file = File::create(&cast_path)?;

        let header = serde_json::json!({
            "version": 2,
            "width": width,
            "height": height,
            "timestamp": std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
            "env": {
                "SHELL": std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".into()),
                "TERM": std::env::var("TERM").unwrap_or_else(|_| "xterm-256color".into())
            },
            "title": "claudectl"
        });
        writeln!(file, "{}", header)?;

        Ok(Self {
            file,
            start: Instant::now(),
            buf: Vec::with_capacity(8192),
            cast_path,
            final_path,
            format,
        })
    }

    /// Accumulate bytes (called from TeeWriter::write).
    pub fn capture(&mut self, data: &[u8]) {
        self.buf.extend_from_slice(data);
    }

    /// Flush accumulated bytes as a single asciicast event.
    pub fn flush_frame(&mut self) -> io::Result<()> {
        if self.buf.is_empty() {
            return Ok(());
        }
        let elapsed = self.start.elapsed().as_secs_f64();
        let data = String::from_utf8_lossy(&self.buf);
        let event = serde_json::json!([elapsed, "o", data]);
        writeln!(self.file, "{}", event)?;
        self.buf.clear();
        Ok(())
    }

    /// Finish recording. For GIF output, converts the .cast file via `agg`.
    pub fn finish(&mut self) -> io::Result<()> {
        self.flush_frame()?;
        self.file.flush()?;

        if self.format == OutputFormat::Gif {
            return self.convert_to_gif();
        }
        Ok(())
    }

    fn convert_to_gif(&self) -> io::Result<()> {
        let cast = self.cast_path.to_string_lossy();
        let gif = self.final_path.to_string_lossy();

        // Try agg first (Rust-based, best quality)
        let result = std::process::Command::new("agg")
            .args([cast.as_ref(), gif.as_ref()])
            .output();

        match result {
            Ok(output) if output.status.success() => {
                // Clean up temp .cast file
                let _ = std::fs::remove_file(&self.cast_path);
                Ok(())
            }
            _ => {
                // agg not found — keep the .cast and tell the user
                let fallback = self.final_path.with_extension("cast");
                if self.cast_path != fallback {
                    std::fs::rename(&self.cast_path, &fallback)?;
                }
                Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    format!(
                        "agg not found — install with: cargo install agg\n\
                         Saved asciicast to {}\n\
                         Convert manually: agg {} {}",
                        fallback.display(),
                        fallback.display(),
                        gif
                    ),
                ))
            }
        }
    }
}

/// A writer that sends output to both stdout and a recorder.
/// Used as the backend for ratatui's Terminal to capture exact ANSI output.
pub struct TeeWriter {
    stdout: io::Stdout,
    recorder: *mut Recorder, // Raw pointer to avoid lifetime issues with Terminal ownership
}

// SAFETY: TeeWriter is only used on the main thread, and the Recorder
// outlives the Terminal that uses this writer.
unsafe impl Send for TeeWriter {}

impl TeeWriter {
    /// Create a new TeeWriter.
    ///
    /// # Safety
    /// The caller must ensure that `recorder` outlives this TeeWriter.
    pub unsafe fn new(recorder: *mut Recorder) -> Self {
        Self {
            stdout: io::stdout(),
            recorder,
        }
    }
}

impl Write for TeeWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.stdout.write(buf)?;
        // SAFETY: recorder is guaranteed to be valid by the caller of TeeWriter::new
        unsafe {
            (*self.recorder).capture(&buf[..n]);
        }
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.stdout.flush()?;
        // Flush accumulated data as one asciicast event per frame
        unsafe {
            let _ = (*self.recorder).flush_frame();
        }
        Ok(())
    }
}
