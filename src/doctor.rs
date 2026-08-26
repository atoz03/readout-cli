//! Diagnostics: whether the numbers everywhere else can be trusted.
//!
//! Every other view answers "how much". This one answers "how much of it did
//! readout actually see" — which transcripts were found, which lines could not
//! be read, how much spend is excluded for want of a rate, whether the cache
//! and the synced devices are current.
//!
//! It is strictly a diagnosis. Nothing here deletes a cache, contacts a device,
//! or edits a price: each finding names the command that would, and leaves it
//! to the reader. A tool whose job is to tell you something is wrong must not
//! also be the tool that quietly changes things.

use crate::agg::{Filter, Summary};
use crate::devices::DeviceRecord;
use crate::fmt;
use crate::model::{Source, UsageEvent};
use crate::pricing::Pricing;
use crate::scan::ScanStats;
use serde_json::json;
use std::fmt::Write as _;

/// A snapshot older than this is stale enough to say so. Long enough that a
/// machine left off over a weekend does not read as broken.
const STALE_SNAPSHOT_SECS: i64 = 3 * 86_400;

/// Share of tokens that may sit on unpriced models before the cost figures
/// stop being a usable estimate.
const UNPRICED_SERIOUS: f64 = 0.10;

/// How bad a finding is.
///
/// `Fail` is reserved for "a number readout reports is wrong or missing", so
/// that a nonzero exit code means something specific enough to gate CI on.
/// Everything explicable — an absent tool, a corpus with no cache yet — is
/// `Note` and exits zero.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Ok,
    Note,
    Warn,
    Fail,
}

impl Level {
    pub fn label(self) -> &'static str {
        match self {
            Level::Ok => "ok",
            Level::Note => "note",
            Level::Warn => "warn",
            Level::Fail => "fail",
        }
    }

    /// Status glyphs, so severity is never carried by colour alone.
    pub fn glyph(self) -> &'static str {
        match self {
            Level::Ok => "✓",
            Level::Note => "ⓘ",
            Level::Warn => "▲",
            Level::Fail => "✕",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Check {
    /// Stable machine-readable name, so a script can key on one finding.
    pub id: &'static str,
    pub title: &'static str,
    pub level: Level,
    /// One line saying what was found.
    pub summary: String,
    /// Supporting lines: the files, the models, the devices.
    pub detail: Vec<String>,
    /// The command that would fix it, when one exists.
    pub remedy: Option<String>,
}

impl Check {
    fn new(id: &'static str, title: &'static str, level: Level, summary: String) -> Check {
        Check { id, title, level, summary, detail: Vec::new(), remedy: None }
    }

    fn with_detail(mut self, detail: Vec<String>) -> Check {
        self.detail = detail;
        self
    }

    fn with_remedy(mut self, remedy: impl Into<String>) -> Check {
        self.remedy = Some(remedy.into());
        self
    }
}

#[derive(Debug, Clone)]
pub struct Report {
    pub checks: Vec<Check>,
}

impl Report {
    /// The worst thing found, which is what the exit code reports.
    pub fn worst(&self) -> Level {
        self.checks.iter().map(|c| c.level).max().unwrap_or(Level::Ok)
    }

    /// Whether the exit code should be nonzero.
    ///
    /// Only a `Fail` qualifies. A warning is worth reading and worth acting on,
    /// but a check that goes red because a laptop has been off for a week would
    /// make `readout doctor` useless in CI.
    pub fn failed(&self) -> bool {
        self.worst() == Level::Fail
    }
}

/// Everything the checks read. Gathered by the caller so that this module runs
/// one scan's worth of already-loaded state and performs no I/O of its own
/// beyond stat-ing readout's own cache.
pub struct Input<'a> {
    pub events: &'a [UsageEvent],
    pub summary: &'a Summary,
    pub stats: &'a ScanStats,
    pub devices: &'a [DeviceRecord],
    /// Snapshots that would not load, from `LoadedUsage::warnings`.
    pub device_warnings: &'a [String],
    pub pricing: &'a Pricing,
    pub filter: &'a Filter,
    pub sources: &'a [Source],
    pub cache_path: Option<std::path::PathBuf>,
}

pub fn run(input: &Input<'_>) -> Report {
    Report {
        checks: vec![
            transcript_sources(input),
            coverage(input),
            malformed(input),
            duplicates(input),
            timestamps(input),
            unpriced(input),
            cache_health(input),
            device_freshness(input),
        ],
    }
}

/// Are the trees readout reads actually there?
fn transcript_sources(input: &Input<'_>) -> Check {
    let missing = crate::report::missing_sources(input.sources);
    let mut detail = Vec::new();
    for (source, dir) in [
        (Source::Claude, crate::paths::claude_projects_dir()),
        (Source::Codex, crate::paths::codex_sessions_dir()),
    ] {
        if !input.sources.contains(&source) {
            continue;
        }
        detail.push(match dir {
            Some(path) => format!("{:<12} {}", source.short(), path.display()),
            None => format!("{:<12} not found", source.short()),
        });
    }
    if missing.len() == input.sources.len() {
        return Check::new(
            "sources",
            "Transcript sources",
            // Both tools absent is the whole product having nothing to read.
            Level::Fail,
            "no transcript directory was found for any selected tool".to_string(),
        )
        .with_detail(detail);
    }
    if missing.is_empty() {
        Check::new(
            "sources",
            "Transcript sources",
            Level::Ok,
            format!(
                "{} transcript {} present",
                detail.len(),
                plural(detail.len(), "tree", "trees")
            ),
        )
        .with_detail(detail)
    } else {
        // One tool not installed is the ordinary case, not a defect.
        Check::new(
            "sources",
            "Transcript sources",
            Level::Note,
            format!("no data for {}", missing.join(" and ")),
        )
        .with_detail(detail)
    }
}

/// How much was found, and how much of it was actually read this run.
fn coverage(input: &Input<'_>) -> Check {
    let s = input.stats;
    let mut detail = vec![
        format!("{:<28} {}", "transcripts discovered", fmt::count(s.files_total as u64)),
        format!(
            "{:<28} {} reused · {} appended · {} full",
            "read this run",
            fmt::count(s.files_reused as u64),
            fmt::count(s.files_appended as u64),
            fmt::count(s.files_full as u64),
        ),
        format!("{:<28} {}", "bytes on disk", fmt::bytes(s.bytes_total)),
        format!("{:<28} {}", "billed requests", fmt::count(s.events as u64)),
    ];
    if s.files_without_events > 0 {
        detail.push(format!(
            "{:<28} {}",
            "transcripts with no usage",
            fmt::count(s.files_without_events as u64)
        ));
    }
    if s.skipped_synthetic > 0 {
        detail.push(format!(
            "{:<28} {} (never billed)",
            "synthetic records skipped",
            fmt::count(u64::from(s.skipped_synthetic))
        ));
    }

    if s.files_total == 0 {
        return Check::new(
            "coverage",
            "Transcript coverage",
            Level::Warn,
            "no transcripts were discovered".to_string(),
        )
        .with_detail(detail);
    }
    if s.events == 0 {
        return Check::new(
            "coverage",
            "Transcript coverage",
            Level::Warn,
            format!("{} transcripts held no billed request", fmt::count(s.files_total as u64)),
        )
        .with_detail(detail);
    }
    Check::new(
        "coverage",
        "Transcript coverage",
        Level::Ok,
        format!(
            "{} requests across {} transcripts",
            fmt::count(s.events as u64),
            fmt::count(s.files_total as u64)
        ),
    )
    .with_detail(detail)
}

/// Lines that could not be read. These are tokens readout cannot see.
fn malformed(input: &Input<'_>) -> Check {
    let s = input.stats;
    if s.malformed_lines == 0 {
        return Check::new(
            "malformed",
            "Malformed records",
            Level::Ok,
            "every record parsed".to_string(),
        );
    }
    let mut detail: Vec<String> = s
        .malformed_files
        .iter()
        .map(|(path, count)| {
            format!("{:>6} {}", fmt::count(u64::from(*count)), fmt::terminal_text(path))
        })
        .collect();
    let named: u64 = s.malformed_files.iter().map(|(_, count)| u64::from(*count)).sum();
    if s.malformed_lines > named {
        detail.push(format!(
            "{:>6} in further files (only the worst {} are named)",
            fmt::count(s.malformed_lines - named),
            crate::scan::MAX_REPORTED_MALFORMED_FILES,
        ));
    }
    Check::new(
        "malformed",
        "Malformed records",
        // Usage may be hiding in there, so this is a real gap in the totals.
        Level::Warn,
        format!(
            "{} {} could not be read as JSON",
            fmt::count(s.malformed_lines),
            plural(s.malformed_lines as usize, "line", "lines"),
        ),
    )
    .with_detail(detail)
    .with_remedy("readout refresh  # reparse from scratch after repairing a transcript")
}

/// Responses that appeared in more than one transcript and were billed once.
fn duplicates(input: &Input<'_>) -> Check {
    let s = input.stats;
    let raw = s.events.saturating_add(s.duplicates_dropped);
    if s.duplicates_dropped == 0 {
        return Check::new(
            "duplicates",
            "Duplicate responses",
            Level::Ok,
            "no duplicated responses to collapse".to_string(),
        );
    }
    let share = s.duplicates_dropped as f64 / raw.max(1) as f64;
    Check::new(
        "duplicates",
        "Duplicate responses",
        // Expected, not a defect: Claude copies history into forked
        // transcripts and Codex re-emits cumulative snapshots. Dropping them
        // is what keeps a copied session from being billed twice.
        Level::Ok,
        format!(
            "{} of {} records ({}) were copies, counted once",
            fmt::count(s.duplicates_dropped as u64),
            fmt::count(raw as u64),
            fmt::share(share),
        ),
    )
}

/// Events that cannot be placed on the calendar.
fn timestamps(input: &Input<'_>) -> Check {
    let undated = input.events.iter().filter(|e| e.ts == 0).count();
    if undated == 0 {
        return Check::new(
            "timestamps",
            "Event timestamps",
            Level::Ok,
            "every request carries a usable timestamp".to_string(),
        );
    }
    let share = undated as f64 / input.events.len().max(1) as f64;
    Check::new(
        "timestamps",
        "Event timestamps",
        Level::Warn,
        format!(
            "{} {} ({}) carry no timestamp and are excluded from every dated view",
            fmt::count(undated as u64),
            plural(undated, "request", "requests"),
            fmt::share(share),
        ),
    )
    .with_detail(vec![match input.filter.since {
        Some(since) => {
            format!("They count toward all-time totals; this window (from {since}) excludes them.")
        }
        None => "They count toward the all-time totals; any -d window drops them.".to_string(),
    }])
}

/// Spend readout cannot estimate because it has no rate.
fn unpriced(input: &Input<'_>) -> Check {
    let coverage = input.summary.total.priced.coverage();
    let excluded = 1.0 - coverage;
    let models = &input.summary.unpriced_models;
    if models.is_empty() {
        let known = input.pricing.known_models().len();
        return Check::new(
            "pricing",
            "Model pricing",
            Level::Ok,
            format!("every model in this window has a rate ({known} on file)"),
        );
    }
    let detail: Vec<String> = models
        .iter()
        .take(20)
        .map(|model| format!("       {}", fmt::terminal_text(model)))
        .chain((models.len() > 20).then(|| format!("       … and {} more", models.len() - 20)))
        .collect();
    // Unpriced is not free: the tokens are counted, the cost is excluded, and
    // how much that hides decides how loudly this is said.
    let level = if excluded >= UNPRICED_SERIOUS { Level::Warn } else { Level::Note };
    Check::new(
        "pricing",
        "Model pricing",
        level,
        format!(
            "{} {} no rate — {} of tokens are excluded from cost",
            fmt::count(models.len() as u64),
            plural(models.len(), "model has", "models have"),
            fmt::share(excluded),
        ),
    )
    .with_detail(detail)
    .with_remedy("readout pricing --init  # write a starter override file")
}

/// Whether the incremental cache is present and usable.
fn cache_health(input: &Input<'_>) -> Check {
    let Some(path) = input.cache_path.as_ref() else {
        return Check::new(
            "cache",
            "Incremental cache",
            Level::Warn,
            "no cache location is available; every run will be a cold scan".to_string(),
        );
    };
    let mut detail = vec![
        format!("{:<28} {}", "path", fmt::terminal_text(&path.display().to_string())),
        format!("{:<28} {}", "schema version", crate::cache::SCHEMA_VERSION),
    ];
    let Ok(meta) = std::fs::metadata(path) else {
        return Check::new(
            "cache",
            "Incremental cache",
            // Absent is the normal state before the first successful scan.
            Level::Note,
            "no cache written yet; the next scan will build one".to_string(),
        )
        .with_detail(detail);
    };
    detail.push(format!("{:<28} {}", "size", fmt::bytes(meta.len())));
    if input.stats.files_forgotten > 0 {
        detail.push(format!(
            "{:<28} {}",
            "entries dropped this run",
            fmt::count(input.stats.files_forgotten as u64)
        ));
    }

    // A cache that holds nothing while transcripts exist means every launch
    // pays for a cold scan — the one failure mode that is invisible except as
    // slowness.
    let reused = input.stats.files_reused;
    if input.stats.files_total > 0
        && reused == 0
        && input.stats.files_full == input.stats.files_total
    {
        return Check::new(
            "cache",
            "Incremental cache",
            Level::Note,
            "every transcript was parsed from scratch this run".to_string(),
        )
        .with_detail(detail)
        .with_remedy("run readout again; a warm scan should reuse most files");
    }
    Check::new(
        "cache",
        "Incremental cache",
        Level::Ok,
        format!(
            "{} of {} transcripts reused from cache",
            fmt::count(reused as u64),
            fmt::count(input.stats.files_total as u64)
        ),
    )
    .with_detail(detail)
}

/// Whether synced devices are current and readable.
fn device_freshness(input: &Input<'_>) -> Check {
    let remote: Vec<&DeviceRecord> = input.devices.iter().filter(|d| !d.is_local).collect();
    if remote.is_empty() && input.device_warnings.is_empty() {
        return Check::new(
            "devices",
            "Device sync",
            Level::Note,
            "no SSH devices are configured; totals are this machine only".to_string(),
        );
    }
    let now = chrono::Local::now().timestamp();
    let mut detail = Vec::new();
    let mut broken = 0usize;
    let mut stale = 0usize;
    for device in &remote {
        let host = device.host.as_deref().unwrap_or("-");
        let state = if let Some(problem) = device.problem.as_deref() {
            broken += 1;
            format!("unreadable — {}", fmt::terminal_text(problem))
        } else if !device.enabled {
            "not enabled".to_string()
        } else if device.generated_at <= 0 {
            stale += 1;
            "never synced".to_string()
        } else if now - device.generated_at > STALE_SNAPSHOT_SECS {
            stale += 1;
            format!("stale — synced {}", fmt::relative(device.generated_at))
        } else {
            format!("synced {}", fmt::relative(device.generated_at))
        };
        detail.push(format!("{:<24} {state}", fmt::terminal_ellipsize(host, 24)));
    }
    // `load_usage` reports each bad snapshot twice — once as a warning and
    // once as a device carrying the same `problem` string. The device records
    // are the authoritative list, so warnings only contribute a line for a
    // host that produced no record at all.
    let described: Vec<&str> = remote.iter().filter_map(|d| d.host.as_deref()).collect();
    for warning in input.device_warnings {
        let host = warning.split(':').next().unwrap_or_default();
        if !described.contains(&host) {
            broken += 1;
            detail.push(format!("skipped {}", fmt::terminal_text(warning)));
        }
    }
    // A snapshot that will not load silently removes a machine from the
    // totals, which is a wrong number rather than an old one.
    if broken > 0 {
        Check::new(
            "devices",
            "Device sync",
            Level::Fail,
            format!(
                "{} device {} could not be read; that usage is missing from the totals",
                fmt::count(broken as u64),
                plural(broken, "snapshot", "snapshots"),
            ),
        )
        .with_detail(detail)
        .with_remedy("readout sync  # rebuild the snapshots")
    } else if stale > 0 {
        Check::new(
            "devices",
            "Device sync",
            Level::Warn,
            format!(
                "{} device {} out of date",
                fmt::count(stale as u64),
                plural(stale, "snapshot is", "snapshots are")
            ),
        )
        .with_detail(detail)
        .with_remedy("readout sync")
    } else {
        Check::new(
            "devices",
            "Device sync",
            Level::Ok,
            format!(
                "{} device {} current",
                fmt::count(remote.len() as u64),
                plural(remote.len(), "snapshot is", "snapshots are")
            ),
        )
        .with_detail(detail)
    }
}

fn plural(n: usize, one: &'static str, many: &'static str) -> &'static str {
    if n == 1 { one } else { many }
}

pub fn text(report: &Report, filter: &Filter) -> String {
    let mut o = String::new();
    // Two scopes are in play and conflating them would misread every figure:
    // what is on disk is a property of the corpus, while which models want a
    // rate is a property of the window being asked about.
    let _ = match filter.since {
        Some(since) => writeln!(
            o,
            "readout doctor — corpus health; pricing judged over the window from {since}\n"
        ),
        None => writeln!(o, "readout doctor — corpus health, all time\n"),
    };
    for check in &report.checks {
        let _ = writeln!(
            o,
            "  {} {:<5} {:<22} {}",
            check.level.glyph(),
            check.level.label(),
            check.title,
            check.summary
        );
        for line in &check.detail {
            let _ = writeln!(o, "          {line}");
        }
        if let Some(remedy) = &check.remedy {
            let _ = writeln!(o, "          → {remedy}");
        }
        let _ = writeln!(o);
    }
    let counts = |level: Level| report.checks.iter().filter(|c| c.level == level).count();
    let _ = writeln!(
        o,
        "  {} ok · {} note · {} warn · {} fail",
        counts(Level::Ok),
        counts(Level::Note),
        counts(Level::Warn),
        counts(Level::Fail),
    );
    if report.failed() {
        let _ = writeln!(
            o,
            "\n  Something readout reports is wrong or missing. Exit code {}.",
            crate::FINDINGS_EXIT_CODE
        );
    }
    o
}

pub fn json(report: &Report, filter: &Filter) -> String {
    let v = json!({
        "generated_ts": chrono::Local::now().timestamp(),
        "since": filter.since.map(|d| d.to_string()),
        "until": filter.until.map(|d| d.to_string()),
        "worst_level": report.worst().label(),
        "failed": report.failed(),
        "checks": report.checks.iter().map(|c| json!({
            "id": c.id,
            "title": c.title,
            "level": c.level.label(),
            "summary": c.summary,
            "detail": c.detail,
            "remedy": c.remedy,
        })).collect::<Vec<_>>(),
    });
    serde_json::to_string_pretty(&v).unwrap_or_else(|_| "{}".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agg::summarize;
    use crate::model::Tokens;

    fn event(model: &str, ts: i64) -> UsageEvent {
        UsageEvent {
            source: Source::Claude,
            ts,
            model: model.into(),
            session: "s1".into(),
            project: "alpha".into(),
            tokens: Tokens { input: 10, output: 20, ..Default::default() },
            observed_on: Vec::new(),
            dedup_key: None,
            dedup_rank: 0,
        }
    }

    struct Fixture {
        events: Vec<UsageEvent>,
        summary: Summary,
        stats: ScanStats,
        devices: Vec<DeviceRecord>,
        warnings: Vec<String>,
        pricing: Pricing,
        filter: Filter,
    }

    impl Fixture {
        fn new(events: Vec<UsageEvent>, stats: ScanStats) -> Fixture {
            let pricing = Pricing::builtin();
            let filter = Filter::default();
            let summary = summarize(&events, &filter, &pricing);
            Fixture {
                events,
                summary,
                stats,
                devices: Vec::new(),
                warnings: Vec::new(),
                pricing,
                filter,
            }
        }

        fn report(&self) -> Report {
            run(&Input {
                events: &self.events,
                summary: &self.summary,
                stats: &self.stats,
                devices: &self.devices,
                device_warnings: &self.warnings,
                pricing: &self.pricing,
                filter: &self.filter,
                sources: &Source::ALL,
                cache_path: None,
            })
        }
    }

    fn find<'a>(report: &'a Report, id: &str) -> &'a Check {
        report.checks.iter().find(|c| c.id == id).expect("every check is always reported")
    }

    fn healthy_stats() -> ScanStats {
        ScanStats { files_total: 4, files_reused: 4, events: 1, ..Default::default() }
    }

    #[test]
    fn a_clean_corpus_reports_nothing_to_fix_and_exits_zero() {
        let now = chrono::Local::now().timestamp();
        let f = Fixture::new(vec![event("claude-opus-5", now)], healthy_stats());
        let report = f.report();
        assert!(!report.failed());
        assert_eq!(find(&report, "malformed").level, Level::Ok);
        assert_eq!(find(&report, "pricing").level, Level::Ok);
        assert_eq!(find(&report, "timestamps").level, Level::Ok);
    }

    #[test]
    fn malformed_lines_are_named_with_the_files_holding_them() {
        let now = chrono::Local::now().timestamp();
        let stats = ScanStats {
            malformed_lines: 12,
            malformed_files: vec![("/t/a.jsonl".into(), 7), ("/t/b.jsonl".into(), 5)],
            ..healthy_stats()
        };
        let f = Fixture::new(vec![event("claude-opus-5", now)], stats);
        let check = find(&f.report(), "malformed").clone();
        assert_eq!(check.level, Level::Warn);
        assert!(check.summary.contains("12"));
        assert!(check.detail.iter().any(|d| d.contains("/t/a.jsonl")));
        assert!(check.remedy.is_some(), "a finding the reader can act on names the command");
    }

    #[test]
    fn a_capped_file_list_accounts_for_the_lines_it_did_not_name() {
        // The count is exact and the list is a sample; the difference has to
        // be stated or the two figures look like they disagree.
        let now = chrono::Local::now().timestamp();
        let stats = ScanStats {
            malformed_lines: 100,
            malformed_files: vec![("/t/a.jsonl".into(), 7)],
            ..healthy_stats()
        };
        let f = Fixture::new(vec![event("claude-opus-5", now)], stats);
        let check = find(&f.report(), "malformed").clone();
        assert!(
            check.detail.iter().any(|d| d.contains("93") && d.contains("further files")),
            "unnamed damage must still be accounted for: {:?}",
            check.detail
        );
    }

    #[test]
    fn an_unpriced_model_is_reported_without_being_called_free() {
        let now = chrono::Local::now().timestamp();
        let f = Fixture::new(
            vec![event("claude-opus-5", now), event("codex-auto-review", now)],
            healthy_stats(),
        );
        let check = find(&f.report(), "pricing").clone();
        assert!(check.level >= Level::Note);
        assert!(check.detail.iter().any(|d| d.contains("codex-auto-review")));
        assert!(check.summary.contains("excluded from cost"));
    }

    #[test]
    fn undated_requests_are_flagged_because_every_window_drops_them() {
        let now = chrono::Local::now().timestamp();
        let f = Fixture::new(
            vec![event("claude-opus-5", now), event("claude-opus-5", 0)],
            healthy_stats(),
        );
        let check = find(&f.report(), "timestamps").clone();
        assert_eq!(check.level, Level::Warn);
        assert!(check.summary.contains('1'));
    }

    #[test]
    fn an_unreadable_device_snapshot_fails_because_the_total_is_now_wrong() {
        // A stale snapshot is an old number; one that will not load removes a
        // machine from the totals without saying so. Only the second is a
        // failure, and only failures gate a script.
        let now = chrono::Local::now().timestamp();
        let mut f = Fixture::new(vec![event("claude-opus-5", now)], healthy_stats());
        f.devices = vec![DeviceRecord {
            id: "dev-remote".into(),
            name: "workstation".into(),
            host: Some("workstation".into()),
            exporter_version: None,
            generated_at: now,
            is_local: false,
            available: false,
            enabled: true,
            discovered: true,
            problem: Some("unsupported usage bundle schema".into()),
        }];
        // `load_usage` reports the same bad snapshot twice — as a device
        // carrying a `problem` and as a warning. Counting both turned one
        // broken machine into "2 device snapshots".
        f.warnings = vec!["workstation: unsupported usage bundle schema".to_string()];
        let report = f.report();
        let check = find(&report, "devices").clone();
        assert_eq!(check.level, Level::Fail);
        assert!(check.summary.starts_with("1 device snapshot"), "counted twice: {}", check.summary);
        assert_eq!(check.detail.len(), 1, "the warning repeats the device row: {:?}", check.detail);
        assert!(report.failed(), "a wrong total is what a nonzero exit code is for");

        // The same device, merely out of date, is a warning and exits zero.
        f.warnings.clear();
        f.devices[0].problem = None;
        f.devices[0].generated_at = now - STALE_SNAPSHOT_SECS - 1;
        let report = f.report();
        assert_eq!(find(&report, "devices").level, Level::Warn);
        assert!(!report.failed());
    }

    #[test]
    fn duplicate_collapsing_is_reported_as_working_rather_than_as_damage() {
        // Both tools write more records than they bill for. Dropping the
        // copies is the correct behaviour, so it must not read as a defect.
        let now = chrono::Local::now().timestamp();
        let stats = ScanStats { duplicates_dropped: 40, events: 60, ..healthy_stats() };
        let f = Fixture::new(vec![event("claude-opus-5", now)], stats);
        let check = find(&f.report(), "duplicates").clone();
        assert_eq!(check.level, Level::Ok);
        assert!(check.summary.contains("counted once"));
    }

    #[test]
    fn the_json_report_carries_every_check_and_the_exit_verdict() {
        let now = chrono::Local::now().timestamp();
        let f = Fixture::new(vec![event("claude-opus-5", now)], healthy_stats());
        let report = f.report();
        let v: serde_json::Value = serde_json::from_str(&json(&report, &f.filter)).unwrap();
        assert_eq!(v["checks"].as_array().unwrap().len(), report.checks.len());
        assert_eq!(v["failed"], false);
        assert!(v["checks"][0]["id"].is_string());
    }

    #[test]
    fn the_text_report_shows_a_glyph_beside_every_level() {
        let now = chrono::Local::now().timestamp();
        let f = Fixture::new(vec![event("claude-opus-5", now)], healthy_stats());
        let out = text(&f.report(), &f.filter);
        assert!(out.starts_with("readout doctor"));
        // Severity never rests on colour alone.
        assert!(out.contains(Level::Ok.glyph()));
        assert!(out.contains("ok · "));
    }
}
