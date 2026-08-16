# readout

English | [简体中文](README.zh-CN.md)

[![CI](https://github.com/atoz03/readout-cli/actions/workflows/ci.yml/badge.svg)](https://github.com/atoz03/readout-cli/actions/workflows/ci.yml)
[![crates.io](https://img.shields.io/crates/v/readout.svg)](https://crates.io/crates/readout)
[![license](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

A terminal dashboard for Claude Code and Codex usage, sessions, tokens, and
estimated cost.

`readout` reads the local transcripts both tools already write. It provides a
clickable TUI for exploration and text, JSON, and CSV output for scripts. It
never proxies requests, reads API credentials, or modifies Claude Code or Codex
configuration.

```sh
readout                  # open the dashboard
readout summary          # print a summary
readout summary --json   # emit JSON
```

## Features

- Break down Claude Code and Codex usage by date, model, project, session, and hour
- View today, the last 7/30/90 days, or all recorded history
- Estimate model cost while clearly marking unpriced usage
- Replay messages and tool calls from local sessions on a timeline
- Aggregate multiple machines over SSH without double-counting copied transcripts
- Incrementally scan transcripts and keep the dashboard live at low overhead
- Export JSON, CSV, and fixed-size dashboard snapshots with consistent CLI filters

## Installation

Linux and macOS:

```sh
curl -fsSL https://github.com/atoz03/readout-cli/releases/latest/download/install.sh | sh
```

Windows:

```powershell
powershell -c "irm https://github.com/atoz03/readout-cli/releases/latest/download/install.ps1 | iex"
```

The installers download a prebuilt binary for the current platform and verify
its published SHA-256 checksum. Their default destinations are:

- Linux/macOS: `~/.local/bin`
- Windows: `%LOCALAPPDATA%\Programs\readout`

They do not edit `PATH`. If the destination is not already available, the
installer prints the line needed to add it. Set `READOUT_INSTALL_DIR` to choose
another directory or `READOUT_VERSION` to pin a release.

Update an existing installation with:

```sh
readout update
```

If Rust is installed, readout is also available from crates.io:

```sh
cargo install readout
```

Platform archives and checksums are attached to every
[GitHub Release](https://github.com/atoz03/readout-cli/releases/latest).

## Quick start

```sh
readout                 # all recorded history
readout -d 30           # the last 30 days
readout -s claude       # Claude Code only
readout -s codex        # Codex only
readout -w              # refresh incrementally every few seconds
```

The same filters apply to the dashboard and all reporting commands:

| Option | Meaning |
|---|---|
| `-d, --days N` | Limit the view to the last N days |
| `-s, --source claude\|codex` | Include only one tool |
| `-p, --project PATH` | Include one full project path |
| `-m, --model MODEL` | Include one model |
| `--no-cache` | Ignore the incremental cache and parse everything |

## Dashboard

The pages move from high-level totals to details, sessions, devices, and
configuration:

| Page | What it shows |
|---|---|
| Overview | Tokens, estimated cost, requests, sessions, and recent trends |
| Daily | Usage by day |
| Models | Model distribution and pricing coverage |
| Projects | Projects, with navigation into their sessions |
| Sessions / Replay | Session usage and local message/tool timelines |
| Devices | The local machine, configured SSH devices, and sync state |
| Pricing | Effective model prices |
| Settings | Aggregation, local identity, devices, aliases, and config paths |

Sidebar entries, filters, rows, card headers, and Replay events are clickable.
The essential keyboard controls are:

| Key | Action |
|---|---|
| `↑/↓`, `j/k` | Move the selection |
| `Enter` | Open or confirm the selected row |
| `Esc` | Go back, clear a filter, or cancel confirmation |
| `Tab` / `Shift-Tab`, `←/→` | Change page |
| `t`, `1/2/3/4` | Today, 7 days, 30 days, 90 days, all time |
| `c/x` | Toggle Claude Code / Codex |
| `r` | Sync devices on Devices; rescan locally elsewhere |
| `w` | Toggle watch mode |
| `u` twice | Update the selected configured remote |
| `Delete` / `Backspace` | Remove the selected SSH device |
| `?` | Show contextual help |
| `q`, `Ctrl-C` | Quit |

Replay starts paused. Press `Space` to play or pause, `1/2/4` to change speed,
and `[`/`]` to move one event at a time. Remote bundles contain no message
content, so remote sessions are usage-only; full Replay is available only where
the transcript itself exists.

## Multiple devices

readout uses OpenSSH to run `readout export` on remote machines. Exported
bundles contain usage metadata only—never prompts, assistant messages, tool
arguments, tool results, passwords, or keys.

### Add a device

1. Run `ssh HOST` once to accept the host key and verify authentication.
2. Open Devices and select `Add SSH device…`.
3. Search the hosts from `~/.ssh/config`, or type a hostname directly.
4. If readout is already installed remotely, press `Enter` to validate and add it.
5. If it is missing, press `Ctrl-U` twice in the picker to install it.
6. Return to Devices and press `r` to sync all configured machines.

The main list contains only devices you explicitly add. SSH usernames, ports,
identity files, and jump hosts remain under OpenSSH's control; readout does not
copy or reinterpret them.

Non-interactive SSH sessions do not always load the remote shell profile.
readout checks `PATH` first, then the official installers' per-user defaults:

- Unix/macOS: `$HOME/.local/bin/readout`
- Windows: `%LOCALAPPDATA%\Programs\readout\readout.exe`

Press `u` twice on a configured remote to update it. DNS, authentication, host
key, and timeout failures are reported directly and never mistaken for a
missing installation.

### Device names and project paths

Each machine has a stable device ID and an editable display name. Renaming a
machine does not create a new device:

```sh
readout device rename workstation
```

When one project has different paths across operating systems, map each exact
path to a shared name on the central machine:

```sh
readout project-alias set readout-cli /mnt/work/readout-cli
readout project-alias set readout-cli 'C:\work\readout-cli'
```

If the same transcript appears on multiple machines, readout counts it once.
Copied events that cannot be attributed to a single machine appear under the
`Shared` device bucket.

## CLI

```text
readout summary [--json|--csv] [--timing]
readout models
readout projects
readout daily [--json|--csv]
readout pricing [--init]
readout refresh [--clear]
readout snapshot [--width N] [--height N] [--page PAGE]

readout sync
readout update
readout device list
readout device add HOST
readout device remove HOST
readout device rename NAME

readout project-alias set NAME PATH
readout project-alias remove PATH
readout project-alias list
```

`summary --json` includes device coverage so scripts can distinguish newly
synced history from newly generated usage. `summary --csv` and `daily --csv`
include cost coverage, preventing partial estimates from looking complete.

`snapshot` renders one dashboard frame at a fixed size without entering an
interactive terminal mode:

```sh
readout snapshot --width 120 --height 40 --page devices
```

## Cost estimates

Cost is calculated from the built-in model price table and should be treated as
an estimate, not an invoice. When Codex is used through a ChatGPT subscription,
the displayed value estimates equivalent API pricing rather than the
subscription charge.

- `$12.34` — every token in the total has a known price
- `$12.34+` — some tokens are unpriced, so the value is a lower bound
- `—` — none of the usage in this row can be priced

Inspect prices or create an override file with:

```sh
readout pricing
readout pricing --init
```

Override rates are USD per million tokens and take precedence over built-in
values. Unknown models remain unpriced rather than being treated as free.

## Data and privacy

By default, readout scans only:

- `~/.claude/projects/**/*.jsonl`
- `~/.codex/sessions/**/*.jsonl`

Codex `archived_sessions/` is intentionally excluded. Session Replay reads
message content only when a specific session is opened.

readout never opens `~/.claude/settings.json`, `~/.codex/config.toml`, or
`~/.codex/auth.json`, and it never modifies either tool's configuration. Its
only persistent data is its own settings, usage cache, price overrides, and
remote usage snapshots.

| Environment variable | Purpose |
|---|---|
| `READOUT_CLAUDE_DIR` | Override the Claude transcript directory |
| `READOUT_CODEX_DIR` | Override the Codex transcript directory |
| `READOUT_STATE_DIR` | Override cache, remote snapshot, and price data |
| `READOUT_CONFIG_DIR` | Override the readout settings directory |
| `READOUT_SSH_CONFIG` | Use a specific SSH config for discovery and connections |

The incremental cache stores usage metadata only, never prompts, responses, or
tool content. `readout refresh` rebuilds it; `readout refresh --clear` only
removes it.

## Accounting model

Claude Code responses are deduplicated by message ID. Codex cumulative usage is
converted into per-request deltas. The parsers normalize the tools' different
treatment of cached input, and sub-agent and Workflow transcripts are included.

These rules prevent known forms of duplication and cumulative over-counting,
but the result remains a local transcript-based view rather than a vendor bill.

## Development

```sh
cargo test --all-targets
cargo clippy --all-targets -- -D warnings
cargo fmt --all --check
```

The codebase uses Rust 2024 edition. Releases build binaries for Linux x86-64
and arm64, macOS Intel and Apple Silicon, and Windows x86-64.

## License

[MIT](LICENSE)
