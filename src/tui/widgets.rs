//! Drawing primitives.
//!
//! These are functions over a buffer rather than `Widget` impls because every
//! one of them also needs to register clickable regions as it draws. Keeping
//! paint and hit-registration in the same call is what stops the two from
//! drifting apart.
//!
//! Chart rules applied throughout: one axis per chart, thin marks, a 1-cell
//! gap between adjacent fills, recessive gridlines, direct labels in text
//! tokens rather than the series color, and no encoding carried by color
//! alone.

use crate::fmt;
use crate::tui::hit::{Action, Registry};
use crate::tui::theme;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};

/// Fill a rect with a background color.
pub fn fill(buf: &mut Buffer, area: Rect, color: Color) {
    let area = area.intersection(buf.area);
    if area.is_empty() {
        return;
    }
    buf.set_style(area, Style::default().bg(color));
}

/// Write a string at `(x, y)`, clipped to the buffer, returning the number of
/// columns actually written.
///
/// The bounds check is not decoration: the underlying buffer indexes rows
/// without validating them, so a single row of overdraw at the bottom of a
/// small terminal is a panic rather than a clipped glyph. Every draw helper
/// in this module goes through here.
pub fn text(buf: &mut Buffer, x: u16, y: u16, max_w: u16, s: &str, style: Style) -> u16 {
    let area = buf.area;
    if max_w == 0 || y < area.y || y >= area.bottom() || x < area.x || x >= area.right() {
        return 0;
    }
    let (end_x, _) = buf.set_stringn(x, y, s, max_w as usize, style);
    end_x.saturating_sub(x)
}

/// Right-align a string inside `[x, x+w)`.
pub fn text_right(buf: &mut Buffer, x: u16, y: u16, w: u16, s: &str, style: Style) {
    let len = s.chars().count() as u16;
    let start = x + w.saturating_sub(len);
    text(buf, start, y, w.min(len), s, style);
}

/// A card: one-cell-inset surface with a header row.
///
/// Returns the inner content rect. The chevron is drawn only when the card
/// leads somewhere, so it never promises an interaction that does not exist.
pub struct Card<'a> {
    pub title: &'a str,
    pub glyph: &'a str,
    pub glyph_color: Color,
    pub meta: Option<String>,
    pub action: Option<Action>,
}

pub fn card(buf: &mut Buffer, hits: &mut Registry, area: Rect, c: Card<'_>) -> Rect {
    if area.height < 2 || area.width < 4 {
        return Rect { x: area.x, y: area.y, width: 0, height: 0 };
    }
    fill(buf, area, theme::SURFACE_RAISED);

    let y = area.y;
    let mut x = area.x + 1;
    x += text(buf, x, y, 2, c.glyph, Style::default().fg(c.glyph_color));
    x += 1;
    x += text(
        buf,
        x,
        y,
        area.width.saturating_sub(x - area.x + 6),
        c.title,
        Style::default().fg(theme::TEXT_PRIMARY).add_modifier(Modifier::BOLD),
    );
    if let Some(meta) = &c.meta {
        x += 1;
        text(
            buf,
            x,
            y,
            area.right().saturating_sub(x + 3),
            meta,
            Style::default().fg(theme::TEXT_MUTED),
        );
    }
    if let Some(action) = c.action {
        let chev = Rect { x: area.right().saturating_sub(3), y, width: 3, height: 1 };
        text(buf, chev.x + 1, y, 1, theme::CHEVRON, Style::default().fg(theme::TEXT_MUTED));
        hits.add(chev, action);
    }

    Rect {
        x: area.x + 1,
        y: area.y + 1,
        width: area.width.saturating_sub(2),
        height: area.height.saturating_sub(1),
    }
}

/// A KPI tile: a big value, a colored dot, and a label.
///
/// The dot carries the tile's identity color; the number itself stays in the
/// text token, so the figure never depends on hue to be read.
///
/// Wide by nature: an immediate-mode widget takes its whole world as
/// arguments. A params struct would move the same fields behind one more name
/// without removing any of them.
#[allow(clippy::too_many_arguments)]
pub fn kpi_tile(
    buf: &mut Buffer,
    hits: &mut Registry,
    area: Rect,
    value: &str,
    label: &str,
    accent: Color,
    action: Option<Action>,
    hovered: bool,
) {
    if area.height < 3 || area.width < 6 {
        return;
    }
    fill(buf, area, if hovered { theme::SURFACE_ACTIVE } else { theme::SURFACE_RAISED });

    // Centre within the tile and clip to it. Passing the full tile width as
    // the limit from an offset start would let a long label run past the
    // right edge and collide with the next tile.
    let vw = value.chars().count() as u16;
    let vx = area.x + area.width.saturating_sub(vw) / 2;
    text(
        buf,
        vx,
        area.y + 1,
        area.right().saturating_sub(vx),
        value,
        Style::default().fg(theme::TEXT_PRIMARY).add_modifier(Modifier::BOLD),
    );

    let lw = (label.chars().count() as u16 + 2).min(area.width);
    let lx = area.x + area.width.saturating_sub(lw) / 2;
    text(buf, lx, area.y + 2, 1, theme::DOT, Style::default().fg(accent));
    text(
        buf,
        lx + 2,
        area.y + 2,
        area.right().saturating_sub(lx + 2),
        label,
        Style::default().fg(theme::TEXT_MUTED),
    );

    if let Some(a) = action {
        hits.add_hoverable(area, a, hover_id(label));
    }
}

/// One row: label, bar, right-aligned value.
///
/// Adjacent rows are separated by a blank cell rather than touching, so two
/// bars of similar hue never read as one shape.
pub struct BarRow<'a> {
    pub label: &'a str,
    pub value: &'a str,
    /// 0.0..=1.0 of the row's bar track.
    pub fraction: f64,
    pub color: Color,
    pub selected: bool,
    pub hovered: bool,
}

pub fn bar_row(buf: &mut Buffer, area: Rect, label_w: u16, value_w: u16, row: BarRow<'_>) {
    if area.height == 0 || area.width < 8 {
        return;
    }
    // Degrade rather than disappear: when the row cannot hold label, bar and
    // value, the bar is the part that goes. Dropping the whole row instead
    // would leave a card that renders as empty rather than as cramped.
    let label_w = label_w.min(area.width.saturating_sub(value_w + 2));
    let y = area.y;
    if row.selected || row.hovered {
        fill(buf, area, if row.selected { theme::SURFACE_ACTIVE } else { theme::SURFACE_RAISED });
    }

    let label_style = if row.selected {
        Style::default().fg(theme::TEXT_PRIMARY).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(theme::TEXT_SECONDARY)
    };
    text(buf, area.x, y, label_w, &fmt::ellipsize(row.label, label_w as usize), label_style);

    let track_x = area.x + label_w + 1;
    let track_w = area.width.saturating_sub(label_w + value_w + 2);
    hbar(buf, track_x, y, track_w, row.fraction, row.color);

    text_right(
        buf,
        area.right().saturating_sub(value_w),
        y,
        value_w,
        row.value,
        Style::default().fg(theme::TEXT_SECONDARY),
    );
}

/// A horizontal bar with eighth-cell resolution, so short bars stay visible
/// instead of rounding away to nothing.
pub fn hbar(buf: &mut Buffer, x: u16, y: u16, w: u16, fraction: f64, color: Color) {
    if w == 0 {
        return;
    }
    let f = fraction.clamp(0.0, 1.0);
    let eighths = (f * w as f64 * 8.0).round() as u32;
    // Any nonzero magnitude gets at least one eighth: a value that exists
    // must not render as absent.
    let eighths = if f > 0.0 { eighths.max(1) } else { 0 };
    let full = (eighths / 8) as u16;
    let rem = (eighths % 8) as usize;
    let style = Style::default().fg(color);
    for i in 0..full.min(w) {
        text(buf, x + i, y, 1, theme::BLOCK_H[8], style);
    }
    if rem > 0 && full < w {
        text(buf, x + full, y, 1, theme::BLOCK_H[rem], style);
    }
}

/// A multi-row vertical bar chart with a baseline.
///
/// `labels` are drawn under the axis at whatever spacing fits without
/// collision; a label that would overlap its neighbour is dropped rather than
/// truncated into ambiguity.
///
/// Wide for the same reason as [`kpi_tile`]: buffer, hit registry, geometry,
/// data, and styling all arrive together because nothing is retained.
#[allow(clippy::too_many_arguments)]
pub fn vbars(
    buf: &mut Buffer,
    hits: &mut Registry,
    area: Rect,
    values: &[u64],
    labels: &[String],
    color_for: impl Fn(usize) -> Color,
    grow: f64,
    hovered_index: Option<usize>,
    action_for: Option<&dyn Fn(usize) -> Action>,
) {
    if area.width == 0 || area.height < 2 || values.is_empty() {
        return;
    }
    let plot_h = area.height - 1;
    let max = values.iter().copied().max().unwrap_or(0);
    if max == 0 {
        text(
            buf,
            area.x,
            area.y + plot_h / 2,
            area.width,
            "no activity in this window",
            Style::default().fg(theme::TEXT_MUTED),
        );
        return;
    }

    let n = values.len();
    let w = area.width as usize;
    // One column per bar when they fit; otherwise show the most recent `w`.
    let start = n.saturating_sub(w);
    let visible = &values[start..];

    for (i, v) in visible.iter().enumerate() {
        let x = area.x + i as u16;
        let frac = (*v as f64 / max as f64) * grow.clamp(0.0, 1.0);
        let eighths = (frac * plot_h as f64 * 8.0).round() as u32;
        let eighths = if *v > 0 { eighths.max(1) } else { 0 };
        let full = (eighths / 8) as u16;
        let rem = (eighths % 8) as usize;
        let hovered = hovered_index == Some(start + i);
        let color = if hovered { theme::TEXT_PRIMARY } else { color_for(start + i) };
        let style = Style::default().fg(color);

        for r in 0..full.min(plot_h) {
            let y = area.y + plot_h - 1 - r;
            text(buf, x, y, 1, theme::BLOCK_V[8], style);
        }
        if rem > 0 && full < plot_h {
            let y = area.y + plot_h - 1 - full;
            text(buf, x, y, 1, theme::BLOCK_V[rem], style);
        }

        if let Some(f) = action_for {
            let col = Rect { x, y: area.y, width: 1, height: plot_h };
            hits.add_hoverable(col, f(start + i), (start + i) as u64);
        }
    }

    // Baseline, recessive.
    let axis_y = area.y + plot_h;
    for i in 0..area.width {
        text(buf, area.x + i, axis_y, 1, "─", Style::default().fg(theme::RULE));
    }

    // Axis labels, dropped rather than crowded.
    let mut next_free = 0usize;
    for (i, label) in labels.iter().enumerate().skip(start) {
        if label.is_empty() {
            continue;
        }
        let col = i - start;
        if col < next_free || col >= w {
            continue;
        }
        let len = label.chars().count();
        if col + len > w {
            continue;
        }
        text(
            buf,
            area.x + col as u16,
            axis_y,
            len as u16,
            label,
            Style::default().fg(theme::TEXT_MUTED),
        );
        next_free = col + len + 2;
    }
}

/// A legend: colored dot plus label, in text tokens.
pub fn legend(buf: &mut Buffer, area: Rect, items: &[(String, Color)]) {
    let mut x = area.x;
    for (label, color) in items {
        let need = label.chars().count() as u16 + 4;
        if x + need > area.right() {
            break;
        }
        text(buf, x, area.y, 1, theme::DOT, Style::default().fg(*color));
        text(buf, x + 2, area.y, need, label, Style::default().fg(theme::TEXT_MUTED));
        x += need;
    }
}

/// A callout row: reserved status color, always with an icon and a word.
pub fn callout(
    buf: &mut Buffer,
    hits: &mut Registry,
    area: Rect,
    icon: &str,
    color: Color,
    message: &str,
    action: Option<Action>,
) {
    if area.height == 0 || area.width < 8 {
        return;
    }
    fill(buf, area, theme::SURFACE_RAISED);
    let y = area.y + area.height / 2;
    text(buf, area.x + 1, y, 2, icon, Style::default().fg(color));
    text(
        buf,
        area.x + 3,
        y,
        area.width.saturating_sub(6),
        message,
        Style::default().fg(theme::TEXT_SECONDARY),
    );
    if let Some(a) = action {
        text(
            buf,
            area.right().saturating_sub(2),
            y,
            1,
            theme::CHEVRON,
            Style::default().fg(theme::TEXT_MUTED),
        );
        hits.add_hoverable(area, a, hover_id(message));
    }
}

/// A determinate progress bar for the initial scan.
pub fn progress(buf: &mut Buffer, area: Rect, fraction: f64, label: &str) {
    if area.width < 4 || area.height == 0 {
        return;
    }
    let track = Rect { x: area.x, y: area.y, width: area.width, height: 1 };
    for i in 0..track.width {
        text(buf, track.x + i, track.y, 1, "─", Style::default().fg(theme::RULE));
    }
    hbar(buf, track.x, track.y, track.width, fraction, theme::SERIES[0]);
    if area.height > 1 {
        text(buf, area.x, area.y + 1, area.width, label, Style::default().fg(theme::TEXT_MUTED));
    }
}

/// Stable id for hover highlighting.
pub fn hover_id(s: &str) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in s.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

#[cfg(test)]
mod tests {
    use super::*;

    fn buffer(w: u16, h: u16) -> Buffer {
        Buffer::empty(Rect { x: 0, y: 0, width: w, height: h })
    }

    fn area_of(buf: &Buffer) -> Rect {
        buf.area
    }

    fn row_text(buf: &Buffer, y: u16) -> String {
        (0..buf.area.width)
            .map(|x| buf.cell((x, y)).map(|c| c.symbol()).unwrap_or(" ").to_string())
            .collect()
    }

    #[test]
    fn a_tiny_but_nonzero_bar_is_still_drawn() {
        let mut buf = buffer(20, 1);
        hbar(&mut buf, 0, 0, 20, 0.0001, theme::SERIES[0]);
        assert_ne!(row_text(&buf, 0).trim(), "", "a value that exists must not render as absent");
    }

    #[test]
    fn a_zero_bar_draws_nothing() {
        let mut buf = buffer(20, 1);
        hbar(&mut buf, 0, 0, 20, 0.0, theme::SERIES[0]);
        assert_eq!(row_text(&buf, 0).trim(), "");
    }

    #[test]
    fn a_full_bar_fills_its_track_exactly() {
        let mut buf = buffer(10, 1);
        hbar(&mut buf, 0, 0, 10, 1.0, theme::SERIES[0]);
        assert_eq!(row_text(&buf, 0), "██████████");
    }

    #[test]
    fn bars_never_overrun_their_track() {
        let mut buf = buffer(12, 1);
        hbar(&mut buf, 0, 0, 6, 5.0, theme::SERIES[0]);
        let s = row_text(&buf, 0);
        assert_eq!(&s[..], "██████      ", "an out-of-range fraction clamps");
    }

    #[test]
    fn an_empty_window_says_so_rather_than_drawing_a_flat_line() {
        let mut buf = buffer(30, 4);
        let area = area_of(&buf);
        let mut hits = Registry::default();
        vbars(&mut buf, &mut hits, area, &[0, 0, 0], &[], |_| theme::SERIES[0], 1.0, None, None);
        let joined: String = (0..4).map(|y| row_text(&buf, y)).collect();
        assert!(joined.contains("no activity"));
    }

    #[test]
    fn vbars_register_one_clickable_column_per_bar() {
        let mut buf = buffer(10, 5);
        let area = area_of(&buf);
        let mut hits = Registry::default();
        let action = |i: usize| Action::Row(i);
        vbars(
            &mut buf,
            &mut hits,
            area,
            &[1, 2, 3],
            &[],
            |_| theme::SERIES[0],
            1.0,
            None,
            Some(&action),
        );
        assert_eq!(hits.len(), 3);
        assert_eq!(hits.hit(0, 0), Some(&Action::Row(0)));
        assert_eq!(hits.hit(2, 0), Some(&Action::Row(2)));
    }

    #[test]
    fn a_card_returns_an_inner_rect_below_its_header() {
        let mut buf = buffer(40, 6);
        let area = area_of(&buf);
        let mut hits = Registry::default();
        let inner = card(
            &mut buf,
            &mut hits,
            area,
            Card {
                title: "Model Usage",
                glyph: "◱",
                glyph_color: theme::SERIES[0],
                meta: Some("6 models".into()),
                action: Some(Action::Page(crate::tui::app::Page::Models)),
            },
        );
        assert_eq!(inner.y, 1);
        assert_eq!(inner.height, 5);
        assert!(row_text(&buf, 0).contains("Model Usage"));
        assert!(row_text(&buf, 0).contains("6 models"));
        // The chevron is clickable.
        assert_eq!(hits.hit(38, 0), Some(&Action::Page(crate::tui::app::Page::Models)));
    }

    #[test]
    fn a_card_with_no_room_yields_an_empty_rect_instead_of_panicking() {
        let mut buf = buffer(40, 1);
        let mut hits = Registry::default();
        let inner = card(
            &mut buf,
            &mut hits,
            Rect { x: 0, y: 0, width: 40, height: 1 },
            Card {
                title: "t",
                glyph: "◱",
                glyph_color: theme::SERIES[0],
                meta: None,
                action: None,
            },
        );
        assert_eq!(inner.width, 0);
    }

    #[test]
    fn right_aligned_text_ends_at_the_right_edge() {
        let mut buf = buffer(10, 1);
        text_right(&mut buf, 0, 0, 10, "42", Style::default());
        assert_eq!(row_text(&buf, 0), "        42");
    }

    #[test]
    fn axis_labels_that_would_collide_are_dropped() {
        let mut buf = buffer(12, 3);
        let area = area_of(&buf);
        let mut hits = Registry::default();
        let labels: Vec<String> = (0..12).map(|i| format!("Aug {i}")).collect();
        vbars(&mut buf, &mut hits, area, &[1; 12], &labels, |_| theme::SERIES[0], 1.0, None, None);
        let axis = row_text(&buf, 2);
        assert!(axis.contains("Aug 0"));
        assert!(!axis.contains("Aug 1"), "an overlapping label is dropped, not truncated");
    }
}
