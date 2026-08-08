use crate::app::App;
use crate::locale::t;
use crate::theme::Theme;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use super::{btop_block_active, grad_at, make_gradient, truncate_str};

pub(crate) fn draw_models_panel(f: &mut Frame, app: &App, area: Rect, theme: &Theme) {
    draw_models_panel_active(f, app, area, theme, false);
}

pub(crate) fn draw_models_panel_active(
    f: &mut Frame,
    app: &App,
    area: Rect,
    theme: &Theme,
    active: bool,
) {
    let mut lines = Vec::new();
    let no_models = t("models.no_models");
    let running = t("models.running");
    let stopped = t("models.stopped");
    let default = t("models.default");
    let provider_label = t("models.provider");
    let ctx_label = t("models.context");
    let out_label = t("models.output");
    let source_label = t("models.source");

    let status = if app.factory_app_running {
        format!("● {}", running)
    } else {
        format!("○ {}", stopped)
    };
    lines.push(Line::from(vec![
        Span::styled(" ", Style::default()),
        Span::styled(status, Style::default().fg(theme.main_fg)),
    ]));

    if app.factory_models.is_empty() {
        lines.push(Line::from(Span::styled(
            format!(" {}", no_models),
            Style::default().fg(theme.inactive_fg),
        )));
    } else {
        let grad = make_gradient(
            theme.used_grad.start,
            theme.used_grad.mid,
            theme.used_grad.end,
        );
        for model in &app.factory_models {
            let name = if model.display_name.is_empty() {
                model.model.clone()
            } else {
                model.display_name.clone()
            };
            let mut spans = vec![Span::styled(
                format!(" {}", truncate_str(&name, 18)),
                Style::default()
                    .fg(theme.title)
                    .add_modifier(Modifier::BOLD),
            )];
            if model.is_default {
                spans.push(Span::styled(
                    format!(" {}", default),
                    Style::default().fg(theme.proc_misc),
                ));
            }
            lines.push(Line::from(spans));
            lines.push(Line::from(vec![
                Span::styled(
                    format!("   {}:{}", provider_label, model.provider),
                    Style::default().fg(theme.proc_misc),
                ),
                Span::styled(
                    format!(" {}:{}", ctx_label, model.max_context_limit),
                    Style::default().fg(theme.main_fg),
                ),
                Span::styled(
                    format!(" {}:{}", out_label, model.max_output_tokens),
                    Style::default().fg(grad_at(&grad, 60.0)),
                ),
                Span::styled(
                    format!(" {}:{}", source_label, model.source),
                    Style::default().fg(theme.inactive_fg),
                ),
            ]));
        }
    }

    if !app.factory_issues.is_empty() {
        let high_color = grad_at(
            &make_gradient(
                theme.used_grad.start,
                theme.used_grad.mid,
                theme.used_grad.end,
            ),
            100.0,
        );
        lines.push(Line::from(Span::styled("", Style::default())));
        for issue in &app.factory_issues {
            let color = match issue.severity {
                "high" => high_color,
                "medium" => theme.proc_misc,
                _ => theme.inactive_fg,
            };
            lines.push(Line::from(vec![
                Span::styled(
                    format!(" ⚠ {}", truncate_str(&issue.file, 12)),
                    Style::default().fg(color),
                ),
                Span::styled(
                    format!(
                        " {}",
                        truncate_str(&issue.message, area.width.saturating_sub(22) as usize)
                    ),
                    Style::default().fg(theme.main_fg),
                ),
            ]));
        }
    }

    let block = btop_block_active("models", "⁸", theme.mem_box, theme, active);
    f.render_widget(Paragraph::new(lines).block(block), area);
}
