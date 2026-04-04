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
#[command(name = "claudectl", about = "Monitor and manage Claude Code CLI agents")]
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
        "{:<7} {:<20} {:<12} {:<11} {:<9} {:<10} {:<7} {:<7} {:<8} TOKENS",
        "PID", "PROJECT", "STATUS", "MODEL", "TTY", "ELAPSED", "CPU%", "MEM", "COST"
    );
    println!("{}", "-".repeat(110));

    for s in &app.sessions {
        println!(
            "{:<7} {:<20} {:<12} {:<11} {:<9} {:<10} {:<7.1} {:<7} {:<8} {}",
            s.pid,
            s.display_name(),
            s.status.to_string(),
            s.model,
            s.tty,
            s.format_elapsed(),
            s.cpu_percent,
            s.format_mem(),
            s.format_cost(),
            s.format_tokens(),
        );
    }

    let total_cost: f64 = app.sessions.iter().map(|s| s.cost_usd).sum();
    println!("{}", "-".repeat(110));
    println!("Total cost: ${total_cost:.2}");

    Ok(())
}

fn run(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>, tick_rate: Duration) -> io::Result<()> {
    let mut app = App::new();
    let mut last_tick = Instant::now();

    loop {
        terminal.draw(|frame| {
            ui::table::render(frame, frame.area(), &app.sessions, &mut app.table_state, &app.status_msg);
        })?;

        let timeout = tick_rate
            .checked_sub(last_tick.elapsed())
            .unwrap_or_else(|| Duration::from_secs(0));

        if event::poll(timeout)? {
            if let Event::Key(key) = event::read()? {
                match (key.code, key.modifiers) {
                    (KeyCode::Char('q'), _) | (KeyCode::Esc, _) => {
                        app.should_quit = true;
                    }
                    (KeyCode::Char('c'), KeyModifiers::CONTROL) => {
                        app.should_quit = true;
                    }
                    (KeyCode::Char('j'), _) | (KeyCode::Down, _) => {
                        app.next();
                    }
                    (KeyCode::Char('k'), _) | (KeyCode::Up, _) => {
                        app.previous();
                    }
                    (KeyCode::Char('r'), _) => {
                        app.cancel_pending_kill();
                        app.refresh();
                    }
                    (KeyCode::Char('d'), _) | (KeyCode::Char('x'), _) => {
                        app.handle_kill();
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
