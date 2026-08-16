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

    // Today, always — the window totals answer "what have I spent", and the
    // question people actually ask next is "what have I spent *today*". A
    // one-day window already is today, so repeating it there says nothing.
    if days != Some(1) {
        let t = s.today();
        let _ = writeln!(o);
        let _ = writeln!(
            o,
            "  {:>15}  {:>10}  {:>9}  {:>8}   today",
            fmt::count(t.map_or(0, |b| b.tokens.total())),
            fmt::money_partial(
                t.map_or(0.0, |b| b.priced.cost),
                t.map_or(1.0, |b| b.priced.coverage())
            ),
            fmt::count(t.map_or(0, |b| b.events)),
            fmt::count(t.map_or(0, |b| b.session_count() as u64)),
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
        let width = s
            .by_model
            .iter()
            .take(12)
            .map(|b| fmt::terminal_ellipsize(&b.label, 48).chars().count())
            .max()
            .unwrap_or(8);
        for b in s.by_model.iter().take(12) {
            let label = fmt::terminal_ellipsize(&b.label, 48);
            let cost =
                if b.priced.is_complete() { fmt::money(b.priced.cost) } else { "—".to_string() };
            let _ = writeln!(
                o,
                "    {:<width$}  {:>15}  {:>10}",
                label,
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
            .map(|b| fmt::terminal_text(&b.label).chars().count().min(32))
            .max()
            .unwrap_or(8);
        for b in s.by_project.iter().take(10) {
            let _ = writeln!(
                o,
                "    {:<width$}  {:>15}  {:>10}  {}",
                fmt::terminal_ellipsize(&b.label, 32),
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
            s.unpriced_models
                .iter()
                .map(|model| fmt::terminal_text(model))
                .collect::<Vec<_>>()
                .join(", "),
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

/// `devices` 是本次汇总覆盖的设备。总量里合进了远端 usage 却不在输出里留下痕迹，
/// 会让脚本看到一个说不出理由的跳变，所以设备清单和分设备明细一起给出来。
pub fn json(
    s: &Summary,
    stats: &ScanStats,
    devices: &[crate::devices::DeviceRecord],
    days: Option<i64>,
) -> String {
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
        // Null rather than a zeroed bucket: "nothing billed today" and "today
        // is not in this window" are both real answers, and a row of zeros
        // would be indistinguishable from either.
        "today": s.today().map(bucket),
        "by_source": s.by_source.iter().map(|(src, b)| {
            let mut o = bucket(b);
            o["source"] = json!(src.short());
            o
        }).collect::<Vec<_>>(),
        "by_model": s.by_model.iter().map(bucket).collect::<Vec<_>>(),
        "by_project": s.by_project.iter().map(bucket).collect::<Vec<_>>(),
        // 一个事件若被多台设备观察到，它只落在 `@shared` 这一桶里，不会重复计入
        // 任何一台设备——分设备之和等于总量。
        "by_device": s.by_device.iter().map(|b| {
            let mut o = bucket(b);
            o["device"] = json!(device_name(devices, &b.label));
            o
        }).collect::<Vec<_>>(),
        "devices": devices.iter().map(|d| json!({
            "id": d.id,
            "name": d.name,
            "ssh_host": d.host,
            "local": d.is_local,
            "available": d.available,
            "exporter_version": d.exporter_version,
            "synced_ts": (d.generated_at > 0).then_some(d.generated_at),
            "problem": d.problem,
        })).collect::<Vec<_>>(),
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

fn device_name<'a>(devices: &'a [crate::devices::DeviceRecord], id: &'a str) -> &'a str {
    if id == crate::agg::SHARED_DEVICE_ID {
        return "Shared";
    }
    devices.iter().find(|device| device.id == id).map_or(id, |device| device.name.as_str())
}

/// Daily CSV — the shape most useful to pipe into a spreadsheet.
pub fn csv(s: &Summary, days: usize) -> String {
    let mut o = String::from("date,tokens,cost_usd,cost_coverage,requests\n");
    let dense = dense_daily(&s.daily, days);
    let by_date: std::collections::HashMap<_, _> =
        s.daily.iter().map(|d| (d.date, (d.bucket.events, d.bucket.priced.coverage()))).collect();
    for (date, tokens, cost) in dense {
        let (requests, coverage) = by_date.get(&date).copied().unwrap_or((0, 1.0));
        let _ = writeln!(o, "{date},{tokens},{cost:.6},{coverage:.6},{requests}");
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
        let model = fmt::terminal_text(&model);
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
            let _ = writeln!(o, "    {}", fmt::terminal_text(&m));
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
                observed_on: Vec::new(),
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
                observed_on: Vec::new(),
                dedup_key: None,
                dedup_rank: 0,
            },
        ]
    }

    #[test]
    fn json_output_is_parseable_and_carries_coverage() {
        let p = Pricing::builtin();
        let s = summarize(&sample(), &Filter::default(), &p);
        let out = json(&s, &ScanStats::default(), &[], Some(30));
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
        assert!(out.starts_with("date,tokens,cost_usd,cost_coverage,requests\n"));
        let today: Vec<_> = out.lines().last().unwrap().split(',').collect();
        assert!(today[3].parse::<f64>().unwrap() < 1.0, "partial cost must be explicit in CSV");
    }

    #[test]
    fn json_says_which_devices_the_totals_came_from() {
        // 默认聚合会把远端 usage 合进总量。脚本必须能从输出本身看出这一点，
        // 否则同一条命令在启用一台设备前后会给出无法解释的跳变。
        let p = Pricing::builtin();
        let mut events = sample();
        events[0].observed_on = vec!["dev-local".into()];
        events[1].observed_on = vec!["dev-local".into(), "dev-remote".into()];
        let s = summarize(&events, &Filter::default(), &p);
        let devices = [crate::devices::DeviceRecord {
            id: "dev-local".into(),
            name: "laptop".into(),
            host: None,
            exporter_version: Some("0.2.3".into()),
            generated_at: 0,
            is_local: true,
            available: true,
            enabled: true,
            discovered: true,
            problem: None,
        }];
        let v: serde_json::Value =
            serde_json::from_str(&json(&s, &ScanStats::default(), &devices, None)).unwrap();

        let by_device = v["by_device"].as_array().unwrap();
        assert_eq!(by_device.len(), 2, "one exclusive bucket plus the shared one");
        let names: Vec<_> = by_device.iter().map(|d| d["device"].as_str().unwrap()).collect();
        assert!(names.contains(&"laptop"), "ids resolve to the names on screen: {names:?}");
        assert!(names.contains(&"Shared"));
        // 复制的事件只进 @shared，所以分设备之和正好等于总量，不会重复计数。
        let summed: u64 =
            by_device.iter().map(|d| d["tokens"]["total"].as_u64().unwrap()).sum::<u64>();
        assert_eq!(summed, v["total"]["tokens"]["total"].as_u64().unwrap());
        assert_eq!(v["devices"][0]["name"], "laptop");
        assert_eq!(v["devices"][0]["local"], true);
    }

    #[test]
    fn text_output_flags_partial_pricing() {
        let p = Pricing::builtin();
        let s = summarize(&sample(), &Filter::default(), &p);
        let out = text(&s, &ScanStats::default(), Some(30));
        assert!(out.contains("codex-auto-review"));
        assert!(out.contains("no price on file"));
    }

    #[test]
    fn text_output_reports_today_beside_the_window() {
        let p = Pricing::builtin();
        let s = summarize(&sample(), &Filter::default(), &p);
        let out = text(&s, &ScanStats::default(), Some(30));
        let today =
            out.lines().find(|l| l.ends_with("   today")).expect("a today row under the totals");
        assert!(today.contains("400"), "the sample was all billed today: {today}");
    }

    #[test]
    fn a_one_day_window_does_not_repeat_itself_as_today() {
        let p = Pricing::builtin();
        let s = summarize(&sample(), &Filter::default(), &p);
        let out = text(&s, &ScanStats::default(), Some(1));
        assert!(!out.lines().any(|l| l.ends_with("   today")), "TOTAL already is today");
    }

    #[test]
    fn json_carries_today_and_null_when_there_is_none() {
        let p = Pricing::builtin();

        let s = summarize(&sample(), &Filter::default(), &p);
        let v: serde_json::Value =
            serde_json::from_str(&json(&s, &ScanStats::default(), &[], None)).unwrap();
        assert_eq!(v["today"]["tokens"]["total"], 400);

        // Nothing billed today: the key stays, the value says so.
        let old: Vec<UsageEvent> = sample()
            .into_iter()
            .map(|mut e| {
                e.ts -= 3 * 86_400;
                e
            })
            .collect();
        let s = summarize(&old, &Filter::default(), &p);
        let v: serde_json::Value =
            serde_json::from_str(&json(&s, &ScanStats::default(), &[], None)).unwrap();
        assert!(v["today"].is_null());
    }
}
