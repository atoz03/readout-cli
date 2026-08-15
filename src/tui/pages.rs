//! Page rendering.
//!
//! Chart forms follow from the job each number does: magnitude over time is a
//! bar series on a single axis, share-of-total is a ranked bar list, and a
//! headline figure with no distribution behind it is a stat tile rather than
//! a chart. Nothing here uses two y-scales, a pie, or a rainbow ramp.

use crate::agg::{Summary, current_streak, dense_daily, month_to_date};
use crate::fmt;
use crate::model::Source;
use crate::replay::{ReplayEvent, ReplayKind};
use crate::tui::app::{App, Drill, Loading, Page, Range};
use crate::tui::hit::{Action, Registry};
use crate::tui::theme;
use crate::tui::widgets::{self as w, BarRow, Card};
use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};

pub const SIDEBAR_W: u16 = 18;

/// Titles for the hour histogram. Across a long window the bars are a working
/// habit; across one day they are that day as far as it has got.
const HOURS_HABIT: &str = "When You Work";
const HOURS_TODAY: &str = "Today, by Hour";

/// A list that can own the keyboard selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NavList {
    Models,
    Projects,
    Days,
    Sessions,
    ReplayEvents,
    Devices,
    Rates,
    Settings,
}

/// Whether this list is the one ↑↓ drives on the current page.
///
/// Several lists share the screen — Overview draws models and recent sessions
/// side by side — but only one answers to the keyboard, and it is the one
/// `App::row_count` counts. The scroll window has to follow that list and no
/// other, or `End` on Overview scrolls the models by a row count taken from
/// the sessions card beside it.
fn owns_selection(page: Page, list: NavList) -> bool {
    match page {
        Page::Overview | Page::Models => list == NavList::Models,
        Page::Projects => list == NavList::Projects,
        Page::Daily => list == NavList::Days,
        Page::Sessions => list == NavList::Sessions,
        Page::Replay => list == NavList::ReplayEvents,
        Page::Devices => list == NavList::Devices,
        Page::Pricing => list == NavList::Rates,
        Page::Settings => list == NavList::Settings,
    }
}

pub fn draw(app: &mut App, buf: &mut Buffer, area: Rect) {
    w::fill(buf, area, theme::SURFACE);
    let mut hits = std::mem::take(&mut app.hits);
    hits.clear();

    let [header, body, footer] =
        Layout::vertical([Constraint::Length(2), Constraint::Min(6), Constraint::Length(1)])
            .areas(area);
    let [sidebar, content] =
        Layout::horizontal([Constraint::Length(SIDEBAR_W), Constraint::Min(20)]).areas(body);

    draw_header(app, buf, &mut hits, header);
    draw_sidebar(app, buf, &mut hits, sidebar);

    let content = Rect {
        x: content.x + 1,
        y: content.y,
        width: content.width.saturating_sub(2),
        height: content.height,
    };
    // With nothing to show, the page's own cards would each repeat a variant
    // of "no data" around empty axes. One message that says *why* is more use
    // than five that say *that*. Pricing is exempt: its table is a list of
    // rates, which exists whether or not any of them were used.
    if is_empty(app) && app.page != Page::Pricing {
        draw_empty(app, buf, &mut hits, content);
    } else {
        match app.page {
            Page::Overview => overview(app, buf, &mut hits, content),
            Page::Daily => daily(app, buf, &mut hits, content),
            Page::Models => ranked(app, buf, &mut hits, content, RankKind::Model),
            Page::Projects => ranked(app, buf, &mut hits, content, RankKind::Project),
            Page::Sessions => sessions(app, buf, &mut hits, content),
            Page::Replay => replay(app, buf, &mut hits, content),
            Page::Devices => devices(app, buf, &mut hits, content),
            Page::Pricing => pricing(app, buf, &mut hits, content),
            Page::Settings => settings(app, buf, &mut hits, content),
        }
    }

    draw_footer(app, buf, &mut hits, footer);
    draw_loading(app, buf, content);
    app.hits = hits;
}

/// The empty state: why there is nothing here, and what to do about it.
///
/// An empty dashboard is ambiguous — no usage, no transcripts, or a filter
/// that matches nothing all look identical. This says which, and is skipped
/// while the scan is still running because the answer is not known yet.
fn is_empty(app: &App) -> bool {
    // Mid-scan the answer is not known yet, so the dashboard keeps drawing
    // what it has rather than claiming there is nothing.
    app.summary.total.events == 0
        && !matches!(app.loading, Loading::Scanning(_))
        && !matches!(app.page, Page::Pricing | Page::Devices | Page::Settings)
}

fn draw_empty(app: &App, buf: &mut Buffer, hits: &mut Registry, area: Rect) {
    let msg = empty_message(app, &app.summary);
    if msg.is_empty() || area.height < 3 {
        return;
    }
    let y = area.y + area.height / 2;
    let band = Rect { x: area.x, y: y.saturating_sub(1), width: area.width, height: 3 };
    w::fill(buf, band, theme::SURFACE);
    let action = (app.drill != Drill::None).then_some(Action::ClearFilter);
    w::callout(buf, hits, band, theme::ICON_INFO, theme::TEXT_MUTED, &msg, action);
}

/// Scan progress, shown over the content area while the corpus is read.
///
/// The dashboard paints before the scan finishes, so this is what fills the
/// gap. It reports files and bytes rather than a bare spinner, because a cold
/// scan of a large corpus should look like work in progress, not a hang.
fn draw_loading(app: &App, buf: &mut Buffer, area: Rect) {
    let Loading::Scanning(progress) = &app.loading else { return };
    if area.width < 24 || area.height < 5 {
        return;
    }
    let w = area.width.min(52);
    let panel = Rect {
        x: area.x + (area.width - w) / 2,
        y: area.y + area.height / 2 - 2,
        width: w,
        height: 4,
    };
    w::fill(buf, panel, theme::SURFACE_ACTIVE);

    let frame = crate::tui::anim::SPINNER
        [app.pulse.frame(crate::tui::anim::SPINNER.len(), std::time::Duration::from_millis(800))];
    let (fraction, detail) = match progress {
        Some(p) if p.total > 0 => (
            p.done as f64 / p.total as f64,
            format!("{} / {} files · {}", p.done, p.total, fmt::bytes(p.bytes_read)),
        ),
        _ => (0.0, "finding transcripts…".to_string()),
    };
    w::text(
        buf,
        panel.x + 2,
        panel.y,
        panel.width,
        &format!("{frame}  Reading transcripts"),
        Style::default().fg(theme::TEXT_PRIMARY).add_modifier(Modifier::BOLD),
    );
    w::progress(
        buf,
        Rect { x: panel.x + 2, y: panel.y + 2, width: panel.width.saturating_sub(4), height: 2 },
        fraction,
        &detail,
    );
}

fn draw_header(app: &App, buf: &mut Buffer, hits: &mut Registry, area: Rect) {
    w::fill(buf, area, theme::SURFACE);
    let y = area.y;

    let mut x = area.x + 1;
    x += w::text(
        buf,
        x,
        y,
        10,
        "readout",
        Style::default().fg(theme::TEXT_PRIMARY).add_modifier(Modifier::BOLD),
    );
    x += 2;

    // Tool toggles carry their own reserved hue and a word, so which tools are
    // on is never signalled by color alone.
    for s in Source::ALL {
        if !app.sources.contains(&s) {
            continue;
        }
        let on = !app.disabled.contains(&s);
        let label = format!("{} {}", if on { theme::DOT } else { "○" }, s.label());
        let width = label.chars().count() as u16;
        let style = if on {
            Style::default().fg(theme::source_color(s))
        } else {
            Style::default().fg(theme::TEXT_MUTED)
        };
        w::text(buf, x, y, width, &label, style);
        hits.add(Rect { x, y, width, height: 1 }, Action::ToggleSource(s));
        x += width + 2;
    }

    // A mouse-driven dashboard needs a mouse-reachable exit; keyboard users
    // still have q. Dropped on narrow terminals, where the chips matter more.
    let quit_w = if area.width >= 70 { 3u16 } else { 0 };
    if quit_w > 0 {
        let qx = area.right().saturating_sub(quit_w);
        let hovered = app.hover == Some(w::hover_id("quit"));
        let style = if hovered {
            Style::default().fg(theme::CRITICAL).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme::TEXT_MUTED)
        };
        w::text(buf, qx, y, quit_w, " ✕ ", style);
        hits.add_hoverable(
            Rect { x: qx, y, width: quit_w, height: 1 },
            Action::Quit,
            w::hover_id("quit"),
        );
    }

    // Range chips, right-aligned. "Today" says what it is; "1d" is what fits
    // once the tool toggles have taken the left half of a narrow header.
    let long = area.width >= 76;
    let chips: Vec<String> = Range::ORDER
        .iter()
        .map(|r| format!(" {} ", if long { r.label() } else { r.label_short() }))
        .collect();
    let total: u16 = chips.iter().map(|c| c.chars().count() as u16).sum();
    let mut cx = area.right().saturating_sub(total + quit_w + 1);
    for (r, label) in Range::ORDER.iter().zip(&chips) {
        let width = label.chars().count() as u16;
        let active = app.range == *r;
        let style = if active {
            Style::default()
                .fg(theme::TEXT_PRIMARY)
                .bg(theme::SURFACE_ACTIVE)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme::TEXT_MUTED)
        };
        w::text(buf, cx, y, width, label, style);
        hits.add(Rect { x: cx, y, width, height: 1 }, Action::Range(*r));
        cx += width;
    }

    // Sub-line: the sentence that tells you what you are looking at.
    let width = area.width.saturating_sub(2);
    let sub = headline(app, width);
    w::text(buf, area.x + 1, y + 1, width, &sub, Style::default().fg(theme::TEXT_MUTED));
}

fn headline(app: &App, width: u16) -> String {
    if app.page == Page::Replay
        && let Some(request) = app.replay.request.as_ref()
    {
        let text = format!(
            "{} › {} · {} · {}",
            request.project,
            request.session,
            request.source.label(),
            request.model
        );
        return fmt::ellipsize(&text, width as usize);
    }
    if app.page == Page::Settings {
        return fmt::ellipsize(
            &format!(
                "Preferences for {} · {} enabled SSH hosts",
                app.settings.device.name,
                app.settings.ssh_hosts.len()
            ),
            width as usize,
        );
    }
    if app.page == Page::Devices {
        return fmt::ellipsize(
            &format!("{} devices · remote snapshots contain usage only", app.devices.len()),
            width as usize,
        );
    }
    let s = &app.summary;
    if let Drill::Device(id) = &app.drill {
        return format!("Filtered to device: {} — press Esc to clear", app.device_name(id));
    }
    if let Some(drill) = app.drill.label() {
        return format!("Filtered to {drill} — press Esc to clear");
    }
    if s.total.events == 0 {
        return "No usage in this window.".into();
    }
    let streak = match current_streak(&s.daily) {
        0 | 1 => String::new(),
        n => format!(" · {n}-day streak"),
    };
    let shape = format!(
        "{} sessions across {} projects and {} models",
        fmt::count(s.total.session_count() as u64),
        s.by_project.len(),
        s.by_model.len(),
    );

    // Under a one-day window the tiles above are already today's, so leading
    // with today would be the same number twice.
    if app.range == Range::Today {
        return format!("{shape}{streak}");
    }

    // Everywhere else the tiles show the window and this shows the day, which
    // is the figure people actually keep an eye on. It comes first, and it is
    // the last thing dropped as the terminal narrows.
    let today = match s.today() {
        Some(b) => format!(
            "Today {} · {}",
            fmt::money_partial(b.priced.cost, b.priced.coverage()),
            fmt::tokens(b.tokens.total()),
        ),
        None => "Nothing today yet".to_string(),
    };
    if width >= 92 {
        format!("{today}{streak} — {shape}")
    } else if width >= 40 {
        format!("{today}{streak}")
    } else {
        today
    }
}

fn draw_sidebar(app: &App, buf: &mut Buffer, hits: &mut Registry, area: Rect) {
    w::fill(buf, area, theme::SURFACE);
    let mut y = area.y;
    for (group, pages) in Page::GROUPS {
        if y >= area.bottom() {
            break;
        }
        w::text(
            buf,
            area.x + 1,
            y,
            area.width,
            &group.to_uppercase(),
            Style::default().fg(theme::TEXT_MUTED),
        );
        y += 1;
        for page in pages {
            if y >= area.bottom() {
                break;
            }
            let active = app.page == *page || (app.page == Page::Replay && *page == Page::Sessions);
            let row = Rect { x: area.x + 1, y, width: area.width.saturating_sub(2), height: 1 };
            if active {
                // The active pill is a filled surface plus a bold label — the
                // selection survives a monochrome terminal.
                w::fill(buf, row, theme::SURFACE_ACTIVE);
            }
            let style = if active {
                Style::default().fg(theme::TEXT_PRIMARY).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(theme::TEXT_SECONDARY)
            };
            w::text(buf, row.x + 1, y, 2, page.icon(), style);
            w::text(buf, row.x + 3, y, row.width.saturating_sub(3), page.title(), style);
            hits.add_hoverable(row, Action::Page(*page), w::hover_id(page.title()));
            y += 1;
        }
        y += 1;
    }
}

fn draw_footer(app: &App, buf: &mut Buffer, hits: &mut Registry, area: Rect) {
    w::fill(buf, area, theme::SURFACE_RAISED);
    let right = match &app.loading {
        Loading::Scanning(_) => "scanning…".to_string(),
        Loading::Failed(e) => format!("scan failed: {e}"),
        Loading::Done => format!(
            "{} files · {} · {}",
            app.stats.files_total,
            fmt::bytes(app.stats.bytes_read),
            fmt::duration_ms(app.stats.total_ms)
        ),
    };
    let rw = right.chars().count() as u16;
    let rx = area.right().saturating_sub(rw + 1);
    let right_style = match &app.loading {
        Loading::Failed(_) => Style::default().fg(theme::CRITICAL),
        _ => Style::default().fg(theme::TEXT_MUTED),
    };
    w::text(buf, rx, area.y, rw, &right, right_style);
    hits.add(Rect { x: rx, y: area.y, width: rw, height: 1 }, Action::Refresh);

    // Watch chip, left of the scan summary. Dot plus word, like the tool
    // toggles, so whether it is on never rests on colour alone. Dropped on a
    // footer too narrow to hold it and a hint both; `w` still works there.
    let mut edge = rx;
    if area.width >= 44 {
        let watch = if app.watch { "● live" } else { "○ live" };
        let ww = watch.chars().count() as u16;
        let wx = rx.saturating_sub(ww + 2);
        let style = if app.watch {
            Style::default().fg(theme::SERIES[2])
        } else {
            Style::default().fg(theme::TEXT_MUTED)
        };
        w::text(buf, wx, area.y, ww, watch, style);
        hits.add(Rect { x: wx, y: area.y, width: ww, height: 1 }, Action::ToggleWatch);
        edge = wx;
    }

    // The hint is chosen against the room the scan summary leaves, not the
    // terminal width: sized against the latter it gets clipped mid-word, and
    // "q qui" is not an instruction.
    let room = edge.saturating_sub(area.x + 2);
    // Each threshold is the exact width of the string it admits, so a hint is
    // either whole or replaced by a shorter whole one — never trimmed.
    let left = match &app.status {
        Some(msg) => msg.clone(),
        None if app.page == Page::Replay && room >= 67 => {
            "space play/pause · [ ] step · 1/2/4 speed · esc sessions · q quit".into()
        }
        None if app.page == Page::Replay && room >= 35 => {
            "space play · [ ] step · esc sessions · q quit".into()
        }
        None if app.page == Page::Replay => "space play · esc back · q quit".into(),
        None if room >= 83 => {
            "↑↓ move · ⏎ drill · esc clear · tab page · t/1-4 range · r rescan · w live · q quit"
                .into()
        }
        None if room >= 55 => "↑↓ ⏎ esc tab · t/1-4 range · r rescan · w live · q quit".into(),
        None if room >= 38 => "↑↓ ⏎ esc · t/1-4 · r · w live · q quit".into(),
        None if room >= 24 => "? help · w live · q quit".into(),
        None if room >= 15 => "? help · q quit".into(),
        None => "q quit".into(),
    };
    w::text(buf, area.x + 1, area.y, room, &left, Style::default().fg(theme::TEXT_MUTED));
}

// ── Overview ────────────────────────────────────────────────────────────────

fn overview(app: &App, buf: &mut Buffer, hits: &mut Registry, area: Rect) {
    // Allocate by priority rather than fixed heights. A short window that
    // divides its space evenly ends up with a chart too small to plot and a
    // list too small to list; dropping a whole section is more useful than
    // rendering two stubs.
    let has_callout = !app.summary.unpriced_models.is_empty();
    let callout_h = if has_callout && area.height >= 16 { 4 } else { 0 };
    let tiles_h = if area.height >= 8 { 4 } else { area.height };
    let rest = area.height.saturating_sub(tiles_h + callout_h);
    let charts_h = if rest >= 16 {
        8
    } else if rest >= 12 {
        rest - 6
    } else {
        0
    };
    let lists_h = rest - charts_h;

    let [tiles, activity, split, callout_area] = Layout::vertical([
        Constraint::Length(tiles_h),
        Constraint::Length(charts_h),
        Constraint::Length(lists_h),
        Constraint::Length(callout_h),
    ])
    .areas(area);

    kpi_row(app, buf, hits, tiles);

    if charts_h > 0 {
        // A one-day window has nothing to trend: the chart would be a single
        // bar next to its own axis. The hours of that day are the shape there
        // is, so they take the whole row.
        if app.range == Range::Today {
            hour_card(app, buf, hits, activity, HOURS_TODAY);
        } else if activity.width >= 64 {
            // Below ~64 columns the two charts would each be too narrow to
            // read; give the width to the trend, which is the one that
            // answers "how much, lately".
            let [trend, clock] =
                Layout::horizontal([Constraint::Percentage(60), Constraint::Percentage(40)])
                    .areas(activity);
            trend_card(app, buf, hits, trend);
            hour_card(app, buf, hits, clock, HOURS_HABIT);
        } else {
            trend_card(app, buf, hits, activity);
        }
    }

    if lists_h > 0 {
        if split.width >= 64 {
            let [models, recent] =
                Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)])
                    .areas(split);
            model_card(app, buf, hits, models);
            recent_card(app, buf, hits, recent);
        } else {
            model_card(app, buf, hits, split);
        }
    }

    if has_callout {
        let n = app.summary.unpriced_models.len();
        let pct = (1.0 - app.summary.total.priced.coverage()) * 100.0;
        let share = fmt::share(1.0 - app.summary.total.priced.coverage());
        // Filling the price table down to one straggler made "1 models" a
        // thing the user actually sees.
        let (models, have) = if n == 1 { ("model", "has") } else { ("models", "have") };
        // Sized to fit at three widths, because the short form still overran
        // a narrow terminal and a clipped warning reads as a rendering bug.
        let msg = if callout_area.width >= 100 {
            format!(
                "{n} {models} {have} no rate — {share} of tokens are excluded from cost. \
                 Run `readout pricing --init` to add rates."
            )
        } else if callout_area.width >= 62 {
            format!("{n} {models} unpriced · {share} of tokens excluded from cost")
        } else {
            format!("{share} of tokens unpriced")
        };
        // Reserved status hues, escalating with the share of spend the gap
        // hides: a couple of missing rates is a nudge, most of the corpus
        // missing means the cost figure on this page is not the real one.
        let severity = if pct >= 50.0 { theme::SERIOUS } else { theme::WARNING };
        w::callout(
            buf,
            hits,
            callout_area,
            theme::ICON_WARNING,
            severity,
            &msg,
            Some(Action::Page(Page::Pricing)),
        );
    }
}

fn kpi_row(app: &App, buf: &mut Buffer, hits: &mut Registry, area: Rect) {
    let cols = Layout::horizontal([Constraint::Ratio(1, 4); 4]).spacing(1).split(area);
    let s = &app.summary;
    // A tile's label is the only thing naming its number, so it shortens
    // rather than clips: "Est. cos" and "Request" are not words.
    let tile_w = cols[1].width;
    let narrow = tile_w < 13;
    let cost_label = match (s.total.priced.is_complete(), tile_w) {
        (true, w) if w < 13 => "Cost",
        (true, _) => "Est. cost",
        (false, w) if w >= 22 => "Est. cost (partial)",
        (false, w) if w >= 13 => "Est. cost +",
        (false, _) => "Cost +",
    };
    let tiles: [(String, &str, ratatui::style::Color, Action); 4] = [
        (
            fmt::tokens(app.kpi[0].value() as u64),
            "Tokens",
            theme::SERIES[0],
            Action::Page(Page::Models),
        ),
        (
            fmt::money_partial(app.kpi[1].value(), s.total.priced.coverage()),
            cost_label,
            theme::SERIES[3],
            Action::Page(Page::Models),
        ),
        (
            fmt::count(app.kpi[2].value() as u64),
            if narrow { "Reqs" } else { "Requests" },
            theme::SERIES[2],
            Action::Page(Page::Daily),
        ),
        (
            fmt::count(app.kpi[3].value() as u64),
            if narrow { "Runs" } else { "Sessions" },
            theme::SERIES[4],
            Action::Page(Page::Sessions),
        ),
    ];
    for (i, (value, label, accent, action)) in tiles.into_iter().enumerate() {
        let hovered = app.hover == Some(w::hover_id(label));
        w::kpi_tile(buf, hits, cols[i], &value, label, accent, Some(action), hovered);
    }
}

fn trend_card(app: &App, buf: &mut Buffer, hits: &mut Registry, area: Rect) {
    // One column per day, so the window is whatever actually fits. Claiming
    // "252d" while drawing the last 78 would misstate the axis.
    let days = app
        .range
        .chart_days(app.summary.first_ts)
        .min(area.width.saturating_sub(2).max(7) as usize);
    let inner = w::card(
        buf,
        hits,
        area,
        Card {
            title: "Activity",
            glyph: "▤",
            glyph_color: theme::SERIES[0],
            meta: Some(format!("{days}d")),
            action: Some(Action::Page(Page::Daily)),
        },
    );
    if inner.height < 2 {
        return;
    }
    let dense = dense_daily(&app.summary.daily, days);
    let values: Vec<u64> = dense.iter().map(|(_, t, _)| *t).collect();
    let step = (days / 6).max(1);
    let labels: Vec<String> = dense
        .iter()
        .enumerate()
        .map(|(i, (d, _, _))| if i % step == 0 { fmt::short_date(*d) } else { String::new() })
        .collect();

    let plot = Rect { x: inner.x, y: inner.y, width: inner.width, height: inner.height - 1 };
    w::vbars(buf, hits, plot, &values, &labels, |_| theme::SERIES[0], app.grow.value(), None, None);

    // The annotation must describe the window that was drawn. Reporting the
    // all-time peak under a 57-day chart points at a bar that is not there.
    if let Some((date, tokens, _)) =
        dense.iter().max_by_key(|(_, t, _)| *t).filter(|(_, t, _)| *t > 0)
    {
        let note =
            format!("Busiest: {} with {} tokens", fmt::short_date(*date), fmt::tokens(*tokens));
        w::text(
            buf,
            inner.x,
            inner.bottom().saturating_sub(1),
            inner.width,
            &note,
            Style::default().fg(theme::TEXT_MUTED),
        );
    }
}

/// A 24-hour histogram of the window.
///
/// The title is the caller's because the same bars mean two different things:
/// across a long window they are a habit, and across one day they are that
/// day's shape so far.
fn hour_card(app: &App, buf: &mut Buffer, hits: &mut Registry, area: Rect, title: &'static str) {
    let inner = w::card(
        buf,
        hits,
        area,
        Card { title, glyph: "◷", glyph_color: theme::SERIES[5], meta: None, action: None },
    );
    if inner.height < 3 {
        return;
    }
    let hourly: Vec<u64> = app.summary.by_hour.iter().map(|b| b.tokens.total()).collect();
    let plot = Rect { x: inner.x, y: inner.y, width: inner.width, height: inner.height - 1 };
    let (values, labels, hour_of_col) = fit_day(&hourly, plot.width);
    w::vbars(
        buf,
        hits,
        plot,
        &values,
        &labels,
        move |i| theme::hour_band(hour_of_col(i)).0,
        app.grow.value(),
        None,
        None,
    );

    // Four bands need ~40 columns spelled out; abbreviate rather than drop
    // entries, since a legend missing a band is worse than a terse one.
    let names: [&str; 4] = if inner.width >= 42 {
        ["night", "morning", "afternoon", "evening"]
    } else {
        ["nite", "morn", "aftn", "eve"]
    };
    let items: Vec<(String, ratatui::style::Color)> = names
        .iter()
        .enumerate()
        .map(|(i, name)| ((*name).to_string(), theme::hour_band([0, 8, 14, 20][i]).0))
        .collect();
    w::legend(
        buf,
        Rect { x: inner.x, y: inner.bottom().saturating_sub(1), width: inner.width, height: 1 },
        &items,
    );
}

/// Lay a 24-hour histogram out in exactly `width` columns.
///
/// A day is a closed set, not a time series: cropping it to the columns that
/// happen to fit would drop the small hours and still label the axis "12a".
/// So when space is short, hours are merged into wider buckets, and when it is
/// plentiful each hour gets two columns for a more readable silhouette. The
/// returned closure maps a column back to an hour, so the time-of-day bands
/// stay correct at every size.
///
/// Returns `(values, axis labels, column → hour)`.
fn fit_day(hourly: &[u64], width: u16) -> (Vec<u64>, Vec<String>, impl Fn(usize) -> usize) {
    let w = width.max(1) as usize;
    // Columns per hour when we have room, else hours merged per column. The
    // merge factor is derived from the width rather than picked from a ladder,
    // so there is no width at which the layout runs out of buckets.
    let per_hour = if w >= 48 { 2usize } else { 1 };
    let per_col = if w >= 24 { 1usize } else { 24usize.div_ceil(w).min(24) };

    let cols = 24usize.div_ceil(per_col) * per_hour;
    let mut values = Vec::with_capacity(cols);
    let mut labels = vec![String::new(); cols];
    for (c, label) in labels.iter_mut().enumerate() {
        let hour = (c / per_hour) * per_col;
        let sum = (hour..(hour + per_col).min(24))
            .fold(0u64, |total, h| total.saturating_add(hourly.get(h).copied().unwrap_or(0)));
        // When an hour spans two columns both carry its full height: the bar
        // gets wider, not taller. Splitting the sum between them would halve
        // the silhouette purely because the terminal was wide.
        values.push(sum);
        // Label every six hours, at the column where that hour starts.
        if hour.is_multiple_of(6) && c.is_multiple_of(per_hour) {
            *label = fmt::hour_label(hour);
        }
    }
    (values, labels, move |c: usize| (c / per_hour) * per_col)
}

fn model_card(app: &App, buf: &mut Buffer, hits: &mut Registry, area: Rect) {
    let inner = w::card(
        buf,
        hits,
        area,
        Card {
            // The bars encode tokens, not dollars — most of this corpus is
            // unpriced, and a "cost" chart drawn from token counts would lie.
            title: "Usage by Model",
            glyph: "◱",
            glyph_color: theme::SERIES[3],
            meta: Some(format!("{} models", app.summary.by_model.len())),
            action: Some(Action::Page(Page::Models)),
        },
    );
    // Interactive on Overview too. `owns_selection` gives this list the arrow
    // keys on this page, so drawing it inert meant ↑↓ moved a selection with
    // nothing on screen to show for it — the keys looked broken.
    bar_list(app, buf, hits, inner, RankKind::Model, true);
}

fn recent_card(app: &App, buf: &mut Buffer, hits: &mut Registry, area: Rect) {
    let inner = w::card(
        buf,
        hits,
        area,
        Card {
            title: "Recent Sessions",
            glyph: "◷",
            glyph_color: theme::SERIES[2],
            meta: Some(format!("{} total", app.summary.total.session_count())),
            action: Some(Action::Page(Page::Sessions)),
        },
    );
    session_list(app, buf, hits, inner, 0);
}

// ── Daily ───────────────────────────────────────────────────────────────────

fn daily(app: &App, buf: &mut Buffer, hits: &mut Registry, area: Rect) {
    let [chart_area, table_area] =
        Layout::vertical([Constraint::Length(14), Constraint::Min(4)]).areas(area);

    // A one-day window plots one bar, which is a number wearing a chart's
    // clothes. Show that day's hours instead; the table below still lists the
    // day, so nothing is lost.
    if app.range == Range::Today {
        hour_card(app, buf, hits, chart_area, HOURS_TODAY);
        day_table(app, buf, hits, table_area);
        return;
    }

    // One column per day: the window is what fits, and the label says so.
    let days = app
        .range
        .chart_days(app.summary.first_ts)
        .min(chart_area.width.saturating_sub(2).max(7) as usize);

    let inner = w::card(
        buf,
        hits,
        chart_area,
        Card {
            title: "Daily Tokens",
            glyph: "▤",
            glyph_color: theme::SERIES[0],
            meta: Some(format!("{days}d")),
            action: None,
        },
    );
    let dense = dense_daily(&app.summary.daily, days);
    let values: Vec<u64> = dense.iter().map(|(_, t, _)| *t).collect();
    let step = (days / 8).max(1);
    let labels: Vec<String> = dense
        .iter()
        .enumerate()
        .map(|(i, (d, _, _))| if i % step == 0 { fmt::short_date(*d) } else { String::new() })
        .collect();
    let hovered = app.hover.map(|h| h as usize);
    let action = |i: usize| Action::Row(i);
    w::vbars(
        buf,
        hits,
        inner,
        &values,
        &labels,
        |_| theme::SERIES[0],
        app.grow.value(),
        hovered,
        Some(&action),
    );

    day_table(app, buf, hits, table_area);
}

fn day_table(app: &App, buf: &mut Buffer, hits: &mut Registry, table_area: Rect) {
    let inner = w::card(
        buf,
        hits,
        table_area,
        Card {
            title: "By Day",
            glyph: "▤",
            glyph_color: theme::SERIES[0],
            meta: Some(format!("{} active days", app.summary.daily.len())),
            action: None,
        },
    );
    if inner.height == 0 {
        return;
    }
    let max = app.summary.daily.iter().map(|d| d.bucket.tokens.total()).max().unwrap_or(1).max(1);
    let rows = inner.height as usize;
    if owns_selection(app.page, NavList::Days) {
        app.list_rows.set(rows);
    }
    // Most recent first: the day you care about is today, not the oldest.
    let ordered: Vec<_> = app.summary.daily.iter().rev().collect();
    for (i, d) in ordered.iter().enumerate().skip(app.scroll).take(rows) {
        let y = inner.y + (i - app.scroll) as u16;
        let value = format!(
            "{:>10}  {:>9}  {:>6} req",
            fmt::count(d.bucket.tokens.total()),
            fmt::money_partial(d.bucket.priced.cost, d.bucket.priced.coverage()),
            fmt::count(d.bucket.events),
        );
        let row = Rect { x: inner.x, y, width: inner.width, height: 1 };
        w::bar_row(
            buf,
            row,
            12,
            value.chars().count() as u16,
            BarRow {
                label: &d.date.format("%a %b %-d").to_string(),
                value: &value,
                fraction: d.bucket.tokens.total() as f64 / max as f64 * app.grow.value(),
                color: theme::SERIES[0],
                selected: i == app.selected,
                hovered: app.hover == Some(i as u64),
            },
        );
        hits.add_hoverable(row, Action::Row(i), i as u64);
    }
}

// ── Ranked lists ────────────────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq)]
enum RankKind {
    Model,
    Project,
}

fn ranked(app: &App, buf: &mut Buffer, hits: &mut Registry, area: Rect, kind: RankKind) {
    let (title, glyph, color) = match kind {
        RankKind::Model => ("Model Usage", "◱", theme::SERIES[0]),
        RankKind::Project => ("By Project", "▣", theme::SERIES[2]),
    };
    let n = match kind {
        RankKind::Model => app.summary.by_model.len(),
        RankKind::Project => app.summary.by_project.len(),
    };
    let inner = w::card(
        buf,
        hits,
        area,
        Card {
            title,
            glyph,
            glyph_color: color,
            meta: Some(format!("{n}")),
            action: app.drill.label().map(|_| Action::ClearFilter),
        },
    );
    bar_list(app, buf, hits, inner, kind, true);
}

fn bar_list(
    app: &App,
    buf: &mut Buffer,
    hits: &mut Registry,
    area: Rect,
    kind: RankKind,
    interactive: bool,
) {
    if area.height == 0 || area.width < 24 {
        return;
    }
    let buckets = match kind {
        RankKind::Model => &app.summary.by_model,
        RankKind::Project => &app.summary.by_project,
    };
    if buckets.is_empty() {
        w::text(
            buf,
            area.x,
            area.y,
            area.width,
            "no data in this window",
            Style::default().fg(theme::TEXT_MUTED),
        );
        return;
    }
    let max = buckets.iter().map(|b| b.tokens.total()).max().unwrap_or(1).max(1);
    let label_w = buckets
        .iter()
        .map(|b| b.label.chars().count() as u16)
        .max()
        .unwrap_or(10)
        .clamp(8, (area.width / 3).max(8));

    // Values are pre-rendered so the column is sized to what it must hold. A
    // width derived from the area instead would silently clip "$700.84" to
    // "$70" — a wrong number, not a shortened one.
    let values: Vec<String> = buckets
        .iter()
        .map(|b| {
            // A partly-priced row shows the floor it is sure of, not a dash:
            // the CLI reports `$20.55+` for the same row, and a dashboard that
            // silently drops that number disagrees with it.
            format!(
                "{:>8} {:>9}",
                fmt::tokens(b.tokens.total()),
                fmt::money_partial(b.priced.cost, b.priced.coverage()),
            )
        })
        .collect();
    let value_w = values.iter().map(|v| v.chars().count() as u16).max().unwrap_or(10);
    let rows = area.height as usize;
    let nav = match kind {
        RankKind::Model => NavList::Models,
        RankKind::Project => NavList::Projects,
    };
    if owns_selection(app.page, nav) {
        app.list_rows.set(rows);
    }

    for (i, b) in buckets.iter().enumerate().skip(app.scroll).take(rows) {
        let y = area.y + (i - app.scroll) as u16;
        let value = &values[i];
        let row = Rect { x: area.x, y, width: area.width, height: 1 };
        w::bar_row(
            buf,
            row,
            label_w,
            value_w,
            BarRow {
                label: &b.label,
                value,
                fraction: b.tokens.total() as f64 / max as f64 * app.grow.value(),
                // Color follows the entity, so a model keeps its hue when the
                // filter changes the ranking.
                color: theme::series_for(&b.label),
                selected: interactive && i == app.selected,
                hovered: app.hover == Some(i as u64),
            },
        );
        if interactive {
            // Row, not a drill action: one click selects and a second opens,
            // the same contract every other list here honours. Drilling on the
            // first click made a passing click rewrite the whole dashboard.
            let action = match kind {
                RankKind::Model => Action::Row(i),
                RankKind::Project => Action::ProjectRow(i),
            };
            hits.add_hoverable(row, action, i as u64);
        }
    }
}

// ── Sessions ────────────────────────────────────────────────────────────────

fn sessions(app: &App, buf: &mut Buffer, hits: &mut Registry, area: Rect) {
    let inner = w::card(
        buf,
        hits,
        area,
        Card {
            title: "Sessions",
            glyph: "◷",
            glyph_color: theme::SERIES[2],
            meta: Some(format!("{} in window", app.summary.by_session.len())),
            action: None,
        },
    );
    session_list(app, buf, hits, inner, app.scroll);
}

fn session_list(app: &App, buf: &mut Buffer, hits: &mut Registry, area: Rect, scroll: usize) {
    if area.height == 0 || area.width < 20 {
        return;
    }
    if app.summary.by_session.is_empty() {
        w::text(
            buf,
            area.x,
            area.y,
            area.width,
            "no sessions in this window",
            Style::default().fg(theme::TEXT_MUTED),
        );
        return;
    }
    let rows = area.height as usize;
    if owns_selection(app.page, NavList::Sessions) {
        app.list_rows.set(rows);
    }
    let time_w = 11u16;
    let tok_w = 9u16;
    for (i, b) in app.summary.by_session.iter().enumerate().skip(scroll).take(rows) {
        let y = area.y + (i - scroll) as u16;
        let row = Rect { x: area.x, y, width: area.width, height: 1 };
        let selected = app.page == Page::Sessions && i == app.selected;
        let hover_id = w::hover_id(&format!("session:{i}"));
        if selected || app.hover == Some(hover_id) {
            w::fill(
                buf,
                row,
                if selected { theme::SURFACE_SELECTED } else { theme::SURFACE_RAISED },
            );
        }
        let model = b.top_model().unwrap_or("—");
        // The dot becomes the selection marker on the selected row: same
        // column, same hue, unmistakably a different shape.
        let mark = if selected { theme::SELECT_MARK } else { theme::DOT };
        w::text(buf, row.x, y, 1, mark, Style::default().fg(theme::series_for(model)));
        let label_w = row.width.saturating_sub(time_w + tok_w + 4);
        // A raw session UUID identifies nothing to a reader; the project it
        // ran in and the model it used are what make a row recognizable.
        let device = match b.devices.len() {
            0 => None,
            1 => b.devices.iter().next().map(|id| app.device_name(id)),
            _ => Some("Shared"),
        };
        let label = match device {
            Some(device) => format!("[{device}]  {}  {}", b.top_project().unwrap_or("—"), model),
            None => format!("{}  {}", b.top_project().unwrap_or("—"), model),
        };
        w::text(
            buf,
            row.x + 2,
            y,
            label_w,
            &fmt::ellipsize(&label, label_w as usize),
            Style::default().fg(if selected { theme::TEXT_PRIMARY } else { theme::TEXT_SECONDARY }),
        );
        w::text_right(
            buf,
            row.right().saturating_sub(time_w + tok_w),
            y,
            tok_w,
            &fmt::tokens(b.tokens.total()),
            Style::default().fg(theme::TEXT_SECONDARY),
        );
        w::text_right(
            buf,
            row.right().saturating_sub(time_w),
            y,
            time_w,
            &fmt::relative(b.last_ts),
            Style::default().fg(theme::TEXT_MUTED),
        );
        hits.add_hoverable(row, Action::SessionRow(i), hover_id);
    }
}

// ── Devices ────────────────────────────────────────────────────────────────

fn devices(app: &App, buf: &mut Buffer, hits: &mut Registry, area: Rect) {
    let discovered =
        app.devices.iter().filter(|device| !device.is_local && device.discovered).count();
    let inner = w::card(
        buf,
        hits,
        area,
        Card {
            title: "Devices",
            glyph: "◫",
            glyph_color: theme::SERIES[1],
            meta: Some(format!(
                "{discovered} found · {} enabled · r sync · Enter connect · Del disable · u update",
                app.settings.ssh_hosts.len()
            )),
            action: app.settings.has_ssh_hosts().then_some(Action::SyncDevices),
        },
    );
    let rows = inner.height as usize;
    if owns_selection(app.page, NavList::Devices) {
        app.list_rows.set(rows);
    }
    for index in (app.scroll..app.device_row_count()).take(rows) {
        let y = inner.y + (index - app.scroll) as u16;
        let row = Rect { x: inner.x, y, width: inner.width, height: 1 };
        let selected = index == app.selected;
        let record = app.devices.get(index);
        let id = record.map_or(crate::agg::SHARED_DEVICE_ID, |device| device.id.as_str());
        let hover_key = record.and_then(|device| device.host.as_deref()).unwrap_or(id);
        let hover_id = w::hover_id(&format!("device:{hover_key}"));
        if selected || app.hover == Some(hover_id) {
            w::fill(
                buf,
                row,
                if selected { theme::SURFACE_SELECTED } else { theme::SURFACE_RAISED },
            );
        }
        let bucket = app.device_summary.by_device.iter().find(|bucket| bucket.label == id);
        let name = record.map_or("Shared", |device| device.name.as_str());
        let state = if id == crate::agg::SHARED_DEVICE_ID {
            "copied history".to_string()
        } else if record.is_some_and(|device| device.is_local) {
            "local".to_string()
        } else if record.is_some_and(|device| !device.discovered) {
            "missing config".to_string()
        } else if record.is_some_and(|device| !device.enabled) {
            "available".to_string()
        } else if record.is_some_and(|device| !device.available) {
            "not synced".to_string()
        } else {
            record.map_or_else(
                || "imported".to_string(),
                |device| {
                    let version = device.exporter_version.as_deref().unwrap_or("?");
                    format!("v{version} · {}", fmt::relative(device.generated_at))
                },
            )
        };
        let mark = if selected { theme::SELECT_MARK } else { theme::DOT };
        w::text(buf, row.x, y, 1, mark, Style::default().fg(theme::series_for(id)));
        let value = bucket.map_or_else(
            || "—".to_string(),
            |bucket| {
                format!(
                    "{}  {}",
                    fmt::tokens(bucket.tokens.total()),
                    fmt::money_partial(bucket.priced.cost, bucket.priced.coverage())
                )
            },
        );
        let value_w = (value.chars().count() as u16).min(row.width / 3);
        let state_w = 18u16.min(row.width / 4);
        let label_w = row.width.saturating_sub(value_w + state_w + 4);
        w::text(
            buf,
            row.x + 2,
            y,
            label_w,
            &fmt::ellipsize(name, label_w as usize),
            Style::default().fg(if selected { theme::TEXT_PRIMARY } else { theme::TEXT_SECONDARY }),
        );
        w::text_right(
            buf,
            row.right().saturating_sub(value_w + state_w + 1),
            y,
            value_w,
            &value,
            Style::default().fg(theme::TEXT_SECONDARY),
        );
        w::text_right(
            buf,
            row.right().saturating_sub(state_w),
            y,
            state_w,
            &state,
            Style::default().fg(theme::TEXT_MUTED),
        );
        hits.add_hoverable(row, Action::Row(index), hover_id);
    }
}

// ── Settings ───────────────────────────────────────────────────────────────

fn settings(app: &App, buf: &mut Buffer, hits: &mut Registry, area: Rect) {
    let inner = w::card(
        buf,
        hits,
        area,
        Card {
            title: "Settings",
            glyph: "⚙",
            glyph_color: theme::SERIES[0],
            meta: Some(format!("device {}", app.settings.device.name)),
            action: None,
        },
    );
    let rows = [(
        "Aggregate devices",
        if app.settings.aggregate_devices { "On" } else { "Off" },
        "Include remote usage in the default totals",
    )];
    app.list_rows.set(rows.len().min(inner.height as usize));
    for (index, (label, value, help)) in rows.iter().enumerate().take(inner.height as usize) {
        let y = inner.y + index as u16;
        let row = Rect { x: inner.x, y, width: inner.width, height: 1 };
        let selected = index == app.selected;
        let hover_id = w::hover_id(&format!("setting:{index}"));
        if selected || app.hover == Some(hover_id) {
            w::fill(
                buf,
                row,
                if selected { theme::SURFACE_SELECTED } else { theme::SURFACE_RAISED },
            );
        }
        let actual = (*value).to_string();
        let value_w = 8u16.min(row.width);
        let label_w = 22u16.min(row.width.saturating_sub(value_w + 2));
        let mark = if selected { theme::SELECT_MARK } else { " " };
        w::text(buf, row.x, y, 1, mark, Style::default().fg(theme::SERIES[0]));
        w::text(
            buf,
            row.x + 2,
            y,
            label_w,
            label,
            Style::default()
                .fg(if selected { theme::TEXT_PRIMARY } else { theme::TEXT_SECONDARY })
                .add_modifier(if selected { Modifier::BOLD } else { Modifier::empty() }),
        );
        let help_x = row.x + label_w + 2;
        let help_w = row.width.saturating_sub(label_w + value_w + 3);
        w::text(buf, help_x, y, help_w, help, Style::default().fg(theme::TEXT_MUTED));
        w::text_right(
            buf,
            row.right().saturating_sub(value_w),
            y,
            value_w,
            &actual,
            Style::default().fg(if actual == "On" { theme::SERIES[2] } else { theme::TEXT_MUTED }),
        );
        hits.add_hoverable(row, Action::Setting(index), hover_id);
    }

    if inner.height > rows.len() as u16 + 1 {
        let y = inner.y + rows.len() as u16 + 1;
        let remotes = format!(
            "{} enabled SSH hosts · {} project aliases · config {}",
            app.settings.ssh_hosts.len(),
            app.settings.project_aliases.len(),
            app.settings_path
        );
        w::text(
            buf,
            inner.x,
            y,
            inner.width,
            &fmt::ellipsize(&remotes, inner.width as usize),
            Style::default().fg(theme::TEXT_MUTED),
        );
    }
}

// ── Session replay ─────────────────────────────────────────────────────────

fn replay(app: &App, buf: &mut Buffer, hits: &mut Registry, area: Rect) {
    let detail_h = if area.height >= 20 { 6 } else { 0 };
    let [meta, timeline, events, detail] = Layout::vertical([
        Constraint::Length(4),
        Constraint::Length(5),
        Constraint::Min(5),
        Constraint::Length(detail_h),
    ])
    .areas(area);

    replay_meta(app, buf, hits, meta);
    if app.replay.loading {
        let frame = crate::tui::anim::SPINNER[app
            .pulse
            .frame(crate::tui::anim::SPINNER.len(), std::time::Duration::from_millis(800))];
        w::callout(
            buf,
            hits,
            events,
            frame,
            theme::TEXT_MUTED,
            "reading this session's active transcripts…",
            Some(Action::BackToSessions),
        );
        return;
    }
    if let Some(error) = app.replay.error.as_ref() {
        w::callout(
            buf,
            hits,
            events,
            theme::ICON_WARNING,
            theme::CRITICAL,
            &format!("replay unavailable: {error}"),
            Some(Action::BackToSessions),
        );
        return;
    }
    let Some(replay) = app.replay.data.as_ref() else { return };
    replay_timeline(app, replay.events.as_slice(), buf, hits, timeline);
    replay_events(app, replay.events.as_slice(), replay.truncated, buf, hits, events);
    if detail_h > 0 {
        replay_detail(app, replay.events.as_slice(), buf, hits, detail);
    }
}

fn replay_meta(app: &App, buf: &mut Buffer, hits: &mut Registry, area: Rect) {
    let request = app.replay.request.as_ref();
    let inner = w::card(
        buf,
        hits,
        area,
        Card {
            title: "Session Replay",
            glyph: "◀",
            glyph_color: theme::SERIES[0],
            meta: request.map(|request| format!("{} · {}", request.source.label(), request.model)),
            action: Some(Action::BackToSessions),
        },
    );
    let Some(request) = request else { return };
    if inner.height > 0 {
        let project = format!("project  {}", request.project);
        w::text(
            buf,
            inner.x,
            inner.y,
            inner.width,
            &fmt::ellipsize(&project, inner.width as usize),
            Style::default().fg(theme::TEXT_SECONDARY),
        );
    }
    if inner.height > 1 {
        let time = app
            .replay
            .data
            .as_ref()
            .map(|replay| replay_wall_range(replay.first_ts_ms, replay.last_ts_ms))
            .filter(|time| !time.is_empty());
        let session = match time {
            Some(time) => format!("session  {} · {time}", request.session),
            None => format!("session  {}", request.session),
        };
        w::text(
            buf,
            inner.x,
            inner.y + 1,
            inner.width,
            &fmt::ellipsize(&session, inner.width as usize),
            Style::default().fg(theme::TEXT_MUTED),
        );
    }
}

fn replay_timeline(
    app: &App,
    events: &[ReplayEvent],
    buf: &mut Buffer,
    hits: &mut Registry,
    area: Rect,
) {
    let duration = events.last().map_or(0, |event| event.offset_ms);
    let inner = w::card(
        buf,
        hits,
        area,
        Card {
            title: "Timeline",
            glyph: "▷",
            glyph_color: theme::SERIES[2],
            meta: Some(format!("{} events · {}", events.len(), replay_time(duration))),
            action: None,
        },
    );
    if events.is_empty() || inner.height == 0 || inner.width == 0 {
        w::text(
            buf,
            inner.x,
            inner.y,
            inner.width,
            "no replayable messages or tool calls",
            Style::default().fg(theme::TEXT_MUTED),
        );
        return;
    }

    let line_y = inner.y;
    for x in inner.x..inner.right() {
        w::text(buf, x, line_y, 1, "─", Style::default().fg(theme::RULE));
    }
    let track = inner.width.saturating_sub(1).max(1);
    let visible_track =
        (f64::from(track) * app.grow.value()).round().clamp(0.0, f64::from(track)) as u16;
    let mut start = 0;
    for column in 0..=visible_track {
        // 同一终端列里的事件只画一个代表点。通过有序时间戳二分列边界，
        // 单帧成本只随屏幕宽度增长，不再随最多 20,000 条事件增长。
        let end = events.partition_point(|event| {
            replay_timeline_column(event.offset_ms, duration, visible_track) <= column
        });
        if start == end {
            continue;
        }

        // 选中事件必须保持可见；否则用该列最新的事件，点击后落到这段轨迹的末端。
        let index =
            if (start..end).contains(&app.selected) { app.selected } else { end.saturating_sub(1) };
        let event = &events[index];
        let selected = index == app.selected;
        let mark = if selected { "◆" } else { "│" };
        let color = if selected { theme::TEXT_PRIMARY } else { replay_color(event.kind) };
        let x = inner.x + column;
        w::text(buf, x, line_y, 1, mark, Style::default().fg(color));
        hits.add(Rect { x, y: line_y, width: 1, height: 1 }, Action::ReplaySeek(index));
        start = end;
    }

    if inner.height < 2 {
        return;
    }
    let y = inner.bottom() - 1;
    let play = if app.replay.playing { " ❚❚ " } else { " ▶ " };
    let play_w = play.chars().count() as u16;
    w::text(
        buf,
        inner.x,
        y,
        play_w,
        play,
        Style::default().fg(theme::TEXT_PRIMARY).bg(theme::SURFACE_ACTIVE),
    );
    hits.add(Rect { x: inner.x, y, width: play_w, height: 1 }, Action::ReplayToggle);
    let mut x = inner.x + play_w + 1;
    for speed in [1u8, 2, 4] {
        let label = format!(" {speed}x ");
        let width = label.chars().count() as u16;
        let active = app.replay.speed == speed;
        w::text(
            buf,
            x,
            y,
            width,
            &label,
            if active {
                Style::default().fg(theme::TEXT_PRIMARY).bg(theme::SURFACE_ACTIVE)
            } else {
                Style::default().fg(theme::TEXT_MUTED)
            },
        );
        hits.add(Rect { x, y, width, height: 1 }, Action::ReplaySpeed(speed));
        x += width;
    }
    let status = format!(
        "Event {}/{} · {}",
        app.selected.saturating_add(1).min(events.len()),
        events.len(),
        replay_time(events.get(app.selected).map_or(0, |event| event.offset_ms))
    );
    w::text_right(
        buf,
        x,
        y,
        inner.right().saturating_sub(x),
        &status,
        Style::default().fg(theme::TEXT_MUTED),
    );
}

/// 把事件映射到已经展开的时间线列；使用整数运算避免长 session 的浮点边界漂移。
fn replay_timeline_column(offset_ms: u64, duration_ms: u64, track: u16) -> u16 {
    if duration_ms == 0 || track == 0 {
        return 0;
    }
    let duration = u128::from(duration_ms);
    let column = (u128::from(offset_ms) * u128::from(track) + duration / 2) / duration;
    column.min(u128::from(track)) as u16
}

fn replay_events(
    app: &App,
    events: &[ReplayEvent],
    truncated: bool,
    buf: &mut Buffer,
    hits: &mut Registry,
    area: Rect,
) {
    let meta =
        if truncated { format!("{} · truncated", events.len()) } else { events.len().to_string() };
    let inner = w::card(
        buf,
        hits,
        area,
        Card {
            title: "Event Trace",
            glyph: "⑂",
            glyph_color: theme::SERIES[3],
            meta: Some(meta),
            action: None,
        },
    );
    let rows = inner.height as usize;
    app.list_rows.set(rows);
    for (index, event) in events.iter().enumerate().skip(app.scroll).take(rows) {
        let y = inner.y + (index - app.scroll) as u16;
        let row = Rect { x: inner.x, y, width: inner.width, height: 1 };
        let selected = index == app.selected;
        let hover = app.hover == Some(w::hover_id(&format!("replay:{index}")));
        if selected || hover {
            w::fill(
                buf,
                row,
                if selected { theme::SURFACE_SELECTED } else { theme::SURFACE_RAISED },
            );
        }
        let color = replay_color(event.kind);
        let mark = if selected { theme::SELECT_MARK } else { replay_glyph(event.kind) };
        w::text(buf, row.x, y, 1, mark, Style::default().fg(color));
        let time_w = 10u16.min(row.width);
        w::text(
            buf,
            row.x + 2,
            y,
            time_w.saturating_sub(2),
            &replay_time(event.offset_ms),
            Style::default().fg(theme::TEXT_MUTED),
        );
        let title_x = row.x + time_w;
        let title_w = 22u16.min(row.right().saturating_sub(title_x));
        w::text(
            buf,
            title_x,
            y,
            title_w,
            &fmt::ellipsize(&event.title, title_w as usize),
            Style::default()
                .fg(if selected { theme::TEXT_PRIMARY } else { color })
                .add_modifier(if selected { Modifier::BOLD } else { Modifier::empty() }),
        );
        let detail_x = title_x + title_w + 1;
        let detail_w = row.right().saturating_sub(detail_x);
        let detail_style = if index > app.selected {
            Style::default().fg(theme::TEXT_MUTED)
        } else {
            Style::default().fg(theme::TEXT_SECONDARY)
        };
        w::text(
            buf,
            detail_x,
            y,
            detail_w,
            &fmt::ellipsize(&event.detail, detail_w as usize),
            detail_style,
        );
        hits.add_hoverable(row, Action::ReplaySeek(index), w::hover_id(&format!("replay:{index}")));
    }
}

fn replay_detail(
    app: &App,
    events: &[ReplayEvent],
    buf: &mut Buffer,
    hits: &mut Registry,
    area: Rect,
) {
    let Some(event) = events.get(app.selected) else { return };
    let inner = w::card(
        buf,
        hits,
        area,
        Card {
            title: &event.title,
            glyph: replay_glyph(event.kind),
            glyph_color: replay_color(event.kind),
            meta: Some(replay_time(event.offset_ms)),
            action: None,
        },
    );
    draw_wrapped(buf, inner, &event.detail, Style::default().fg(theme::TEXT_SECONDARY));
}

fn replay_time(ms: u64) -> String {
    let seconds = ms / 1_000;
    let minutes = seconds / 60;
    if minutes >= 60 {
        format!("+{}:{:02}:{:02}", minutes / 60, minutes % 60, seconds % 60)
    } else {
        format!("+{}:{:02}", minutes, seconds % 60)
    }
}

fn replay_wall_range(first_ms: i64, last_ms: i64) -> String {
    if first_ms <= 0 || last_ms <= 0 {
        return String::new();
    }
    let Some(first) = chrono::DateTime::from_timestamp_millis(first_ms) else {
        return String::new();
    };
    let Some(last) = chrono::DateTime::from_timestamp_millis(last_ms) else {
        return String::new();
    };
    let first = first.with_timezone(&chrono::Local);
    let last = last.with_timezone(&chrono::Local);
    if first.date_naive() == last.date_naive() {
        format!("{}–{}", first.format("%b %-d %H:%M"), last.format("%H:%M"))
    } else {
        format!("{}–{}", first.format("%b %-d %H:%M"), last.format("%b %-d %H:%M"))
    }
}

fn replay_glyph(kind: ReplayKind) -> &'static str {
    match kind {
        ReplayKind::User => "●",
        ReplayKind::Assistant => "◆",
        ReplayKind::ToolCall => "⚒",
        ReplayKind::ToolResult => "↳",
        ReplayKind::ToolError => "×",
        ReplayKind::System => "ⓘ",
    }
}

fn replay_color(kind: ReplayKind) -> ratatui::style::Color {
    match kind {
        ReplayKind::User => theme::SERIES[2],
        ReplayKind::Assistant => theme::SERIES[0],
        ReplayKind::ToolCall => theme::WARNING,
        ReplayKind::ToolResult => theme::GOOD,
        ReplayKind::ToolError => theme::CRITICAL,
        ReplayKind::System => theme::SERIES[6],
    }
}

fn draw_wrapped(buf: &mut Buffer, area: Rect, raw: &str, style: Style) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let safe = fmt::terminal_text(raw);
    let chars: Vec<char> = safe.chars().collect();
    let width = area.width as usize;
    for (line, chunk) in chars.chunks(width).take(area.height as usize).enumerate() {
        let text: String = chunk.iter().collect();
        w::text(buf, area.x, area.y + line as u16, area.width, &text, style);
    }
}

// ── Pricing ─────────────────────────────────────────────────────────────────

fn pricing(app: &App, buf: &mut Buffer, hits: &mut Registry, area: Rect) {
    let has_unpriced = !app.summary.unpriced_models.is_empty();
    let [table, unpriced_area, spend] = Layout::vertical([
        Constraint::Min(6),
        Constraint::Length(if has_unpriced { 8 } else { 0 }),
        Constraint::Length(4),
    ])
    .areas(area);

    let inner = w::card(
        buf,
        hits,
        table,
        Card {
            title: "Rates",
            glyph: "$",
            glyph_color: theme::SERIES[3],
            meta: Some("USD per million tokens".into()),
            action: None,
        },
    );
    if inner.height > 1 {
        // Columns are dropped from the right rather than clipped: the cache
        // rates are derived from input, so they are the ones a narrow terminal
        // can lose, and a rate cut mid-number ("10.00    5") is not a number.
        // Two columns reserved on the left for the selection marker, as in
        // every other list, so the table does not shift when it moves.
        let tx = inner.x + 2;
        let tw = inner.width.saturating_sub(2);
        let iw = tw as usize;
        let (name_w, show_output, show_cache) = if iw >= 67 {
            (26, true, true)
        } else if iw >= 44 {
            (26, true, false)
        } else {
            (iw.saturating_sub(9).clamp(8, 26), false, false)
        };
        let mut head = format!("{:<name_w$}{:>9}", "model", "input");
        if show_output {
            head.push_str(&format!("{:>9}", "output"));
        }
        if show_cache {
            head.push_str(&format!("{:>11}{:>12}", "cache read", "cache write"));
        }
        w::text(buf, tx, inner.y, tw, &head, Style::default().fg(theme::TEXT_MUTED));
        let known = app.pricing.known_models();
        // The rate table is the only list on this page, so it is what ↑↓ and
        // the wheel move. Before this it ignored `scroll` while `row_count`
        // still counted its rows, so both silently did nothing here.
        let body = (inner.height - 1) as usize;
        let overflow = known.len() > body;
        let rows = body.saturating_sub(overflow as usize).max(1);
        if owns_selection(app.page, NavList::Rates) {
            app.list_rows.set(rows);
        }
        for (i, (model, rate)) in known.iter().enumerate().skip(app.scroll).take(rows) {
            let y = inner.y + 1 + (i - app.scroll) as u16;
            let row = Rect { x: inner.x, y, width: inner.width, height: 1 };
            let selected = i == app.selected;
            if selected || app.hover == Some(i as u64) {
                w::fill(
                    buf,
                    row,
                    if selected { theme::SURFACE_SELECTED } else { theme::SURFACE_RAISED },
                );
            }
            if selected {
                w::text(
                    buf,
                    row.x,
                    y,
                    1,
                    theme::SELECT_MARK,
                    Style::default().fg(theme::series_for(model)),
                );
            }
            let mut line =
                format!("{:<name_w$}{:>9.2}", fmt::ellipsize(model, name_w - 1), rate.input);
            if show_output {
                line.push_str(&format!("{:>9.2}", rate.output));
            }
            if show_cache {
                line.push_str(&format!(
                    "{:>11.2}{:>12.2}",
                    rate.cache_read_rate(),
                    rate.cache_write_5m_rate(),
                ));
            }
            let fg = if selected { theme::TEXT_PRIMARY } else { theme::TEXT_SECONDARY };
            w::text(buf, tx, y, tw, &line, Style::default().fg(fg));
            hits.add_hoverable(row, Action::Row(i), i as u64);
        }
        if overflow {
            // Where in the table this window sits. A list that scrolls with no
            // sense of position is worse than one that admits it was cut.
            let last = (app.scroll + rows).min(known.len());
            w::text(
                buf,
                inner.x,
                inner.bottom().saturating_sub(1),
                inner.width,
                &format!("{}–{} of {} · ↑↓ scrolls", app.scroll + 1, last, known.len()),
                Style::default().fg(theme::TEXT_MUTED),
            );
        }
    }

    if has_unpriced {
        let inner = w::card(
            buf,
            hits,
            unpriced_area,
            Card {
                title: "No rate on file",
                glyph: theme::ICON_WARNING,
                glyph_color: theme::WARNING,
                meta: Some("tokens counted, cost shown as —".into()),
                action: None,
            },
        );
        for (i, m) in app.summary.unpriced_models.iter().enumerate() {
            let y = inner.y + i as u16;
            if y >= inner.bottom().saturating_sub(1) {
                w::text(
                    buf,
                    inner.x,
                    y,
                    inner.width,
                    &format!("… and {} more", app.summary.unpriced_models.len() - i),
                    Style::default().fg(theme::TEXT_MUTED),
                );
                break;
            }
            w::text(buf, inner.x, y, inner.width, m, Style::default().fg(theme::TEXT_SECONDARY));
        }
        if let Ok(p) = crate::paths::pricing_override_file() {
            w::text(
                buf,
                inner.x,
                inner.bottom().saturating_sub(1),
                inner.width,
                &format!("Add rates in {}  (`readout pricing --init`)", p.display()),
                Style::default().fg(theme::TEXT_MUTED),
            );
        }
    }

    let mtd = month_to_date(&app.summary.daily);
    w::callout(
        buf,
        hits,
        spend,
        if mtd.is_complete() { theme::ICON_GOOD } else { theme::ICON_INFO },
        if mtd.is_complete() { theme::GOOD } else { theme::SERIES[0] },
        &format!(
            "Month to date: {} across {} tokens{}",
            fmt::money_partial(mtd.cost, mtd.coverage()),
            fmt::tokens(mtd.total_tokens()),
            if mtd.is_complete() { "" } else { " (priced models only)" },
        ),
        None,
    );
}

/// Summary text used by the empty state.
fn empty_message(app: &App, summary: &Summary) -> String {
    if summary.total.events > 0 {
        return String::new();
    }
    let missing = crate::report::missing_sources(&app.active_sources());
    if !missing.is_empty() {
        return format!("No transcripts found for {}.", missing.join(" and "));
    }
    match app.drill {
        Drill::None => "No usage in this window. Try a wider range.".into(),
        _ => "Nothing matches this filter. Press Esc to clear it.".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_day_is_never_cropped_at_any_width() {
        let hourly: Vec<u64> = (0..24).map(|h| h as u64 + 1).collect();
        let expected: u64 = hourly.iter().sum();
        for width in 1..=80u16 {
            let (values, labels, hour_of) = fit_day(&hourly, width);
            // Count each bucket once: a wide plot draws an hour across two
            // columns, which must not be read as twice the tokens.
            let mut per_bucket = std::collections::HashMap::new();
            for (c, v) in values.iter().enumerate() {
                per_bucket.entry(hour_of(c)).or_insert(*v);
            }
            assert_eq!(
                per_bucket.values().sum::<u64>(),
                expected,
                "width {width} dropped part of the day"
            );
            assert!(values.len() <= width.max(1) as usize, "width {width} overflowed its plot");
            assert_eq!(labels.len(), values.len());
        }
    }

    #[test]
    fn merged_hours_saturate_instead_of_wrapping() {
        let (values, _, _) = fit_day(&[u64::MAX; 24], 1);
        assert_eq!(values, vec![u64::MAX]);
    }

    #[test]
    fn the_headline_leads_with_today_at_every_width() {
        use crate::agg::Filter;
        use crate::model::{Source, Tokens, UsageEvent};
        use crate::pricing::Pricing;

        let mut a = App::new(Source::ALL.to_vec(), Filter::default(), Pricing::builtin());
        a.events = vec![UsageEvent {
            source: Source::Claude,
            ts: chrono::Local::now().timestamp(),
            model: "claude-opus-5".into(),
            session: "s1".into(),
            project: "alpha".into(),
            tokens: Tokens { input: 1_000, output: 1_000, ..Default::default() },
            observed_on: Vec::new(),
            dedup_key: None,
            dedup_rank: 0,
        }];
        a.recompute(false);

        for width in 20..=200u16 {
            let line = headline(&a, width);
            assert!(line.starts_with("Today "), "width {width} dropped today: {line}");
            assert!(
                line.chars().count() <= width as usize,
                "width {width} would be clipped: {line}"
            );
        }

        // Under a one-day window the tiles above already are today, so saying
        // it again in the sub-line is just the same number twice.
        a.set_range(Range::Today);
        assert!(!headline(&a, 200).starts_with("Today "));
    }

    #[test]
    fn midnight_is_always_labelled() {
        let hourly = vec![1u64; 24];
        for width in 6..=80u16 {
            let (_, labels, _) = fit_day(&hourly, width);
            assert_eq!(labels[0], "12a", "width {width} lost the start of the day");
        }
    }

    #[test]
    fn columns_map_back_to_the_right_time_of_day_band() {
        let hourly = vec![1u64; 24];
        for width in [8u16, 12, 24, 48, 60] {
            let (values, _, hour_of) = fit_day(&hourly, width);
            assert_eq!(hour_of(0), 0);
            let last = hour_of(values.len() - 1);
            assert!(last < 24, "width {width} mapped a column past the end of the day");
            // Hours must advance monotonically across the plot.
            let mut prev = 0;
            for c in 0..values.len() {
                assert!(hour_of(c) >= prev);
                prev = hour_of(c);
            }
        }
    }

    #[test]
    fn a_wide_plot_gives_each_hour_two_columns_without_doubling_its_height() {
        let mut hourly = vec![0u64; 24];
        hourly[9] = 100;
        let (values, _, _) = fit_day(&hourly, 60);
        assert_eq!(values.len(), 48);
        assert_eq!(values.iter().filter(|v| **v > 0).count(), 2);
        assert!(values.iter().all(|v| *v == 0 || *v == 100), "a split hour must keep its height");
    }
}
