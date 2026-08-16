//! Emit rich per-function context (disassembly interleaved with source-level
//! statements) for binary matching.
//!
//! Base (compiled) example:
//!   pdb_rich_context \
//!     --pdb vostok/binaries/Win32/survarium-dx11-win32-gold.pdb \
//!     --exe vostok/binaries/Win32/survarium-dx11-win32-gold.exe \
//!     --engine-path 'c:\survarium\sources' \
//!     --source-root vostok/sources \
//!     --mode base --out out/rich/base
//!
//! Target (original game, no sources) to stdout for inspection:
//!   pdb_rich_context --pdb survarium.pdb --exe survarium.exe --mode target

use std::path::PathBuf;

use clap::Parser;
use clap::ValueEnum;

use vostok_pdb_parser::rich_context::{Options, dump_rich_context};

#[derive(Copy, Clone, ValueEnum)]
enum Mode {
    /// Compiled build: read real source lines from `--source-root`.
    Base,
    /// Original game: no sources, statements show line-number placeholders.
    Target,
}

#[derive(Parser)]
struct Cli {
    #[arg(long, value_hint = clap::ValueHint::FilePath)]
    pdb: PathBuf,

    #[arg(long, value_hint = clap::ValueHint::FilePath)]
    exe: PathBuf,

    /// Recorded source-path prefix to strip (identifies engine files).
    ///
    /// REPEATABLE. A build draws compilands from more than one tree: the
    /// engine's own sources, and the Scaleform GFx SDK, which sits outside it
    /// (retail records it under C:\w\<hash>\Scaleform\Releases\GFx_4.2.21\,
    /// ours under the local SDK checkout). A single prefix silently dropped the
    /// other tree's compilands on BOTH sides, so those functions produced no
    /// records at all. Prefixes are tried in order, first match wins; give each
    /// tree the prefix that leaves the SAME relative path on both sides.
    #[arg(long, default_values_t = [String::from(r"c:\survarium\sources")])]
    engine_path: Vec<String>,

    /// Local engine source root to read statement text from (base mode).
    #[arg(long, value_hint = clap::ValueHint::DirPath)]
    source_root: Option<PathBuf>,

    #[arg(long, value_enum, default_value_t = Mode::Target)]
    mode: Mode,

    /// Output directory (structure-style tree). Omit to print to stdout.
    #[arg(long, value_hint = clap::ValueHint::DirPath)]
    out: Option<PathBuf>,
}

fn main() {
    let cli = Cli::parse();

    let engine_paths: Vec<String> = cli
        .engine_path
        .iter()
        .map(|path| {
            let mut path = path.to_lowercase().replace('/', "\\");
            if !path.ends_with('\\') {
                path.push('\\');
            }
            path
        })
        .collect();

    let opts = Options {
        engine_paths,
        source_root: cli.source_root,
        target_mode: matches!(cli.mode, Mode::Target),
        out_dir: cli.out,
    };

    if let Err(error) = dump_rich_context(&cli.pdb, &cli.exe, &opts) {
        eprintln!("{error}");
        std::process::exit(1);
    }
}
