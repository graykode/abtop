use crate::app::App;
use crate::theme::Theme;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

pub(crate) fn draw_header(f: &mut Frame, app: &App, area: Rect, theme: &Theme) {
    let session_count = app.sessions.len();
    let active = app
        .sessions
        .iter()
        .filter(|s| s.status.is_active())
        .count();

    let now = chrono::Local::now().format("%H:%M").to_string();
    let version = env!("CARGO_PKG_VERSION");
    // " abtop vX.Y.Z " + "─" + " agent monitor " + right side (~12)
    let header_fixed = format!(" abtop v{version} ").len() + 1 + 15 + 12;
    let remaining = (area.width as usize).saturating_sub(header_fixed);
    let line = Line::from(vec![
        Span::styled(format!(" abtop v{version} "), Style::default().fg(theme.title).add_modifier(Modifier::BOLD)),
        Span::styled("─", Style::default().fg(theme.div_line)),
        Span::styled(" agent monitor ", Style::default().fg(theme.graph_text)),
        Span::styled(
            format!("{:>width$}", now, width = remaining),
            Style::default().fg(theme.graph_text),
        ),
        Span::styled(format!("  {}↑", active), Style::default().fg(theme.proc_misc)),
        Span::styled(format!(" {}●", session_count), Style::default().fg(theme.main_fg)),
        Span::styled("  ", Style::default()),
    ]);
    f.render_widget(Paragraph::new(line), area);
}
