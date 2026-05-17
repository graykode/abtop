//! Composer/dispatch overlay (`P6-UX-01`).
//!
//! Renders the composer modal on top of any tab when
//! `App.composer.is_open()`. Content varies by state; the actual state
//! machine lives in `app.rs` + `composer::ComposerState`.

use crate::app::App;
use crate::composer::{ComposerState, DispatchOutcome, DispatchResult, DispatchTarget};
use crate::theme::Theme;
use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph};
use ratatui::Frame;

pub(crate) fn draw_composer_overlay(f: &mut Frame, app: &App, theme: &Theme) {
    let state = &app.composer;
    if matches!(state, ComposerState::Closed) {
        return;
    }

    let area = f.area();
    let popup_w = 80u16.min(area.width.saturating_sub(4));
    let popup_h = 22u16.min(area.height.saturating_sub(4));
    let x = area.width.saturating_sub(popup_w) / 2;
    let y = area.height.saturating_sub(popup_h) / 2;
    let popup = Rect::new(x, y, popup_w, popup_h);

    f.render_widget(Clear, popup);

    let agent_label = state
        .agent()
        .map(|agent| agent.label.as_str())
        .unwrap_or("dispatch");
    let title = format!("Dispatch task → {agent_label}");

    let block = Block::default()
        .style(Style::default().bg(theme.main_bg))
        .title(
            Line::from(vec![Span::styled(
                title,
                Style::default()
                    .fg(theme.title)
                    .add_modifier(Modifier::BOLD),
            )])
            .alignment(Alignment::Center),
        )
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(theme.cpu_box));
    f.render_widget(block, popup);

    let inner = Rect::new(
        popup.x + 2,
        popup.y + 1,
        popup.width.saturating_sub(4),
        popup.height.saturating_sub(2),
    );

    let mut lines: Vec<Line> = Vec::new();

    if let Some(target) = state.target() {
        push_target_header(&mut lines, target, theme);
    } else if let ComposerState::Done { result } = state {
        push_result_header(&mut lines, result, theme);
    } else if let ComposerState::Failed { agent_cli, .. } = state {
        lines.push(Line::from(Span::styled(
            format!("Dispatch to {agent_cli}"),
            Style::default()
                .fg(theme.hi_fg)
                .add_modifier(Modifier::BOLD),
        )));
    }

    lines.push(Line::from(""));

    if let Some(brief) = state.brief() {
        push_section_header(&mut lines, "Auto context (preview, redacted)", theme);
        for body_line in brief.lines() {
            lines.push(Line::from(Span::styled(
                body_line.to_string(),
                Style::default().fg(theme.main_fg),
            )));
        }
        lines.push(Line::from(""));
    }

    if state.draft().is_some() {
        push_section_header(&mut lines, "Your question / instruction", theme);
        let draft = state.draft().unwrap_or("");
        let draft_line = if matches!(state, ComposerState::Drafting { .. }) {
            format!("> {draft}\u{2588}")
        } else {
            format!("> {draft}")
        };
        if draft.is_empty()
            && !matches!(
                state,
                ComposerState::AwaitConfirm { .. } | ComposerState::PreviewBrief { .. }
            )
        {
            lines.push(Line::from(Span::styled(
                "> (type a question, or press Enter to dispatch the brief alone)",
                Style::default().fg(theme.graph_text),
            )));
        } else {
            lines.push(Line::from(Span::styled(
                draft_line,
                Style::default().fg(theme.main_fg),
            )));
        }
        lines.push(Line::from(""));
    }

    if let ComposerState::Done { result } = state {
        push_section_header(&mut lines, "Result", theme);
        for body_line in result_summary_lines(result) {
            lines.push(Line::from(Span::styled(
                body_line,
                Style::default().fg(theme.main_fg),
            )));
        }
        lines.push(Line::from(""));
    }

    if let ComposerState::Failed { error, .. } = state {
        push_section_header(&mut lines, "Failed", theme);
        lines.push(Line::from(Span::styled(
            error.clone(),
            Style::default().fg(theme.main_fg),
        )));
        lines.push(Line::from(""));
    }

    push_section_header(&mut lines, "Status", theme);
    lines.push(Line::from(Span::styled(
        status_hint_for(state),
        Style::default().fg(theme.graph_text),
    )));

    f.render_widget(Paragraph::new(lines), inner);
}

fn push_target_header(lines: &mut Vec<Line>, target: &DispatchTarget, theme: &Theme) {
    lines.push(Line::from(vec![
        Span::styled(
            "Task: ",
            Style::default()
                .fg(theme.title)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(target.task_title.clone(), Style::default().fg(theme.hi_fg)),
        Span::styled(
            format!(" [{}]", target.task_status),
            Style::default().fg(theme.graph_text),
        ),
    ]));
    lines.push(Line::from(vec![
        Span::styled(
            "Project: ",
            Style::default()
                .fg(theme.title)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(target.project.clone(), Style::default().fg(theme.main_fg)),
    ]));
}

fn push_result_header(lines: &mut Vec<Line>, result: &DispatchResult, theme: &Theme) {
    lines.push(Line::from(vec![
        Span::styled(
            "Task: ",
            Style::default()
                .fg(theme.title)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(result.task_id.clone(), Style::default().fg(theme.hi_fg)),
        Span::styled(
            format!(" [{}]", outcome_label(result.outcome)),
            Style::default().fg(theme.graph_text),
        ),
    ]));
}

fn push_section_header(lines: &mut Vec<Line>, title: &str, theme: &Theme) {
    lines.push(Line::from(Span::styled(
        format!("─ {title} ─"),
        Style::default()
            .fg(theme.title)
            .add_modifier(Modifier::BOLD),
    )));
}

fn outcome_label(outcome: DispatchOutcome) -> &'static str {
    match outcome {
        DispatchOutcome::DryRun => "dry-run",
        DispatchOutcome::Sent => "sent",
        DispatchOutcome::Failed => "failed",
    }
}

fn result_summary_lines(result: &DispatchResult) -> Vec<String> {
    let mut out = vec![
        format!("- outcome: {}", outcome_label(result.outcome)),
        format!("- agent: {}", result.agent_cli),
        format!("- response bytes: {}", result.response_bytes),
    ];
    if let Some(path) = &result.response_path {
        out.push(format!("- response saved: {}", path.display()));
    }
    if let Some(err) = &result.error {
        out.push(format!("- error: {err}"));
    }
    out
}

fn status_hint_for(state: &ComposerState) -> String {
    match state {
        ComposerState::Closed => String::new(),
        ComposerState::Drafting { .. } => {
            "Enter → preview  ·  Ctrl+R cycle agent  ·  Esc cancel".to_string()
        }
        ComposerState::PreviewBrief { .. } => {
            "Enter → request confirmation  ·  Esc cancel".to_string()
        }
        ComposerState::AwaitConfirm { .. } => {
            "CONFIRM: press Enter within 5s to dispatch  ·  Esc cancel".to_string()
        }
        ComposerState::Dispatching { .. } => "Dispatching… please wait".to_string(),
        ComposerState::Done { .. } => "Done. Press Enter to close.".to_string(),
        ComposerState::Failed { .. } => "Failed. Press Enter to close.".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_hint_changes_per_stage() {
        let make = |stage: &str| -> String {
            let dummy_target = DispatchTarget {
                project: "p".into(),
                task_id: "t".into(),
                task_title: "T".into(),
                task_status: "Ready".into(),
                task_phase: None,
                acceptance_count: 0,
                verification_completed: 0,
                verification_total: 0,
                dependency_count: 0,
            };
            let dummy_agent = crate::composer::DispatchAgent::claude();
            let state = match stage {
                "drafting" => ComposerState::Drafting {
                    target: dummy_target,
                    agent: dummy_agent,
                    draft: String::new(),
                    brief: String::new(),
                },
                "preview" => ComposerState::PreviewBrief {
                    target: dummy_target,
                    agent: dummy_agent,
                    draft: String::new(),
                    brief: String::new(),
                },
                "await" => ComposerState::AwaitConfirm {
                    target: dummy_target,
                    agent: dummy_agent,
                    draft: String::new(),
                    brief: String::new(),
                    requested_at: std::time::Instant::now(),
                },
                _ => ComposerState::Closed,
            };
            status_hint_for(&state)
        };

        assert!(make("drafting").contains("Enter"));
        assert!(make("drafting").contains("Esc"));
        assert!(make("preview").contains("confirmation"));
        assert!(make("await").contains("CONFIRM"));
        assert!(make("await").contains("5s"));
    }

    #[test]
    fn outcome_label_covers_all_variants() {
        assert_eq!(outcome_label(DispatchOutcome::DryRun), "dry-run");
        assert_eq!(outcome_label(DispatchOutcome::Sent), "sent");
        assert_eq!(outcome_label(DispatchOutcome::Failed), "failed");
    }
}
