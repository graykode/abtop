use crate::app::App;
use crate::locale::t;
use crate::theme::Theme;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use super::{btop_block_active, grad_at, make_gradient, truncate_str};

pub(crate) fn draw_projects_panel(f: &mut Frame, app: &App, area: Rect, theme: &Theme) {
    draw_projects_panel_active(f, app, area, theme, false);
}

pub(crate) fn draw_projects_panel_active(
    f: &mut Frame,
    app: &App,
    area: Rect,
    theme: &Theme,
    active: bool,
) {
    let mut lines = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let no_git = t("projects.no_git");
    let clean = t("projects.clean");
    let no_projects = t("projects.no_projects");

    for session in &app.sessions {
        if !seen.insert(&session.project_name) {
            continue;
        }
        lines.push(Line::from(vec![Span::styled(
            format!(" {}", truncate_str(&session.project_name, 14)),
            Style::default()
                .fg(theme.title)
                .add_modifier(Modifier::BOLD),
        )]));
        let branch = if session.git_branch.is_empty() {
            no_git.clone()
        } else {
            session.git_branch.clone()
        };
        let used_grad = make_gradient(
            theme.used_grad.start,
            theme.used_grad.mid,
            theme.used_grad.end,
        );
        let branch_color = if session.git_branch.is_empty() {
            theme.inactive_fg
        } else {
            theme.main_fg
        };
        let mut branch_spans = vec![
            Span::styled("   ", Style::default()),
            Span::styled(branch, Style::default().fg(branch_color)),
        ];
        if session.git_added > 0 || session.git_modified > 0 {
            branch_spans.push(Span::styled(" ", Style::default()));
            if session.git_added > 0 {
                branch_spans.push(Span::styled(
                    format!("+{}", session.git_added),
                    Style::default().fg(theme.proc_misc),
                ));
            }
            if session.git_modified > 0 {
                if session.git_added > 0 {
                    branch_spans.push(Span::styled(" ", Style::default()));
                }
                branch_spans.push(Span::styled(
                    format!("~{}", session.git_modified),
                    Style::default().fg(grad_at(&used_grad, 70.0)),
                ));
            }
        } else {
            branch_spans.push(Span::styled(
                format!(" {}", clean),
                Style::default().fg(theme.proc_misc),
            ));
        }
        lines.push(Line::from(branch_spans));
    }
    if lines.is_empty() {
        lines.push(Line::from(Span::styled(
            format!(" {}", no_projects),
            Style::default().fg(theme.inactive_fg),
        )));
    }

    let block = btop_block_active("projects", "⁴", theme.mem_box, theme, active);
    f.render_widget(Paragraph::new(lines).block(block), area);
}
