mod context;
mod footer;
mod header;
mod ports;
mod projects;
mod quota;
mod sessions;
mod tokens;

use crate::app::App;
use crate::theme::Theme;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Paragraph};
use ratatui::Frame;

// ── braille graph symbols — from btop_draw.cpp ──────────────────────────────
// 5x5 lookup: [prev_val * 5 + cur_val], values 0-4
pub(crate) const BRAILLE_UP: [&str; 25] = [
    " ", "⢀", "⢠", "⢰", "⢸",
    "⡀", "⣀", "⣠", "⣰", "⣸",
    "⡄", "⣄", "⣤", "⣴", "⣼",
    "⡆", "⣆", "⣦", "⣶", "⣾",
    "⡇", "⣇", "⣧", "⣷", "⣿",
];

// ── gradient interpolation (btop-faithful: linear RGB, 101 steps) ────────────

/// Generate 101-step gradient from start→mid→end, matching btop's generateGradients().
pub(crate) fn make_gradient(start: (u8, u8, u8), mid: (u8, u8, u8), end: (u8, u8, u8)) -> [Color; 101] {
    let mut out = [Color::Reset; 101];
    #[allow(clippy::needless_range_loop)]
    for i in 0..=100 {
        let (s, e, offset, range) = if i <= 50 {
            (start, mid, 0, 50)
        } else {
            (mid, end, 50, 50)
        };
        let t = i - offset;
        let r = s.0 as i32 + t as i32 * (e.0 as i32 - s.0 as i32) / range;
        let g = s.1 as i32 + t as i32 * (e.1 as i32 - s.1 as i32) / range;
        let b = s.2 as i32 + t as i32 * (e.2 as i32 - s.2 as i32) / range;
        out[i] = Color::Rgb(r.clamp(0, 255) as u8, g.clamp(0, 255) as u8, b.clamp(0, 255) as u8);
    }
    out
}

/// Pick color from a gradient at a given percentage.
pub(crate) fn grad_at(gradient: &[Color; 101], pct: f64) -> Color {
    let idx = (pct.clamp(0.0, 100.0)).round() as usize;
    gradient[idx.min(100)]
}

// ── btop-style meter bar using ■ character ───────────────────────────────────

/// Render a btop-style meter: filled ■ with gradient color, empty ■ with meter_bg.
pub(crate) fn meter_bar(pct: f64, width: usize, gradient: &[Color; 101], meter_bg: Color) -> Vec<Span<'static>> {
    if width == 0 {
        return Vec::new();
    }
    let clamped = pct.clamp(0.0, 100.0);
    let filled = ((clamped / 100.0) * width as f64).round() as usize;
    let mut spans = Vec::new();
    for i in 0..width {
        if i < filled {
            let cell_pct = (i as f64 / width as f64) * 100.0;
            spans.push(Span::styled(
                "■",
                Style::default().fg(grad_at(gradient, cell_pct)),
            ));
        } else {
            spans.push(Span::styled("■", Style::default().fg(meter_bg)));
        }
    }
    spans
}

/// Meter bar showing remaining quota: filled = remaining, color reflects urgency.
/// When remaining is high → green (safe), when low → red (danger).
pub(crate) fn remaining_bar(remaining_pct: f64, width: usize, gradient: &[Color; 101], meter_bg: Color) -> Vec<Span<'static>> {
    if width == 0 {
        return Vec::new();
    }
    let clamped = remaining_pct.clamp(0.0, 100.0);
    let filled = ((clamped / 100.0) * width as f64).round() as usize;
    let used_pct = 100.0 - clamped;
    let mut spans = Vec::new();
    for i in 0..width {
        if i < filled {
            let cell_pct = used_pct;
            spans.push(Span::styled(
                "■",
                Style::default().fg(grad_at(gradient, cell_pct)),
            ));
        } else {
            spans.push(Span::styled("■", Style::default().fg(meter_bg)));
        }
    }
    spans
}

// ── braille sparkline ────────────────────────────────────────────────────────

/// Render a braille sparkline from data points (0.0–1.0), colored with gradient.
pub(crate) fn braille_sparkline(data: &[f64], width: usize, gradient: &[Color; 101], graph_text: Color) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    if data.is_empty() || width == 0 {
        for _ in 0..width {
            spans.push(Span::styled(" ", Style::default().fg(graph_text)));
        }
        return spans;
    }

    // We need pairs of data points per braille char (prev, cur)
    // Pad or sample data to fit width * 2 points
    let needed = width * 2;
    let sampled: Vec<f64> = if data.len() >= needed {
        data[data.len() - needed..].to_vec()
    } else {
        let mut v = vec![0.0; needed - data.len()];
        v.extend_from_slice(data);
        v
    };

    for i in 0..width {
        let prev = (sampled[i * 2].clamp(0.0, 1.0) * 4.0).round() as usize;
        let cur = (sampled[i * 2 + 1].clamp(0.0, 1.0) * 4.0).round() as usize;
        let idx = prev * 5 + cur;
        let pct = (sampled[i * 2 + 1] * 100.0).round() as usize;
        let color = grad_at(gradient, pct as f64);
        spans.push(Span::styled(
            BRAILLE_UP[idx.min(24)].to_string(),
            Style::default().fg(color),
        ));
    }
    spans
}

// ── multi-row braille area graph (btop-style filled CPU graph) ──────────────

/// Render a multi-row braille area graph. `data` values are 0.0–1.0.
/// Returns one Vec<Span> per terminal row (top to bottom).
pub(crate) fn braille_graph_multirow(
    data: &[f64],
    width: usize,
    height: usize,
    gradient: &[Color; 101],
    graph_text: Color,
) -> Vec<Vec<Span<'static>>> {
    if height == 0 || width == 0 {
        return vec![vec![]; height];
    }

    let total_vres = height * 4; // vertical resolution in braille dots
    let needed = width * 2; // 2 data points per braille character

    let sampled: Vec<f64> = if data.len() >= needed {
        data[data.len() - needed..].to_vec()
    } else {
        let mut v = vec![0.0; needed - data.len()];
        v.extend_from_slice(data);
        v
    };

    let heights: Vec<usize> = sampled
        .iter()
        .map(|&v| (v.clamp(0.0, 1.0) * total_vres as f64).round() as usize)
        .collect();

    // Braille dot bits — bottom-to-top within each cell:
    // Left col:  row0(bottom)=0x40, row1=0x04, row2=0x02, row3(top)=0x01
    // Right col: row0(bottom)=0x80, row1=0x20, row2=0x10, row3(top)=0x08
    let left_bits: [u32; 4] = [0x40, 0x04, 0x02, 0x01];
    let right_bits: [u32; 4] = [0x80, 0x20, 0x10, 0x08];

    let mut rows: Vec<Vec<Span<'static>>> = Vec::with_capacity(height);

    for row in 0..height {
        let mut spans = Vec::with_capacity(width);
        let inv_row = height - 1 - row; // row 0 in output = top of graph
        let base_y = inv_row * 4;

        for col in 0..width {
            let left_h = heights[col * 2];
            let right_h = heights[col * 2 + 1];

            let mut pattern: u32 = 0;
            for dot_row in 0..4u32 {
                let y_pos = base_y + dot_row as usize;
                if left_h > y_pos {
                    pattern |= left_bits[dot_row as usize];
                }
                if right_h > y_pos {
                    pattern |= right_bits[dot_row as usize];
                }
            }

            let ch = char::from_u32(0x2800 + pattern).unwrap_or(' ');
            let max_val = sampled[col * 2].max(sampled[col * 2 + 1]);
            let color = if pattern == 0 {
                graph_text
            } else {
                grad_at(gradient, max_val * 100.0)
            };
            spans.push(Span::styled(ch.to_string(), Style::default().fg(color)));
        }
        rows.push(spans);
    }

    rows
}

// ── btop-style block with notch title: ──┐¹title┌────── ─────────────────────

pub(crate) fn btop_block(title: &str, number: &str, box_color: Color, theme: &Theme) -> Block<'static> {
    Block::default()
        .title(Line::from(vec![
            Span::styled("┐", Style::default().fg(box_color)),
            Span::styled(
                number.to_string(),
                Style::default().fg(theme.hi_fg).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                title.to_string(),
                Style::default().fg(theme.title).add_modifier(Modifier::BOLD),
            ),
            Span::styled("┌", Style::default().fg(box_color)),
        ]))
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(box_color))
}

pub(crate) fn styled_label(text: &str, graph_text: Color) -> Span<'static> {
    Span::styled(text.to_string(), Style::default().fg(graph_text))
}

// ── main draw ────────────────────────────────────────────────────────────────

const MIN_WIDTH: u16 = 100;
const MIN_HEIGHT: u16 = 24;

pub fn draw(f: &mut Frame, app: &App) {
    let theme = &app.theme;
    let area = f.area();
    let w = area.width;
    let h = area.height;

    if w < MIN_WIDTH || h < MIN_HEIGHT {
        let msg = vec![
            Line::from(Span::styled(
                "Terminal size too small:",
                Style::default().fg(theme.main_fg).add_modifier(Modifier::BOLD),
            )),
            Line::from(vec![
                Span::styled("Width = ", Style::default().fg(theme.main_fg)),
                Span::styled(
                    w.to_string(),
                    Style::default().fg(if w < MIN_WIDTH { Color::Red } else { Color::Green }),
                ),
                Span::styled(" Height = ", Style::default().fg(theme.main_fg)),
                Span::styled(
                    h.to_string(),
                    Style::default().fg(if h < MIN_HEIGHT { Color::Red } else { Color::Green }),
                ),
            ]),
            Line::from(""),
            Line::from(Span::styled(
                "Needed for current config:",
                Style::default().fg(theme.main_fg).add_modifier(Modifier::BOLD),
            )),
            Line::from(Span::styled(
                format!("Width = {} Height = {}", MIN_WIDTH, MIN_HEIGHT),
                Style::default().fg(theme.main_fg),
            )),
        ];
        let block = Paragraph::new(msg).alignment(Alignment::Center);
        let y = h / 2 - 2;
        let msg_area = Rect { x: 0, y, width: w, height: 5.min(h.saturating_sub(y)) };
        f.render_widget(block, msg_area);
        return;
    }

    // Layout priority: sessions first → mid → context (only with surplus space)
    // Sessions get their full ideal height before anything else.

    const CONTEXT_MIN: u16 = 5;
    const FIXED: u16 = 2; // header + footer

    // Check which mid panels are visible
    let any_mid = app.show_quota || app.show_tokens || app.show_ports;

    let mid_h_ideal: u16 = if any_mid { 8 } else { 0 };
    // Sessions: border(2) + header(1) + 2 rows/session + detail area
    let sessions_ideal: u16 = if app.show_sessions {
        (app.sessions.len() as u16 * 2 + 7).max(8)
    } else {
        0
    };
    let context_ideal: u16 = (app.sessions.len() as u16 + 4).clamp(5, 10);

    let available = h.saturating_sub(FIXED);
    const MID_MIN: u16 = 6;
    let mid_reserved = if any_mid { MID_MIN.min(available) } else { 0 };
    let sessions_budget = available.saturating_sub(mid_reserved);
    let sessions_h = if app.show_sessions {
        sessions_ideal.min(sessions_budget).max(5.min(sessions_budget))
    } else {
        0
    };
    let after_sessions = available.saturating_sub(sessions_h);
    let mid_h = if any_mid {
        mid_h_ideal.min(after_sessions).max(mid_reserved.min(after_sessions))
    } else {
        0
    };
    let surplus = available.saturating_sub(sessions_h + mid_h);
    let context_h = if app.show_context && sessions_h >= sessions_ideal && surplus >= CONTEXT_MIN {
        context_ideal.min(surplus)
    } else if app.show_context && !app.show_sessions && surplus >= CONTEXT_MIN {
        context_ideal.min(available.saturating_sub(mid_h))
    } else {
        0
    };

    let mut constraints = [Constraint::Length(0); 5];
    let mut n = 0;
    constraints[n] = Constraint::Length(1); n += 1; // header
    if context_h > 0 {
        constraints[n] = Constraint::Length(context_h); n += 1;
    }
    if mid_h > 0 {
        constraints[n] = Constraint::Length(mid_h); n += 1;
    }
    if sessions_h > 0 {
        constraints[n] = Constraint::Min(sessions_h); n += 1;
    }
    constraints[n] = Constraint::Length(1); // footer
    n += 1;

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(&constraints[..n])
        .split(area);

    let mut idx = 0;
    header::draw_header(f, app, chunks[idx], theme);
    idx += 1;

    if context_h > 0 {
        context::draw_context_panel(f, app, chunks[idx], theme);
        idx += 1;
    }

    if mid_h > 0 {
        // Build constraints for visible mid panels only
        let mut mid_constraints: Vec<Constraint> = Vec::new();
        if app.show_quota { mid_constraints.push(Constraint::Ratio(1, mid_constraints.len() as u32 + 1)); }
        if app.show_tokens { mid_constraints.push(Constraint::Ratio(1, mid_constraints.len() as u32 + 1)); }
        // projects always visible when mid row is shown
        mid_constraints.push(Constraint::Ratio(1, mid_constraints.len() as u32 + 1));
        if app.show_ports { mid_constraints.push(Constraint::Ratio(1, mid_constraints.len() as u32 + 1)); }

        // Recalculate even distribution
        let count = mid_constraints.len() as u32;
        mid_constraints = mid_constraints.iter().map(|_| Constraint::Ratio(1, count)).collect();

        let mid_panels = Layout::default()
            .direction(Direction::Horizontal)
            .constraints(mid_constraints)
            .split(chunks[idx]);

        let mut mi = 0;
        if app.show_quota { quota::draw_quota_panel(f, app, mid_panels[mi], theme); mi += 1; }
        if app.show_tokens { tokens::draw_tokens_panel(f, app, mid_panels[mi], theme); mi += 1; }
        projects::draw_projects_panel(f, app, mid_panels[mi], theme); mi += 1;
        if app.show_ports { ports::draw_ports_panel(f, app, mid_panels[mi], theme); }
        idx += 1;
    }

    if sessions_h > 0 {
        sessions::draw_sessions_panel(f, app, chunks[idx], theme);
        idx += 1;
    }
    footer::draw_footer(f, app, chunks[idx], theme);
}

// ── utility functions ────────────────────────────────────────────────────────

pub(crate) fn fmt_mem_kb(kb: u64) -> String {
    if kb >= 1_048_576 {
        format!("{:.1}G", kb as f64 / 1_048_576.0)
    } else if kb >= 1024 {
        format!("{}M", kb / 1024)
    } else {
        format!("{}K", kb)
    }
}

pub(crate) fn fmt_tokens(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else {
        format!("{}", n)
    }
}

pub(crate) fn truncate_str(s: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let truncated: String = s.chars().take(max - 1).collect();
        format!("{}…", truncated)
    }
}
