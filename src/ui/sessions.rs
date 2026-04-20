use crate::app::App;
use crate::theme::Theme;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Cell, Paragraph, Row, Table};
use ratatui::Frame;

use super::{btop_block, fmt_mem_kb, fmt_tokens, grad_at, make_gradient, truncate_str};

pub(crate) fn draw_sessions_panel(f: &mut Frame, app: &App, area: Rect, theme: &Theme) {
    // Render the outer block
    let block = btop_block("sessions", "⁵", theme.proc_box, theme);
    f.render_widget(block, area);

    let inner = Rect {
        x: area.x + 1,
        y: area.y + 1,
        width: area.width.saturating_sub(2),
        height: area.height.saturating_sub(2),
    };

    // Session list: 1 header + 2 rows per visible session, plus subagent rows in tree view.
    let visible = app.visible_indices();
    let session_rows: u16 = visible.iter().map(|&i| {
        let base = 2u16;
        if app.tree_view { base + app.sessions[i].subagents.len() as u16 } else { base }
    }).sum();
    // Fixed detail height: keeps the detail panel stable regardless of content
    let detail_reserve: u16 = 10.min(inner.height / 2);
    let max_table = inner.height.saturating_sub(detail_reserve);
    let table_h = (1 + session_rows).min(max_table);

    let panel_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(table_h),
            Constraint::Length(1), // separator line
            Constraint::Min(0),
        ])
        .split(inner);

    // Draw separator line between session list and detail
    {
        let sep_area = panel_chunks[1];
        let sep_line = "─".repeat(sep_area.width as usize);
        f.render_widget(
            Paragraph::new(Span::styled(sep_line, Style::default().fg(theme.proc_box))),
            sep_area,
        );
    }

    // ── Session list table ──
    let proc_grad = make_gradient(theme.proc_grad.start, theme.proc_grad.mid, theme.proc_grad.end);
    let mut rows = Vec::new();

    // Responsive columns — 9 core columns always visible, widths shrink at narrow terminals.
    // Only Memory/Turn/Pid are hidden when truly narrow.
    let w = inner.width;
    let show_pid = w >= 120;
    let show_memory = w >= 100;
    let show_turn = w >= 100;

    // Responsive widths — all 9 core columns always visible, widths adapt
    let project_w: u16 = if w >= 120 { 14 } else if w >= 100 { 10 } else { 7 };
    let session_w: u16 = if w >= 110 { 9 } else { 5 };
    let session_label = if w >= 110 { "Session" } else { "Sess" };
    let status_w: u16 = if w >= 100 { 8 } else { 6 };
    let model_w: u16 = if w >= 110 { 13 } else { 10 };
    let context_w: u16 = if w >= 100 { 7 } else { 5 };
    let context_label = if w >= 100 { "Context" } else { "Ctx" };
    let tokens_w: u16 = if w >= 100 { 7 } else { 5 };

    let visible = app.visible_indices();
    for &i in &visible {
        let session = &app.sessions[i];
        let selected = i == app.selected;
        let marker = if selected { "►" } else { " " };

        let (agent_label, agent_color) = match session.agent_cli {
            "claude" => ("*CC", Color::Rgb(217, 119, 87)),  // #D97757 terracotta
            "codex"  => (">CD", Color::Rgb(122, 157, 255)), // #7A9DFF periwinkle
            other => {
                let fallback: String = other.chars().take(3).collect::<String>().to_uppercase();
                (Box::leak(fallback.into_boxed_str()) as &str, theme.inactive_fg)
            }
        };

        let (status_icon, status_color) = match &session.status {
            crate::model::SessionStatus::Working => ("● Work", theme.proc_misc),
            crate::model::SessionStatus::Waiting => {
                ("◌ Wait", grad_at(&proc_grad, 50.0))
            }
            crate::model::SessionStatus::Done => ("✓ Done", theme.inactive_fg),
        };

        let is_1m = session.total_tokens() > 200_000 || session.model.contains("[1m]");
        let model_short = shorten_model(&session.model, is_1m);
        let ctx_color = grad_at(&proc_grad, session.context_percent);

        let is_done = matches!(session.status, crate::model::SessionStatus::Done);
        let row_style = if selected {
            Style::default()
                .bg(theme.selected_bg)
                .fg(theme.selected_fg)
                .add_modifier(Modifier::BOLD)
        } else if is_done {
            Style::default().fg(theme.inactive_fg)
        } else {
            Style::default()
        };

        let sid_short = if session.session_id.len() >= 8 {
            &session.session_id[..8]
        } else {
            &session.session_id
        };

        let summary_col = app.session_summary(session);

        // Build cells — 9 core columns always present, only Pid/Memory/Turn conditional
        let mut cells = vec![
            Cell::from(Span::styled(marker, Style::default().fg(theme.hi_fg))),
            Cell::from(Span::styled(agent_label, Style::default().fg(agent_color))),
        ];
        if show_pid {
            cells.push(Cell::from(Span::styled(
                format!("{}", session.pid),
                Style::default().fg(theme.inactive_fg),
            )));
        }
        cells.extend([
            Cell::from(Span::styled(
                truncate_str(&session.project_name, project_w as usize),
                Style::default().fg(theme.title),
            )),
            Cell::from(Span::styled(
                truncate_str(sid_short, session_w as usize),
                Style::default().fg(theme.session_id),
            )),
            Cell::from(Span::styled(summary_col, Style::default().fg(theme.main_fg))),
            Cell::from(Span::styled(
                truncate_str(status_icon, status_w as usize),
                Style::default().fg(status_color),
            )),
            Cell::from(Span::styled(
                truncate_str(&model_short, model_w as usize),
                Style::default().fg(if model_short == "-" { theme.inactive_fg } else { theme.graph_text }),
            )),
            Cell::from(Span::styled(
                format!("{:.0}%", session.context_percent),
                Style::default().fg(ctx_color),
            )),
            Cell::from(Span::styled(
                fmt_tokens(session.total_tokens()),
                Style::default().fg(theme.main_fg),
            )),
        ]);
        if show_memory {
            cells.push(Cell::from(Span::styled(
                if session.mem_mb > 0 { format!("{}M", session.mem_mb) } else { "—".into() },
                Style::default().fg(theme.graph_text),
            )));
        }
        if show_turn {
            cells.push(Cell::from(Span::styled(
                format!("{}", session.turn_count),
                Style::default().fg(theme.graph_text),
            )));
        }

        rows.push(Row::new(cells).style(row_style).height(1));

        // 2nd line: task text in Summary column
        let summary_idx = if show_pid { 5 } else { 4 };
        let total_cols = 9 + show_pid as usize + show_memory as usize + show_turn as usize;
        let task_cells: Vec<Cell> = (0..total_cols).map(|j| {
            if j == summary_idx {
                let task_text = session.current_tasks.last().map(|s| s.as_str()).unwrap_or("");
                Cell::from(Span::styled(
                    format!("└─ {}", task_text),
                    Style::default().fg(theme.graph_text),
                ))
            } else {
                Cell::from("")
            }
        }).collect();
        rows.push(Row::new(task_cells).height(1));

        // Tree view: show subagents as indented rows
        if app.tree_view && !session.subagents.is_empty() {
            for (sa_idx, sa) in session.subagents.iter().enumerate() {
                let is_last = sa_idx == session.subagents.len() - 1;
                // Tree connector fits the 3-wide agent column (was truncated before).
                let prefix = if is_last { "└─" } else { "├─" };
                let is_working = sa.status.eq_ignore_ascii_case("working")
                    || sa.status.eq_ignore_ascii_case("in_progress");
                let icon = if is_working { "●" } else { "✓" };
                let sa_fg = if is_working { theme.proc_misc } else { theme.inactive_fg };

                let mut sa_cells: Vec<Cell> = vec![
                    Cell::from(""),
                    Cell::from(Span::styled(prefix, Style::default().fg(theme.div_line))),
                ];
                if show_pid {
                    sa_cells.push(Cell::from(""));
                }
                sa_cells.extend([
                    Cell::from(Span::styled(
                        truncate_str(&sa.name, project_w as usize),
                        Style::default().fg(theme.graph_text),
                    )),
                    Cell::from(""),
                    Cell::from(""),
                    Cell::from(Span::styled(icon, Style::default().fg(sa_fg))),
                    Cell::from(""),
                    Cell::from(""),
                    Cell::from(Span::styled(
                        fmt_tokens(sa.tokens),
                        Style::default().fg(theme.graph_text),
                    )),
                ]);
                if show_memory { sa_cells.push(Cell::from("")); }
                if show_turn { sa_cells.push(Cell::from("")); }
                rows.push(Row::new(sa_cells).height(1));
            }
        }
    }

    let header_style = Style::default()
        .fg(theme.main_fg)
        .add_modifier(Modifier::BOLD);
    let mut header_cells = vec![
        Cell::from(""),
        Cell::from(Span::styled("AI", header_style)),
    ];
    if show_pid {
        header_cells.push(Cell::from(Span::styled("Pid", header_style)));
    }
    header_cells.extend([
        Cell::from(Span::styled("Project", header_style)),
        Cell::from(Span::styled(session_label, header_style)),
        Cell::from(Span::styled("Summary", header_style)),
        Cell::from(Span::styled("Status", header_style)),
        Cell::from(Span::styled("Model", header_style)),
        Cell::from(Span::styled(context_label, header_style)),
        Cell::from(Span::styled("Tokens", header_style)),
    ]);
    if show_memory {
        header_cells.push(Cell::from(Span::styled("Memory", header_style)));
    }
    if show_turn {
        header_cells.push(Cell::from(Span::styled("Turn", header_style)));
    }
    let header = Row::new(header_cells).height(1);

    let mut widths_vec: Vec<Constraint> = vec![
        Constraint::Length(1),   // marker
        Constraint::Length(3),   // agent label
    ];
    if show_pid {
        widths_vec.push(Constraint::Length(6));   // pid
    }
    widths_vec.extend([
        Constraint::Length(project_w),   // project
        Constraint::Length(session_w),   // session id
        Constraint::Fill(1),             // summary (fills remaining)
        Constraint::Length(status_w),    // status
        Constraint::Length(model_w),     // model
        Constraint::Length(context_w),   // context
        Constraint::Length(tokens_w),    // tokens
    ]);
    if show_memory {
        widths_vec.push(Constraint::Length(8));   // memory
    }
    if show_turn {
        widths_vec.push(Constraint::Length(4));   // turn
    }

    // Scroll: rows vary per session in tree view; use the built row list as the source of truth.
    let visible_sessions = app.visible_indices();
    let total_rows = rows.len();
    let needs_scroll = total_rows > panel_chunks[0].height.saturating_sub(1) as usize;

    // Split table area into [table | scrollbar(1)] when scrollable
    let table_area;
    let scrollbar_area: Option<Rect>;
    if needs_scroll && panel_chunks[0].width > 2 {
        let hsplit = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Min(1), Constraint::Length(1)])
            .split(panel_chunks[0]);
        table_area = hsplit[0];
        scrollbar_area = Some(hsplit[1]);
    } else {
        table_area = panel_chunks[0];
        scrollbar_area = None;
    }

    let visible_rows = table_area.height.saturating_sub(1) as usize; // -1 for header
    // Row offset for the selected session, accounting for subagent rows in tree view
    // and filter-hidden sessions above it.
    let selected_pos = visible_sessions.iter().position(|&i| i == app.selected).unwrap_or(0);
    let selected_row_start: usize = visible_sessions.iter().take(selected_pos).map(|&i| {
        let base = 2;
        if app.tree_view { base + app.sessions[i].subagents.len() } else { base }
    }).sum();
    let selected_session_rows = if app.tree_view {
        2 + app.sessions.get(app.selected).map_or(0, |s| s.subagents.len())
    } else { 2 };
    let selected_row_end = selected_row_start + selected_session_rows;
    let scroll_offset = selected_row_end.saturating_sub(visible_rows);
    let visible = if scroll_offset < rows.len() {
        rows.into_iter().skip(scroll_offset).collect::<Vec<_>>()
    } else {
        Vec::new()
    };

    let table = Table::new(visible, widths_vec).header(header);
    f.render_widget(table, table_area);

    // ── Scrollbar column (dedicated 1-char width, btop-style) ──
    if let Some(sb) = scrollbar_area {
        let bar_h = sb.height as usize;
        if bar_h > 0 {
            let thumb_size = ((visible_rows as f64 / total_rows as f64) * bar_h as f64)
                .ceil().max(1.0) as usize;
            let thumb_size = thumb_size.min(bar_h);
            let thumb_pos = if total_rows > visible_rows {
                ((scroll_offset as f64 / (total_rows - visible_rows) as f64)
                    * (bar_h - thumb_size) as f64)
                    .round() as usize
            } else {
                0
            };

            let buf = f.buffer_mut();
            for i in 0..bar_h {
                let y = sb.y + i as u16;
                let (ch, color) = if i >= thumb_pos && i < thumb_pos + thumb_size {
                    ("┃", theme.main_fg)
                } else {
                    ("│", theme.div_line)
                };
                buf[(sb.x, y)].set_symbol(ch).set_fg(color);
            }

            // ↑/↓ arrows at edges when more content exists
            if scroll_offset > 0 {
                buf[(sb.x, sb.y)].set_symbol("↑").set_fg(theme.proc_box);
            }
            if scroll_offset + visible_rows < total_rows {
                buf[(sb.x, sb.y + sb.height - 1)].set_symbol("↓").set_fg(theme.proc_box);
            }
        }
    }

    // ── Detail section for selected session (full-width Paragraph, not Table) ──
    if let Some(session) = app.sessions.get(app.selected) {
        let detail_area = panel_chunks[2];
        if detail_area.height < 3 {
            return;
        }

        // Reserve bottom lines for MEM + version
        let footer_h = 3u16;
        let detail_body_h = detail_area.height.saturating_sub(footer_h);
        let detail_body = Rect {
            x: detail_area.x,
            y: detail_area.y,
            width: detail_area.width,
            height: detail_body_h,
        };
        let detail_footer = Rect {
            x: detail_area.x,
            y: detail_area.y + detail_body_h,
            width: detail_area.width,
            height: footer_h.min(detail_area.height),
        };

        let has_children = !session.children.is_empty();
        let has_subagents = !session.subagents.is_empty();

        // Always show SESSION header (task) at top, then children/subagents below
        let session_header_h: u16 = {
            let mut h = 1u16; // SESSION title
            if !session.initial_prompt.is_empty() { h += 1; }
            h
        };
        let (header_area, lower_area) = if has_children || has_subagents {
            let parts = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(session_header_h),
                    Constraint::Min(1),
                ])
                .split(detail_body);
            (parts[0], Some(parts[1]))
        } else {
            (detail_body, None)
        };

        // SESSION header — always rendered
        {
            let mut lines = Vec::new();
            lines.push(Line::from(Span::styled(
                format!(" SESSION (►{} · {})", &session.session_id, &session.cwd),
                Style::default().fg(theme.title).add_modifier(Modifier::BOLD),
            )));
            if !session.initial_prompt.is_empty() {
                let max_w = (header_area.width as usize).saturating_sub(9);
                lines.push(Line::from(vec![
                    Span::styled("  task ", Style::default().fg(theme.graph_text)),
                    Span::styled(
                        truncate_str(&session.initial_prompt, max_w),
                        Style::default().fg(theme.main_fg),
                    ),
                ]));
            }
            f.render_widget(Paragraph::new(lines), header_area);
        }

        // Children + Subagents below session header
        if let Some(lower) = lower_area {
            let body_chunks = if has_children && has_subagents {
                Layout::default()
                    .direction(Direction::Horizontal)
                    .constraints([Constraint::Percentage(45), Constraint::Percentage(55)])
                    .split(lower)
            } else {
                Layout::default()
                    .direction(Direction::Horizontal)
                    .constraints([Constraint::Percentage(100)])
                    .split(lower)
            };

            // Children (left side)
            if has_children {
                let children_area = body_chunks[0];
                let mut lines = Vec::new();
                lines.push(Line::from(Span::styled(
                    " CHILDREN",
                    Style::default().fg(theme.title).add_modifier(Modifier::BOLD),
                )));
                for child in &session.children {
                    let cmd_short = child
                        .command
                        .split_whitespace()
                        .take(3)
                        .collect::<Vec<_>>()
                        .join(" ");
                    let port_str = child.port.map(|p| format!(" :{}", p)).unwrap_or_default();
                    let max_cmd = (children_area.width as usize).saturating_sub(18);
                    lines.push(Line::from(vec![
                        Span::styled(
                            format!(" {:<6}", child.pid),
                            Style::default().fg(theme.main_fg),
                        ),
                        Span::styled(
                            truncate_str(&cmd_short, max_cmd),
                            Style::default().fg(theme.graph_text),
                        ),
                        Span::styled(
                            format!(" {:>5}", fmt_mem_kb(child.mem_kb)),
                            Style::default().fg(theme.graph_text),
                        ),
                        Span::styled(port_str, Style::default().fg(theme.proc_misc)),
                    ]));
                }
                f.render_widget(Paragraph::new(lines), children_area);
            }

            // Subagents (right side, or full width if no children)
            if has_subagents {
                let sa_area = if has_children {
                    body_chunks[1]
                } else {
                    body_chunks[0]
                };

                let mut lines = Vec::new();
                lines.push(Line::from(Span::styled(
                    " SUBAGENTS",
                    Style::default().fg(theme.title).add_modifier(Modifier::BOLD),
                )));

                let col_w = sa_area.width as usize;
                let use_two_cols = session.subagents.len() > 6 && col_w >= 50;

                if use_two_cols {
                    let half_w = col_w / 2;
                    let name_w = half_w.saturating_sub(12);
                    let mid = session.subagents.len().div_ceil(2);
                    let left_agents = &session.subagents[..mid];
                    let right_agents = &session.subagents[mid..];

                    for (row_idx, sa) in left_agents.iter().enumerate() {
                        let mut spans = Vec::new();
                        // Left column
                        let icon = if sa.status == "working" { "●" } else { "✓" };
                        let fg = if sa.status == "working" { theme.main_fg } else { theme.graph_text };
                        spans.push(Span::styled(
                            format!("  {} {:<w$}", icon, truncate_str(&sa.name, name_w), w = name_w),
                            Style::default().fg(fg),
                        ));
                        spans.push(Span::styled(
                            format!("{:>6}", fmt_tokens(sa.tokens)),
                            Style::default().fg(theme.graph_text),
                        ));

                        // Right column
                        if let Some(sa_r) = right_agents.get(row_idx) {
                            let icon_r = if sa_r.status == "working" { "●" } else { "✓" };
                            let fg_r = if sa_r.status == "working" { theme.main_fg } else { theme.graph_text };
                            spans.push(Span::styled(
                                format!("  {} {:<w$}", icon_r, truncate_str(&sa_r.name, name_w), w = name_w),
                                Style::default().fg(fg_r),
                            ));
                            spans.push(Span::styled(
                                format!("{:>6}", fmt_tokens(sa_r.tokens)),
                                Style::default().fg(theme.graph_text),
                            ));
                        }
                        lines.push(Line::from(spans));
                    }
                } else {
                    let name_w = col_w.saturating_sub(12);
                    for sa in &session.subagents {
                        let icon = if sa.status == "working" { "●" } else { "✓" };
                        let fg = if sa.status == "working" { theme.main_fg } else { theme.graph_text };
                        lines.push(Line::from(vec![
                            Span::styled(
                                format!("  {} {:<w$}", icon, truncate_str(&sa.name, name_w), w = name_w),
                                Style::default().fg(fg),
                            ),
                            Span::styled(
                                format!("{:>6}", fmt_tokens(sa.tokens)),
                                Style::default().fg(theme.graph_text),
                            ),
                        ]));
                    }
                }
                f.render_widget(Paragraph::new(lines), sa_area);
            }
        }

        // Footer: MEM + version (full width)
        {
            let cpu_grad = make_gradient(theme.cpu_grad.start, theme.cpu_grad.mid, theme.cpu_grad.end);
            let mem_color = if session.mem_line_count >= 180 {
                grad_at(&cpu_grad, 100.0)
            } else {
                theme.graph_text
            };
            let mut footer_lines = vec![Line::from("")];
            // MEM line only for Claude Code sessions (Codex has no memory system)
            if session.agent_cli == "claude" {
                footer_lines.push(Line::from(Span::styled(
                    format!(
                        " MEM {} files · {}/200 lines",
                        session.mem_file_count, session.mem_line_count
                    ),
                    Style::default().fg(mem_color),
                )));
            }
            // Context evolution sparkline (if history available)
            if !session.context_history.is_empty() && session.context_window > 0 {
                let normalized: Vec<f64> = session.context_history.iter()
                    .map(|&v| (v as f64 / session.context_window as f64).min(1.0))
                    .collect();
                let spark_w = (detail_footer.width as usize).saturating_sub(16).clamp(4, 40);
                let mut ctx_spans = vec![
                    Span::styled(" CTX ", Style::default().fg(theme.graph_text)),
                ];
                ctx_spans.extend(super::braille_sparkline(&normalized, spark_w, &cpu_grad, theme.graph_text));
                if session.compaction_count > 0 {
                    ctx_spans.push(Span::styled(
                        format!(" C{}", session.compaction_count),
                        Style::default().fg(grad_at(&cpu_grad, 80.0)),
                    ));
                }
                footer_lines.push(Line::from(ctx_spans));
            }
            let effort_part = if session.effort.is_empty() {
                String::new()
            } else {
                format!(" · effort: {}", session.effort)
            };
            footer_lines.push(Line::from(Span::styled(
                format!(
                    " {} · {} · {} turns{}",
                    session.version,
                    session.elapsed_display(),
                    session.turn_count,
                    effort_part,
                ),
                Style::default().fg(theme.inactive_fg),
            )));
            f.render_widget(Paragraph::new(footer_lines), detail_footer);
        }
    }
}

pub(crate) fn shorten_model(model: &str, is_1m: bool) -> String {
    // "claude-opus-4-6" → "opus4.6", "claude-sonnet-4-6" → "sonnet4.6", "claude-haiku-4-5" → "haiku4.5"
    let s = model
        .strip_prefix("claude-")
        .unwrap_or(model);
    let s = s.trim_end_matches("[1m]");
    // Extract name and version: "opus-4-6" → ("opus", "4.6")
    let base = if let Some(pos) = s.find(|c: char| c.is_ascii_digit()) {
        let name = s[..pos].trim_end_matches('-');
        let ver = s[pos..].replace('-', ".");
        format!("{}{}", name, ver)
    } else {
        s.to_string()
    };
    if is_1m {
        format!("{}[1m]", base)
    } else {
        base
    }
}
