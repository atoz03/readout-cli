//! Global search across Claude Code and Codex history.
//!
//! Usage scanning keeps only billing metadata. The words in a transcript are
//! read on demand and never enter the incremental cache, for the same reason
//! Replay works that way: message bodies and tool output are the sensitive
//! part of this corpus, and a tool that promises to read them without keeping
//! them has to actually not keep them. Every search is therefore a fresh pass
//! over the files — a second or two on a gigabyte-scale corpus, which is the
//! price of the promise.
//!
//! A hit is decoded through [`replay::record_texts`], so the line search shows
//! and the line Replay shows for the same record are produced by one piece of
//! code and cannot disagree.

use crate::agg::Summary;
use crate::model::Source;
use crate::replay::{self, RecordText, ReplayKind};
use crate::scan;
use anyhow::{Context, Result};
use chrono::NaiveDate;
use rayon::prelude::*;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader, Read};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Instant;

/// Enough context to recognize the hit, not so much that a result list becomes
/// the transcript it is meant to point at.
const MAX_SNIPPET_CHARS: usize = 160;
/// Lines quoted per session. The match count above them is always the full
/// figure; these are the sample.
pub const MAX_SAMPLES_PER_SESSION: usize = 6;
/// Where a search stops looking. A query matching this much is not a search,
/// and reading the rest would only make the caller wait to be told so.
const MAX_TOTAL_MATCHES: usize = 20_000;
const MAX_MATCHING_SESSIONS: usize = 2_000;
/// Aggregate bytes one search may read, mirroring Replay's own ceiling.
const MAX_SEARCH_READ_BYTES: u64 = 16 * 1024 * 1024 * 1024;
/// Shorter than this matches everything and tells the reader nothing.
pub const MIN_QUERY_CHARS: usize = 2;
const MAX_QUERY_CHARS: usize = 512;
pub const DEFAULT_LIMIT: usize = 20;

/// What to look for, and where.
#[derive(Debug, Clone)]
pub struct Request {
    pub query: String,
    pub sources: Vec<Source>,
    /// Session identities the usage layer already knows, so a result is
    /// labelled with the same project the Sessions page would show.
    pub index: SessionIndex,
    pub project: Option<String>,
    /// Inclusive local-date bounds on the *records*, not on the session: a
    /// window means "what was said then".
    pub since: Option<NaiveDate>,
    pub until: Option<NaiveDate>,
    /// Sessions reported. The count of those found is reported regardless.
    pub limit: usize,
}

/// One matching record.
#[derive(Debug, Clone)]
pub struct Hit {
    /// Unix milliseconds; 0 when the record carried no timestamp.
    pub ts_ms: i64,
    pub kind: ReplayKind,
    /// The role or tool name, exactly as Replay labels the same record.
    pub title: String,
    pub snippet: String,
}

/// Every match inside one session.
#[derive(Debug, Clone)]
pub struct SessionHits {
    pub source: Source,
    pub session: String,
    pub project: String,
    /// Most recent match, unix milliseconds.
    pub last_ts_ms: i64,
    /// Matches found, which may exceed the samples kept.
    pub matches: usize,
    pub samples: Vec<Hit>,
}

#[derive(Debug, Clone, Default)]
pub struct Results {
    pub query: String,
    /// Most recently active session first.
    pub sessions: Vec<SessionHits>,
    /// Sessions that matched, before `limit` was applied.
    pub sessions_matched: usize,
    pub total_matches: usize,
    pub files_searched: usize,
    /// Transcripts that could not be opened or read. Counted rather than
    /// fatal, so one unreadable file leaves a named gap instead of taking the
    /// whole sweep down with it.
    pub files_failed: usize,
    pub files_total: usize,
    pub bytes_read: u64,
    /// A cap was reached, so the figures above are floors rather than totals.
    pub truncated: bool,
    pub elapsed_ms: u128,
}

impl Results {
    pub fn is_empty(&self) -> bool {
        self.sessions.is_empty()
    }
}

/// What the usage layer knows about the sessions on this machine.
///
/// Search resolves a file to a session by proposing candidate ids from the
/// path and accepting only one this index already contains. That inverts
/// `replay::path_matches_session` without being allowed to contradict it: an
/// unrecognized candidate is simply not used, so the worst case is a result
/// labelled by its file name rather than a result labelled wrongly.
#[derive(Debug, Clone, Default)]
pub struct SessionIndex {
    known: HashMap<(Source, String), String>,
}

impl SessionIndex {
    /// Take the identities from a summary, so search and the Sessions page
    /// name the same session the same way by construction.
    pub fn from_summary(summary: &Summary) -> Self {
        let mut known = HashMap::new();
        for bucket in &summary.by_session {
            if bucket.label.is_empty() {
                continue;
            }
            let project = bucket.top_project().unwrap_or("unknown").to_string();
            for source in &bucket.sources {
                known.insert((*source, bucket.label.clone()), project.clone());
            }
        }
        SessionIndex { known }
    }

    /// The session a transcript belongs to, and the project it ran in.
    fn resolve(&self, path: &Path, source: Source) -> Option<(String, String)> {
        for candidate in path_candidates(path) {
            if let Some(project) = self.known.get(&(source, candidate.clone())) {
                return Some((candidate, project.clone()));
            }
        }
        None
    }
}

/// Ids a path could be naming, in the order `path_matches_session` tries them.
///
/// The file stem first (a Claude session file), then directory names (a
/// subagent or workflow transcript under its parent session), then the
/// dash-delimited substrings of the stem, longest first — a Codex rollout is
/// `rollout-<timestamp>-<thread id>`, and the id is what the usage layer holds.
fn path_candidates(path: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or_default();
    if !stem.is_empty() {
        out.push(stem.to_string());
    }
    for component in path.components() {
        if let Some(text) = component.as_os_str().to_str()
            && !text.is_empty()
            && text != stem
        {
            out.push(text.to_string());
        }
    }
    // Offsets of every dash-delimited field boundary in the stem.
    let mut starts = vec![0usize];
    starts.extend(stem.match_indices('-').map(|(at, _)| at + 1));
    let mut ends: Vec<usize> = stem.match_indices('-').map(|(at, _)| at).collect();
    ends.push(stem.len());
    let mut spans = Vec::new();
    for &start in &starts {
        for &end in &ends {
            if end > start && !(start == 0 && end == stem.len()) {
                spans.push((start, end));
            }
        }
    }
    // Longest first, so a full thread id always beats one of its fields.
    spans.sort_by_key(|&(start, end)| std::cmp::Reverse(end - start));
    out.extend(spans.into_iter().map(|(start, end)| stem[start..end].to_string()));
    out
}

/// Run a search over every discovered transcript.
pub fn run(request: &Request) -> Result<Results> {
    let targets = scan::discover_all(&request.sources);
    run_targets(request, targets)
}

fn run_targets(request: &Request, targets: Vec<scan::Target>) -> Result<Results> {
    let started = Instant::now();
    let query = replay::preview(&request.query);
    anyhow::ensure!(
        query.chars().count() >= MIN_QUERY_CHARS,
        "search needs at least {MIN_QUERY_CHARS} characters"
    );
    anyhow::ensure!(
        query.chars().count() <= MAX_QUERY_CHARS,
        "search query is longer than {MAX_QUERY_CHARS} characters"
    );
    let files_total = targets.len();
    let token = prefilter_token(&query);
    let matches = AtomicUsize::new(0);
    let files_searched = AtomicUsize::new(0);
    let files_failed = AtomicUsize::new(0);
    let bytes_read = std::sync::atomic::AtomicU64::new(0);
    let truncated = AtomicBool::new(false);

    let found: Vec<FileHits> = scan::scan_pool().install(|| {
        targets
            .par_iter()
            .map(|target| {
                if matches.load(Ordering::Relaxed) >= MAX_TOTAL_MATCHES
                    || bytes_read.load(Ordering::Relaxed) >= MAX_SEARCH_READ_BYTES
                {
                    truncated.store(true, Ordering::Relaxed);
                    return Vec::new();
                }
                // One unreadable transcript is a gap in the answer, not the end
                // of it: a single root-owned file must not stop a sweep of the
                // whole corpus. The gap is counted and reported rather than
                // swallowed.
                let Ok(hits) = search_file(target, &query, token, &bytes_read) else {
                    files_failed.fetch_add(1, Ordering::Relaxed);
                    return Vec::new();
                };
                files_searched.fetch_add(1, Ordering::Relaxed);
                if !hits.is_empty() {
                    matches.fetch_add(hits.len(), Ordering::Relaxed);
                    return vec![(target.clone(), hits)];
                }
                Vec::new()
            })
            .collect()
    });

    // A cap reached inside the last file leaves no later file to notice it.
    if matches.load(Ordering::Relaxed) >= MAX_TOTAL_MATCHES {
        truncated.store(true, Ordering::Relaxed);
    }
    let per_file: FileHits = found.into_iter().flatten().collect();
    let mut results = group(per_file, request, &truncated);
    results.query = query;
    results.files_total = files_total;
    results.files_searched = files_searched.load(Ordering::Relaxed);
    results.files_failed = files_failed.load(Ordering::Relaxed);
    results.bytes_read = bytes_read.load(Ordering::Relaxed);
    results.elapsed_ms = started.elapsed().as_millis();
    Ok(results)
}

/// What one worker returns: the file it read, and what it found there.
///
/// A vector rather than an option so a worker that stopped early, or found
/// nothing, costs no allocation.
type FileHits = Vec<(scan::Target, Vec<RawHit>)>;

/// A match before it has been attributed to a session.
#[derive(Debug, Clone)]
struct RawHit {
    ts_ms: i64,
    kind: ReplayKind,
    title: String,
    snippet: String,
    dedup_key: Option<String>,
}

fn group(per_file: FileHits, request: &Request, truncated: &AtomicBool) -> Results {
    // Deterministic order: two runs over an unchanged corpus must agree, and
    // the dedup below keeps the first copy of a duplicated record.
    let mut per_file = per_file;
    per_file.sort_by(|a, b| a.0.path.cmp(&b.0.path));

    let mut grouped: HashMap<(Source, String), SessionHits> = HashMap::new();
    let mut order: Vec<(Source, String)> = Vec::new();
    let mut seen: HashMap<(Source, String), HashSet<String>> = HashMap::new();
    let mut truncated = truncated.load(Ordering::Relaxed);
    let mut total_matches = 0usize;

    for (target, hits) in per_file {
        let (session, project) = request
            .index
            .resolve(&target.path, target.source)
            .unwrap_or_else(|| (file_label(&target.path), "—".to_string()));
        if request.project.as_deref().is_some_and(|wanted| wanted != project) {
            continue;
        }
        let key = (target.source, session.clone());
        for hit in hits {
            if !admits_date(request, hit.ts_ms) {
                continue;
            }
            if let Some(dedup) = hit.dedup_key.as_ref()
                && !seen.entry(key.clone()).or_default().insert(dedup.clone())
            {
                continue;
            }
            if !grouped.contains_key(&key) {
                if grouped.len() >= MAX_MATCHING_SESSIONS {
                    truncated = true;
                    continue;
                }
                order.push(key.clone());
                grouped.insert(
                    key.clone(),
                    SessionHits {
                        source: target.source,
                        session: session.clone(),
                        project: project.clone(),
                        last_ts_ms: 0,
                        matches: 0,
                        samples: Vec::new(),
                    },
                );
            }
            let entry = grouped.get_mut(&key).expect("session row was just inserted");
            entry.matches = entry.matches.saturating_add(1);
            entry.last_ts_ms = entry.last_ts_ms.max(hit.ts_ms);
            total_matches = total_matches.saturating_add(1);
            if entry.samples.len() < MAX_SAMPLES_PER_SESSION {
                entry.samples.push(Hit {
                    ts_ms: hit.ts_ms,
                    kind: hit.kind,
                    title: hit.title,
                    snippet: hit.snippet,
                });
            }
        }
    }

    let mut sessions: Vec<SessionHits> =
        order.into_iter().filter_map(|key| grouped.remove(&key)).collect();
    // Most recent match first: the answer to "where did I say that" is almost
    // always the last time it was said.
    sessions.sort_by(|a, b| {
        b.last_ts_ms
            .cmp(&a.last_ts_ms)
            .then_with(|| b.matches.cmp(&a.matches))
            .then_with(|| a.session.cmp(&b.session))
    });
    for session in &mut sessions {
        session.samples.sort_by_key(|hit| hit.ts_ms);
    }
    let sessions_matched = sessions.len();
    if request.limit > 0 {
        sessions.truncate(request.limit);
    }

    Results { sessions, sessions_matched, total_matches, truncated, ..Default::default() }
}

/// A window bounds the records, and a record with no timestamp cannot be
/// placed in one — the same rule `Filter::admits` applies to usage events.
fn admits_date(request: &Request, ts_ms: i64) -> bool {
    if request.since.is_none() && request.until.is_none() {
        return true;
    }
    if ts_ms == 0 {
        return request.since.is_none();
    }
    let Some(date) = crate::agg::local_datetime(ts_ms.div_euclid(1_000)).map(|dt| dt.date_naive())
    else {
        return false;
    };
    !(request.since.is_some_and(|since| date < since)
        || request.until.is_some_and(|until| date > until))
}

/// The label for a transcript whose session the usage layer has never seen —
/// an aborted run, or one whose records carried no billed request.
fn file_label(path: &Path) -> String {
    path.file_stem().and_then(|s| s.to_str()).unwrap_or("unknown").to_string()
}

fn search_file(
    target: &scan::Target,
    query: &str,
    token: &str,
    bytes_read: &std::sync::atomic::AtomicU64,
) -> Result<Vec<RawHit>> {
    let meta = std::fs::symlink_metadata(&target.path)
        .with_context(|| format!("reading metadata for {}", target.path.display()))?;
    if !meta.file_type().is_file() || meta.len() > scan::MAX_TRANSCRIPT_BYTES {
        return Ok(Vec::new());
    }
    let file = std::fs::File::open(&target.path)
        .with_context(|| format!("opening transcript {}", target.path.display()))?;
    let mut reader = BufReader::new(file);
    let mut line = Vec::new();
    let mut call_names: HashMap<String, String> = HashMap::new();
    let mut out = Vec::new();

    loop {
        line.clear();
        let mut limited = (&mut reader).take(scan::MAX_JSONL_LINE_BYTES as u64 + 1);
        let read = limited
            .read_until(b'\n', &mut line)
            .with_context(|| format!("reading transcript {}", target.path.display()))?;
        if read == 0 {
            break;
        }
        bytes_read.fetch_add(read as u64, Ordering::Relaxed);
        // A line past the ceiling, or the torn tail of a file being appended
        // to right now: stop here and let the next search read it whole.
        if line.len() > scan::MAX_JSONL_LINE_BYTES || line.last() != Some(&b'\n') {
            break;
        }
        // The cheap gate. It may skip work; it may never skip an answer, which
        // is why `token` is only the part of the query that survives both JSON
        // escaping and `preview`'s whitespace folding.
        if !token.is_empty() && !contains_ignore_ascii_case(&line, token.as_bytes()) {
            continue;
        }
        let Ok(value) = serde_json::from_slice::<Value>(&line) else { continue };
        for record in replay::record_texts(target.source, &value, &mut call_names) {
            if let Some(hit) = matched(&record, query) {
                out.push(hit);
            }
        }
        if out.len() >= MAX_TOTAL_MATCHES {
            break;
        }
    }
    Ok(out)
}

/// Confirm a candidate line against the decoded record.
///
/// The pre-filter matched raw bytes; this matches what a reader would actually
/// see. A query that only appears in a uuid or some other field Replay never
/// shows is not a hit, because there would be nothing to point the reader at.
fn matched(record: &RecordText, query: &str) -> Option<RawHit> {
    let text = if contains_ignore_ascii_case(record.detail.as_bytes(), query.as_bytes()) {
        &record.detail
    } else if contains_ignore_ascii_case(record.title.as_bytes(), query.as_bytes()) {
        &record.title
    } else {
        return None;
    };
    Some(RawHit {
        ts_ms: record.ts_ms,
        kind: record.kind,
        title: crate::fmt::terminal_ellipsize(&record.title, 32),
        snippet: snippet(text, query),
        dedup_key: record.dedup_key.clone(),
    })
}

/// The longest run of the query guaranteed to appear verbatim in the file.
///
/// Records are JSON on disk and have been through `preview` by the time
/// anything compares against them, so the two representations differ in ways a
/// naive byte scan gets wrong: a quote is written `\"`, a backslash `\\`, and a
/// newline is written `\n` but read back as a space. Whitespace, `"` and `\`
/// are therefore split on rather than searched for. A query made entirely of
/// them leaves nothing, and every line is parsed instead — slower, and still
/// correct, which is the required direction for an accelerator to fail in.
///
/// Non-ASCII is deliberately kept: both tools write UTF-8 directly rather than
/// `\u` escapes, and an ASCII-only token would disable the gate for exactly
/// the corpora that need it most.
fn prefilter_token(query: &str) -> &str {
    query
        .split(|c: char| c.is_whitespace() || c == '"' || c == '\\')
        .max_by_key(|part| part.len())
        .unwrap_or_default()
}

/// Substring search that ignores ASCII case, over bytes.
///
/// Safe on UTF-8 for a UTF-8 needle: a needle's leading byte is never a
/// continuation byte, so a match can only begin on a character boundary.
fn contains_ignore_ascii_case(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() {
        return true;
    }
    let Some(limit) = haystack.len().checked_sub(needle.len()) else { return false };
    let first = needle[0];
    let (lower, upper) = (first.to_ascii_lowercase(), first.to_ascii_uppercase());
    let mut base = 0usize;
    while base <= limit {
        let Some(offset) = memchr::memchr2(lower, upper, &haystack[base..=limit]) else {
            return false;
        };
        let at = base + offset;
        if haystack[at..at + needle.len()].eq_ignore_ascii_case(needle) {
            return true;
        }
        base = at + 1;
    }
    false
}

/// A bounded window of the record around the match.
fn snippet(text: &str, query: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let query_chars = query.chars().count();
    let at = char_index_of(text, query).unwrap_or(0);
    // Keep a little of what came before, so a hit reads as part of a sentence.
    let lead = MAX_SNIPPET_CHARS.saturating_sub(query_chars) / 3;
    let start = at.saturating_sub(lead);
    let end = start.saturating_add(MAX_SNIPPET_CHARS).min(chars.len());
    let mut out = String::new();
    if start > 0 {
        out.push('…');
    }
    out.extend(&chars[start..end]);
    if end < chars.len() {
        out.push('…');
    }
    crate::fmt::terminal_text(&out)
}

fn char_index_of(haystack: &str, needle: &str) -> Option<usize> {
    let (bytes, needle) = (haystack.as_bytes(), needle.as_bytes());
    for (chars, (byte_at, _)) in haystack.char_indices().enumerate() {
        let window = bytes.get(byte_at..byte_at + needle.len())?;
        if window.eq_ignore_ascii_case(needle) {
            return Some(chars);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Tokens, UsageEvent};

    /// A throwaway corpus on disk. Targets are handed to `run_targets`
    /// directly rather than discovered, so a test never depends on — or
    /// redirects — the environment of the machine running it.
    struct Corpus {
        root: std::path::PathBuf,
        targets: Vec<scan::Target>,
    }

    impl Corpus {
        fn new(tag: &str) -> Corpus {
            let nonce = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let root = std::env::temp_dir()
                .join(format!("readout-search-{tag}-{}-{nonce}", std::process::id()));
            std::fs::create_dir_all(&root).unwrap();
            Corpus { root, targets: Vec::new() }
        }

        fn file(&mut self, relative: &str, source: Source, lines: &[String]) -> &mut Corpus {
            let path = self.root.join(relative);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, lines.join("\n") + "\n").unwrap();
            self.targets.push(scan::Target { path, source });
            self
        }

        fn search(&self, query: &str, source: Source) -> Results {
            self.search_with(request(query, source))
        }

        fn search_with(&self, request: Request) -> Results {
            run_targets(&request, self.targets.clone()).unwrap()
        }

        fn index(pairs: &[(Source, &str, &str)]) -> SessionIndex {
            SessionIndex {
                known: pairs
                    .iter()
                    .map(|(source, session, project)| {
                        ((*source, (*session).to_string()), (*project).to_string())
                    })
                    .collect(),
            }
        }
    }

    impl Drop for Corpus {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    /// A request with everything but the query left at its default.
    fn request(query: &str, source: Source) -> Request {
        Request {
            query: query.into(),
            sources: vec![source],
            index: SessionIndex::default(),
            project: None,
            since: None,
            until: None,
            limit: 10,
        }
    }

    fn claude_text(uuid: &str, role: &str, text: &str) -> String {
        let escaped = serde_json::to_string(text).unwrap();
        format!(
            r#"{{"type":"{role}","uuid":"{uuid}","timestamp":"2026-08-15T10:00:00Z","message":{{"role":"{role}","content":[{{"type":"text","text":{escaped}}}]}}}}"#
        )
    }

    #[test]
    fn a_phrase_that_crosses_a_line_break_is_still_found() {
        // On disk the record holds `\n`; by the time anything reads it the
        // newline is a space. A gate built from the whole query would look for
        // a run of bytes the file never contained, and drop the hit.
        let mut corpus = Corpus::new("newline");
        corpus.file(
            "-w-proj/s-newline.jsonl",
            Source::Claude,
            &[claude_text("u1", "user", "the scan pool would\ndeadlock here")],
        );
        let results = corpus.search("would deadlock", Source::Claude);
        assert_eq!(results.total_matches, 1, "a folded newline must not hide the phrase");
        assert!(results.sessions[0].samples[0].snippet.contains("would deadlock"));
    }

    #[test]
    fn a_query_with_quotes_survives_json_escaping() {
        let mut corpus = Corpus::new("quotes");
        corpus.file(
            "-w-proj/s-quotes.jsonl",
            Source::Claude,
            &[claude_text("u1", "user", r#"call it "readout" everywhere"#)],
        );
        assert_eq!(corpus.search(r#"it "readout" every"#, Source::Claude).total_matches, 1);
    }

    #[test]
    fn matching_ignores_ascii_case_in_both_the_gate_and_the_confirmation() {
        let mut corpus = Corpus::new("case");
        corpus.file(
            "-w-proj/s-case.jsonl",
            Source::Claude,
            &[claude_text("u1", "assistant", "Deadlock detected")],
        );
        assert_eq!(corpus.search("deadlock", Source::Claude).total_matches, 1);
        assert_eq!(corpus.search("DEADLOCK", Source::Claude).total_matches, 1);
    }

    #[test]
    fn a_match_that_lives_only_in_metadata_is_not_a_hit() {
        // The uuid contains the query, so the raw line passes the byte gate.
        // Reporting it would hand the reader a result with nothing to read.
        let mut corpus = Corpus::new("meta");
        corpus.file(
            "-w-proj/s-meta.jsonl",
            Source::Claude,
            &[claude_text("carburettor-1", "user", "nothing to see")],
        );
        assert_eq!(corpus.search("carburettor", Source::Claude).total_matches, 0);
    }

    #[test]
    fn one_record_copied_into_a_forked_transcript_counts_once() {
        // Claude repeats history verbatim into forks. Replay collapses those,
        // so search has to as well, or one sentence is reported as several.
        let mut corpus = Corpus::new("fork");
        let line = claude_text("shared-uuid", "user", "find the deadlock");
        corpus.file("-w-proj/s-fork.jsonl", Source::Claude, std::slice::from_ref(&line));
        corpus.file("-w-proj/s-fork/subagents/a.jsonl", Source::Claude, &[line]);

        let results = corpus.search_with(Request {
            index: Corpus::index(&[(Source::Claude, "s-fork", "/w/proj")]),
            ..request("deadlock", Source::Claude)
        });
        assert_eq!(results.sessions.len(), 1, "a subagent file belongs to its parent session");
        assert_eq!(results.total_matches, 1, "the same record in two files is one match");
        assert_eq!(results.sessions[0].project, "/w/proj");
    }

    #[test]
    fn a_codex_rollout_is_attributed_to_the_thread_id_inside_its_name() {
        let thread = "019abec8-ab01-7df1-8fed-c1908d52660b";
        let mut corpus = Corpus::new("codex");
        corpus.file(
            &format!("2026/08/15/rollout-2026-08-15T10-00-00-{thread}.jsonl"),
            Source::Codex,
            &[r#"{"timestamp":"2026-08-15T10:00:00Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"reproduce the deadlock"}]}}"#.to_string()],
        );
        let results = corpus.search_with(Request {
            index: Corpus::index(&[(Source::Codex, thread, "/w/codex-proj")]),
            ..request("deadlock", Source::Codex)
        });
        assert_eq!(results.sessions.len(), 1);
        assert_eq!(results.sessions[0].session, thread);
        assert_eq!(results.sessions[0].project, "/w/codex-proj");
    }

    #[test]
    fn a_transcript_with_no_billed_request_is_still_searchable() {
        // A session the usage layer never saw still holds words. Losing it
        // would make search a view of the billing data rather than of history.
        let mut corpus = Corpus::new("unbilled");
        corpus.file(
            "-w-proj/s-unbilled.jsonl",
            Source::Claude,
            &[claude_text("u1", "user", "deadlock notes")],
        );
        let results = corpus.search("deadlock", Source::Claude);
        assert_eq!(results.sessions.len(), 1);
        assert_eq!(results.sessions[0].session, "s-unbilled");
        assert_eq!(results.sessions[0].project, "—", "an unknown project is named, not guessed");
    }

    #[test]
    fn the_gate_only_narrows_what_is_parsed() {
        assert_eq!(prefilter_token("would deadlock now"), "deadlock");
        assert_eq!(prefilter_token(r#"say "hi" politely"#), "politely");
        // Nothing survives escaping, so the gate stands down entirely.
        assert_eq!(prefilter_token(r#" " \ "#), "");
        assert_eq!(prefilter_token("死锁复现"), "死锁复现", "CJK keeps the gate useful");
    }

    #[test]
    fn a_query_of_only_separators_falls_back_to_parsing_every_line() {
        let mut corpus = Corpus::new("separators");
        corpus.file(
            "-w-proj/s-sep.jsonl",
            Source::Claude,
            &[claude_text("u1", "user", r#"say " " twice"#)],
        );
        assert_eq!(prefilter_token(r#"" ""#), "", "nothing in this query survives escaping");
        assert_eq!(
            corpus.search(r#"" ""#, Source::Claude).total_matches,
            1,
            "an unusable gate must not lose the hit"
        );
    }

    #[test]
    fn a_window_bounds_the_records_rather_than_the_sessions() {
        let mut corpus = Corpus::new("window");
        corpus.file(
            "-w-proj/s-window.jsonl",
            Source::Claude,
            &[
                claude_text("u1", "user", "deadlock in august"),
                claude_text("u2", "user", "deadlock again")
                    .replace("2026-08-15T10:00:00Z", "2026-08-20T10:00:00Z"),
            ],
        );
        let results = corpus.search_with(Request {
            since: Some(NaiveDate::from_ymd_opt(2026, 8, 18).unwrap()),
            until: Some(NaiveDate::from_ymd_opt(2026, 8, 25).unwrap()),
            ..request("deadlock", Source::Claude)
        });
        assert_eq!(results.total_matches, 1, "only the record inside the window counts");
        assert!(results.sessions[0].samples[0].snippet.contains("again"));
    }

    #[test]
    fn one_unreadable_transcript_leaves_a_counted_gap_rather_than_no_answer() {
        // A sweep of the whole corpus meets one root-owned or vanished file
        // sooner or later. Failing the search outright would make search
        // unusable on exactly the machines that have the most history.
        let mut corpus = Corpus::new("unreadable");
        corpus.file(
            "-w-proj/s-ok.jsonl",
            Source::Claude,
            &[claude_text("u1", "user", "deadlock here")],
        );
        corpus.targets.push(scan::Target {
            path: corpus.root.join("-w-proj/does-not-exist.jsonl"),
            source: Source::Claude,
        });
        let results = corpus.search("deadlock", Source::Claude);
        assert_eq!(results.total_matches, 1, "the readable file still answers");
        assert_eq!(results.files_failed, 1);
        assert_eq!(results.files_searched, 1);
        assert_eq!(results.files_total, 2, "the gap is visible in the counts");
    }

    #[test]
    fn a_torn_final_line_is_left_for_the_next_search() {
        let mut corpus = Corpus::new("torn");
        let whole = claude_text("u1", "user", "deadlock one");
        let torn = claude_text("u2", "user", "deadlock two");
        let path = corpus.root.join("-w-proj/s-torn.jsonl");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, format!("{whole}\n{}", &torn[..torn.len() / 2])).unwrap();
        corpus.targets.push(scan::Target { path, source: Source::Claude });
        assert_eq!(corpus.search("deadlock", Source::Claude).total_matches, 1);
    }

    #[test]
    fn a_query_shorter_than_the_floor_is_refused_rather_than_answered() {
        let corpus = Corpus::new("short");
        assert!(run_targets(&request("a", Source::Claude), corpus.targets.clone()).is_err());
    }

    #[test]
    fn the_session_index_takes_its_project_from_the_same_place_the_sessions_page_does() {
        let event = UsageEvent {
            source: Source::Claude,
            ts: 1_786_701_600,
            model: "claude-opus-5".into(),
            session: "s-1".into(),
            project: "/w/demo".into(),
            tokens: Tokens { input: 1, ..Default::default() },
            observed_on: Vec::new(),
            dedup_key: None,
            dedup_rank: 1,
        };
        let summary = crate::agg::summarize(
            std::slice::from_ref(&event),
            &crate::agg::Filter::default(),
            &crate::pricing::Pricing::builtin(),
        );
        let index = SessionIndex::from_summary(&summary);
        assert_eq!(
            index.resolve(Path::new("/c/-w-demo/s-1.jsonl"), Source::Claude),
            Some(("s-1".to_string(), "/w/demo".to_string()))
        );
        assert_eq!(
            index.resolve(Path::new("/c/-w-demo/s-1.jsonl"), Source::Codex),
            None,
            "a Codex rollout must not inherit a Claude session's project"
        );
    }

    #[test]
    fn a_snippet_is_bounded_and_points_at_the_match() {
        let text = format!("{}needle{}", "a".repeat(400), "b".repeat(400));
        let out = snippet(&text, "needle");
        assert!(out.contains("needle"));
        assert!(out.chars().count() <= MAX_SNIPPET_CHARS + 2, "two ellipses at most");
        assert!(out.starts_with('…') && out.ends_with('…'));
    }
}
