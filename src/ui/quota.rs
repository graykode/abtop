use crate::app::App;
use crate::model::RateLimitInfo;
use crate::theme::Theme;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use super::{btop_block, fmt_tokens, grad_at, make_gradient, remaining_bar, styled_label};

/// Data considered "stale" when its updated_at is older than this many seconds.
const STALE_SECS: u64 = 600;

/// Fixed source order so columns stay stable across runs.
const SOURCES: &[&str] = &["claude", "codex"];

pub(crate) fn draw_quota_panel(f: &mut Frame, app: &App, area: Rect, theme: &Theme) {
    let cpu_grad = make_gradient(theme.cpu_grad.start, theme.cpu_grad.mid, theme.cpu_grad.end);

    let block = btop_block("quota", "²", theme.cpu_box, theme);
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

    // Split into side-by-side columns: one per known source (CLAUDE | CODEX).
    // Columns are always rendered so the panel layout stays stable even when a
    // source has no data yet.
    let num_sources = SOURCES.len() as u16;
    let col_w = inner.width / num_sources;
    let content_h = inner.height.saturating_sub(1); // reserve last row for totals

    for (i, source) in SOURCES.iter().enumerate() {
        let col_x = inner.x + (i as u16) * col_w;
        let this_w = if i as u16 == num_sources - 1 {
            inner.width - (i as u16) * col_w
        } else {
            col_w
        };
        let col_area = Rect { x: col_x, y: inner.y, width: this_w, height: content_h };

        let rl = app.rate_limits.iter().find(|r| r.source.eq_ignore_ascii_case(source));
        draw_source_column(f, col_area, source, rl, &cpu_grad, theme);
    }

    // Total tokens summary on last row (full width)
    let bottom_area = Rect {
        x: inner.x,
        y: inner.y + content_h,
        width: inner.width,
        height: 1,
    };
    f.render_widget(Paragraph::new(vec![Line::from(vec![
        Span::styled(format!(" {}", fmt_tokens(total_tokens)), Style::default().fg(theme.main_fg)),
        Span::styled(format!(" {}/min", fmt_tokens(tokens_per_min as u64)), Style::default().fg(theme.graph_text)),
    ])]), bottom_area);
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
        let hint = if source.eq_ignore_ascii_case("claude") {
            "  abtop --setup"
        } else {
            "  run codex once"
        };
        let lines = vec![
            Line::from(Span::styled(
                format!(" {}", source.to_uppercase()),
                Style::default().fg(theme.title).add_modifier(Modifier::BOLD),
            )),
            Line::from(Span::styled("  — no data", Style::default().fg(theme.inactive_fg))),
            Line::from(Span::styled(hint, Style::default().fg(theme.graph_text))),
        ];
        f.render_widget(Paragraph::new(lines), area);
        return;
    };

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let ago_secs = rl.updated_at.map(|ts| now.saturating_sub(ts));
    let is_stale = ago_secs.is_some_and(|s| s > STALE_SECS);

    let fresh_str = ago_secs.map(format_ago).unwrap_or_default();
    let fresh_color = if is_stale { theme.inactive_fg } else { theme.graph_text };

    let mut lines: Vec<Line> = Vec::new();
    let mut label_spans = vec![Span::styled(
        format!(" {}", rl.source.to_uppercase()),
        Style::default().fg(theme.title).add_modifier(Modifier::BOLD),
    )];
    if !fresh_str.is_empty() {
        label_spans.push(Span::styled(fresh_str, Style::default().fg(fresh_color)));
    }
    lines.push(Line::from(label_spans));

    if let Some(used_pct) = rl.five_hour_pct {
        let remaining = (100.0 - used_pct).clamp(0.0, 100.0);
        let reset = rl.five_hour_resets_at.map(format_reset_time).unwrap_or_default();
        let c = grad_at(cpu_grad, used_pct);
        let mut s = vec![styled_label(" 5h ", theme.graph_text)];
        s.extend(remaining_bar(remaining, bar_w, cpu_grad, theme.meter_bg));
        s.push(Span::styled(format!(" {:>3.0}%", remaining), Style::default().fg(c)));
        lines.push(Line::from(s));
        if !reset.is_empty() {
            lines.push(Line::from(Span::styled(format!("  {}", reset), Style::default().fg(theme.graph_text))));
        }
    }
    if let Some(used_pct) = rl.seven_day_pct {
        let remaining = (100.0 - used_pct).clamp(0.0, 100.0);
        let reset = rl.seven_day_resets_at.map(format_reset_time).unwrap_or_default();
        let c = grad_at(cpu_grad, used_pct);
        let mut s = vec![styled_label(" 7d ", theme.graph_text)];
        s.extend(remaining_bar(remaining, bar_w, cpu_grad, theme.meter_bg));
        s.push(Span::styled(format!(" {:>3.0}%", remaining), Style::default().fg(c)));
        lines.push(Line::from(s));
        if !reset.is_empty() {
            lines.push(Line::from(Span::styled(format!("  {}", reset), Style::default().fg(theme.graph_text))));
        }
    }

    f.render_widget(Paragraph::new(lines), area);
}

/// Format an "N ago" badge with auto-scaling unit (s → m → h → d).
fn format_ago(secs: u64) -> String {
    if secs < 60 {
        format!(" {}s ago", secs)
    } else if secs < 3600 {
        format!(" {}m ago", secs / 60)
    } else if secs < 86400 {
        format!(" {}h ago", secs / 3600)
    } else {
        format!(" {}d ago", secs / 86400)
    }
}

/// Format a reset timestamp as relative time (e.g., "1h 23m")
pub(crate) fn format_reset_time(reset_ts: u64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    if reset_ts <= now {
        return "now".to_string();
    }
    let diff = reset_ts - now;
    if diff < 60 {
        format!("{}s", diff)
    } else if diff < 3600 {
        format!("{}m", diff / 60)
    } else if diff < 86400 {
        format!("{}h {}m", diff / 3600, (diff % 3600) / 60)
    } else {
        format!("{}d {}h", diff / 86400, (diff % 86400) / 3600)
    }
}
