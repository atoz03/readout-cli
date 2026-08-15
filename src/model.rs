//! Core data model.
//!
//! Everything the scanners produce collapses into a flat stream of
//! [`UsageEvent`]s — one per billed API response. Aggregation is a pure
//! function of that stream, so every view (day, model, project, hour) is
//! derived rather than accumulated separately.

use serde::{Deserialize, Serialize};
use std::fmt;

/// Which coding tool produced the event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, PartialOrd, Ord)]
#[serde(rename_all = "lowercase")]
pub enum Source {
    Claude,
    Codex,
}

impl Source {
    pub const ALL: [Source; 2] = [Source::Claude, Source::Codex];

    pub fn label(self) -> &'static str {
        match self {
            Source::Claude => "Claude Code",
            Source::Codex => "Codex",
        }
    }

    pub fn short(self) -> &'static str {
        match self {
            Source::Claude => "claude",
            Source::Codex => "codex",
        }
    }

    pub fn parse(s: &str) -> Option<Source> {
        match s.to_ascii_lowercase().as_str() {
            "claude" | "cc" | "claude-code" => Some(Source::Claude),
            "codex" | "cx" => Some(Source::Codex),
            _ => None,
        }
    }
}

impl fmt::Display for Source {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.short())
    }
}

/// Token counts for a single billed request.
///
/// Cache writes are split by TTL because they are priced differently
/// (1.25x base input for the 5-minute default, 2x for the 1-hour tier).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tokens {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    /// Cache-creation tokens written with the 5-minute TTL.
    pub cache_write_5m: u64,
    /// Cache-creation tokens written with the 1-hour TTL.
    pub cache_write_1h: u64,
}

impl Tokens {
    pub fn cache_write(&self) -> u64 {
        self.cache_write_5m.saturating_add(self.cache_write_1h)
    }

    /// Every token the request was billed for, cached or not.
    pub fn total(&self) -> u64 {
        self.input
            .saturating_add(self.output)
            .saturating_add(self.cache_read)
            .saturating_add(self.cache_write())
    }

    pub fn is_empty(&self) -> bool {
        self.total() == 0
    }

    pub fn add(&mut self, other: &Tokens) {
        self.input = self.input.saturating_add(other.input);
        self.output = self.output.saturating_add(other.output);
        self.cache_read = self.cache_read.saturating_add(other.cache_read);
        self.cache_write_5m = self.cache_write_5m.saturating_add(other.cache_write_5m);
        self.cache_write_1h = self.cache_write_1h.saturating_add(other.cache_write_1h);
    }
}

impl std::ops::AddAssign<&Tokens> for Tokens {
    fn add_assign(&mut self, rhs: &Tokens) {
        self.add(rhs);
    }
}

/// One billed request, normalized across both tools.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsageEvent {
    pub source: Source,
    /// Unix timestamp in seconds. Zero when the record carried no usable time.
    pub ts: i64,
    /// Raw model id as written by the tool, after alias normalization.
    pub model: String,
    /// Session / thread identifier.
    pub session: String,
    /// Full working directory used as an unambiguous project identity.
    pub project: String,
    pub tokens: Tokens,
    /// Stable id used to drop duplicates seen across forked transcripts.
    /// `None` when the format offers nothing to dedup on.
    pub dedup_key: Option<String>,
    /// Rank hint for the dedup replace rule — a later record with a higher
    /// rank wins. See `parse::claude` for the derivation.
    #[serde(default)]
    pub dedup_rank: u8,
}

/// Normalize the bare aliases the tools sometimes emit into full model ids.
///
/// Claude Code writes `sonnet` / `opus` when a session was started with the
/// short alias flag. Leaving them unnormalized splits one model across two
/// rows and leaves the alias unpriced.
pub fn normalize_model(raw: &str, source: Source) -> String {
    let m = raw.trim();
    if m.is_empty() {
        return "unknown".to_string();
    }
    if source == Source::Claude {
        match m {
            "opus" => return "claude-opus-5".to_string(),
            "sonnet" => return "claude-sonnet-5".to_string(),
            "haiku" => return "claude-haiku-4-5".to_string(),
            _ => {}
        }
    }
    m.to_string()
}

/// Reasoning-effort suffixes Codex appends to a model id. They select how hard
/// the model thinks, not what it costs, so all of them bill at the base rate.
///
/// Every one is an effort level and none is a model tier: `-mini`, `-nano` and
/// `-max` name real models (`gpt-5-mini`, `gpt-5.1-codex-max`) and are absent
/// here on purpose. `-minimal` is the effort level and does not collide with
/// `-mini`, because the match is on the whole suffix.
const EFFORT_SUFFIXES: [&str; 5] = ["-minimal", "-low", "-medium", "-high", "-xhigh"];

/// Trim a Claude `-YYYYMMDD` date suffix or a Codex reasoning-effort suffix, so
/// every id of the same model shares one price row. Charts still bucket by the
/// raw model id — this is a pricing lookup, not a display grouping.
pub fn pricing_key(model: &str) -> &str {
    if let Some((head, date)) = model.rsplit_once('-')
        && !head.is_empty()
        && date.len() == 8
        && date.bytes().all(|b| b.is_ascii_digit())
    {
        return head;
    }
    for suffix in EFFORT_SUFFIXES {
        if let Some(head) = model.strip_suffix(suffix)
            && !head.is_empty()
        {
            return head;
        }
    }
    model
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_totals() {
        let t =
            Tokens { input: 1, output: 2, cache_read: 4, cache_write_5m: 8, cache_write_1h: 16 };
        assert_eq!(t.cache_write(), 24);
        assert_eq!(t.total(), 31);
    }

    #[test]
    fn hostile_counts_saturate_instead_of_wrapping() {
        let mut t = Tokens { input: u64::MAX, ..Default::default() };
        t.add(&Tokens { input: 1, output: u64::MAX, ..Default::default() });
        assert_eq!(t.input, u64::MAX);
        assert_eq!(t.total(), u64::MAX);
    }

    #[test]
    fn reasoning_effort_folds_onto_the_base_model_but_a_tier_does_not() {
        assert_eq!(pricing_key("gpt-5.6-high"), "gpt-5.6");
        assert_eq!(pricing_key("gpt-5.1-codex-max-xhigh"), "gpt-5.1-codex-max");
        assert_eq!(pricing_key("gpt-5.5-minimal"), "gpt-5.5");
        // Tiers are separate models at separate prices, and must survive.
        assert_eq!(pricing_key("gpt-5-mini"), "gpt-5-mini");
        assert_eq!(pricing_key("gpt-5.4-nano"), "gpt-5.4-nano");
        assert_eq!(pricing_key("gpt-5.1-codex-max"), "gpt-5.1-codex-max");
        // A dated Claude id still trims its date.
        assert_eq!(pricing_key("claude-opus-4-6-20260206"), "claude-opus-4-6");
    }

    #[test]
    fn aliases_normalize_only_for_claude() {
        assert_eq!(normalize_model("opus", Source::Claude), "claude-opus-5");
        assert_eq!(normalize_model("sonnet", Source::Claude), "claude-sonnet-5");
        // Codex model ids are passed through untouched.
        assert_eq!(normalize_model("gpt-5.2-codex", Source::Codex), "gpt-5.2-codex");
        assert_eq!(normalize_model("  ", Source::Codex), "unknown");
    }

    #[test]
    fn date_suffix_is_stripped_for_pricing() {
        assert_eq!(pricing_key("claude-haiku-4-5-20251001"), "claude-haiku-4-5");
        assert_eq!(pricing_key("claude-opus-5"), "claude-opus-5");
        assert_eq!(pricing_key("gpt-5.2"), "gpt-5.2");
        // Not a date: must not be trimmed.
        assert_eq!(pricing_key("model-abcdefgh"), "model-abcdefgh");
        // Transcript fields are external input. Byte-counted ASCII suffix
        // checks must not slice through a multi-byte character.
        assert_eq!(pricing_key("你abcdefgh"), "你abcdefgh");
        assert_eq!(pricing_key("模型-20260815"), "模型");
    }
}
