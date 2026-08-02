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
//! `"Executing"`, `"Idle"`, …) and chat roles serialize as `"user"` /
//! `"assistant"`.
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
mod codex_compat;
mod codex_hooks;
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
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
    MouseButton, MouseEvent, MouseEventKind,
};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use crossterm::ExecutableCommand;
use ratatui::prelude::*;
use std::ffi::{OsStr, OsString};
use std::io::{self, stdout};
use std::time::Duration;

/// Construct a headless `App`. Demo mode deliberately ignores user-specific
/// visibility and discovery settings so its snapshots remain reproducible.
fn build_app(theme: theme::Theme, cfg: &config::AppConfig, demo_mode: bool) -> App {
    if demo_mode {
        App::new_with_config_and_claude_dirs(theme, &[], config::PanelVisibility::default(), &[])
    } else {
        App::new_with_config_and_claude_dirs(
            theme,
            &cfg.hidden_agents,
            cfg.panels,
            &cfg.claude_config_dirs,
        )
    }
}

pub fn run() -> io::Result<()> {
    let raw_args = std::env::args_os().skip(1).collect::<Vec<_>>();

    // Hook launchers must never print, block the provider on an error, or
    // enter the TUI. Any invocation beginning with this private flag is
    // consumed here and exits successfully, including malformed arguments.
    if raw_args
        .first()
        .is_some_and(|argument| argument == OsStr::new("--codex-hook-ingest"))
    {
        codex_hooks::ingest_silently(raw_args[1..].to_vec());
        return Ok(());
    }

    match codex_dispatch(raw_args.clone()) {
        CodexDispatch::NotRequested => {}
        CodexDispatch::Help => {
            print_codex_launcher_help();
            return Ok(());
        }
        CodexDispatch::Invalid(message) => {
            eprintln!("{message}");
            eprintln!("usage: abtop codex -- [LEGACY_FORWARDED_ARGS...]");
            std::process::exit(2);
        }
        CodexDispatch::Launch(args) => {
            let exit_code = codex_compat::run(args)?;
            if exit_code != 0 {
                std::process::exit(exit_code);
            }
            return Ok(());
        }
    }

    // Native Codex plugin administration is separate from Claude's --setup.
    match codex_admin_dispatch(raw_args.clone()) {
        CodexAdminDispatch::NotRequested => {}
        CodexAdminDispatch::Invalid => {
            eprintln!(
                "usage: abtop --setup-codex | --uninstall-codex | --codex-integration-status"
            );
            std::process::exit(2);
        }
        CodexAdminDispatch::Setup => {
            match codex_hooks::plugin::setup() {
                Ok(report) => print_codex_setup_report(&report),
                Err(error) => {
                    eprintln!("Codex plugin setup failed: {error}");
                    std::process::exit(1);
                }
            }
            return Ok(());
        }
        CodexAdminDispatch::Uninstall => {
            match codex_hooks::plugin::uninstall() {
                Ok(report) => print_codex_uninstall_report(&report),
                Err(error) => {
                    eprintln!("Codex plugin uninstall failed: {error}");
                    std::process::exit(1);
                }
            }
            return Ok(());
        }
        CodexAdminDispatch::Status => {
            match codex_hooks::plugin::status() {
                Ok(status) => {
                    print_codex_integration_status(&status);
                    if !status.healthy {
                        std::process::exit(1);
                    }
                }
                Err(error) => {
                    eprintln!("Codex integration status failed: {error}");
                    std::process::exit(1);
                }
            }
            return Ok(());
        }
    }

    // --setup is deliberately a Claude-only exact singleton.
    if raw_args.iter().any(|a| a == OsStr::new("--setup")) {
        if raw_args.as_slice() != [OsString::from("--setup")] {
            eprintln!("usage: abtop --setup");
            std::process::exit(2);
        }
        setup::run_setup();
        return Ok(());
    }

    // --version / -V flag: print version and exit
    if raw_args
        .iter()
        .any(|a| a == OsStr::new("--version") || a == OsStr::new("-V"))
    {
        println!("abtop {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }

    // --update flag: self-update via GitHub releases installer
    if raw_args.iter().any(|a| a == OsStr::new("--update")) {
        return run_update();
    }

    // Load config once; it drives both the default theme and the hidden-agents list.
    let cfg = config::load_config();

    let demo_mode = std::env::args().any(|a| a == "--demo");

    // --theme flag > config file > default. Demo mode ignores the persisted
    // theme when no explicit override is supplied.
    let explicit_theme = std::env::args()
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
        });
    let initial_theme = explicit_theme.or_else(|| {
        if demo_mode {
            None
        } else {
            theme::Theme::by_name(&cfg.theme)
        }
    });

    let exit_on_jump = std::env::args().any(|a| a == "--exit-on-jump");
    let mouse_capture = should_enable_mouse_capture(std::env::args());

    // --json flag: print a machine-readable JSON snapshot and exit.
    // Single tick, no summary subprocesses. Useful for scripting and as a
    // manual check of the web snapshot API; the web tool uses the library
    // `App::to_snapshot` directly rather than shelling out to this.
    if std::env::args().any(|a| a == "--json") {
        let mut app = build_app(initial_theme.unwrap_or_default(), &cfg, demo_mode);
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
        let mut app = build_app(initial_theme.unwrap_or_default(), &cfg, demo_mode);
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
            // Summary generation can take long enough for lifecycle state to
            // change. Recollect once without scheduling another summary job so
            // the printed status reflects the end of the wait, not its start.
            app.tick_no_summaries();
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

#[derive(Debug, PartialEq, Eq)]
enum CodexDispatch {
    NotRequested,
    Help,
    Launch(Vec<OsString>),
    Invalid(String),
}

#[derive(Debug, PartialEq, Eq)]
enum CodexAdminDispatch {
    NotRequested,
    Setup,
    Uninstall,
    Status,
    Invalid,
}

fn codex_admin_dispatch<I>(args: I) -> CodexAdminDispatch
where
    I: IntoIterator<Item = OsString>,
{
    let args: Vec<_> = args.into_iter().collect();
    let requested = args.iter().any(|argument| {
        matches!(
            argument.to_str(),
            Some("--setup-codex" | "--uninstall-codex" | "--codex-integration-status")
        )
    });
    if !requested {
        return CodexAdminDispatch::NotRequested;
    }
    match args.as_slice() {
        [argument] if argument == OsStr::new("--setup-codex") => CodexAdminDispatch::Setup,
        [argument] if argument == OsStr::new("--uninstall-codex") => CodexAdminDispatch::Uninstall,
        [argument] if argument == OsStr::new("--codex-integration-status") => {
            CodexAdminDispatch::Status
        }
        _ => CodexAdminDispatch::Invalid,
    }
}

fn codex_dispatch<I>(args: I) -> CodexDispatch
where
    I: IntoIterator<Item = OsString>,
{
    let mut args = args.into_iter();
    if args.next().as_deref() != Some(OsStr::new("codex")) {
        return CodexDispatch::NotRequested;
    }

    let Some(separator) = args.next() else {
        return CodexDispatch::Invalid("missing `--` before Codex arguments".to_string());
    };
    if separator == OsStr::new("--help") || separator == OsStr::new("-h") {
        if args.next().is_some() {
            return CodexDispatch::Invalid(
                "launcher help does not accept additional arguments".to_string(),
            );
        }
        return CodexDispatch::Help;
    }
    if separator != OsStr::new("--") {
        return CodexDispatch::Invalid(format!(
            "expected `--` before Codex arguments, got `{}`",
            separator.to_string_lossy()
        ));
    }

    CodexDispatch::Launch(args.collect())
}

fn print_codex_launcher_help() {
    println!("Compatibility trampoline for retired abtop 0.6 shell integration.");
    println!();
    println!("usage: abtop codex -- [LEGACY_FORWARDED_ARGS...]");
    println!();
    println!("It directly executes the binary captured by the old shell block.");
    println!("Use native `codex ...` for normal work and run `abtop --setup-codex`");
    println!("to remove the retired block and install the isolated hook plugin.");
}

const CODEX_COVERAGE_NOTICE: &str = "Codex 0.146.0 cannot attest effective managed/cloud/profile/live hook coverage; installation readiness is diagnostic only.";
const CODEX_LIVE_STATUS_NOTICE: &str = "Live Codex Think/Exec/Wait/Idle remain Unknown and non-actionable; only an exact supported process/session Live→Gone transition can report non-actionable heuristic Done for 30 seconds.";

fn print_codex_coverage_notice() {
    println!("{CODEX_COVERAGE_NOTICE}");
    println!("{CODEX_LIVE_STATUS_NOTICE}");
}

fn print_codex_setup_report(report: &codex_hooks::plugin::SetupReport) {
    println!("Codex hook base installation completed.");
    println!("  Plugin: {}", codex_hooks::plugin::PLUGIN_ID);
    println!("  Marketplace: {}", report.paths.marketplace_root.display());
    println!("  Hooks declared: {}", report.hook_count);
    println!(
        "  Base config trusted: {}/{}",
        report.base_config_trusted_hooks, report.hook_count
    );
    println!(
        "  Base config enabled: {}/{}",
        report.base_config_enabled_hooks, report.hook_count
    );
    if !report.legacy_cleanup.changed_files.is_empty() {
        println!(
            "  Removed retired shell blocks from {} file(s).",
            report.legacy_cleanup.changed_files.len()
        );
    }
    if let Some(guidance) = &report.legacy_cleanup.powershell_guidance {
        println!("  {guidance}");
    }
    println!("Native `codex` was not wrapped, aliased, or replaced.");
    print_codex_coverage_notice();
    if report.review_required {
        println!("Restart Codex and review only the 11 abtop hooks in its trust prompt.");
        println!("Do not approve unrelated hooks from the same prompt.");
    } else {
        println!("Restart existing Codex sessions so they load this plugin version.");
    }
}

fn print_codex_uninstall_report(report: &codex_hooks::plugin::UninstallReport) {
    println!("Codex hook integration uninstalled.");
    println!("  Plugin removed: {}", report.plugin_removed);
    println!("  Marketplace removed: {}", report.marketplace_removed);
    if !report.legacy_cleanup.changed_files.is_empty() {
        println!(
            "  Removed retired shell blocks from {} file(s).",
            report.legacy_cleanup.changed_files.len()
        );
    }
    println!(
        "  Content-free audit data preserved at: {}",
        report.preserved_data_root.display()
    );
    if let Some(guidance) = &report.legacy_cleanup.powershell_guidance {
        println!("  {guidance}");
    }
    println!("Native `codex` was not modified.");
}

fn print_codex_integration_status(status: &codex_hooks::plugin::IntegrationStatus) {
    println!(
        "Codex hook base installation: {}",
        if status.healthy { "ready" } else { "not ready" }
    );
    print_codex_coverage_notice();
    println!("  CODEX_HOME: {}", status.paths.codex_home.display());
    println!(
        "  Codex executable: {}",
        status
            .codex_binary
            .as_deref()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "—".to_string())
    );
    println!("  Hook schema revision: {}", status.hook_schema_revision);
    println!(
        "  Helper digest: {}",
        status.helper_digest.as_deref().unwrap_or("—")
    );
    println!(
        "  Marketplace registered: {}",
        status.marketplace_registered
    );
    println!("  Plugin installed: {}", status.plugin_installed);
    println!("  Plugin enabled: {}", status.plugin_enabled);
    println!(
        "  Installed version: {}",
        status.installed_version.as_deref().unwrap_or("—")
    );
    println!("  Bundle valid: {}", status.bundle_valid);
    println!("  Attestation valid: {}", status.attestation_valid);
    println!(
        "  Base config trusted: {}/{}",
        status.base_config_trusted_hooks, status.hook_count
    );
    println!(
        "  Base config enabled: {}/{}",
        status.base_config_enabled_hooks, status.hook_count
    );
    println!(
        "  Legacy profile inspection: {}",
        if status.legacy_inspection_valid {
            "valid"
        } else {
            "failed"
        }
    );
    println!(
        "  Retired shell blocks found: {}",
        status.legacy_marker_files.len()
    );
    for detail in &status.details {
        println!("  Note: {detail}");
    }
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
    let mut app = if demo_mode {
        App::new_with_config_and_claude_dirs(
            initial_theme.unwrap_or_default(),
            &[],
            config::PanelVisibility::default(),
            &[],
        )
    } else {
        App::new_with_config_and_claude_dirs(
            initial_theme.unwrap_or_default(),
            hidden_agents,
            panels,
            claude_config_dirs,
        )
    };
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
                    handle_key_press(&mut app, key, demo_mode, exit_on_jump, |app| {
                        app.jump_to_session()
                    })
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

fn handle_key_press(
    app: &mut App,
    key: KeyEvent,
    demo_mode: bool,
    exit_on_jump: bool,
    jump_to_session: impl FnOnce(&mut App) -> JumpOutcome,
) {
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
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('c') => app.toggle_config(),
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
            KeyCode::Char('+') | KeyCode::Char('=') => app.maximize_active_narrow_section(),
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
            KeyCode::Enter if !demo_mode => match jump_to_session(app) {
                JumpOutcome::Jumped if exit_on_jump => app.quit(),
                JumpOutcome::Failed(msg) => app.set_status(msg),
                JumpOutcome::Jumped | JumpOutcome::NoOp => {}
            },
            _ => {}
        }
    }
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
        let status = snapshot_status_label(&session.status);
        let sid_short: String = session.session_id.chars().take(7).collect();
        let project_label = format!("{}({})", session.project_name, sid_short);
        let summary = sanitize_output(&app.session_summary(session));
        println!(
            "{}",
            format_snapshot_session_line(session, &project_label, &summary, status)
        );
        if let Some(task) = session.display_task() {
            println!("       └─ {}", sanitize_output(task));
        }
        if session.status_evidence.has_sample() {
            let recent = session.status_evidence.recent(5);
            let statuses = recent
                .observations
                .iter()
                .map(|sample| snapshot_status_name(&sample.status))
                .collect::<Vec<_>>()
                .join(",");
            println!(
                "       evidence={} reason={} observed_at_ms={} status_since_ms={} generation={} matching={} recent=[{}]",
                session.status_evidence.authority.as_str(),
                session.status_evidence.reason.as_str(),
                session.status_evidence.observed_at_ms,
                session.status_evidence.status_since_ms,
                session.status_evidence.connection_generation,
                session.status_evidence.consecutive_matching,
                statuses,
            );
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

fn snapshot_status_name(status: &model::SessionStatus) -> &'static str {
    match status {
        model::SessionStatus::Thinking => "Thinking",
        model::SessionStatus::Executing => "Executing",
        model::SessionStatus::Waiting => "Waiting",
        model::SessionStatus::Idle => "Idle",
        model::SessionStatus::Unknown => "Unknown",
        model::SessionStatus::RateLimited => "RateLimited",
        model::SessionStatus::Error => "Error",
        model::SessionStatus::Done => "Done",
    }
}

fn snapshot_status_label(status: &model::SessionStatus) -> &'static str {
    match status {
        model::SessionStatus::Thinking => "◉ Think",
        model::SessionStatus::Executing => "● Exec",
        model::SessionStatus::Waiting => "◌ Wait",
        model::SessionStatus::Idle => "○ Idle",
        model::SessionStatus::Unknown => "? Unknown",
        model::SessionStatus::Error => "✗ Error",
        model::SessionStatus::RateLimited => "⏳ Rate",
        model::SessionStatus::Done => "✓ Done",
    }
}

fn format_snapshot_session_line(
    session: &model::AgentSession,
    project_label: &str,
    summary: &str,
    status: &str,
) -> String {
    let context = if session.context_window == 0 {
        "—".to_string()
    } else {
        format!("{:.0}%", session.context_percent)
    };

    let model = session
        .model
        .strip_prefix("claude-")
        .unwrap_or(&session.model);
    format!(
        "  {:<8} {} {:<20} {} {} {:<10} CTX:{:>4} Tok:{} Mem:{}M {}",
        sanitize_output(session.agent_cli),
        session.pid,
        sanitize_output(project_label),
        summary,
        status,
        sanitize_output(model),
        context,
        fmt_tok(session.total_tokens()),
        session.mem_mb,
        session.elapsed_display(),
    )
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
    use crossterm::event::{KeyEvent, KeyModifiers};
    use ratatui::backend::TestBackend;

    #[test]
    fn mouse_capture_is_opt_in() {
        assert!(!should_enable_mouse_capture(["abtop"]));
        assert!(should_enable_mouse_capture(["abtop", "--mouse"]));
    }

    #[test]
    fn demo_app_ignores_persisted_panel_visibility() {
        let cfg = config::AppConfig {
            panels: config::PanelVisibility {
                context: false,
                quota: false,
                tokens: false,
                projects: false,
                ports: false,
                sessions: false,
                mcp: false,
            },
            ..config::AppConfig::default()
        };

        let app = build_app(theme::Theme::default(), &cfg, true);

        assert!(app.show_context);
        assert!(app.show_quota);
        assert!(app.show_tokens);
        assert!(app.show_projects);
        assert!(app.show_ports);
        assert!(app.show_sessions);
        assert!(app.show_mcp);
    }

    #[test]
    fn codex_launcher_requires_explicit_separator() {
        assert_eq!(
            codex_dispatch([OsString::from("codex")]),
            CodexDispatch::Invalid("missing `--` before Codex arguments".to_string())
        );
        assert!(matches!(
            codex_dispatch([OsString::from("codex"), OsString::from("resume")]),
            CodexDispatch::Invalid(_)
        ));
    }

    #[test]
    fn codex_admin_commands_are_exact_singletons() {
        assert_eq!(
            codex_admin_dispatch([OsString::from("--setup-codex")]),
            CodexAdminDispatch::Setup
        );
        assert_eq!(
            codex_admin_dispatch([OsString::from("--uninstall-codex")]),
            CodexAdminDispatch::Uninstall
        );
        assert_eq!(
            codex_admin_dispatch([OsString::from("--codex-integration-status")]),
            CodexAdminDispatch::Status
        );
        assert_eq!(
            codex_admin_dispatch([OsString::from("--setup")]),
            CodexAdminDispatch::NotRequested
        );
        assert_eq!(
            codex_admin_dispatch([OsString::from("--setup-codex"), OsString::from("--json"),]),
            CodexAdminDispatch::Invalid
        );
    }

    #[test]
    fn codex_launcher_preserves_os_arguments_after_separator() {
        let prompt = OsString::from("review paths with spaces");
        assert_eq!(
            codex_dispatch([
                OsString::from("codex"),
                OsString::from("--"),
                OsString::from("resume"),
                prompt.clone(),
            ]),
            CodexDispatch::Launch(vec![OsString::from("resume"), prompt])
        );
    }

    #[test]
    fn codex_launcher_help_is_unambiguous() {
        assert_eq!(
            codex_dispatch([OsString::from("codex"), OsString::from("--help")]),
            CodexDispatch::Help
        );
        assert_eq!(
            codex_dispatch([
                OsString::from("codex"),
                OsString::from("--"),
                OsString::from("--help"),
            ]),
            CodexDispatch::Launch(vec![OsString::from("--help")])
        );
    }

    #[test]
    fn codex_admin_notice_separates_installation_from_live_status_proof() {
        assert!(CODEX_COVERAGE_NOTICE.contains("cannot attest effective"));
        assert!(CODEX_COVERAGE_NOTICE.contains("installation readiness is diagnostic only"));
        assert!(CODEX_LIVE_STATUS_NOTICE.contains("Think/Exec/Wait/Idle remain Unknown"));
        assert!(CODEX_LIVE_STATUS_NOTICE.contains("Live→Gone"));
        assert!(CODEX_LIVE_STATUS_NOTICE.contains("Done for 30 seconds"));
    }

    #[test]
    fn once_session_line_includes_provider_and_unavailable_context() {
        let mut app = App::new_with_config(
            theme::Theme::default(),
            &[],
            config::PanelVisibility::default(),
        );
        demo::populate_demo(&mut app);
        let session = &mut app.sessions[0];
        session.agent_cli = "grok";
        session.context_window = 0;
        session.context_percent = 99.0;

        let line = format_snapshot_session_line(session, "project(session)", "summary", "◌ Wait");

        assert!(line.starts_with("  grok"));
        assert!(line.contains("CTX:   —"));
        assert!(!line.contains("99%"));
    }

    #[test]
    fn once_status_labels_distinguish_idle_from_actionable_waiting() {
        assert_eq!(snapshot_status_label(&model::SessionStatus::Idle), "○ Idle");
        assert_eq!(
            snapshot_status_label(&model::SessionStatus::Waiting),
            "◌ Wait"
        );
    }

    #[test]
    fn enter_jump_failure_renders_footer_status() {
        let mut app = App::new_with_config(
            theme::Theme::default(),
            &[],
            config::PanelVisibility::default(),
        );
        demo::populate_demo(&mut app);

        handle_key_press(
            &mut app,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            false,
            false,
            |_| JumpOutcome::Failed("cmux: socket broken; restart cmux".to_string()),
        );

        let backend = TestBackend::new(120, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| ui::draw(f, &app)).unwrap();
        let text = format!("{}", terminal.backend());

        assert!(text.contains("cmux: socket broken; restart cmux"));
        assert!(!text.contains("Broken pipe"));
        assert!(!text.contains("select-workspace"));
    }
}
