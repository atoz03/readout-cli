//! Display formatting.
//!
//! Numbers in a dashboard are read at a glance, so they are abbreviated to
//! three significant characters and given a fixed unit ladder. Precision the
//! reader cannot use is noise.

use chrono::{Local, NaiveDate};

/// `25.4M`, `1.2k`, `937`.
pub fn tokens(n: u64) -> String {
    const UNITS: [(u64, &str); 4] =
        [(1_000_000_000_000, "T"), (1_000_000_000, "B"), (1_000_000, "M"), (1_000, "k")];
    for (div, suffix) in UNITS {
        if n >= div {
            let v = n as f64 / div as f64;
            return if v < 10.0 { format!("{v:.1}{suffix}") } else { format!("{v:.0}{suffix}") };
        }
    }
    n.to_string()
}

/// `1,039` — grouped digits, for counts small enough to read exactly.
pub fn count(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// `$2,616` above a dollar, `$0.42` below it.
pub fn money(v: f64) -> String {
    if v >= 1000.0 {
        format!("${}", count(v.round() as u64))
    } else if v >= 1.0 {
        format!("${v:.2}")
    } else if v > 0.0 {
        format!("${v:.3}")
    } else {
        "$0".to_string()
    }
}

/// Cost that may be incomplete. Renders the em dash when nothing is priced,
/// which is how "we don't know" must look — never `$0`.
pub fn money_partial(cost: f64, coverage: f64) -> String {
    if coverage <= 0.0 {
        "—".to_string()
    } else if coverage >= 0.999 {
        money(cost)
    } else {
        format!("{}+", money(cost))
    }
}

/// A fraction as a percentage, without rounding a real quantity away.
///
/// `0%` next to a list of models that are demonstrably present reads as a bug,
/// so anything above nothing but under half a percent renders `<1%`, and the
/// same at the top end.
pub fn share(fraction: f64) -> String {
    let pct = fraction * 100.0;
    if pct > 0.0 && pct < 0.5 {
        "<1%".to_string()
    } else if (99.5..100.0).contains(&pct) {
        ">99%".to_string()
    } else {
        format!("{pct:.0}%")
    }
}

/// `20h ago`, `yesterday`, `Jul 27`.
pub fn relative(ts: i64) -> String {
    if ts == 0 {
        return "—".into();
    }
    let Some(then) = crate::agg::local_datetime(ts) else { return "—".into() };
    let now = Local::now();
    let secs = (now - then).num_seconds();
    match secs {
        s if s < 0 => "just now".into(),
        s if s < 90 => "just now".into(),
        s if s < 3600 => format!("{}m ago", s / 60),
        s if s < 86_400 => format!("{}h ago", s / 3600),
        s if s < 172_800 => "yesterday".into(),
        s if s < 604_800 => format!("{}d ago", s / 86_400),
        _ => then.format("%b %-d").to_string(),
    }
}

/// `Aug 14` — compact absolute date.
pub fn short_date(d: NaiveDate) -> String {
    d.format("%b %-d").to_string()
}

/// `12a`, `6a`, `12p`, `6p` — the hour-axis ticks.
pub fn hour_label(h: usize) -> String {
    match h {
        0 => "12a".into(),
        1..=11 => format!("{h}a"),
        12 => "12p".into(),
        _ => format!("{}p", h - 12),
    }
}

/// Truncate to `max` display columns, ending in `…` when cut.
pub fn ellipsize(s: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= max {
        return s.to_string();
    }
    if max == 1 {
        return "…".into();
    }
    let mut out: String = chars[..max - 1].iter().collect();
    out.push('…');
    out
}

/// 把外部标签变成安全的单行终端文本，避免控制序列和双向文本伪装。
pub fn terminal_text(s: &str) -> String {
    s.chars().map(|c| if c.is_control() || is_bidi_control(c) { '�' } else { c }).collect()
}

/// 状态栏只有一行，先放根因才能避免外层上下文把真正的 SSH/DNS/权限错误截掉。
pub fn error_chain(error: &anyhow::Error) -> String {
    let parts: Vec<_> = error.chain().rev().map(|part| terminal_text(&part.to_string())).collect();
    parts.join(" · ")
}

pub fn terminal_ellipsize(s: &str, max: usize) -> String {
    ellipsize(&terminal_text(s), max)
}

fn is_bidi_control(c: char) -> bool {
    matches!(
        c,
        '\u{061c}'
            | '\u{200e}'
            | '\u{200f}'
            | '\u{202a}'..='\u{202e}'
            | '\u{2066}'..='\u{2069}'
    )
}

/// `1.4 GB`, `66 MB`.
pub fn bytes(n: u64) -> String {
    const UNITS: [(u64, &str); 3] = [(1 << 30, "GB"), (1 << 20, "MB"), (1 << 10, "kB")];
    for (div, suffix) in UNITS {
        if n >= div {
            return format!("{:.1} {suffix}", n as f64 / div as f64);
        }
    }
    format!("{n} B")
}

/// `1.4s`, `320ms`.
pub fn duration_ms(ms: u128) -> String {
    if ms >= 10_000 {
        format!("{:.1}s", ms as f64 / 1000.0)
    } else if ms >= 1000 {
        format!("{:.2}s", ms as f64 / 1000.0)
    } else {
        format!("{ms}ms")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_share_never_rounds_a_real_quantity_to_nothing() {
        // 301k unpriced tokens out of 12.2B is real, and "0% of tokens are on
        // models with no price on file: codex-auto-review" reads as a bug.
        assert_eq!(share(301_000.0 / 12_224_791_141.0), "<1%");
        assert_eq!(share(0.0), "0%");
        assert_eq!(share(1.0), "100%");
        assert_eq!(share(0.999), ">99%");
        assert_eq!(share(0.91), "91%");
    }

    #[test]
    fn token_counts_climb_the_unit_ladder() {
        assert_eq!(tokens(937), "937");
        assert_eq!(tokens(1_200), "1.2k");
        assert_eq!(tokens(25_400_000), "25M");
        assert_eq!(tokens(1_500_000), "1.5M");
        assert_eq!(tokens(2_000_000_000), "2.0B");
    }

    #[test]
    fn counts_are_digit_grouped() {
        assert_eq!(count(1039), "1,039");
        assert_eq!(count(7), "7");
        assert_eq!(count(1_000_000), "1,000,000");
    }

    #[test]
    fn money_scales_its_precision() {
        assert_eq!(money(2616.4), "$2,616");
        assert_eq!(money(50.712), "$50.71");
        assert_eq!(money(0.4213), "$0.421");
        assert_eq!(money(0.0), "$0");
    }

    #[test]
    fn unknown_cost_never_renders_as_zero() {
        assert_eq!(money_partial(0.0, 0.0), "—");
        assert_eq!(money_partial(12.0, 1.0), "$12.00");
        assert_eq!(money_partial(12.0, 0.5), "$12.00+", "a partial total must say so");
    }

    #[test]
    fn ellipsize_respects_the_budget() {
        assert_eq!(ellipsize("hello", 10), "hello");
        assert_eq!(ellipsize("hello", 5), "hello");
        assert_eq!(ellipsize("hello", 4), "hel…");
        assert_eq!(ellipsize("hello", 1), "…");
        assert_eq!(ellipsize("hello", 0), "");
        // Multi-byte input must not be sliced mid-character.
        assert_eq!(ellipsize("你好世界", 3), "你好…");
    }

    #[test]
    fn terminal_text_neutralizes_layout_and_bidi_controls() {
        let safe = terminal_text("ok\n\x1b]8;;https://evil\x07x\u{202e}txt");
        assert!(!safe.chars().any(char::is_control));
        assert!(!safe.contains('\u{202e}'));
        assert_eq!(safe, "ok��]8;;https://evil�x�txt");
    }

    #[test]
    fn error_chains_put_the_actionable_root_cause_first() {
        use anyhow::Context as _;

        let error = Err::<(), _>(anyhow::anyhow!("DNS lookup failed"))
            .context("syncing host")
            .context("validating device")
            .unwrap_err();
        assert_eq!(error_chain(&error), "DNS lookup failed · syncing host · validating device");
    }

    #[test]
    fn hour_labels_read_as_a_clock() {
        assert_eq!(hour_label(0), "12a");
        assert_eq!(hour_label(6), "6a");
        assert_eq!(hour_label(12), "12p");
        assert_eq!(hour_label(18), "6p");
        assert_eq!(hour_label(23), "11p");
    }
}
