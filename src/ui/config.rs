use crate::app::App;
use crate::collector::codexbar::CodexBarQuotaState;
use crate::locale::t;
use crate::theme::Theme;
use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph};
use ratatui::Frame;

pub(crate) fn draw_config_overlay(f: &mut Frame, app: &App, theme: &Theme) {
    let area = f.area();

    let popup_w = 50u16.min(area.width.saturating_sub(4));
    let popup_h = 15u16.min(area.height.saturating_sub(4));
    let x = (area.width.saturating_sub(popup_w)) / 2;
    let y = (area.height.saturating_sub(popup_h)) / 2;
    let popup = Rect::new(x, y, popup_w, popup_h);

    f.render_widget(Clear, popup);

    let config_title = t("config.title");
    let block = Block::default()
        .style(Style::default().bg(theme.main_bg))
        .title(
            Line::from(vec![Span::styled(
                config_title.clone(),
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

    let theme_label = t("config.theme");
    let on_str = t("config.on");
    let off_str = t("config.off");
    let codexbar_state = app.codexbar_quota_status().state;
    let items: Vec<(String, String, ConfigValueTone)> = vec![
        (
            theme_label,
            app.theme.name.to_string(),
            ConfigValueTone::Plain,
        ),
        (
            t("config.context_panel"),
            toggle_str(&on_str, &off_str, app.show_context),
            toggle_tone(app.show_context),
        ),
        (
            t("config.quota_panel"),
            toggle_str(&on_str, &off_str, app.show_quota),
            toggle_tone(app.show_quota),
        ),
        (
            t("config.tokens_panel"),
            toggle_str(&on_str, &off_str, app.show_tokens),
            toggle_tone(app.show_tokens),
        ),
        (
            t("config.projects_panel"),
            toggle_str(&on_str, &off_str, app.show_projects),
            toggle_tone(app.show_projects),
        ),
        (
            t("config.ports_panel"),
            toggle_str(&on_str, &off_str, app.show_ports),
            toggle_tone(app.show_ports),
        ),
        (
            t("config.sessions_panel"),
            toggle_str(&on_str, &off_str, app.show_sessions),
            toggle_tone(app.show_sessions),
        ),
        (
            t("config.mcp_panel"),
            toggle_str(&on_str, &off_str, app.show_mcp),
            toggle_tone(app.show_mcp),
        ),
        (
            t("config.codexbar_quota"),
            codexbar_state_label(codexbar_state),
            codexbar_state_tone(codexbar_state),
        ),
    ];

    let mut lines = Vec::new();
    lines.push(Line::from(""));

    for (i, (label, value, tone)) in items.iter().enumerate() {
        let selected = i == app.config_selected;
        let cursor = if selected { ">" } else { " " };

        let label_style = if selected {
            Style::default()
                .fg(theme.selected_fg)
                .bg(theme.selected_bg)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme.main_fg)
        };

        let value_style = if selected {
            Style::default().fg(theme.selected_fg).bg(theme.selected_bg)
        } else {
            Style::default().fg(match tone {
                ConfigValueTone::Plain => theme.session_id,
                ConfigValueTone::Active => theme.proc_misc,
                ConfigValueTone::Inactive => theme.inactive_fg,
                ConfigValueTone::Pending => theme.warning_fg,
                ConfigValueTone::Error => theme.status_fg,
            })
        };

        let label_w = 25;
        let padded_label = format!("{} {:<width$}", cursor, label, width = label_w);
        let padded_value = format!("{:<14}", value);

        lines.push(Line::from(vec![
            Span::styled(padded_label, label_style),
            Span::styled(padded_value, value_style),
        ]));
    }

    lines.push(Line::from(""));
    let change_label = t("config.change");
    let close_label = t("config.close");
    lines.push(Line::from(Span::styled(
        format!(
            " abtop v{}  {}  Esc {}",
            env!("CARGO_PKG_VERSION"),
            change_label,
            close_label
        ),
        Style::default().fg(theme.graph_text),
    )));

    f.render_widget(Paragraph::new(lines), inner);
}

fn toggle_str(on_str: &str, off_str: &str, v: bool) -> String {
    if v {
        on_str.to_string()
    } else {
        off_str.to_string()
    }
}

#[derive(Clone, Copy)]
enum ConfigValueTone {
    Plain,
    Active,
    Inactive,
    Pending,
    Error,
}

fn toggle_tone(enabled: bool) -> ConfigValueTone {
    if enabled {
        ConfigValueTone::Active
    } else {
        ConfigValueTone::Inactive
    }
}

fn codexbar_state_label(state: CodexBarQuotaState) -> String {
    match state {
        CodexBarQuotaState::Off => t("config.off"),
        CodexBarQuotaState::Checking => t("config.codexbar_checking"),
        CodexBarQuotaState::Available => t("config.codexbar_active"),
        CodexBarQuotaState::Partial => t("config.codexbar_partial"),
        CodexBarQuotaState::Unavailable => t("config.codexbar_unavailable"),
    }
}

fn codexbar_state_tone(state: CodexBarQuotaState) -> ConfigValueTone {
    match state {
        CodexBarQuotaState::Off => ConfigValueTone::Inactive,
        CodexBarQuotaState::Available => ConfigValueTone::Active,
        CodexBarQuotaState::Checking | CodexBarQuotaState::Partial => ConfigValueTone::Pending,
        CodexBarQuotaState::Unavailable => ConfigValueTone::Error,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::PanelVisibility;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    #[test]
    fn codexbar_states_have_compact_unambiguous_labels() {
        assert_eq!(codexbar_state_label(CodexBarQuotaState::Off), "off");
        assert_eq!(
            codexbar_state_label(CodexBarQuotaState::Checking),
            "checking"
        );
        assert_eq!(
            codexbar_state_label(CodexBarQuotaState::Available),
            "active"
        );
        assert_eq!(codexbar_state_label(CodexBarQuotaState::Partial), "partial");
        assert_eq!(
            codexbar_state_label(CodexBarQuotaState::Unavailable),
            "unavailable"
        );
    }

    #[test]
    fn config_overlay_keeps_codexbar_state_visible_at_minimum_size() {
        let app = App::new_with_config(Theme::default(), &[], PanelVisibility::default());
        let backend = TestBackend::new(60, 18);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| draw_config_overlay(f, &app, &app.theme))
            .unwrap();
        let text = format!("{}", terminal.backend());

        assert!(text.contains("CodexBar"), "{text}");
        assert!(text.contains("off"), "{text}");
    }
}
