//! Query function matching statistics from objdiff `report.json`.
//!
//!   # All 100%-matched functions in game_core, sorted smallest-first, as a table
//!   pdb_stats --report report.json list --min-percent 100 --unit-pattern game_core \
//!             --sort size --table
//!
//!   # Smallest unmatched functions (easiest to pick up next)
//!   pdb_stats --report report.json list --max-percent 99 --matched-only --sort size \
//!             --limit 20 --table
//!
//!   # Bucketed match distribution
//!   pdb_stats --report report.json summary
//!
//!   # Functions only in one side, cross-referenced with the index
//!   pdb_stats --report report.json --target-index index.jsonl orphans --cross-ref

use std::collections::HashMap;
use std::io::Write;
use std::path::PathBuf;

use clap::Parser;
use clap::Subcommand;
use clap::ValueEnum;
use vostok_pdb_parser::report_stats;

// ---------------------------------------------------------------------------
// CLI
// ---------------------------------------------------------------------------

#[derive(Parser)]
struct Cli {
    /// Path to objdiff report.json.
    #[arg(long, value_hint = clap::ValueHint::FilePath)]
    report: PathBuf,

    /// Path to target index.jsonl (for enrichment).
    #[arg(long, value_hint = clap::ValueHint::FilePath)]
    target_index: Option<PathBuf>,

    /// Path to base index.jsonl (for cross-reference).
    #[arg(long, value_hint = clap::ValueHint::FilePath)]
    base_index: Option<PathBuf>,

    /// Human-readable aligned table instead of JSON.
    #[arg(long)]
    table: bool,

    /// Pretty-printed JSON (default: compact).
    #[arg(long)]
    pretty: bool,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// List functions with match percentages, filterable.
    List {
        /// Minimum fuzzy_match_percent (inclusive).
        #[arg(long)]
        min_percent: Option<f64>,
        /// Maximum fuzzy_match_percent (inclusive).
        #[arg(long)]
        max_percent: Option<f64>,
        /// Exclude unmatched functions (no fuzzy_match_percent).
        #[arg(long)]
        matched_only: bool,
        /// Substring filter on unit name.
        #[arg(long)]
        unit_pattern: Option<String>,
        /// Sort field.
        #[arg(long, value_enum, default_value = "percent")]
        sort: SortField,
        /// Sort direction.
        #[arg(long, value_enum, default_value = "asc")]
        order: SortOrder,
        /// Max results.
        #[arg(long, default_value = "100")]
        limit: usize,
        /// Minimum function byte size.
        #[arg(long)]
        min_size: Option<u64>,
    },
    /// Functions with no fuzzy_match_percent (only in one side).
    Orphans {
        /// Substring filter on unit name.
        #[arg(long)]
        unit_pattern: Option<String>,
        /// Sort field.
        #[arg(long, value_enum, default_value = "size")]
        sort: SortField,
        /// Sort direction.
        #[arg(long, value_enum, default_value = "asc")]
        order: SortOrder,
        /// Max results.
        #[arg(long, default_value = "100")]
        limit: usize,
        /// Minimum function byte size (skip trivial stubs).
        #[arg(long)]
        min_size: Option<u64>,
        /// Also scan index.jsonl for entries absent from report entirely.
        #[arg(long)]
        cross_ref: bool,
    },
    /// Aggregate bucketed match distribution.
    Summary {
        /// Substring filter on unit name.
        #[arg(long)]
        unit_pattern: Option<String>,
        /// Include per-unit breakdown.
        #[arg(long)]
        by_unit: bool,
    },
    /// Convenience: smallest functions below a threshold (--sort size asc
    /// --matched-only --max-percent ...).
    Simplest {
        /// Only functions at or below this match percent (default 99).
        #[arg(long, default_value = "99")]
        max_percent: f64,
        /// Substring filter on unit name.
        #[arg(long)]
        unit_pattern: Option<String>,
        /// Max results.
        #[arg(long, default_value = "20")]
        limit: usize,
        /// Minimum function byte size.
        #[arg(long)]
        min_size: Option<u64>,
    },
}

#[derive(Clone, Copy, ValueEnum)]
enum SortField {
    Percent,
    Size,
    Name,
}

#[derive(Clone, Copy, ValueEnum)]
enum SortOrder {
    Asc,
    Desc,
}

impl From<SortField> for report_stats::SortField {
    fn from(f: SortField) -> Self {
        match f {
            SortField::Percent => report_stats::SortField::Percent,
            SortField::Size => report_stats::SortField::Size,
            SortField::Name => report_stats::SortField::Name,
        }
    }
}

impl From<SortOrder> for report_stats::SortOrder {
    fn from(o: SortOrder) -> Self {
        match o {
            SortOrder::Asc => report_stats::SortOrder::Asc,
            SortOrder::Desc => report_stats::SortOrder::Desc,
        }
    }
}

// ---------------------------------------------------------------------------
// JSON output helpers
// ---------------------------------------------------------------------------

#[derive(serde::Serialize)]
struct ListOutput<'a> {
    total: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    matched: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    unmatched: Option<usize>,
    functions: Vec<&'a report_stats::FuncEntry>,
}

fn write_json<T: serde::Serialize>(value: &T, pretty: bool) -> anyhow::Result<()> {
    let stdout = std::io::stdout();
    if pretty {
        serde_json::to_writer_pretty(stdout.lock(), value)?;
    } else {
        serde_json::to_writer(stdout.lock(), value)?;
    }
    println!();
    Ok(())
}

// ---------------------------------------------------------------------------
// Table output
// ---------------------------------------------------------------------------

const COL_WIDTH_NAME: usize = 62;
const COL_WIDTH_DEMANGLED: usize = 54;
const COL_WIDTH_UNIT: usize = 42;
const COL_WIDTH_SIZE: usize = 6;
const COL_WIDTH_PCT: usize = 7;
const COL_WIDTH_ADDR: usize = 6;
const COL_WIDTH_RVA: usize = 8;
const COL_WIDTH_FILE: usize = 40;

fn ellipsis(s: &str, w: usize) -> String {
    if s.len() <= w {
        s.to_string()
    } else {
        format!("{}...", &s[..w.saturating_sub(3)])
    }
}

fn pct_str(pct: Option<f64>) -> String {
    match pct {
        Some(v) if (v - 100.0).abs() < 0.001 => "100.0%".to_string(),
        Some(v) => format!("{v:>5.1}%"),
        None => "     -".to_string(),
    }
}

fn print_list_table(
    functions: &[&report_stats::FuncEntry],
    total: usize,
) {
    let has_demangled = functions.iter().any(|f| f.demangled.is_some());
    let has_enriched = functions.iter().any(|f| f.enriched.is_some());

    // Header
    if has_demangled {
        print!("{:<COL_WIDTH_DEMANGLED$}  ", "DEMANGLED");
    }
    print!("{:<COL_WIDTH_NAME$}  ", "MANGLED");
    print!("{:<COL_WIDTH_UNIT$}  ", "UNIT");
    print!("{:>COL_WIDTH_SIZE$}  ", "SIZE");
    print!("{:>COL_WIDTH_PCT$}  ", "MATCH%");
    if has_enriched {
        print!("{:>COL_WIDTH_RVA$}  ", "RVA");
        print!("{:<COL_WIDTH_FILE$}  ", "FILE");
    } else {
        print!("{:>COL_WIDTH_ADDR$}  ", "ADDR");
    }
    println!();

    for f in functions {
        if has_demangled {
            print!(
                "{:<COL_WIDTH_DEMANGLED$}  ",
                ellipsis(f.demangled.as_deref().unwrap_or("-"), COL_WIDTH_DEMANGLED)
            );
        }
        print!("{:<COL_WIDTH_NAME$}  ", ellipsis(&f.name, COL_WIDTH_NAME));
        print!("{:<COL_WIDTH_UNIT$}  ", ellipsis(&f.unit, COL_WIDTH_UNIT));
        print!("{:>COL_WIDTH_SIZE$}  ", format!("0x{:x}", f.size));
        print!("{:>COL_WIDTH_PCT$}  ", pct_str(f.fuzzy_match_percent));
        if let Some(enr) = &f.enriched {
            print!("{:>COL_WIDTH_RVA$}  ", format!("0x{:x}", enr.rva));
            print!("{:<COL_WIDTH_FILE$}  ", ellipsis(&enr.file, COL_WIDTH_FILE));
        } else {
            print!("{:>COL_WIDTH_ADDR$}  ", format!("0x{:x}", f.address));
        }
        println!();
    }

    println!("-- {total} total, {} shown --", functions.len());
}

fn print_orphan_table(
    functions: &[&report_stats::FuncEntry],
    base_only: Option<&[report_stats::IndexEntry]>,
    target_only: Option<&[report_stats::IndexEntry]>,
) {
    let has_demangled = functions.iter().any(|f| f.demangled.is_some());

    if !functions.is_empty() {
        println!("=== report.json (no fuzzy_match_percent) ===");
        if has_demangled {
            print!("{:<COL_WIDTH_DEMANGLED$}  ", "DEMANGLED");
        }
        print!("{:<COL_WIDTH_NAME$}  ", "MANGLED");
        print!("{:<COL_WIDTH_UNIT$}  ", "UNIT");
        print!("{:>COL_WIDTH_SIZE$}  ", "SIZE");
        print!("{:>COL_WIDTH_ADDR$}", "ADDR");
        println!();

        for f in functions {
            if has_demangled {
                print!(
                    "{:<COL_WIDTH_DEMANGLED$}  ",
                    ellipsis(f.demangled.as_deref().unwrap_or("-"), COL_WIDTH_DEMANGLED)
                );
            }
            print!("{:<COL_WIDTH_NAME$}  ", ellipsis(&f.name, COL_WIDTH_NAME));
            print!("{:<COL_WIDTH_UNIT$}  ", ellipsis(&f.unit, COL_WIDTH_UNIT));
            print!("{:>COL_WIDTH_SIZE$}  ", format!("0x{:x}", f.size));
            print!("{:>COL_WIDTH_ADDR$}", format!("0x{:x}", f.address));
            println!();
        }
        println!(
            "-- {} total, {} shown --",
            functions.len(),
            functions.len()
        );
    }

    if let Some(base) = base_only {
        if !base.is_empty() {
            println!("\n=== index-only: base ===");
            print_index_only_table(base);
        }
    }
    if let Some(target) = target_only {
        if !target.is_empty() {
            println!("\n=== index-only: target ===");
            print_index_only_table(target);
        }
    }
}

fn print_index_only_table(entries: &[report_stats::IndexEntry]) {
    print!("{:<COL_WIDTH_NAME$}  ", "MANGLED");
    print!("{:<COL_WIDTH_DEMANGLED$}  ", "DEMANGLED");
    print!("{:>COL_WIDTH_SIZE$}  ", "SIZE");
    print!("{:>COL_WIDTH_RVA$}  ", "RVA");
    print!("{:<COL_WIDTH_FILE$}", "FILE");
    println!();

    for e in entries {
        print!("{:<COL_WIDTH_NAME$}  ", ellipsis(&e.mangled, COL_WIDTH_NAME));
        print!("{:<COL_WIDTH_DEMANGLED$}  ", ellipsis(&e.name, COL_WIDTH_DEMANGLED));
        print!("{:>COL_WIDTH_SIZE$}  ", format!("0x{:x}", e.size));
        print!("{:>COL_WIDTH_RVA$}  ", format!("0x{:x}", e.rva));
        print!("{:<COL_WIDTH_FILE$}", ellipsis(&e.file, COL_WIDTH_FILE));
        println!();
    }
}

fn print_summary_table(summary: &report_stats::Summary) {
    let tl = &summary.top_level;
    println!("=== Top-level ===");
    if let Some(v) = tl.fuzzy_match_percent {
        println!("  fuzzy_match_percent:    {v:.2}%");
    }
    println!(
        "  total_functions:        {}",
        tl.total_functions.unwrap_or(0)
    );
    println!(
        "  matched_functions:      {}",
        tl.matched_functions.unwrap_or(0)
    );
    if let Some(v) = tl.matched_functions_percent {
        println!("  matched_functions_percent: {v:.2}%");
    }
    if let Some(ref v) = tl.total_code {
        println!("  total_code:             {v}");
    }
    if let Some(ref v) = tl.matched_code {
        println!("  matched_code:           {v}");
    }
    if let Some(v) = tl.matched_code_percent {
        println!("  matched_code_percent:   {v:.2}%");
    }

    println!("\n=== Buckets ===");
    println!("  {:<8} {:>7} {:>10}", "RANGE", "COUNT", "BYTES");
    for label in ["0", "1-49", "50-79", "80-89", "90-94", "95-98", "99-99", "100"] {
        if let Some(b) = summary.buckets.get(label) {
            println!(
                "  {:<8} {:>7}  0x{:x}",
                label, b.count, b.code_bytes
            );
        }
    }

    if let Some(ref units) = summary.by_unit {
        println!("\n=== By unit ===");
        println!(
            "  {:<COL_WIDTH_UNIT$} {:>6} {:>6} {:>7}",
            "UNIT", "FUNCS", "MATCH", "CODE"
        );
        for u in units {
            println!(
                "  {:<COL_WIDTH_UNIT$} {:>6} {:>6} {:>7}",
                ellipsis(&u.name, COL_WIDTH_UNIT),
                u.total_functions.unwrap_or(0),
                u.matched_functions.unwrap_or(0),
                u.total_code.as_deref().unwrap_or("-"),
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Audit log (same pattern as pdb_fetch.rs)
// ---------------------------------------------------------------------------

struct LogGuard {
    when: chrono::DateTime<chrono::Local>,
    path: Option<PathBuf>,
}

impl LogGuard {
    fn start(path: Option<PathBuf>) -> Self {
        Self {
            when: chrono::Local::now(),
            path,
        }
    }
}

impl Drop for LogGuard {
    fn drop(&mut self) {
        let Some(path) = &self.path else {
            return;
        };
        use chrono::Timelike as _;
        let args: Vec<String> = std::env::args().collect();
        let line = format!(
            "[{}.{:02}][{}]: {}\n",
            self.when.format("%Y-%m-%d %H:%M:%S"),
            self.when.nanosecond() / 10_000_000,
            current_branch(),
            args.join(" "),
        );
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
        {
            let _ = f.write_all(line.as_bytes());
        }
    }
}

fn resolve_log_path(cli: &Cli) -> Option<PathBuf> {
    if let Ok(p) = std::env::var("PDB_FETCH_LOG") {
        return Some(PathBuf::from(p));
    }
    let binaries = cli.report.parent()?.parent()?;
    let tool = std::env::args()
        .next()
        .and_then(|a| {
            std::path::Path::new(&a)
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
        })
        .unwrap_or_else(|| "pdb_stats".into());
    Some(binaries.join(format!("{tool}.log")))
}

fn current_branch() -> String {
    std::process::Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "?".into())
}

// ---------------------------------------------------------------------------
// main
// ---------------------------------------------------------------------------

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let _log = LogGuard::start(resolve_log_path(&cli));

    // Load optional indexes.
    let target_idx: Option<HashMap<String, report_stats::IndexEntry>> = cli
        .target_index
        .as_deref()
        .map(|p| report_stats::load_mangled_index(p))
        .transpose()?;
    let base_idx: Option<HashMap<String, report_stats::IndexEntry>> = cli
        .base_index
        .as_deref()
        .map(|p| report_stats::load_mangled_index(p))
        .transpose()?;

    match cli.command {
        Command::List {
            min_percent,
            max_percent,
            matched_only,
            unit_pattern,
            sort,
            order,
            limit,
            min_size,
        } => {
            let mut functions = report_stats::load_report(&cli.report)?;
            let total = functions.len();
            let n_matched = functions
                .iter()
                .filter(|f| f.fuzzy_match_percent.is_some())
                .count();
            let n_unmatched = total - n_matched;

            // Enrich from target index if available.
            if let Some(ref idx) = target_idx {
                report_stats::enrich(&mut functions, idx);
            }

            let filter = report_stats::FuncFilter {
                unit_pattern: unit_pattern.as_deref(),
                min_percent,
                max_percent,
                matched_only,
                min_size,
                limit: Some(limit),
                sort: sort.into(),
                order: order.into(),
            };
            let result = report_stats::filter_functions(&functions, &filter);

            if cli.table {
                print_list_table(&result, total);
            } else {
                let output = ListOutput {
                    total,
                    matched: Some(n_matched),
                    unmatched: Some(n_unmatched),
                    functions: result,
                };
                write_json(&output, cli.pretty)?;
            }
        }

        Command::Orphans {
            unit_pattern,
            sort,
            order,
            limit,
            min_size,
            cross_ref,
        } => {
            let functions = report_stats::load_report(&cli.report)?;

            let filter = report_stats::FuncFilter {
                unit_pattern: unit_pattern.as_deref(),
                min_size,
                limit: Some(limit),
                sort: sort.into(),
                order: order.into(),
                ..Default::default()
            };
            let result = report_stats::find_orphans(&functions, &filter);

            let index_only = if cross_ref {
                let report_set = report_stats::mangled_set(&functions);
                let base_only = base_idx
                    .as_ref()
                    .map(|idx| report_stats::cross_ref_orphans(&report_set, idx));
                let target_only = target_idx
                    .as_ref()
                    .map(|idx| report_stats::cross_ref_orphans(&report_set, idx));
                Some((base_only, target_only))
            } else {
                None
            };

            if cli.table {
                let base_refs: Option<&[report_stats::IndexEntry]> =
                    index_only.as_ref().and_then(|(b, _)| b.as_deref());
                let target_refs: Option<&[report_stats::IndexEntry]> =
                    index_only.as_ref().and_then(|(_, t)| t.as_deref());
                print_orphan_table(
                    &result,
                    base_refs,
                    target_refs,
                );
            } else {
                let output = report_stats::OrphanOutput {
                    report: result.into_iter().cloned().collect(),
                    index_only: index_only.map(|(b, t)| report_stats::IndexOnlyOrphans {
                        base: b,
                        target: t,
                    }),
                };
                write_json(&output, cli.pretty)?;
            }
        }

        Command::Summary {
            unit_pattern,
            by_unit,
        } => {
            let functions = report_stats::load_report(&cli.report)?;
            let mut summary =
                report_stats::compute_summary(&functions, unit_pattern.as_deref(), by_unit);
            let top = report_stats::load_top_measures(&cli.report)?;
            report_stats::attach_top_measures(&mut summary, top);

            if cli.table {
                print_summary_table(&summary);
            } else {
                write_json(&summary, cli.pretty)?;
            }
        }

        Command::Simplest {
            max_percent,
            unit_pattern,
            limit,
            min_size,
        } => {
            let mut functions = report_stats::load_report(&cli.report)?;
            let total = functions.len();

            if let Some(ref idx) = target_idx {
                report_stats::enrich(&mut functions, idx);
            }

            let filter = report_stats::FuncFilter {
                unit_pattern: unit_pattern.as_deref(),
                max_percent: Some(max_percent),
                matched_only: true,
                min_size,
                limit: Some(limit),
                sort: report_stats::SortField::Size,
                order: report_stats::SortOrder::Asc,
                ..Default::default()
            };
            let result = report_stats::filter_functions(&functions, &filter);

            if cli.table {
                print_list_table(&result, total);
            } else {
                let output = ListOutput {
                    total,
                    matched: None,
                    unmatched: None,
                    functions: result,
                };
                write_json(&output, cli.pretty)?;
            }
        }
    }

    Ok(())
}
