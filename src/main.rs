mod download;
mod gui;
mod index;
mod manifest;
mod progress;

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use clap::parser::ValueSource;
use clap::{CommandFactory, FromArgMatches, Parser};
use futures::StreamExt;
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};

use download::{download_one, load_all_entries, plan_tasks, Outcome, Progress, USER_AGENT_VALUE};
use progress::LiveView;

pub const DEFAULT_DAT_DIR: &str = r"C:\Program Files (x86)\Steam\steamapps\common\UmamusumePrettyDerby\UmamusumePrettyDerby_Data\Persistent";

#[derive(Parser)]
#[command(
    name = "uma-downloader",
    version,
    about = "Umamusume: Pretty Derby asset downloader (opens the GUI by default; pass a CLI flag for headless sync)"
)]
struct Cli {
    /// Persistent data directory (parent of dat/).
    #[arg(long, default_value = DEFAULT_DAT_DIR)]
    dat_dir: String,

    /// Comma-separated categories to sync (default: all).
    #[arg(long, default_value = "")]
    category: String,

    /// Concurrent download workers.
    #[arg(long, default_value_t = 8)]
    workers: usize,

    /// List categories and exit.
    #[arg(long)]
    list: bool,

    /// Show totals without downloading.
    #[arg(long)]
    dry_run: bool,

    /// Fetch this resource version's root from the CDN instead of the game's cached manifests.
    #[arg(long)]
    resource_version: Option<String>,

    /// Mark size-matching files as downloaded in the game's meta database, then exit.
    #[arg(long)]
    index: bool,

    /// Launch the native GUI (default; the CLI runs only when a CLI flag is passed).
    #[arg(long)]
    gui: bool,
}

/// CLI progress sink: forwards byte counts to the indicatif main bar.
struct CliProgress {
    main: ProgressBar,
}

impl Progress for CliProgress {
    fn on_bytes(&self, n: u64) {
        self.main.inc(n);
    }
}

fn main() -> Result<()> {
    let matches = Cli::command().get_matches();
    // The GUI is the default. The CLI runs only when the user passes an
    // explicit CLI flag (--list, --category, --dry-run, --workers, --dat-dir).
    let cli_invoked = [
        "dat_dir",
        "category",
        "workers",
        "list",
        "dry_run",
        "resource_version",
        "index",
    ]
    .iter()
    .any(|id| matches.value_source(id) == Some(ValueSource::CommandLine));
    let args = match Cli::from_arg_matches(&matches) {
        Ok(a) => a,
        Err(e) => e.exit(),
    };

    if args.gui || !cli_invoked {
        return gui::run();
    }
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(run_cli(args))
}

async fn run_cli(args: Cli) -> Result<()> {
    anyhow::ensure!(args.workers >= 1, "--workers must be at least 1");

    if args.index {
        let report = index::index_meta(std::path::Path::new(&args.dat_dir), true)?;
        println!(
            "assets {} / {} indexed, {} marked, {} missing",
            report.indexed, report.assets, report.marked, report.missing
        );
        for line in &report.missing_samples {
            println!("  {line}");
        }
        if let Some(check) = &report.quick_check {
            println!("quick_check {check}");
            anyhow::ensure!(check == "ok", "quick_check failed");
        }
        if let Some(backup) = &report.backup {
            println!("backup {}", backup.display());
        }
        return Ok(());
    }

    let client = reqwest::Client::builder()
        .user_agent(USER_AGENT_VALUE)
        .build()?;

    eprintln!("Fetching manifests...");
    let set = load_all_entries(
        client.clone(),
        args.dat_dir.clone(),
        args.resource_version.clone(),
    )
    .await?;
    let assets = set.assets;

    if args.list {
        let mut by_cat: BTreeMap<&[u8], (usize, u64)> = BTreeMap::new();
        for a in &assets {
            let e = by_cat.entry(a.category.as_slice()).or_default();
            e.0 += 1;
            e.1 += a.size;
        }
        print_list(&by_cat, assets.len());
        return Ok(());
    }

    let wanted: HashSet<Vec<u8>> = args
        .category
        .split(',')
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(|s| s.as_bytes().to_vec())
        .collect();

    if !wanted.is_empty() {
        let cats: BTreeSet<&[u8]> = assets.iter().map(|a| a.category.as_slice()).collect();
        let mut unknown: Vec<String> = wanted
            .iter()
            .filter(|c| !cats.contains(c.as_slice()))
            .map(|c| String::from_utf8_lossy(c).into_owned())
            .collect();
        if !unknown.is_empty() {
            unknown.sort();
            let available: Vec<String> = cats
                .iter()
                .map(|c| String::from_utf8_lossy(c).into_owned())
                .collect();
            eprintln!("Unknown categories: {}", unknown.join(", "));
            eprintln!("Available: {}", available.join(", "));
            std::process::exit(1);
        }
    }

    let selected = assets
        .iter()
        .filter(|a| wanted.is_empty() || wanted.contains(&a.category))
        .count();
    let total_bytes: u64 = assets
        .iter()
        .filter(|a| wanted.is_empty() || wanted.contains(&a.category))
        .map(|a| a.size)
        .sum();
    eprintln!("{selected} assets to sync ({})", fmt_bytes(total_bytes));

    if args.dry_run {
        return Ok(());
    }

    // SHA1 hashing and metadata syscalls for up to ~191k assets would otherwise
    // stall the async runtime.
    let dat_dir = args.dat_dir.clone();
    let (todo, skipped) = tokio::task::spawn_blocking(move || {
        plan_tasks(&assets, &dat_dir, |a| wanted.is_empty() || wanted.contains(&a.category))
    })
    .await?;
    eprintln!("{skipped} already complete, {} to download", todo.len());
    if todo.is_empty() {
        return Ok(());
    }

    let total_files = todo.len();
    let todo_bytes: u64 = todo.iter().map(|t| t.size).sum();

    let mp = MultiProgress::new();
    let main = mp.add(ProgressBar::new(todo_bytes));
    main.set_style(
        ProgressStyle::with_template(
            "{spinner:.green} {wide_bar:.cyan/blue} {bytes}/{total_bytes} ({bytes_per_sec}, {eta})  {msg}",
        )
        .unwrap()
        .progress_chars("█▉▊▋▌▍▎▏  "),
    );
    main.enable_steady_tick(Duration::from_millis(100));
    let live = Arc::new(LiveView::new(&mp, args.workers));

    let cli_progress = Arc::new(CliProgress { main: main.clone() });

    let done = Arc::new(AtomicUsize::new(0));
    let fail = Arc::new(AtomicUsize::new(0));
    let skip = Arc::new(AtomicUsize::new(0));
    let done_bytes = Arc::new(AtomicU64::new(0));
    let finished = Arc::new(AtomicUsize::new(0));

    let futs = todo.into_iter().map(|t| {
        let client = client.clone();
        let main = main.clone();
        let live = Arc::clone(&live);
        let progress = Arc::clone(&cli_progress);
        let done = Arc::clone(&done);
        let fail = Arc::clone(&fail);
        let skip = Arc::clone(&skip);
        let done_bytes = Arc::clone(&done_bytes);
        let finished = Arc::clone(&finished);
        async move {
            let slot = live.acquire();
            if let Some(s) = &slot {
                s.set_file(&t.display);
            }
            let outcome =
                download_one(&client, &t.url, &t.dest, t.size, progress.as_ref()).await;
            match outcome {
                Outcome::Ok => {
                    done.fetch_add(1, Ordering::Relaxed);
                    done_bytes.fetch_add(t.size, Ordering::Relaxed);
                }
                Outcome::Skip => {
                    skip.fetch_add(1, Ordering::Relaxed);
                }
                Outcome::Fail => {
                    fail.fetch_add(1, Ordering::Relaxed);
                }
                // The CLI has no cancel signal; treat as a failure if it ever
                // fires so the summary still adds up.
                Outcome::Cancelled => {
                    fail.fetch_add(1, Ordering::Relaxed);
                }
            }
            let fin = finished.fetch_add(1, Ordering::Relaxed) + 1;
            main.set_message(format!(
                "{fin}/{total_files} files  ok={} skip={} fail={}",
                done.load(Ordering::Relaxed),
                skip.load(Ordering::Relaxed),
                fail.load(Ordering::Relaxed),
            ));
        }
    });

    futures::stream::iter(futs)
        .buffer_unordered(args.workers)
        .collect::<Vec<()>>()
        .await;

    let d = done.load(Ordering::Relaxed);
    let s = skip.load(Ordering::Relaxed);
    let f = fail.load(Ordering::Relaxed);
    main.finish_and_clear();
    let _ = mp.println(format!(
        "Done: {d} downloaded, {s} skipped, {f} failed ({})",
        fmt_bytes(done_bytes.load(Ordering::Relaxed)),
    ));
    if f > 0 {
        std::process::exit(1); // scripts must see a partial sync as a failure
    }
    Ok(())
}

fn print_list(by_cat: &BTreeMap<&[u8], (usize, u64)>, total_count: usize) {
    println!("{:<20} {:>7} {:>12}", "category", "count", "bytes");
    let mut total = 0u64;
    for (cat, (n, b)) in by_cat {
        total += b;
        println!(
            "{:<20} {:>7} {:>12}",
            String::from_utf8_lossy(cat),
            n,
            fmt_bytes(*b)
        );
    }
    println!(
        "{:<20} {:>7} {:>12}",
        "TOTAL",
        total_count,
        fmt_bytes(total)
    );
}

pub fn fmt_bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = n as f64;
    for (i, unit) in UNITS.iter().enumerate() {
        if v < 1024.0 || i == 4 {
            if i == 0 {
                return format!("{v:.0}{unit}");
            }
            return format!("{v:.1}{unit}");
        }
        v /= 1024.0;
    }
    format!("{v:.1}TB")
}
