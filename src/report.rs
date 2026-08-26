//! Non-interactive output: plain text, JSON, CSV.
//!
//! The TUI is the point of this tool, but statistics that can only be read by
//! a human in a terminal are hard to script against. Every view the dashboard
//! shows is also available here.

use crate::agg::{Insights, Summary, dense_daily};
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

/// The derived view: ratios, rates and rankings rather than totals.
pub fn insights_text(i: &Insights, days: Option<i64>) -> String {
    let mut o = String::new();
    let window = match days {
        Some(d) => format!("last {d} days"),
        None => "all time".to_string(),
    };
    let _ = writeln!(o, "readout insights — {window}");
    if i.is_empty() {
        let _ = writeln!(o, "\n  No usage in this window.");
        return o;
    }

    // A ratio with nothing behind it prints as an em dash, never as 0 — the
    // same rule the cost figures follow.
    let ratio =
        |v: Option<f64>, render: fn(f64) -> String| v.map_or_else(|| "—".to_string(), render);
    let _ = writeln!(o, "\n  Efficiency");
    let _ = writeln!(
        o,
        "    {:<22} {:>10}   {} of {} tokens of context served from cache",
        "cache hit ratio",
        ratio(i.cache_hit_ratio, fmt::share),
        fmt::tokens(i.tokens.cache_read),
        fmt::tokens(i.tokens.context()),
    );
    let _ = writeln!(
        o,
        "    {:<22} {:>10}   output per token of context sent",
        "output ratio",
        ratio(i.output_per_context, fmt::multiplier),
    );
    let _ = writeln!(
        o,
        "    {:<22} {:>10}   context carried by the average request",
        "context per request",
        ratio(i.context_per_request, |v| fmt::tokens(v.round() as u64)),
    );
    let _ = writeln!(
        o,
        "    {:<22} {:>10}",
        "tokens per request",
        ratio(i.tokens_per_request, |v| fmt::tokens(v.round() as u64)),
    );
    let _ = writeln!(
        o,
        "    {:<22} {:>10}",
        "requests per session",
        ratio(i.requests_per_session, |v| format!("{v:.1}")),
    );

    let cost = |v: f64| fmt::money_partial(v, i.cost_coverage);
    let _ = writeln!(o, "\n  Burn rate");
    let _ = writeln!(
        o,
        "    {:<22} {:>10}   over {} calendar days",
        "per day",
        cost(i.cost_per_day),
        i.span_days,
    );
    let _ = writeln!(
        o,
        "    {:<22} {:>10}   over {} days with activity",
        "per active day",
        cost(i.cost_per_active_day),
        i.active_days,
    );
    let _ = writeln!(o, "    {:<22} {:>10}", "per session", cost(i.cost_per_session));
    let _ = writeln!(o, "    {:<22} {:>10}", "per request", cost(i.cost_per_request));
    let _ = writeln!(
        o,
        "    {:<22} {:>10}   {}",
        "month to date",
        fmt::money_partial(i.month_to_date.cost, i.month_to_date.coverage()),
        match i.projected_month {
            Some(projected) =>
                format!("on track for {}", fmt::money_partial(projected, i.cost_coverage)),
            None => "widen the window to project the month".to_string(),
        },
    );

    if let Some(p) = &i.previous {
        let _ = writeln!(o, "\n  Against the previous {} days", p.span_days);
        let row = |o: &mut String,
                   label: &str,
                   change: crate::agg::Change,
                   value: String,
                   was: String| {
            let _ = writeln!(
                o,
                "    {:<22} {:>10}   {:>6}   was {was}",
                label,
                value,
                fmt::delta(change.ratio()),
            );
        };
        row(
            &mut o,
            "tokens",
            p.tokens,
            fmt::tokens(p.tokens.current as u64),
            fmt::tokens(p.tokens.previous as u64),
        );
        row(
            &mut o,
            "cost",
            p.cost,
            cost(p.cost.current),
            fmt::money_partial(p.cost.previous, p.previous_coverage),
        );
        row(
            &mut o,
            "requests",
            p.requests,
            fmt::count(p.requests.current as u64),
            fmt::count(p.requests.previous as u64),
        );
        row(
            &mut o,
            "sessions",
            p.sessions,
            fmt::count(p.sessions.current as u64),
            fmt::count(p.sessions.previous as u64),
        );
    }

    if !i.costly_sessions.is_empty() {
        let _ = writeln!(o, "\n  Most expensive sessions{}", ranked_note(i.session_total));
        for s in i.costly_sessions.iter().take(10) {
            let _ = writeln!(
                o,
                "    {:>10}  {:>9}  {:>6} req  {:<40}  {:<22}  {}",
                fmt::money_partial(s.priced.cost, s.priced.coverage()),
                fmt::tokens(s.tokens.total()),
                fmt::count(s.events),
                fmt::terminal_ellipsize(&s.project, 40),
                fmt::terminal_ellipsize(&s.model, 22),
                fmt::relative(s.last_ts),
            );
        }
    }

    if !i.context_heavy.is_empty() {
        let _ = writeln!(o, "\n  Context-heavy sessions (2+ requests, by context per request)");
        for s in i.context_heavy.iter().take(10).filter(|s| s.events >= 2) {
            let _ = writeln!(
                o,
                "    {:>10}  {:>9}  {:>6} req  {:<40}  {}",
                s.context_per_request().map_or_else(
                    || "—".to_string(),
                    |v| format!("{}/req", fmt::tokens(v.round() as u64))
                ),
                fmt::tokens(s.tokens.total()),
                fmt::count(s.events),
                fmt::terminal_ellipsize(&s.project, 40),
                fmt::terminal_ellipsize(&s.session, 20),
            );
        }
    }

    for (title, rows) in
        [("Cost by project", &i.costly_projects), ("Cost by model", &i.costly_models)]
    {
        if rows.is_empty() {
            continue;
        }
        let _ = writeln!(o, "\n  {title}");
        for row in rows.iter().take(10) {
            let _ = writeln!(
                o,
                "    {:>10}  {:>15}  {:>6} {:<8}  {}",
                fmt::money_partial(row.cost, row.coverage),
                fmt::count(row.tokens),
                fmt::count(row.sessions as u64),
                if row.sessions == 1 { "session" } else { "sessions" },
                fmt::terminal_ellipsize(&row.label, 48),
            );
        }
    }
    o
}

/// Search results, grouped by session and most recent first.
pub fn search_text(results: &crate::search::Results) -> String {
    let mut o = String::new();
    let _ = writeln!(
        o,
        "readout search — {}",
        fmt::terminal_ellipsize(&format!("\"{}\"", results.query), 72)
    );
    let _ = writeln!(
        o,
        "  {} {} in {} {} · {} of {} transcripts read in {}",
        results.total_matches,
        plural(results.total_matches, "match", "matches"),
        results.sessions_matched,
        plural(results.sessions_matched, "session", "sessions"),
        results.files_searched,
        results.files_total,
        fmt::duration_ms(results.elapsed_ms),
    );
    if results.files_failed > 0 {
        let _ = writeln!(
            o,
            "  note: {} {} could not be read and {} skipped",
            results.files_failed,
            plural(results.files_failed, "transcript", "transcripts"),
            plural(results.files_failed, "was", "were"),
        );
    }
    if results.truncated {
        // A cap that shows as a shorter list and says nothing reads as "that
        // is all there was".
        let _ = writeln!(o, "  note: a search limit was reached; the figures above are floors");
    }
    if results.is_empty() {
        let _ = writeln!(o, "\n  Nothing matched.");
        return o;
    }
    if results.sessions_matched > results.sessions.len() {
        let _ = writeln!(
            o,
            "  showing the {} most recent; pass --limit for more",
            results.sessions.len()
        );
    }

    for session in &results.sessions {
        let _ = writeln!(o);
        let _ = writeln!(
            o,
            "  {:<14} {:<7} {:<38} {:>4} {:<4} {}",
            fmt::terminal_ellipsize(&session.session, 14),
            session.source.short(),
            fmt::terminal_ellipsize(&session.project, 38),
            session.matches,
            plural(session.matches, "hit", "hits"),
            fmt::relative(session.last_ts_ms.div_euclid(1_000)),
        );
        for hit in &session.samples {
            // The title is what Replay labels the row: a role for a message,
            // the tool's name for a call — more use than the kind alone.
            let _ = writeln!(
                o,
                "    {:<5} {:<9} {}",
                hit_time(hit.ts_ms),
                fmt::terminal_ellipsize(&hit.title, 9),
                fmt::terminal_ellipsize(&hit.snippet, 80),
            );
        }
        if session.matches > session.samples.len() {
            let _ = writeln!(o, "    {:<5} {} more", "", session.matches - session.samples.len());
        }
    }
    o
}

pub fn search_json(results: &crate::search::Results) -> String {
    let hit = |h: &crate::search::Hit| {
        json!({
            "ts_ms": h.ts_ms,
            "kind": h.kind.label(),
            "title": h.title,
            "snippet": h.snippet,
        })
    };
    let v = json!({
        "query": results.query,
        "generated_ts": chrono::Local::now().timestamp(),
        "total_matches": results.total_matches,
        "sessions_matched": results.sessions_matched,
        "files_searched": results.files_searched,
        "files_failed": results.files_failed,
        "files_total": results.files_total,
        "bytes_read": results.bytes_read,
        // True means every count above is a floor, not a total.
        "truncated": results.truncated,
        "elapsed_ms": results.elapsed_ms,
        "sessions": results.sessions.iter().map(|s| json!({
            "session": s.session,
            "source": s.source.short(),
            "project": s.project,
            "last_ts_ms": s.last_ts_ms,
            "matches": s.matches,
            "samples": s.samples.iter().map(hit).collect::<Vec<_>>(),
        })).collect::<Vec<_>>(),
    });
    serde_json::to_string_pretty(&v).unwrap_or_else(|_| "{}".into())
}

fn hit_time(ts_ms: i64) -> String {
    crate::agg::local_datetime(ts_ms.div_euclid(1_000))
        .map_or_else(|| "—".to_string(), |dt| dt.format("%H:%M").to_string())
}

fn plural<'a>(n: usize, one: &'a str, many: &'a str) -> &'a str {
    if n == 1 { one } else { many }
}

/// Say when a ranking was cut, so a capped list never reads as the whole set.
fn ranked_note(total: usize) -> String {
    if total > crate::agg::RANKED_LIMIT {
        format!(" (top {} of {total})", crate::agg::RANKED_LIMIT)
    } else {
        String::new()
    }
}

pub fn insights_json(i: &Insights, days: Option<i64>) -> String {
    let session = |s: &crate::agg::SessionInsight| {
        json!({
            "session": s.session,
            "source": s.source.short(),
            "project": s.project,
            "model": s.model,
            "tokens": {
                "input": s.tokens.input,
                "output": s.tokens.output,
                "cache_read": s.tokens.cache_read,
                "cache_write": s.tokens.cache_write(),
                "context": s.tokens.context(),
                "total": s.tokens.total(),
            },
            "cost_usd": s.priced.cost,
            "cost_coverage": s.priced.coverage(),
            "requests": s.events,
            "context_per_request": s.context_per_request(),
            "last_ts": s.last_ts,
        })
    };
    let cost_row = |r: &crate::agg::CostRow| {
        json!({
            "label": r.label,
            "cost_usd": r.cost,
            "cost_coverage": r.coverage,
            "tokens": r.tokens,
            "requests": r.events,
            "sessions": r.sessions,
        })
    };
    let change = |c: crate::agg::Change| json!({ "current": c.current, "previous": c.previous, "delta": c.delta(), "ratio": c.ratio() });

    let v = json!({
        "window_days": days,
        "generated_ts": chrono::Local::now().timestamp(),
        "span_days": i.span_days,
        "active_days": i.active_days,
        "efficiency": {
            // Null, not zero: "nothing was sent" and "nothing came from cache"
            // are different findings and a script must be able to tell them apart.
            "cache_hit_ratio": i.cache_hit_ratio,
            "output_per_context": i.output_per_context,
            "context_per_request": i.context_per_request,
            "tokens_per_request": i.tokens_per_request,
            "requests_per_session": i.requests_per_session,
            "context_tokens": i.tokens.context(),
            "cached_context_tokens": i.tokens.cache_read,
        },
        "burn": {
            "tokens_per_day": i.tokens_per_day,
            "cost_per_day": i.cost_per_day,
            "cost_per_active_day": i.cost_per_active_day,
            "cost_per_session": i.cost_per_session,
            "cost_per_request": i.cost_per_request,
            "cost_coverage": i.cost_coverage,
            "month_to_date_usd": i.month_to_date.cost,
            "month_to_date_coverage": i.month_to_date.coverage(),
            "projected_month_usd": i.projected_month,
        },
        "previous_window": i.previous.map(|p| json!({
            "span_days": p.span_days,
            "cost_coverage": p.previous_coverage,
            "tokens": change(p.tokens),
            "cost_usd": change(p.cost),
            "requests": change(p.requests),
            "sessions": change(p.sessions),
        })),
        "ranked_limit": crate::agg::RANKED_LIMIT,
        "session_total": i.session_total,
        "costly_sessions": i.costly_sessions.iter().map(session).collect::<Vec<_>>(),
        "context_heavy_sessions": i.context_heavy.iter()
            .filter(|s| s.events >= 2).map(session).collect::<Vec<_>>(),
        "costly_projects": i.costly_projects.iter().map(cost_row).collect::<Vec<_>>(),
        "costly_models": i.costly_models.iter().map(cost_row).collect::<Vec<_>>(),
    });
    serde_json::to_string_pretty(&v).unwrap_or_else(|_| "{}".into())
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

    fn search_sample(matches: usize, truncated: bool) -> crate::search::Results {
        crate::search::Results {
            query: "deadlock".into(),
            sessions: vec![crate::search::SessionHits {
                source: Source::Claude,
                session: "s1".into(),
                project: "alpha".into(),
                last_ts_ms: chrono::Local::now().timestamp() * 1_000,
                matches,
                samples: vec![crate::search::Hit {
                    ts_ms: chrono::Local::now().timestamp() * 1_000,
                    kind: crate::replay::ReplayKind::ToolCall,
                    title: "Bash".into(),
                    snippet: "grep deadlock src".into(),
                }],
            }],
            sessions_matched: 4,
            total_matches: matches,
            files_searched: 9,
            files_failed: 0,
            files_total: 10,
            bytes_read: 1234,
            truncated,
            elapsed_ms: 12,
        }
    }

    #[test]
    fn search_output_says_when_a_list_was_cut_rather_than_just_being_shorter() {
        let out = search_text(&search_sample(9, false));
        assert!(out.contains("9 matches in 4 sessions"));
        // One session is shown of four found; silence here would read as
        // "that is all there was".
        assert!(out.contains("showing the 1 most recent"), "{out}");
        assert!(out.contains("8 more"), "the samples are a sample, not the matches: {out}");
        assert!(!out.contains("limit was reached"));

        let capped = search_text(&search_sample(9, true));
        assert!(capped.contains("floors"), "a search that stopped early must say so");
    }

    #[test]
    fn search_output_labels_a_hit_the_way_replay_would() {
        let out = search_text(&search_sample(1, false));
        assert!(out.contains("Bash"), "a tool hit names its tool, not just its kind: {out}");
        assert!(out.contains("1 hit "), "singular when there is one");

        let value: serde_json::Value =
            serde_json::from_str(&search_json(&search_sample(1, false))).unwrap();
        assert_eq!(value["sessions"][0]["samples"][0]["kind"], "tool");
        assert_eq!(value["sessions"][0]["samples"][0]["title"], "Bash");
        assert_eq!(value["truncated"], false);
    }

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
    fn insight_json_keeps_an_unmeasurable_ratio_null_rather_than_zero() {
        // A script has to be able to tell "nothing came from cache" from
        // "nothing was sent", and `0.0` says the first about both.
        let p = Pricing::builtin();
        let empty = summarize(&[], &Filter::default(), &p);
        let i = crate::agg::insights(&empty, None, Some(7));
        let v: serde_json::Value = serde_json::from_str(&insights_json(&i, Some(7))).unwrap();
        assert!(v["efficiency"]["cache_hit_ratio"].is_null());
        assert!(v["previous_window"].is_null(), "no comparison was supplied");
        assert_eq!(v["window_days"], 7);
    }

    #[test]
    fn insight_text_marks_partly_priced_figures_and_names_its_cap() {
        let p = Pricing::builtin();
        let s = summarize(&sample(), &Filter::default(), &p);
        let i = crate::agg::insights(&s, None, None);
        let out = insights_text(&i, None);
        assert!(out.contains("readout insights — all time"));
        assert!(out.contains("cache hit ratio"));
        // Half the sample is on a model with no rate, so every cost figure
        // derived from it has to carry the `+` that says "at least".
        assert!(out.contains("+"), "a partly priced burn rate must say so: {out}");
        assert!(!out.contains("top 100 of"), "nothing was truncated at this size");
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
