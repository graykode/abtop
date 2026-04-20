use crate::app::App;
use crate::theme::Theme;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Cell, Paragraph, Row, Table};
use ratatui::Frame;

use super::{
    braille_graph_multirow, btop_block, fmt_tokens, grad_at, make_gradient, meter_bar,
    truncate_str,
};

pub(crate) fn draw_context_panel(f: &mut Frame, app: &App, area: Rect, theme: &Theme) {
    let cpu_grad = make_gradient(theme.cpu_grad.start, theme.cpu_grad.mid, theme.cpu_grad.end);

    let block = btop_block("context", "¹", theme.cpu_box, theme);
    f.render_widget(block, area);

    let inner = Rect {
        x: area.x + 1,
        y: area.y + 1,
        width: area.width.saturating_sub(2),
        height: area.height.saturating_sub(2),
    };

    // Compact mode: single-line text summary when too short for graph
    if inner.height <= 1 {
        let ticks_per_min = 30usize;
        let rates: Vec<f64> = app.token_rates.iter().copied().collect();
        let tokens_per_min: f64 = rates.iter().rev().take(ticks_per_min).sum();
        let total: u64 = app.sessions.iter().map(|s| s.total_tokens()).sum();
        let active = app.sessions.iter()
            .filter(|s| matches!(s.status, crate::model::SessionStatus::Working))
            .count();

        let line = Line::from(vec![
            Span::styled(" Rate ", Style::default().fg(theme.graph_text)),
            Span::styled(
                format!("{}/min", fmt_tokens(tokens_per_min as u64)),
                Style::default().fg(grad_at(&cpu_grad, 50.0)),
            ),
            Span::styled("  Total ", Style::default().fg(theme.graph_text)),
            Span::styled(fmt_tokens(total), Style::default().fg(theme.main_fg)),
            Span::styled(
                format!("  {} active", active),
                Style::default().fg(theme.proc_misc),
            ),
        ]);
        f.render_widget(Paragraph::new(line), inner);
        return;
    }

    // Full mode: sparkline graph + context bars
    let halves = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(65), Constraint::Percentage(35)])
        .split(inner);

    draw_context_sparkline(f, app, halves[0], &cpu_grad, theme);
    draw_context_bars(f, app, halves[1], &cpu_grad, theme);
}

fn draw_context_sparkline(f: &mut Frame, app: &App, area: Rect, cpu_grad: &[Color; 101], theme: &Theme) {
    let avail_h = area.height as usize;
    let avail_w = area.width as usize;
    let mut lines: Vec<Line> = Vec::new();

    let spark_w = avail_w.saturating_sub(2).max(4);
    let rates: Vec<f64> = app.token_rates.iter().copied().collect();
    let max_rate = rates.iter().cloned().fold(1.0_f64, f64::max);
    let normalized: Vec<f64> = rates.iter().map(|&v| v / max_rate).collect();

    // Graph title with current rate (btop-style)
    let ticks_per_min = 30usize;
    let tokens_per_min: f64 = rates.iter().rev().take(ticks_per_min).sum();
    let current_pct = normalized.last().copied().unwrap_or(0.0) * 100.0;
    let pct_color = grad_at(cpu_grad, current_pct);
    lines.push(Line::from(vec![
        Span::styled(" Token Rate", Style::default().fg(theme.graph_text)),
        Span::styled(
            format!("  {}/min", fmt_tokens(tokens_per_min as u64)),
            Style::default().fg(pct_color),
        ),
    ]));

    // Multi-row braille area graph (fills available height minus title + summary)
    let graph_h = avail_h.saturating_sub(2).max(1);
    let graph_rows = braille_graph_multirow(&normalized, spark_w, graph_h, cpu_grad, theme.graph_text);
    for row_spans in graph_rows {
        let mut line_spans = vec![Span::styled(" ", Style::default())];
        line_spans.extend(row_spans);
        lines.push(Line::from(line_spans));
    }

    // Summary line: total tokens
    let total_tokens: u64 = app.sessions.iter().map(|s| s.total_tokens()).sum();
    lines.push(Line::from(vec![
        Span::styled(format!(" {}", fmt_tokens(total_tokens)), Style::default().fg(theme.main_fg)),
        Span::styled(" total", Style::default().fg(theme.graph_text)),
    ]));

    f.render_widget(Paragraph::new(lines), area);
}

fn draw_context_bars(f: &mut Frame, app: &App, area: Rect, cpu_grad: &[Color; 101], theme: &Theme) {
    let header_style = Style::default().fg(theme.main_fg).add_modifier(Modifier::BOLD);

    // bar width = remaining space after Project(10) + pct(5) + info(10) + padding
    let bar_width = (area.width as usize).saturating_sub(30).clamp(4, 20);

    let mut rows = Vec::new();

    for session in &app.sessions {
        let raw_pct = session.context_percent;
        let bar_pct = raw_pct.min(100.0);
        let warn = if raw_pct >= 90.0 { "⚠" } else if raw_pct >= 75.0 { "!" } else { "" };
        let pct_color = grad_at(cpu_grad, bar_pct);

        // Context info: window size + compaction count (e.g. "200k C2")
        let ctx_info = match (session.context_window > 0, session.compaction_count) {
            (true, 0) => fmt_tokens(session.context_window),
            (true, n) => format!("{} C{}", fmt_tokens(session.context_window), n),
            (false, 0) => String::new(),
            (false, n) => format!("C{}", n),
        };

        rows.push(Row::new(vec![
            Cell::from(Span::styled(
                truncate_str(&session.project_name, 10),
                Style::default().fg(theme.title),
            )),
            Cell::from(Line::from({
                let mut spans = meter_bar(bar_pct, bar_width, cpu_grad, theme.meter_bg);
                spans.push(Span::styled(
                    format!(" {:>3.0}%{}", raw_pct, warn),
                    Style::default().fg(pct_color),
                ));
                spans
            })),
            Cell::from(Span::styled(
                ctx_info,
                Style::default().fg(theme.graph_text),
            )),
        ]));
    }

    if app.sessions.is_empty() {
        rows.push(Row::new(vec![
            Cell::from(Span::styled(
                "no active sessions",
                Style::default().fg(theme.inactive_fg),
            )),
            Cell::from(""),
            Cell::from(""),
        ]));
    }

    let header = Row::new(vec![
        Cell::from(Span::styled("Project", header_style)),
        Cell::from(Span::styled("Context", header_style)),
        Cell::from(Span::styled("Window", header_style)),
    ]);

    let widths = [
        Constraint::Length(10),
        Constraint::Min(10),
        Constraint::Length(10),
    ];

    let table = Table::new(rows, widths).header(header);
    f.render_widget(table, area);
}
