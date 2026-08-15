//! Palette and glyphs.
//!
//! The categorical hues are a validated set: on the dark surface every slot
//! clears 3:1 contrast, sits in the L 0.48–0.67 band, holds chroma, and keeps
//! adjacent-pair separation at ΔE ≥ 8 under protan/deutan/tritan simulation
//! and ≥ 15 for normal vision. They are assigned in fixed order and never
//! cycled — an entity keeps its hue when a filter changes the row count, and
//! a ninth series folds into "Other" rather than inventing a hue.
//!
//! Status colors are reserved. They never stand in for "series 5", and they
//! always ship alongside an icon or a word, so state is never encoded by
//! color alone.

use ratatui::style::Color;

pub const SURFACE: Color = Color::Rgb(0x1a, 0x1a, 0x19);
/// One step up from the surface, for cards.
pub const SURFACE_RAISED: Color = Color::Rgb(0x21, 0x21, 0x20);
/// Two steps up, for the active sidebar pill and hovered rows.
pub const SURFACE_ACTIVE: Color = Color::Rgb(0x2e, 0x2e, 0x2c);
/// The selected row. Deliberately a bigger step than the hover surface: a
/// selection has to be findable at a glance across a full screen of rows, and
/// on a terminal that flattens near-black greys the fill alone is not enough —
/// which is why it always ships with the marker glyph below.
pub const SURFACE_SELECTED: Color = Color::Rgb(0x44, 0x44, 0x40);
/// Drawn in the row's own hue at the left edge of the selected row.
///
/// Deliberately not an eighth-block: `▌` is what a half-width bar draws, so a
/// marker made of one would be indistinguishable from data — and would let a
/// test for "the selection is visible" pass on a bar that happens to be half
/// full.
pub const SELECT_MARK: &str = "┃";

pub const TEXT_PRIMARY: Color = Color::Rgb(0xff, 0xff, 0xff);
pub const TEXT_SECONDARY: Color = Color::Rgb(0xc3, 0xc2, 0xb7);
pub const TEXT_MUTED: Color = Color::Rgb(0x86, 0x85, 0x7c);
/// Grid lines, rules, inactive borders — recessive by design.
pub const RULE: Color = Color::Rgb(0x3a, 0x3a, 0x38);

/// Fixed categorical order. Index by entity, never by rank.
pub const SERIES: [Color; 8] = [
    Color::Rgb(0x39, 0x87, 0xe5), // 1 blue
    Color::Rgb(0xd9, 0x59, 0x26), // 2 orange
    Color::Rgb(0x19, 0x9e, 0x70), // 3 aqua
    Color::Rgb(0xc9, 0x85, 0x00), // 4 yellow
    Color::Rgb(0xd5, 0x51, 0x81), // 5 magenta
    Color::Rgb(0x00, 0x83, 0x00), // 6 green
    Color::Rgb(0x90, 0x85, 0xe9), // 7 violet
    Color::Rgb(0xe6, 0x67, 0x67), // 8 red
];

/// Reserved status colors — never reused as a series hue.
pub const GOOD: Color = Color::Rgb(0x0c, 0xa3, 0x0c);
pub const WARNING: Color = Color::Rgb(0xfa, 0xb2, 0x19);
pub const SERIOUS: Color = Color::Rgb(0xec, 0x83, 0x5a);
pub const CRITICAL: Color = Color::Rgb(0xd0, 0x3b, 0x3b);

/// Stable hue for a named entity, so a model keeps its color across views and
/// across filter changes. Rank is never the input.
pub fn series_for(name: &str) -> Color {
    SERIES[stable_index(name) % SERIES.len()]
}

fn stable_index(name: &str) -> usize {
    // FNV-1a: tiny, stable across runs, no dependency.
    let mut h: u64 = 0xcbf29ce484222325;
    for b in name.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h as usize
}

/// The hue for a tool, held constant everywhere it appears.
pub fn source_color(s: crate::model::Source) -> Color {
    match s {
        crate::model::Source::Claude => SERIES[1], // orange
        crate::model::Source::Codex => SERIES[0],  // blue
    }
}

/// Time-of-day band for the "when you work" histogram. Three ordered bands,
/// each a distinct categorical hue, with the night band deliberately recessive.
pub fn hour_band(hour: usize) -> (Color, &'static str) {
    match hour {
        0..=5 => (Color::Rgb(0x55, 0x55, 0x52), "night"),
        6..=11 => (SERIES[5], "morning"),
        12..=17 => (SERIES[0], "afternoon"),
        _ => (SERIES[6], "evening"),
    }
}

// Marks. Eighth-blocks give a bar chart eight sub-cell steps of resolution,
// which is what makes a one-row sparkline readable at terminal density.
pub const BLOCK_H: [&str; 9] = ["", "▏", "▎", "▍", "▌", "▋", "▊", "▉", "█"];
pub const BLOCK_V: [&str; 9] = [" ", "▁", "▂", "▃", "▄", "▅", "▆", "▇", "█"];

pub const CHEVRON: &str = "›";
pub const DOT: &str = "●";
pub const ICON_WARNING: &str = "▲";
pub const ICON_GOOD: &str = "✓";
pub const ICON_INFO: &str = "ⓘ";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_entity_keeps_its_hue_regardless_of_rank() {
        let a = series_for("claude-opus-5");
        let b = series_for("claude-opus-5");
        assert_eq!(a, b);
        assert_ne!(series_for("claude-opus-5"), series_for("gpt-5.2"));
    }

    #[test]
    fn the_categorical_hues_are_all_distinct() {
        // The validated palette is only validated as long as no two slots
        // collapse onto the same hue.
        let mut seen = std::collections::HashSet::new();
        for c in SERIES {
            assert!(seen.insert(format!("{c:?}")), "a hue is repeated in the series order");
        }
    }

    #[test]
    fn status_hues_are_not_in_the_categorical_set() {
        for s in [GOOD, WARNING, SERIOUS, CRITICAL] {
            assert!(!SERIES.contains(&s), "a reserved status color leaked into the series order");
        }
    }
}
