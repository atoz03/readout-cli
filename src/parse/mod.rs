//! Transcript parsing.
//!
//! Both parsers share one contract: given the bytes of a file from a byte
//! offset, return the events found plus how many bytes were *fully* consumed
//! (i.e. ended in a newline). A transcript being appended to while we read it
//! will have a torn final line; leaving it unconsumed means the next scan
//! picks it up whole instead of dropping it.

pub mod claude;
pub mod codex;

use crate::model::UsageEvent;
use serde::{Deserialize, Serialize};

/// Per-file state that must survive between incremental scans.
///
/// Claude only needs the project path. Codex needs enough to keep computing
/// deltas correctly when resuming mid-file: the cumulative high-water mark,
/// the per-source snapshot signatures used to suppress re-emitted totals, and
/// the model/thread in effect at the cut point.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ParseCursor {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claude_cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codex: Option<codex::CodexState>,
}

/// Result of parsing one file (or one incremental slice of it).
#[derive(Debug, Default)]
pub struct FileParse {
    pub events: Vec<UsageEvent>,
    /// Bytes consumed from the start of the supplied slice. Always lands on a
    /// newline boundary.
    pub consumed: usize,
    pub cursor: ParseCursor,
    /// Records recognized but deliberately excluded (e.g. `<synthetic>`).
    pub skipped_synthetic: u32,
}

/// Parse an RFC 3339 / ISO 8601 timestamp into unix seconds.
pub fn parse_ts(s: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(s).ok().map(|dt| dt.timestamp())
}

/// Iterate `(start, end)` byte ranges of complete newline-terminated lines.
pub fn complete_lines(bytes: &[u8]) -> impl Iterator<Item = (usize, usize)> + '_ {
    memchr::memchr_iter(b'\n', bytes).scan(0usize, |start, nl| {
        let s = *start;
        *start = nl + 1;
        Some((s, nl))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamps_parse_to_unix_seconds() {
        assert_eq!(parse_ts("2026-08-14T10:00:00.000Z"), Some(1786701600));
        // An explicit offset is honoured rather than assumed to be UTC.
        assert_eq!(parse_ts("2026-08-14T18:00:00+08:00"), Some(1786701600));
        assert_eq!(parse_ts("not a date"), None);
    }

    #[test]
    fn only_newline_terminated_lines_are_yielded() {
        let b = b"a\nbb\nccc";
        let got: Vec<_> = complete_lines(b).map(|(s, e)| &b[s..e]).collect();
        assert_eq!(got, vec![&b"a"[..], &b"bb"[..]]);
    }
}
