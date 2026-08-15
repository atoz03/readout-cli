# readout

[![CI](https://github.com/atoz03/readout-cli/actions/workflows/ci.yml/badge.svg)](https://github.com/atoz03/readout-cli/actions/workflows/ci.yml)
[![crates.io](https://img.shields.io/crates/v/readout.svg)](https://crates.io/crates/readout)
[![license](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

A terminal dashboard for what Claude Code and Codex actually cost you.

`readout` reads the transcripts both tools already write, and reports tokens,
requests, sessions and estimated spend — by day, by model, by project, by
session, and by hour of the day. It is a mouse-driven TUI with a plain CLI
behind it for scripts.

```
readout                  # the dashboard
readout summary          # the same numbers, printed
readout summary --json   # …for a script
```

## Read-only, by construction

`readout` opens exactly two directories:

- `~/.claude/projects/**/*.jsonl`
- `~/.codex/sessions/**/*.jsonl` (and `archived_sessions/`)

It never opens `~/.claude/settings.json`, `~/.codex/config.toml`, or
`~/.codex/auth.json`. Those files hold live API credentials, and nothing here
needs them — they are not read, not copied, not backed up. It writes to one
place only, its own cache under `~/.cache/readout/`. Neither tool's
configuration is modified, so there is no way for this to break a working
setup.

Switching providers, editing configs, and proxying requests are deliberately
out of scope. This tool answers "what did I spend", and nothing else.

## Install

```sh
curl -fsSL https://raw.githubusercontent.com/atoz03/readout-cli/main/install.sh | sh
```

Downloads the binary for your platform into `~/.local/bin`, checks it against
the SHA-256 published with the release, and touches nothing else. Set
`READOUT_INSTALL_DIR` to put it elsewhere or `READOUT_VERSION` to pin a version.

From the registry, if you have Rust:

```sh
cargo install readout
```

Or from source:

```sh
git clone https://github.com/atoz03/readout-cli && cd readout-cli
cargo build --release && ./target/release/readout
```

The repository is `readout-cli`; the crate and the command are both `readout`.

**Prebuilt binaries** are attached to every [release]: Linux x86-64 and arm64
(static musl, so the distro doesn't matter), macOS Intel and Apple Silicon, and
Windows x86-64. On Windows, download the `.zip` and put `readout.exe` on your
PATH. Building from source needs a recent stable Rust — the crate is on the
2024 edition and uses let-chains.

[release]: https://github.com/atoz03/readout-cli/releases/latest

## The dashboard

```
readout            # opens on Overview
readout -d 30      # start with the window limited to 30 days
readout -s claude  # one tool only
```

Everything on screen that means something is clickable.

| Mouse | |
|---|---|
| Sidebar entry | switch page |
| `7d` `30d` `90d` `All` | change the time window |
| `● Claude Code` / `● Codex` | include or exclude that tool |
| A row in any list | select it; click again to filter the whole dashboard by it |
| A KPI tile | jump to the page that breaks it down |
| A card header (`›`) | open the full page for that card |
| The scan summary, bottom right | re-scan |
| `✕`, top right | quit |
| Wheel | scroll the focused list |

| Keys | |
|---|---|
| `↑` `↓` / `j` `k` | move the selection (the window scrolls to follow) |
| `PgUp` `PgDn` `Home` `End` | move it faster |
| `Enter` | filter everything by the selected row |
| `Esc` | clear that filter |
| `Tab` / `Shift-Tab`, `←` `→` | change page |
| `1` `2` `3` `4` | 7d / 30d / 90d / all time |
| `c` / `x` | toggle Claude Code / Codex |
| `r` | re-scan |
| `?` | what's clickable |
| `q`, `Ctrl-C` | quit |

At least one tool always stays on: switching the last one off would leave a
dashboard of zeroes rather than an answer.

## The CLI

```
readout summary [--json|--csv] [--timing]
readout models
readout projects
readout daily
readout pricing [--init]
readout refresh [--clear]
readout snapshot [--width N] [--height N] [--page overview|daily|models|projects|sessions|pricing]
```

Filters apply to every subcommand, the dashboard included:

```
-d, --days N        the last N days (default: all time)
-s, --source S      claude or codex
-p, --project P     one project
-m, --model M       one model
    --no-cache      reparse everything, ignoring the incremental cache
```

`snapshot` prints a single dashboard frame at a fixed size, with no terminal
setup and no input — useful in a bug report, a diff, or a layout check.

## Cost, and what `+` means

Prices come from a built-in table, with cache reads at 0.10× input and cache
writes at 1.25× (5-minute TTL) or 2.00× (1-hour TTL).

The two halves of that table have different provenance, and it matters:

- **Anthropic rates are first-party list prices.**
- **OpenAI rates are secondhand** — taken from the `model_pricing` table
  cc-switch ships in `~/.cc-switch/cc-switch.db` (read 2026-08-14). Neither
  vendor publishes them somewhere this tool can verify, so treat Codex cost as
  a good estimate rather than an invoice. Override any row in `pricing.json`.

Reasoning-effort suffixes bill at the base model's rate, so `gpt-5.2-xhigh`
and `gpt-5.2` share a price row. Model *tiers* do not: `gpt-5-mini` and
`gpt-5.1-codex-max` are priced separately, as they should be.

If your Codex usage runs on a ChatGPT plan rather than an API key, none of
those tokens are billed per token at all — the figure is then what the same
work would have cost through the API, not what you paid.

A model with no rate on file is **unpriced, not free**. Its tokens are counted
and its cost is left out, and every figure that includes it is marked:

- `$863.46+` — this is a floor; some tokens in this total had no rate
- `—` — nothing in this row could be priced at all
- `<1% of tokens excluded from cost` — how much of the window the gap covers,
  never rounded down to `0%` while the gap is real

To fill the gap, write your own rates:

```
readout pricing --init      # writes ~/.cache/readout/pricing.json,
                            # pre-filled with every model in your data
```

Known models come out at their built-in rate; unknown ones come out zeroed, so
the ones that need your attention are the ones reading `0.00`. Edit the `input`
/ `output` numbers (USD per million tokens) and they take precedence over the
built-in table on the next run. `--init` will not overwrite a file that already
exists.

Claude Sonnet 5 is billed at the standard $3/$15 rather than the promotional
$2/$10 introductory rate: applying a promo retroactively across months of
history would silently understate what was actually spent.

## Speed

The scan is incremental. Each file is remembered by device+inode, size and
mtime, and a file that only grew is re-read from the byte where the last scan
stopped. On a 1.4 GB corpus of 455 transcripts:

```
cold   455 files, 1.4 GB   →  ~390 ms  (360 ms of it parsing, in parallel)
warm   454 reused, 1 grown →  ~140 ms, 3.3 kB read
```

`readout refresh` rebuilds the cache; `readout refresh --clear` deletes it.
Set `READOUT_STATE_DIR` to move the cache and the price overrides elsewhere.

## Accuracy notes

Both tools write more records than they bill for, in different ways, and the
numbers here are the result of reconciling them:

- **Claude** repeats a response across forked transcripts, so records are
  deduplicated by `message.id`. Where a streamed response reports a
  `cache_creation` TTL breakdown that under-sums its own total, the
  authoritative total is kept and split in the breakdown's proportion.
- **Codex** writes a running cumulative total. Per-turn deltas come from the
  exact `last_token_usage` where present, and from high-water subtraction
  where it is not — never from summing the cumulative figure.
- **Codex counts cached tokens inside `input_tokens`; Claude does not.** The
  overlap is subtracted at parse time, so "input" means the same thing in both
  columns.
- Sub-agent and Workflow transcripts are included. Skipping them would make
  every token spent inside a `Task` or a `Workflow` disappear from the totals.
