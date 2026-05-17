use crate::app::{App, WorkspaceTask};
use crate::model::{AgentSession, SessionStatus};
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
    let attention_projects = app
        .workspace_projects
        .iter()
        .filter(|project| project.attention_score > 0)
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
        Span::styled("  attention ", Style::default().fg(theme.graph_text)),
        Span::styled(
            attention_projects.to_string(),
            Style::default().fg(if attention_projects > 0 {
                theme.warning_fg
            } else {
                theme.inactive_fg
            }),
        ),
        Span::styled("  lens ", Style::default().fg(theme.graph_text)),
        Span::styled(
            app.workspace_lens.label(),
            Style::default().fg(theme.main_fg),
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
        7
    };
    let available_rows = area
        .height
        .saturating_sub(5 + detail_rows as u16)
        .max(project_rows as u16) as usize;
    let max_projects = (available_rows / project_rows).max(1);
    let visible_projects = app.visible_workspace_project_indices();
    let selected_pos = visible_projects
        .iter()
        .position(|&idx| idx == app.workspace_selected)
        .unwrap_or(0);
    let start = if selected_pos >= max_projects {
        selected_pos + 1 - max_projects
    } else {
        0
    };
    for idx in visible_projects
        .iter()
        .copied()
        .skip(start)
        .take(max_projects)
    {
        let project = &app.workspace_projects[idx];
        let selected = idx == app.workspace_selected;
        let name_w = area.width.saturating_sub(22).clamp(8, 24) as usize;
        let dw = if project.has_dw { " dw" } else { "" };
        let attention = if project.attention_score > 0 {
            " !"
        } else {
            ""
        };
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
            Span::styled(attention, Style::default().fg(theme.warning_fg)),
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
                    project.active_task_status.label()
                } else {
                    "idle"
                },
                Style::default().fg(if project.has_active_task {
                    theme.proc_misc
                } else {
                    theme.inactive_fg
                }),
            ));
            flow.push(Span::styled(
                " tasks ",
                Style::default().fg(theme.graph_text),
            ));
            flow.push(Span::styled(
                project.task_count.to_string(),
                Style::default().fg(theme.main_fg),
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
            Span::styled("  enter ", Style::default().fg(theme.graph_text)),
            Span::styled("open", Style::default().fg(theme.main_fg)),
            Span::styled("  o ", Style::default().fg(theme.graph_text)),
            Span::styled("lens", Style::default().fg(theme.main_fg)),
        ]));
        if project.has_dw {
            let task_title =
                project
                    .active_task_title
                    .as_deref()
                    .unwrap_or(if project.has_active_task {
                        "active task"
                    } else {
                        "none"
                    });
            lines.push(Line::from(vec![
                Span::styled(" task ", Style::default().fg(theme.graph_text)),
                Span::styled(
                    truncate_str(task_title, area.width.saturating_sub(52) as usize),
                    Style::default().fg(if project.has_active_task {
                        theme.main_fg
                    } else {
                        theme.inactive_fg
                    }),
                ),
            ]));
            lines.push(Line::from(vec![
                Span::styled(" status ", Style::default().fg(theme.graph_text)),
                Span::styled(
                    project
                        .active_task_raw_status
                        .as_deref()
                        .unwrap_or(project.active_task_status.label()),
                    Style::default().fg(theme.proc_misc),
                ),
                Span::styled(" phase ", Style::default().fg(theme.graph_text)),
                Span::styled(
                    project.active_task_phase.as_deref().unwrap_or("-"),
                    Style::default().fg(theme.proc_misc),
                ),
                Span::styled(" tasks ", Style::default().fg(theme.graph_text)),
                Span::styled(
                    project.task_count.to_string(),
                    Style::default().fg(theme.main_fg),
                ),
                Span::styled(" deps ", Style::default().fg(theme.graph_text)),
                Span::styled(
                    project.dependency_count.to_string(),
                    Style::default().fg(theme.main_fg),
                ),
                Span::styled(" decisions ", Style::default().fg(theme.graph_text)),
                Span::styled(
                    project.decision_count.to_string(),
                    Style::default().fg(theme.main_fg),
                ),
            ]));
            lines.push(Line::from(vec![
                Span::styled(" records ", Style::default().fg(theme.graph_text)),
                Span::styled(
                    project.record_count.to_string(),
                    Style::default().fg(theme.main_fg),
                ),
                Span::styled(" accept ", Style::default().fg(theme.graph_text)),
                Span::styled(
                    project.active_task_acceptance_count.to_string(),
                    Style::default().fg(theme.main_fg),
                ),
                Span::styled(" verification ", Style::default().fg(theme.graph_text)),
                Span::styled(
                    format!(
                        "{}/{}",
                        project.completed_verification_count, project.verification_count
                    ),
                    Style::default().fg(theme.main_fg),
                ),
                Span::styled(" next ", Style::default().fg(theme.graph_text)),
                Span::styled(
                    project.active_task_next_action(),
                    Style::default().fg(theme.proc_misc),
                ),
            ]));
            if !project.tasks.is_empty() {
                lines.push(Line::from(vec![
                    Span::styled(" task tree ", Style::default().fg(theme.graph_text)),
                    Span::styled(
                        format!("{} items", project.tasks.len()),
                        Style::default().fg(theme.main_fg),
                    ),
                ]));
                for task in project.tasks.iter().take(task_tree_limit(area.height)) {
                    lines.push(render_task_tree_line(task, area.width, theme));
                }
            }
        }
        if !project.attention.is_empty() {
            lines.push(Line::from(vec![
                Span::styled(" attention ", Style::default().fg(theme.graph_text)),
                Span::styled(
                    project.attention.join(","),
                    Style::default().fg(theme.warning_fg),
                ),
                Span::styled(" score ", Style::default().fg(theme.graph_text)),
                Span::styled(
                    project.attention_score.to_string(),
                    Style::default().fg(theme.main_fg),
                ),
            ]));
        }
        lines.push(Line::from(vec![
            Span::styled(" cwd ", Style::default().fg(theme.graph_text)),
            Span::styled(
                truncate_str(&project.cwd, area.width.saturating_sub(6) as usize),
                Style::default().fg(theme.inactive_fg),
            ),
        ]));
        let project_sessions: Vec<_> = app
            .sessions
            .iter()
            .filter(|session| session.cwd == project.cwd)
            .collect();
        if !project_sessions.is_empty() {
            lines.push(Line::from(vec![
                Span::styled(" agents ", Style::default().fg(theme.graph_text)),
                Span::styled(
                    project_sessions.len().to_string(),
                    Style::default().fg(theme.main_fg),
                ),
            ]));
            for session in project_sessions.iter().copied().take(3) {
                let (status, color) = workspace_status(session, theme);
                let task = session
                    .current_tasks
                    .first()
                    .map(String::as_str)
                    .unwrap_or_else(|| workspace_idle_text(&session.status));
                let summary = app.session_summary(session);
                let line_w = area.width.saturating_sub(16) as usize;
                lines.push(Line::from(vec![
                    Span::styled("   ", Style::default().fg(theme.graph_text)),
                    Span::styled(status, Style::default().fg(color)),
                    Span::styled(" ", Style::default().fg(theme.graph_text)),
                    Span::styled(
                        truncate_str(&summary, 24),
                        Style::default().fg(theme.main_fg),
                    ),
                    Span::styled(" - ", Style::default().fg(theme.graph_text)),
                    Span::styled(
                        truncate_str(task, line_w),
                        Style::default().fg(theme.inactive_fg),
                    ),
                ]));
            }
            let timeline: Vec<_> = project_sessions
                .iter()
                .flat_map(|session| session.tool_calls.iter())
                .rev()
                .take(3)
                .collect();
            if !timeline.is_empty() {
                lines.push(Line::from(vec![
                    Span::styled(" timeline ", Style::default().fg(theme.graph_text)),
                    Span::styled(
                        timeline.len().to_string(),
                        Style::default().fg(theme.main_fg),
                    ),
                ]));
                for call in timeline {
                    let arg_w = area.width.saturating_sub(22) as usize;
                    lines.push(Line::from(vec![
                        Span::styled("   ", Style::default().fg(theme.graph_text)),
                        Span::styled(
                            truncate_str(&call.name, 10),
                            Style::default().fg(theme.hi_fg),
                        ),
                        Span::styled(" ", Style::default().fg(theme.graph_text)),
                        Span::styled(
                            truncate_str(&call.arg, arg_w),
                            Style::default().fg(theme.main_fg),
                        ),
                        Span::styled(" ", Style::default().fg(theme.graph_text)),
                        Span::styled(
                            fmt_duration_ms(call.duration_ms),
                            Style::default().fg(theme.inactive_fg),
                        ),
                    ]));
                }
            }
        }
    }

    if app.workspace_projects.is_empty() {
        lines.push(Line::from(Span::styled(
            " no live workspace data",
            Style::default().fg(theme.inactive_fg),
        )));
    } else if visible_projects.is_empty() {
        lines.push(Line::from(Span::styled(
            " no projects match lens",
            Style::default().fg(theme.inactive_fg),
        )));
    }

    let block = btop_block_active("workspace", "A", theme.mem_box, theme, active);
    f.render_widget(Paragraph::new(lines).block(block), area);
}

fn task_tree_limit(height: u16) -> usize {
    if height >= 36 {
        6
    } else if height >= 28 {
        4
    } else {
        2
    }
}

fn render_task_tree_line(task: &WorkspaceTask, width: u16, theme: &Theme) -> Line<'static> {
    let marker = if task.is_active { "*" } else { "-" };
    let title_width = width.saturating_sub(54).clamp(12, 42) as usize;
    let phase = task.phase.as_deref().unwrap_or("-");
    Line::from(vec![
        Span::styled("   ", Style::default().fg(theme.graph_text)),
        Span::styled(marker, Style::default().fg(theme.hi_fg)),
        Span::styled(" ", Style::default().fg(theme.graph_text)),
        Span::styled(
            truncate_str(&task.title, title_width),
            Style::default().fg(if task.is_active {
                theme.main_fg
            } else {
                theme.inactive_fg
            }),
        ),
        Span::styled("  ", Style::default().fg(theme.graph_text)),
        Span::styled(
            task.status_label().to_string(),
            task_status_style(task, theme),
        ),
        Span::styled(" phase ", Style::default().fg(theme.graph_text)),
        Span::styled(
            truncate_str(phase, 12),
            Style::default().fg(theme.proc_misc),
        ),
        Span::styled(" v ", Style::default().fg(theme.graph_text)),
        Span::styled(
            format!(
                "{}/{}",
                task.completed_verification_count, task.verification_count
            ),
            Style::default().fg(theme.main_fg),
        ),
        Span::styled(" a ", Style::default().fg(theme.graph_text)),
        Span::styled(
            task.acceptance_count.to_string(),
            Style::default().fg(theme.main_fg),
        ),
        Span::styled(" d ", Style::default().fg(theme.graph_text)),
        Span::styled(
            task.dependencies.len().to_string(),
            Style::default().fg(if task.dependencies.is_empty() {
                theme.inactive_fg
            } else {
                theme.warning_fg
            }),
        ),
    ])
}

fn task_status_style(task: &WorkspaceTask, theme: &Theme) -> Style {
    match task.status {
        crate::task::TaskStatus::Blocked => Style::default().fg(theme.warning_fg),
        crate::task::TaskStatus::Doing => Style::default().fg(theme.proc_misc),
        crate::task::TaskStatus::Review => Style::default().fg(theme.hi_fg),
        crate::task::TaskStatus::Done => Style::default().fg(theme.inactive_fg),
        crate::task::TaskStatus::Ready | crate::task::TaskStatus::Unknown => {
            Style::default().fg(theme.main_fg)
        }
    }
}

fn workspace_status(
    session: &AgentSession,
    theme: &Theme,
) -> (&'static str, ratatui::style::Color) {
    match session.status {
        SessionStatus::Thinking => ("think", theme.proc_misc),
        SessionStatus::Executing => ("work", theme.hi_fg),
        SessionStatus::Waiting => ("wait", theme.main_fg),
        SessionStatus::RateLimited => ("rate", theme.warning_fg),
        SessionStatus::Done => ("done", theme.inactive_fg),
    }
}

fn workspace_idle_text(status: &SessionStatus) -> &'static str {
    match status {
        SessionStatus::Thinking => "generating reply",
        SessionStatus::Executing => "working",
        SessionStatus::Waiting => "waiting for input",
        SessionStatus::RateLimited => "rate limited",
        SessionStatus::Done => "finished",
    }
}

fn fmt_duration_ms(ms: u64) -> String {
    if ms == 0 {
        "live".into()
    } else if ms < 1_000 {
        format!("{}ms", ms)
    } else {
        format!("{:.1}s", ms as f64 / 1_000.0)
    }
}
