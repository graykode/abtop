use crate::app::App;
use crate::theme::Theme;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use super::{
    braille_sparkline, btop_block, fmt_tokens_for_agent, grad_at, make_gradient, meter_bar,
    styled_label, truncate_str,
};

pub(crate) fn draw_tokens_panel(f: &mut Frame, app: &App, area: Rect, theme: &Theme) {
    let selected = app.sessions.get(app.selected);
    let total_in: u64 = selected.map(|s| s.total_input_tokens).unwrap_or(0);
    let total_out: u64 = selected.map(|s| s.total_output_tokens).unwrap_or(0);
    let cache_read: u64 = selected.map(|s| s.total_cache_read).unwrap_or(0);
    let cache_write: u64 = selected.map(|s| s.total_cache_create).unwrap_or(0);
    let total: u64 = total_in + total_out + cache_read + cache_write;
    let turns: u32 = selected.map(|s| s.turn_count).unwrap_or(0);
    let avg = if turns > 0 { total / turns as u64 } else { 0 };

    let free_grad = make_gradient(theme.free_grad.start, theme.free_grad.mid, theme.free_grad.end);
    let used_grad = make_gradient(theme.used_grad.start, theme.used_grad.mid, theme.used_grad.end);
    let cached_grad = make_gradient(theme.cached_grad.start, theme.cached_grad.mid, theme.cached_grad.end);
    let cpu_grad = make_gradient(theme.cpu_grad.start, theme.cpu_grad.mid, theme.cpu_grad.end);

    let bar_w = (area.width as usize).saturating_sub(20).clamp(5, 15);
    let agent = selected.map(|s| s.agent_cli).unwrap_or("");
    let is_kiro = agent == "kiro";

    // Per-turn sparkline (shared by both layouts — y-axis label differs).
    let all_history: Vec<u64> = selected.map(|s| s.token_history.clone()).unwrap_or_default();
    let spark_w = (area.width as usize).saturating_sub(16).clamp(5, 20);
    let max_val = all_history.iter().copied().max().unwrap_or(1).max(1);
    let normalized: Vec<f64> = all_history.iter().map(|&v| v as f64 / max_val as f64).collect();
    let spark_label = if is_kiro { " credits/turn" } else { " tokens/turn" };
    let mut spark_line_spans = vec![styled_label(" ", theme.graph_text)];
    spark_line_spans.extend(braille_sparkline(&normalized, spark_w, &cpu_grad, theme.graph_text));
    spark_line_spans.push(Span::styled(spark_label, Style::default().fg(theme.graph_text)));

    let lines = if is_kiro {
        // Kiro: credits are quota units, not session tokens. Show credits honestly,
        // give context% visual prominence, skip the Input/Output/CacheR/CacheW
        // breakdown (Kiro doesn't expose that split).
        let ctx_pct = selected.map(|s| s.context_percent).unwrap_or(0.0);
        let ctx_color = grad_at(&cpu_grad, ctx_pct.min(100.0));
        let mut ctx_line = vec![styled_label(" Context:", theme.graph_text)];
        ctx_line.extend(meter_bar(ctx_pct.min(100.0), bar_w, &cpu_grad, theme.meter_bg));
        ctx_line.push(Span::styled(
            format!(" {:>3.0}%", ctx_pct),
            Style::default().fg(ctx_color),
        ));

        vec![
            Line::from(vec![
                styled_label(" Credits: ", theme.graph_text),
                Span::styled(
                    fmt_tokens_for_agent(total, agent),
                    Style::default().fg(theme.title).add_modifier(Modifier::BOLD),
                ),
            ]),
            Line::from(vec![
                styled_label(" Turns: ", theme.graph_text),
                Span::styled(format!("{}", turns), Style::default().fg(theme.main_fg)),
                styled_label("  Avg: ", theme.graph_text),
                Span::styled(
                    format!("{}/t", fmt_tokens_for_agent(avg, agent)),
                    Style::default().fg(theme.graph_text),
                ),
            ]),
            Line::from(ctx_line),
            Line::from(spark_line_spans),
        ]
    } else {
        // Claude/Codex: real tokens with input/output/cache breakdown.
        let (in_pct, out_pct, cache_r_pct, cache_w_pct) = if total > 0 {
            (
                total_in as f64 / total as f64 * 100.0,
                total_out as f64 / total as f64 * 100.0,
                cache_read as f64 / total as f64 * 100.0,
                cache_write as f64 / total as f64 * 100.0,
            )
        } else {
            (0.0, 0.0, 0.0, 0.0)
        };

        let total_line = vec![
            styled_label(" Total: ", theme.graph_text),
            Span::styled(
                fmt_tokens_for_agent(total, agent),
                Style::default().fg(theme.title).add_modifier(Modifier::BOLD),
            ),
        ];

        let mut input_line = vec![styled_label(" Input :", theme.graph_text)];
        input_line.extend(meter_bar(in_pct, bar_w, &free_grad, theme.meter_bg));
        input_line.push(Span::styled(
            format!(" {}", fmt_tokens_for_agent(total_in, agent)),
            Style::default().fg(grad_at(&free_grad, 80.0)),
        ));

        let mut output_line = vec![styled_label(" Output:", theme.graph_text)];
        output_line.extend(meter_bar(out_pct, bar_w, &used_grad, theme.meter_bg));
        output_line.push(Span::styled(
            format!(" {}", fmt_tokens_for_agent(total_out, agent)),
            Style::default().fg(grad_at(&used_grad, 80.0)),
        ));

        let mut cache_r_line = vec![styled_label(" CacheR:", theme.graph_text)];
        cache_r_line.extend(meter_bar(cache_r_pct, bar_w, &cached_grad, theme.meter_bg));
        cache_r_line.push(Span::styled(
            format!(" {}", fmt_tokens_for_agent(cache_read, agent)),
            Style::default().fg(grad_at(&cached_grad, 80.0)),
        ));

        let mut cache_w_line = vec![styled_label(" CacheW:", theme.graph_text)];
        cache_w_line.extend(meter_bar(cache_w_pct, bar_w, &cached_grad, theme.meter_bg));
        cache_w_line.push(Span::styled(
            format!(" {}", fmt_tokens_for_agent(cache_write, agent)),
            Style::default().fg(grad_at(&cached_grad, 80.0)),
        ));

        vec![
            Line::from(total_line),
            Line::from(input_line),
            Line::from(output_line),
            Line::from(cache_r_line),
            Line::from(cache_w_line),
            Line::from(spark_line_spans),
            Line::from(vec![
                styled_label(" Turns: ", theme.graph_text),
                Span::styled(format!("{}", turns), Style::default().fg(theme.main_fg)),
                styled_label("  Avg: ", theme.graph_text),
                Span::styled(
                    format!("{}/t", fmt_tokens_for_agent(avg, agent)),
                    Style::default().fg(theme.graph_text),
                ),
            ]),
        ]
    };

    let panel_title = if let Some(s) = selected {
        format!("tokens ({}/{})", truncate_str(&s.project_name, 12), truncate_str(&s.session_id, 8))
    } else {
        "tokens".to_string()
    };
    let block = btop_block(&panel_title, "³", theme.mem_box, theme);
    f.render_widget(Paragraph::new(lines).block(block), area);
}
