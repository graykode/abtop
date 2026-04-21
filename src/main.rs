mod app;
mod collector;
mod config;
mod demo;
mod model;
mod setup;
mod theme;
mod ui;

use app::{App, JumpOutcome};
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen};
use crossterm::ExecutableCommand;
use ratatui::prelude::*;
use std::io::{self, stdout};
use std::time::Duration;

fn main() -> io::Result<()> {
    // --version / -V flag: print version and exit
    if std::env::args().any(|a| a == "--version" || a == "-V") {
        println!("abtop {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }

    // --update flag: self-update via GitHub releases installer
    if std::env::args().any(|a| a == "--update") {
        return run_update();
    }

    // --setup flag: configure StatusLine hook and exit
    if std::env::args().any(|a| a == "--setup") {
        setup::run_setup();
        return Ok(());
    }

    // --theme flag > config file > default
    let initial_theme = std::env::args()
        .position(|a| a == "--theme")
        .map(|pos| {
            let val = std::env::args().nth(pos + 1);
            match val {
                Some(name) if !name.starts_with('-') => name,
                Some(name) => {
                    eprintln!("--theme requires a theme name, got '{}'", name);
                    eprintln!("available: {}", theme::THEME_NAMES.join(", "));
                    std::process::exit(1);
                }
                None => {
                    eprintln!("--theme requires a theme name");
                    eprintln!("available: {}", theme::THEME_NAMES.join(", "));
                    std::process::exit(1);
                }
            }
        })
        .map(|name| {
            theme::Theme::by_name(&name).unwrap_or_else(|| {
                eprintln!(
                    "unknown theme '{}'. available: {}",
                    name,
                    theme::THEME_NAMES.join(", ")
                );
                std::process::exit(1);
            })
        })
        .or_else(|| {
            let cfg = config::load_config();
            theme::Theme::by_name(&cfg.theme)
        });

    let demo_mode = std::env::args().any(|a| a == "--demo");
    let exit_on_jump = std::env::args().any(|a| a == "--exit-on-jump");

    // --once flag: print snapshot and exit
    if std::env::args().any(|a| a == "--once") {
        let mut app = App::new(initial_theme.unwrap_or_default());
        if demo_mode {
            demo::populate_demo(&mut app);
        } else {
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
        }
        print_snapshot(&app);
        return Ok(());
    }

    // Setup terminal
    enable_raw_mode()?;
    stdout().execute(EnterAlternateScreen)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(stdout()))?;

    let app_result = run_app(&mut terminal, demo_mode, initial_theme, exit_on_jump);

    // Always attempt both cleanup steps regardless of app result
    let r1 = disable_raw_mode();
    let r2 = stdout().execute(LeaveAlternateScreen).map(|_| ());

    // Return app error first, then cleanup errors
    app_result.and(r1).and(r2)
}

fn run_app(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>, demo_mode: bool, initial_theme: Option<theme::Theme>, exit_on_jump: bool) -> io::Result<()> {
    let mut app = App::new(initial_theme.unwrap_or_default());
    if demo_mode {
        demo::populate_demo(&mut app);
    } else {
        app.tick();
    }

    let mut last_tick = std::time::Instant::now();
    let tick_interval = Duration::from_secs(2);
    let render_interval = Duration::from_millis(500);

    loop {
        terminal.draw(|f| ui::draw(f, &app))?;

        // Poll at 500ms for smooth animations; data tick every 2s
        let had_input = if event::poll(render_interval)? {
            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Press {
                    if app.config_open {
                        match key.code {
                            KeyCode::Esc | KeyCode::Char('q') => app.toggle_config(),
                            KeyCode::Down | KeyCode::Char('j') => app.config_select_next(),
                            KeyCode::Up | KeyCode::Char('k') => app.config_select_prev(),
                            KeyCode::Enter | KeyCode::Char(' ') => app.config_toggle_selected(),
                            _ => {}
                        }
                    } else if app.filter_active {
                        match key.code {
                            KeyCode::Esc => app.clear_filter(),
                            KeyCode::Enter => app.filter_active = false,
                            KeyCode::Backspace => app.filter_pop(),
                            KeyCode::Down => app.select_next(),
                            KeyCode::Up => app.select_prev(),
                            KeyCode::Char(c) => app.filter_push(c),
                            _ => {}
                        }
                    } else {
                        match key.code {
                            KeyCode::Char('q') => app.quit(),
                            KeyCode::Char('r') if !demo_mode => app.tick(),
                            KeyCode::Down | KeyCode::Char('j') => app.select_next(),
                            KeyCode::Up | KeyCode::Char('k') => app.select_prev(),
                            KeyCode::Char('x') if !demo_mode => app.kill_selected(),
                            KeyCode::Char('X') if !demo_mode => app.kill_orphan_ports(),
                            KeyCode::Char('t') => app.cycle_theme(),
                            KeyCode::Char('T') => app.tree_view = !app.tree_view,
                            KeyCode::Char(c @ '1'..='5') => app.toggle_panel(c as u8 - b'0'),
                            KeyCode::Char('c') => app.toggle_config(),
                            KeyCode::Char('/') => app.filter_active = true,
                            KeyCode::Esc if !app.filter_text.is_empty() => app.clear_filter(),
                            KeyCode::Enter if !demo_mode => {
                                match app.jump_to_session() {
                                    JumpOutcome::Jumped if exit_on_jump => app.quit(),
                                    JumpOutcome::Failed(msg) => app.set_status(msg),
                                    JumpOutcome::Jumped | JumpOutcome::NoOp => {}
                                }
                            },
                            _ => {}
                        }
                    }
                }
            }
            true
        } else {
            false
        };

        if demo_mode {
            // Rotate token rates to animate the sparkline
            if let Some(front) = app.token_rates.pop_front() {
                app.token_rates.push_back(front);
            }
        } else if !had_input && last_tick.elapsed() >= tick_interval {
            // Data tick every 2s — skip when handling input to avoid lag
            app.tick();
            last_tick = std::time::Instant::now();
        }

        if app.should_quit {
            break;
        }
    }

    Ok(())
}

/// Strip control characters (including ANSI escapes) and Unicode bidi
/// overrides from a string for safe terminal output. Defeats CVE-2021-42574
/// (Trojan Source) style attacks via RTLO/LRO/PDF/isolate characters.
fn sanitize_output(s: &str) -> String {
    s.chars()
        .filter(|c| !c.is_control()
            && !matches!(*c,
                '\u{202A}'..='\u{202E}'
                | '\u{2066}'..='\u{2069}'
                | '\u{200E}'
                | '\u{200F}'))
        .collect()
}

fn print_snapshot(app: &App) {
    println!("abtop — {} sessions\n", app.sessions.len());
    for session in &app.sessions {
        let status = match &session.status {
            model::SessionStatus::Working => "● Work",
            model::SessionStatus::Waiting => "◌ Wait",
            model::SessionStatus::Done => "✓ Done",
        };
        let sid_short = if session.session_id.len() >= 7 {
            &session.session_id[..7]
        } else {
            &session.session_id
        };
        let project_label = format!("{}({})", session.project_name, sid_short);
        let summary = sanitize_output(&app.session_summary(session));
        println!(
            "  {} {:<20} {} {} {:<10} CTX:{:>3.0}% Tok:{} Mem:{}M {}",
            session.pid,
            sanitize_output(&project_label),
            summary,
            status,
            session.model.replace("claude-", ""),
            session.context_percent,
            fmt_tok(session.total_tokens()),
            session.mem_mb,
            session.elapsed_display(),
        );
        if let Some(task) = session.current_tasks.last() {
            println!("       └─ {}", sanitize_output(task));
        }
        for child in &session.children {
            let port = child.port.map(|p| format!(":{}", p)).unwrap_or_default();
            println!(
                "       {} {} {}K {}",
                child.pid,
                sanitize_output(&child.command.split_whitespace().take(3).collect::<Vec<_>>().join(" ")),
                child.mem_kb / 1024,
                port,
            );
        }
    }
}

fn run_update() -> io::Result<()> {
    let current = env!("CARGO_PKG_VERSION");
    println!("abtop v{current} — checking for updates...\n");

    // Download to a private temp file (O_EXCL + random suffix) so a local
    // attacker can't pre-place a symlink or swap the file mid-run.
    let tmp = tempfile::Builder::new()
        .prefix("abtop-installer-")
        .suffix(".sh")
        .tempfile()?;
    let installer_path = tmp.path().to_path_buf();

    let dl_status = std::process::Command::new("curl")
        .args([
            "--proto", "=https",
            "--tlsv1.2",
            "-LsSf",
            "https://github.com/graykode/abtop/releases/latest/download/abtop-installer.sh",
            "-o",
        ])
        .arg(&installer_path)
        .status()?;

    if !dl_status.success() {
        eprintln!("\nDownload failed. You can also update manually:");
        eprintln!("  cargo install abtop --force");
        std::process::exit(1);
    }

    // Show checksum so the user can verify if desired.
    // macOS ships `shasum` (Perl) by default, Linux ships `sha256sum` (coreutils).
    let checksum_shown = std::process::Command::new("shasum")
        .args(["-a", "256"])
        .arg(&installer_path)
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !checksum_shown {
        let _ = std::process::Command::new("sha256sum")
            .arg(&installer_path)
            .status();
    }

    let status = std::process::Command::new("sh")
        .arg(&installer_path)
        .status()?;

    // NamedTempFile::drop removes the file; explicit drop to sequence it
    // after sh exits.
    drop(tmp);

    if !status.success() {
        eprintln!("\nUpdate failed. You can also update manually:");
        eprintln!("  cargo install abtop --force");
        std::process::exit(1);
    }

    Ok(())
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
