# readout

[English](README.md) | 简体中文

[![CI](https://github.com/atoz03/readout-cli/actions/workflows/ci.yml/badge.svg)](https://github.com/atoz03/readout-cli/actions/workflows/ci.yml)
[![crates.io](https://img.shields.io/crates/v/readout.svg)](https://crates.io/crates/readout)
[![license](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

在终端里查看 Claude Code 与 Codex 的 token、请求、会话和预估费用。

`readout` 直接读取两种工具已经生成的本地会话记录，提供可点击的 TUI，也能输出适合脚本处理的文本、JSON 和 CSV。它不代理请求，不读取 API 凭据，也不修改 Claude Code 或 Codex 的配置。

```sh
readout                  # 打开 dashboard
readout summary          # 输出汇总
readout summary --json   # 输出 JSON
```

## 功能

- 按日期、模型、项目、会话和小时统计 Claude Code 与 Codex 用量
- 显示今日、最近 7/30/90 天或全部历史
- 提供模型费用估算，并明确标记未定价部分
- 回放本机 session 的消息与工具调用时间线
- 通过 SSH 聚合多台设备，同时对重复会话记录去重
- 增量扫描会话记录，支持低开销 watch 模式
- 提供 JSON、CSV、固定尺寸快照和统一的 CLI 过滤参数

## 安装

Linux 和 macOS：

```sh
curl -fsSL https://github.com/atoz03/readout-cli/releases/latest/download/install.sh | sh
```

Windows：

```powershell
powershell -c "irm https://github.com/atoz03/readout-cli/releases/latest/download/install.ps1 | iex"
```

安装器会下载当前平台的预编译二进制并校验 SHA-256。默认目录：

- Linux/macOS：`~/.local/bin`
- Windows：`%LOCALAPPDATA%\Programs\readout`

安装器不会自动修改 PATH；目录不在 PATH 时会打印需要添加的配置。可以用 `READOUT_INSTALL_DIR` 修改安装目录，或用 `READOUT_VERSION` 固定版本。

已经安装后，直接更新：

```sh
readout update
```

也可以通过 crates.io 安装：

```sh
cargo install readout
```

各平台压缩包与校验文件见 [GitHub Releases](https://github.com/atoz03/readout-cli/releases/latest)。

## 快速开始

```sh
readout                 # 全部历史
readout -d 30           # 最近 30 天
readout -s claude       # 只看 Claude Code
readout -s codex        # 只看 Codex
readout -w              # 每几秒增量刷新
```

所有统计命令都支持相同的过滤条件：

| 参数 | 含义 |
|---|---|
| `-d, --days N` | 最近 N 天 |
| `-s, --source claude\|codex` | 只统计一种工具 |
| `-p, --project PATH` | 只统计一个完整项目路径 |
| `-m, --model MODEL` | 只统计一个模型 |
| `--no-cache` | 忽略增量缓存并完整重扫 |

## Dashboard

页面按“总览 → 明细 → 会话 → 设备与设置”组织：

| 页面 | 内容 |
|---|---|
| Overview | token、费用、请求、会话和近期趋势 |
| Daily | 每日用量 |
| Models | 模型分布与费用覆盖率 |
| Projects | 项目列表；进入后查看 Sessions |
| Sessions / Replay | 会话用量，以及本机 transcript 的消息和工具时间线 |
| Devices | 本机、已添加的 SSH 设备和同步状态 |
| Pricing | 当前模型价格 |
| Settings | 聚合开关、本机名称、SSH 设备、项目别名和配置路径 |

鼠标可点击侧栏、筛选项、列表行、卡片标题和 Replay 时间线。常用键盘操作：

| 按键 | 操作 |
|---|---|
| `↑/↓`、`j/k` | 移动选择 |
| `Enter` | 打开或确认当前行 |
| `Esc` | 返回、清除筛选或取消确认 |
| `Tab` / `Shift-Tab`、`←/→` | 切换页面 |
| `t`、`1/2/3/4` | 今日、7 天、30 天、90 天、全部 |
| `c/x` | 切换 Claude Code / Codex |
| `r` | Devices 中同步远端；其他页面重新扫描 |
| `w` | 开关 watch 模式 |
| `u` 两次 | 更新选中的已添加远端 |
| `Delete` / `Backspace` | 删除选中的 SSH 设备 |
| `?` | 显示当前页面帮助 |
| `q`、`Ctrl-C` | 退出 |

Replay 默认暂停。`Space` 播放或暂停，`1/2/4` 调整倍速，`[`/`]` 逐事件移动。远端 bundle 不包含消息正文，因此远程 session 只显示用量；完整 Replay 仅在当前设备持有会话记录时可用。

## 多设备

readout 通过 OpenSSH 从远端执行 `readout export`。远端 bundle 只包含用量元数据，不包含 prompt、回复正文、工具参数、工具结果或认证信息。

### 添加设备

1. 先在终端执行一次 `ssh HOST`，完成 host key 确认并验证认证。
2. 打开 Devices，选择 `Add SSH device…`。
3. 搜索 `~/.ssh/config` 中的 Host，或直接输入主机名。
4. 远端已安装 readout 时按 `Enter` 验证并添加。
5. 远端尚未安装时，在添加器里连续按两次 `Ctrl-U` 自动安装。
6. 回到 Devices 后按 `r` 同步所有已添加设备。

主列表只显示用户明确添加的远端，不会把大型 SSH config 全部展开。SSH 的用户名、端口、密钥和跳板机仍由 OpenSSH 处理，readout 不复制这些配置。

非交互 SSH 可能不读取远端 shell profile。readout 会先从 PATH 查找命令，再尝试官方安装器的默认用户目录：

- Unix/macOS：`$HOME/.local/bin/readout`
- Windows：`%LOCALAPPDATA%\Programs\readout\readout.exe`

选中已添加的远端后连续按两次 `u` 可更新远端 readout。DNS、认证、host key 和连接超时会直接报告，不会误触发安装器。

### 设备名称与项目路径

每台设备由稳定 device ID 和可修改显示名组成。修改名称不会制造新设备：

```sh
readout device rename workstation
```

同一项目在不同系统上路径不同时，可以在中心设备建立精确映射：

```sh
readout project-alias set readout-cli /mnt/work/readout-cli
readout project-alias set readout-cli 'C:\work\readout-cli'
```

多台设备出现同一会话记录时，readout 只计一次。无法唯一归属的重复记录显示在 `Shared` 设备桶中。

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

`summary --json` 会同时输出设备覆盖信息，便于脚本区分“新增了远端数据”和“实际用量发生变化”。`summary --csv` 与 `daily --csv` 包含费用覆盖率，避免把部分估算误当成完整费用。

`snapshot` 输出一个固定尺寸的静态 dashboard frame，不切换终端模式，适合提交布局问题：

```sh
readout snapshot --width 120 --height 40 --page devices
```

## 费用说明

费用是按内置模型价格计算的估算值，不等同于账单。通过 ChatGPT 订阅使用 Codex 时，页面显示的是对应 API 价格估算，而不是订阅实际扣费。

- `$12.34`：涉及的 token 都有价格
- `$12.34+`：仍有部分 token 未定价，当前值只是下限
- `—`：该行没有可用价格

查看价格或创建覆盖文件：

```sh
readout pricing
readout pricing --init
```

覆盖文件中的价格单位为“每百万 token 的美元价格”，并优先于内置值。未知模型不会被当成免费。

## 数据与隐私

readout 默认只扫描：

- `~/.claude/projects/**/*.jsonl`
- `~/.codex/sessions/**/*.jsonl`

Codex 的 `archived_sessions/` 不在默认范围。Session Replay 仅在打开具体会话时按需读取正文。

readout 不读取 `~/.claude/settings.json`、`~/.codex/config.toml` 或 `~/.codex/auth.json`，也不会修改两种工具的配置。持久化内容只有 readout 自己的设置、用量缓存、价格覆盖和远端 usage 快照。

| 环境变量 | 用途 |
|---|---|
| `READOUT_CLAUDE_DIR` | 修改 Claude 会话记录目录 |
| `READOUT_CODEX_DIR` | 修改 Codex 会话记录目录 |
| `READOUT_STATE_DIR` | 修改缓存、远端快照和价格覆盖目录 |
| `READOUT_CONFIG_DIR` | 修改 readout 设置目录 |
| `READOUT_SSH_CONFIG` | 使用指定 SSH config 进行发现和连接 |

增量缓存只保存统计所需的用量元数据，不保存 prompt、回复正文或工具内容。`readout refresh` 会重建缓存，`readout refresh --clear` 只删除缓存。

## 统计口径

Claude Code 的重复响应按消息 ID 去重；Codex 的累计用量会转换成每次请求的增量。两种工具对 cached input 的字段口径不同，readout 会在解析时统一。子代理和 Workflow 会话记录也会纳入统计。

这些处理用于避免明显重复和累计值误加，但结果仍应视为本地 transcript 的统计视图，而不是供应商账单。

## 开发

```sh
cargo test --all-targets
cargo clippy --all-targets -- -D warnings
cargo fmt --all --check
```

源码使用 Rust 2024 edition。发布流程会为 Linux x86-64/arm64、macOS Intel/Apple Silicon 和 Windows x86-64 构建产物。

## 许可证

[MIT](LICENSE)
