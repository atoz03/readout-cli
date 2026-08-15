//! Discovery and parallel scanning.
//!
//! Everything here is read-only: transcripts are opened, read, and closed.
//! The only file this crate writes is its own cache.

use crate::cache::{self, Cache, FileEntry, FileId, Plan};
use crate::model::{Source, UsageEvent};
use crate::parse;
use anyhow::Result;
use rayon::prelude::*;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

/// A transcript file to consider.
#[derive(Debug, Clone)]
pub struct Target {
    pub path: PathBuf,
    pub source: Source,
}

/// Progress callback payload, for the TUI's loading state.
#[derive(Debug, Clone, Copy)]
pub struct Progress {
    pub done: usize,
    pub total: usize,
    pub bytes_read: u64,
}

#[derive(Debug, Default, Clone)]
pub struct ScanStats {
    pub files_total: usize,
    pub files_reused: usize,
    pub files_appended: usize,
    pub files_full: usize,
    pub bytes_read: u64,
    pub bytes_total: u64,
    pub events: usize,
    pub duplicates_dropped: usize,
    pub skipped_synthetic: u32,
    /// Cache entries dropped because their file is gone.
    pub files_forgotten: usize,
    pub discover_ms: u128,
    pub parse_ms: u128,
    pub total_ms: u128,
}

pub struct ScanResult {
    pub events: Vec<UsageEvent>,
    pub stats: ScanStats,
}

/// Find Claude transcripts at the three depths that carry assistant records.
///
/// Recursing blindly would also pull in unrelated JSON under project
/// directories; the shapes below are the ones Claude Code actually writes.
pub fn discover_claude(root: &Path) -> Vec<Target> {
    let mut out = Vec::new();
    let Ok(projects) = std::fs::read_dir(root) else { return out };
    for project in projects.flatten() {
        let pdir = project.path();
        if !pdir.is_dir() {
            continue;
        }
        // <project>/*.jsonl — main sessions
        push_jsonl(&pdir, Source::Claude, &mut out);

        let Ok(sessions) = std::fs::read_dir(&pdir) else { continue };
        for session in sessions.flatten() {
            let sdir = session.path();
            if !sdir.is_dir() {
                continue;
            }
            let subagents = sdir.join("subagents");
            if !subagents.is_dir() {
                continue;
            }
            // <session>/subagents/*.jsonl — Task/Agent subagents
            push_jsonl(&subagents, Source::Claude, &mut out);

            // <session>/subagents/workflows/wf_*/*.jsonl — workflow subagents.
            // Skipping this level makes every token spent inside a Workflow
            // vanish from the totals.
            let workflows = subagents.join("workflows");
            let Ok(wfs) = std::fs::read_dir(&workflows) else { continue };
            for wf in wfs.flatten() {
                let wdir = wf.path();
                if wdir.is_dir() {
                    push_jsonl(&wdir, Source::Claude, &mut out);
                }
            }
        }
    }
    out
}

/// Codex buckets rollouts as `YYYY/MM/DD/*.jsonl`, and also keeps
/// `archived_sessions/`. Walk the tree rather than hardcode the depth.
pub fn discover_codex(root: &Path) -> Vec<Target> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        for e in rd.flatten() {
            let p = e.path();
            match e.file_type() {
                Ok(ft) if ft.is_dir() => stack.push(p),
                Ok(ft)
                    if ft.is_file() && p.extension().and_then(|s| s.to_str()) == Some("jsonl") =>
                {
                    out.push(Target { path: p, source: Source::Codex });
                }
                _ => {}
            }
        }
    }
    out
}

fn push_jsonl(dir: &Path, source: Source, out: &mut Vec<Target>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        if p.extension().and_then(|s| s.to_str()) == Some("jsonl")
            && e.file_type().map(|t| t.is_file()).unwrap_or(false)
        {
            out.push(Target { path: p, source });
        }
    }
}

/// All transcripts from whichever tools are installed.
pub fn discover_all(sources: &[Source]) -> Vec<Target> {
    let mut out = Vec::new();
    if sources.contains(&Source::Claude)
        && let Some(dir) = crate::paths::claude_projects_dir()
    {
        out.extend(discover_claude(&dir));
    }
    if sources.contains(&Source::Codex)
        && let Some(dir) = crate::paths::codex_sessions_dir()
    {
        out.extend(discover_codex(&dir));
    }
    // Deterministic order keeps dedup tie-breaks and output stable run to run.
    out.sort_by(|a, b| a.path.cmp(&b.path));
    out
}

/// Scan every target, reusing cached work where the file has not grown.
///
/// `on_progress` is called from worker threads; keep it cheap.
pub fn scan(
    targets: &[Target],
    cache: &mut Cache,
    on_progress: Option<&(dyn Fn(Progress) + Sync)>,
) -> Result<ScanResult> {
    let started = Instant::now();
    let discover_ms = 0;

    let done = AtomicUsize::new(0);
    let bytes_read = std::sync::atomic::AtomicU64::new(0);
    let total = targets.len();
    let cache_ref = &*cache;

    let parse_start = Instant::now();
    let results: Vec<Option<(String, FileEntry, ParseOutcome)>> = targets
        .par_iter()
        .map(|t| {
            let out = scan_one(t, cache_ref);
            let n = done.fetch_add(1, Ordering::Relaxed) + 1;
            if let Some((_, _, ref outcome)) = out {
                bytes_read.fetch_add(outcome.bytes_read, Ordering::Relaxed);
            }
            if let Some(cb) = on_progress {
                // Report every file for small corpora, sparsely for large ones.
                if total < 200 || n.is_multiple_of(16) || n == total {
                    cb(Progress { done: n, total, bytes_read: bytes_read.load(Ordering::Relaxed) });
                }
            }
            out
        })
        .collect();
    let parse_ms = parse_start.elapsed().as_millis();

    let mut stats = ScanStats {
        files_total: total,
        bytes_read: bytes_read.load(Ordering::Relaxed),
        discover_ms,
        parse_ms,
        ..Default::default()
    };

    let mut seen: HashSet<String> = HashSet::with_capacity(total);
    let mut all: Vec<UsageEvent> = Vec::new();
    for item in results.into_iter().flatten() {
        let (key, entry, outcome) = item;
        match outcome.plan {
            Plan::Unchanged => stats.files_reused += 1,
            Plan::Append { .. } => stats.files_appended += 1,
            Plan::Full => stats.files_full += 1,
        }
        stats.bytes_total += entry.size;
        stats.skipped_synthetic += entry.skipped_synthetic;
        all.extend(entry.events.iter().cloned());
        seen.insert(key.clone());
        cache.files.insert(key, entry);
    }
    stats.files_forgotten = cache.retain_existing(&seen);

    let before = all.len();
    let events = dedup(all);
    stats.duplicates_dropped = before - events.len();
    stats.events = events.len();
    stats.total_ms = started.elapsed().as_millis();

    Ok(ScanResult { events, stats })
}

struct ParseOutcome {
    plan: Plan,
    bytes_read: u64,
}

fn scan_one(t: &Target, cache: &Cache) -> Option<(String, FileEntry, ParseOutcome)> {
    let key = cache::key(&t.path);
    let meta = std::fs::metadata(&t.path).ok()?;
    if !meta.is_file() {
        return None;
    }
    let existing = cache.files.get(&key);
    let plan = cache::plan(existing, &meta);

    match &plan {
        Plan::Unchanged => {
            let e = existing?.clone();
            Some((key, e, ParseOutcome { plan, bytes_read: 0 }))
        }
        Plan::Append { from_offset, cursor } => {
            let bytes = cache::read_from(&t.path, *from_offset).ok()?;
            let parsed = run_parser(t, cursor, &bytes);
            let prev = existing?;
            let mut events = prev.events.clone();
            events.extend(parsed.events);
            let entry = FileEntry {
                id: FileId::of(&meta),
                offset: from_offset + parsed.consumed as u64,
                size: meta.len(),
                mtime_ns: cache::mtime_ns(&meta),
                cursor: parsed.cursor,
                events,
                skipped_synthetic: prev.skipped_synthetic + parsed.skipped_synthetic,
            };
            let read = bytes.len() as u64;
            Some((key, entry, ParseOutcome { plan, bytes_read: read }))
        }
        Plan::Full => {
            let bytes = std::fs::read(&t.path).ok()?;
            let parsed = run_parser(t, &parse::ParseCursor::default(), &bytes);
            let entry = FileEntry {
                id: FileId::of(&meta),
                offset: parsed.consumed as u64,
                size: meta.len(),
                mtime_ns: cache::mtime_ns(&meta),
                cursor: parsed.cursor,
                events: parsed.events,
                skipped_synthetic: parsed.skipped_synthetic,
            };
            let read = bytes.len() as u64;
            Some((key, entry, ParseOutcome { plan, bytes_read: read }))
        }
    }
}

fn run_parser(t: &Target, cursor: &parse::ParseCursor, bytes: &[u8]) -> parse::FileParse {
    match t.source {
        Source::Claude => parse::claude::parse_file(&t.path, cursor, bytes),
        Source::Codex => parse::codex::parse_file(&t.path, cursor, bytes),
    }
}

/// Collapse events that describe the same billed request.
///
/// Claude copies prior history verbatim into forked transcripts, so one API
/// response can appear in several files. It was billed once. Within a file the
/// parser already collapsed by `message.id`; this does the same across files,
/// keeping the record with the higher rank (a finished response beats a
/// mid-stream one) and then the larger output count.
///
/// Codex events carry no such key and pass through untouched.
fn dedup(events: Vec<UsageEvent>) -> Vec<UsageEvent> {
    let mut best: HashMap<String, usize> = HashMap::new();
    let mut keep: Vec<bool> = vec![true; events.len()];
    for (i, e) in events.iter().enumerate() {
        let Some(k) = e.dedup_key.as_ref() else { continue };
        match best.get(k) {
            None => {
                best.insert(k.clone(), i);
            }
            Some(&j) => {
                let prev = &events[j];
                let wins = (e.dedup_rank, e.tokens.output) > (prev.dedup_rank, prev.tokens.output);
                if wins {
                    keep[j] = false;
                    best.insert(k.clone(), i);
                } else {
                    keep[i] = false;
                }
            }
        }
    }
    events.into_iter().zip(keep).filter_map(|(e, k)| k.then_some(e)).collect()
}

/// Convenience wrapper used by both the CLI and the TUI's background thread.
pub fn scan_with_cache(
    sources: &[Source],
    use_cache: bool,
    on_progress: Option<&(dyn Fn(Progress) + Sync)>,
) -> Result<ScanResult> {
    let t0 = Instant::now();
    let targets = discover_all(sources);
    let discover_ms = t0.elapsed().as_millis();

    let cache_path = cache::default_path().ok();
    let mut cache = match (&cache_path, use_cache) {
        (Some(p), true) => Cache::load(p),
        _ => Cache::default(),
    };
    let mut result = scan(&targets, &mut cache, on_progress)?;
    result.stats.discover_ms = discover_ms;
    result.stats.total_ms = t0.elapsed().as_millis();
    if let (Some(p), true) = (&cache_path, use_cache)
        && cache_changed(&result.stats)
    {
        // A cache we cannot persist costs speed, never correctness.
        let _ = cache.save(p);
    }
    Ok(result)
}

/// Whether this scan learned anything the cache does not already hold.
///
/// The cache carries the parsed events for every file, so writing it is
/// proportional to the whole corpus rather than to what changed. That was
/// fine when a scan happened once per launch; the watching dashboard scans
/// every few seconds, and rewriting megabytes to record that nothing moved is
/// the one way this tool could become a nuisance on someone's disk.
fn cache_changed(stats: &ScanStats) -> bool {
    stats.files_appended > 0 || stats.files_full > 0 || stats.files_forgotten > 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Tokens;

    fn ev(key: Option<&str>, rank: u8, output: u64) -> UsageEvent {
        UsageEvent {
            source: Source::Claude,
            ts: 0,
            model: "claude-opus-5".into(),
            session: "s".into(),
            project: "p".into(),
            tokens: Tokens { output, ..Default::default() },
            dedup_key: key.map(str::to_string),
            dedup_rank: rank,
        }
    }

    #[test]
    fn the_same_response_in_two_transcripts_is_counted_once() {
        let out = dedup(vec![ev(Some("m1"), 1, 10), ev(Some("m1"), 1, 10)]);
        assert_eq!(out.len(), 1);
    }

    #[test]
    fn a_finished_copy_beats_a_mid_stream_copy() {
        let out = dedup(vec![ev(Some("m1"), 0, 99), ev(Some("m1"), 1, 5)]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].tokens.output, 5, "stop_reason outranks a larger partial count");
    }

    #[test]
    fn equal_rank_keeps_the_larger_output() {
        let out = dedup(vec![ev(Some("m1"), 1, 5), ev(Some("m1"), 1, 50)]);
        assert_eq!(out[0].tokens.output, 50);
        let out = dedup(vec![ev(Some("m1"), 1, 50), ev(Some("m1"), 1, 5)]);
        assert_eq!(out[0].tokens.output, 50);
    }

    #[test]
    fn a_scan_that_learned_nothing_leaves_the_cache_alone() {
        // The cache holds every parsed event, so writing it costs the whole
        // corpus. Watch mode scans every few seconds; rewriting megabytes to
        // record that nothing changed is the one way this becomes a nuisance.
        let reused = ScanStats { files_total: 400, files_reused: 400, ..Default::default() };
        assert!(!cache_changed(&reused));

        assert!(cache_changed(&ScanStats { files_appended: 1, ..reused.clone() }));
        assert!(cache_changed(&ScanStats { files_full: 1, ..reused.clone() }));
        assert!(cache_changed(&ScanStats { files_forgotten: 1, ..reused }));
    }

    #[test]
    fn keyless_events_are_never_merged() {
        let out = dedup(vec![ev(None, 0, 1), ev(None, 0, 1), ev(None, 0, 1)]);
        assert_eq!(out.len(), 3, "Codex events have no id to merge on");
    }

    #[test]
    fn distinct_keys_survive() {
        let out = dedup(vec![ev(Some("a"), 1, 1), ev(Some("b"), 1, 1)]);
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn claude_discovery_reaches_workflow_subagents() {
        let root = std::env::temp_dir().join(format!("readout-disc-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let proj = root.join("-a-b");
        let wf = proj.join("sess-1").join("subagents").join("workflows").join("wf_abc");
        std::fs::create_dir_all(&wf).unwrap();
        std::fs::write(proj.join("main.jsonl"), b"").unwrap();
        std::fs::write(proj.join("sess-1").join("subagents").join("sub.jsonl"), b"").unwrap();
        std::fs::write(wf.join("agent-1.jsonl"), b"").unwrap();
        std::fs::write(wf.join("journal.jsonl"), b"").unwrap();
        // Not a transcript; must be ignored.
        std::fs::write(proj.join("notes.txt"), b"").unwrap();

        let found = discover_claude(&root);
        let mut names: Vec<_> = found
            .iter()
            .map(|t| t.path.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(names, vec!["agent-1.jsonl", "journal.jsonl", "main.jsonl", "sub.jsonl"]);
        let _ = std::fs::remove_dir_all(&root);
    }
}
