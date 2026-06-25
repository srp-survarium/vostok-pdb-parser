//! Cross-PDB structural divergence report.
//!
//! Takes two PDBs — `base` (our compiled build) and `target` (the original
//! game) — and prints where their headers and sources diverge:
//!
//! * headers: class/struct/union/enum size, member layout, member-function order
//! * sources: function definition order, per-function statement count, and
//!   per-function constants (matched by value+type, so renames surface as
//!   misnames)
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
