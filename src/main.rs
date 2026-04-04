mod action;
mod app;
mod discovery;
mod monitor;
mod process;
mod session;
mod ui;

use std::io;
use std::time::{Duration, Instant};

use clap::Parser;
use crossterm::{
    event::{self, Event, KeyCode, KeyModifiers},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{Terminal, backend::CrosstermBackend};

use app::App;

#[derive(Parser)]
#[command(name = "claudectl", version, about = "Monitor and manage Claude Code CLI agents")]
struct Cli {
    /// Refresh interval in milliseconds
    #[arg(short, long, default_value_t = 2000)]
    interval: u64,

    /// Print session list to stdout and exit (no TUI)
    #[arg(short, long)]
    list: bool,
}

fn main() -> io::Result<()> {
    let cli = Cli::parse();

    if cli.list {
        return print_list();
    }

    let tick_rate = Duration::from_millis(cli.interval);

    // Setup terminal
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    // Run app
    let result = run(&mut terminal, tick_rate);

    // Restore terminal
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    result
}

fn print_list() -> io::Result<()> {
    let app = App::new();

    if app.sessions.is_empty() {
        println!("No active Claude sessions.");
        return Ok(());
    }

    println!(
        "{:<7} {:<16} {:<12} {:<8} {:<8} {:<9} {:<10} {:<6} {:<6} TOKENS",
        "PID", "PROJECT", "STATUS", "CTX%", "COST", "$/HR", "ELAPSED", "CPU%", "MEM"
    );
    println!("{}", "-".repeat(105));

    for s in &app.sessions {
        println!(
            "{:<7} {:<16} {:<12} {:<8} {:<8} {:<9} {:<10} {:<6.1} {:<6} {}",
            s.pid,
            s.display_name(),
            s.status.to_string(),
            s.format_context(),
            s.format_cost(),
            s.format_burn_rate(),
            s.format_elapsed(),
            s.cpu_percent,
            s.format_mem(),
            s.format_tokens(),
        );
    }

    let total_cost: f64 = app.sessions.iter().map(|s| s.cost_usd).sum();
    println!("{}", "-".repeat(105));
    println!("Total cost: ${total_cost:.2}");

    Ok(())
}

fn run(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>, tick_rate: Duration) -> io::Result<()> {
    let mut app = App::new();
    let mut last_tick = Instant::now();

    loop {
        terminal.draw(|frame| {
            ui::table::render(
                frame,
                frame.area(),
                &app.sessions,
                &mut app.table_state,
                &app.status_msg,
                app.input_mode,
                &app.input_buffer,
            );
        })?;

        let timeout = tick_rate
            .checked_sub(last_tick.elapsed())
            .unwrap_or_else(|| Duration::from_secs(0));

        if event::poll(timeout)? {
            if let Event::Key(key) = event::read()? {
                // Input mode: capture text for sending to a session
                if app.input_mode {
                    match key.code {
                        KeyCode::Enter => {
                            if let Some(pid) = app.input_target_pid {
                                if let Some(session) = app.sessions.iter().find(|s| s.pid == pid) {
                                    let text = format!("{}\n", app.input_buffer);
                                    match action::send_input(session, &text) {
                                        Ok(()) => app.status_msg = format!("Sent to {}", session.display_name()),
                                        Err(e) => app.status_msg = format!("Error: {e}"),
                                    }
                                }
                            }
                            app.input_mode = false;
                            app.input_buffer.clear();
                            app.input_target_pid = None;
                        }
                        KeyCode::Esc => {
                            app.input_mode = false;
                            app.input_buffer.clear();
                            app.input_target_pid = None;
                            app.status_msg = "Input cancelled".into();
                        }
                        KeyCode::Backspace => {
                            app.input_buffer.pop();
                        }
                        KeyCode::Char(c) => {
                            app.input_buffer.push(c);
                        }
                        _ => {}
                    }
                    continue;
                }

                // Normal mode
                match (key.code, key.modifiers) {
                    (KeyCode::Char('q'), _) | (KeyCode::Esc, _) => {
                        app.should_quit = true;
                    }
                    (KeyCode::Char('c'), KeyModifiers::CONTROL) => {
                        app.should_quit = true;
                    }
                    (KeyCode::Char('j'), _) | (KeyCode::Down, _) => {
                        app.cancel_pending_kill();
                        app.next();
                    }
                    (KeyCode::Char('k'), _) | (KeyCode::Up, _) => {
                        app.cancel_pending_kill();
                        app.previous();
                    }
                    (KeyCode::Char('r'), _) => {
                        app.cancel_pending_kill();
                        app.refresh();
                    }
                    (KeyCode::Char('d'), _) | (KeyCode::Char('x'), _) => {
                        app.handle_kill();
                    }
                    (KeyCode::Char('y'), _) => {
                        // Quick approve: send Enter to NeedsInput sessions
                        app.cancel_pending_kill();
                        if let Some(session) = app.selected_session() {
                            if session.status == session::SessionStatus::NeedsInput {
                                match action::approve_session(session) {
                                    Ok(()) => app.status_msg = format!("Approved {}", session.display_name()),
                                    Err(e) => app.status_msg = format!("Error: {e}"),
                                }
                            } else {
                                app.status_msg = "Session is not waiting for input".into();
                            }
                        }
                    }
                    (KeyCode::Char('i'), _) => {
                        // Enter input mode to send text to a session
                        app.cancel_pending_kill();
                        let info = app.selected_session().map(|s| (s.pid, s.display_name().to_string()));
                        if let Some((pid, name)) = info {
                            app.input_mode = true;
                            app.input_buffer.clear();
                            app.input_target_pid = Some(pid);
                            app.status_msg = format!("Input to {name} (Enter to send, Esc to cancel): ");
                        }
                    }
                    (KeyCode::Tab, _) | (KeyCode::Enter, _) => {
                        app.cancel_pending_kill();
                        if let Some(session) = app.selected_session() {
                            match action::switch_to_terminal(session) {
                                Ok(()) => {
                                    app.status_msg = format!("Switched to {}", session.display_name());
                                }
                                Err(e) => {
                                    app.status_msg = format!("Error: {e}");
                                }
                            }
                        } else {
                            app.status_msg = "No session selected".into();
                        }
                    }
                    _ => {
                        app.cancel_pending_kill();
                    }
                }
            }
        }

        if app.should_quit {
            return Ok(());
        }

        if last_tick.elapsed() >= tick_rate {
            app.tick();
            last_tick = Instant::now();
        }
    }
}
