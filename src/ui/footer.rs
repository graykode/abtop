use crate::app::App;
use crate::locale::t;
use crate::theme::Theme;
use chrono::Timelike;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use super::truncate_str;

pub(crate) fn draw_footer(f: &mut Frame, app: &App, area: Rect, theme: &Theme) {
    // Filter input mode: show filter bar instead of normal keybindings
    if app.filter_active {
        let visible_count = app.visible_indices().len();
        let count = format!(
            "{}/{} {}",
            visible_count,
            app.sessions.len(),
            t("footer.sessions")
        );
        let suffix = if area.width <= 80 {
            format!("_  {}", count)
        } else {
            format!(
                "_  {}  (Esc {}, Enter {})",
                count,
                t("footer.esc_clear")
                    .split(',')
                    .next()
                    .unwrap_or(&t("footer.esc_clear")),
                t("footer.esc_clear")
                    .split(',')
                    .nth(1)
                    .unwrap_or("keep")
                    .trim()
            )
        };
        let filter_w = (area.width as usize).saturating_sub(2 + suffix.chars().count());
        let spans = vec![
            Span::styled(" /", Style::default().fg(theme.hi_fg)),
            Span::styled(
                truncate_str(&app.filter_text, filter_w),
                Style::default().fg(theme.title),
            ),
            Span::styled(suffix, Style::default().fg(theme.inactive_fg)),
        ];
        f.render_widget(Paragraph::new(Line::from(spans)), area);
        return;
    }

    let compact = area.width <= 80;
    let ultra_compact = area.width <= 70;

    let mut spans = vec![
        Span::styled(" ↑↓", Style::default().fg(theme.hi_fg)),
        Span::styled(
            format!(" {} ", t("footer.select")),
            Style::default().fg(theme.main_fg),
        ),
    ];
    if !ultra_compact {
        spans.push(Span::styled("↵", Style::default().fg(theme.hi_fg)));
        spans.push(Span::styled(
            format!(" {} ", t("footer.jump")),
            Style::default().fg(theme.main_fg),
        ));
    }
    if compact {
        spans.push(Span::styled("←→", Style::default().fg(theme.hi_fg)));
        spans.push(Span::styled(" tabs ", Style::default().fg(theme.main_fg)));
    }
    if !ultra_compact {
        spans.push(Span::styled("x", Style::default().fg(theme.hi_fg)));
        spans.push(Span::styled(
            format!(" {} ", t("footer.kill")),
            Style::default().fg(theme.main_fg),
        ));
    }
    spans.push(Span::styled("/", Style::default().fg(theme.hi_fg)));
    spans.push(Span::styled(
        format!(" {} ", t("footer.filter")),
        Style::default().fg(theme.main_fg),
    ));
    if !ultra_compact {
        spans.push(Span::styled("v", Style::default().fg(theme.hi_fg)));
        spans.push(Span::styled(
            format!(" {} ", t("footer.view")),
            Style::default().fg(theme.main_fg),
        ));
        if !compact {
            spans.push(Span::styled("c", Style::default().fg(theme.hi_fg)));
            spans.push(Span::styled(
                format!(" {} ", t("footer.config")),
                Style::default().fg(theme.main_fg),
            ));
        }
        spans.push(Span::styled("?", Style::default().fg(theme.hi_fg)));
        spans.push(Span::styled(
            format!(" {} ", t("footer.help")),
            Style::default().fg(theme.main_fg),
        ));
    }
    spans.push(Span::styled("q", Style::default().fg(theme.hi_fg)));
    spans.push(Span::styled(
        format!(" {} ", t("footer.quit")),
        Style::default().fg(theme.main_fg),
    ));

    // Show active filter or transient status
    if !compact && !app.filter_text.is_empty() {
        spans.push(Span::styled(
            format!(" /{} ", app.filter_text),
            Style::default().fg(theme.status_fg),
        ));
    } else if !compact {
        let status_text = app
            .status_msg
            .as_ref()
            .filter(|(_, when)| when.elapsed().as_secs() < 3)
            .map(|(msg, _)| msg.as_str());
        if let Some(msg) = status_text {
            spans.push(Span::styled(
                format!(" {msg} "),
                Style::default().fg(theme.status_fg),
            ));
        } else {
            spans.push(Span::styled(
                t("footer.auto"),
                Style::default().fg(theme.inactive_fg),
            ));
        }
    }

    // Peak hours warning: US business hours = PT 5am–11am = UTC 12:00–18:00
    let peak_info = {
        let now = chrono::Utc::now();
        let hour = now.hour();
        if (12..18).contains(&hour) {
            let mins_left = (18 - hour) * 60 - now.minute();
            let h = mins_left / 60;
            let m = mins_left % 60;
            let peak_label = t("footer.peak_hours");
            let resets_in = t("footer.resets_in");
            Some(format!("⚡{} ({} {}h{:02}m)", peak_label, resets_in, h, m))
        } else {
            None
        }
    };
    if let Some(ref peak) = peak_info.filter(|_| !compact) {
        spans.push(Span::styled(
            format!(" {peak} "),
            Style::default().fg(theme.warning_fg),
        ));
    }

    let visible_count = app.visible_indices().len();
    let sessions_label = t("footer.sessions");
    let count_label = if visible_count < app.sessions.len() {
        format!(
            "{}/{} {}",
            visible_count,
            app.sessions.len(),
            sessions_label
        )
    } else {
        format!("{} {}", app.sessions.len(), sessions_label)
    };
    let used: usize = spans.iter().map(|s| s.content.chars().count()).sum();
    let count_w = count_label.chars().count();
    if used + count_w < area.width as usize {
        let remaining = (area.width as usize).saturating_sub(used + count_w);
        spans.push(Span::styled(
            format!("{:>width$}", count_label, width = remaining),
            Style::default().fg(theme.graph_text),
        ));
    }

    f.render_widget(Paragraph::new(Line::from(spans)), area);
}
