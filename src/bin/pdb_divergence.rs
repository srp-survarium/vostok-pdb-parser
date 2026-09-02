//! Cross-PDB structural divergence report.
//!
//! Takes two PDBs — `base` (our compiled build) and `target` (the original
//! game) — and prints where their headers and sources diverge:
//!
//! * headers: class/struct/union/enum size, member layout, member-function order
//! * sources: function definition order and per-function constants (matched by
//!   value+type, so renames surface as misnames). Raw CodeView line-table entry
//!   counts are available only through `--raw-line-table-counts`; they are not
//!   semantic statement counts.
//!
//! `--skip <pat>` (repeatable) drops any header (by qualified name) or source
//! (by engine-relative path) whose name contains the case-insensitive substring,
//! e.g. `--skip render --skip ai`.
//!
//! ```text
//! pdb_divergence \
//!   --base-pdb   ../vostok/binaries/Win32/survarium-dx11-win32-gold.pdb \
//!   --base-engine-path   'z:\home\sheep\projects\surv-decomp\vostok\sources\' \
//!   --target-pdb ../vcproj2ninja/survarium.pdb \
//!   --target-engine-path 'c:\survarium\sources' \
//!   --skip render
//! ```

use clap::Parser;
use vostok_pdb_parser::divergence::{self, Config};

#[derive(Parser)]
#[command(about = "Report header/source divergences between two PDBs")]
struct Cli {
    /// Base (our compiled build) PDB.
    #[arg(long, value_hint = clap::ValueHint::FilePath)]
    base_pdb: std::path::PathBuf,

    /// Source path prefix in the base PDB, e.g. `z:\...\vostok\sources\`.
    #[arg(long)]
    base_engine_path: String,

    /// Target (original game) PDB.
    #[arg(long, value_hint = clap::ValueHint::FilePath)]
    target_pdb: std::path::PathBuf,

    /// Source path prefix in the target PDB, e.g. `c:\survarium\sources`.
    #[arg(long)]
    target_engine_path: String,

    /// Skip any header (qualified name) or source (engine-relative path) whose
    /// name contains this case-insensitive substring. Repeatable.
    #[arg(long = "skip", value_name = "PAT")]
    skip: Vec<String>,

    /// Also compare `std::`/`boost::`/… library types (skipped by default).
    #[arg(long)]
    include_external: bool,

    /// Print the names of one-sided headers/files, not just their counts.
    #[arg(long)]
    list_presence: bool,

    /// Print the per-function out-of-line PRESENCE divergences — functions with
    /// a standalone out-of-line body in exactly one PDB (`base-only`: we emit it
    /// standalone, target inlines it; `tgt-only`: target emits it standalone, we
    /// inline it / it is `/* no source */`) — not just their counts.
    #[arg(long)]
    list_presence_fns: bool,

    /// Also report raw per-function CodeView line-table entry-count differences.
    /// These counts reflect optimization attribution and source-line packing,
    /// not semantic statement structure, so they are excluded by default.
    #[arg(long)]
    raw_line_table_counts: bool,

    #[command(flatten)]
    scope: Scope,
}

#[derive(clap::Args)]
#[group(required = false, multiple = false)]
struct Scope {
    /// Only compare headers.
    #[arg(long)]
    headers_only: bool,

    /// Only compare sources.
    #[arg(long)]
    sources_only: bool,
}

fn normalize_prefix(s: &str) -> String {
    let mut p = s.to_lowercase().replace('/', "\\");
    if !p.ends_with('\\') {
        p.push('\\');
    }
    p
}

fn main() {
    let cli = Cli::parse();

    let cfg = Config {
        skip: cli.skip.iter().map(|s| s.to_lowercase()).collect(),
        include_external: cli.include_external,
        do_headers: !cli.scope.sources_only,
        do_sources: !cli.scope.headers_only,
        list_presence: cli.list_presence,
        list_presence_fns: cli.list_presence_fns,
        compare_raw_line_table_counts: cli.raw_line_table_counts,
    };

    let base_engine = normalize_prefix(&cli.base_engine_path);
    let target_engine = normalize_prefix(&cli.target_engine_path);

    if let Err(error) = divergence::run(
        &cli.base_pdb,
        &base_engine,
        &cli.target_pdb,
        &target_engine,
        &cfg,
    ) {
        eprintln!("{error}");
        std::process::exit(1);
    }
}
