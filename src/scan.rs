//! Discovery and parallel scanning.
//!
//! Everything here is read-only: transcripts are opened, read, and closed.
//! The only file this crate writes is its own cache.

use crate::cache::{self, Cache, FileEntry, FileId, Plan};
use crate::model::{Source, UsageEvent};
use crate::parse;
use anyhow::{Context, Result};
use rayon::prelude::*;
use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

/// 单个 transcript 和单行都必须有硬边界；流式读取只保留当前行。
const MAX_TRANSCRIPT_BYTES: u64 = 16 * 1024 * 1024 * 1024;
const MAX_JSONL_LINE_BYTES: usize = 64 * 1024 * 1024;
const MAX_SCAN_THREADS: usize = 4;

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
        if !entry_is_real_dir(&project) {
            continue;
        }
        // <project>/*.jsonl — main sessions
        push_jsonl(&pdir, Source::Claude, &mut out);

        let Ok(sessions) = std::fs::read_dir(&pdir) else { continue };
        for session in sessions.flatten() {
            let sdir = session.path();
            if !entry_is_real_dir(&session) {
                continue;
            }
            let subagents = sdir.join("subagents");
            if !path_is_real_dir(&subagents) {
                continue;
            }
            // <session>/subagents/*.jsonl — Task/Agent subagents
            push_jsonl(&subagents, Source::Claude, &mut out);

            // <session>/subagents/workflows/wf_*/*.jsonl — workflow subagents.
            // Skipping this level makes every token spent inside a Workflow
            // vanish from the totals.
            let workflows = subagents.join("workflows");
            if !path_is_real_dir(&workflows) {
                continue;
            }
            let Ok(wfs) = std::fs::read_dir(&workflows) else { continue };
            for wf in wfs.flatten() {
                let wdir = wf.path();
                if entry_is_real_dir(&wf) {
                    push_jsonl(&wdir, Source::Claude, &mut out);
                }
            }
        }
    }
    out
}

fn entry_is_real_dir(entry: &std::fs::DirEntry) -> bool {
    entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false)
}

fn path_is_real_dir(path: &Path) -> bool {
    std::fs::symlink_metadata(path).map(|meta| meta.file_type().is_dir()).unwrap_or(false)
}

/// Codex buckets active rollouts as `YYYY/MM/DD/*.jsonl`. Archived sessions are
/// intentionally outside readout's scope and are not discovered.
pub fn discover_codex(root: &Path) -> Vec<Target> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        if dir.file_name().and_then(|name| name.to_str()) == Some("archived_sessions") {
            continue;
        }
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
    let results: Vec<Result<(String, FileEntry, ParseOutcome)>> = scan_pool().install(|| {
        targets
            .par_iter()
            .map(|t| {
                let out = scan_one(t, cache_ref);
                let n = done.fetch_add(1, Ordering::Relaxed).saturating_add(1);
                if let Ok((_, _, ref outcome)) = out {
                    let _ =
                        bytes_read.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
                            Some(value.saturating_add(outcome.bytes_read))
                        });
                }
                if let Some(cb) = on_progress {
                    // 小语料逐文件报告进度，大语料降低回调频率。
                    if total < 200 || n.is_multiple_of(16) || n == total {
                        cb(Progress {
                            done: n,
                            total,
                            bytes_read: bytes_read.load(Ordering::Relaxed),
                        });
                    }
                }
                out
            })
            .collect()
    });
    let parse_ms = parse_start.elapsed().as_millis();

    let mut stats = ScanStats {
        files_total: total,
        bytes_read: bytes_read.load(Ordering::Relaxed),
        discover_ms,
        parse_ms,
        ..Default::default()
    };

    // Do not partially mutate the cache when one worker failed. The caller can
    // keep showing the previous complete result and retry the whole scan.
    let results: Vec<_> = results.into_iter().collect::<Result<_>>()?;
    let mut seen: HashSet<String> = HashSet::with_capacity(total);
    let mut all: Vec<UsageEvent> = Vec::new();
    let mut all_text_bytes = 0usize;
    for item in results {
        let (key, entry, outcome) = item;
        anyhow::ensure!(
            entry.events.len() <= cache::MAX_EVENTS_PER_FILE,
            "transcript produced more than {} usage events: {}",
            cache::MAX_EVENTS_PER_FILE,
            key
        );
        anyhow::ensure!(
            all.len().saturating_add(entry.events.len()) <= cache::MAX_EVENTS_TOTAL,
            "scan produced more than {} usage events",
            cache::MAX_EVENTS_TOTAL
        );
        let file_text_bytes = cache::event_text_bytes(&entry.events);
        anyhow::ensure!(
            file_text_bytes <= cache::MAX_EVENT_TEXT_BYTES_PER_FILE,
            "transcript event text exceeds the {} byte safety limit: {}",
            cache::MAX_EVENT_TEXT_BYTES_PER_FILE,
            key
        );
        all_text_bytes = all_text_bytes.saturating_add(file_text_bytes);
        anyhow::ensure!(
            all_text_bytes <= cache::MAX_EVENT_TEXT_BYTES_TOTAL,
            "scan event text exceeds the {} byte safety limit",
            cache::MAX_EVENT_TEXT_BYTES_TOTAL
        );
        match outcome.plan {
            Plan::Unchanged => stats.files_reused += 1,
            Plan::Append { .. } => stats.files_appended += 1,
            Plan::Full => stats.files_full += 1,
        }
        stats.bytes_total = stats.bytes_total.saturating_add(entry.size);
        stats.skipped_synthetic = stats.skipped_synthetic.saturating_add(entry.skipped_synthetic);
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

fn scan_pool() -> &'static rayon::ThreadPool {
    static POOL: OnceLock<rayon::ThreadPool> = OnceLock::new();
    POOL.get_or_init(|| {
        let available = std::thread::available_parallelism().map_or(1, usize::from);
        rayon::ThreadPoolBuilder::new()
            .num_threads(available.min(MAX_SCAN_THREADS))
            .build()
            .expect("building the bounded transcript scan pool")
    })
}

struct ParseOutcome {
    plan: Plan,
    bytes_read: u64,
}

struct StreamParse {
    events: Vec<UsageEvent>,
    cursor: parse::ParseCursor,
    skipped_synthetic: u32,
    offset: u64,
    bytes_read: u64,
}

fn scan_one(t: &Target, cache: &Cache) -> Result<(String, FileEntry, ParseOutcome)> {
    let key = cache::key(&t.path);
    let meta = std::fs::metadata(&t.path)
        .with_context(|| format!("reading metadata for {}", t.path.display()))?;
    anyhow::ensure!(meta.is_file(), "transcript is no longer a file: {}", t.path.display());
    anyhow::ensure!(
        meta.len() <= MAX_TRANSCRIPT_BYTES,
        "transcript exceeds the {} byte safety limit: {}",
        MAX_TRANSCRIPT_BYTES,
        t.path.display()
    );
    let existing = cache.files.get(&key);
    let plan = cache::plan(existing, &meta);

    match &plan {
        Plan::Unchanged => {
            let e = existing.context("unchanged scan plan had no cache entry")?.clone();
            Ok((key, e, ParseOutcome { plan, bytes_read: 0 }))
        }
        Plan::Append { from_offset, cursor } => {
            let parsed = parse_stream(t, cursor, *from_offset)?;
            let prev = existing.context("append scan plan had no cache entry")?;
            let mut events = prev.events.clone();
            events.extend(parsed.events);
            let events = dedup(events);
            let entry = FileEntry {
                id: FileId::of(&meta),
                offset: parsed.offset,
                size: meta.len(),
                mtime_ns: cache::mtime_ns(&meta),
                cursor: parsed.cursor,
                events,
                skipped_synthetic: prev.skipped_synthetic.saturating_add(parsed.skipped_synthetic),
            };
            Ok((key, entry, ParseOutcome { plan, bytes_read: parsed.bytes_read }))
        }
        Plan::Full => {
            let parsed = parse_stream(t, &parse::ParseCursor::default(), 0)?;
            let entry = FileEntry {
                id: FileId::of(&meta),
                offset: parsed.offset,
                size: meta.len(),
                mtime_ns: cache::mtime_ns(&meta),
                cursor: parsed.cursor,
                events: dedup(parsed.events),
                skipped_synthetic: parsed.skipped_synthetic,
            };
            Ok((key, entry, ParseOutcome { plan, bytes_read: parsed.bytes_read }))
        }
    }
}

fn parse_stream(t: &Target, cursor: &parse::ParseCursor, from_offset: u64) -> Result<StreamParse> {
    anyhow::ensure!(from_offset <= MAX_TRANSCRIPT_BYTES, "cached transcript offset is too large");
    let mut file = std::fs::File::open(&t.path)
        .with_context(|| format!("opening transcript {}", t.path.display()))?;
    file.seek(SeekFrom::Start(from_offset))
        .with_context(|| format!("seeking transcript {}", t.path.display()))?;
    let mut reader = BufReader::new(file);
    let mut line = Vec::new();
    let mut events = Vec::new();
    let mut event_text_bytes = 0usize;
    let mut next_cursor = cursor.clone();
    let mut skipped_synthetic = 0u32;
    let mut offset = from_offset;
    let mut bytes_read = 0u64;

    loop {
        line.clear();
        let mut limited = (&mut reader).take(MAX_JSONL_LINE_BYTES as u64 + 1);
        let read = limited
            .read_until(b'\n', &mut line)
            .with_context(|| format!("reading transcript {}", t.path.display()))?;
        if read == 0 {
            break;
        }
        bytes_read = bytes_read.saturating_add(read as u64);
        anyhow::ensure!(
            from_offset.saturating_add(bytes_read) <= MAX_TRANSCRIPT_BYTES,
            "transcript grew beyond the {} byte safety limit: {}",
            MAX_TRANSCRIPT_BYTES,
            t.path.display()
        );

        if line.last() != Some(&b'\n') {
            anyhow::ensure!(
                line.len() <= MAX_JSONL_LINE_BYTES,
                "JSONL line exceeds the {} byte safety limit: {}",
                MAX_JSONL_LINE_BYTES,
                t.path.display()
            );
            // 正在写入的末行留到下次扫描，避免解析半条 JSON。
            break;
        }

        let parsed = run_parser(t, &next_cursor, &line);
        anyhow::ensure!(
            parsed.consumed == line.len(),
            "parser did not consume a complete JSONL line: {}",
            t.path.display()
        );
        offset =
            offset.checked_add(parsed.consumed as u64).context("transcript offset overflow")?;
        next_cursor = parsed.cursor;
        event_text_bytes = event_text_bytes.saturating_add(cache::event_text_bytes(&parsed.events));
        events.extend(parsed.events);
        anyhow::ensure!(
            events.len() <= cache::MAX_EVENTS_PER_FILE,
            "transcript produced more than {} usage events: {}",
            cache::MAX_EVENTS_PER_FILE,
            t.path.display()
        );
        anyhow::ensure!(
            event_text_bytes <= cache::MAX_EVENT_TEXT_BYTES_PER_FILE,
            "transcript event text exceeds the {} byte safety limit: {}",
            cache::MAX_EVENT_TEXT_BYTES_PER_FILE,
            t.path.display()
        );
        skipped_synthetic = skipped_synthetic.saturating_add(parsed.skipped_synthetic);
    }

    Ok(StreamParse { events, cursor: next_cursor, skipped_synthetic, offset, bytes_read })
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

    #[cfg(unix)]
    #[test]
    fn discovery_does_not_follow_directory_symlinks() {
        use std::os::unix::fs::symlink;

        let base = std::env::temp_dir().join(format!(
            "readout-symlink-scope-{}-{}",
            std::process::id(),
            line!()
        ));
        let claude = base.join("claude");
        let codex = base.join("codex");
        let outside = base.join("outside");
        std::fs::create_dir_all(&claude).unwrap();
        std::fs::create_dir_all(&codex).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("leak.jsonl"), b"{}\n").unwrap();
        symlink(&outside, claude.join("linked-project")).unwrap();
        symlink(&outside, codex.join("linked-year")).unwrap();

        assert!(discover_claude(&claude).is_empty());
        assert!(discover_codex(&codex).is_empty());
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn streaming_scan_leaves_a_torn_tail_for_the_next_append() {
        use std::io::Write;

        let path = std::env::temp_dir().join(format!(
            "readout-stream-{}-{}.jsonl",
            std::process::id(),
            line!()
        ));
        let line1 = claude_record("msg_1", 10);
        let line2 = claude_record("msg_2", 20);
        let split = line2.len() / 2;
        std::fs::write(&path, format!("{line1}\n{}", &line2[..split])).unwrap();

        let target = Target { path: path.clone(), source: Source::Claude };
        let mut cache = Cache::default();
        let first = scan(std::slice::from_ref(&target), &mut cache, None).unwrap();
        assert_eq!(first.events.len(), 1);
        assert_eq!(cache.files[&cache::key(&path)].offset, (line1.len() + 1) as u64);

        let mut file = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
        writeln!(file, "{}", &line2[split..]).unwrap();
        let second = scan(&[target], &mut cache, None).unwrap();
        assert_eq!(second.events.len(), 2);
        assert_eq!(second.stats.files_appended, 1);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn streamed_claude_records_are_deduplicated_inside_the_cache() {
        let path = std::env::temp_dir().join(format!(
            "readout-stream-dedup-{}-{}.jsonl",
            std::process::id(),
            line!()
        ));
        let partial = claude_record("same", 50);
        let finished = claude_record("same", 5)
            .replace(r#""stop_reason":null"#, r#""stop_reason":"end_turn""#);
        std::fs::write(&path, format!("{partial}\n{finished}\n")).unwrap();
        let target = Target { path: path.clone(), source: Source::Claude };
        let mut cache = Cache::default();
        scan(&[target], &mut cache, None).unwrap();
        let events = &cache.files[&cache::key(&path)].events;
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].tokens.output, 5);
        let _ = std::fs::remove_file(path);
    }

    fn claude_record(id: &str, output: u64) -> String {
        format!(
            r#"{{"type":"assistant","timestamp":"2026-08-15T00:00:00Z","sessionId":"s","cwd":"/tmp/p","message":{{"id":"{id}","model":"claude-opus-5","stop_reason":null,"usage":{{"input_tokens":1,"output_tokens":{output}}}}}}}"#
        )
    }

    #[test]
    fn a_transcript_that_disappears_after_discovery_fails_the_scan() {
        let target = Target {
            path: std::env::temp_dir().join(format!(
                "readout-missing-{}-{}.jsonl",
                std::process::id(),
                line!()
            )),
            source: Source::Codex,
        };
        let mut cache = Cache::default();
        let err = scan(&[target], &mut cache, None).err().expect("missing file must be reported");
        assert!(err.to_string().contains("metadata"));
    }

    #[test]
    fn codex_discovery_does_not_cross_into_archived_sessions() {
        let root = std::env::temp_dir().join(format!(
            "readout-codex-scope-{}-{}",
            std::process::id(),
            line!()
        ));
        let sessions = root.join("sessions").join("2026").join("08").join("15");
        let archived = root.join("archived_sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        std::fs::create_dir_all(&archived).unwrap();
        std::fs::write(sessions.join("live.jsonl"), b"").unwrap();
        std::fs::write(archived.join("old.jsonl"), b"").unwrap();

        // Deliberately pass the broader parent rather than `sessions`: the
        // discovery layer itself must enforce the archive exclusion too.
        let found = discover_codex(&root);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].path.file_name().and_then(|s| s.to_str()), Some("live.jsonl"));
        let _ = std::fs::remove_dir_all(root);
    }
}
