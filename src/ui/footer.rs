use crate::app::App;
use crate::theme::Theme;
use chrono::Timelike;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

pub(crate) fn draw_footer(f: &mut Frame, app: &App, area: Rect, theme: &Theme) {
    // Filter input mode: show filter bar instead of normal keybindings
    if app.filter_active {
        let visible_count = app.visible_indices().len();
        let spans = vec![
            Span::styled(" /", Style::default().fg(theme.hi_fg)),
            Span::styled(&app.filter_text, Style::default().fg(theme.title)),
            Span::styled("_", Style::default().fg(theme.hi_fg)),
            Span::styled(
                format!("  {}/{} sessions  (Esc clear, Enter keep)",
                    visible_count, app.sessions.len()),
                Style::default().fg(theme.inactive_fg),
            ),
        ];
        f.render_widget(Paragraph::new(Line::from(spans)), area);
        return;
    }

    let has_tmux = std::env::var("TMUX").is_ok();

    let mut spans = vec![
        Span::styled(" ↑↓", Style::default().fg(theme.hi_fg)),
        Span::styled(" select ", Style::default().fg(theme.main_fg)),
    ];
    if has_tmux {
        spans.push(Span::styled("↵", Style::default().fg(theme.hi_fg)));
        spans.push(Span::styled(" jump ", Style::default().fg(theme.main_fg)));
    }
    spans.push(Span::styled("x", Style::default().fg(theme.hi_fg)));
    spans.push(Span::styled(" kill ", Style::default().fg(theme.main_fg)));
    spans.push(Span::styled("/", Style::default().fg(theme.hi_fg)));
    spans.push(Span::styled(" filter ", Style::default().fg(theme.main_fg)));
    spans.push(Span::styled("q", Style::default().fg(theme.hi_fg)));
    spans.push(Span::styled(" quit ", Style::default().fg(theme.main_fg)));
    spans.push(Span::styled("t", Style::default().fg(theme.hi_fg)));
    spans.push(Span::styled(" theme ", Style::default().fg(theme.main_fg)));
    spans.push(Span::styled("T", Style::default().fg(theme.hi_fg)));
    let tree_label = if app.tree_view { " tree* " } else { " tree " };
    spans.push(Span::styled(tree_label, Style::default().fg(theme.main_fg)));
    spans.push(Span::styled("1-5", Style::default().fg(theme.hi_fg)));
    spans.push(Span::styled(" panels ", Style::default().fg(theme.main_fg)));
    spans.push(Span::styled("c", Style::default().fg(theme.hi_fg)));
    spans.push(Span::styled(" config ", Style::default().fg(theme.main_fg)));

    // Show active filter or transient status
    if !app.filter_text.is_empty() {
        spans.push(Span::styled(
            format!(" /{} ", app.filter_text),
            Style::default().fg(theme.status_fg),
        ));
    } else {
        let status_text = app.status_msg.as_ref()
            .filter(|(_, when)| when.elapsed().as_secs() < 3)
            .map(|(msg, _)| msg.as_str());
        if let Some(msg) = status_text {
            spans.push(Span::styled(format!(" {msg} "), Style::default().fg(theme.status_fg)));
        } else {
            spans.push(Span::styled("2s auto", Style::default().fg(theme.inactive_fg)));
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
            Some(format!("⚡Claude Peak Hours (resets in {}h{:02}m)", h, m))
        } else {
            None
        }
    };
    if let Some(ref peak) = peak_info {
        spans.push(Span::styled(format!(" {peak} "), Style::default().fg(theme.warning_fg)));
    }

    let visible_count = app.visible_indices().len();
    let count_label = if visible_count < app.sessions.len() {
        format!("{}/{} sessions", visible_count, app.sessions.len())
    } else {
        format!("{} sessions", app.sessions.len())
    };
    let used: usize = spans.iter().map(|s| s.content.len()).sum();
    let remaining = (area.width as usize).saturating_sub(used + 2);
    spans.push(Span::styled(
        format!("{:>width$}", count_label, width = remaining),
        Style::default().fg(theme.graph_text),
    ));

    f.render_widget(Paragraph::new(Line::from(spans)), area);
}
