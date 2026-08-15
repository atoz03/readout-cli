//! Non-interactive output: plain text, JSON, CSV.
//!
//! The TUI is the point of this tool, but statistics that can only be read by
//! a human in a terminal are hard to script against. Every view the dashboard
//! shows is also available here.

use crate::agg::{Summary, dense_daily};
use crate::fmt;
use crate::model::Source;
use crate::pricing::Pricing;
use crate::scan::ScanStats;
use serde_json::json;
use std::fmt::Write as _;

/// Human-readable summary, the default for `readout summary`.
pub fn text(s: &Summary, stats: &ScanStats, days: Option<i64>) -> String {
    let mut o = String::new();
    let window = match days {
        Some(d) => format!("last {d} days"),
        None => "all time".to_string(),
    };

    let _ = writeln!(o, "readout — {window}");
    let _ = writeln!(o);
    let _ = writeln!(o, "  {:>15}  {:>10}  {:>9}  {:>8}", "tokens", "cost", "requests", "sessions");
    let _ = writeln!(
        o,
        "  {:>15}  {:>10}  {:>9}  {:>8}   TOTAL",
        fmt::count(s.total.tokens.total()),
        fmt::money_partial(s.total.priced.cost, s.total.priced.coverage()),
        fmt::count(s.total.events),
        fmt::count(s.total.session_count() as u64),
    );
    for (source, b) in &s.by_source {
        let _ = writeln!(
            o,
            "  {:>15}  {:>10}  {:>9}  {:>8}   {}",
            fmt::count(b.tokens.total()),
            fmt::money_partial(b.priced.cost, b.priced.coverage()),
            fmt::count(b.events),
            fmt::count(b.session_count() as u64),
            source.label(),
        );
    }

    let _ = writeln!(o);
    let _ = writeln!(
        o,
        "  input {}  ·  output {}  ·  cache read {}  ·  cache write {}",
        fmt::tokens(s.total.tokens.input),
        fmt::tokens(s.total.tokens.output),
        fmt::tokens(s.total.tokens.cache_read),
        fmt::tokens(s.total.tokens.cache_write()),
    );

    if !s.by_model.is_empty() {
        let _ = writeln!(o, "\n  By model");
        let width = s.by_model.iter().take(12).map(|b| b.label.chars().count()).max().unwrap_or(8);
        for b in s.by_model.iter().take(12) {
            let cost =
                if b.priced.is_complete() { fmt::money(b.priced.cost) } else { "—".to_string() };
            let _ = writeln!(
                o,
                "    {:<width$}  {:>15}  {:>10}",
                b.label,
                fmt::count(b.tokens.total()),
                cost,
            );
        }
    }

    if !s.by_project.is_empty() {
        let _ = writeln!(o, "\n  By project");
        let width = s
            .by_project
            .iter()
            .take(10)
            .map(|b| b.label.chars().count().min(32))
            .max()
            .unwrap_or(8);
        for b in s.by_project.iter().take(10) {
            let _ = writeln!(
                o,
                "    {:<width$}  {:>15}  {:>10}  {}",
                fmt::ellipsize(&b.label, 32),
                fmt::count(b.tokens.total()),
                fmt::money_partial(b.priced.cost, b.priced.coverage()),
                fmt::relative(b.last_ts),
            );
        }
    }

    if !s.unpriced_models.is_empty() {
        let _ = writeln!(
            o,
            "\n  {} of tokens are on {} with no price on file: {}",
            fmt::share(1.0 - s.total.priced.coverage()),
            if s.unpriced_models.len() == 1 { "a model" } else { "models" },
            s.unpriced_models.join(", "),
        );
        let _ = writeln!(o, "  Add rates with `readout pricing --init` to include them in cost.");
    }

    let _ = writeln!(
        o,
        "\n  scanned {} files ({} reused, {} appended, {} full) · read {} in {}",
        stats.files_total,
        stats.files_reused,
        stats.files_appended,
        stats.files_full,
        fmt::bytes(stats.bytes_read),
        fmt::duration_ms(stats.total_ms),
    );
    o
}

/// Timing detail for `--timing`, so the incremental cache's value is visible.
pub fn timing(stats: &ScanStats) -> String {
    let mut o = String::new();
    let _ = writeln!(o, "discover   {:>10}", fmt::duration_ms(stats.discover_ms));
    let _ = writeln!(o, "parse      {:>10}", fmt::duration_ms(stats.parse_ms));
    let _ = writeln!(o, "total      {:>10}", fmt::duration_ms(stats.total_ms));
    let _ = writeln!(o);
    let _ = writeln!(o, "files      {:>10}", fmt::count(stats.files_total as u64));
    let _ = writeln!(o, "  reused   {:>10}", fmt::count(stats.files_reused as u64));
    let _ = writeln!(o, "  appended {:>10}", fmt::count(stats.files_appended as u64));
    let _ = writeln!(o, "  full     {:>10}", fmt::count(stats.files_full as u64));
    let _ = writeln!(
        o,
        "bytes read {:>10}  of {}",
        fmt::bytes(stats.bytes_read),
        fmt::bytes(stats.bytes_total)
    );
    let _ = writeln!(o, "events     {:>10}", fmt::count(stats.events as u64));
    let _ = writeln!(
        o,
        "  dropped  {:>10}  duplicate responses across transcripts",
        fmt::count(stats.duplicates_dropped as u64)
    );
    let _ = writeln!(
        o,
        "  skipped  {:>10}  synthetic records",
        fmt::count(stats.skipped_synthetic as u64)
    );
    o
}

pub fn json(s: &Summary, stats: &ScanStats, days: Option<i64>) -> String {
    let bucket = |b: &crate::agg::Bucket| {
        json!({
            "label": b.label,
            "tokens": {
                "input": b.tokens.input,
                "output": b.tokens.output,
                "cache_read": b.tokens.cache_read,
                "cache_write_5m": b.tokens.cache_write_5m,
                "cache_write_1h": b.tokens.cache_write_1h,
                "total": b.tokens.total(),
            },
            "cost_usd": b.priced.cost,
            "cost_coverage": b.priced.coverage(),
            "requests": b.events,
            "sessions": b.session_count(),
            "last_ts": b.last_ts,
        })
    };

    let v = json!({
        "window_days": days,
        "generated_ts": chrono::Local::now().timestamp(),
        "total": bucket(&s.total),
        "by_source": s.by_source.iter().map(|(src, b)| {
            let mut o = bucket(b);
            o["source"] = json!(src.short());
            o
        }).collect::<Vec<_>>(),
        "by_model": s.by_model.iter().map(bucket).collect::<Vec<_>>(),
        "by_project": s.by_project.iter().map(bucket).collect::<Vec<_>>(),
        "daily": s.daily.iter().map(|d| {
            let mut o = bucket(&d.bucket);
            o["date"] = json!(d.date.to_string());
            o
        }).collect::<Vec<_>>(),
        "by_hour": s.by_hour.iter().enumerate().map(|(h, b)| {
            let mut o = bucket(b);
            o["hour"] = json!(h);
            o
        }).collect::<Vec<_>>(),
        "unpriced_models": s.unpriced_models,
        "scan": {
            "files_total": stats.files_total,
            "files_reused": stats.files_reused,
            "files_appended": stats.files_appended,
            "files_full": stats.files_full,
            "bytes_read": stats.bytes_read,
            "events": stats.events,
            "duplicates_dropped": stats.duplicates_dropped,
            "skipped_synthetic": stats.skipped_synthetic,
            "total_ms": stats.total_ms,
        },
    });
    serde_json::to_string_pretty(&v).unwrap_or_else(|_| "{}".into())
}

/// Daily CSV — the shape most useful to pipe into a spreadsheet.
pub fn csv(s: &Summary, days: i64) -> String {
    let mut o = String::from("date,tokens,cost_usd,requests\n");
    let dense = dense_daily(&s.daily, days as usize);
    let by_date: std::collections::HashMap<_, _> =
        s.daily.iter().map(|d| (d.date, d.bucket.events)).collect();
    for (date, tokens, cost) in dense {
        let requests = by_date.get(&date).copied().unwrap_or(0);
        let _ = writeln!(o, "{date},{tokens},{cost:.6},{requests}");
    }
    o
}

/// Long-form model rate table for `readout pricing`.
pub fn pricing_table(p: &Pricing, observed: &[String]) -> String {
    let mut o = String::new();
    let _ = writeln!(o, "Rates are USD per million tokens.");
    let _ = writeln!(
        o,
        "Cache read defaults to {}x input; cache write to {}x (5m TTL) or {}x (1h TTL).\n\
         A model may pin its own — OpenAI does not bill cache writes.\n",
        crate::pricing::CACHE_READ_MULTIPLIER,
        crate::pricing::CACHE_WRITE_5M_MULTIPLIER,
        crate::pricing::CACHE_WRITE_1H_MULTIPLIER,
    );
    let _ = writeln!(
        o,
        "  {:<28} {:>9} {:>9} {:>11} {:>12}",
        "model", "input", "output", "cache read", "cache write"
    );
    for (model, rate) in p.known_models() {
        let _ = writeln!(
            o,
            "  {model:<28} {:>9.2} {:>9.2} {:>11.2} {:>12.2}",
            rate.input,
            rate.output,
            rate.cache_read_rate(),
            rate.cache_write_5m_rate(),
        );
    }
    let unpriced = p.unpriced_among(observed.iter().map(String::as_str));
    if !unpriced.is_empty() {
        let _ = writeln!(o, "\n  No rate on file (tokens counted, cost shown as —):");
        for m in unpriced {
            let _ = writeln!(o, "    {m}");
        }
    }
    o
}

/// Which sources produced no data, so an empty dashboard explains itself.
pub fn missing_sources(sources: &[Source]) -> Vec<String> {
    let mut out = Vec::new();
    if sources.contains(&Source::Claude) && crate::paths::claude_projects_dir().is_none() {
        out.push("Claude Code (~/.claude/projects not found)".to_string());
    }
    if sources.contains(&Source::Codex) && crate::paths::codex_sessions_dir().is_none() {
        out.push("Codex (~/.codex/sessions not found)".to_string());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agg::{Filter, summarize};
    use crate::model::{Tokens, UsageEvent};

    fn sample() -> Vec<UsageEvent> {
        vec![
            UsageEvent {
                source: Source::Claude,
                ts: chrono::Local::now().timestamp(),
                model: "claude-opus-5".into(),
                session: "s1".into(),
                project: "alpha".into(),
                tokens: Tokens { input: 100, output: 200, ..Default::default() },
                dedup_key: None,
                dedup_rank: 0,
            },
            UsageEvent {
                source: Source::Codex,
                ts: chrono::Local::now().timestamp(),
                // Deliberately a model with no rate on file, so the sample
                // exercises the partial-pricing path.
                model: "codex-auto-review".into(),
                session: "s2".into(),
                project: "beta".into(),
                tokens: Tokens { input: 50, output: 50, ..Default::default() },
                dedup_key: None,
                dedup_rank: 0,
            },
        ]
    }

    #[test]
    fn json_output_is_parseable_and_carries_coverage() {
        let p = Pricing::builtin();
        let s = summarize(&sample(), &Filter::default(), &p);
        let out = json(&s, &ScanStats::default(), Some(30));
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["total"]["tokens"]["total"], 400);
        assert_eq!(v["unpriced_models"][0], "codex-auto-review");
        assert!(v["total"]["cost_coverage"].as_f64().unwrap() < 1.0);
    }

    #[test]
    fn csv_has_one_row_per_day_including_idle_ones() {
        let p = Pricing::builtin();
        let s = summarize(&sample(), &Filter::default(), &p);
        let out = csv(&s, 7);
        assert_eq!(out.lines().count(), 8, "header plus seven days");
        assert!(out.starts_with("date,tokens,cost_usd,requests\n"));
    }

    #[test]
    fn text_output_flags_partial_pricing() {
        let p = Pricing::builtin();
        let s = summarize(&sample(), &Filter::default(), &p);
        let out = text(&s, &ScanStats::default(), Some(30));
        assert!(out.contains("codex-auto-review"));
        assert!(out.contains("no price on file"));
    }
}
