use crate::app::App;
use crate::theme::Theme;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use super::{btop_block_active, fmt_tokens, grad_at, make_gradient, truncate_str};

pub(crate) fn draw_workspace_panel_active(
    f: &mut Frame,
    app: &App,
    area: Rect,
    theme: &Theme,
    active: bool,
) {
    let mut lines = Vec::new();
    let active_sessions = app.sessions.iter().filter(|s| s.status.is_active()).count();
    let waiting_sessions = app
        .sessions
        .iter()
        .filter(|s| matches!(s.status, crate::model::SessionStatus::Waiting))
        .count();
    let blocked_sessions = app
        .sessions
        .iter()
        .filter(|s| matches!(s.status, crate::model::SessionStatus::RateLimited))
        .count();

    lines.push(Line::from(vec![
        Span::styled(" projects ", Style::default().fg(theme.graph_text)),
        Span::styled(
            app.workspace_projects.len().to_string(),
            Style::default()
                .fg(theme.title)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled("  agents ", Style::default().fg(theme.graph_text)),
        Span::styled(
            app.sessions.len().to_string(),
            Style::default()
                .fg(theme.title)
                .add_modifier(Modifier::BOLD),
        ),
    ]));
    lines.push(Line::from(vec![
        Span::styled(" active ", Style::default().fg(theme.graph_text)),
        Span::styled(
            active_sessions.to_string(),
            Style::default().fg(theme.proc_misc),
        ),
        Span::styled("  wait ", Style::default().fg(theme.graph_text)),
        Span::styled(
            waiting_sessions.to_string(),
            Style::default().fg(theme.main_fg),
        ),
        Span::styled("  blocked ", Style::default().fg(theme.graph_text)),
        Span::styled(
            blocked_sessions.to_string(),
            Style::default().fg(theme.warning_fg),
        ),
    ]));

    if !app.workspace_projects.is_empty() {
        lines.push(Line::from(""));
    }

    let used_grad = make_gradient(
        theme.used_grad.start,
        theme.used_grad.mid,
        theme.used_grad.end,
    );
    let project_rows = 3usize;
    let detail_rows = if app.workspace_projects.is_empty() {
        0
    } else {
        2
    };
    let available_rows = area
        .height
        .saturating_sub(5 + detail_rows as u16)
        .max(project_rows as u16) as usize;
    let max_projects = (available_rows / project_rows).max(1);
    let start = if app.workspace_selected >= max_projects {
        app.workspace_selected + 1 - max_projects
    } else {
        0
    };
    for (idx, project) in app
        .workspace_projects
        .iter()
        .enumerate()
        .skip(start)
        .take(max_projects)
    {
        let selected = idx == app.workspace_selected;
        let name_w = area.width.saturating_sub(22).clamp(8, 24) as usize;
        let dw = if project.has_dw { " dw" } else { "" };
        let name_style = if selected {
            Style::default()
                .fg(theme.selected_fg)
                .bg(theme.selected_bg)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default()
                .fg(theme.title)
                .add_modifier(Modifier::BOLD)
        };
        lines.push(Line::from(vec![
            Span::styled(
                if selected { ">" } else { " " },
                Style::default().fg(theme.hi_fg),
            ),
            Span::styled(
                format!(" {}", truncate_str(&project.name, name_w)),
                name_style,
            ),
            Span::styled(dw, Style::default().fg(theme.proc_misc)),
        ]));

        let mut status = vec![
            Span::styled("   A", Style::default().fg(theme.graph_text)),
            Span::styled(
                project.active_count.to_string(),
                Style::default().fg(theme.proc_misc),
            ),
            Span::styled(" W", Style::default().fg(theme.graph_text)),
            Span::styled(
                project.waiting_count.to_string(),
                Style::default().fg(theme.main_fg),
            ),
        ];
        if project.rate_limited_count > 0 {
            status.push(Span::styled(" B", Style::default().fg(theme.graph_text)));
            status.push(Span::styled(
                project.rate_limited_count.to_string(),
                Style::default().fg(theme.warning_fg),
            ));
        }
        status.push(Span::styled(" ctx ", Style::default().fg(theme.graph_text)));
        status.push(Span::styled(
            format!("{:.0}%", project.max_context_percent),
            Style::default().fg(grad_at(&used_grad, project.max_context_percent)),
        ));
        status.push(Span::styled(" tok ", Style::default().fg(theme.graph_text)));
        status.push(Span::styled(
            fmt_tokens(project.total_tokens),
            Style::default().fg(theme.main_fg),
        ));
        lines.push(Line::from(status));

        let mut flow = vec![
            Span::styled("   git ", Style::default().fg(theme.graph_text)),
            Span::styled(
                format!("+{} ~{}", project.git_added, project.git_modified),
                Style::default().fg(theme.proc_misc),
            ),
            Span::styled(" ports ", Style::default().fg(theme.graph_text)),
            Span::styled(
                project.port_count.to_string(),
                Style::default().fg(theme.main_fg),
            ),
        ];
        if project.has_dw {
            flow.push(Span::styled(
                " task ",
                Style::default().fg(theme.graph_text),
            ));
            flow.push(Span::styled(
                if project.has_active_task {
                    "active"
                } else {
                    "idle"
                },
                Style::default().fg(if project.has_active_task {
                    theme.proc_misc
                } else {
                    theme.inactive_fg
                }),
            ));
            flow.push(Span::styled(" adr ", Style::default().fg(theme.graph_text)));
            flow.push(Span::styled(
                project.decision_count.to_string(),
                Style::default().fg(theme.main_fg),
            ));
        }
        lines.push(Line::from(flow));
    }

    if let Some(project) = app.workspace_projects.get(app.workspace_selected) {
        lines.push(Line::from(""));
        lines.push(Line::from(vec![
            Span::styled(" selected ", Style::default().fg(theme.graph_text)),
            Span::styled(
                truncate_str(&project.name, 20),
                Style::default()
                    .fg(theme.title)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(" sessions ", Style::default().fg(theme.graph_text)),
            Span::styled(
                project.session_count.to_string(),
                Style::default().fg(theme.main_fg),
            ),
            Span::styled(" children ", Style::default().fg(theme.graph_text)),
            Span::styled(
                project.child_count.to_string(),
                Style::default().fg(theme.main_fg),
            ),
        ]));
        lines.push(Line::from(vec![
            Span::styled(" cwd ", Style::default().fg(theme.graph_text)),
            Span::styled(
                truncate_str(&project.cwd, area.width.saturating_sub(6) as usize),
                Style::default().fg(theme.inactive_fg),
            ),
        ]));
    }

    if app.workspace_projects.is_empty() {
        lines.push(Line::from(Span::styled(
            " no live workspace data",
            Style::default().fg(theme.inactive_fg),
        )));
    }

    let block = btop_block_active("workspace", "A", theme.mem_box, theme, active);
    f.render_widget(Paragraph::new(lines).block(block), area);
}
