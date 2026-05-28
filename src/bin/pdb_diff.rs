use clap::Parser;
use digest::Digest;
use pdb::FallibleIterator;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[derive(Parser)]
struct Cli {
    /// Target (original game) PDB — always required
    #[arg(long, value_hint = clap::ValueHint::FilePath)]
    target_pdb: std::path::PathBuf,

    /// Source path prefix in the target PDB, e.g. "c:\survarium\sources\vostok"
    #[arg(long)]
    target_engine_path: String,

    #[command(flatten)]
    source: SourceArgs,

    /// Print first 20 module names and source paths from target PDB, then exit
    #[arg(long)]
    list: bool,
}

#[derive(clap::Args)]
#[group(required = false, multiple = false)]
struct SourceArgs {
    /// Compare against a compiled base PDB
    #[arg(long, value_hint = clap::ValueHint::FilePath)]
    base_pdb: Option<PathBuf>,

    /// Source path prefix in the base PDB, e.g. "e:\projects\vostok\sources\vostok"
    #[arg(long, requires = "base_pdb")]
    base_engine_path: Option<String>,

    /// Compare against source files on disk (no build needed)
    #[arg(long, value_hint = clap::ValueHint::DirPath)]
    source_dir: Option<PathBuf>,
}

fn normalize_prefix(s: &str) -> String {
    let mut p = s.to_lowercase().replace('/', "\\");
    if !p.ends_with('\\') {
        p.push('\\');
    }
    p
}

fn main() -> anyhow::Result<()> {
    let Cli { target_pdb, target_engine_path, source, list } = Cli::parse();

    if list {
        return list_modules(&target_pdb, &normalize_prefix(&target_engine_path));
    }

    let target = collect_checksums_from_pdb(&target_pdb, &normalize_prefix(&target_engine_path))?;

    let base = match (source.base_pdb, source.source_dir) {
        (Some(pdb_path), _) => {
            let prefix = normalize_prefix(&source.base_engine_path.unwrap());
            collect_checksums_from_pdb(&pdb_path, &prefix)?
        }
        (_, Some(dir)) => collect_checksums_from_dir(&dir, &target)?,
        (None, None) => anyhow::bail!("provide either --base-pdb or --source-dir"),
    };

    print_diff(&base, &target);
    Ok(())
}

// ── PDB mode ─────────────────────────────────────────────────────────────────

/// Returns a map of lowercased engine-relative .cpp path → raw checksum bytes.
fn collect_checksums_from_pdb(
    path: &Path,
    engine_prefix: &str,
) -> anyhow::Result<HashMap<String, Vec<u8>>> {
    let file = std::fs::File::open(path)?;
    let mut pdb = pdb::PDB::open(file)?;

    let string_table = pdb.string_table()?;
    let dbi = pdb.debug_information()?;

    let mut result: HashMap<String, Vec<u8>> = HashMap::new();
    let mut modules = dbi.modules()?;

    while let Some(module) = modules.next()? {
        let Some(module_info) = pdb.module_info(&module)? else {
            continue;
        };

        let program = module_info.line_program()?;
        let mut symbols = module_info.symbols()?;

        while let Some(sym) = symbols.next()? {
            let offset = match sym.parse() {
                Ok(pdb::SymbolData::Procedure(p)) => p.offset,
                _ => continue,
            };

            let mut lines = program.lines_for_symbol(offset);
            let line = match lines.next()? {
                Some(l) => l,
                None => continue,
            };

            let file_info = program.get_file_info(line.file_index)?;
            let name = file_info.name.to_string_lossy(&string_table)?;
            let name_lower = name.to_lowercase();

            if !name_lower.ends_with(".cpp") {
                continue;
            }
            let Some(relative) = name_lower.strip_prefix(engine_prefix) else {
                continue;
            };

            let key = relative.to_owned();
            if result.contains_key(&key) {
                break;
            }

            let checksum = match file_info.checksum {
                pdb::FileChecksum::None => vec![],
                pdb::FileChecksum::Md5(b) => b.to_vec(),
                pdb::FileChecksum::Sha1(b) => b.to_vec(),
                pdb::FileChecksum::Sha256(b) => b.to_vec(),
            };

            result.insert(key, checksum);
            break;
        }
    }

    Ok(result)
}

// ── Source-dir mode ───────────────────────────────────────────────────────────

/// Build a map of lowercased relative path → actual path for every file under
/// `root`. This lets us find files on a case-sensitive filesystem using the
/// lowercased paths that come out of the PDB.
fn build_case_index(root: &Path) -> HashMap<String, std::path::PathBuf> {
    let mut index = HashMap::new();
    let root_len = root.to_string_lossy().len();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else {
                let rel = path.to_string_lossy();
                // strip root prefix + path separator
                let rel = rel[root_len..].trim_start_matches(['/', '\\']);
                index.insert(rel.to_lowercase().replace('\\', "/"), path.clone());
            }
        }
    }
    index
}

/// For each file in target, compute its checksum from disk using the same
/// algorithm the target PDB used (detected from checksum length).
fn collect_checksums_from_dir(
    source_dir: &Path,
    target: &HashMap<String, Vec<u8>>,
) -> anyhow::Result<HashMap<String, Vec<u8>>> {
    let index = build_case_index(source_dir);
    let mut result = HashMap::new();

    for (rel_path, expected) in target {
        let lookup = rel_path.replace('\\', "/");
        let Some(fs_path) = index.get(&lookup) else { continue };

        let data = match std::fs::read(fs_path) {
            Ok(d) => d,
            Err(_) => continue,
        };

        let hash_fn: fn(&[u8]) -> Vec<u8> = match expected.len() {
            16 => md5_of,
            20 => sha1_of,
            32 => sha256_of,
            0  => { result.insert(rel_path.clone(), vec![]); continue; }
            _  => continue,
        };

        // Try LF first; if mismatch, retry with CRLF (Windows build machines store
        // checksums of CRLF content, but our checkout is LF-only).
        let lf_hash = hash_fn(&data);
        if &lf_hash == expected {
            result.insert(rel_path.clone(), lf_hash);
        } else {
            let crlf_data = lf_to_crlf(&data);
            result.insert(rel_path.clone(), hash_fn(&crlf_data));
        }
    }

    Ok(result)
}

fn lf_to_crlf(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len() + data.len() / 20);
    let mut i = 0;
    while i < data.len() {
        if data[i] == b'\n' && (i == 0 || data[i - 1] != b'\r') {
            out.push(b'\r');
        }
        out.push(data[i]);
        i += 1;
    }
    out
}

fn md5_of(data: &[u8]) -> Vec<u8> {
    md5::Md5::digest(data).to_vec()
}

fn sha1_of(data: &[u8]) -> Vec<u8> {
    sha1::Sha1::digest(data).to_vec()
}

fn sha256_of(data: &[u8]) -> Vec<u8> {
    sha2::Sha256::digest(data).to_vec()
}

// ── List mode ─────────────────────────────────────────────────────────────────

fn list_modules(path: &Path, engine_prefix: &str) -> anyhow::Result<()> {
    let file = std::fs::File::open(path)?;
    let mut pdb = pdb::PDB::open(file)?;
    let dbi = pdb.debug_information()?;
    let string_table = pdb.string_table()?;
    let mut modules = dbi.modules()?;
    let mut total = 0usize;
    while let Some(module) = modules.next()? {
        let name = module.module_name();
        if total < 20 {
            let src = pdb.module_info(&module).ok().flatten().and_then(|mi| {
                let prog = mi.line_program().ok()?;
                let mut syms = mi.symbols().ok()?;
                loop {
                    let sym = syms.next().ok()??;
                    if let Ok(pdb::SymbolData::Procedure(p)) = sym.parse() {
                        let mut lines = prog.lines_for_symbol(p.offset);
                        if let Ok(Some(line)) = lines.next() {
                            let fi = prog.get_file_info(line.file_index).ok()?;
                            let raw = fi.name.to_string_lossy(&string_table).ok()?;
                            let raw = raw.to_lowercase();
                            return Some(raw.strip_prefix(engine_prefix)
                                .unwrap_or(&raw)
                                .replace('\\', "/"));
                        }
                    }
                }
            });
            println!("{name:60}  src={}", src.as_deref().unwrap_or("(none)"));
        }
        total += 1;
    }
    println!("total modules: {total}");
    Ok(())
}

// ── Diff output ───────────────────────────────────────────────────────────────

fn print_diff(base: &HashMap<String, Vec<u8>>, target: &HashMap<String, Vec<u8>>) {
    let mut all_keys: Vec<&String> = base.keys().chain(target.keys()).collect();
    all_keys.sort();
    all_keys.dedup();

    let (mut n_match, mut n_diff, mut n_base, mut n_target) = (0usize, 0, 0, 0);

    for key in &all_keys {
        let name = key.replace('\\', "/");
        match (base.get(*key), target.get(*key)) {
            (Some(b), Some(t)) if b == t => { n_match  += 1; println!("MATCH   {name}"); }
            (Some(_), Some(_))           => { n_diff   += 1; println!("DIFF    {name}"); }
            (Some(_), None)              => { n_base   += 1; println!("BASE    {name}"); }
            (None, Some(_))              => { n_target += 1; println!("TARGET  {name}"); }
            (None, None) => unreachable!(),
        }
    }

    println!();
    println!("matched={n_match}  diff={n_diff}  base-only={n_base}  target-only={n_target}");
}
