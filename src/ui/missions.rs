use crate::app::App;
use crate::locale::t;
use crate::theme::Theme;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use super::{btop_block_active, truncate_str};

pub(crate) fn draw_missions_panel(f: &mut Frame, app: &App, area: Rect, theme: &Theme) {
    draw_missions_panel_active(f, app, area, theme, false);
}

pub(crate) fn draw_missions_panel_active(
    f: &mut Frame,
    app: &App,
    area: Rect,
    theme: &Theme,
    active: bool,
) {
    let mut lines = Vec::new();
    let no_missions = t("missions.no_missions");
    let model_label = t("missions.model");

    if app.factory_missions.is_empty() {
        lines.push(Line::from(Span::styled(
            format!(" {}", no_missions),
            Style::default().fg(theme.inactive_fg),
        )));
    } else {
        for mission in &app.factory_missions {
            lines.push(Line::from(vec![
                Span::styled(
                    format!(" {}", truncate_str(&mission.title, 22)),
                    Style::default()
                        .fg(theme.title)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!(" {}", mission.state),
                    Style::default().fg(theme.proc_misc),
                ),
            ]));
            let mut detail = Vec::new();
            if !mission.worker_model.is_empty() {
                detail.push(Span::styled(
                    format!(" {}:{}", model_label, mission.worker_model),
                    Style::default().fg(theme.main_fg),
                ));
            }
            let dir = mission.dir.clone();
            detail.push(Span::styled(
                format!(" {}", truncate_str(&dir, 12)),
                Style::default().fg(theme.inactive_fg),
            ));
            lines.push(Line::from(detail));
        }
    }

    let block = btop_block_active("missions", "⁹", theme.cpu_box, theme, active);
    f.render_widget(Paragraph::new(lines).block(block), area);
}
