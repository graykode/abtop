use crate::app::App;
use crate::locale::t;
use crate::model::RateLimitInfo;
use crate::theme::Theme;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use super::{btop_block_active, fmt_tokens, grad_at, make_gradient, remaining_bar, styled_label};

/// Data considered "stale" when its updated_at is older than this many seconds.
const STALE_SECS: u64 = 600;

/// Fixed source order so columns stay stable across runs.
const SOURCES: &[&str] = &["claude", "codex"];

pub(crate) fn draw_quota_panel(f: &mut Frame, app: &App, area: Rect, theme: &Theme) {
    draw_quota_panel_active(f, app, area, theme, false);
}

pub(crate) fn draw_quota_panel_active(
    f: &mut Frame,
    app: &App,
    area: Rect,
    theme: &Theme,
    active: bool,
) {
    let cpu_grad = make_gradient(theme.cpu_grad.start, theme.cpu_grad.mid, theme.cpu_grad.end);

    let block = btop_block_active("quota", "²", theme.cpu_box, theme, active);
    f.render_widget(block, area);

    let inner = Rect {
        x: area.x + 1,
        y: area.y + 1,
        width: area.width.saturating_sub(2),
        height: area.height.saturating_sub(2),
    };

    // Bottom summary: total tokens + rate
    let total_tokens: u64 = app.sessions.iter().map(|s| s.total_tokens()).sum();
    let rates = &app.token_rates;
    let ticks_per_min = 30usize;
    let tokens_per_min: f64 = rates.iter().rev().take(ticks_per_min).sum();

    // Split into side-by-side columns for active sources. When a workspace is
    // Codex-only, give Codex the full quota panel instead of spending half the
    // space on an empty Claude column.
    let sources = active_quota_sources(app);
    let num_sources = sources.len() as u16;
    let col_w = inner.width / num_sources;
    let content_h = inner.height.saturating_sub(1); // reserve last row for totals

    for (i, source) in sources.iter().enumerate() {
        let col_x = inner.x + (i as u16) * col_w;
        let this_w = if i as u16 == num_sources - 1 {
            inner.width - (i as u16) * col_w
        } else {
            col_w
        };
        let col_area = Rect {
            x: col_x,
            y: inner.y,
            width: this_w,
            height: content_h,
        };

        let rl = app
            .rate_limits
            .iter()
            .find(|r| r.source.eq_ignore_ascii_case(source));
        draw_source_column(f, col_area, source, rl, &cpu_grad, theme);
    }

    // Total tokens summary on last row (full width)
    let bottom_area = Rect {
        x: inner.x,
        y: inner.y + content_h,
        width: inner.width,
        height: 1,
    };
    let total_label = t("quota.total");
    f.render_widget(
        Paragraph::new(vec![Line::from(vec![
            Span::styled(
                format!(" {} {}", total_label, fmt_tokens(total_tokens)),
                Style::default().fg(theme.main_fg),
            ),
            Span::styled(
                format!(" {}/min", fmt_tokens(tokens_per_min as u64)),
                Style::default().fg(theme.graph_text),
            ),
        ])]),
        bottom_area,
    );
}

fn draw_source_column(
    f: &mut Frame,
    area: Rect,
    source: &str,
    rl: Option<&RateLimitInfo>,
    cpu_grad: &[ratatui::style::Color; 101],
    theme: &Theme,
) {
    let col_w_usize = area.width as usize;
    let bar_w = col_w_usize.saturating_sub(10).clamp(2, 8);

    let Some(rl) = rl else {
        let hint = if source.eq_ignore_ascii_case("codex") {
            t("quota.codex_wait")
        } else if source.eq_ignore_ascii_case("claude") {
            t("quota.claude_wait")
        } else {
            t("quota.no_data")
        };
        let lines = vec![
            Line::from(Span::styled(
                format!(" {}", source.to_uppercase()),
                Style::default()
                    .fg(theme.title)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(Span::styled(
                format!("  — {}", t("quota.usage_unknown")),
                Style::default().fg(theme.inactive_fg),
            )),
            Line::from(Span::styled(
                format!("  {}", hint),
                Style::default().fg(theme.graph_text),
            )),
        ];
        f.render_widget(Paragraph::new(lines), area);
        return;
    };

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let is_stale = rl
        .updated_at
        .is_some_and(|ts| now.saturating_sub(ts) > STALE_SECS);

    // Stale data → dim the source name. Drops the unlabeled "Xs ago" row
    // we used to render here; keeping a distinct freshness color but no
    // explicit number is enough to signal "values may be out of date"
    // without competing with the reset countdown for the user's attention.
    let source_color = if is_stale {
        theme.inactive_fg
    } else {
        theme.title
    };

    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::from(Span::styled(
        format!(" {}", rl.source.to_uppercase()),
        Style::default()
            .fg(source_color)
            .add_modifier(Modifier::BOLD),
    )));

    // Reset countdown is only meaningful when the data is fresh enough
    // that the reported `resets_at` is still in the future. Stale sources
    // get the bar (the % is approximately correct) but no countdown row.
    let show_reset = !is_stale;

    if let Some(used_pct) = rl.five_hour_pct {
        let remaining = (100.0 - used_pct).clamp(0.0, 100.0);
        let detail = if show_reset {
            format_quota_detail(
                rl.five_hour_resets_at,
                rl.five_hour_burn_pct_per_hour,
                rl.five_hour_eta_secs,
                now,
            )
        } else {
            String::new()
        };
        let c = grad_at(cpu_grad, used_pct);
        let label_5h = t("quota.5h");
        let mut s = vec![styled_label(
            format!(" {}", label_5h).as_str(),
            theme.graph_text,
        )];
        s.extend(remaining_bar(remaining, bar_w, cpu_grad, theme.meter_bg));
        s.push(Span::styled(
            format!(" {:>3.0}%", remaining),
            Style::default().fg(c),
        ));
        lines.push(Line::from(s));
        // Always reserve the row so both columns line up vertically;
        // when there's nothing meaningful to show (stale source or the
        // cached reset moment is past), render it blank.
        lines.push(Line::from(Span::styled(
            if detail.is_empty() {
                String::new()
            } else {
                format!("  {}", detail)
            },
            Style::default().fg(theme.graph_text),
        )));
    }
    if let Some(used_pct) = rl.seven_day_pct {
        let remaining = (100.0 - used_pct).clamp(0.0, 100.0);
        let detail = if show_reset {
            format_quota_detail(
                rl.seven_day_resets_at,
                rl.seven_day_burn_pct_per_hour,
                rl.seven_day_eta_secs,
                now,
            )
        } else {
            String::new()
        };
        let c = grad_at(cpu_grad, used_pct);
        let label_7d = t("quota.7d");
        let mut s = vec![styled_label(
            format!(" {}", label_7d).as_str(),
            theme.graph_text,
        )];
        s.extend(remaining_bar(remaining, bar_w, cpu_grad, theme.meter_bg));
        s.push(Span::styled(
            format!(" {:>3.0}%", remaining),
            Style::default().fg(c),
        ));
        lines.push(Line::from(s));
        // Always reserve the row so both columns line up vertically;
        // when there's nothing meaningful to show (stale source or the
        // cached reset moment is past), render it blank.
        lines.push(Line::from(Span::styled(
            if detail.is_empty() {
                String::new()
            } else {
                format!("  {}", detail)
            },
            Style::default().fg(theme.graph_text),
        )));
    }

    f.render_widget(Paragraph::new(lines), area);
}

fn active_quota_sources(app: &App) -> Vec<&'static str> {
    let active: Vec<&'static str> = SOURCES
        .iter()
        .copied()
        .filter(|source| {
            app.sessions
                .iter()
                .any(|s| s.agent_cli.eq_ignore_ascii_case(source))
                || app
                    .rate_limits
                    .iter()
                    .any(|r| r.source.eq_ignore_ascii_case(source))
        })
        .collect();

    if active.is_empty() {
        SOURCES.to_vec()
    } else {
        active
    }
}

fn format_reset_time_at(reset_ts: u64, now: u64) -> String {
    if reset_ts <= now {
        return String::new();
    }
    let diff = reset_ts - now;
    let prefix = t("quota.in");
    if diff < 60 {
        format!("{} {}{}", prefix, diff, t("time.s"))
    } else if diff < 3600 {
        format!("{} {}{}", prefix, diff / 60, t("time.m"))
    } else if diff < 86400 {
        let h = diff / 3600;
        let m = (diff % 3600) / 60;
        format!("{} {}{} {}{}", prefix, h, t("time.h"), m, t("time.m"))
    } else {
        let d = diff / 86400;
        let h = (diff % 86400) / 3600;
        format!("{} {}{} {}{}", prefix, d, t("time.d"), h, t("time.h"))
    }
}

fn format_quota_detail(
    reset_ts: Option<u64>,
    burn_pct_per_hour: Option<f64>,
    eta_secs: Option<u64>,
    now: u64,
) -> String {
    let reset = reset_ts
        .map(|ts| format_reset_time_at(ts, now))
        .unwrap_or_default();
    let Some(burn) = burn_pct_per_hour.filter(|burn| *burn >= 0.05) else {
        return reset;
    };
    let burn = format_burn_rate(burn);

    if let (Some(eta), Some(reset_secs)) = (eta_secs, reset_ts.and_then(|ts| ts.checked_sub(now))) {
        if eta < reset_secs {
            return format!("{} {} {}", t("quota.cap"), format_duration_short(eta), burn);
        }
    }

    if reset.is_empty() {
        burn
    } else {
        format!("{} {}", reset, burn)
    }
}

fn format_burn_rate(burn_pct_per_hour: f64) -> String {
    if burn_pct_per_hour >= 10.0 {
        format!("+{:.0}%/h", burn_pct_per_hour)
    } else {
        format!("+{:.1}%/h", burn_pct_per_hour)
    }
}

fn format_duration_short(secs: u64) -> String {
    if secs < 60 {
        format!("{}{}", secs, t("time.s"))
    } else if secs < 3600 {
        format!("{}{}", secs / 60, t("time.m"))
    } else if secs < 86400 {
        let h = secs / 3600;
        let m = (secs % 3600) / 60;
        if m == 0 {
            format!("{}{}", h, t("time.h"))
        } else {
            format!("{}{} {}{}", h, t("time.h"), m, t("time.m"))
        }
    } else {
        let d = secs / 86400;
        let h = (secs % 86400) / 3600;
        if h == 0 {
            format!("{}{}", d, t("time.d"))
        } else {
            format!("{}{} {}{}", d, t("time.d"), h, t("time.h"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::PanelVisibility;
    use crate::model::{AgentSession, SessionStatus};

    fn test_app() -> App {
        App::new_with_config(Theme::default(), &[], PanelVisibility::default())
    }

    fn test_session(agent_cli: &'static str) -> AgentSession {
        AgentSession {
            agent_cli,
            pid: 1,
            session_id: String::new(),
            cwd: String::new(),
            project_name: String::new(),
            started_at: 0,
            status: SessionStatus::Waiting,
            model: String::new(),
            effort: String::new(),
            context_percent: 0.0,
            total_input_tokens: 0,
            total_output_tokens: 0,
            total_cache_read: 0,
            total_cache_create: 0,
            turn_count: 0,
            current_tasks: Vec::new(),
            mem_mb: 0,
            version: String::new(),
            git_branch: String::new(),
            git_added: 0,
            git_modified: 0,
            token_history: Vec::new(),
            context_history: Vec::new(),
            compaction_count: 0,
            context_window: 0,
            subagents: Vec::new(),
            mem_file_count: 0,
            mem_line_count: 0,
            children: Vec::new(),
            initial_prompt: String::new(),
            first_assistant_text: String::new(),
            chat_messages: Vec::new(),
            tool_calls: Vec::new(),
            pending_since_ms: 0,
            thinking_since_ms: 0,
            file_accesses: Vec::new(),
            config_root: String::new(),
        }
    }

    #[test]
    fn quota_sources_focus_codex_only_sessions() {
        let mut app = test_app();
        app.sessions.push(test_session("codex"));

        assert_eq!(active_quota_sources(&app), vec!["codex"]);
    }

    #[test]
    fn quota_sources_default_when_runtime_is_empty() {
        let app = test_app();

        assert_eq!(active_quota_sources(&app), vec!["claude", "codex"]);
    }

    #[test]
    fn quota_detail_warns_when_cap_arrives_before_reset() {
        let detail = format_quota_detail(Some(10_000), Some(50.0), Some(3_600), 1_000);

        assert_eq!(detail, "cap 1h +50%/h");
    }

    #[test]
    fn quota_detail_keeps_reset_when_reset_arrives_first() {
        let detail = format_quota_detail(Some(4_600), Some(10.0), Some(7_200), 1_000);

        assert_eq!(detail, "in 1h 0m +10%/h");
    }
}
