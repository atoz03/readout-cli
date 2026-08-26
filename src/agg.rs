//! Rollups.
//!
//! Every view is derived from the same event stream, so a filter applied once
//! is reflected everywhere consistently. Dates and hours are bucketed in
//! **local time** — "when you work" is a question about your day, not UTC's.

use crate::model::{Source, Tokens, UsageEvent};
use crate::pricing::{Priced, Pricing, price};
use chrono::{DateTime, Datelike, Local, NaiveDate, TimeZone, Timelike};
use std::collections::{BTreeMap, HashMap, HashSet};

pub const SHARED_DEVICE_ID: &str = "@shared";

/// One row of any breakdown: a label plus its totals.
#[derive(Debug, Clone, Default)]
pub struct Bucket {
    pub label: String,
    pub tokens: Tokens,
    pub priced: Priced,
    pub events: u64,
    pub sessions: HashSet<(Source, String)>,
    /// 观察到该桶事件的设备；用于 session/device 状态，不参与 usage 求和。
    pub devices: HashSet<String>,
    pub sources: HashSet<Source>,
    /// Most recent activity in the bucket, unix seconds.
    pub last_ts: i64,
    /// Models that contributed, keyed to their token volume.
    pub models: HashMap<String, u64>,
    /// Projects that contributed, keyed to their token volume.
    pub projects: HashMap<String, u64>,
}

impl Bucket {
    fn new(label: impl Into<String>) -> Self {
        Bucket { label: label.into(), ..Default::default() }
    }

    fn absorb(&mut self, e: &UsageEvent, pricing: &Pricing) {
        self.tokens += &e.tokens;
        self.priced.add(&price(pricing, &e.model, &e.tokens));
        self.events = self.events.saturating_add(1);
        if !e.session.is_empty() {
            self.sessions.insert((e.source, e.session.clone()));
        }
        self.devices.extend(e.observed_on.iter().cloned());
        self.sources.insert(e.source);
        self.last_ts = self.last_ts.max(e.ts);
        let model_tokens = self.models.entry(e.model.clone()).or_default();
        *model_tokens = model_tokens.saturating_add(e.tokens.total());
        let project_tokens = self.projects.entry(e.project.clone()).or_default();
        *project_tokens = project_tokens.saturating_add(e.tokens.total());
    }

    pub fn session_count(&self) -> usize {
        self.sessions.len()
    }

    /// The model responsible for the most tokens here.
    pub fn top_model(&self) -> Option<&str> {
        top_key(&self.models)
    }

    /// The project responsible for the most tokens here.
    pub fn top_project(&self) -> Option<&str> {
        top_key(&self.projects)
    }
}

/// A calendar day's totals.
#[derive(Debug, Clone)]
pub struct DayBucket {
    pub date: NaiveDate,
    pub bucket: Bucket,
}

/// Everything the UI reads, computed once per scan + filter.
#[derive(Debug, Default)]
pub struct Summary {
    pub total: Bucket,
    pub by_source: Vec<(Source, Bucket)>,
    pub by_model: Vec<Bucket>,
    pub by_project: Vec<Bucket>,
    pub by_session: Vec<Bucket>,
    /// 独占事件按设备分桶；被多台设备观察到的复制事件只进入一个 Shared 桶。
    pub by_device: Vec<Bucket>,
    pub daily: Vec<DayBucket>,
    /// 24 slots, local hour 0..=23, by total tokens.
    pub by_hour: [Bucket; 24],
    pub first_ts: i64,
    pub last_ts: i64,
    /// Models observed that we have no price for.
    pub unpriced_models: Vec<String>,
}

/// Which events to include.
#[derive(Debug, Clone)]
pub struct Filter {
    pub sources: Vec<Source>,
    /// Inclusive lower bound on local date. `None` means no bound.
    pub since: Option<NaiveDate>,
    /// Inclusive upper bound on local date. This is normally today, so a
    /// transcript written by a misconfigured future clock cannot enter a
    /// current window while remaining absent from its chart.
    pub until: Option<NaiveDate>,
    pub project: Option<String>,
    pub model: Option<String>,
    pub session: Option<String>,
    /// 只显示该设备观察到的事件；共享事件仍会出现，但只计一次。
    pub device: Option<String>,
}

impl Default for Filter {
    fn default() -> Self {
        Filter {
            sources: Source::ALL.to_vec(),
            since: None,
            until: Some(Local::now().date_naive()),
            project: None,
            model: None,
            session: None,
            device: None,
        }
    }
}

impl Filter {
    /// A filter covering the last `days` calendar days including today.
    #[cfg(test)]
    pub fn last_days(days: i64) -> Filter {
        let today = Local::now().date_naive();
        Filter {
            since: Some(today - chrono::Duration::days(days - 1)),
            until: Some(today),
            ..Default::default()
        }
    }

    pub(crate) fn admits(&self, e: &UsageEvent) -> bool {
        if !self.sources.contains(&e.source) {
            return false;
        }
        // Events with no timestamp cannot be placed on the calendar, so a
        // lower-bounded view must exclude them rather than guess. An all-time
        // view still keeps them in the lifetime total, but must continue
        // through the non-date filters below.
        if e.ts == 0 {
            if self.since.is_some() {
                return false;
            }
        } else if self.since.is_some() || self.until.is_some() {
            let Some(date) = local_datetime(e.ts).map(|dt| dt.date_naive()) else {
                return false;
            };
            if self.since.is_some_and(|since| date < since)
                || self.until.is_some_and(|until| date > until)
            {
                return false;
            }
        }
        if let Some(p) = &self.project
            && &e.project != p
        {
            return false;
        }
        if let Some(m) = &self.model
            && &e.model != m
        {
            return false;
        }
        if let Some(s) = &self.session
            && &e.session != s
        {
            return false;
        }
        if let Some(device) = &self.device {
            let admitted = if device == SHARED_DEVICE_ID {
                e.observed_on.len() > 1
            } else {
                e.observed_on.iter().any(|id| id == device)
            };
            if !admitted {
                return false;
            }
        }
        true
    }
}

pub fn local_datetime(ts: i64) -> Option<DateTime<Local>> {
    Local.timestamp_opt(ts, 0).single()
}

/// Build every rollup in a single pass over the events.
pub fn summarize(events: &[UsageEvent], filter: &Filter, pricing: &Pricing) -> Summary {
    let mut s = Summary::default();
    let mut by_source: BTreeMap<Source, Bucket> = BTreeMap::new();
    let mut by_model: HashMap<String, Bucket> = HashMap::new();
    let mut by_project: HashMap<String, Bucket> = HashMap::new();
    let mut by_session: HashMap<(Source, String), Bucket> = HashMap::new();
    let mut by_device: HashMap<String, Bucket> = HashMap::new();
    let mut daily: BTreeMap<NaiveDate, Bucket> = BTreeMap::new();
    let mut hours: Vec<Bucket> = (0..24).map(|h| Bucket::new(format!("{h:02}"))).collect();
    let mut observed: HashSet<&str> = HashSet::new();
    let mut first_ts = i64::MAX;

    for e in events.iter().filter(|e| filter.admits(e)) {
        s.total.absorb(e, pricing);
        observed.insert(e.model.as_str());

        by_source
            .entry(e.source)
            .or_insert_with(|| Bucket::new(e.source.label()))
            .absorb(e, pricing);
        by_model.entry(e.model.clone()).or_insert_with(|| Bucket::new(&e.model)).absorb(e, pricing);
        by_project
            .entry(e.project.clone())
            .or_insert_with(|| Bucket::new(&e.project))
            .absorb(e, pricing);
        by_session
            .entry((e.source, e.session.clone()))
            .or_insert_with(|| Bucket::new(&e.session))
            .absorb(e, pricing);
        if let Some(device) = match e.observed_on.as_slice() {
            [] => None,
            [device] => Some(device.as_str()),
            _ => Some(SHARED_DEVICE_ID),
        } {
            by_device
                .entry(device.to_string())
                .or_insert_with(|| Bucket::new(device))
                .absorb(e, pricing);
        }

        if e.ts > 0 {
            first_ts = first_ts.min(e.ts);
            s.last_ts = s.last_ts.max(e.ts);
            if let Some(dt) = local_datetime(e.ts) {
                daily
                    .entry(dt.date_naive())
                    .or_insert_with(|| Bucket::new(dt.date_naive().to_string()))
                    .absorb(e, pricing);
                hours[dt.hour() as usize].absorb(e, pricing);
            }
        }
    }

    s.first_ts = if first_ts == i64::MAX { 0 } else { first_ts };
    s.by_source = by_source.into_iter().collect();
    s.by_model = sorted_by_tokens(by_model);
    s.by_project = sorted_by_tokens(by_project);
    s.by_session = {
        let mut v: Vec<Bucket> = by_session.into_values().collect();
        // Sessions read as a timeline, so recency wins over volume here.
        v.sort_by(|a, b| b.last_ts.cmp(&a.last_ts).then(b.tokens.total().cmp(&a.tokens.total())));
        v
    };
    s.by_device = sorted_by_tokens(by_device);
    s.daily = daily.into_iter().map(|(date, bucket)| DayBucket { date, bucket }).collect();
    s.by_hour = hours.try_into().expect("24 hour buckets");
    s.unpriced_models = pricing.unpriced_among(observed);
    s
}

impl Summary {
    /// Today's totals, under whatever filter produced this summary.
    ///
    /// Derived from `daily` rather than accumulated separately, so it can
    /// never disagree with the day it is a row of. `None` means no billed
    /// request has landed today — which is not the same as zero, and the
    /// callers that care render it differently.
    pub fn today(&self) -> Option<&Bucket> {
        let today = Local::now().date_naive();
        self.daily.iter().find(|d| d.date == today).map(|d| &d.bucket)
    }
}

/// Highest-valued key, with the name as a tiebreak so the answer is stable
/// across runs rather than dependent on hash order.
fn top_key(map: &HashMap<String, u64>) -> Option<&str> {
    map.iter().max_by(|a, b| a.1.cmp(b.1).then_with(|| b.0.cmp(a.0))).map(|(k, _)| k.as_str())
}

fn sorted_by_tokens(map: HashMap<String, Bucket>) -> Vec<Bucket> {
    let mut v: Vec<Bucket> = map.into_values().collect();
    v.sort_by(|a, b| b.tokens.total().cmp(&a.tokens.total()).then_with(|| a.label.cmp(&b.label)));
    v
}

/// Fill in days with no activity so a trend chart has a continuous x-axis.
///
/// Without this, a gap of idle days compresses into nothing and the chart
/// silently misrepresents the cadence of work.
pub fn dense_daily(daily: &[DayBucket], days: usize) -> Vec<(NaiveDate, u64, f64)> {
    let end = Local::now().date_naive();
    let start = end - chrono::Duration::days(days as i64 - 1);
    let mut index: HashMap<NaiveDate, (u64, f64)> = HashMap::new();
    for d in daily {
        index.insert(d.date, (d.bucket.tokens.total(), d.bucket.priced.cost));
    }
    (0..days)
        .map(|i| {
            let date = start + chrono::Duration::days(i as i64);
            let (t, c) = index.get(&date).copied().unwrap_or((0, 0.0));
            (date, t, c)
        })
        .collect()
}

/// Streak of consecutive days ending today (or yesterday) with activity.
pub fn current_streak(daily: &[DayBucket]) -> u32 {
    let active: HashSet<NaiveDate> =
        daily.iter().filter(|d| d.bucket.tokens.total() > 0).map(|d| d.date).collect();
    let today = Local::now().date_naive();
    let mut cursor =
        if active.contains(&today) { today } else { today.pred_opt().unwrap_or(today) };
    if !active.contains(&cursor) {
        return 0;
    }
    let mut n = 0;
    while active.contains(&cursor) {
        n += 1;
        match cursor.pred_opt() {
            Some(p) => cursor = p,
            None => break,
        }
    }
    n
}

/// Month-to-date totals, for the budget line.
pub fn month_to_date(daily: &[DayBucket]) -> Priced {
    let now = Local::now().date_naive();
    let mut acc = Priced::default();
    for d in daily {
        if d.date.year() == now.year() && d.date.month() == now.month() {
            acc.add(&d.bucket.priced);
        }
    }
    acc
}

// ── Insights ────────────────────────────────────────────────────────────────
//
// Everything below is derived from a [`Summary`] rather than from the event
// stream, for the same reason every page is: two things computed from one
// summary cannot disagree with each other, and the summary is already the
// answer to "what is in this window".

/// How many ranked rows an insight list keeps.
///
/// A corpus can hold tens of thousands of sessions and none of them past the
/// first screenful is being read. The cap is surfaced in the output rather
/// than applied quietly — a list that says "top 100" is honest, one that shows
/// 100 of 9,000 rows without saying so is not.
pub const RANKED_LIMIT: usize = 100;

/// One session, ranked by something derived rather than by raw volume.
#[derive(Debug, Clone)]
pub struct SessionInsight {
    pub source: Source,
    pub session: String,
    pub project: String,
    pub model: String,
    pub tokens: Tokens,
    pub priced: Priced,
    pub events: u64,
    pub last_ts: i64,
}

impl SessionInsight {
    /// Context carried per request. `None` for a session with no requests,
    /// which cannot have an average.
    pub fn context_per_request(&self) -> Option<f64> {
        (self.events > 0).then(|| self.tokens.context() as f64 / self.events as f64)
    }
}

/// A cost ranking row — a project or a model, with the coverage behind it.
#[derive(Debug, Clone)]
pub struct CostRow {
    pub label: String,
    pub cost: f64,
    pub coverage: f64,
    pub tokens: u64,
    pub events: u64,
    pub sessions: usize,
}

/// One measure across two windows.
#[derive(Debug, Clone, Copy, Default)]
pub struct Change {
    pub current: f64,
    pub previous: f64,
}

impl Change {
    pub fn delta(self) -> f64 {
        self.current - self.previous
    }

    /// Relative change, as a fraction. `None` when the previous window held
    /// nothing: a rise from zero is not a percentage, and rendering one as
    /// `+∞%` or `+100%` would both be inventions.
    pub fn ratio(self) -> Option<f64> {
        (self.previous > 0.0).then(|| (self.current - self.previous) / self.previous)
    }
}

/// This window against the one immediately before it.
#[derive(Debug, Clone, Copy)]
pub struct Comparison {
    pub tokens: Change,
    pub cost: Change,
    pub requests: Change,
    pub sessions: Change,
    /// Cost coverage of the earlier window. A delta drawn between a fully
    /// priced window and a partly priced one is not a like-for-like figure,
    /// and the renderers mark it.
    pub previous_coverage: f64,
    /// Calendar days each side of the comparison covers.
    pub span_days: u32,
}

/// Derived metrics: the ratios and rates behind the totals.
#[derive(Debug, Clone, Default)]
pub struct Insights {
    /// Calendar days this window covers, whether or not they saw work.
    pub span_days: u32,
    /// Days inside it that billed at least one request.
    pub active_days: u32,
    /// The window's raw token mix, so every ratio below can be checked
    /// against the counts it came from.
    pub tokens: Tokens,

    /// Cache reads as a share of all context sent. `None` when nothing was
    /// sent — which is not the same claim as a 0% hit rate.
    pub cache_hit_ratio: Option<f64>,
    /// Output tokens produced per token of context sent.
    pub output_per_context: Option<f64>,
    /// Context carried by the average request.
    pub context_per_request: Option<f64>,
    pub tokens_per_request: Option<f64>,
    pub requests_per_session: Option<f64>,

    pub tokens_per_day: f64,
    pub cost_per_day: f64,
    /// Spend divided by the days that actually saw work, which is the rate a
    /// working day costs rather than the rate a calendar day does.
    pub cost_per_active_day: f64,
    pub cost_per_session: f64,
    pub cost_per_request: f64,
    /// How much of the cost figures above is backed by a known rate.
    pub cost_coverage: f64,

    pub month_to_date: Priced,
    /// The current calendar month at its month-to-date run rate. `None` when
    /// the window does not reach back to the first of the month, because a
    /// projection from a 7-day slice of a 20-day month is not a projection.
    pub projected_month: Option<f64>,

    pub previous: Option<Comparison>,

    /// Sessions by estimated cost, most expensive first.
    pub costly_sessions: Vec<SessionInsight>,
    /// How many sessions the ranking above was taken from.
    pub session_total: usize,
    /// Sessions by context per request, heaviest first.
    pub context_heavy: Vec<SessionInsight>,
    /// Projects by estimated cost.
    pub costly_projects: Vec<CostRow>,
    /// Models by estimated cost.
    pub costly_models: Vec<CostRow>,
}

impl Insights {
    pub fn is_empty(&self) -> bool {
        self.session_total == 0 && self.costly_projects.is_empty()
    }
}

/// The window immediately before this one, of the same length.
///
/// Week-over-week only means anything against a comparable stretch of
/// calendar, so only the dates move — sources, project, model and device
/// carry over untouched. An unbounded range has no period before it and gets
/// `None` rather than a window invented for the sake of a delta.
pub fn previous_window(filter: &Filter) -> Option<Filter> {
    let (since, until) = (filter.since?, filter.until?);
    let span = (until - since).num_days().checked_add(1)?;
    if span <= 0 {
        return None;
    }
    let prev_until = since.pred_opt()?;
    let prev_since = prev_until.checked_sub_signed(chrono::Duration::days(span - 1))?;
    Some(Filter { since: Some(prev_since), until: Some(prev_until), ..filter.clone() })
}

/// Derive the insight metrics for a window.
///
/// `previous` is the summary of [`previous_window`] under the same filter, and
/// `window_days` the length of the range chip that produced `summary` —
/// `None` for all-time, where the corpus itself sets the span.
pub fn insights(
    summary: &Summary,
    previous: Option<&Summary>,
    window_days: Option<i64>,
) -> Insights {
    let total = &summary.total;
    let today = Local::now().date_naive();
    let span_days = match window_days {
        Some(days) => days.clamp(1, i64::from(u32::MAX)) as u32,
        // All-time: the window is however long this corpus has been running.
        None => local_datetime(summary.first_ts)
            .map(|dt| dt.date_naive())
            .map_or(summary.daily.len().max(1) as u32, |first| {
                ((today - first).num_days() + 1).clamp(1, i64::from(u32::MAX)) as u32
            }),
    };
    let active_days = summary.daily.len() as u32;

    let context = total.tokens.context() as f64;
    let requests = total.events as f64;
    let sessions = total.session_count() as f64;
    let cost = total.priced.cost;
    let per =
        |numerator: f64, denominator: f64| (denominator > 0.0).then_some(numerator / denominator);

    // A month projected from a fraction of itself is a guess dressed as a
    // figure, so it only appears once the window covers the month so far.
    let day_of_month = i64::from(today.day());
    let month_to_date = month_to_date(&summary.daily);
    let projected_month = window_days
        .is_none_or(|days| days >= day_of_month)
        .then(|| month_to_date.cost / day_of_month as f64 * f64::from(days_in_month(today)));

    Insights {
        span_days,
        active_days,
        tokens: total.tokens,

        cache_hit_ratio: per(total.tokens.cache_read as f64, context),
        output_per_context: per(total.tokens.output as f64, context),
        context_per_request: per(context, requests),
        tokens_per_request: per(total.tokens.total() as f64, requests),
        requests_per_session: per(requests, sessions),

        tokens_per_day: total.tokens.total() as f64 / f64::from(span_days),
        cost_per_day: cost / f64::from(span_days),
        cost_per_active_day: per(cost, f64::from(active_days)).unwrap_or(0.0),
        cost_per_session: per(cost, sessions).unwrap_or(0.0),
        cost_per_request: per(cost, requests).unwrap_or(0.0),
        cost_coverage: total.priced.coverage(),

        month_to_date,
        projected_month,

        previous: previous.map(|prev| Comparison {
            tokens: Change {
                current: total.tokens.total() as f64,
                previous: prev.total.tokens.total() as f64,
            },
            cost: Change { current: cost, previous: prev.total.priced.cost },
            requests: Change { current: requests, previous: prev.total.events as f64 },
            sessions: Change { current: sessions, previous: prev.total.session_count() as f64 },
            previous_coverage: prev.total.priced.coverage(),
            span_days,
        }),

        costly_sessions: ranked_sessions(summary, |a, b| {
            b.priced
                .cost
                .total_cmp(&a.priced.cost)
                .then_with(|| b.tokens.total().cmp(&a.tokens.total()))
                .then_with(|| a.session.cmp(&b.session))
        }),
        session_total: summary.by_session.len(),
        // A one-request session is all context and no average, and would top
        // this list on a single large paste. Two requests is the smallest
        // number that makes "per request" mean anything.
        context_heavy: ranked_sessions(summary, |a, b| {
            let (x, y) = (a.events >= 2, b.events >= 2);
            y.cmp(&x)
                .then_with(|| {
                    b.context_per_request()
                        .unwrap_or(0.0)
                        .total_cmp(&a.context_per_request().unwrap_or(0.0))
                })
                .then_with(|| a.session.cmp(&b.session))
        }),
        costly_projects: cost_rows(&summary.by_project),
        costly_models: cost_rows(&summary.by_model),
    }
}

fn ranked_sessions(
    summary: &Summary,
    order: impl Fn(&SessionInsight, &SessionInsight) -> std::cmp::Ordering,
) -> Vec<SessionInsight> {
    let mut rows: Vec<SessionInsight> = summary
        .by_session
        .iter()
        .filter_map(|bucket| {
            Some(SessionInsight {
                // Sessions bucket by (source, id), so a bucket has exactly one
                // source; a bucket with none held no admitted event.
                source: *bucket.sources.iter().next()?,
                session: bucket.label.clone(),
                project: bucket.top_project().unwrap_or("unknown").to_string(),
                model: bucket.top_model().unwrap_or("unknown").to_string(),
                tokens: bucket.tokens,
                priced: bucket.priced,
                events: bucket.events,
                last_ts: bucket.last_ts,
            })
        })
        .collect();
    rows.sort_by(order);
    rows.truncate(RANKED_LIMIT);
    rows
}

fn cost_rows(buckets: &[Bucket]) -> Vec<CostRow> {
    let mut rows: Vec<CostRow> = buckets
        .iter()
        .map(|bucket| CostRow {
            label: bucket.label.clone(),
            cost: bucket.priced.cost,
            coverage: bucket.priced.coverage(),
            tokens: bucket.tokens.total(),
            events: bucket.events,
            sessions: bucket.session_count(),
        })
        .collect();
    rows.sort_by(|a, b| {
        b.cost
            .total_cmp(&a.cost)
            .then_with(|| b.tokens.cmp(&a.tokens))
            .then_with(|| a.label.cmp(&b.label))
    });
    rows.truncate(RANKED_LIMIT);
    rows
}

/// Days in `date`'s calendar month.
fn days_in_month(date: NaiveDate) -> u32 {
    let (year, month) = (date.year(), date.month());
    let (next_year, next_month) = if month == 12 { (year + 1, 1) } else { (year, month + 1) };
    NaiveDate::from_ymd_opt(next_year, next_month, 1)
        .zip(NaiveDate::from_ymd_opt(year, month, 1))
        .map_or(30, |(next, first)| (next - first).num_days() as u32)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    fn ev(
        source: Source,
        model: &str,
        project: &str,
        session: &str,
        ts: i64,
        out: u64,
    ) -> UsageEvent {
        UsageEvent {
            source,
            ts,
            model: model.into(),
            project: project.into(),
            session: session.into(),
            tokens: Tokens { input: 10, output: out, ..Default::default() },
            observed_on: Vec::new(),
            dedup_key: None,
            dedup_rank: 0,
        }
    }

    fn at(days_ago: i64, hour: u32) -> i64 {
        let d = Local::now().date_naive() - Duration::days(days_ago);
        Local.from_local_datetime(&d.and_hms_opt(hour, 0, 0).unwrap()).unwrap().timestamp()
    }

    #[test]
    fn rollups_agree_with_the_total() {
        let p = Pricing::builtin();
        let events = vec![
            ev(Source::Claude, "claude-opus-5", "alpha", "s1", at(0, 9), 100),
            ev(Source::Claude, "claude-sonnet-5", "alpha", "s1", at(0, 9), 200),
            ev(Source::Codex, "gpt-5.2", "beta", "s2", at(1, 14), 300),
        ];
        let s = summarize(&events, &Filter::default(), &p);
        assert_eq!(s.total.events, 3);
        assert_eq!(s.total.tokens.output, 600);
        assert_eq!(s.by_model.iter().map(|b| b.tokens.output).sum::<u64>(), 600);
        assert_eq!(s.by_project.iter().map(|b| b.tokens.output).sum::<u64>(), 600);
        assert_eq!(s.by_source.iter().map(|(_, b)| b.tokens.output).sum::<u64>(), 600);
        assert_eq!(s.daily.iter().map(|d| d.bucket.tokens.output).sum::<u64>(), 600);
        assert_eq!(s.by_hour.iter().map(|b| b.tokens.output).sum::<u64>(), 600);
    }

    #[test]
    fn today_is_the_same_number_however_wide_the_window() {
        // The figure the dashboard shows for today has to be the figure a
        // one-day window totals, or one of the two is lying.
        let p = Pricing::builtin();
        let events = vec![
            ev(Source::Claude, "claude-opus-5", "alpha", "s1", at(0, 9), 100),
            ev(Source::Claude, "claude-opus-5", "alpha", "s2", at(0, 21), 200),
            ev(Source::Codex, "gpt-5.2", "beta", "s3", at(1, 14), 300),
            ev(Source::Codex, "gpt-5.2", "beta", "s4", at(20, 14), 400),
        ];
        let wide = summarize(&events, &Filter::default(), &p);
        let narrow = summarize(&events, &Filter::last_days(1), &p);

        let today = wide.today().expect("two events landed today");
        assert_eq!(today.tokens.total(), narrow.total.tokens.total());
        assert_eq!(today.events, 2);
        assert_eq!(today.session_count(), 2);
        assert_eq!(today.priced.cost, narrow.total.priced.cost);
    }

    #[test]
    fn today_is_absent_rather_than_zero_when_nothing_was_billed() {
        // `None` and a zeroed bucket are different claims: one says no request
        // has landed today, the other says one landed and cost nothing.
        let p = Pricing::builtin();
        let events = vec![ev(Source::Claude, "claude-opus-5", "alpha", "s1", at(3, 9), 100)];
        let s = summarize(&events, &Filter::default(), &p);
        assert!(s.today().is_none());
    }

    #[test]
    fn today_respects_the_filter_it_was_summarized_under() {
        let p = Pricing::builtin();
        let events = vec![
            ev(Source::Claude, "claude-opus-5", "alpha", "s1", at(0, 9), 100),
            ev(Source::Codex, "gpt-5.2", "beta", "s2", at(0, 9), 300),
        ];
        let claude_only = Filter { sources: vec![Source::Claude], ..Default::default() };
        let s = summarize(&events, &claude_only, &p);
        assert_eq!(s.today().unwrap().events, 1, "the excluded tool must not count");
    }

    #[test]
    fn model_rows_are_ranked_by_token_volume() {
        let p = Pricing::builtin();
        let events = vec![
            ev(Source::Claude, "small", "a", "s", at(0, 1), 1),
            ev(Source::Claude, "big", "a", "s", at(0, 1), 1000),
        ];
        let s = summarize(&events, &Filter::default(), &p);
        assert_eq!(s.by_model[0].label, "big");
    }

    #[test]
    fn unpriced_models_are_surfaced_and_cost_coverage_drops() {
        let p = Pricing::builtin();
        let events = vec![
            ev(Source::Claude, "claude-opus-5", "a", "s", at(0, 1), 100),
            // A model with no rate on file — `codex-auto-review` is the real
            // one that shows up in transcripts and is not a billable model id.
            ev(Source::Codex, "codex-auto-review", "a", "s", at(0, 1), 100),
        ];
        let s = summarize(&events, &Filter::default(), &p);
        assert_eq!(s.unpriced_models, vec!["codex-auto-review".to_string()]);
        assert!(s.total.priced.coverage() < 1.0);
        assert!(s.total.priced.coverage() > 0.0);
    }

    #[test]
    fn a_date_window_excludes_older_events() {
        let p = Pricing::builtin();
        let events = vec![
            ev(Source::Claude, "claude-opus-5", "a", "s", at(0, 9), 1),
            ev(Source::Claude, "claude-opus-5", "a", "s", at(10, 9), 1),
        ];
        let s = summarize(&events, &Filter::last_days(7), &p);
        assert_eq!(s.total.events, 1);
    }

    #[test]
    fn a_current_window_excludes_future_events() {
        let p = Pricing::builtin();
        let events = vec![
            ev(Source::Claude, "claude-opus-5", "a", "past", at(0, 9), 1),
            ev(Source::Claude, "claude-opus-5", "a", "future", at(-1, 9), 1),
        ];
        let s = summarize(&events, &Filter::last_days(7), &p);
        assert_eq!(s.total.events, 1);
        assert_eq!(s.daily.len(), 1, "the headline and the calendar must cover the same dates");
    }

    #[test]
    fn undated_events_are_excluded_from_a_windowed_view_not_guessed_into_it() {
        let p = Pricing::builtin();
        let events = vec![ev(Source::Claude, "claude-opus-5", "a", "s", 0, 5)];
        assert_eq!(summarize(&events, &Filter::last_days(7), &p).total.events, 0);
        // With no window they still count toward lifetime totals.
        assert_eq!(summarize(&events, &Filter::default(), &p).total.events, 1);
    }

    #[test]
    fn undated_events_still_respect_non_date_filters() {
        let p = Pricing::builtin();
        let events = vec![
            ev(Source::Claude, "claude-opus-5", "alpha", "wanted", 0, 1),
            ev(Source::Claude, "claude-opus-5", "beta", "wanted", 0, 1),
            ev(Source::Claude, "other", "alpha", "wanted", 0, 1),
            ev(Source::Claude, "claude-opus-5", "alpha", "other", 0, 1),
        ];
        let filter = Filter {
            project: Some("alpha".into()),
            model: Some("claude-opus-5".into()),
            session: Some("wanted".into()),
            ..Default::default()
        };
        assert_eq!(summarize(&events, &filter, &p).total.events, 1);
    }

    #[test]
    fn source_and_project_filters_compose() {
        let p = Pricing::builtin();
        let events = vec![
            ev(Source::Claude, "claude-opus-5", "alpha", "s", at(0, 9), 1),
            ev(Source::Codex, "gpt-5.2", "alpha", "s", at(0, 9), 1),
            ev(Source::Claude, "claude-opus-5", "beta", "s", at(0, 9), 1),
        ];
        let f = Filter {
            sources: vec![Source::Claude],
            project: Some("alpha".into()),
            ..Default::default()
        };
        assert_eq!(summarize(&events, &f, &p).total.events, 1);
    }

    #[test]
    fn projects_with_the_same_basename_remain_distinct() {
        let p = Pricing::builtin();
        let events = vec![
            ev(Source::Claude, "claude-opus-5", "/work/client/api", "s1", at(0, 9), 1),
            ev(Source::Claude, "claude-opus-5", "/home/me/api", "s2", at(0, 9), 1),
        ];
        let s = summarize(&events, &Filter::default(), &p);
        assert_eq!(s.by_project.len(), 2);
        let f = Filter { project: Some("/work/client/api".into()), ..Default::default() };
        assert_eq!(summarize(&events, &f, &p).total.events, 1);
    }

    #[test]
    fn idle_days_appear_as_zeros_rather_than_vanishing() {
        let p = Pricing::builtin();
        let events = vec![
            ev(Source::Claude, "claude-opus-5", "a", "s", at(0, 9), 1),
            ev(Source::Claude, "claude-opus-5", "a", "s", at(3, 9), 1),
        ];
        let s = summarize(&events, &Filter::default(), &p);
        let dense = dense_daily(&s.daily, 5);
        assert_eq!(dense.len(), 5);
        assert_eq!(dense.iter().filter(|(_, t, _)| *t == 0).count(), 3);
    }

    #[test]
    fn a_streak_counts_back_from_today() {
        let p = Pricing::builtin();
        let events: Vec<_> =
            (0..3).map(|d| ev(Source::Claude, "claude-opus-5", "a", "s", at(d, 9), 1)).collect();
        let s = summarize(&events, &Filter::default(), &p);
        assert_eq!(current_streak(&s.daily), 3);
    }

    #[test]
    fn sessions_are_counted_distinctly_not_summed() {
        let p = Pricing::builtin();
        let events = vec![
            ev(Source::Claude, "claude-opus-5", "a", "s1", at(0, 9), 1),
            ev(Source::Claude, "claude-opus-5", "a", "s1", at(0, 10), 1),
            ev(Source::Claude, "claude-opus-5", "a", "s2", at(0, 11), 1),
        ];
        let s = summarize(&events, &Filter::default(), &p);
        assert_eq!(s.total.session_count(), 2);
    }

    #[test]
    fn copied_events_enter_one_shared_device_bucket() {
        let mut copied = ev(Source::Codex, "gpt", "p", "s", Local::now().timestamp(), 10);
        copied.observed_on = vec!["dev-a".into(), "dev-b".into()];
        let summary = summarize(&[copied], &Filter::default(), &Pricing::builtin());
        assert_eq!(summary.total.tokens.output, 10);
        assert_eq!(summary.by_device.len(), 1);
        assert_eq!(summary.by_device[0].label, SHARED_DEVICE_ID);
        assert_eq!(summary.by_device[0].tokens.output, 10);
    }

    #[test]
    fn shared_device_filter_selects_only_events_seen_on_multiple_devices() {
        let now = Local::now().timestamp();
        let mut copied = ev(Source::Codex, "gpt", "p", "shared", now, 10);
        copied.observed_on = vec!["dev-a".into(), "dev-b".into()];
        let mut local = ev(Source::Codex, "gpt", "p", "local", now, 20);
        local.observed_on = vec!["dev-a".into()];
        let filter = Filter { device: Some(SHARED_DEVICE_ID.into()), ..Filter::default() };
        let summary = summarize(&[copied, local], &filter, &Pricing::builtin());
        assert_eq!(summary.total.tokens.output, 10);
        assert_eq!(summary.total.events, 1);
    }

    /// An event with an explicit token mix, for the ratio assertions.
    fn mixed(session: &str, ts: i64, tokens: Tokens) -> UsageEvent {
        UsageEvent {
            source: Source::Claude,
            ts,
            model: "claude-opus-5".into(),
            project: "alpha".into(),
            session: session.into(),
            tokens,
            observed_on: Vec::new(),
            dedup_key: None,
            dedup_rank: 0,
        }
    }

    fn derive(events: &[UsageEvent], days: Option<i64>) -> Insights {
        let p = Pricing::builtin();
        let filter = match days {
            Some(d) => Filter::last_days(d),
            None => Filter::default(),
        };
        let summary = summarize(events, &filter, &p);
        let previous = previous_window(&filter).map(|f| summarize(events, &f, &p));
        insights(&summary, previous.as_ref(), days)
    }

    #[test]
    fn the_previous_window_sits_immediately_before_and_is_the_same_length() {
        let today = Local::now().date_naive();
        let filter = Filter::last_days(7);
        let prev = previous_window(&filter).expect("a bounded window has one before it");
        assert_eq!(prev.until, Some(today - Duration::days(7)));
        assert_eq!(prev.since, Some(today - Duration::days(13)));
        // Only the dates move: a comparison across different filters would be
        // measuring two different things.
        assert_eq!(prev.sources, filter.sources);
        assert_eq!(prev.project, filter.project);
    }

    #[test]
    fn an_unbounded_range_has_no_period_before_it() {
        assert!(previous_window(&Filter::default()).is_none(), "all time has no previous all time");
    }

    #[test]
    fn cache_hit_ratio_measures_reads_against_everything_sent_not_against_the_total() {
        // Output is billed but never sent, so it must stay out of the
        // denominator — otherwise a talkative model looks like a cache miss.
        let events = vec![mixed(
            "s1",
            at(0, 9),
            Tokens {
                input: 100,
                output: 900,
                cache_read: 300,
                cache_write_5m: 100,
                ..Default::default()
            },
        )];
        let i = derive(&events, Some(7));
        assert_eq!(i.cache_hit_ratio, Some(0.6), "300 of 500 tokens of context came from cache");
        assert_eq!(i.context_per_request, Some(500.0));
        assert_eq!(i.tokens_per_request, Some(1400.0));
    }

    #[test]
    fn ratios_are_absent_rather_than_zero_when_there_is_nothing_to_divide() {
        let i = derive(&[], Some(7));
        assert_eq!(i.cache_hit_ratio, None, "a 0% hit rate is a different claim from no data");
        assert_eq!(i.tokens_per_request, None);
        assert_eq!(i.requests_per_session, None);
        assert!(i.is_empty());
    }

    #[test]
    fn a_rise_from_an_empty_window_has_no_percentage() {
        let events = vec![ev(Source::Claude, "claude-opus-5", "alpha", "s1", at(0, 9), 100)];
        let i = derive(&events, Some(7));
        let prev = i.previous.expect("a 7-day window compares against the 7 before it");
        assert!(prev.tokens.previous == 0.0);
        assert_eq!(prev.tokens.ratio(), None, "growth from nothing is not a percentage");
        assert!(prev.tokens.delta() > 0.0, "the absolute change is still real");
    }

    #[test]
    fn week_over_week_compares_against_the_previous_seven_days() {
        let events = vec![
            ev(Source::Claude, "claude-opus-5", "alpha", "now", at(1, 9), 100),
            ev(Source::Claude, "claude-opus-5", "alpha", "before", at(9, 9), 50),
        ];
        let i = derive(&events, Some(7));
        let prev = i.previous.expect("bounded windows compare");
        assert_eq!((prev.requests.current, prev.requests.previous), (1.0, 1.0));
        assert!((prev.tokens.current - 110.0).abs() < 1e-9, "10 input + 100 output this week");
        assert!((prev.tokens.previous - 60.0).abs() < 1e-9, "10 input + 50 output last week");
        assert!((prev.tokens.ratio().unwrap() - (110.0 - 60.0) / 60.0).abs() < 1e-9);
    }

    #[test]
    fn the_costliest_session_leads_the_ranking_and_the_cap_is_knowable() {
        let events = vec![
            mixed("cheap", at(0, 9), Tokens { output: 10, ..Default::default() }),
            mixed("dear", at(0, 10), Tokens { output: 10_000, ..Default::default() }),
        ];
        let i = derive(&events, Some(7));
        assert_eq!(i.costly_sessions[0].session, "dear");
        assert_eq!(i.costly_sessions.len(), 2);
        // The total is reported beside the ranking, so a truncated list can
        // never read as the whole corpus.
        assert_eq!(i.session_total, 2);
    }

    #[test]
    fn a_single_request_session_never_tops_the_context_ranking() {
        // One enormous paste is not a context-heavy way of working, and left
        // unguarded it would outrank every real session on the page.
        let events = vec![
            mixed("one-shot", at(0, 9), Tokens { input: 900_000, ..Default::default() }),
            mixed("sustained", at(0, 10), Tokens { input: 100_000, ..Default::default() }),
            mixed("sustained", at(0, 11), Tokens { input: 100_000, ..Default::default() }),
        ];
        let i = derive(&events, Some(7));
        assert_eq!(i.context_heavy[0].session, "sustained");
        assert_eq!(i.context_heavy[0].context_per_request(), Some(100_000.0));
        assert_eq!(i.context_heavy[1].session, "one-shot");
    }

    #[test]
    fn a_month_is_not_projected_from_a_window_that_does_not_cover_it() {
        let events = vec![ev(Source::Claude, "claude-opus-5", "alpha", "s1", at(0, 9), 100)];
        // All time always reaches the first of the month.
        assert!(derive(&events, None).projected_month.is_some());
        // A one-day window only does on the first, so the assertion is on the
        // rule rather than on today's date.
        let today = Local::now().date_naive();
        let covered = derive(&events, Some(1)).projected_month.is_some();
        assert_eq!(covered, today.day() == 1);
    }

    #[test]
    fn burn_rate_divides_by_the_window_not_by_the_days_that_happened_to_be_busy() {
        // Two active days inside a 30-day window: the calendar rate and the
        // working-day rate are different numbers and both are wanted.
        let events = vec![
            ev(Source::Claude, "claude-opus-5", "alpha", "s1", at(0, 9), 100),
            ev(Source::Claude, "claude-opus-5", "alpha", "s2", at(1, 9), 100),
        ];
        let i = derive(&events, Some(30));
        assert_eq!(i.span_days, 30);
        assert_eq!(i.active_days, 2);
        assert!((i.cost_per_day * 30.0 - i.cost_per_active_day * 2.0).abs() < 1e-9);
        assert!(i.cost_per_active_day > i.cost_per_day);
    }

    #[test]
    fn identical_session_ids_from_different_tools_remain_distinct() {
        let now = Local::now().timestamp();
        let events = [
            ev(Source::Claude, "claude", "p", "same", now, 10),
            ev(Source::Codex, "gpt", "p", "same", now, 20),
        ];
        let summary = summarize(&events, &Filter::default(), &Pricing::builtin());
        assert_eq!(summary.by_session.len(), 2);
        assert_eq!(summary.total.session_count(), 2);
    }
}
