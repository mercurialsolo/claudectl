use ratatui::style::Color;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThemeMode {
    Dark,
    Light,
    None,
}

impl ThemeMode {
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_lowercase().as_str() {
            "dark" => Some(Self::Dark),
            "light" => Some(Self::Light),
            "none" => Some(Self::None),
            _ => None,
        }
    }

    /// Detect theme mode: CLI flag > config > NO_COLOR env > default (dark).
    pub fn detect(cli_theme: Option<&str>) -> Self {
        if let Some(t) = cli_theme.and_then(Self::parse) {
            return t;
        }
        if std::env::var_os("NO_COLOR").is_some() {
            return Self::None;
        }
        Self::Dark
    }
}

/// Semantic color palette for the TUI.
#[derive(Debug, Clone, Copy)]
pub struct Theme {
    #[allow(dead_code)]
    pub mode: ThemeMode,

    // Status colors
    pub status_needs_input: Color,
    pub status_processing: Color,
    pub status_waiting: Color,
    pub status_unknown: Color,
    pub status_idle: Color,
    pub status_finished: Color,

    // UI chrome
    pub border: Color,
    pub header: Color,
    pub footer: Color,
    pub highlight_key: Color,
    pub text_primary: Color,
    pub text_muted: Color,

    // Data colors
    pub cost: Color,
    pub cost_warning: Color,
    pub cost_danger: Color,
    pub context_ok: Color,
    pub context_warning: Color,
    pub context_danger: Color,
    pub burn_rate_low: Color,
    pub burn_rate_mid: Color,
    pub burn_rate_high: Color,
    pub sparkline: Color,
    pub input_accent: Color,
    pub success: Color,
    pub error: Color,
}

impl Theme {
    pub fn from_mode(mode: ThemeMode) -> Self {
        match mode {
            ThemeMode::Dark => Self::dark(),
            ThemeMode::Light => Self::light(),
            ThemeMode::None => Self::none(),
        }
    }

    fn dark() -> Self {
        Self {
            mode: ThemeMode::Dark,
            status_needs_input: Color::Magenta,
            status_processing: Color::Green,
            status_waiting: Color::Yellow,
            status_unknown: Color::Blue,
            status_idle: Color::DarkGray,
            status_finished: Color::Red,
            border: Color::DarkGray,
            header: Color::Cyan,
            footer: Color::DarkGray,
            highlight_key: Color::Yellow,
            text_primary: Color::White,
            text_muted: Color::DarkGray,
            cost: Color::Yellow,
            cost_warning: Color::LightRed,
            cost_danger: Color::Red,
            context_ok: Color::Green,
            context_warning: Color::Yellow,
            context_danger: Color::Red,
            burn_rate_low: Color::DarkGray,
            burn_rate_mid: Color::Yellow,
            burn_rate_high: Color::Red,
            sparkline: Color::Blue,
            input_accent: Color::Cyan,
            success: Color::Green,
            error: Color::Red,
        }
    }

    fn light() -> Self {
        Self {
            mode: ThemeMode::Light,
            status_needs_input: Color::Magenta,
            status_processing: Color::Blue,
            status_waiting: Color::Rgb(180, 140, 0), // Dark yellow
            status_unknown: Color::Gray,
            status_idle: Color::Gray,
            status_finished: Color::Red,
            border: Color::Gray,
            header: Color::Blue,
            footer: Color::Gray,
            highlight_key: Color::Blue,
            text_primary: Color::Black,
            text_muted: Color::Gray,
            cost: Color::Rgb(180, 140, 0),
            cost_warning: Color::Red,
            cost_danger: Color::LightRed,
            context_ok: Color::Blue,
            context_warning: Color::Rgb(180, 140, 0),
            context_danger: Color::Red,
            burn_rate_low: Color::Gray,
            burn_rate_mid: Color::Rgb(180, 140, 0),
            burn_rate_high: Color::Red,
            sparkline: Color::Blue,
            input_accent: Color::Blue,
            success: Color::Blue,
            error: Color::Red,
        }
    }

    fn none() -> Self {
        // Monochrome — no color, use terminal defaults
        Self {
            mode: ThemeMode::None,
            status_needs_input: Color::Reset,
            status_processing: Color::Reset,
            status_waiting: Color::Reset,
            status_unknown: Color::Reset,
            status_idle: Color::Reset,
            status_finished: Color::Reset,
            border: Color::Reset,
            header: Color::Reset,
            footer: Color::Reset,
            highlight_key: Color::Reset,
            text_primary: Color::Reset,
            text_muted: Color::Reset,
            cost: Color::Reset,
            cost_warning: Color::Reset,
            cost_danger: Color::Reset,
            context_ok: Color::Reset,
            context_warning: Color::Reset,
            context_danger: Color::Reset,
            burn_rate_low: Color::Reset,
            burn_rate_mid: Color::Reset,
            burn_rate_high: Color::Reset,
            sparkline: Color::Reset,
            input_accent: Color::Reset,
            success: Color::Reset,
            error: Color::Reset,
        }
    }

    /// Get the color for a session status.
    pub fn status_color(&self, status: &crate::session::SessionStatus) -> Color {
        use crate::session::SessionStatus;
        match status {
            SessionStatus::NeedsInput => self.status_needs_input,
            SessionStatus::Processing => self.status_processing,
            SessionStatus::WaitingInput => self.status_waiting,
            SessionStatus::Unknown => self.status_unknown,
            SessionStatus::Idle => self.status_idle,
            SessionStatus::Finished => self.status_finished,
        }
    }
}
