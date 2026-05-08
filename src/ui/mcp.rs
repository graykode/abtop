use crate::app::App;
use crate::collector::mcp::ACTIVE_MTIME_SECS;
use crate::locale::t;
use crate::theme::Theme;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;
use std::time::SystemTime;

use super::{btop_block, fmt_age, grad_at, make_gradient};

pub(crate) fn draw_mcp_panel(f: &mut Frame, app: &App, area: Rect, theme: &Theme) {
    let header_style = Style::default()
        .fg(theme.main_fg)
        .add_modifier(Modifier::BOLD);
    let parent_label = t("mcp.parent");
    let profile_label = t("mcp.profile");
    let act_tot_label = t("mcp.act_tot");
    let last_label = t("mcp.last");
    let mut lines = vec![Line::from(vec![
        Span::styled(format!(" {}  ", parent_label), header_style),
        Span::styled(format!("{:<13}", profile_label), header_style),
        Span::styled(format!("{} ", act_tot_label), header_style),
        Span::styled(last_label, header_style),
    ])];

    if app.mcp_servers.is_empty() {
        let no_servers = t("mcp.no_servers");
        lines.push(Line::from(Span::styled(
            format!(" {}", no_servers),
            Style::default().fg(theme.inactive_fg),
        )));
        let block = btop_block("mcp servers", "⁷", theme.net_box, theme);
        f.render_widget(Paragraph::new(lines).block(block), area);
        return;
    }

    let proc_grad = make_gradient(
        theme.proc_grad.start,
        theme.proc_grad.mid,
        theme.proc_grad.end,
    );
    let now = SystemTime::now();
    let default_label = t("mcp.default");

    for server in &app.mcp_servers {
        let active = server.active_count(now, ACTIVE_MTIME_SECS);
        let total = server.rollouts.len();
        let last_age = server
            .latest_mtime()
            .and_then(|m| now.duration_since(m).ok());

        let active_color = if active > 0 {
            grad_at(&proc_grad, 100.0)
        } else if total > 0 {
            theme.proc_misc
        } else {
            theme.inactive_fg
        };
        let count_text = format!("{:>3}/{:<3}", active, total);

        let last_text = match last_age {
            Some(d) => fmt_age(d.as_secs()),
            None => t("misc.dash"),
        };

        let parent_label_text = format!(" {:<7}", server.parent_cli);
        let profile_label_text = match &server.profile {
            Some(p) => super::truncate_str(p, 12),
            None => default_label.clone(),
        };
        let profile_padded = format!("{:<13}", profile_label_text);

        lines.push(Line::from(vec![
            Span::styled(parent_label_text, Style::default().fg(theme.main_fg)),
            Span::styled(profile_padded, Style::default().fg(theme.session_id)),
            Span::styled(
                format!("{} ", count_text),
                Style::default().fg(active_color),
            ),
            Span::styled(last_text, Style::default().fg(theme.inactive_fg)),
        ]));
    }

    if !app.mcp_suppress_sessions {
        let suppress_off = t("mcp.suppress_off");
        lines.push(Line::from(Span::styled(
            format!(" {}", suppress_off),
            Style::default().fg(theme.inactive_fg),
        )));
    }

    let block = btop_block("mcp servers", "⁷", theme.net_box, theme);
    f.render_widget(Paragraph::new(lines).block(block), area);
}

