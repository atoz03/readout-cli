//! Session replay 按需读取原始 transcript。
//!
//! Usage 扫描只保留计费元数据；消息正文和工具输出可能包含敏感信息，
//! 因此 replay 不进入增量缓存，只在用户打开具体 session 时读取。

use crate::model::Source;
use crate::scan;
use anyhow::{Context, Result};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::io::{BufRead, BufReader, Read};
use std::path::Path;

const MAX_REPLAY_FILES: usize = 256;
const MAX_REPLAY_EVENTS: usize = 20_000;
const MAX_REPLAY_TEXT_BYTES: usize = 16 * 1024 * 1024;
const MAX_REPLAY_READ_BYTES: u64 = 16 * 1024 * 1024 * 1024;
const MAX_EVENT_PREVIEW_CHARS: usize = 4_096;
const MAX_ID_CHARS: usize = 512;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplayRequest {
    pub source: Source,
    pub session: String,
    pub project: String,
    pub model: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplayKind {
    User,
    Assistant,
    ToolCall,
    ToolResult,
    ToolError,
    System,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplayEvent {
    /// Unix 毫秒；源记录没有时间时为 0。
    pub ts_ms: i64,
    /// 相对 replay 起点的毫秒数。
    pub offset_ms: u64,
    pub kind: ReplayKind,
    pub title: String,
    pub detail: String,
}

#[derive(Debug, Clone)]
pub struct SessionReplay {
    pub events: Vec<ReplayEvent>,
    pub first_ts_ms: i64,
    pub last_ts_ms: i64,
    pub truncated: bool,
}

impl SessionReplay {
    pub fn duration_ms(&self) -> u64 {
        self.events.last().map_or(0, |event| event.offset_ms)
    }
}

#[derive(Debug)]
struct PendingEvent {
    ts_ms: i64,
    order: u64,
    kind: ReplayKind,
    title: String,
    detail: String,
    dedup_key: Option<String>,
}

/// 从 active transcript 中读取一个 session；不会触碰 archived_sessions。
pub fn load(request: ReplayRequest) -> Result<SessionReplay> {
    let mut targets: Vec<_> = scan::discover_all(&[request.source])
        .into_iter()
        .filter(|target| path_matches_session(&target.path, &request.session))
        .collect();
    targets.sort_by(|a, b| a.path.cmp(&b.path));
    anyhow::ensure!(
        !targets.is_empty(),
        "active transcript not found for session {}",
        request.session
    );
    anyhow::ensure!(
        targets.len() <= MAX_REPLAY_FILES,
        "session spans more than {MAX_REPLAY_FILES} transcript files"
    );
    load_targets(request, targets)
}

fn load_targets(request: ReplayRequest, targets: Vec<scan::Target>) -> Result<SessionReplay> {
    let mut pending = Vec::new();
    let mut seen = HashSet::new();
    let mut call_names: HashMap<String, String> = HashMap::new();
    let mut order = 0u64;
    let mut text_bytes = 0usize;
    let mut bytes_read = 0u64;
    let mut truncated = false;

    'files: for target in targets {
        let meta = std::fs::symlink_metadata(&target.path)
            .with_context(|| format!("reading metadata for {}", target.path.display()))?;
        anyhow::ensure!(meta.file_type().is_file(), "replay transcript is not a regular file");
        anyhow::ensure!(
            meta.len() <= scan::MAX_TRANSCRIPT_BYTES,
            "replay transcript exceeds the {} byte safety limit",
            scan::MAX_TRANSCRIPT_BYTES
        );

        let file = std::fs::File::open(&target.path)
            .with_context(|| format!("opening replay transcript {}", target.path.display()))?;
        let mut reader = BufReader::new(file);
        let mut line = Vec::new();

        loop {
            line.clear();
            let mut limited = (&mut reader).take(scan::MAX_JSONL_LINE_BYTES as u64 + 1);
            let read = limited
                .read_until(b'\n', &mut line)
                .with_context(|| format!("reading replay transcript {}", target.path.display()))?;
            if read == 0 {
                break;
            }
            bytes_read = bytes_read.saturating_add(read as u64);
            anyhow::ensure!(
                bytes_read <= MAX_REPLAY_READ_BYTES,
                "session replay exceeds the {} byte aggregate read limit",
                MAX_REPLAY_READ_BYTES
            );
            anyhow::ensure!(
                line.len() <= scan::MAX_JSONL_LINE_BYTES,
                "replay JSONL line exceeds the {} byte safety limit",
                scan::MAX_JSONL_LINE_BYTES
            );
            // 正在追加的尾行不完整，留给下一次打开 replay。
            if line.last() != Some(&b'\n') {
                break;
            }
            let Ok(value) = serde_json::from_slice::<Value>(&line) else { continue };
            let parsed = match request.source {
                Source::Claude => parse_claude(&value, &mut call_names),
                Source::Codex => parse_codex(&value, &mut call_names),
            };
            for mut event in parsed {
                if let Some(key) = event.dedup_key.as_ref()
                    && !seen.insert(key.clone())
                {
                    continue;
                }
                order = order.saturating_add(1);
                event.order = order;
                if pending.len() >= MAX_REPLAY_EVENTS {
                    truncated = true;
                    break 'files;
                }
                let remaining = MAX_REPLAY_TEXT_BYTES.saturating_sub(text_bytes);
                if event.title.len() > remaining {
                    truncated = true;
                    break 'files;
                }
                let detail_budget = remaining - event.title.len();
                if event.detail.len() > detail_budget {
                    const OMITTED: &str = "[detail omitted: replay text limit reached]";
                    event.detail =
                        if OMITTED.len() <= detail_budget { OMITTED.into() } else { String::new() };
                    truncated = true;
                }
                text_bytes =
                    text_bytes.saturating_add(event.title.len()).saturating_add(event.detail.len());
                pending.push(event);
            }
        }
    }

    pending.sort_by(|a, b| match (a.ts_ms > 0, b.ts_ms > 0) {
        (true, true) => a.ts_ms.cmp(&b.ts_ms).then(a.order.cmp(&b.order)),
        (true, false) => std::cmp::Ordering::Less,
        (false, true) => std::cmp::Ordering::Greater,
        (false, false) => a.order.cmp(&b.order),
    });

    let first_ts_ms =
        pending.iter().filter(|event| event.ts_ms > 0).map(|e| e.ts_ms).min().unwrap_or(0);
    let last_ts_ms =
        pending.iter().filter(|event| event.ts_ms > 0).map(|e| e.ts_ms).max().unwrap_or(0);
    let mut fallback_offset = 0u64;
    let events = pending
        .into_iter()
        .map(|event| {
            let offset_ms = if event.ts_ms > 0 && first_ts_ms > 0 {
                event.ts_ms.saturating_sub(first_ts_ms) as u64
            } else {
                fallback_offset = fallback_offset.saturating_add(1_000);
                fallback_offset
            };
            fallback_offset = fallback_offset.max(offset_ms);
            ReplayEvent {
                ts_ms: event.ts_ms,
                offset_ms,
                kind: event.kind,
                title: event.title,
                detail: event.detail,
            }
        })
        .collect();

    Ok(SessionReplay { events, first_ts_ms, last_ts_ms, truncated })
}

fn path_matches_session(path: &Path, session: &str) -> bool {
    let wanted = OsStr::new(session);
    path.file_stem().is_some_and(|stem| stem == wanted)
        || path.components().any(|component| component.as_os_str() == wanted)
        || path.file_name().and_then(OsStr::to_str).is_some_and(|name| name.contains(session))
}

fn parse_codex(value: &Value, call_names: &mut HashMap<String, String>) -> Vec<PendingEvent> {
    if value.get("type").and_then(Value::as_str) != Some("response_item") {
        return Vec::new();
    }
    let Some(payload) = value.get("payload") else { return Vec::new() };
    let ts_ms = timestamp_ms(value);
    let kind = payload.get("type").and_then(Value::as_str).unwrap_or_default();
    match kind {
        "message" => {
            let role = payload.get("role").and_then(Value::as_str).unwrap_or("system");
            let detail = message_content(payload.get("content"));
            if detail.is_empty() {
                return Vec::new();
            }
            let (event_kind, title) = match role {
                "user" => (ReplayKind::User, "user".to_string()),
                "assistant" => (ReplayKind::Assistant, "assistant".to_string()),
                other => (ReplayKind::System, preview(other)),
            };
            vec![pending(
                ts_ms,
                event_kind,
                title,
                detail,
                payload
                    .get("id")
                    .and_then(Value::as_str)
                    .map(bounded_id)
                    .map(|id| format!("codex:message:{id}")),
            )]
        }
        "function_call" | "custom_tool_call" => {
            let name = preview(payload.get("name").and_then(Value::as_str).unwrap_or("tool"));
            let call_id = payload
                .get("call_id")
                .or_else(|| payload.get("id"))
                .and_then(Value::as_str)
                .unwrap_or_default();
            let call_id = bounded_id(call_id);
            if !call_id.is_empty() {
                call_names.insert(call_id.clone(), name.clone());
            }
            let input = payload
                .get("arguments")
                .or_else(|| payload.get("input"))
                .map(value_preview)
                .unwrap_or_default();
            vec![pending(
                ts_ms,
                ReplayKind::ToolCall,
                name,
                input,
                (!call_id.is_empty()).then(|| format!("codex:call:{call_id}")),
            )]
        }
        "function_call_output" | "custom_tool_call_output" => {
            let call_id =
                bounded_id(payload.get("call_id").and_then(Value::as_str).unwrap_or_default());
            let name = call_names.get(&call_id).cloned().unwrap_or_else(|| "tool result".into());
            let output = payload.get("output").map(value_preview).unwrap_or_default();
            let failed = payload
                .get("status")
                .and_then(Value::as_str)
                .is_some_and(|status| matches!(status, "failed" | "error" | "cancelled"))
                || payload.get("output").is_some_and(value_reports_failure);
            vec![pending(
                ts_ms,
                if failed { ReplayKind::ToolError } else { ReplayKind::ToolResult },
                name,
                output,
                (!call_id.is_empty()).then(|| format!("codex:result:{call_id}")),
            )]
        }
        _ => Vec::new(),
    }
}

fn parse_claude(value: &Value, call_names: &mut HashMap<String, String>) -> Vec<PendingEvent> {
    let record_type = value.get("type").and_then(Value::as_str).unwrap_or_default();
    if !matches!(record_type, "user" | "assistant" | "system") {
        return Vec::new();
    }
    let ts_ms = timestamp_ms(value);
    let uuid = value.get("uuid").and_then(Value::as_str).map(bounded_id);
    let Some(message) = value.get("message") else {
        let detail = value.get("content").map(value_preview).unwrap_or_default();
        return (!detail.is_empty())
            .then(|| pending(ts_ms, ReplayKind::System, record_type, detail, uuid.clone()))
            .into_iter()
            .collect();
    };
    let role = message.get("role").and_then(Value::as_str).unwrap_or(record_type);
    let Some(content) = message.get("content") else { return Vec::new() };
    if let Some(text) = content.as_str() {
        return vec![pending(
            ts_ms,
            if role == "user" { ReplayKind::User } else { ReplayKind::Assistant },
            role,
            preview(text),
            uuid.clone(),
        )];
    }

    let Some(blocks) = content.as_array() else { return Vec::new() };
    let mut out = Vec::new();
    for (index, block) in blocks.iter().enumerate() {
        let block_type = block.get("type").and_then(Value::as_str).unwrap_or_default();
        let dedup = uuid.as_ref().map(|id| format!("claude:{id}:{index}"));
        match block_type {
            "text" => {
                let detail =
                    block.get("text").and_then(Value::as_str).map(preview).unwrap_or_default();
                if !detail.is_empty() {
                    out.push(pending(
                        ts_ms,
                        if role == "user" { ReplayKind::User } else { ReplayKind::Assistant },
                        role,
                        detail,
                        dedup,
                    ));
                }
            }
            "tool_use" | "server_tool_use" => {
                let name = preview(block.get("name").and_then(Value::as_str).unwrap_or("tool"));
                let call_id =
                    bounded_id(block.get("id").and_then(Value::as_str).unwrap_or_default());
                if !call_id.is_empty() {
                    call_names.insert(call_id, name.clone());
                }
                out.push(pending(
                    ts_ms,
                    ReplayKind::ToolCall,
                    name,
                    block.get("input").map(value_preview).unwrap_or_default(),
                    dedup,
                ));
            }
            "tool_result" | "advisor_tool_result" => {
                let call_id = bounded_id(
                    block.get("tool_use_id").and_then(Value::as_str).unwrap_or_default(),
                );
                let name =
                    call_names.get(&call_id).cloned().unwrap_or_else(|| "tool result".into());
                let failed = block.get("is_error").and_then(Value::as_bool).unwrap_or(false);
                out.push(pending(
                    ts_ms,
                    if failed { ReplayKind::ToolError } else { ReplayKind::ToolResult },
                    name,
                    block.get("content").map(value_preview).unwrap_or_default(),
                    dedup,
                ));
            }
            _ => {}
        }
    }
    out
}

fn pending(
    ts_ms: i64,
    kind: ReplayKind,
    title: impl Into<String>,
    detail: String,
    dedup_key: Option<String>,
) -> PendingEvent {
    PendingEvent { ts_ms, order: 0, kind, title: title.into(), detail, dedup_key }
}

fn timestamp_ms(value: &Value) -> i64 {
    value
        .get("timestamp")
        .and_then(Value::as_str)
        .and_then(|raw| chrono::DateTime::parse_from_rfc3339(raw).ok())
        .map_or(0, |timestamp| timestamp.timestamp_millis())
}

fn message_content(content: Option<&Value>) -> String {
    let Some(content) = content.and_then(Value::as_array) else { return String::new() };
    let mut parts = Vec::new();
    for item in content.iter().take(64) {
        match item.get("type").and_then(Value::as_str) {
            Some("input_text" | "output_text" | "text") => {
                if let Some(text) = item.get("text").and_then(Value::as_str) {
                    parts.push(preview(text));
                }
            }
            Some("input_image") => parts.push("[image]".into()),
            _ => {}
        }
    }
    preview(&parts.join(" "))
}

fn value_preview(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::String(text) => preview(text),
        Value::Array(items) => {
            let joined = items
                .iter()
                .take(64)
                .map(value_preview)
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
                .join(" ");
            preview(&joined)
        }
        Value::Object(object) => {
            if object.contains_key("image_url") {
                return "[image]".into();
            }
            for key in ["text", "content", "message", "output", "stdout", "stderr"] {
                if let Some(value) = object.get(key) {
                    let extracted = value_preview(value);
                    if !extracted.is_empty() {
                        return extracted;
                    }
                }
            }
            preview(&serde_json::to_string(value).unwrap_or_default())
        }
        _ => preview(&value.to_string()),
    }
}

fn bounded_id(raw: &str) -> String {
    raw.chars().take(MAX_ID_CHARS).collect()
}

fn value_reports_failure(value: &Value) -> bool {
    match value {
        Value::Array(items) => items.iter().any(value_reports_failure),
        Value::Object(object) => {
            object.get("success").and_then(Value::as_bool) == Some(false)
                || object
                    .get("status")
                    .and_then(Value::as_str)
                    .is_some_and(|status| matches!(status, "failed" | "error" | "cancelled"))
                || object.values().any(value_reports_failure)
        }
        _ => false,
    }
}

fn preview(raw: &str) -> String {
    let mut out = String::new();
    let mut pending_space = false;
    for ch in raw.chars() {
        if ch.is_whitespace() {
            pending_space = !out.is_empty();
            continue;
        }
        if pending_space {
            out.push(' ');
            pending_space = false;
        }
        if out.chars().count() >= MAX_EVENT_PREVIEW_CHARS {
            out.push('…');
            break;
        }
        out.push(ch);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codex_messages_and_tool_pairs_become_a_trace() {
        let mut names = HashMap::new();
        let user: Value = serde_json::from_str(
            r#"{"timestamp":"2026-08-15T00:00:00Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"fix it"}]}}"#,
        )
        .unwrap();
        let call: Value = serde_json::from_str(
            r#"{"timestamp":"2026-08-15T00:00:01Z","type":"response_item","payload":{"type":"function_call","name":"shell","arguments":"{\"cmd\":\"pwd\"}","call_id":"c1"}}"#,
        )
        .unwrap();
        let result: Value = serde_json::from_str(
            r#"{"timestamp":"2026-08-15T00:00:02Z","type":"response_item","payload":{"type":"function_call_output","call_id":"c1","output":"ok"}}"#,
        )
        .unwrap();
        assert_eq!(parse_codex(&user, &mut names)[0].kind, ReplayKind::User);
        assert_eq!(parse_codex(&call, &mut names)[0].title, "shell");
        let result = parse_codex(&result, &mut names);
        assert_eq!(result[0].kind, ReplayKind::ToolResult);
        assert_eq!(result[0].title, "shell");
    }

    #[test]
    fn claude_tool_errors_keep_their_call_name() {
        let mut names = HashMap::new();
        let call: Value = serde_json::from_str(
            r#"{"timestamp":"2026-08-15T00:00:00Z","type":"assistant","uuid":"a","message":{"role":"assistant","content":[{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"false"}}]}}"#,
        )
        .unwrap();
        let result: Value = serde_json::from_str(
            r#"{"timestamp":"2026-08-15T00:00:01Z","type":"user","uuid":"b","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","is_error":true,"content":"failed"}]}}"#,
        )
        .unwrap();
        parse_claude(&call, &mut names);
        let result = parse_claude(&result, &mut names);
        assert_eq!(result[0].kind, ReplayKind::ToolError);
        assert_eq!(result[0].title, "Bash");
    }

    #[test]
    fn matching_accepts_main_files_subagents_and_codex_rollouts() {
        assert!(path_matches_session(Path::new("/projects/p/s-1.jsonl"), "s-1"));
        assert!(path_matches_session(Path::new("/projects/p/s-1/subagents/a.jsonl"), "s-1"));
        assert!(path_matches_session(
            Path::new("/sessions/rollout-2026-08-15T00-00-00-s-1.jsonl"),
            "s-1"
        ));
        assert!(!path_matches_session(Path::new("/projects/p/other.jsonl"), "s-1"));
    }

    #[test]
    fn previews_are_single_line_and_bounded() {
        assert_eq!(preview("a\n  b\t c"), "a b c");
        let long = "x".repeat(MAX_EVENT_PREVIEW_CHARS + 100);
        assert_eq!(preview(&long).chars().count(), MAX_EVENT_PREVIEW_CHARS + 1);
        assert!(preview(&long).ends_with('…'));
    }

    #[test]
    fn streaming_loader_orders_events_and_ignores_a_torn_tail() {
        let nonce =
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let dir =
            std::env::temp_dir().join(format!("readout-replay-{}-{nonce}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("rollout-s1.jsonl");
        let transcript = concat!(
            r#"{"timestamp":"2026-08-15T00:00:01Z","type":"response_item","payload":{"type":"function_call","name":"shell","arguments":"pwd","call_id":"c1"}}"#,
            "\n",
            r#"{"timestamp":"2026-08-15T00:00:00Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"start"}]}}"#,
            "\n",
            r#"{"timestamp":"2026-08-15T00:00:02Z","type":"response_item","payload":{"type":"function_call_output","call_id":"c1","output":"ok"}}"#,
            "\n",
            r#"{"timestamp":"2026-08-15T00:00:03Z","type":"response_item""#
        );
        std::fs::write(&path, transcript).unwrap();
        let request = ReplayRequest {
            source: Source::Codex,
            session: "s1".into(),
            project: "/work/demo".into(),
            model: "gpt-test".into(),
        };
        let replay =
            load_targets(request, vec![scan::Target { path, source: Source::Codex }]).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();

        assert_eq!(replay.events.len(), 3);
        assert_eq!(replay.events[0].kind, ReplayKind::User);
        assert_eq!(replay.events[1].kind, ReplayKind::ToolCall);
        assert_eq!(replay.events[2].kind, ReplayKind::ToolResult);
        assert_eq!(replay.events[2].title, "shell");
        assert_eq!(replay.events[2].offset_ms, 2_000);
    }
}
