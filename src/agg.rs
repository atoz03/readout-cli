//! Rollups.
//!
//! Every view is derived from the same event stream, so a filter applied once
//! is reflected everywhere consistently. Dates and hours are bucketed in
//! **local time** — "when you work" is a question about your day, not UTC's.

use crate::model::{Source, Tokens, UsageEvent};
use crate::pricing::{Priced, Pricing, price};
use chrono::{DateTime, Datelike, Local, NaiveDate, TimeZone, Timelike};
use std::collections::{BTreeMap, HashMap, HashSet};

/// One row of any breakdown: a label plus its totals.
#[derive(Debug, Clone, Default)]
pub struct Bucket {
    pub label: String,
    pub tokens: Tokens,
    pub priced: Priced,
    pub events: u64,
    pub sessions: HashSet<String>,
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
        self.events += 1;
        if !e.session.is_empty() {
            self.sessions.insert(e.session.clone());
        }
        self.last_ts = self.last_ts.max(e.ts);
        *self.models.entry(e.model.clone()).or_default() += e.tokens.total();
        *self.projects.entry(e.project.clone()).or_default() += e.tokens.total();
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

    fn admits(&self, e: &UsageEvent) -> bool {
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
    let mut by_session: HashMap<String, Bucket> = HashMap::new();
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
            .entry(e.session.clone())
            .or_insert_with(|| Bucket::new(&e.session))
            .absorb(e, pricing);

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
}
