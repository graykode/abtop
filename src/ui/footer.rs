use crate::app::App;
use crate::theme::Theme;
use chrono::Timelike;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

pub(crate) fn draw_footer(f: &mut Frame, app: &App, area: Rect, theme: &Theme) {
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
    spans.push(Span::styled("q", Style::default().fg(theme.hi_fg)));
    spans.push(Span::styled(" quit ", Style::default().fg(theme.main_fg)));
    spans.push(Span::styled("r", Style::default().fg(theme.hi_fg)));
    spans.push(Span::styled(" refresh ", Style::default().fg(theme.main_fg)));
    spans.push(Span::styled("t", Style::default().fg(theme.hi_fg)));
    spans.push(Span::styled(" theme ", Style::default().fg(theme.main_fg)));
    spans.push(Span::styled("T", Style::default().fg(theme.hi_fg)));
    spans.push(Span::styled(" tree ", Style::default().fg(theme.main_fg)));

    // Show transient status message or default "2s auto"
    let status_text = app.status_msg.as_ref()
        .filter(|(_, when)| when.elapsed().as_secs() < 3)
        .map(|(msg, _)| msg.as_str());
    if let Some(msg) = status_text {
        spans.push(Span::styled(format!(" {msg} "), Style::default().fg(theme.status_fg)));
    } else {
        spans.push(Span::styled("2s auto", Style::default().fg(theme.inactive_fg)));
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

    let used: usize = spans.iter().map(|s| s.content.len()).sum();
    let remaining = (area.width as usize).saturating_sub(used + 2);
    spans.push(Span::styled(
        format!("{:>width$}", format!("{} sessions", app.sessions.len()), width = remaining),
        Style::default().fg(theme.graph_text),
    ));

    f.render_widget(Paragraph::new(Line::from(spans)), area);
}
