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

`readout` 的 transcript 扫描范围只有两个目录：

- `~/.claude/projects/**/*.jsonl`
- `~/.codex/sessions/**/*.jsonl` (active sessions only; `archived_sessions/` is
  intentionally out of scope)

进入 Devices 页面时，它会额外按需读取 `~/.ssh/config` 及其中的 `Include`，只提取
具体的 `Host` 别名；不会读取或保存密码、私钥、`IdentityFile`、`ProxyCommand` 等连接
配置，也不会因为发现设备就建立连接。

It never opens `~/.claude/settings.json`, `~/.codex/config.toml`, or
`~/.codex/auth.json`. Those files hold live API credentials, and nothing here
needs them — they are not read, not copied, not backed up. Usage cache 和远端
usage bundle 写入系统 cache 目录下的 `readout/`，持久化设置写入系统 config 目录下的
`readout/settings.json`（Linux 通常分别是 `~/.cache/readout/` 与
`~/.config/readout/`）。Neither tool's configuration is modified, so there is
no way for this to break a working setup.

Session Replay 只在用户打开具体 session 时按需读取消息和工具调用。正文不会写入
`~/.cache/readout/`；增量缓存仍然只保存 usage 元数据。

Switching providers, editing configs, and proxying requests are deliberately
out of scope. This tool answers "what did I spend", and nothing else.

## Install

Linux and macOS:

```sh
curl -fsSL https://github.com/atoz03/readout-cli/releases/latest/download/install.sh | sh
```

Windows:

```powershell
powershell -c "irm https://github.com/atoz03/readout-cli/releases/latest/download/install.ps1 | iex"
```

Either one downloads the binary for your platform — into `~/.local/bin`, or
`%LOCALAPPDATA%\Programs\readout` — checks it against the SHA-256 published with
the release, and touches nothing else. Neither edits your PATH; if the install
directory isn't on it, they print the line that adds it. Set
`READOUT_INSTALL_DIR` to put the binary elsewhere or `READOUT_VERSION` to pin a
version.

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

已安装 release 后可原地更新到最新版：

```sh
readout update
```

它复用同一套官方安装器和 SHA-256 校验，不在二进制里重复维护下载与解包逻辑。

**Prebuilt binaries** are attached to every [release]: Linux x86-64 and arm64
(static musl, so the distro doesn't matter), macOS Intel and Apple Silicon, and
Windows x86-64 — which is what ARM Windows gets too, under emulation, until
there is an arm64 build. To skip the installers, download the archive and put
the binary on your PATH yourself. Building from source needs a recent stable
Rust — the crate is on the 2024 edition and uses let-chains.

[release]: https://github.com/atoz03/readout-cli/releases/latest

## The dashboard

```
readout            # opens on Overview
readout -d 30      # start with the window limited to 30 days
readout -s claude  # one tool only
readout -w         # keep the numbers live while you work
```

`-d` opens on the chip that matches, rounding up to the next one when the
number falls between two — the chips are the only thing naming the active
window, so the dashboard never shows a window its header disagrees with. The
text subcommands take `-d` exactly.

Everything on screen that means something is clickable.

| Mouse | |
|---|---|
| Sidebar entry | switch page |
| `Today` `7d` `30d` `90d` `All` | change the time window |
| `● Claude Code` / `● Codex` | include or exclude that tool |
| A model row | select it; click again to filter the dashboard by it |
| A project row | open that project's sessions |
| A session row | open Session Replay |
| A device row | 首次点击连接并校验；已同步设备则按设备过滤 |
| Devices card header | sync enabled SSH hosts |
| A Settings row | toggle or cycle that persisted setting |
| Replay controls and timeline events | play/pause, change speed, or seek to an event |
| A day or rate row | select it |
| A KPI tile | jump to the page that breaks it down |
| A card header (`›`) | open the full page for that card |
| `● live`, bottom right | start or stop watching |
| The scan summary, bottom right | re-scan |
| `✕`, top right | quit |
| Wheel | scroll the focused list |

| Keys | |
|---|---|
| `↑` `↓` / `j` `k` | move the selection (the window scrolls to follow) |
| `PgUp` `PgDn` `Home` `End` | move it faster |
| `Enter` | filter/open the selected row；Devices 中用于校验并启用 SSH Host |
| `Delete` / `Backspace` | 在 Devices 中禁用选中的 SSH Host |
| `u` | 在 Devices 中升级选中的远端 readout，并重新校验兼容性 |
| `r` | 在 Devices 中异步同步已启用设备；其他页面重新扫描本机 usage |
| `Esc` | clear that filter |
| `Tab` / `Shift-Tab`, `←` `→` | change page |
| `t` / `1` `2` `3` `4` | today / 7d / 30d / 90d / all time |
| `c` / `x` | toggle Claude Code / Codex |
| `r` | re-scan |
| `w` | watch: keep the numbers live |
| `?` | what's clickable |
| `q`, `Ctrl-C` | quit |

### Session Replay

项目和 session 现在是明确的两级导航：点击项目进入该项目过滤后的 Sessions，点击
session 进入独立的 Replay 页面。Replay 会按时间排列用户消息、助手消息、工具调用、
工具结果和失败结果，并显示可点击的时间轴。

Replay 默认暂停在首个事件。`Space` 播放或暂停，`1`/`2`/`4` 切换倍速，`[`/`]`
逐事件移动，`Esc` 返回当前项目的 Sessions。播放位置、列表选中项和时间轴指示器会
同步更新；进入页面时沿用 dashboard 的缓入动画。

远端 bundle 不包含消息或工具正文。远程 session 仍会显示 usage、模型、项目、时间和
设备，但 Replay 页面会明确标记为 usage-only；只有当前设备持有 active transcript 时
才会读取完整 Replay。

## 多设备与 SSH

中心设备默认合并本机 usage 和已经同步的远端 bundle。Bundle 只包含 token、费用所需
字段、时间、模型、项目和 session ID，不包含 prompt、助手消息、工具参数、工具结果或
任何认证信息。

设备来源只使用现有的 SSH config，不再维护第二份远端清单。进入 Devices 页面后，
readout 会从 `~/.ssh/config`（含 `Include` 和通配 include 文件）列出所有具体 `Host` 别名；
`Host *`、`!negated` 等模式不会成为设备。按 `Enter` 后才执行固定命令
`ssh <host-alias> readout export`，校验 bundle schema、设备 ID 和输出边界，成功后才把
这个别名记为启用。SSH 的 User、HostName、端口、密钥和跳板机继续完全交给 OpenSSH。

远端需要先安装 readout，并且 `readout` 必须在非交互 SSH 的 PATH 中。首次 host key
确认仍应由用户正常执行一次 `ssh <host-alias>`。兼容的旧版本可以继续同步，设备行会
显示它的版本；需要升级时选中后按 `u`。还没有 `readout update` 的旧版本需要手动跑
一次最新安装器，之后即可远程升级。

保留的自动化命令只有：

```sh
readout sync     # 拉取 Devices 中所有已启用设备
readout update   # 更新本机 readout
```

同步使用 BatchMode、连接超时、总超时和有界输出，不保存密码或私钥。
TUI 的 Settings 页面只控制默认是否显示全部设备聚合值；关闭后
Overview/Models/Projects/Sessions 只看本机。SSH 不做定时同步：在 Devices 页面按 `r`
或点击卡片标题会启动后台同步，界面不会被 SSH 阻塞。

Devices 页面始终保留全设备状态。若同一个 transcript 出现在多台设备，稳定事件 ID 会
保证总量只计算一次；由于 Codex 记录不包含真实主机 ID，这部分会进入 `Shared`，不会
伪装成某台设备的独占 usage。

同一项目在不同系统上的 cwd 不同时，可以映射到统一名称：

```sh
readout project-alias set readout-cli /mnt/work/readout-cli
readout project-alias set readout-cli 'C:\work\readout-cli'
readout project-alias list
```

所有日期都由中心设备根据 bundle 中的 Unix 时间戳重新分桶，因此 Overview 的
`Today` 始终使用当前查看设备的时区，不会直接相加各设备含义不同的“今天”。

At least one tool always stays on: switching the last one off would leave a
dashboard of zeroes rather than an answer.

## Today, and keeping it live

The window totals answer "what have I spent". The question after that is
usually "what have I spent *today*", so today is always on screen — in the
line under the header, in `readout summary`, and under `"today"` in the JSON:

```
           tokens        cost   requests  sessions
      613,900,468     $449.37      3,652        18   TOTAL
      569,241,294     $417.09      3,251        11   Claude Code
       44,659,174      $32.28        401         7   Codex

      245,542,893     $189.95      1,706         6   today
```

`Today` is also a window of its own — the chip, `t`, or `readout -d 1`. Under
it the trend chart gives way to that day's hours, because a one-day trend is
one bar next to its own axis.

Watch mode keeps all of it current:

```sh
readout --watch     # or press w in the dashboard
```

Every five seconds it re-runs the same incremental scan, which reads only what
the transcripts appended. A figure that moved eases from the number you were
looking at to the new one, rather than counting up from zero as if the
dashboard had just opened.

A scan that finds nothing new draws no frame and writes no cache. So watching
costs a warm scan's worth of CPU every five seconds and *nothing at all down
the wire* — the terminal sees not one byte until a number actually changes,
which is what makes it reasonable over ssh. It repaints when the numbers move,
when a failed scan recovers, and at midnight, when every window shifts a day
without a single token moving.

## The CLI

```
readout summary [--json|--csv] [--timing]
readout models
readout projects
readout daily
readout pricing [--init]
readout refresh [--clear]
readout sync
readout update
readout project-alias set|remove|list
readout snapshot [--width N] [--height N] [--page overview|daily|models|projects|sessions|devices|pricing|settings]
```

Filters apply to every subcommand, the dashboard included:

```
-d, --days N        the last N days, 1–36500 (default: all time)
-s, --source S      claude or codex
-p, --project P     one project, identified by its full working directory
-m, --model M       one model
    --no-cache      reparse everything, ignoring the incremental cache
```

`snapshot` prints a single dashboard frame at a fixed size, with no terminal
setup and no input — useful in a bug report, a diff, or a layout check.
Daily CSV includes `cost_coverage`, so a partial estimate cannot be mistaken
for a complete cost.

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

Known models come out at their built-in rate; unknown ones come out zeroed as
placeholders, but remain unpriced until you edit their `input` / `output`
numbers (USD per million tokens). The edited values take precedence over the
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

The cache is rewritten only when a scan actually learned something. It holds
the parsed events for every file, so saving it costs the whole corpus rather
than the part that changed — fine once per launch, wrong every five seconds
under `--watch`.

`readout refresh` rebuilds the cache; `readout refresh --clear` deletes it.
Set `READOUT_STATE_DIR` to move the cache, remote usage snapshots, and price
overrides elsewhere. Set `READOUT_CONFIG_DIR` to move readout's own settings.

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
