mod app;
mod collector;
mod model;
mod ui;

use app::App;
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen};
use crossterm::ExecutableCommand;
use ratatui::prelude::*;
use std::io::{self, stdout};
use std::time::Duration;

fn main() -> io::Result<()> {
    // --once flag: print snapshot and exit
    if std::env::args().any(|a| a == "--once") {
        let mut app = App::new();
        app.tick();
        // Wait for summaries: retry-aware budget (up to 30s total to allow 2 × 10s attempts + slack)
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        while std::time::Instant::now() < deadline {
            app.drain_and_retry_summaries();
            if !app.has_pending_summaries() && !app.has_retryable_summaries() {
                break;
            }
            std::thread::sleep(Duration::from_millis(500));
        }
        print_snapshot(&app);
        return Ok(());
    }

    // Setup terminal
    enable_raw_mode()?;
    stdout().execute(EnterAlternateScreen)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(stdout()))?;

    let app_result = run_app(&mut terminal);

    // Always attempt both cleanup steps regardless of app result
    let r1 = disable_raw_mode();
    let r2 = stdout().execute(LeaveAlternateScreen).map(|_| ());

    // Return app error first, then cleanup errors
    app_result.and(r1).and(r2)
}

fn run_app(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>) -> io::Result<()> {
    let mut app = App::new();
    app.tick();

    loop {
        terminal.draw(|f| ui::draw(f, &app))?;

        // Poll for events with 2s timeout (tick interval)
        if event::poll(Duration::from_secs(2))? {
            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Press {
                    match key.code {
                        KeyCode::Char('q') => app.quit(),
                        KeyCode::Char('r') => app.tick(),
                        KeyCode::Down | KeyCode::Char('j') => app.select_next(),
                        KeyCode::Up | KeyCode::Char('k') => app.select_prev(),
                        KeyCode::Char('x') => app.kill_selected(),
                        _ => {}
                    }
                }
            }
        } else {
            // Timeout = tick
            app.tick();
        }

        if app.should_quit {
            break;
        }
    }

    Ok(())
}

fn print_snapshot(app: &App) {
    println!("abtop — {} sessions\n", app.sessions.len());
    for session in &app.sessions {
        let status = match &session.status {
            model::SessionStatus::Working => "● Work",
            model::SessionStatus::Waiting => "◌ Wait",
            model::SessionStatus::Error(_) => "✗ Err",
            model::SessionStatus::Done => "✓ Done",
        };
        let sid_short = if session.session_id.len() >= 7 {
            &session.session_id[..7]
        } else {
            &session.session_id
        };
        let project_label = format!("{}({})", session.project_name, sid_short);
        let summary = app.session_summary(session);
        println!(
            "  {} {:<20} {} {} {:<10} CTX:{:>3.0}% Tok:{} Mem:{}M {}",
            session.pid,
            project_label,
            summary,
            status,
            session.model.replace("claude-", ""),
            session.context_percent,
            fmt_tok(session.total_tokens()),
            session.mem_mb,
            session.elapsed_display(),
        );
        if let Some(task) = session.current_tasks.last() {
            println!("       └─ {}", task);
        }
        for child in &session.children {
            let port = child.port.map(|p| format!(":{}", p)).unwrap_or_default();
            println!(
                "       {} {} {}K {}",
                child.pid,
                child.command.split_whitespace().take(3).collect::<Vec<_>>().join(" "),
                child.mem_kb / 1024,
                port,
            );
        }
    }
}

fn fmt_tok(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else {
        format!("{}", n)
    }
}
