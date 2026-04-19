use crate::app::App;
use crate::theme::Theme;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use super::{btop_block, fmt_tokens, grad_at, make_gradient, remaining_bar, styled_label};

pub(crate) fn draw_quota_panel(f: &mut Frame, app: &App, area: Rect, theme: &Theme) {
    let cpu_grad = make_gradient(theme.cpu_grad.start, theme.cpu_grad.mid, theme.cpu_grad.end);

    let block = btop_block("quota(left)", "²", theme.cpu_box, theme);
    f.render_widget(block, area);

    let inner = Rect {
        x: area.x + 1,
        y: area.y + 1,
        width: area.width.saturating_sub(2),
        height: area.height.saturating_sub(2),
    };

    let avail_h = inner.height as usize;

    // Bottom summary: total tokens + rate (real-token agents only — keep units
    // consistent across aggregation).
    let total_tokens: u64 = app.sessions.iter().filter(|s| s.reports_real_tokens()).map(|s| s.total_tokens()).sum();
    let rates = &app.token_rates;
    let ticks_per_min = 30usize;
    let tokens_per_min: f64 = rates.iter().rev().take(ticks_per_min).sum();
    if app.rate_limits.is_empty() {
        let mut lines: Vec<Line> = Vec::new();
        lines.push(Line::from(Span::styled(" QUOTA", Style::default().fg(theme.title).add_modifier(Modifier::BOLD))));
        lines.push(Line::from(Span::styled("  — unavailable", Style::default().fg(theme.inactive_fg))));
        lines.push(Line::from(Span::styled("  abtop --setup", Style::default().fg(theme.graph_text))));
        while lines.len() < avail_h.saturating_sub(1) {
            lines.push(Line::from(""));
        }
        lines.push(Line::from(vec![
            Span::styled(format!(" {}", fmt_tokens(total_tokens)), Style::default().fg(theme.main_fg)),
            Span::styled(format!(" {}/min", fmt_tokens(tokens_per_min as u64)), Style::default().fg(theme.graph_text)),
        ]));
        f.render_widget(Paragraph::new(lines), inner);
        return;
    }

    // Split into side-by-side columns: one per rate limit source (CLAUDE | CODEX)
    let num_sources = app.rate_limits.len().max(1) as u16;
    let col_w = inner.width / num_sources;
    let content_h = inner.height.saturating_sub(1); // reserve last row for totals

    for (i, rl) in app.rate_limits.iter().enumerate() {
        let col_x = inner.x + (i as u16) * col_w;
        let this_w = if i as u16 == num_sources - 1 {
            inner.width - (i as u16) * col_w
        } else {
            col_w
        };
        let col_area = Rect { x: col_x, y: inner.y, width: this_w, height: content_h };
        let col_w_usize = col_area.width as usize;
        let bar_w = col_w_usize.saturating_sub(10).clamp(2, 8);

        let mut lines: Vec<Line> = Vec::new();

        // Source label with freshness
        let fresh_str = rl.updated_at.map(|ts| {
            let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs();
            let ago = now.saturating_sub(ts);
            if ago < 60 { format!(" {}s ago", ago) } else { format!(" {}m ago", ago / 60) }
        }).unwrap_or_default();
        let label = format!(" {}{}", rl.source.to_uppercase(), fresh_str);
        lines.push(Line::from(Span::styled(label, Style::default().fg(theme.title).add_modifier(Modifier::BOLD))));

        if let Some(used_pct) = rl.five_hour_pct {
            let remaining = (100.0 - used_pct).clamp(0.0, 100.0);
            let reset = rl.five_hour_resets_at.map(format_reset_time).unwrap_or_default();
            // Color by urgency: low remaining = red (high used), high remaining = green
            let c = grad_at(&cpu_grad, used_pct);
            let mut s = vec![styled_label(" 5h ", theme.graph_text)];
            s.extend(remaining_bar(remaining, bar_w, &cpu_grad, theme.meter_bg));
            s.push(Span::styled(format!(" {:>3.0}%", remaining), Style::default().fg(c)));
            lines.push(Line::from(s));
            if !reset.is_empty() {
                lines.push(Line::from(Span::styled(format!("  {}", reset), Style::default().fg(theme.graph_text))));
            }
        }
        if let Some(used_pct) = rl.seven_day_pct {
            let remaining = (100.0 - used_pct).clamp(0.0, 100.0);
            let reset = rl.seven_day_resets_at.map(format_reset_time).unwrap_or_default();
            let c = grad_at(&cpu_grad, used_pct);
            let mut s = vec![styled_label(" 7d ", theme.graph_text)];
            s.extend(remaining_bar(remaining, bar_w, &cpu_grad, theme.meter_bg));
            s.push(Span::styled(format!(" {:>3.0}%", remaining), Style::default().fg(c)));
            lines.push(Line::from(s));
            if !reset.is_empty() {
                lines.push(Line::from(Span::styled(format!("  {}", reset), Style::default().fg(theme.graph_text))));
            }
        }

        f.render_widget(Paragraph::new(lines), col_area);
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
