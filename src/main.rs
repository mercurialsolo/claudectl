mod app;
mod discovery;
mod monitor;
mod process;
mod session;
mod terminals;
mod ui;

use std::io;
use std::time::{Duration, Instant};

use clap::Parser;
use crossterm::{
    event::{self, Event},
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

    /// Enable desktop notifications on NeedsInput transitions
    #[arg(long)]
    notify: bool,

    /// Print JSON array of sessions and exit
    #[arg(long)]
    json: bool,
}

fn main() -> io::Result<()> {
    let cli = Cli::parse();

    if cli.json {
        return print_json();
    }

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
    let result = run(&mut terminal, tick_rate, cli.notify);

    // Restore terminal
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    result
}

fn print_json() -> io::Result<()> {
    let app = App::new();
    let values: Vec<serde_json::Value> = app.sessions.iter().map(|s| s.to_json_value()).collect();
    let json = serde_json::to_string_pretty(&values).unwrap_or_else(|_| "[]".to_string());
    println!("{json}");
    Ok(())
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

fn run(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    tick_rate: Duration,
    notify: bool,
) -> io::Result<()> {
    let mut app = App::new();
    app.notify = notify;
    let mut last_tick = Instant::now();

    loop {
        terminal.draw(|frame| {
            ui::table::render(frame, frame.area(), &app);
        })?;

        let timeout = tick_rate
            .checked_sub(last_tick.elapsed())
            .unwrap_or_else(|| Duration::from_secs(0));

        if event::poll(timeout)? {
            if let Event::Key(key) = event::read()? {
                if !app.handle_key(key) {
                    return Ok(());
                }
            }
        }

        if last_tick.elapsed() >= tick_rate {
            app.tick();
            last_tick = Instant::now();
        }
    }
}
