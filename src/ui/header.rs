use crate::app::App;
use crate::locale::t;
use crate::theme::Theme;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

pub(crate) fn draw_header(f: &mut Frame, app: &App, area: Rect, theme: &Theme) {
    let session_count = app.sessions.len();
    let active = app.agent_aggregate.active_count;

    let now = chrono::Local::now().format("%H:%M").to_string();
    let version = env!("CARGO_PKG_VERSION");

    let title = format!(" abtop v{version} ");
    let right = format!(" {now}  {active}↑ {session_count}● ");

    let host_str = app.host_metrics.as_ref().map(fmt_host);
    let agent_str = fmt_agent(&app.agent_aggregate);

    // Width budget: prefer host + agents; fall back to agents-only; then to nothing.
    let width = area.width as usize;
    let base = title.len() + right.len() + 4; // 4 = separators / padding
    let (host_render, agent_render) = if width <= 80 {
        (None, None)
    } else {
        pick_metrics(host_str.as_deref(), &agent_str, width, base)
    };

    let mut spans: Vec<Span> = Vec::with_capacity(8);
    spans.push(Span::styled(
        title.clone(),
        Style::default()
            .fg(theme.title)
            .add_modifier(Modifier::BOLD),
    ));

    if let Some(h) = host_render {
        spans.push(Span::styled(
            format!(" {h} "),
            Style::default().fg(theme.graph_text),
        ));
    }

    if host_render.is_some() && agent_render.is_some() {
        spans.push(Span::styled("─", Style::default().fg(theme.div_line)));
    }

    if let Some(a) = agent_render {
        spans.push(Span::styled(
            format!(" {a} "),
            Style::default().fg(theme.graph_text),
        ));
    }

    // Right-align the timestamp+counters block.
    let used: usize = spans.iter().map(|s| s.content.chars().count()).sum();
    let pad = width.saturating_sub(used + right.chars().count());
    if pad > 0 {
        spans.push(Span::raw(" ".repeat(pad)));
    }
    spans.push(Span::styled(
        format!(" {now}  "),
        Style::default().fg(theme.graph_text),
    ));
    spans.push(Span::styled(
        format!("{active}↑"),
        Style::default().fg(theme.proc_misc),
    ));
    spans.push(Span::styled(
        format!(" {session_count}●"),
        Style::default().fg(theme.main_fg),
    ));
    spans.push(Span::raw("  "));

    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn fmt_host(h: &crate::host_info::HostMetrics) -> String {
    let cpu_label = t("header.cpu");
    let mem_label = t("header.mem");
    let load_label = t("header.load");
    format!(
        "{} {:>2.0}%  {} {:>2.0}%  {} {:.1}",
        cpu_label, h.cpu_pct, mem_label, h.mem_pct, load_label, h.load1
    )
}

fn fmt_agent(a: &crate::host_info::AgentAggregate) -> String {
    let mem = if a.mem_mb >= 1024 {
        format!("{:.1}G", a.mem_mb as f64 / 1024.0)
    } else {
        format!("{}M", a.mem_mb)
    };
    let agents_label = t("header.agents");
    let ctx_label = t("header.ctx");
    format!(
        "{} Σ{} {}%{:.0}%",
        agents_label, mem, ctx_label, a.avg_ctx_pct
    )
}

/// Decide which metrics to render given available width. Drops host first, then
/// agents, returning `(None, None)` when the header is too narrow for either.
fn pick_metrics<'a>(
    host: Option<&'a str>,
    agent: &'a str,
    width: usize,
    base: usize,
) -> (Option<&'a str>, Option<&'a str>) {
    let agent_w = agent.chars().count() + 2;
    let host_w = host.map(|h| h.chars().count() + 3).unwrap_or(0); // +3 for " ─ "

    if width >= base + host_w + agent_w {
        (host, Some(agent))
    } else if width >= base + agent_w {
        (None, Some(agent))
    } else {
        (None, None)
    }
}
