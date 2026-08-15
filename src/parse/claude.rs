//! Claude Code transcript parser.
//!
//! Transcripts live under `~/.claude/projects/<encoded-cwd>/` at three fixed
//! depths:
//!
//! ```text
//! <project>/*.jsonl                                    main sessions
//! <project>/<session-id>/subagents/*.jsonl             Task/Agent subagents
//! <project>/<session-id>/subagents/workflows/wf_*/*.jsonl  workflow subagents
//! ```
//!
//! The workflow layer matters: omit it and every token spent inside a
//! Workflow disappears from the totals. `journal.jsonl` sits alongside the
//! workflow transcripts but holds no `assistant` records, so it costs one
//! cheap scan and contributes nothing.

use crate::model::{Source, Tokens, UsageEvent, normalize_model};
use crate::parse::{FileParse, ParseCursor, complete_lines};
use serde_json::Value;
use std::collections::HashMap;
use std::path::Path;

/// One assistant record, before dedup.
#[derive(Debug, Clone)]
struct Assistant {
    message_id: String,
    model: String,
    tokens: Tokens,
    /// Present once the response finished streaming.
    has_stop_reason: bool,
    ts: i64,
    session: String,
    cwd: Option<String>,
}

/// Rank used by the replace rule. Higher wins; ties fall through to
/// output-token count.
fn rank(has_stop_reason: bool) -> u8 {
    if has_stop_reason { 1 } else { 0 }
}

/// Parse the newline-complete portion of a transcript starting at
/// `cursor.offset`.
pub fn parse_file(path: &Path, cursor: &ParseCursor, bytes: &[u8]) -> FileParse {
    // Same API response is emitted more than once per file (identical
    // `message.id` and `requestId`, different `uuid`) as streaming iterations
    // land. Collapsing on `message.id` is mandatory, not an optimization.
    let mut by_id: HashMap<String, Assistant> = HashMap::new();
    let mut consumed = 0usize;
    let mut cwd: Option<String> = cursor.claude_cwd.clone();
    let mut skipped_synthetic = 0u32;

    for (start, end) in complete_lines(bytes) {
        consumed = end + 1;
        let raw = &bytes[start..end];
        if raw.is_empty() {
            continue;
        }
        // Cheap reject before paying for JSON: every record we want carries
        // both markers.
        if !contains(raw, b"\"assistant\"") || !contains(raw, b"\"usage\"") {
            // `cwd` is on non-assistant records too, and it is the only
            // reliable way to recover the real project path.
            if contains(raw, b"\"cwd\"")
                && let Ok(v) = serde_json::from_slice::<Value>(raw)
                && let Some(next) = v.get("cwd").and_then(Value::as_str)
            {
                cwd = Some(next.to_string());
            }
            continue;
        }
        let Ok(v) = serde_json::from_slice::<Value>(raw) else { continue };
        if v.get("type").and_then(Value::as_str) != Some("assistant") {
            continue;
        }
        if let Some(next) = v.get("cwd").and_then(Value::as_str) {
            cwd = Some(next.to_string());
        }
        let Some(mut parsed) = parse_assistant(&v) else { continue };
        // Some transcript variants put `cwd` only on the preceding user or
        // system record. Snapshot the directory in effect now; using the
        // parser's final cwd later would reassign earlier requests after a
        // resumed session changes directories.
        parsed.cwd = cwd.clone();
        if parsed.model.starts_with('<') {
            skipped_synthetic = skipped_synthetic.saturating_add(1);
            continue;
        }
        // Billable gate: any nonzero token class counts. An earlier, stricter
        // gate (requiring input or output specifically) dropped cache-only
        // requests and undercounted the corpus by ~4%, almost all of it inside
        // workflow and subagent traffic.
        if parsed.tokens.is_empty() {
            continue;
        }

        match by_id.get(&parsed.message_id) {
            None => {
                by_id.insert(parsed.message_id.clone(), parsed);
            }
            Some(existing) => {
                let replace = match (parsed.has_stop_reason, existing.has_stop_reason) {
                    (true, false) => true,
                    (a, b) if a == b => parsed.tokens.output > existing.tokens.output,
                    _ => false,
                };
                if replace {
                    by_id.insert(parsed.message_id.clone(), parsed);
                }
            }
        }
    }

    let project = cwd
        .as_deref()
        .map(crate::paths::project_label_from_cwd)
        .unwrap_or_else(|| project_label_for(path));

    let events = by_id
        .into_values()
        .map(|a| UsageEvent {
            source: Source::Claude,
            ts: a.ts,
            model: a.model,
            session: a.session,
            project: a
                .cwd
                .as_deref()
                .map(crate::paths::project_label_from_cwd)
                .unwrap_or_else(|| project.clone()),
            tokens: a.tokens,
            dedup_rank: rank(a.has_stop_reason),
            dedup_key: Some(a.message_id),
        })
        .collect();

    FileParse {
        events,
        consumed,
        cursor: ParseCursor { claude_cwd: cwd, ..ParseCursor::default() },
        skipped_synthetic,
    }
}

fn parse_assistant(v: &Value) -> Option<Assistant> {
    let message = v.get("message")?;
    let usage = message.get("usage")?.as_object()?;
    let message_id = message.get("id").and_then(Value::as_str)?.to_string();
    let model = normalize_model(
        message.get("model").and_then(Value::as_str).unwrap_or_default(),
        Source::Claude,
    );

    let n = |k: &str| usage.get(k).and_then(Value::as_u64).unwrap_or(0);
    let cache_creation_total = n("cache_creation_input_tokens");
    let (w5, w1) = split_cache_creation(usage.get("cache_creation"), cache_creation_total);

    let tokens = Tokens {
        input: n("input_tokens"),
        output: n("output_tokens"),
        cache_read: n("cache_read_input_tokens"),
        cache_write_5m: w5,
        cache_write_1h: w1,
    };

    Some(Assistant {
        message_id,
        model,
        tokens,
        has_stop_reason: !message.get("stop_reason").map(Value::is_null).unwrap_or(true),
        ts: v
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(crate::parse::parse_ts)
            .unwrap_or(0),
        session: v
            .get("sessionId")
            .or_else(|| v.get("session_id"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        cwd: v.get("cwd").and_then(Value::as_str).map(str::to_string),
    })
}

/// Split cache-creation tokens across the 5-minute and 1-hour TTL tiers,
/// which are priced at 1.25x and 2x base input respectively.
///
/// The `cache_creation` breakdown object usually sums exactly to
/// `cache_creation_input_tokens`. When a response streamed across multiple
/// iterations the breakdown reflects only the last one, so it under-sums; in
/// that case we keep the authoritative total and split it in the breakdown's
/// observed proportion. With no breakdown at all, everything lands in the
/// 5-minute tier, which is the default TTL.
fn split_cache_creation(breakdown: Option<&Value>, total: u64) -> (u64, u64) {
    if total == 0 {
        return (0, 0);
    }
    let Some(obj) = breakdown.and_then(Value::as_object) else {
        return (total, 0);
    };
    let g = |k: &str| obj.get(k).and_then(Value::as_u64).unwrap_or(0);
    let five = g("ephemeral_5m_input_tokens");
    let hour = g("ephemeral_1h_input_tokens");
    let sum = five as u128 + hour as u128;
    if sum == total as u128 {
        return (five, hour);
    }
    if sum == 0 {
        return (total, 0);
    }
    // Scale to the authoritative total, giving the remainder to the 1-hour
    // tier so the pair always re-sums exactly.
    let scaled_five = (five as u128 * total as u128 / sum) as u64;
    (scaled_five, total - scaled_five)
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    memchr::memmem::find(haystack, needle).is_some()
}

/// Fallback project label: walk up to the `projects/<dir>` component.
fn project_label_for(path: &Path) -> String {
    let mut cur = path.parent();
    let mut last = None;
    while let Some(dir) = cur {
        if dir.file_name().and_then(|s| s.to_str()) == Some("projects")
            && let Some(name) = last
        {
            return crate::paths::project_label_from_claude_dir(name);
        }
        last = dir.file_name().and_then(|s| s.to_str());
        cur = dir.parent();
    }
    last.map(crate::paths::project_label_from_claude_dir).unwrap_or_else(|| "unknown".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> FileParse {
        parse_file(
            Path::new("/tmp/projects/-a-b/s.jsonl"),
            &ParseCursor::default(),
            text.as_bytes(),
        )
    }

    const MSG: &str = r#"{"type":"assistant","timestamp":"2026-08-14T10:00:00.000Z","sessionId":"s1","cwd":"/home/u/proj","message":{"id":"msg_1","model":"claude-opus-5","stop_reason":null,"usage":{"input_tokens":10,"output_tokens":20,"cache_read_input_tokens":30,"cache_creation_input_tokens":40,"cache_creation":{"ephemeral_5m_input_tokens":15,"ephemeral_1h_input_tokens":25}}}}"#;

    #[test]
    fn parses_one_assistant_record() {
        let out = parse(&format!("{MSG}\n"));
        assert_eq!(out.events.len(), 1);
        let e = &out.events[0];
        assert_eq!(e.model, "claude-opus-5");
        assert_eq!(e.session, "s1");
        assert_eq!(e.project, "/home/u/proj");
        assert_eq!(e.tokens.input, 10);
        assert_eq!(e.tokens.output, 20);
        assert_eq!(e.tokens.cache_read, 30);
        assert_eq!(e.tokens.cache_write_5m, 15);
        assert_eq!(e.tokens.cache_write_1h, 25);
        assert_eq!(out.consumed, MSG.len() + 1);
    }

    #[test]
    fn a_finished_response_replaces_an_unfinished_one() {
        let partial = MSG.replace(r#""output_tokens":20"#, r#""output_tokens":5"#);
        let final_ = MSG
            .replace(r#""stop_reason":null"#, r#""stop_reason":"end_turn""#)
            .replace(r#""output_tokens":20"#, r#""output_tokens":9"#);
        // Even though the final record reports FEWER output tokens, having a
        // stop_reason wins.
        let out = parse(&format!("{partial}\n{final_}\n"));
        assert_eq!(out.events.len(), 1);
        assert_eq!(out.events[0].tokens.output, 9);
    }

    #[test]
    fn among_equally_unfinished_records_the_largest_wins() {
        let a = MSG.replace(r#""output_tokens":20"#, r#""output_tokens":5"#);
        let b = MSG.replace(r#""output_tokens":20"#, r#""output_tokens":50"#);
        assert_eq!(parse(&format!("{a}\n{b}\n")).events[0].tokens.output, 50);
        // Order must not matter.
        assert_eq!(parse(&format!("{b}\n{a}\n")).events[0].tokens.output, 50);
    }

    #[test]
    fn cache_only_requests_are_billable() {
        let cache_only = MSG
            .replace(r#""input_tokens":10"#, r#""input_tokens":0"#)
            .replace(r#""output_tokens":20"#, r#""output_tokens":0"#);
        let out = parse(&format!("{cache_only}\n"));
        assert_eq!(out.events.len(), 1, "cache reads/writes alone are still billed");
    }

    #[test]
    fn zero_token_records_are_dropped() {
        let empty = MSG
            .replace(r#""input_tokens":10"#, r#""input_tokens":0"#)
            .replace(r#""output_tokens":20"#, r#""output_tokens":0"#)
            .replace(r#""cache_read_input_tokens":30"#, r#""cache_read_input_tokens":0"#)
            .replace(r#""cache_creation_input_tokens":40"#, r#""cache_creation_input_tokens":0"#);
        assert!(parse(&format!("{empty}\n")).events.is_empty());
    }

    #[test]
    fn synthetic_records_are_counted_but_not_billed() {
        let synth = MSG.replace(r#""claude-opus-5""#, r#""<synthetic>""#);
        let out = parse(&format!("{synth}\n"));
        assert!(out.events.is_empty());
        assert_eq!(out.skipped_synthetic, 1);
    }

    #[test]
    fn a_partial_trailing_line_is_not_consumed() {
        let out = parse(&format!("{MSG}\n{{\"type\":\"assis"));
        assert_eq!(out.consumed, MSG.len() + 1, "the torn tail line must be re-read next scan");
        assert_eq!(out.events.len(), 1);
    }

    #[test]
    fn cache_creation_breakdown_is_rescaled_when_it_under_sums() {
        // Streaming iterations: breakdown reports only the last iteration.
        let v: Value =
            serde_json::json!({"ephemeral_5m_input_tokens": 0, "ephemeral_1h_input_tokens": 1635});
        let (w5, w1) = split_cache_creation(Some(&v), 3913);
        assert_eq!(w5 + w1, 3913, "the split must re-sum to the authoritative total");
        assert_eq!((w5, w1), (0, 3913));

        let v2: Value =
            serde_json::json!({"ephemeral_5m_input_tokens": 1, "ephemeral_1h_input_tokens": 1});
        let (w5, w1) = split_cache_creation(Some(&v2), 100);
        assert_eq!((w5, w1), (50, 50));
    }

    #[test]
    fn hostile_cache_breakdowns_do_not_overflow() {
        let v: Value = serde_json::json!({
            "ephemeral_5m_input_tokens": u64::MAX,
            "ephemeral_1h_input_tokens": u64::MAX
        });
        let (w5, w1) = split_cache_creation(Some(&v), u64::MAX);
        assert_eq!(w5.saturating_add(w1), u64::MAX);
        assert_eq!(w5, u64::MAX / 2);
    }

    #[test]
    fn missing_breakdown_defaults_to_the_five_minute_tier() {
        assert_eq!(split_cache_creation(None, 500), (500, 0));
        assert_eq!(split_cache_creation(None, 0), (0, 0));
    }

    #[test]
    fn project_falls_back_to_the_directory_when_cwd_is_absent() {
        let no_cwd = MSG.replace(r#""cwd":"/home/u/proj","#, "");
        let out = parse(&format!("{no_cwd}\n"));
        assert_eq!(out.events[0].project, "a-b");
    }

    #[test]
    fn each_response_keeps_the_cwd_in_effect_for_that_request() {
        let first = MSG;
        let second = MSG
            .replace(r#""id":"msg_1""#, r#""id":"msg_2""#)
            .replace("/home/u/proj", "/work/other/proj");
        let out = parse(&format!("{first}\n{second}\n"));
        let mut projects: Vec<_> = out.events.iter().map(|e| e.project.as_str()).collect();
        projects.sort();
        assert_eq!(projects, vec!["/home/u/proj", "/work/other/proj"]);
    }

    #[test]
    fn cwd_from_a_preceding_record_is_snapshotted_per_response() {
        let first = MSG.replace(r#""cwd":"/home/u/proj","#, "");
        let second = first.replace(r#""id":"msg_1""#, r#""id":"msg_2""#);
        let home = r#"{"type":"user","cwd":"/home/u/proj"}"#;
        let work = r#"{"type":"user","cwd":"/work/other/proj"}"#;
        let out = parse(&format!("{home}\n{first}\n{work}\n{second}\n"));
        let mut projects: Vec<_> = out.events.iter().map(|e| e.project.as_str()).collect();
        projects.sort();
        assert_eq!(projects, vec!["/home/u/proj", "/work/other/proj"]);
    }
}
