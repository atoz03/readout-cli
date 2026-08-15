//! Codex rollout parser.
//!
//! Rollouts live at `~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl`. Three
//! record types matter:
//!
//! * `session_meta`  — thread id and `cwd`, once per file
//! * `turn_context`  — the model in effect from here on
//! * `event_msg` with `payload.type == "token_count"` — usage
//!
//! Usage arrives as *cumulative* snapshots (`total_token_usage`) alongside an
//! exact per-request figure (`last_token_usage`). Two hazards follow:
//!
//! 1. The same snapshot is re-emitted whenever a rate-limit lane refreshes,
//!    with no new request behind it. Counting those inflates every total, so
//!    a snapshot whose signature repeats — for its own rate-limit lane, or
//!    back-to-back — contributes zero.
//! 2. Cumulative counters come from independently advancing lanes, so
//!    subtracting consecutive snapshots can produce garbage. When Codex gives
//!    us `last_token_usage` we take it verbatim and only fall back to
//!    high-water subtraction when it is missing.
//!
//! Codex reports `input_tokens` **inclusive** of `cached_input_tokens`, unlike
//! Claude. We subtract at parse time so `Tokens::input` means "fresh input"
//! everywhere downstream.

use crate::model::{Source, Tokens, UsageEvent};
use crate::parse::{FileParse, ParseCursor, complete_lines};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::path::Path;

/// Cumulative or per-request counters, as written by Codex.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Counters {
    pub input: u64,
    /// Cached portion of `input`, not additional to it.
    pub cached_input: u64,
    pub output: u64,
}

/// Identity of a token snapshot, used to spot re-emissions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Signature {
    total: Option<Counters>,
    last: Option<Counters>,
}

/// Parser state that must survive an incremental resume mid-file.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct CodexState {
    pub thread_id: Option<String>,
    pub cwd: Option<String>,
    pub model: Option<String>,
    pub high_water: Option<Counters>,
    /// Keyed by `rate_limits.limit_id`; `None` is a valid key (no lane id).
    pub last_signature_by_source: Vec<(Option<String>, Signature)>,
    pub previous_signature: Option<Signature>,
}

impl CodexState {
    fn source_map(&self) -> HashMap<Option<String>, Signature> {
        self.last_signature_by_source.iter().cloned().collect()
    }
}

pub fn parse_file(path: &Path, cursor: &ParseCursor, bytes: &[u8]) -> FileParse {
    let mut st = cursor.codex.clone().unwrap_or_default();
    let mut by_source = st.source_map();
    let mut events: Vec<UsageEvent> = Vec::new();
    let mut consumed = 0usize;

    for (start, end) in complete_lines(bytes) {
        consumed = end + 1;
        let raw = &bytes[start..end];
        if raw.is_empty() {
            continue;
        }

        // Rollouts are dominated by `response_item` records carrying full
        // message bodies — the 1.4 GB is almost entirely those. Rejecting them
        // on a substring test keeps serde off the hot path.
        let is_event = contains(raw, b"\"event_msg\"");
        let is_turn = contains(raw, b"\"turn_context\"");
        let is_meta = contains(raw, b"\"session_meta\"");
        if !is_event && !is_turn && !is_meta {
            continue;
        }
        if is_event && !contains(raw, b"\"token_count\"") {
            continue;
        }

        let Ok(v) = serde_json::from_slice::<Value>(raw) else { continue };
        let Some(kind) = v.get("type").and_then(Value::as_str) else { continue };
        let Some(payload) = v.get("payload") else { continue };

        match kind {
            "session_meta" => {
                if st.thread_id.is_none() {
                    st.thread_id = payload
                        .get("session_id")
                        .or_else(|| payload.get("id"))
                        .and_then(Value::as_str)
                        .map(str::to_string);
                }
                if st.cwd.is_none() {
                    st.cwd = payload.get("cwd").and_then(Value::as_str).map(str::to_string);
                }
            }
            "turn_context" => {
                if let Some(m) = payload.get("model").and_then(Value::as_str) {
                    st.model = Some(normalize_codex_model(m));
                }
                if st.cwd.is_none() {
                    st.cwd = payload.get("cwd").and_then(Value::as_str).map(str::to_string);
                }
            }
            "event_msg" => {
                if payload.get("type").and_then(Value::as_str) != Some("token_count") {
                    continue;
                }
                // `info` is explicitly null on some events; `get` alone would
                // hand back a Value::Null and every field read would silently
                // yield zero.
                let Some(info) = payload.get("info").filter(|i| !i.is_null()) else { continue };

                if let Some(m) = info
                    .get("model")
                    .or_else(|| info.get("model_name"))
                    .or_else(|| payload.get("model"))
                    .and_then(Value::as_str)
                {
                    st.model = Some(normalize_codex_model(m));
                }

                let total = info.get("total_token_usage").and_then(counters);
                let last = info.get("last_token_usage").and_then(counters);
                if total.is_none() && last.is_none() {
                    continue;
                }
                let signature = Signature { total, last };
                let source = snapshot_source(payload);

                let duplicate = total.is_some()
                    && (by_source.get(&source) == Some(&signature)
                        || st.previous_signature.as_ref() == Some(&signature));
                if total.is_some() {
                    by_source.insert(source, signature.clone());
                }
                st.previous_signature = Some(signature);

                let delta = if duplicate {
                    Counters::default()
                } else if let Some(last) = last {
                    last
                } else if let Some(total) = total.as_ref() {
                    delta_from_high_water(&st.high_water, total)
                } else {
                    continue;
                };

                if let Some(total) = total {
                    st.high_water = Some(match st.high_water {
                        Some(hw) => Counters {
                            input: hw.input.max(total.input),
                            cached_input: hw.cached_input.max(total.cached_input),
                            output: hw.output.max(total.output),
                        },
                        None => total,
                    });
                }

                // A cached count above the input it is drawn from is
                // impossible; clamp rather than let it drive input negative.
                let cached = delta.cached_input.min(delta.input);
                let tokens = Tokens {
                    input: delta.input - cached,
                    output: delta.output,
                    cache_read: cached,
                    cache_write_5m: 0,
                    cache_write_1h: 0,
                };
                if tokens.is_empty() {
                    continue;
                }

                events.push(UsageEvent {
                    source: Source::Codex,
                    ts: v
                        .get("timestamp")
                        .and_then(Value::as_str)
                        .and_then(super::parse_ts)
                        .unwrap_or(0),
                    model: st.model.clone().unwrap_or_else(|| "unknown".into()),
                    session: st.thread_id.clone().unwrap_or_else(|| file_stem(path)),
                    project: st
                        .cwd
                        .as_deref()
                        .map(crate::paths::project_label_from_cwd)
                        .unwrap_or_else(|| "unknown".into()),
                    tokens,
                    dedup_key: None,
                    dedup_rank: 0,
                });
            }
            _ => {}
        }
    }

    st.last_signature_by_source = by_source.into_iter().collect();
    st.last_signature_by_source.sort_by(|a, b| a.0.cmp(&b.0));

    FileParse {
        events,
        consumed,
        cursor: ParseCursor { codex: Some(st), ..ParseCursor::default() },
        skipped_synthetic: 0,
    }
}

fn delta_from_high_water(prev: &Option<Counters>, current: &Counters) -> Counters {
    match prev {
        None => *current,
        Some(p) => Counters {
            input: current.input.saturating_sub(p.input),
            cached_input: current.cached_input.saturating_sub(p.cached_input),
            output: current.output.saturating_sub(p.output),
        },
    }
}

fn counters(v: &Value) -> Option<Counters> {
    let o = v.as_object()?;
    let n = |k: &str| o.get(k).and_then(Value::as_u64);
    // Reject objects that carry none of the fields we need, so a stray empty
    // object is not mistaken for a zeroed snapshot.
    let input = n("input_tokens");
    let cached = n("cached_input_tokens").or_else(|| n("cache_read_input_tokens"));
    let output = n("output_tokens");
    if input.is_none() && cached.is_none() && output.is_none() && n("total_tokens").is_none() {
        return None;
    }
    Some(Counters {
        input: input.unwrap_or(0),
        cached_input: cached.unwrap_or(0),
        // `reasoning_output_tokens` is already included in `output_tokens`;
        // adding it would double-count reasoning.
        output: output.unwrap_or(0),
    })
}

fn snapshot_source(payload: &Value) -> Option<String> {
    payload
        .get("rate_limits")
        .and_then(|r| r.get("limit_id"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// Lowercase, drop a `provider/` prefix, drop an ISO or compact date suffix.
pub fn normalize_codex_model(raw: &str) -> String {
    let mut name = raw.to_lowercase();
    if let Some(pos) = name.rfind('/') {
        name = name[pos + 1..].to_string();
    }
    // `-YYYY-MM-DD`
    if name.len() > 11 {
        let suffix = &name[name.len() - 11..];
        let b = suffix.as_bytes();
        if suffix.is_ascii()
            && b[0] == b'-'
            && b[1..5].iter().all(u8::is_ascii_digit)
            && b[5] == b'-'
            && b[6..8].iter().all(u8::is_ascii_digit)
            && b[8] == b'-'
            && b[9..11].iter().all(u8::is_ascii_digit)
        {
            name.truncate(name.len() - 11);
            return name;
        }
    }
    // `-YYYYMMDD`
    if name.len() > 9 {
        let (head, tail) = name.split_at(name.len() - 9);
        if tail.starts_with('-') && tail[1..].bytes().all(|c| c.is_ascii_digit()) {
            return head.to_string();
        }
    }
    name
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    memchr::memmem::find(haystack, needle).is_some()
}

fn file_stem(path: &Path) -> String {
    path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> FileParse {
        parse_file(Path::new("/tmp/rollout-x.jsonl"), &ParseCursor::default(), text.as_bytes())
    }

    fn token_event(total: (u64, u64, u64), last: Option<(u64, u64, u64)>, limit: &str) -> String {
        let last_json = match last {
            Some((i, c, o)) => format!(
                r#","last_token_usage":{{"input_tokens":{i},"cached_input_tokens":{c},"output_tokens":{o}}}"#
            ),
            None => String::new(),
        };
        format!(
            r#"{{"timestamp":"2026-08-14T13:04:18.366Z","type":"event_msg","payload":{{"type":"token_count","info":{{"total_token_usage":{{"input_tokens":{},"cached_input_tokens":{},"output_tokens":{}}}{last_json}}},"rate_limits":{{"limit_id":"{limit}"}}}}}}"#,
            total.0, total.1, total.2
        )
    }

    const META: &str = r#"{"timestamp":"2026-08-14T13:04:05.737Z","type":"session_meta","payload":{"session_id":"th-1","cwd":"/home/u/demo-proj"}}"#;
    const CTX: &str = r#"{"timestamp":"2026-08-14T13:04:05.745Z","type":"turn_context","payload":{"model":"gpt-5.6-sol","cwd":"/home/u/demo-proj"}}"#;

    #[test]
    fn input_is_reported_net_of_the_cached_portion() {
        // Codex counts cached tokens inside input_tokens.
        let e = token_event((36428, 17152, 748), Some((18844, 17152, 404)), "codex");
        let out = parse(&format!("{META}\n{CTX}\n{e}\n"));
        assert_eq!(out.events.len(), 1);
        let t = out.events[0].tokens;
        assert_eq!(t.input, 18844 - 17152);
        assert_eq!(t.cache_read, 17152);
        assert_eq!(t.output, 404);
        assert_eq!(out.events[0].model, "gpt-5.6-sol");
        assert_eq!(out.events[0].session, "th-1");
        assert_eq!(out.events[0].project, "demo-proj");
    }

    #[test]
    fn exact_per_request_usage_beats_subtracting_cumulative_snapshots() {
        // The cumulative jump (30000) disagrees with last_token_usage (500);
        // last_token_usage wins because lanes advance independently.
        let a = token_event((1000, 0, 100), Some((1000, 0, 100)), "codex");
        let b = token_event((31000, 0, 300), Some((500, 0, 200)), "codex");
        let out = parse(&format!("{META}\n{CTX}\n{a}\n{b}\n"));
        assert_eq!(out.events.len(), 2);
        assert_eq!(out.events[1].tokens.input, 500);
        assert_eq!(out.events[1].tokens.output, 200);
    }

    #[test]
    fn a_re_emitted_snapshot_contributes_nothing() {
        let e = token_event((1000, 0, 100), Some((1000, 0, 100)), "codex");
        let out = parse(&format!("{META}\n{CTX}\n{e}\n{e}\n"));
        assert_eq!(out.events.len(), 1, "the repeat is a rate-limit refresh, not a request");
    }

    #[test]
    fn repeats_are_suppressed_per_rate_limit_lane() {
        // Two lanes interleave; each lane repeating its own last snapshot is a
        // refresh even though the immediately preceding line differed.
        let a = token_event((1000, 0, 100), Some((1000, 0, 100)), "codex");
        let b = token_event((2000, 0, 200), Some((1000, 0, 100)), "other");
        let out = parse(&format!("{META}\n{CTX}\n{a}\n{b}\n{a}\n"));
        assert_eq!(out.events.len(), 2, "the third line repeats lane `codex`");
    }

    #[test]
    fn falls_back_to_high_water_subtraction_without_last_usage() {
        let a = token_event((1000, 200, 100), None, "codex");
        let b = token_event((2500, 700, 250), None, "codex");
        let out = parse(&format!("{META}\n{CTX}\n{a}\n{b}\n"));
        assert_eq!(out.events.len(), 2);
        assert_eq!(out.events[0].tokens.input, 1000 - 200);
        assert_eq!(out.events[1].tokens.input, 1500 - 500);
        assert_eq!(out.events[1].tokens.cache_read, 500);
    }

    #[test]
    fn a_regressing_cumulative_snapshot_does_not_go_negative() {
        let a = token_event((5000, 0, 500), None, "codex");
        let b = token_event((3000, 0, 200), None, "codex");
        let out = parse(&format!("{META}\n{CTX}\n{a}\n{b}\n"));
        // The regression yields a zero delta, which is dropped, not negative.
        assert_eq!(out.events.len(), 1);
    }

    #[test]
    fn cached_above_input_is_clamped() {
        let e = token_event((100, 0, 10), Some((10, 80, 5)), "codex");
        let out = parse(&format!("{META}\n{CTX}\n{e}\n"));
        let t = out.events[0].tokens;
        assert_eq!(t.cache_read, 10);
        assert_eq!(t.input, 0, "input must not go negative");
    }

    #[test]
    fn null_info_is_skipped() {
        let e = r#"{"type":"event_msg","payload":{"type":"token_count","info":null}}"#;
        assert!(parse(&format!("{META}\n{CTX}\n{e}\n")).events.is_empty());
    }

    #[test]
    fn resuming_mid_file_continues_the_same_dedup_state() {
        let a = token_event((1000, 0, 100), Some((1000, 0, 100)), "codex");
        let whole = format!("{META}\n{CTX}\n{a}\n{a}\n");
        let split = format!("{META}\n{CTX}\n{a}\n");

        let first = parse(&split);
        assert_eq!(first.events.len(), 1);
        // Feed the remainder with the carried cursor, as an incremental scan would.
        let rest = parse_file(
            Path::new("/tmp/rollout-x.jsonl"),
            &first.cursor,
            format!("{a}\n").as_bytes(),
        );
        assert!(rest.events.is_empty(), "the repeat must still be recognized across the resume");
        assert_eq!(parse(&whole).events.len(), 1);
    }

    #[test]
    fn a_partial_trailing_line_is_not_consumed() {
        let a = token_event((1000, 0, 100), Some((1000, 0, 100)), "codex");
        let text = format!("{META}\n{CTX}\n{a}\n{{\"type\":\"eve");
        let out = parse(&text);
        assert_eq!(out.consumed, text.len() - r#"{"type":"eve"#.len());
    }

    #[test]
    fn model_names_normalize() {
        assert_eq!(normalize_codex_model("openai/GPT-5.4"), "gpt-5.4");
        assert_eq!(normalize_codex_model("gpt-5.4-2026-03-05"), "gpt-5.4");
        assert_eq!(normalize_codex_model("gpt-5.4-20260305"), "gpt-5.4");
        assert_eq!(normalize_codex_model("gpt-5.3-codex"), "gpt-5.3-codex");
    }
}
