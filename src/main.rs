//! readout — usage statistics for Claude Code and Codex.
//!
//! Strictly read-only with respect to both tools: it reads transcripts under
//! `~/.claude/projects` and `~/.codex/sessions` and nothing else. Settings
//! files holding API credentials are never opened.

mod agg;
mod cache;
mod fmt;
mod model;
mod parse;
mod paths;
mod pricing;
mod report;
mod scan;
mod tui;

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

    #[command(flatten)]
    common: Common,
}

#[derive(Args, Debug, Clone)]
struct Common {
    /// Limit to the last N days (default: all time)
    #[arg(long, short = 'd', global = true)]
    days: Option<i64>,

    /// Only this tool: claude or codex
    #[arg(long, short = 's', global = true)]
    source: Option<String>,

    /// Only this project
    #[arg(long, short = 'p', global = true)]
    project: Option<String>,

    /// Only this model
    #[arg(long, short = 'm', global = true)]
    model: Option<String>,

    /// Ignore the incremental cache and reparse everything
    #[arg(long, global = true)]
    no_cache: bool,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Open the interactive dashboard (default)
    Tui,
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
        /// overview, daily, models, projects, sessions, pricing
        #[arg(long, default_value = "overview")]
        page: String,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let sources = resolve_sources(&cli.common)?;

    match cli.command {
        None | Some(Command::Tui) => {
            let filter = build_filter(&cli.common, &sources);
            tui::run(sources, filter, !cli.common.no_cache)
        }
        Some(Command::Summary { json, csv, timing }) => {
            let (s, stats, _) = load(&cli.common, &sources)?;
            if json {
                println!("{}", report::json(&s, &stats, cli.common.days));
            } else if csv {
                print!("{}", report::csv(&s, cli.common.days.unwrap_or(30)));
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
            let (s, stats, _) = load(&cli.common, &sources)?;
            if json {
                println!("{}", report::json(&s, &stats, cli.common.days));
            } else {
                print_buckets(&s.by_model, "model", s.total.tokens.total());
            }
            Ok(())
        }
        Some(Command::Projects { json }) => {
            let (s, stats, _) = load(&cli.common, &sources)?;
            if json {
                println!("{}", report::json(&s, &stats, cli.common.days));
            } else {
                print_buckets(&s.by_project, "project", s.total.tokens.total());
            }
            Ok(())
        }
        Some(Command::Daily { json, csv }) => {
            let (s, stats, _) = load(&cli.common, &sources)?;
            if json {
                println!("{}", report::json(&s, &stats, cli.common.days));
            } else if csv {
                print!("{}", report::csv(&s, cli.common.days.unwrap_or(30)));
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
            let (s, _, _) = load(&cli.common, &sources)?;
            let observed: Vec<String> = s.by_model.iter().map(|b| b.label.clone()).collect();
            if init {
                let path = paths::pricing_override_file()?;
                if path.exists() {
                    eprintln!("{} already exists; not overwriting.", path.display());
                } else {
                    std::fs::write(&path, pricing.starter_override(&observed))?;
                    println!("Wrote {}", path.display());
                    println!("Edit the zeroed rows to price the models readout has no rate for.");
                }
            } else {
                print!("{}", report::pricing_table(&pricing, &observed));
                if let Ok(p) = paths::pricing_override_file() {
                    let state = if p.exists() { "in use" } else { "not present" };
                    println!("\n  Overrides: {} ({state})", p.display());
                }
            }
            Ok(())
        }
        Some(Command::Refresh { clear }) => {
            let path = cache::default_path()?;
            if clear {
                if path.exists() {
                    std::fs::remove_file(&path)?;
                    println!("Removed {}", path.display());
                } else {
                    println!("No cache at {}", path.display());
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
                "pricing" => tui::app::Page::Pricing,
                other => anyhow::bail!("unknown page `{other}`"),
            };
            let filter = build_filter(&cli.common, &sources);
            print!(
                "{}",
                tui::snapshot(sources, filter, !cli.common.no_cache, width, height, page)?
            );
            Ok(())
        }
    }
}

type Loaded = (agg::Summary, scan::ScanStats, Pricing);

fn load(common: &Common, sources: &[Source]) -> Result<Loaded> {
    let pricing = Pricing::load(paths::pricing_override_file().ok().as_deref())?;
    let result = scan::scan_with_cache(sources, !common.no_cache, None)?;
    let filter = build_filter(common, sources);
    let summary = summarize(&result.events, &filter, &pricing);
    Ok((summary, result.stats, pricing))
}

fn build_filter(common: &Common, sources: &[Source]) -> Filter {
    Filter {
        sources: sources.to_vec(),
        since: common
            .days
            .map(|d| chrono::Local::now().date_naive() - chrono::Duration::days(d.max(1) - 1)),
        project: common.project.clone(),
        model: common.model.clone(),
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
    let width =
        buckets.iter().map(|b| b.label.chars().count().min(36)).max().unwrap_or(8).max(kind.len());
    println!("{:<width$} {:>15} {:>10} {:>7} {:>9}", kind, "tokens", "cost", "share", "requests");
    for b in buckets {
        let share = if grand_total == 0 {
            0.0
        } else {
            b.tokens.total() as f64 / grand_total as f64 * 100.0
        };
        println!(
            "{:<width$} {:>15} {:>10} {:>6.1}% {:>9}",
            fmt::ellipsize(&b.label, 36),
            fmt::count(b.tokens.total()),
            fmt::money_partial(b.priced.cost, b.priced.coverage()),
            share,
            fmt::count(b.events),
        );
    }
}
