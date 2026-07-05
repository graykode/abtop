//! abtop — AI agent monitor.
//!
//! This crate is both a binary (the TUI, entered via [`run`]) and a library.
//! The library surface exists so a separate local tool (e.g. a web UI) can
//! reuse the data-collection layer in-process and serialize it via
//! [`snapshot::Snapshot`] / [`app::App::to_snapshot`], without reimplementing
//! session discovery and without depending on the terminal frontend.
//!
//! # Public API for library consumers
//!
//! The stable surface for in-process consumers is [`app`] (notably
//! [`App::to_snapshot`](app::App::to_snapshot) and
//! [`App::tick_no_summaries`](app::App::tick_no_summaries)), [`snapshot`],
//! [`config`], [`demo`], [`host_info`], and the data types in [`model`]. The
//! [`collector`], [`locale`], [`setup`], [`theme`], and [`ui`] modules are
//! published mainly to support the bundled TUI binary and may change without a
//! semver-major bump — depend on them at your own risk.
//!
//! Enum wire formats are part of the snapshot contract: variants such as
//! [`model::SessionStatus`] serialize as their CamelCase names (`"Thinking"`,
//! `"Executing"`, …) and chat roles serialize as `"user"` / `"assistant"`.
//! These strings are stable and won't be renamed without a major version bump.
//!
//! # Threading model
//!
//! [`App`] is **not** `Send`: it owns boxed collector trait objects
//! and must stay on the thread that created it. Don't move it between threads
//! or share it with request handlers — instead, run the collector loop on one
//! thread, serialize each [`snapshot::Snapshot`] to JSON, and hand the *string*
//! to other threads.
//!
//! # Typical usage
//!
//! ```no_run
//! use abtop::app::App;
//! use abtop::{config, theme::Theme};
//!
//! let cfg = config::load_config();
//! let mut app = App::new_with_config_and_claude_dirs(
//!     Theme::default(),
//!     &cfg.hidden_agents,
//!     cfg.panels,
//!     &cfg.claude_config_dirs,
//! );
//! loop {
//!     app.tick_no_summaries();                // refresh without spawning `claude --print`
//!     let snap = app.to_snapshot(2_000);      // pure read → JSON-friendly DTO
//!     let json = serde_json::to_string(&snap).unwrap();
//!     // ... serve `json`, sleep for the interval, repeat ...
//!     # break;
//! }
//! ```

pub mod app;
pub mod collector;
pub mod config;
pub mod demo;
pub mod host_info;
pub mod jump;
pub mod locale;
pub mod model;
pub mod setup;
pub mod snapshot;
pub mod theme;
pub mod ui;

use app::{App, JumpOutcome};
use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, MouseButton,
    MouseEvent, MouseEventKind,
};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use crossterm::ExecutableCommand;
use ratatui::prelude::*;
use std::io::{self, stdout};
use std::time::Duration;

/// Construct a headless `App` from loaded config + theme. Shared by the
/// `--json` and `--once` entry points.
fn build_app(theme: theme::Theme, cfg: &config::AppConfig) -> App {
    App::new_with_config_and_claude_dirs(
        theme,
        &cfg.hidden_agents,
        cfg.panels,
        &cfg.claude_config_dirs,
    )
}

pub fn run() -> io::Result<()> {
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

    // Load config once; it drives both the default theme and the hidden-agents list.
    let cfg = config::load_config();

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
        .or_else(|| theme::Theme::by_name(&cfg.theme));

    let demo_mode = std::env::args().any(|a| a == "--demo");
    let exit_on_jump = std::env::args().any(|a| a == "--exit-on-jump");
    let mouse_capture = should_enable_mouse_capture(std::env::args());

    // --json flag: print a machine-readable JSON snapshot and exit.
    // Single tick, no summary subprocesses. Useful for scripting and as a
    // manual check of the web snapshot API; the web tool uses the library
    // `App::to_snapshot` directly rather than shelling out to this.
    if std::env::args().any(|a| a == "--json") {
        let mut app = build_app(initial_theme.unwrap_or_default(), &cfg);
        if demo_mode {
            demo::populate_demo(&mut app);
        } else {
            app.tick_no_summaries();
        }
        match serde_json::to_string_pretty(&app.to_snapshot(2000)) {
            Ok(json) => {
                println!("{}", json);
                return Ok(());
            }
            Err(e) => {
                eprintln!("failed to serialize snapshot: {}", e);
                std::process::exit(1);
            }
        }
    }

    // --once flag: print snapshot and exit
    if std::env::args().any(|a| a == "--once") {
        let mut app = build_app(initial_theme.unwrap_or_default(), &cfg);
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
    if mouse_capture {
        stdout().execute(EnableMouseCapture)?;
    }
    let mut terminal = Terminal::new(CrosstermBackend::new(stdout()))?;

    let app_result = run_app(
        &mut terminal,
        demo_mode,
        initial_theme,
        exit_on_jump,
        &cfg.hidden_agents,
        cfg.panels,
        &cfg.claude_config_dirs,
    );

    // Always attempt both cleanup steps regardless of app result
    let r1 = if mouse_capture {
        stdout().execute(DisableMouseCapture).map(|_| ())
    } else {
        Ok(())
    };
    let r2 = disable_raw_mode();
    let r3 = stdout().execute(LeaveAlternateScreen).map(|_| ());

    // Return app error first, then cleanup errors
    app_result.and(r1).and(r2).and(r3)
}

fn should_enable_mouse_capture<I, S>(args: I) -> bool
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    args.into_iter().any(|a| a.as_ref() == "--mouse")
}

fn run_app(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    demo_mode: bool,
    initial_theme: Option<theme::Theme>,
    exit_on_jump: bool,
    hidden_agents: &[String],
    panels: config::PanelVisibility,
    claude_config_dirs: &[std::path::PathBuf],
) -> io::Result<()> {
    let mut app = App::new_with_config_and_claude_dirs(
        initial_theme.unwrap_or_default(),
        hidden_agents,
        panels,
        claude_config_dirs,
    );
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
            match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    if app.help_open {
                        // Any key dismisses help.
                        app.help_open = false;
                    } else if app.view_open {
                        match key.code {
                            KeyCode::Esc | KeyCode::Char('v') => app.view_open = false,
                            KeyCode::Char('T') => app.tree_view = !app.tree_view,
                            KeyCode::Char('l') => app.toggle_timeline(),
                            KeyCode::Char('f') => app.toggle_file_audit(),
                            KeyCode::Char(c @ '1'..='7') => app.toggle_panel(c as u8 - b'0'),
                            KeyCode::Char('M') => app.toggle_mcp_session_suppression(),
                            KeyCode::Char('t') => app.cycle_theme(),
                            _ => {}
                        }
                    } else if app.config_open {
                        match key.code {
                            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('c') => {
                                app.toggle_config()
                            }
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
                            KeyCode::Right | KeyCode::Tab => app.select_next_narrow_tab(),
                            KeyCode::Left | KeyCode::BackTab => app.select_prev_narrow_tab(),
                            KeyCode::Char('w') => app.set_narrow_tab(app::NarrowTab::Work),
                            KeyCode::Char('u') => app.set_narrow_tab(app::NarrowTab::Usage),
                            KeyCode::Char('s') => app.set_narrow_tab(app::NarrowTab::System),
                            KeyCode::Char('+') | KeyCode::Char('=') => {
                                app.maximize_active_narrow_section()
                            }
                            KeyCode::Char('-') => app.restore_narrow_sections(),
                            KeyCode::Char('x') if !demo_mode => app.kill_selected(),
                            KeyCode::Char('X') if !demo_mode => app.kill_orphan_ports(),
                            KeyCode::Char('t') => app.cycle_theme(),
                            KeyCode::Char('T') => app.tree_view = !app.tree_view,
                            KeyCode::Char('l') | KeyCode::Char('L') => app.toggle_timeline(),
                            KeyCode::Char(c @ '1'..='7') => app.toggle_panel(c as u8 - b'0'),
                            KeyCode::Char('M') => app.toggle_mcp_session_suppression(),
                            KeyCode::Char('c') => app.toggle_config(),
                            KeyCode::Char('v') => app.toggle_view_menu(),
                            KeyCode::Char('?') => app.toggle_help(),
                            KeyCode::Char('/') => app.filter_active = true,
                            KeyCode::Esc if !app.filter_text.is_empty() => app.clear_filter(),
                            KeyCode::Char('f') | KeyCode::Char('F') => app.toggle_file_audit(),
                            KeyCode::Enter if !demo_mode => match app.jump_to_session() {
                                JumpOutcome::Jumped if exit_on_jump => app.quit(),
                                JumpOutcome::Failed(msg) => app.set_status(msg),
                                JumpOutcome::Jumped | JumpOutcome::NoOp => {}
                            },
                            _ => {}
                        }
                    }
                }
                Event::Mouse(mouse) => {
                    let size = terminal.size()?;
                    let area = Rect::new(0, 0, size.width, size.height);
                    handle_mouse_event(&mut app, mouse, area);
                }
                _ => {}
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

fn handle_mouse_event(app: &mut App, mouse: MouseEvent, area: Rect) {
    if app.help_open || app.view_open || app.config_open || app.filter_active {
        return;
    }

    match mouse.kind {
        MouseEventKind::Down(MouseButton::Left) => {
            if let Some(target) = ui::click_target(app, area, mouse.column, mouse.row) {
                match target {
                    ui::ClickTarget::NarrowTab(tab) => app.set_narrow_tab(tab),
                    ui::ClickTarget::NarrowSection(section) => {
                        app.set_active_narrow_section(section);
                    }
                    ui::ClickTarget::NarrowZoom(section) => {
                        app.toggle_narrow_section_zoom(section);
                    }
                    ui::ClickTarget::Session(index) => {
                        app.select_session(index);
                        app.set_active_narrow_section(app::NarrowSection::Sessions);
                    }
                    ui::ClickTarget::KillOrphanPorts => {
                        app.set_active_narrow_section(app::NarrowSection::Ports);
                        app.kill_orphan_ports();
                    }
                }
            }
        }
        MouseEventKind::ScrollDown => app.select_next(),
        MouseEventKind::ScrollUp => app.select_prev(),
        MouseEventKind::ScrollRight => app.select_next_narrow_tab(),
        MouseEventKind::ScrollLeft => app.select_prev_narrow_tab(),
        _ => {}
    }
}

/// Strip control characters (including ANSI escapes) and Unicode bidi
/// overrides from a string for safe terminal output. Defeats CVE-2021-42574
/// (Trojan Source) style attacks via RTLO/LRO/PDF/isolate characters.
fn sanitize_output(s: &str) -> String {
    s.chars()
        .filter(|c| {
            !c.is_control()
                && !matches!(*c,
                '\u{202A}'..='\u{202E}'
                | '\u{2066}'..='\u{2069}'
                | '\u{200E}'
                | '\u{200F}')
        })
        .collect()
}

fn print_snapshot(app: &App) {
    println!(
        "abtop — {} sessions, {} mcp servers\n",
        app.sessions.len(),
        app.mcp_servers.len()
    );
    if !app.mcp_servers.is_empty() {
        let now = std::time::SystemTime::now();
        for server in &app.mcp_servers {
            let active = server.active_count(now, collector::mcp::ACTIVE_MTIME_SECS);
            let total = server.rollouts.len();
            let last_age = server
                .latest_mtime()
                .and_then(|m| now.duration_since(m).ok())
                .map(|d| {
                    if d.as_secs() < 60 {
                        format!("{}s", d.as_secs())
                    } else if d.as_secs() < 3600 {
                        format!("{}m", d.as_secs() / 60)
                    } else {
                        format!("{}h", d.as_secs() / 3600)
                    }
                })
                .unwrap_or_else(|| "—".to_string());
            let profile = server.profile.as_deref().unwrap_or("default");
            println!(
                "  mcp pid={} parent={} profile={:<16} active={}/{} last={}",
                server.pid, server.parent_cli, profile, active, total, last_age
            );
        }
        println!();
    }
    for session in &app.sessions {
        let status = match &session.status {
            model::SessionStatus::Thinking => "◉ Think",
            model::SessionStatus::Executing => "● Exec",
            model::SessionStatus::Waiting => "◌ Wait",
            model::SessionStatus::Unknown => "? Unknown",
            model::SessionStatus::RateLimited => "⏳ Rate",
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
                sanitize_output(
                    &child
                        .command
                        .split_whitespace()
                        .take(3)
                        .collect::<Vec<_>>()
                        .join(" ")
                ),
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
            "--proto",
            "=https",
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mouse_capture_is_opt_in() {
        assert!(!should_enable_mouse_capture(["abtop"]));
        assert!(should_enable_mouse_capture(["abtop", "--mouse"]));
    }
}
