use crate::app::App;
use crate::collector::codexbar::canonical_provider_id;
use crate::locale::t;
use crate::model::{RateLimitInfo, RateLimitProvenance, RateLimitWindow};
use crate::theme::Theme;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use super::{btop_block_active, fmt_tokens, grad_at, make_gradient, remaining_bar};

/// Data considered stale when its update timestamp is older than this many seconds.
const STALE_SECS: u64 = 600;
const DETAILED_MIN_WIDTH: u16 = 24;
const COMPACT_MIN_WIDTH: u16 = 12;
const MIN_COMPACT_HEIGHT: u16 = 2;

#[derive(Clone, Debug)]
struct ProviderCard {
    key: String,
    windows: Vec<RateLimitWindow>,
    updated_at: Option<u64>,
    unavailable: bool,
    force_stale: bool,
}

impl ProviderCard {
    fn from_rate_limit(rate_limit: &RateLimitInfo) -> Option<Self> {
        let key = provider_key(&rate_limit.source)?;
        Some(Self {
            key,
            windows: effective_windows(rate_limit),
            updated_at: rate_limit.updated_at,
            unavailable: false,
            force_stale: false,
        })
    }

    fn unavailable(key: String) -> Self {
        Self {
            key,
            windows: Vec::new(),
            updated_at: None,
            unavailable: true,
            force_stale: false,
        }
    }

    fn is_stale(&self, now: u64) -> bool {
        self.force_stale
            || self
                .updated_at
                .is_none_or(|timestamp| now.saturating_sub(timestamp) > STALE_SECS)
    }

    fn has_codexbar_windows(&self) -> bool {
        self.windows
            .iter()
            .any(|window| window.provenance == RateLimitProvenance::CodexBar)
    }

    fn heading(&self) -> String {
        let has_native = self
            .windows
            .iter()
            .any(|window| window.provenance == RateLimitProvenance::Native);
        let has_codexbar = self.unavailable
            || self
                .windows
                .iter()
                .any(|window| window.provenance == RateLimitProvenance::CodexBar);
        let suffix = match (has_native, has_codexbar) {
            (true, true) => "·MIX",
            (false, true) => "·CB",
            _ => "",
        };
        format!("{}{}", self.key.to_uppercase(), suffix)
    }

    fn detailed_height(&self) -> u16 {
        if self.unavailable || self.windows.is_empty() {
            2
        } else {
            1_u16.saturating_add(self.windows.len() as u16)
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CardMode {
    Detailed,
    Compact,
}

pub(crate) fn draw_quota_panel(f: &mut Frame, app: &App, area: Rect, theme: &Theme) {
    draw_quota_panel_active(f, app, area, theme, false);
}

pub(crate) fn draw_quota_panel_active(
    f: &mut Frame,
    app: &App,
    area: Rect,
    theme: &Theme,
    active: bool,
) {
    let cpu_grad = make_gradient(theme.cpu_grad.start, theme.cpu_grad.mid, theme.cpu_grad.end);

    let block = btop_block_active("quota", "²", theme.cpu_box, theme, active);
    f.render_widget(block, area);

    let inner = Rect {
        x: area.x.saturating_add(1),
        y: area.y.saturating_add(1),
        width: area.width.saturating_sub(2),
        height: area.height.saturating_sub(2),
    };
    if inner.height == 0 || inner.width == 0 {
        return;
    }

    let cards = collect_provider_cards(app);
    let content_height = inner.height.saturating_sub(1);
    draw_provider_grid(
        f,
        &cards,
        Rect {
            height: content_height,
            ..inner
        },
        &cpu_grad,
        theme,
    );

    let total_tokens: u64 = app
        .sessions
        .iter()
        .map(|session| session.total_tokens())
        .sum();
    let tokens_per_min: f64 = app.token_rates.iter().rev().take(30).sum();
    let total_line = Line::from(vec![
        Span::styled(
            format!(" {} {}", t("quota.total"), fmt_tokens(total_tokens)),
            Style::default().fg(theme.main_fg),
        ),
        Span::styled(
            format!(" {}/min", fmt_tokens(tokens_per_min as u64)),
            Style::default().fg(theme.graph_text),
        ),
    ]);
    f.render_widget(
        Paragraph::new(total_line),
        Rect {
            x: inner.x,
            y: inner.y.saturating_add(content_height),
            width: inner.width,
            height: 1,
        },
    );
}

fn collect_provider_cards(app: &App) -> Vec<ProviderCard> {
    let mut cards: Vec<ProviderCard> = Vec::new();
    for rate_limit in &app.rate_limits {
        let Some(incoming) = ProviderCard::from_rate_limit(rate_limit) else {
            continue;
        };
        if let Some(existing) = cards.iter_mut().find(|card| card.key == incoming.key) {
            merge_provider_card(existing, incoming);
        } else {
            cards.push(incoming);
        }
    }

    // Successful snapshots are represented by the merged rate-limit rows above.
    // Snapshot failures add a tile only when no usable row exists for that provider.
    for snapshot in app.codexbar_provider_snapshots() {
        if snapshot.error.is_none() {
            continue;
        }
        let Some(key) = provider_key(&snapshot.provider) else {
            continue;
        };
        if !cards.iter().any(|card| card.key == key) {
            cards.push(ProviderCard::unavailable(key));
        }
    }

    apply_codexbar_transport_failure(&mut cards, app.codexbar_quota_status().error.is_some());

    cards.sort_by(|left, right| provider_sort_key(&left.key).cmp(&provider_sort_key(&right.key)));
    cards
}

fn apply_codexbar_transport_failure(cards: &mut [ProviderCard], failed: bool) {
    if !failed {
        return;
    }
    for card in cards {
        if card.has_codexbar_windows() {
            card.force_stale = true;
        }
    }
}

fn merge_provider_card(existing: &mut ProviderCard, incoming: ProviderCard) {
    existing.updated_at = match (existing.updated_at, incoming.updated_at) {
        (Some(left), Some(right)) => Some(left.max(right)),
        (left, right) => left.or(right),
    };
    existing.unavailable &= incoming.unavailable;
    existing.force_stale |= incoming.force_stale;
    for window in incoming.windows {
        if let Some(index) = existing
            .windows
            .iter()
            .position(|current| current.id.eq_ignore_ascii_case(&window.id))
        {
            if existing.windows[index].provenance == RateLimitProvenance::CodexBar
                && window.provenance == RateLimitProvenance::Native
            {
                existing.windows[index] = window;
            }
        } else {
            existing.windows.push(window);
        }
    }
}

fn effective_windows(rate_limit: &RateLimitInfo) -> Vec<RateLimitWindow> {
    if !rate_limit.windows.is_empty() {
        return rate_limit.windows.clone();
    }

    // Compatibility for native collectors and external JSON consumers that still
    // populate the historical two slots while migrating to `windows`.
    let mut windows = Vec::with_capacity(2);
    if let Some(used_pct) = rate_limit.five_hour_pct {
        if let Some(window) = RateLimitWindow::try_new(
            "primary",
            format_window_label(rate_limit.five_hour_window_minutes, t("quota.5h")),
            used_pct,
            rate_limit.five_hour_resets_at,
            rate_limit.five_hour_window_minutes,
            RateLimitProvenance::Native,
        ) {
            windows.push(window);
        }
    }
    if let Some(used_pct) = rate_limit.seven_day_pct {
        if let Some(window) = RateLimitWindow::try_new(
            "secondary",
            format_window_label(rate_limit.seven_day_window_minutes, t("quota.7d")),
            used_pct,
            rate_limit.seven_day_resets_at,
            rate_limit.seven_day_window_minutes,
            RateLimitProvenance::Native,
        ) {
            windows.push(window);
        }
    }
    windows
}

fn provider_key(source: &str) -> Option<String> {
    canonical_provider_id(source)
}

fn provider_sort_key(provider: &str) -> (u8, &str) {
    let rank = match provider {
        "claude" => 0,
        "codex" => 1,
        "grok" => 2,
        "kimi" => 3,
        _ => 4,
    };
    (rank, provider)
}

fn draw_provider_grid(
    f: &mut Frame,
    cards: &[ProviderCard],
    area: Rect,
    cpu_grad: &[ratatui::style::Color; 101],
    theme: &Theme,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    if cards.is_empty() {
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                format!(" — {}", t("quota.no_data")),
                Style::default().fg(theme.inactive_fg),
            ))),
            area,
        );
        return;
    }

    if let Some((columns, row_heights)) = detailed_layout(cards, area) {
        draw_complete_grid(
            f,
            cards,
            area,
            columns,
            &row_heights,
            CardMode::Detailed,
            cpu_grad,
            theme,
        );
        return;
    }

    draw_compact_grid(f, cards, area, cpu_grad, theme);
}

fn detailed_layout(cards: &[ProviderCard], area: Rect) -> Option<(usize, Vec<u16>)> {
    let max_columns = cards
        .len()
        .min(usize::from(area.width / DETAILED_MIN_WIDTH));
    for columns in (1..=max_columns).rev() {
        let row_heights: Vec<u16> = cards
            .chunks(columns)
            .map(|row| {
                row.iter()
                    .map(ProviderCard::detailed_height)
                    .max()
                    .unwrap_or(0)
            })
            .collect();
        if row_heights.iter().copied().fold(0_u16, u16::saturating_add) <= area.height {
            return Some((columns, row_heights));
        }
    }
    None
}

#[allow(clippy::too_many_arguments)]
fn draw_complete_grid(
    f: &mut Frame,
    cards: &[ProviderCard],
    area: Rect,
    columns: usize,
    row_heights: &[u16],
    mode: CardMode,
    cpu_grad: &[ratatui::style::Color; 101],
    theme: &Theme,
) {
    let mut y = area.y;
    for (row_index, row) in cards.chunks(columns).enumerate() {
        let height = row_heights[row_index];
        for (column, card) in row.iter().enumerate() {
            let cell = grid_cell(area, columns, column, y, height);
            draw_provider_card(f, cell, card, mode, cpu_grad, theme);
        }
        y = y.saturating_add(height);
    }
}

fn draw_compact_grid(
    f: &mut Frame,
    cards: &[ProviderCard],
    area: Rect,
    cpu_grad: &[ratatui::style::Color; 101],
    theme: &Theme,
) {
    let card_columns = cards
        .len()
        .min(usize::from((area.width / COMPACT_MIN_WIDTH).max(1)));
    let maximum_card_rows = usize::from(area.height / MIN_COMPACT_HEIGHT);
    let card_capacity = card_columns.saturating_mul(maximum_card_rows);
    if card_capacity < cards.len() {
        let strip_columns = cards
            .len()
            .min(usize::from((area.width / COMPACT_MIN_WIDTH).max(1)));
        if cards.len().div_ceil(strip_columns) <= usize::from(area.height) {
            draw_strip_grid(f, cards, area, strip_columns, theme);
            return;
        }
    }

    if area.height == 1 {
        let hidden_providers = cards.len().saturating_sub(1);
        let hidden_windows = cards.iter().map(|card| card.windows.len()).sum::<usize>();
        let overflow = format!("+{hidden_providers}p +{hidden_windows}w");
        let overflow_width = UnicodeWidthStr::width(overflow.as_str());
        let heading_width = usize::from(area.width)
            .saturating_sub(overflow_width)
            .saturating_sub(1);
        let text = format!("{} {overflow}", fit_heading(&cards[0], heading_width));
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                fit_text(&text, usize::from(area.width)),
                Style::default()
                    .fg(theme.title)
                    .add_modifier(Modifier::BOLD),
            ))),
            area,
        );
        return;
    }

    let columns = card_columns;
    let maximum_rows = usize::from((area.height / MIN_COMPACT_HEIGHT).max(1));
    let capacity = columns.saturating_mul(maximum_rows).max(1);
    if capacity == 1 && cards.len() > 1 {
        let hidden_windows = cards[0].windows.len().saturating_sub(1)
            + cards[1..]
                .iter()
                .map(|card| card.windows.len())
                .sum::<usize>();
        draw_single_capacity_card(f, area, &cards[0], cards.len() - 1, hidden_windows, theme);
        return;
    }
    let (visible_cards, overflow_tile, hidden_providers) = if cards.len() > capacity {
        (capacity - 1, true, cards.len() - (capacity - 1))
    } else {
        (cards.len(), false, 0)
    };
    let tile_count = visible_cards + usize::from(overflow_tile);
    let rows = tile_count.div_ceil(columns).max(1);
    let row_heights = even_row_heights(area.height, rows);

    let mut y = area.y;
    for (row, height) in row_heights.iter().copied().enumerate() {
        for column in 0..columns {
            let tile = row * columns + column;
            if tile >= tile_count {
                break;
            }
            let cell = grid_cell(area, columns, column, y, height);
            if tile < visible_cards {
                draw_provider_card(f, cell, &cards[tile], CardMode::Compact, cpu_grad, theme);
            } else {
                draw_provider_overflow(f, cell, hidden_providers, theme);
            }
        }
        y = y.saturating_add(height);
    }
}

fn draw_single_capacity_card(
    f: &mut Frame,
    area: Rect,
    card: &ProviderCard,
    hidden_providers: usize,
    hidden_windows: usize,
    theme: &Theme,
) {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let heading_color = if card.is_stale(now) {
        theme.inactive_fg
    } else {
        theme.title
    };
    let width = usize::from(area.width);
    let mut lines = vec![Line::from(Span::styled(
        format!(" {}", fit_heading(card, width.saturating_sub(1))),
        Style::default()
            .fg(heading_color)
            .add_modifier(Modifier::BOLD),
    ))];
    let status = compact_card_status(card);
    let overflow = format!("+{hidden_providers}p +{hidden_windows}w");
    if area.height >= 3 {
        lines.push(Line::from(Span::styled(
            format!(" {}", fit_text(&status, width.saturating_sub(1))),
            Style::default().fg(theme.graph_text),
        )));
        lines.push(Line::from(Span::styled(
            format!(" {}", fit_text(&overflow, width.saturating_sub(1))),
            Style::default().fg(theme.graph_text),
        )));
    } else if area.height >= 2 {
        let overflow_width = UnicodeWidthStr::width(overflow.as_str());
        let status_width = width.saturating_sub(overflow_width).saturating_sub(1);
        let status = fit_text(&status, status_width);
        lines.push(Line::from(Span::styled(
            fit_text(&format!("{status} {overflow}"), width),
            Style::default().fg(theme.graph_text),
        )));
    }
    f.render_widget(Paragraph::new(lines), area);
}

fn compact_card_status(card: &ProviderCard) -> String {
    if card.unavailable {
        return t("quota.unavailable");
    }
    if let Some(window) = card.windows.first() {
        return format!("{:.0}%", (100.0 - window.used_pct).clamp(0.0, 100.0));
    }
    format!("— {}", t("quota.no_data"))
}

fn draw_strip_grid(
    f: &mut Frame,
    cards: &[ProviderCard],
    area: Rect,
    columns: usize,
    theme: &Theme,
) {
    for (index, card) in cards.iter().enumerate() {
        let row = index / columns;
        let column = index % columns;
        let cell = grid_cell(area, columns, column, area.y + row as u16, 1);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let color = if card.is_stale(now) {
            theme.inactive_fg
        } else {
            theme.title
        };
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                strip_text(card, usize::from(cell.width)),
                Style::default().fg(color).add_modifier(Modifier::BOLD),
            ))),
            cell,
        );
    }
}

fn strip_text(card: &ProviderCard, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    let heading = fit_heading(card, width.saturating_sub(1));
    let detail = if card.unavailable {
        let full = format!(" {heading} {}", t("quota.unavailable"));
        if UnicodeWidthStr::width(full.as_str()) <= width {
            return full;
        }
        format!(" {heading} unavail")
    } else if let Some(window) = card.windows.first() {
        let remaining = (100.0 - window.used_pct).clamp(0.0, 100.0);
        let hidden = card.windows.len().saturating_sub(1);
        if hidden == 0 {
            format!(" {heading} {:>3.0}%", remaining)
        } else {
            let with_value = format!(" {heading} {:>3.0}% +{hidden}w", remaining);
            if UnicodeWidthStr::width(with_value.as_str()) <= width {
                return with_value;
            }
            format!(" {heading} +{hidden}w")
        }
    } else {
        format!(" {heading} —")
    };
    fit_text(&detail, width)
}

fn fit_heading(card: &ProviderCard, width: usize) -> String {
    let heading = card.heading();
    if UnicodeWidthStr::width(heading.as_str()) <= width {
        return heading;
    }
    let suffix = if heading.ends_with("·MIX") {
        "·MIX"
    } else if heading.ends_with("·CB") {
        "·CB"
    } else {
        ""
    };
    let suffix_width = UnicodeWidthStr::width(suffix);
    if suffix_width >= width {
        return fit_text(suffix, width);
    }
    let base = heading.strip_suffix(suffix).unwrap_or(heading.as_str());
    format!("{}{}", fit_text(base, width - suffix_width), suffix)
}

fn even_row_heights(total_height: u16, rows: usize) -> Vec<u16> {
    let rows_u16 = rows as u16;
    let base = total_height / rows_u16;
    let remainder = total_height % rows_u16;
    (0..rows_u16)
        .map(|row| base + u16::from(row < remainder))
        .collect()
}

fn grid_cell(area: Rect, columns: usize, column: usize, y: u16, height: u16) -> Rect {
    let columns_u16 = columns as u16;
    let base = area.width / columns_u16;
    let remainder = area.width % columns_u16;
    let column_u16 = column as u16;
    let x_offset = column_u16
        .saturating_mul(base)
        .saturating_add(column_u16.min(remainder));
    Rect {
        x: area.x.saturating_add(x_offset),
        y,
        width: base + u16::from(column_u16 < remainder),
        height,
    }
}

fn draw_provider_card(
    f: &mut Frame,
    area: Rect,
    card: &ProviderCard,
    mode: CardMode,
    cpu_grad: &[ratatui::style::Color; 101],
    theme: &Theme,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let stale = card.is_stale(now);
    let heading_color = if stale {
        theme.inactive_fg
    } else {
        theme.title
    };
    let heading = match mode {
        CardMode::Detailed => card.heading(),
        CardMode::Compact => fit_heading(card, usize::from(area.width.saturating_sub(1))),
    };
    let mut lines = vec![Line::from(Span::styled(
        format!(" {heading}"),
        Style::default()
            .fg(heading_color)
            .add_modifier(Modifier::BOLD),
    ))];

    if card.unavailable {
        lines.push(Line::from(Span::styled(
            format!(" {}", t("quota.unavailable")),
            Style::default().fg(theme.inactive_fg),
        )));
    } else if card.windows.is_empty() {
        lines.push(Line::from(Span::styled(
            format!(" — {}", t("quota.no_data")),
            Style::default().fg(theme.inactive_fg),
        )));
    } else {
        match mode {
            CardMode::Detailed => {
                for window in &card.windows {
                    let reset = (!stale)
                        .then(|| window.resets_at.map(format_reset_time))
                        .flatten()
                        .unwrap_or_default();
                    lines.push(detailed_window_line(
                        window, area.width, &reset, cpu_grad, theme,
                    ));
                }
            }
            CardMode::Compact => {
                append_compact_windows(&mut lines, card, area, cpu_grad, theme);
            }
        }
    }

    f.render_widget(Paragraph::new(lines), area);
}

fn detailed_window_line(
    window: &RateLimitWindow,
    width: u16,
    reset: &str,
    cpu_grad: &[ratatui::style::Color; 101],
    theme: &Theme,
) -> Line<'static> {
    let width = usize::from(width);
    let raw_reset_width = if reset.is_empty() {
        0
    } else {
        UnicodeWidthStr::width(reset) + 1
    };
    let reset_width = if 1 + 4 + 1 + 2 + 5 + raw_reset_width <= width {
        raw_reset_width
    } else {
        0
    };
    let maximum_label_width = width
        .saturating_sub(1 + 1 + 2 + 5 + reset_width)
        .clamp(4, 24);
    let label_width = UnicodeWidthStr::width(window.label.as_str())
        .clamp(4, 24)
        .min(maximum_label_width);
    let label = pad_to_width(&fit_text(&window.label, label_width), label_width);
    let bar_width = width
        .saturating_sub(1 + label_width + 1 + 5 + reset_width)
        .clamp(2, 10);
    let remaining = (100.0 - window.used_pct).clamp(0.0, 100.0);
    let mut spans = vec![Span::styled(
        format!(" {label} "),
        Style::default().fg(theme.graph_text),
    )];
    spans.extend(remaining_bar(
        remaining,
        bar_width,
        cpu_grad,
        theme.meter_bg,
    ));
    spans.push(Span::styled(
        format!(" {:>3.0}%", remaining),
        Style::default().fg(grad_at(cpu_grad, window.used_pct)),
    ));
    if reset_width > 0 {
        spans.push(Span::styled(
            format!(" {reset}"),
            Style::default().fg(theme.graph_text),
        ));
    }
    Line::from(spans)
}

fn append_compact_windows(
    lines: &mut Vec<Line<'static>>,
    card: &ProviderCard,
    area: Rect,
    cpu_grad: &[ratatui::style::Color; 101],
    theme: &Theme,
) {
    let body_rows = usize::from(area.height.saturating_sub(1));
    if body_rows == 0 {
        return;
    }
    let visible_windows = if card.windows.len() <= body_rows {
        card.windows.len()
    } else {
        body_rows.saturating_sub(1)
    };
    for window in card.windows.iter().take(visible_windows) {
        let remaining = (100.0 - window.used_pct).clamp(0.0, 100.0);
        let label_width = usize::from(area.width).saturating_sub(7).max(1);
        let label = fit_text(&window.label, label_width);
        lines.push(Line::from(vec![
            Span::styled(format!(" {label}"), Style::default().fg(theme.graph_text)),
            Span::styled(
                format!(" {:>3.0}%", remaining),
                Style::default().fg(grad_at(cpu_grad, window.used_pct)),
            ),
        ]));
    }
    let hidden = card.windows.len().saturating_sub(visible_windows);
    if hidden > 0 {
        lines.push(Line::from(Span::styled(
            format!(" +{} {}", hidden, t("quota.more_windows")),
            Style::default().fg(theme.graph_text),
        )));
    }
}

fn draw_provider_overflow(f: &mut Frame, area: Rect, hidden: usize, theme: &Theme) {
    let mut lines = vec![Line::from(Span::styled(
        format!(" +{hidden}"),
        Style::default()
            .fg(theme.title)
            .add_modifier(Modifier::BOLD),
    ))];
    if area.height > 1 {
        lines.push(Line::from(Span::styled(
            format!(" {}", t("quota.more_providers")),
            Style::default().fg(theme.graph_text),
        )));
    }
    f.render_widget(Paragraph::new(lines), area);
}

fn fit_text(value: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    if UnicodeWidthStr::width(value) <= width {
        return value.to_string();
    }
    if width == 1 {
        return "…".to_string();
    }
    let target = width - 1;
    let mut output = String::new();
    let mut used = 0;
    for character in value.chars() {
        let character_width = UnicodeWidthChar::width(character).unwrap_or(0);
        if used + character_width > target {
            break;
        }
        output.push(character);
        used += character_width;
    }
    output.push('…');
    output
}

fn pad_to_width(value: &str, width: usize) -> String {
    let padding = width.saturating_sub(UnicodeWidthStr::width(value));
    format!("{value}{}", " ".repeat(padding))
}

fn format_window_label(window_minutes: Option<u64>, fallback: String) -> String {
    let Some(minutes) = window_minutes else {
        return fallback;
    };
    if minutes == 0 {
        fallback
    } else if minutes % (24 * 60) == 0 {
        format!("{}{}", minutes / (24 * 60), t("time.d"))
    } else if minutes % 60 == 0 {
        format!("{}{}", minutes / 60, t("time.h"))
    } else {
        format!("{}{}", minutes, t("time.m"))
    }
}

/// Format a future reset timestamp as a localized relative countdown.
pub(crate) fn format_reset_time(reset_ts: u64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    if reset_ts <= now {
        return String::new();
    }
    let diff = reset_ts - now;
    let prefix = t("quota.in");
    if diff < 60 {
        format!("{} {}{}", prefix, diff, t("time.s"))
    } else if diff < 3600 {
        format!("{} {}{}", prefix, diff / 60, t("time.m"))
    } else if diff < 86400 {
        let hours = diff / 3600;
        let minutes = (diff % 3600) / 60;
        format!(
            "{} {}{} {}{}",
            prefix,
            hours,
            t("time.h"),
            minutes,
            t("time.m")
        )
    } else {
        let days = diff / 86400;
        let hours = (diff % 86400) / 3600;
        format!(
            "{} {}{} {}{}",
            prefix,
            days,
            t("time.d"),
            hours,
            t("time.h")
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn now_secs() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
    }

    fn window(
        id: &str,
        label: &str,
        used_pct: f64,
        provenance: RateLimitProvenance,
    ) -> RateLimitWindow {
        RateLimitWindow::try_new(
            id,
            label,
            used_pct,
            Some(now_secs() + 7200),
            None,
            provenance,
        )
        .unwrap()
    }

    fn card(key: &str, windows: Vec<RateLimitWindow>) -> ProviderCard {
        ProviderCard {
            key: key.to_string(),
            windows,
            updated_at: Some(now_secs()),
            unavailable: false,
            force_stale: false,
        }
    }

    fn real_shape_cards() -> Vec<ProviderCard> {
        let mut cards = vec![
            card(
                "grok",
                vec![window(
                    "primary",
                    "Primary",
                    18.0,
                    RateLimitProvenance::CodexBar,
                )],
            ),
            ProviderCard::unavailable("kimi".to_string()),
            card(
                "claude",
                vec![
                    window("primary", "5h", 28.0, RateLimitProvenance::CodexBar),
                    window("secondary", "7d", 6.0, RateLimitProvenance::CodexBar),
                    window(
                        "claude-weekly-scoped-fable",
                        "Fable only",
                        0.0,
                        RateLimitProvenance::CodexBar,
                    ),
                ],
            ),
            card(
                "codex",
                vec![
                    window("secondary", "7d", 48.0, RateLimitProvenance::CodexBar),
                    window(
                        "codex-spark-weekly",
                        "Codex Spark Weekly",
                        0.0,
                        RateLimitProvenance::CodexBar,
                    ),
                ],
            ),
        ];
        cards.sort_by(|left, right| {
            provider_sort_key(&left.key).cmp(&provider_sort_key(&right.key))
        });
        cards
    }

    #[test]
    fn missing_update_timestamp_is_rendered_as_stale() {
        let mut unknown_age = card(
            "grok",
            vec![window(
                "primary",
                "Primary",
                18.0,
                RateLimitProvenance::CodexBar,
            )],
        );
        unknown_age.updated_at = None;
        assert!(unknown_age.is_stale(now_secs()));
    }

    fn render_cards(cards: &[ProviderCard], width: u16, height: u16) -> String {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        let theme = Theme::default();
        let gradient = make_gradient(theme.cpu_grad.start, theme.cpu_grad.mid, theme.cpu_grad.end);
        terminal
            .draw(|frame| {
                draw_provider_grid(
                    frame,
                    cards,
                    Rect::new(0, 0, width, height),
                    &gradient,
                    &theme,
                )
            })
            .unwrap();
        format!("{}", terminal.backend())
    }

    #[test]
    fn provider_order_is_stable_and_future_providers_are_lexical() {
        let mut cards = [
            card("zeta", Vec::new()),
            card("kimi", Vec::new()),
            card("codex", Vec::new()),
            card("alpha", Vec::new()),
            card("claude", Vec::new()),
            card("grok", Vec::new()),
        ];
        cards.sort_by(|left, right| {
            provider_sort_key(&left.key).cmp(&provider_sort_key(&right.key))
        });
        assert_eq!(
            cards
                .iter()
                .map(|card| card.key.as_str())
                .collect::<Vec<_>>(),
            ["claude", "codex", "grok", "kimi", "alpha", "zeta"]
        );
    }

    #[test]
    fn headings_report_codexbar_and_mixed_provenance() {
        let codexbar = card(
            "grok",
            vec![window(
                "primary",
                "Primary",
                20.0,
                RateLimitProvenance::CodexBar,
            )],
        );
        let mixed = card(
            "codex",
            vec![
                window("primary", "5h", 20.0, RateLimitProvenance::Native),
                window("spark", "Spark Weekly", 0.0, RateLimitProvenance::CodexBar),
            ],
        );
        assert_eq!(codexbar.heading(), "GROK·CB");
        assert_eq!(mixed.heading(), "CODEX·MIX");
        assert_eq!(
            ProviderCard::unavailable("kimi".into()).heading(),
            "KIMI·CB"
        );
    }

    #[test]
    fn four_real_shape_providers_render_at_supported_sizes() {
        let cards = real_shape_cards();
        for (width, height) in [(58, 2), (58, 15), (18, 5), (98, 6), (98, 37), (158, 45)] {
            let text = render_cards(&cards, width, height);
            for heading in ["CLAUDE·CB", "CODEX·CB", "GROK·CB", "KIMI·CB"] {
                assert!(
                    text.contains(heading),
                    "{width}x{height} missing {heading}\n{text}"
                );
            }
            let unavailable = if width == 18 {
                "unavail"
            } else {
                "unavailable"
            };
            assert!(text.contains(unavailable), "{width}x{height}\n{text}");
        }
    }

    #[test]
    fn hundred_by_eighteen_default_quota_keeps_value_and_exact_overflow() {
        // At 100x18 with all five desktop mid panels enabled, Quota receives a
        // 20x6 outer cell: 18x3 remains after borders and the totals row.
        let cards = real_shape_cards();
        let mut app = App::new_with_config(
            Theme::default(),
            &[],
            crate::config::PanelVisibility::default(),
        );
        crate::demo::populate_demo(&mut app);
        app.rate_limits = cards
            .iter()
            .map(|card| RateLimitInfo {
                source: card.key.clone(),
                updated_at: card.updated_at,
                windows: card.windows.clone(),
                ..RateLimitInfo::default()
            })
            .collect();
        let backend = TestBackend::new(100, 18);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| super::super::draw(frame, &app))
            .unwrap();
        let text = format!("{}", terminal.backend());
        assert!(text.contains("CLAUDE·CB"), "{text}");
        assert!(text.contains("72%"), "{text}");
        assert!(text.contains("+3p +5w"), "{text}");

        let two_row_text = render_cards(&cards, 18, 2);
        assert!(two_row_text.contains("72% +3p +5w"), "{two_row_text}");
    }

    #[test]
    fn compact_heading_truncation_preserves_provenance_suffix() {
        let text = render_cards(
            &[card(
                "very-long-provider",
                vec![window(
                    "primary",
                    "Primary",
                    25.0,
                    RateLimitProvenance::CodexBar,
                )],
            )],
            12,
            2,
        );
        assert!(text.contains("·CB"), "{text}");
    }

    #[test]
    fn normal_wide_quota_panel_shows_every_window() {
        let text = render_cards(&real_shape_cards(), 158, 45);
        for label in ["5h", "7d", "Fable only", "Codex Spark Weekly", "Primary"] {
            assert!(text.contains(label), "missing {label}\n{text}");
        }
        assert!(!text.contains("+1 windows"), "{text}");
    }

    #[test]
    fn stale_detailed_card_suppresses_reset_countdown() {
        let mut stale = card(
            "claude",
            vec![window("primary", "5h", 28.0, RateLimitProvenance::Native)],
        );
        stale.updated_at = Some(now_secs() - STALE_SECS - 1);
        let stale_text = render_cards(&[stale], 30, 5);
        assert!(!stale_text.contains("in "), "{stale_text}");

        let fresh_text = render_cards(
            &[card(
                "claude",
                vec![window("primary", "5h", 28.0, RateLimitProvenance::Native)],
            )],
            30,
            5,
        );
        assert!(fresh_text.contains("in "), "{fresh_text}");
    }

    #[test]
    fn retained_transport_failure_stales_only_codexbar_backed_cards() {
        let mut cards = vec![
            card(
                "claude",
                vec![window(
                    "primary",
                    "Native Primary",
                    28.0,
                    RateLimitProvenance::Native,
                )],
            ),
            card(
                "grok",
                vec![window(
                    "primary",
                    "CodexBar Primary",
                    18.0,
                    RateLimitProvenance::CodexBar,
                )],
            ),
            card(
                "codex",
                vec![
                    window(
                        "primary",
                        "Native Primary",
                        48.0,
                        RateLimitProvenance::Native,
                    ),
                    window("spark", "Spark Weekly", 0.0, RateLimitProvenance::CodexBar),
                ],
            ),
        ];
        apply_codexbar_transport_failure(&mut cards, true);
        let now = now_secs();
        assert!(!cards[0].is_stale(now));
        assert!(cards[1].is_stale(now));
        assert!(cards[2].is_stale(now));

        let native_text = render_cards(&cards[..1], 36, 3);
        assert!(native_text.contains("in "), "{native_text}");
        let codexbar_text = render_cards(&cards[1..2], 36, 3);
        assert!(!codexbar_text.contains("in "), "{codexbar_text}");
    }

    #[test]
    fn compact_cards_report_window_overflow() {
        let windows = (0..5)
            .map(|index| {
                window(
                    &format!("window-{index}"),
                    &format!("Window {index}"),
                    index as f64,
                    RateLimitProvenance::CodexBar,
                )
            })
            .collect();
        let text = render_cards(&[card("claude", windows)], 12, 3);
        assert!(text.contains("+4 windows"), "{text}");
    }

    #[test]
    fn compact_grid_reports_provider_overflow() {
        let cards = (0..10)
            .map(|index| card(&format!("provider-{index}"), Vec::new()))
            .collect::<Vec<_>>();
        let text = render_cards(&cards, 24, 4);
        assert!(text.contains("+7"), "{text}");
        assert!(text.contains("providers"), "{text}");
    }

    #[test]
    fn panel_preserves_total_and_rate_summary() {
        let mut app = App::new_with_config(
            Theme::default(),
            &[],
            crate::config::PanelVisibility::default(),
        );
        app.token_rates.extend([100.0, 200.0]);
        let backend = TestBackend::new(60, 6);
        let mut terminal = Terminal::new(backend).unwrap();
        let theme = Theme::default();
        terminal
            .draw(|frame| draw_quota_panel(frame, &app, Rect::new(0, 0, 60, 6), &theme))
            .unwrap();
        let text = format!("{}", terminal.backend());
        assert!(text.contains("total 0 300/min"), "{text}");
    }

    #[test]
    fn legacy_slots_remain_visible_during_model_migration() {
        let rate_limit = RateLimitInfo {
            source: "claude".into(),
            five_hour_pct: Some(25.0),
            five_hour_window_minutes: Some(300),
            seven_day_pct: Some(50.0),
            seven_day_window_minutes: Some(10_080),
            ..RateLimitInfo::default()
        };
        let windows = effective_windows(&rate_limit);
        assert_eq!(windows.len(), 2);
        assert_eq!(windows[0].label, "5h");
        assert_eq!(windows[1].label, "7d");
        assert!(windows
            .iter()
            .all(|window| window.provenance == RateLimitProvenance::Native));
    }

    #[test]
    fn text_fitting_is_unicode_width_safe() {
        assert_eq!(fit_text("Codex Spark Weekly", 8), "Codex S…");
        assert_eq!(
            UnicodeWidthStr::width(fit_text("日本語 provider", 8).as_str()),
            8
        );
        assert_eq!(pad_to_width("5h", 4), "5h  ");
    }
}
