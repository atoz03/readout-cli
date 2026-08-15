//! Cost estimation.
//!
//! Rates are USD per million tokens. Anthropic rates are first-party list
//! prices; cache rates are derived from the published prompt-caching
//! multipliers rather than stored separately:
//!
//! * cache read  = 0.10x base input
//! * cache write = 1.25x base input (5-minute TTL, the default)
//! * cache write = 2.00x base input (1-hour TTL)
//!
//! Models we have no authoritative rate for are **unpriced**, not guessed.
//! Their tokens are still counted everywhere; their cost renders as `—` and
//! is excluded from totals, and [`Priced::coverage`] reports what fraction of
//! tokens the headline cost actually covers. Fabricating a plausible number
//! would be worse than admitting the gap.
//!
//! Users can supply rates for anything (including overriding the built-ins)
//! via `pricing.json` in the state directory — see [`Pricing::load`].

use crate::model::{Tokens, pricing_key};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;

pub const CACHE_READ_MULTIPLIER: f64 = 0.10;
pub const CACHE_WRITE_5M_MULTIPLIER: f64 = 1.25;
pub const CACHE_WRITE_1H_MULTIPLIER: f64 = 2.00;

/// Per-million-token rates for one model.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct Rate {
    pub input: f64,
    pub output: f64,
    /// Overrides the 0.10x derivation when present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read: Option<f64>,
    /// Overrides the 1.25x derivation when present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write_5m: Option<f64>,
    /// Overrides the 2.00x derivation when present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write_1h: Option<f64>,
}

impl Rate {
    pub const fn new(input: f64, output: f64) -> Self {
        Rate { input, output, cache_read: None, cache_write_5m: None, cache_write_1h: None }
    }

    /// A rate for a provider that does not bill cache writes at all.
    ///
    /// Without this the 1.25x derivation applies and the rate table advertises
    /// a fee that cannot be charged.
    const fn no_cache_write(input: f64, output: f64) -> Self {
        Rate {
            input,
            output,
            cache_read: None,
            cache_write_5m: Some(0.0),
            cache_write_1h: Some(0.0),
        }
    }

    /// The rate actually charged, derivation or override. Anything displaying
    /// a cache rate must go through these, or it shows the derivation for a
    /// model that overrode it.
    pub fn cache_read_rate(&self) -> f64 {
        self.cache_read.unwrap_or(self.input * CACHE_READ_MULTIPLIER)
    }

    pub fn cache_write_5m_rate(&self) -> f64 {
        self.cache_write_5m.unwrap_or(self.input * CACHE_WRITE_5M_MULTIPLIER)
    }

    fn cache_write_1h_rate(&self) -> f64 {
        self.cache_write_1h.unwrap_or(self.input * CACHE_WRITE_1H_MULTIPLIER)
    }

    /// Cost in USD for the given token counts.
    pub fn cost(&self, t: &Tokens) -> f64 {
        let per = |n: u64, rate: f64| (n as f64) * rate / 1_000_000.0;
        per(t.input, self.input)
            + per(t.output, self.output)
            + per(t.cache_read, self.cache_read_rate())
            + per(t.cache_write_5m, self.cache_write_5m_rate())
            + per(t.cache_write_1h, self.cache_write_1h_rate())
    }

    fn validate(&self, model: &str) -> Result<()> {
        let fields = [
            ("input", Some(self.input)),
            ("output", Some(self.output)),
            ("cache_read", self.cache_read),
            ("cache_write_5m", self.cache_write_5m),
            ("cache_write_1h", self.cache_write_1h),
        ];
        for (field, value) in fields {
            if let Some(value) = value
                && (!value.is_finite() || value < 0.0)
            {
                anyhow::bail!(
                    "invalid {field} rate for `{model}`: expected a finite non-negative number"
                );
            }
        }
        Ok(())
    }

    /// `pricing --init` writes unknown models as zeroed placeholders. They are
    /// deliberately not rates: merely creating the starter file must not turn
    /// an unknown cost into a confidently reported `$0`.
    fn is_zero_placeholder(&self) -> bool {
        self.input == 0.0
            && self.output == 0.0
            && self.cache_read.unwrap_or(0.0) == 0.0
            && self.cache_write_5m.unwrap_or(0.0) == 0.0
            && self.cache_write_1h.unwrap_or(0.0) == 0.0
    }
}

/// Anthropic first-party list prices (source: bundled `claude-api` skill
/// model table, cached 2026-06-24).
///
/// Claude Sonnet 5 carries a promotional $2/$10 introductory rate through
/// 2026-08-31. We deliberately bill history at the standard $3/$15: applying a
/// promo retroactively across months of transcripts would silently understate
/// spend. Override in `pricing.json` if you want the intro rate.
const CLAUDE_RATES: &[(&str, Rate)] = &[
    ("claude-fable-5", Rate::new(10.00, 50.00)),
    ("claude-mythos-5", Rate::new(10.00, 50.00)),
    ("claude-opus-5", Rate::new(5.00, 25.00)),
    ("claude-opus-4-8", Rate::new(5.00, 25.00)),
    ("claude-opus-4-7", Rate::new(5.00, 25.00)),
    ("claude-opus-4-6", Rate::new(5.00, 25.00)),
    ("claude-sonnet-5", Rate::new(3.00, 15.00)),
    ("claude-sonnet-4-6", Rate::new(3.00, 15.00)),
    ("claude-haiku-4-5", Rate::new(1.00, 5.00)),
];

/// OpenAI rates, as published by cc-switch's bundled `model_pricing` table
/// (read from `~/.cc-switch/cc-switch.db`, 2026-08-14).
///
/// **These are secondhand.** Anthropic's rates above come from a first-party
/// price list; these do not, and neither Anthropic nor OpenAI publishes them
/// in a form this tool can check. They are here because leaving 91% of a
/// Codex-heavy corpus unpriced makes the headline cost useless, and a rate
/// with a stated source beats no rate at all. Override any of them in
/// `pricing.json` if you have better numbers.
///
/// Only base ids are listed: reasoning-effort suffixes (`-high`, `-xhigh`, …)
/// bill at the base model's rate and are folded in by [`pricing_key`].
///
/// Cache reads are the derived 0.10x in every row, which matches the source
/// table exactly. Cache writes are pinned to zero: OpenAI's caching is
/// automatic with no write fee, and the Codex parser never records a
/// cache-write token anyway (`src/parse/codex.rs`), so the 1.25x derivation
/// would only ever surface in the rate table as a charge that does not exist.
/// The source table lists a derived write fee for the gpt-5.6 family and zero
/// for everything older; the zeros are the ones that match how OpenAI bills.
const OPENAI_RATES: &[(&str, Rate)] = &[
    ("gpt-5.6", Rate::no_cache_write(5.00, 30.00)),
    ("gpt-5.6-sol", Rate::no_cache_write(5.00, 30.00)),
    ("gpt-5.6-terra", Rate::no_cache_write(2.50, 15.00)),
    ("gpt-5.6-luna", Rate::no_cache_write(1.00, 6.00)),
    ("gpt-5.5", Rate::no_cache_write(5.00, 30.00)),
    ("gpt-5.4", Rate::no_cache_write(2.50, 15.00)),
    ("gpt-5.4-mini", Rate::no_cache_write(0.75, 4.50)),
    ("gpt-5.4-nano", Rate::no_cache_write(0.20, 1.25)),
    ("gpt-5.3-codex", Rate::no_cache_write(1.75, 14.00)),
    ("gpt-5.2", Rate::no_cache_write(1.75, 14.00)),
    ("gpt-5.2-codex", Rate::no_cache_write(1.75, 14.00)),
    ("gpt-5.1", Rate::no_cache_write(1.25, 10.00)),
    ("gpt-5.1-codex", Rate::no_cache_write(1.25, 10.00)),
    ("gpt-5.1-codex-max", Rate::no_cache_write(1.25, 10.00)),
    ("gpt-5.1-codex-mini", Rate::no_cache_write(1.25, 10.00)),
    ("gpt-5", Rate::no_cache_write(1.25, 10.00)),
    ("gpt-5-codex", Rate::no_cache_write(1.25, 10.00)),
    ("gpt-5-codex-mini", Rate::no_cache_write(1.25, 10.00)),
    ("gpt-5-mini", Rate::no_cache_write(0.25, 2.00)),
    ("gpt-5-nano", Rate::no_cache_write(0.05, 0.40)),
    ("codex-mini", Rate::no_cache_write(0.75, 3.00)),
];

/// Resolved rate table: built-ins plus any user overrides.
///
/// Immutable after construction, so it can be shared across scan threads.
#[derive(Debug, Clone, Default)]
pub struct Pricing {
    rates: HashMap<String, Rate>,
}

impl Pricing {
    pub fn builtin() -> Self {
        let mut rates = HashMap::new();
        for (id, rate) in CLAUDE_RATES.iter().chain(OPENAI_RATES) {
            rates.insert((*id).to_string(), *rate);
        }
        Pricing { rates }
    }

    /// Built-ins merged with `pricing.json`, if that file exists.
    ///
    /// The file is a flat object keyed by model id:
    /// `{ "gpt-5.2": { "input": 1.25, "output": 10.0 } }`
    pub fn load(override_path: Option<&Path>) -> Result<Self> {
        let mut pricing = Pricing::builtin();
        let Some(path) = override_path else { return Ok(pricing) };
        if !path.exists() {
            return Ok(pricing);
        }
        let text =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let overrides: HashMap<String, Rate> = serde_json::from_str(&text).with_context(|| {
            format!("parsing {} (expected {{model: {{input, output}}}})", path.display())
        })?;
        for (model, rate) in overrides {
            pricing.apply_override(model, rate)?;
        }
        Ok(pricing)
    }

    fn apply_override(&mut self, model: String, rate: Rate) -> Result<()> {
        rate.validate(&model)?;
        if rate.is_zero_placeholder() {
            if self.rates.contains_key(&model) {
                anyhow::bail!(
                    "zeroed rate for built-in model `{model}` is ambiguous; remove the row to use the built-in rate"
                );
            }
            // Unknown starter rows remain unpriced until the user fills them.
            return Ok(());
        }
        self.rates.insert(model, rate);
        Ok(())
    }

    pub fn rate(&self, model: &str) -> Option<Rate> {
        let key = pricing_key(model);
        self.rates.get(model).or_else(|| self.rates.get(key)).copied()
    }

    pub fn is_priced(&self, model: &str) -> bool {
        self.rate(model).is_some()
    }

    /// Cost for a model's tokens. `None` means "we have no rate", which the
    /// caller must render as unknown rather than as zero.
    pub fn cost(&self, model: &str, tokens: &Tokens) -> Option<f64> {
        self.rate(model).map(|rate| rate.cost(tokens))
    }

    /// Of the models actually observed, those we have no rate for.
    pub fn unpriced_among<'a, I: IntoIterator<Item = &'a str>>(&self, observed: I) -> Vec<String> {
        let mut set: std::collections::BTreeSet<String> = Default::default();
        for m in observed {
            if !self.is_priced(m) {
                set.insert(m.to_string());
            }
        }
        set.into_iter().collect()
    }

    pub fn known_models(&self) -> Vec<(String, Rate)> {
        let mut v: Vec<_> = self.rates.iter().map(|(k, r)| (k.clone(), *r)).collect();
        v.sort_by(|a, b| a.0.cmp(&b.0));
        v
    }

    /// Emit a starter override file listing every model in `models`, with
    /// built-in rates filled in and unknown ones zeroed for the user to edit.
    pub fn starter_override(&self, models: &[String]) -> Result<String> {
        let rows: std::collections::BTreeMap<_, _> = models
            .iter()
            .map(|model| {
                let rate = self.rate(model).unwrap_or(Rate::new(0.0, 0.0));
                (model.clone(), rate)
            })
            .collect();
        let mut out = serde_json::to_string_pretty(&rows).context("serializing pricing starter")?;
        out.push('\n');
        Ok(out)
    }
}

/// A cost figure plus how much of the underlying token volume it accounts for.
///
/// Reporting a bare dollar amount over a corpus with unpriced models would
/// read as complete when it isn't. Every headline cost carries its coverage.
#[derive(Debug, Clone, Copy, Default)]
pub struct Priced {
    pub cost: f64,
    pub priced_tokens: u64,
    pub unpriced_tokens: u64,
}

impl Priced {
    pub fn add(&mut self, other: &Priced) {
        self.cost += other.cost;
        self.priced_tokens += other.priced_tokens;
        self.unpriced_tokens += other.unpriced_tokens;
    }

    pub fn total_tokens(&self) -> u64 {
        self.priced_tokens + self.unpriced_tokens
    }

    /// Fraction of tokens the cost covers, in 0.0..=1.0. Empty input is
    /// fully covered by convention.
    pub fn coverage(&self) -> f64 {
        let total = self.total_tokens();
        if total == 0 { 1.0 } else { self.priced_tokens as f64 / total as f64 }
    }

    pub fn is_complete(&self) -> bool {
        self.unpriced_tokens == 0
    }
}

/// Price one model's tokens into a [`Priced`].
pub fn price(pricing: &Pricing, model: &str, tokens: &Tokens) -> Priced {
    match pricing.cost(model, tokens) {
        Some(cost) => Priced { cost, priced_tokens: tokens.total(), unpriced_tokens: 0 },
        None => Priced { cost: 0.0, priced_tokens: 0, unpriced_tokens: tokens.total() },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_rates_derive_from_the_input_rate() {
        let r = Rate::new(5.00, 25.00);
        assert_eq!(r.cache_read_rate(), 0.50);
        assert_eq!(r.cache_write_5m_rate(), 6.25);
        assert_eq!(r.cache_write_1h_rate(), 10.00);
    }

    #[test]
    fn cost_sums_every_token_class() {
        let r = Rate::new(5.00, 25.00);
        let t = Tokens {
            input: 1_000_000,
            output: 1_000_000,
            cache_read: 1_000_000,
            cache_write_5m: 1_000_000,
            cache_write_1h: 1_000_000,
        };
        // 5 + 25 + 0.50 + 6.25 + 10.00
        assert!((r.cost(&t) - 46.75).abs() < 1e-9);
    }

    #[test]
    fn openai_rates_pin_cache_writes_to_zero_and_fold_effort_suffixes() {
        let p = Pricing::builtin();
        let r = p.rate("gpt-5.4").expect("a built-in OpenAI rate");
        // Not the 1.25x derivation: OpenAI charges nothing to write a cache
        // entry, and the rate table must not claim otherwise.
        assert_eq!(r.cache_write_5m_rate(), 0.0);
        assert_eq!(r.cache_read_rate(), 0.25);
        // An effort-suffixed id resolves to the same rate rather than falling
        // through to unpriced.
        assert_eq!(p.rate("gpt-5.4-xhigh").map(|r| r.input), Some(2.50));
        assert!(p.is_priced("gpt-5.2-codex-high"));
    }

    #[test]
    fn unknown_models_are_unpriced_not_zero() {
        let p = Pricing::builtin();
        let t = Tokens { input: 1_000_000, ..Default::default() };
        assert!(p.cost("some-model-we-never-heard-of", &t).is_none());
        assert_eq!(
            p.unpriced_among(["some-model-we-never-heard-of", "claude-opus-5", "gpt-5.2"]),
            vec!["some-model-we-never-heard-of".to_string()]
        );

        let priced = price(&p, "some-model-we-never-heard-of", &t);
        assert_eq!(priced.cost, 0.0);
        assert_eq!(priced.unpriced_tokens, 1_000_000);
        assert!(!priced.is_complete());
        assert_eq!(priced.coverage(), 0.0);
    }

    #[test]
    fn dated_claude_ids_resolve_to_the_undated_rate() {
        let p = Pricing::builtin();
        assert!(p.is_priced("claude-haiku-4-5-20251001"));
        assert_eq!(p.rate("claude-haiku-4-5-20251001").unwrap().input, 1.00);
    }

    #[test]
    fn coverage_is_a_token_weighted_fraction() {
        let mut acc = Priced::default();
        acc.add(&Priced { cost: 1.0, priced_tokens: 300, unpriced_tokens: 0 });
        acc.add(&Priced { cost: 0.0, priced_tokens: 0, unpriced_tokens: 100 });
        assert_eq!(acc.total_tokens(), 400);
        assert!((acc.coverage() - 0.75).abs() < 1e-9);
    }

    #[test]
    fn zeroed_unknown_override_rows_remain_unpriced() {
        let mut p = Pricing::builtin();
        p.apply_override("future-model".into(), Rate::new(0.0, 0.0)).unwrap();
        assert!(!p.is_priced("future-model"));
        assert!(p.apply_override("gpt-5.2".into(), Rate::new(0.0, 0.0)).is_err());
    }

    #[test]
    fn invalid_rates_are_rejected() {
        let mut p = Pricing::builtin();
        assert!(p.apply_override("bad".into(), Rate::new(-1.0, 2.0)).is_err());
        assert!(p.apply_override("bad".into(), Rate::new(f64::INFINITY, 2.0)).is_err());
    }

    #[test]
    fn starter_override_escapes_model_names_and_preserves_explicit_cache_rates() {
        let p = Pricing::builtin();
        let text = p.starter_override(&["odd\"model\\name".into(), "gpt-5.2".into()]).unwrap();
        let rows: HashMap<String, Rate> = serde_json::from_str(&text).unwrap();
        assert!(rows.contains_key("odd\"model\\name"));
        assert_eq!(rows["gpt-5.2"].cache_write_5m, Some(0.0));
    }
}
