//! Dashboard state.
//!
//! The app owns the raw event stream once, and every view is a rollup of it
//! under the current filter. Changing a filter recomputes the summary — which
//! is cheap next to the scan — rather than mutating per-view state, so the
//! pages can never disagree with each other.

use crate::agg::{Filter, Summary, summarize};
use crate::model::{Source, UsageEvent};
use crate::pricing::Pricing;
use crate::scan::{Progress, ScanStats};
use crate::tui::anim::{Eased, Pulse};
use crate::tui::hit::Registry;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Page {
    Overview,
    Daily,
    Models,
    Projects,
    Sessions,
    Pricing,
}

impl Page {
    /// Sidebar order, grouped under headings.
    pub const GROUPS: [(&'static str, &'static [Page]); 3] = [
        ("Overview", &[Page::Overview, Page::Daily]),
        ("Breakdown", &[Page::Models, Page::Projects, Page::Sessions]),
        ("Config", &[Page::Pricing]),
    ];

    pub const ORDER: [Page; 6] =
        [Page::Overview, Page::Daily, Page::Models, Page::Projects, Page::Sessions, Page::Pricing];

    pub fn title(self) -> &'static str {
        match self {
            Page::Overview => "Readout",
            Page::Daily => "Daily",
            Page::Models => "Models",
            Page::Projects => "Projects",
            Page::Sessions => "Sessions",
            Page::Pricing => "Pricing",
        }
    }

    pub fn icon(self) -> &'static str {
        match self {
            Page::Overview => "◈",
            Page::Daily => "▤",
            Page::Models => "◱",
            Page::Projects => "▣",
            Page::Sessions => "◷",
            Page::Pricing => "$",
        }
    }

    pub fn index(self) -> usize {
        Page::ORDER.iter().position(|p| *p == self).unwrap_or(0)
    }
}

/// Time window chips in the header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Range {
    Today,
    D7,
    D30,
    D90,
    All,
}

impl Range {
    pub const ORDER: [Range; 5] = [Range::Today, Range::D7, Range::D30, Range::D90, Range::All];

    pub fn label(self) -> &'static str {
        match self {
            Range::Today => "Today",
            Range::D7 => "7d",
            Range::D30 => "30d",
            Range::D90 => "90d",
            Range::All => "All",
        }
    }

    /// The label for a header with no room for the long one. Only `Today`
    /// differs; the rest are already as short as they get.
    pub fn label_short(self) -> &'static str {
        match self {
            Range::Today => "1d",
            other => other.label(),
        }
    }

    pub fn days(self) -> Option<i64> {
        match self {
            Range::Today => Some(1),
            Range::D7 => Some(7),
            Range::D30 => Some(30),
            Range::D90 => Some(90),
            Range::All => None,
        }
    }

    /// The chip a `--days N` on the command line lands on.
    ///
    /// Every window has to be one of the five, because the chips are the only
    /// thing on screen saying which one is active. Rounding up to the nearest
    /// chip that still contains the requested window is the honest direction:
    /// it shows the user at least what they asked for, never less.
    pub fn for_days(days: Option<i64>) -> Range {
        match days {
            None => Range::All,
            Some(d) if d <= 1 => Range::Today,
            Some(d) if d <= 7 => Range::D7,
            Some(d) if d <= 30 => Range::D30,
            Some(d) if d <= 90 => Range::D90,
            _ => Range::All,
        }
    }

    /// How many days a trend chart should span for this window. "All" is
    /// capped so a two-year history does not compress into invisible slivers.
    pub fn chart_days(self, first_ts: i64) -> usize {
        match self.days() {
            Some(d) => d as usize,
            None => {
                if first_ts == 0 {
                    30
                } else {
                    let days = (chrono::Local::now().timestamp() - first_ts) / 86_400 + 1;
                    days.clamp(30, 365) as usize
                }
            }
        }
    }
}

/// A drill-down applied on top of the range.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Drill {
    #[default]
    None,
    Model(String),
    Project(String),
    Session(String),
}

impl Drill {
    pub fn label(&self) -> Option<String> {
        match self {
            Drill::None => None,
            Drill::Model(m) => Some(format!("model: {m}")),
            Drill::Project(p) => Some(format!("project: {p}")),
            Drill::Session(s) => Some(format!("session: {s}")),
        }
    }
}

/// Progress of the background scan.
#[derive(Debug, Clone)]
pub enum Loading {
    Scanning(Option<Progress>),
    Done,
    Failed(String),
}

/// Why a scan is about to run.
///
/// A manual rescan says so on screen, because the user asked and deserves an
/// acknowledgement. A watch rescan must not: flashing a loading panel over
/// the dashboard every few seconds would make a tool for reading numbers
/// impossible to read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rescan {
    Manual,
    Watch,
}

/// How often watch mode looks for new transcripts.
///
/// A warm scan is a few hundred milliseconds at worst and reads only what was
/// appended, so this could be shorter. It is not, because the number being
/// watched moves on the timescale of a model finishing a response, and a
/// dashboard that redraws faster than the thing it measures is just motion.
pub const WATCH_INTERVAL: std::time::Duration = std::time::Duration::from_secs(5);

pub struct App {
    pub page: Page,
    pub range: Range,
    pub drill: Drill,
    pub sources: Vec<Source>,
    /// Sources the user has switched off in the UI. Kept separate from
    /// `sources` so a CLI `--source` restriction is never widened by a click.
    pub disabled: Vec<Source>,

    pub events: Vec<UsageEvent>,
    pub pricing: Pricing,
    pub summary: Summary,
    pub stats: ScanStats,
    pub loading: Loading,

    /// Selected row on the focused list, per page.
    pub selected: usize,
    pub scroll: usize,
    /// Rows the last frame had space for, written during render so keyboard
    /// navigation can scroll the window without knowing the layout.
    pub list_rows: std::cell::Cell<usize>,
    pub hover: Option<u64>,

    pub hits: Registry,
    pub pulse: Pulse,
    /// KPI counters, eased so they count up on load and on filter changes.
    pub kpi: [Eased; 4],
    /// Bar growth factor, 0..1, restarted whenever the data behind a chart
    /// changes so bars grow rather than snap.
    pub grow: Eased,
    pub should_quit: bool,
    pub needs_redraw: bool,
    /// Set when a rescan has been asked for but not yet started.
    pub rescan_requested: Option<Rescan>,
    /// A scan thread is running. Watch scans deliberately leave `loading` on
    /// `Done`, so this is the only thing that knows one is in flight — and
    /// without it the loop would spawn a fresh thread every tick.
    pub scan_pending: bool,
    /// When the last scan finished, successfully or not. Watch mode counts
    /// from here, so a failing scan retries on the next interval rather than
    /// wedging the dashboard on a stale number.
    pub last_scan: Option<std::time::Instant>,
    /// Re-scan on a timer, so the numbers keep up with the work.
    pub watch: bool,
    /// The local date the current summary was built on. Every window is
    /// anchored to "now", so a dashboard left watching past midnight is
    /// showing yesterday's answer until it repaints.
    pub summary_date: chrono::NaiveDate,
    pub status: Option<String>,
}

impl App {
    pub fn new(sources: Vec<Source>, base: Filter, pricing: Pricing) -> Self {
        // `--days 7` used to open on the 30-day chip: the window was right but
        // the header said otherwise. Land on the chip that matches.
        let today = chrono::Local::now().date_naive();
        let range = Range::for_days(base.since.map(|s| (today - s).num_days() + 1));
        App {
            page: Page::Overview,
            range,
            drill: match (&base.model, &base.project) {
                (Some(m), _) => Drill::Model(m.clone()),
                (_, Some(p)) => Drill::Project(p.clone()),
                _ => Drill::None,
            },
            sources,
            disabled: Vec::new(),
            events: Vec::new(),
            pricing,
            summary: Summary::default(),
            stats: ScanStats::default(),
            loading: Loading::Scanning(None),
            selected: 0,
            scroll: 0,
            list_rows: std::cell::Cell::new(0),
            hover: None,
            hits: Registry::default(),
            pulse: Pulse::default(),
            kpi: [Eased::from_zero(0.0); 4],
            grow: Eased::from_zero(1.0),
            should_quit: false,
            needs_redraw: true,
            rescan_requested: None,
            scan_pending: false,
            last_scan: None,
            watch: false,
            summary_date: today,
            status: None,
        }
    }

    /// Sources currently contributing, honouring both the CLI restriction and
    /// the in-UI toggles.
    pub fn active_sources(&self) -> Vec<Source> {
        let active: Vec<Source> =
            self.sources.iter().copied().filter(|s| !self.disabled.contains(s)).collect();
        // Turning off the last tool would blank the dashboard with no way to
        // tell an empty filter from empty data; keep at least one on.
        if active.is_empty() { self.sources.clone() } else { active }
    }

    pub fn filter(&self) -> Filter {
        let today = chrono::Local::now().date_naive();
        let mut f = Filter { sources: self.active_sources(), ..Default::default() };
        f.since = self.range.days().map(|d| today - chrono::Duration::days(d - 1));
        f.until = Some(today);
        match &self.drill {
            Drill::None => {}
            Drill::Model(m) => f.model = Some(m.clone()),
            Drill::Project(p) => f.project = Some(p.clone()),
            Drill::Session(s) => f.session = Some(s.clone()),
        }
        f
    }

    /// Recompute every rollup and restart the entrance animations.
    pub fn recompute(&mut self, animate: bool) {
        self.summary = summarize(&self.events, &self.filter(), &self.pricing);
        self.summary_date = chrono::Local::now().date_naive();
        let targets = [
            self.summary.total.tokens.total() as f64,
            self.summary.total.priced.cost,
            self.summary.total.events as f64,
            self.summary.total.session_count() as f64,
        ];
        for (slot, target) in self.kpi.iter_mut().zip(targets) {
            if animate {
                *slot = Eased::from_zero(target);
            } else {
                slot.snap_to(target);
            }
        }
        if animate {
            self.grow = Eased::from_zero(1.0).with_rate(crate::tui::anim::RATE_FAST);
        } else {
            self.grow.snap_to(1.0);
        }
        self.clamp_selection();
        self.needs_redraw = true;
    }

    /// What the dashboard is currently showing, reduced to the things a new
    /// scan could change. Used to tell a rescan that found work from one that
    /// found nothing.
    fn signature(&self) -> (usize, u64, u64) {
        (self.stats.events, self.summary.total.tokens.total(), self.summary.total.events)
    }

    /// Fold a finished scan into the dashboard.
    ///
    /// A watch scan neither replays the entrance animation nor snaps: the
    /// KPIs keep the value they are showing and ease from there to the new
    /// one, so a number that went up looks like a number that went up. When
    /// nothing moved it issues no redraw at all, so a watched dashboard on an
    /// idle machine sends nothing to the terminal — which is what makes
    /// leaving one open over ssh reasonable.
    pub fn apply_scan(&mut self, events: Vec<UsageEvent>, stats: ScanStats, kind: Rescan) {
        let before: [f64; 4] = std::array::from_fn(|i| self.kpi[i].value());
        let was = self.signature();
        let day = self.summary_date;
        // A failure is on screen until something paints over it. Recovering
        // quietly would leave "scan failed" under a dashboard that is fine.
        let recovered = matches!(self.loading, Loading::Failed(_));

        self.events = events;
        self.stats = stats;
        self.loading = Loading::Done;
        self.last_scan = Some(std::time::Instant::now());

        match kind {
            Rescan::Manual => {
                self.status = None;
                self.recompute(true);
            }
            Rescan::Watch => {
                self.recompute(false);
                let moved = self.signature() != was;
                if moved {
                    for (slot, from) in self.kpi.iter_mut().zip(before) {
                        slot.ease_from(from);
                    }
                }
                // Midnight moves every window without moving a single token,
                // so the totals can match while the frame on screen is a day
                // out of date — "Today $189" on a day that has spent nothing.
                let rolled = self.summary_date != day;
                self.needs_redraw = moved || recovered || rolled;
            }
        }
    }

    /// Record a scan that ended without a result, so watch mode can try again.
    pub fn scan_failed(&mut self, err: String) {
        self.loading = Loading::Failed(err);
        self.last_scan = Some(std::time::Instant::now());
        self.needs_redraw = true;
    }

    /// Whether watch mode should start a scan now.
    ///
    /// Takes the clock as an argument so the schedule can be tested without
    /// waiting five seconds for it.
    pub fn watch_due(&self, now: std::time::Instant) -> bool {
        if !self.watch || self.scan_pending || self.rescan_requested.is_some() {
            return false;
        }
        match self.last_scan {
            None => false,
            Some(t) => now.duration_since(t) >= WATCH_INTERVAL,
        }
    }

    pub fn toggle_watch(&mut self) {
        self.watch = !self.watch;
        self.status = Some(if self.watch {
            "watching — rescanning every 5s".into()
        } else {
            "watch off".into()
        });
        self.needs_redraw = true;
    }

    /// Ask for a rescan, unless one is already under way.
    pub fn request_rescan(&mut self, kind: Rescan) {
        if self.scan_pending || self.rescan_requested.is_some() {
            return;
        }
        self.rescan_requested = Some(kind);
    }

    /// Rows on the currently focused list, used to bound selection.
    pub fn row_count(&self) -> usize {
        match self.page {
            Page::Overview => self.summary.by_model.len(),
            Page::Daily => self.summary.daily.len(),
            Page::Models => self.summary.by_model.len(),
            Page::Projects => self.summary.by_project.len(),
            Page::Sessions => self.summary.by_session.len(),
            Page::Pricing => self.pricing.known_models().len(),
        }
    }

    pub fn clamp_selection(&mut self) {
        let n = self.row_count();
        if n == 0 {
            self.selected = 0;
            self.scroll = 0;
        } else if self.selected >= n {
            self.selected = n - 1;
        }
    }

    pub fn set_page(&mut self, page: Page) {
        if self.page != page {
            self.page = page;
            self.selected = 0;
            self.scroll = 0;
            self.grow = Eased::from_zero(1.0).with_rate(crate::tui::anim::RATE_FAST);
            self.needs_redraw = true;
        }
    }

    pub fn next_page(&mut self, delta: isize) {
        let n = Page::ORDER.len() as isize;
        let i = (self.page.index() as isize + delta).rem_euclid(n) as usize;
        self.set_page(Page::ORDER[i]);
    }

    pub fn set_range(&mut self, range: Range) {
        if self.range != range {
            self.range = range;
            self.recompute(true);
        }
    }

    pub fn toggle_source(&mut self, s: Source) {
        if !self.sources.contains(&s) {
            self.status = Some(format!("{} was excluded on the command line", s.label()));
            return;
        }
        if let Some(i) = self.disabled.iter().position(|d| *d == s) {
            self.disabled.remove(i);
        } else if self.active_sources().len() > 1 {
            self.disabled.push(s);
        } else {
            self.status = Some("at least one tool must stay on".into());
            return;
        }
        self.recompute(true);
    }

    pub fn set_drill(&mut self, drill: Drill) {
        if self.drill != drill {
            self.drill = drill;
            self.recompute(true);
        }
    }

    pub fn move_selection(&mut self, delta: isize) {
        let n = self.row_count();
        if n == 0 {
            return;
        }
        let i = (self.selected as isize + delta).clamp(0, n as isize - 1) as usize;
        if i != self.selected {
            self.selected = i;
            self.needs_redraw = true;
        }
    }

    /// Scroll the visible window to keep the selection inside it.
    pub fn ensure_visible(&mut self, visible_rows: usize) {
        if visible_rows == 0 {
            return;
        }
        if self.selected < self.scroll {
            self.scroll = self.selected;
        } else if self.selected >= self.scroll + visible_rows {
            self.scroll = self.selected + 1 - visible_rows;
        }
    }

    pub fn scroll_by(&mut self, delta: isize) {
        let n = self.row_count();
        let max = n.saturating_sub(1);
        let s = (self.scroll as isize + delta).clamp(0, max as isize) as usize;
        if s != self.scroll {
            self.scroll = s;
            self.needs_redraw = true;
        }
    }

    /// Open whatever the selected row points at, or release it if it is
    /// already the filter — so the same gesture that applied a drill takes it
    /// back, and Esc is a shortcut rather than the only way out.
    pub fn activate_selected(&mut self) {
        let next = match self.page {
            Page::Models | Page::Overview => {
                self.summary.by_model.get(self.selected).map(|b| Drill::Model(b.label.clone()))
            }
            Page::Projects => {
                self.summary.by_project.get(self.selected).map(|b| Drill::Project(b.label.clone()))
            }
            Page::Sessions => {
                self.summary.by_session.get(self.selected).map(|b| Drill::Session(b.label.clone()))
            }
            _ => None,
        };
        if let Some(next) = next {
            self.set_drill(if self.drill == next { Drill::None } else { next });
        }
    }

    /// Advance animations; returns true if a redraw is warranted.
    pub fn tick(&mut self) -> bool {
        let mut moving = false;
        for k in self.kpi.iter_mut() {
            moving |= k.tick();
        }
        moving |= self.grow.tick();
        // A running scan animates its spinner regardless of eased values.
        if matches!(self.loading, Loading::Scanning(_)) {
            moving = true;
        }
        moving
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Tokens;

    fn app() -> App {
        let mut a = App::new(Source::ALL.to_vec(), Filter::default(), Pricing::builtin());
        a.events = vec![
            UsageEvent {
                source: Source::Claude,
                ts: chrono::Local::now().timestamp(),
                model: "claude-opus-5".into(),
                session: "s1".into(),
                project: "alpha".into(),
                tokens: Tokens { input: 100, output: 100, ..Default::default() },
                dedup_key: None,
                dedup_rank: 0,
            },
            UsageEvent {
                source: Source::Codex,
                ts: chrono::Local::now().timestamp(),
                model: "gpt-5.2".into(),
                session: "s2".into(),
                project: "beta".into(),
                tokens: Tokens { input: 50, output: 50, ..Default::default() },
                dedup_key: None,
                dedup_rank: 0,
            },
        ];
        a.recompute(false);
        a
    }

    #[test]
    fn drilling_into_a_model_narrows_every_rollup() {
        let mut a = app();
        assert_eq!(a.summary.total.events, 2);
        a.set_drill(Drill::Model("gpt-5.2".into()));
        assert_eq!(a.summary.total.events, 1);
        assert_eq!(a.summary.by_project[0].label, "beta");
        a.set_drill(Drill::None);
        assert_eq!(a.summary.total.events, 2);
    }

    #[test]
    fn turning_off_the_last_tool_is_refused() {
        let mut a = app();
        a.toggle_source(Source::Claude);
        assert_eq!(a.active_sources(), vec![Source::Codex]);
        a.toggle_source(Source::Codex);
        assert_eq!(a.active_sources(), vec![Source::Codex], "one tool must stay on");
        assert!(a.status.is_some());
    }

    #[test]
    fn a_cli_excluded_tool_cannot_be_switched_on_from_the_ui() {
        let mut a = App::new(vec![Source::Claude], Filter::default(), Pricing::builtin());
        a.toggle_source(Source::Codex);
        assert_eq!(a.active_sources(), vec![Source::Claude]);
    }

    #[test]
    fn selection_stays_inside_the_list_when_it_shrinks() {
        let mut a = app();
        a.set_page(Page::Models);
        a.move_selection(10);
        assert_eq!(a.selected, a.row_count() - 1);
        a.set_drill(Drill::Model("gpt-5.2".into()));
        assert!(a.selected < a.row_count().max(1));
    }

    #[test]
    fn scrolling_follows_the_selection() {
        let mut a = app();
        a.set_page(Page::Models);
        a.selected = 1;
        a.ensure_visible(1);
        assert_eq!(a.scroll, 1);
        a.selected = 0;
        a.ensure_visible(1);
        assert_eq!(a.scroll, 0);
    }

    #[test]
    fn kpis_animate_from_zero_on_a_filter_change() {
        let mut a = app();
        a.set_range(Range::D7);
        assert_eq!(a.kpi[0].value(), 0.0);
        for _ in 0..200 {
            a.tick();
        }
        assert_eq!(a.kpi[0].value(), a.summary.total.tokens.total() as f64);
    }

    #[test]
    fn a_days_flag_opens_on_the_chip_that_matches_it() {
        // The chips are the only thing on screen naming the active window, so
        // `--days 7` landing on the 30-day chip made the header contradict the
        // data underneath it.
        assert_eq!(Range::for_days(None), Range::All);
        assert_eq!(Range::for_days(Some(1)), Range::Today);
        assert_eq!(Range::for_days(Some(7)), Range::D7);
        assert_eq!(Range::for_days(Some(30)), Range::D30);
        assert_eq!(Range::for_days(Some(400)), Range::All);
        // Windows between chips round up to the one that still contains them.
        assert_eq!(Range::for_days(Some(3)), Range::D7);
        assert_eq!(Range::for_days(Some(45)), Range::D90);
    }

    #[test]
    fn the_today_chip_is_a_one_day_window() {
        assert_eq!(Range::Today.days(), Some(1));
        let mut a = app();
        a.set_range(Range::Today);
        assert_eq!(a.filter().since, Some(chrono::Local::now().date_naive()));
    }

    #[test]
    fn watch_waits_for_the_interval_and_for_the_scan_in_flight() {
        let mut a = app();
        let now = std::time::Instant::now();
        let ago = now - WATCH_INTERVAL - std::time::Duration::from_secs(1);

        assert!(!a.watch_due(now), "watch is off");
        a.watch = true;
        assert!(!a.watch_due(now), "nothing has scanned yet");

        a.last_scan = Some(now);
        assert!(!a.watch_due(now), "the interval has not passed");
        a.last_scan = Some(ago);
        assert!(a.watch_due(now));

        // Watch scans leave `loading` on Done, so without this guard the loop
        // would spawn a fresh thread on every 16ms tick.
        a.scan_pending = true;
        assert!(!a.watch_due(now), "a scan is already running");
        a.scan_pending = false;
        a.rescan_requested = Some(Rescan::Manual);
        assert!(!a.watch_due(now), "one is already queued");
    }

    #[test]
    fn a_failed_scan_does_not_wedge_watch_mode() {
        // A transient read error must cost one interval, not the feature.
        let mut a = app();
        a.watch = true;
        a.scan_pending = true;
        a.scan_failed("disk went away".into());
        assert!(!a.scan_pending || a.last_scan.is_some());
        a.scan_pending = false;
        a.last_scan = Some(std::time::Instant::now() - WATCH_INTERVAL);
        assert!(a.watch_due(std::time::Instant::now()), "it tries again next interval");
    }

    #[test]
    fn a_watch_scan_that_finds_nothing_new_does_not_redraw() {
        // The idle cost of a watched dashboard is the whole argument for
        // leaving one open over ssh.
        let mut a = app();
        let (events, stats) = (a.events.clone(), a.stats.clone());
        a.needs_redraw = false;
        a.apply_scan(events, stats, Rescan::Watch);
        assert!(!a.needs_redraw, "nothing moved, so nothing to repaint");
    }

    #[test]
    fn crossing_midnight_repaints_even_if_nothing_moved() {
        // Left watching overnight, every window shifts a day while the totals
        // stay put. Without this the dashboard keeps yesterday's frame, today
        // figure and all, until something else happens to repaint it.
        let mut a = app();
        let (events, stats) = (a.events.clone(), a.stats.clone());
        a.summary_date = chrono::Local::now().date_naive() - chrono::Duration::days(1);
        a.needs_redraw = false;
        a.apply_scan(events, stats, Rescan::Watch);
        assert!(a.needs_redraw, "the date under every window changed");
    }

    #[test]
    fn recovering_from_a_failure_repaints_even_if_nothing_moved() {
        // Otherwise "scan failed" sits in the footer of a dashboard that has
        // been working again for minutes.
        let mut a = app();
        let (events, stats) = (a.events.clone(), a.stats.clone());
        a.scan_failed("disk went away".into());
        a.needs_redraw = false;
        a.apply_scan(events, stats, Rescan::Watch);
        assert!(matches!(a.loading, Loading::Done));
        assert!(a.needs_redraw, "the failure on screen is now wrong");
    }

    #[test]
    fn a_watch_scan_eases_from_the_number_on_screen() {
        // Replaying the count-up from zero would announce a fresh dashboard
        // when what happened is that one figure went up.
        let mut a = app();
        for _ in 0..400 {
            a.tick();
        }
        let shown = a.kpi[0].value();
        assert!(shown > 0.0);

        let mut events = a.events.clone();
        events.push(UsageEvent {
            source: Source::Claude,
            ts: chrono::Local::now().timestamp(),
            model: "claude-opus-5".into(),
            session: "s3".into(),
            project: "alpha".into(),
            tokens: Tokens { input: 1_000, output: 1_000, ..Default::default() },
            dedup_key: None,
            dedup_rank: 0,
        });
        a.needs_redraw = false;
        a.apply_scan(events, a.stats.clone(), Rescan::Watch);

        assert!(a.needs_redraw, "the number moved, so the frame is stale");
        assert_eq!(a.kpi[0].value(), shown, "it resumes from what was on screen");
        for _ in 0..400 {
            a.tick();
        }
        assert_eq!(a.kpi[0].value(), a.summary.total.tokens.total() as f64);
    }

    #[test]
    fn page_navigation_wraps_in_both_directions() {
        let mut a = app();
        a.set_page(Page::Pricing);
        a.next_page(1);
        assert_eq!(a.page, Page::Overview);
        a.next_page(-1);
        assert_eq!(a.page, Page::Pricing);
    }

    #[test]
    fn an_all_time_chart_window_is_bounded() {
        assert_eq!(Range::D7.chart_days(0), 7);
        assert_eq!(Range::All.chart_days(0), 30, "no data means a default window");
        let five_years_ago = chrono::Local::now().timestamp() - 5 * 365 * 86_400;
        assert_eq!(Range::All.chart_days(five_years_ago), 365, "capped so bars stay readable");
    }
}
