//! readout — usage statistics for Claude Code and Codex.
//!
//! Strictly read-only with respect to both tools: it reads transcripts under
//! `~/.claude/projects` and `~/.codex/sessions`. Devices 页面按需读取 SSH config
//! 的 Host 别名；Claude/Codex 的认证与设置文件始终不会打开。

mod agg;
mod cache;
mod devices;
mod fmt;
mod model;
mod parse;
mod paths;
mod pricing;
mod replay;
mod report;
mod scan;
mod settings;
mod tui;
mod updater;

use agg::{Filter, summarize};
use anyhow::Result;
use clap::{Args, Parser, Subcommand};
use model::Source;
use pricing::Pricing;

#[derive(Parser, Debug)]
#[command(
    name = "readout",
    version,
    about = "Usage statistics for Claude Code and Codex",
    long_about = "readout reads Claude Code and Codex transcripts and reports token \
                  usage and estimated cost.\n\nIt never writes to either tool's \
                  configuration and never opens files that hold API credentials."
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,

    /// Keep the dashboard's numbers live, rescanning every few seconds
    #[arg(long, short = 'w')]
    watch: bool,

    #[command(flatten)]
    common: Common,
}

#[derive(Args, Debug, Clone)]
struct Common {
    /// Limit to the last N days (default: all time)
    #[arg(long, short = 'd', global = true, value_parser = parse_days)]
    days: Option<i64>,

    /// Only this tool: claude or codex
    #[arg(long, short = 's', global = true)]
    source: Option<String>,

    /// Only this project (full working directory)
    #[arg(long, short = 'p', global = true)]
    project: Option<String>,

    /// Only this model
    #[arg(long, short = 'm', global = true)]
    model: Option<String>,

    /// Ignore the incremental cache and reparse everything
    #[arg(long, global = true)]
    no_cache: bool,
}

/// A bounded calendar window. Larger histories should use the all-time view;
/// accepting arbitrary `i64` values here would let CSV attempt an effectively
/// unbounded allocation after converting the value to `usize`.
fn parse_days(raw: &str) -> std::result::Result<i64, String> {
    const MAX_DAYS: i64 = 36_500;
    let days = raw.parse::<i64>().map_err(|_| "days must be a whole number".to_string())?;
    if (1..=MAX_DAYS).contains(&days) {
        Ok(days)
    } else {
        Err(format!("days must be between 1 and {MAX_DAYS}"))
    }
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Open the interactive dashboard (default)
    Tui {
        /// Keep the numbers live, rescanning every few seconds
        #[arg(long, short = 'w')]
        watch: bool,
    },
    /// Print a summary
    Summary {
        /// Emit JSON instead of text
        #[arg(long)]
        json: bool,
        /// Emit daily CSV instead of text
        #[arg(long)]
        csv: bool,
        /// Show scan timings
        #[arg(long)]
        timing: bool,
    },
    /// Break down usage by model
    Models {
        #[arg(long)]
        json: bool,
    },
    /// Break down usage by project
    Projects {
        #[arg(long)]
        json: bool,
    },
    /// Daily totals
    Daily {
        #[arg(long)]
        json: bool,
        #[arg(long)]
        csv: bool,
    },
    /// Show the model price table
    Pricing {
        /// Write a starter override file listing every model in your data
        #[arg(long)]
        init: bool,
    },
    /// Rebuild the incremental cache from scratch
    Refresh {
        /// Delete the cache instead of rebuilding it
        #[arg(long)]
        clear: bool,
    },
    /// Print one dashboard frame at a fixed size (for bug reports and layout checks)
    Snapshot {
        #[arg(long, default_value_t = 120)]
        width: u16,
        #[arg(long, default_value_t = 40)]
        height: u16,
        /// overview, daily, models, projects, sessions, devices, pricing, settings
        #[arg(long, default_value = "overview")]
        page: String,
    },
    /// SSH 设备协议：导出只含 usage 的 bundle
    #[command(hide = true)]
    Export {
        /// 原子写入文件，而不是输出到 stdout
        #[arg(long, short = 'o')]
        output: Option<std::path::PathBuf>,
    },
    /// 同步 Devices 页面中已启用的 SSH 设备
    Sync,
    /// 从官方 release 安全更新 readout
    Update,
    /// 把不同设备的 cwd 映射成统一项目名
    ProjectAlias {
        #[command(subcommand)]
        action: ProjectAliasCommand,
    },
}

#[derive(Subcommand, Debug)]
enum ProjectAliasCommand {
    /// 添加或替换精确路径映射
    Set { name: String, path: String },
    /// 删除路径映射
    Remove { path: String },
    /// 列出项目路径映射
    List,
}

fn main() -> std::process::ExitCode {
    match run() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {}", fmt::terminal_text(&format!("{error:#}")));
            std::process::ExitCode::FAILURE
        }
    }
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    // 更新器不读取 transcript 或 settings；即使用户配置损坏，也应能修复二进制。
    if matches!(&cli.command, Some(Command::Update)) {
        return updater::update();
    }
    let sources = resolve_sources(&cli.common)?;
    let mut settings = settings::Settings::load_or_create()?;

    match cli.command {
        // `readout -w` and `readout tui -w` are the same request; accepting it
        // only before the subcommand would make the explicit form an error.
        None | Some(Command::Tui { .. }) => {
            let watch = cli.watch || matches!(cli.command, Some(Command::Tui { watch: true }));
            let filter = build_filter(&cli.common, &sources, &settings);
            tui::run(sources, filter, !cli.common.no_cache, watch, settings)
        }
        Some(Command::Summary { json, csv, timing }) => {
            let (s, stats, _) = load(&cli.common, &sources, &settings)?;
            if json {
                println!("{}", report::json(&s, &stats, cli.common.days));
            } else if csv {
                print!("{}", report::csv(&s, cli.common.days.unwrap_or(30) as usize));
            } else {
                print!("{}", report::text(&s, &stats, cli.common.days));
                for m in report::missing_sources(&sources) {
                    eprintln!("  note: no data for {m}");
                }
            }
            if timing {
                println!("\n{}", report::timing(&stats));
            }
            Ok(())
        }
        Some(Command::Models { json }) => {
            let (s, stats, _) = load(&cli.common, &sources, &settings)?;
            if json {
                println!("{}", report::json(&s, &stats, cli.common.days));
            } else {
                print_buckets(&s.by_model, "model", s.total.tokens.total());
            }
            Ok(())
        }
        Some(Command::Projects { json }) => {
            let (s, stats, _) = load(&cli.common, &sources, &settings)?;
            if json {
                println!("{}", report::json(&s, &stats, cli.common.days));
            } else {
                print_buckets(&s.by_project, "project", s.total.tokens.total());
            }
            Ok(())
        }
        Some(Command::Daily { json, csv }) => {
            let (s, stats, _) = load(&cli.common, &sources, &settings)?;
            if json {
                println!("{}", report::json(&s, &stats, cli.common.days));
            } else if csv {
                print!("{}", report::csv(&s, cli.common.days.unwrap_or(30) as usize));
            } else {
                println!("{:<12} {:>12} {:>10} {:>9}", "date", "tokens", "cost", "requests");
                for d in &s.daily {
                    println!(
                        "{:<12} {:>12} {:>10} {:>9}",
                        d.date,
                        fmt::count(d.bucket.tokens.total()),
                        fmt::money_partial(d.bucket.priced.cost, d.bucket.priced.coverage()),
                        fmt::count(d.bucket.events),
                    );
                }
            }
            Ok(())
        }
        Some(Command::Pricing { init }) => {
            let pricing = Pricing::load(paths::pricing_override_file().ok().as_deref())?;
            let (s, _, _) = load(&cli.common, &sources, &settings)?;
            let observed: Vec<String> = s.by_model.iter().map(|b| b.label.clone()).collect();
            if init {
                let path = paths::pricing_override_file()?;
                if path.exists() {
                    eprintln!(
                        "{} already exists; not overwriting.",
                        fmt::terminal_text(&path.display().to_string())
                    );
                } else {
                    std::fs::write(&path, pricing.starter_override(&observed)?)?;
                    println!("Wrote {}", fmt::terminal_text(&path.display().to_string()));
                    println!("Edit the zeroed rows to price the models readout has no rate for.");
                }
            } else {
                print!("{}", report::pricing_table(&pricing, &observed));
                if let Ok(p) = paths::pricing_override_file() {
                    let state = if p.exists() { "in use" } else { "not present" };
                    println!(
                        "\n  Overrides: {} ({state})",
                        fmt::terminal_text(&p.display().to_string())
                    );
                }
            }
            Ok(())
        }
        Some(Command::Refresh { clear }) => {
            let path = cache::default_path()?;
            if clear {
                if path.exists() {
                    std::fs::remove_file(&path)?;
                    println!("Removed {}", fmt::terminal_text(&path.display().to_string()));
                } else {
                    println!("No cache at {}", fmt::terminal_text(&path.display().to_string()));
                }
                return Ok(());
            }
            let _ = std::fs::remove_file(&path);
            let result = scan::scan_with_cache(&sources, true, None)?;
            print!("{}", report::timing(&result.stats));
            Ok(())
        }
        Some(Command::Snapshot { width, height, page }) => {
            let page = match page.to_ascii_lowercase().as_str() {
                "overview" => tui::app::Page::Overview,
                "daily" => tui::app::Page::Daily,
                "models" => tui::app::Page::Models,
                "projects" => tui::app::Page::Projects,
                "sessions" => tui::app::Page::Sessions,
                "devices" => tui::app::Page::Devices,
                "pricing" => tui::app::Page::Pricing,
                "settings" => tui::app::Page::Settings,
                other => anyhow::bail!("unknown page `{other}`"),
            };
            let filter = build_filter(&cli.common, &sources, &settings);
            print!(
                "{}",
                tui::snapshot(
                    sources,
                    filter,
                    !cli.common.no_cache,
                    width,
                    height,
                    page,
                    settings
                )?
            );
            Ok(())
        }
        Some(Command::Export { output }) => {
            let bundle = devices::export_local(&sources, !cli.common.no_cache, &settings)?;
            if let Some(path) = output {
                bundle.save(&path)?;
                println!("Wrote {}", fmt::terminal_text(&path.display().to_string()));
            } else {
                let mut writer = std::io::BufWriter::new(std::io::stdout().lock());
                bundle.write_json(&mut writer)?;
                std::io::Write::flush(&mut writer)?;
            }
            Ok(())
        }
        Some(Command::Sync) => {
            let report = devices::sync_all(&settings, None)?;
            if !report.synced.is_empty() {
                println!("Synced {}", report.synced.join(", "));
            }
            for failure in &report.failed {
                eprintln!("Failed {failure}");
            }
            anyhow::ensure!(report.failed.is_empty(), "one or more devices failed to sync");
            Ok(())
        }
        Some(Command::Update) => unreachable!("handled before loading settings"),
        Some(Command::ProjectAlias { action }) => {
            match action {
                ProjectAliasCommand::Set { name, path } => {
                    settings.set_project_alias(path.clone(), name.clone())?;
                    settings.save()?;
                    println!("Mapped {} to {name}", fmt::terminal_text(&path));
                }
                ProjectAliasCommand::Remove { path } => {
                    anyhow::ensure!(
                        settings.remove_project_alias(&path),
                        "unknown project path `{}`",
                        fmt::terminal_text(&path)
                    );
                    settings.save()?;
                    println!("Removed project alias for {}", fmt::terminal_text(&path));
                }
                ProjectAliasCommand::List => {
                    if settings.project_aliases.is_empty() {
                        println!("No project aliases configured.");
                    }
                    for alias in &settings.project_aliases {
                        println!(
                            "{:<24} {}",
                            fmt::terminal_text(&alias.name),
                            fmt::terminal_text(&alias.path)
                        );
                    }
                }
            }
            Ok(())
        }
    }
}

type Loaded = (agg::Summary, scan::ScanStats, Pricing);

fn load(common: &Common, sources: &[Source], settings: &settings::Settings) -> Result<Loaded> {
    let pricing = Pricing::load(paths::pricing_override_file().ok().as_deref())?;
    let result = devices::load_usage(sources, !common.no_cache, settings, None)?.scan;
    let filter = build_filter(common, sources, settings);
    let summary = summarize(&result.events, &filter, &pricing);
    Ok((summary, result.stats, pricing))
}

fn build_filter(common: &Common, sources: &[Source], settings: &settings::Settings) -> Filter {
    let today = chrono::Local::now().date_naive();
    Filter {
        sources: sources.to_vec(),
        since: common.days.map(|d| today - chrono::Duration::days(d - 1)),
        until: Some(today),
        project: common.project.clone(),
        model: common.model.clone(),
        session: None,
        device: (!settings.aggregate_devices).then(|| settings.device.id.clone()),
    }
}

fn resolve_sources(common: &Common) -> Result<Vec<Source>> {
    match &common.source {
        None => Ok(Source::ALL.to_vec()),
        Some(s) => Source::parse(s)
            .map(|s| vec![s])
            .ok_or_else(|| anyhow::anyhow!("unknown source `{s}` (expected claude or codex)")),
    }
}

fn print_buckets(buckets: &[agg::Bucket], kind: &str, grand_total: u64) {
    let width = buckets
        .iter()
        .map(|b| fmt::terminal_text(&b.label).chars().count().min(36))
        .max()
        .unwrap_or(8)
        .max(kind.len());
    println!("{:<width$} {:>15} {:>10} {:>7} {:>9}", kind, "tokens", "cost", "share", "requests");
    for b in buckets {
        let share = if grand_total == 0 {
            0.0
        } else {
            b.tokens.total() as f64 / grand_total as f64 * 100.0
        };
        println!(
            "{:<width$} {:>15} {:>10} {:>6.1}% {:>9}",
            fmt::terminal_ellipsize(&b.label, 36),
            fmt::count(b.tokens.total()),
            fmt::money_partial(b.priced.cost, b.priced.coverage()),
            share,
            fmt::count(b.events),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn days_rejects_zero_negative_and_impractically_large_windows() {
        assert_eq!(parse_days("1"), Ok(1));
        assert_eq!(parse_days("36500"), Ok(36_500));
        assert!(parse_days("0").is_err());
        assert!(parse_days("-7").is_err());
        assert!(parse_days("36501").is_err());
        assert!(parse_days("nope").is_err());
    }
}
