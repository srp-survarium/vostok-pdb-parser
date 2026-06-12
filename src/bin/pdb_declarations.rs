//! Dump every function the original source *declared* as JSONL, one object per
//! line — including class methods that were inlined at every call site and so
//! emit NO symbol in the binary (the type stream still records them).
//!
//!   pdb_declarations --pdb survarium.pdb --out declarations.jsonl
//!   pdb_declarations --pdb survarium.pdb            # to stdout
//!
//! Schema (field order is the JSON order):
//!   {"class":   "vostok::network_core::udp_match_connection",  // null for free fns
//!    "name":    "construct_packet",
//!    "signature": "void (vostok::network_core::udp_match_packets_orderer&, u8)",
//!    "access":  "public" | "protected" | "private" | null,
//!    "is_virtual": bool, "is_static": bool, "is_const": bool,
//!    "kind":    "method" | "free"}
//!
//! Rows are sorted by (class, name, signature) and deduplicated, so repeated
//! runs are byte-identical.

use std::io::Write;
use std::path::PathBuf;

use clap::Parser;

use vostok_pdb_parser::declarations::dump_declarations;

#[derive(Parser)]
struct Cli {
    /// PDB to dump declarations from.
    #[arg(long, value_hint = clap::ValueHint::FilePath)]
    pdb: PathBuf,

    /// Write the JSONL to this file instead of stdout.
    #[arg(long, value_hint = clap::ValueHint::FilePath)]
    out: Option<PathBuf>,
}

fn main() {
    let cli = Cli::parse();

    let result = match &cli.out {
        Some(out) => {
            let file = match std::fs::File::create(out) {
                Ok(file) => file,
                Err(error) => {
                    eprintln!("{error}");
                    std::process::exit(1);
                }
            };
            let mut writer = std::io::BufWriter::new(file);
            dump_declarations(&cli.pdb, &mut writer).and_then(|count| {
                writer.flush()?;
                Ok(count)
            })
        }
        None => {
            let stdout = std::io::stdout();
            let mut writer = std::io::BufWriter::new(stdout.lock());
            dump_declarations(&cli.pdb, &mut writer).and_then(|count| {
                writer.flush()?;
                Ok(count)
            })
        }
    };

    match result {
        Ok(count) => {
            if let Some(out) = &cli.out {
                eprintln!("wrote {count} declarations to {}", out.display());
            }
        }
        Err(error) => {
            eprintln!("{error}");
            std::process::exit(1);
        }
    }
}
