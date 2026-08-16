# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

`readout` reads the JSONL transcripts Claude Code and Codex already write, and
reports tokens, requests, sessions and estimated cost — as a mouse-driven TUI
with a plain CLI behind it. Repository is `readout-cli`; crate and binary are
both `readout`. Rust 2024 edition, let-chains, single binary, no `tests/` dir —
every test is a `#[cfg(test)] mod tests` next to the code it covers.

## Commands

```sh
cargo build
cargo run -- summary --json               # scriptable output
cargo test --all-targets                  # ~140 tests, about a second
cargo test dedup                          # one test / substring
cargo test --release                      # see below — not redundant
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
```

`cargo test --release` is a separate obligation, not a repeat: the frame-budget
test (`a_frame_costs_far_less_than_the_frame_budget`, in `src/tui/mod.rs`) only
asserts when optimized, because a debug timing
measured against a runtime budget measures the profile rather than the code. A
debug-only run passes it silently.

CI pins clippy to a **fixed toolchain (1.97.1)** on purpose — a floating
`stable` turns `-D warnings` into a build that breaks on an untouched
repository. Bump the pin in `.github/workflows/ci.yml` and fix the new lints in
that same commit.

### Seeing the TUI without a terminal

The dashboard refuses to run when stdout is not a tty. To inspect layout —
which is the only way an agent can — render one settled frame as text:

```sh
cargo run -- snapshot --width 120 --height 40 --page overview
# pages: overview | daily | models | projects | sessions | devices | pricing | settings
```

`snapshot` and `summary --json` are also CI's cross-platform smoke tests.

### Testing against fixtures, not your real data

These env vars redirect everything the binary touches, so parser and cache work
can run against fixture transcripts and an isolated cache. All of them, not a
subset: every subcommand loads settings, so leaving `READOUT_CONFIG_DIR` unset
writes a real `~/.config/readout/settings.json` on the machine running the test.

| Var | Redirects |
|---|---|
| `READOUT_CLAUDE_DIR` | the `~/.claude/projects` tree |
| `READOUT_CODEX_DIR` | the `~/.codex/sessions` tree |
| `READOUT_STATE_DIR` | the cache, remote snapshots and `pricing.json` (default `~/.cache/readout/`) |
| `READOUT_CONFIG_DIR` | `settings.json` (default `~/.config/readout/`) |
| `READOUT_SSH_CONFIG` | the `~/.ssh/config` the Devices page reads Host aliases from |

## Architecture

One pipeline, and every view is a projection of its output:

```
discover      scan.rs          walk the two transcript trees
   ↓
parse         parse/claude.rs  } bytes + ParseCursor → events, per file, in parallel (rayon)
              parse/codex.rs   }
   ↓
UsageEvent    model.rs         flat stream, one per billed request
   ↓
dedup         scan.rs          collapse responses duplicated across forked transcripts
   ↓
merge         devices.rs       fold in synced remote bundles, tag `observed_on`
   ↓
summarize()   agg.rs           pure fn of (events, Filter, Pricing) → Summary
   ↓
render        report.rs (text/JSON/CSV)  |  tui/ (dashboard)
```

Because `summarize` is a pure function re-run on every filter change, the pages
cannot disagree with each other. Add a view by deriving it in `agg.rs`, not by
accumulating state alongside an existing one.

**Multi-device merge** (`devices.rs`). `load_usage` is the only entry point the
CLI and the TUI both scan through. With no SSH host enabled it skips the merge
entirely and just tags `observed_on` — the cross-device index would allocate a
key per event on every 5s watch tick to compare against nothing. With hosts
enabled it folds each `~/.cache/readout/remotes/*.json` snapshot in by content
addressed id, so a transcript present on two machines is billed once and lands
in the `@shared` bucket rather than being attributed to either.

Those snapshots are cache, and fail like cache: one that will not parse, or that
claims the local device id, takes its own row out (`DeviceRecord::problem`) and
is reported through `LoadedUsage::warnings`. It must never take the local numbers
with it — `load_usage` returning `Err` means every subcommand stops working.

**Incremental cache** (`cache.rs`). Each file carries a watermark: identity
(dev+inode; creation time on Windows), size, mtime, the byte offset parsed so
far, the `ParseCursor` at that point, and the events found. A grown file is
re-read from the offset; a shrunk or replaced one is reparsed whole. The cache
holds every parsed event, so writing it costs the whole corpus — hence
`cache_changed()`: a scan that learned nothing writes nothing, which is what
makes `--watch` (5s rescans) reasonable.

**Parser contract** (`parse/mod.rs`). Return the events plus how many bytes were
*fully* consumed — always ending on a newline. A transcript being appended to
while we read it has a torn final line; leaving it unconsumed means the next
scan picks it up whole rather than dropping it.

**Reconciliations that the numbers depend on.** Both tools write more records
than they bill for, differently:

- Claude repeats responses verbatim into forked transcripts → dedup by
  `message.id`, higher `dedup_rank` wins (finished beats mid-stream), then
  larger output count.
- Codex writes *cumulative* snapshots and re-emits them on rate-limit refresh →
  per-turn deltas come from `last_token_usage` where present, high-water
  subtraction otherwise, never from summing the cumulative figure.
- Codex counts cached tokens inside `input_tokens`; Claude does not. The overlap
  is subtracted at parse time so `Tokens::input` means "fresh input" everywhere
  downstream.
- Subagent and workflow transcripts are included
  (`<project>/<session>/subagents/workflows/wf_*/`). Dropping that depth makes
  every token spent inside a `Task` or `Workflow` vanish.

**TUI** (`tui/`). The scan runs on a background thread streaming progress into
the event loop, so the first frame paints immediately. The loop redraws only
when something changed — an idle dashboard sends zero bytes, which is the point
over ssh. ratatui keeps no scene graph, so `widgets.rs` draws and registers
clickable rects in the same call (`hit.rs`); the registry is rebuilt each frame,
so a region that stops being drawn stops being clickable. `anim.rs` eases
display values only — the underlying numbers are always real.

## Invariants

**Read-only, by construction.** Transcripts: only `~/.claude/projects/**` and
active `~/.codex/sessions/**` are opened; `archived_sessions/` is intentionally
out of scope. `~/.claude/settings.json`, `~/.codex/config.toml` and
`~/.codex/auth.json` hold live credentials and are never touched. Opening the
Devices page additionally reads `~/.ssh/config` and its `Include` files, and
takes nothing from them but concrete `Host` aliases — no `HostName`, no
`IdentityFile`, no `ProxyCommand`, and nothing is written back. Writes go to two
dirs of readout's own: the state dir (cache, remote snapshots, price overrides)
and the config dir (`settings.json`). Widening read scope breaks the product's
central promise — `paths.rs` is the chokepoint where that is enforced, and any
new path belongs there and in the table above, not inlined at a call site.

**Bump `cache::SCHEMA_VERSION`** whenever `UsageEvent`, `ParseCursor`, or parser
semantics change meaning. Stale caches are discarded and rebuilt, never
migrated — forgetting the bump means users silently keep wrong events. The same
rule covers `devices::BUNDLE_SCHEMA_VERSION`: bump it when the bundle changes
meaning, and rely on unreadable snapshots degrading per device, because they are
never migrated either.

**Unpriced is not free.** A model with no rate has its tokens counted and its
cost excluded, and every figure containing it is marked (`$x+`, `—`, `<1% of
tokens excluded`). Never fabricate a plausible rate to make a total look clean;
see `Priced::coverage` in `pricing.rs`.

Dates and hours bucket in **local time** — "when do I work" is a question about
the user's day, not UTC's.

## Style

`rustfmt.toml` sets `max_width = 100` and `use_small_heuristics = "Max"` so
table-shaped expressions (layout constraints, format strings, rate tables) stay
on one line instead of exploding to one argument per line.

Match the prose conventions already in the tree: module-level `//!` docs
explaining *why* the module exists, sentence-length comments on decisions rather
than restatements of the code, and full-sentence test names
(`a_finished_copy_beats_a_mid_stream_copy`,
`a_scan_that_learned_nothing_leaves_the_cache_alone`).
