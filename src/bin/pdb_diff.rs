use clap::Parser;
use pdb::FallibleIterator;
use std::collections::HashMap;

#[derive(Parser)]
struct Cli {
    #[arg(long, value_hint = clap::ValueHint::FilePath)]
    base_pdb: std::path::PathBuf,

    #[arg(long, value_hint = clap::ValueHint::FilePath)]
    target_pdb: std::path::PathBuf,

    /// Source path prefix in the base (compiled) PDB, e.g. "e:\projects\vostok\sources\vostok"
    #[arg(long)]
    base_engine_path: String,

    /// Source path prefix in the target (original) PDB, e.g. "c:\survarium\sources\vostok"
    #[arg(long)]
    target_engine_path: String,

    /// Print first 20 module names and source paths from base PDB, then exit
    #[arg(long)]
    list: bool,
}

fn normalize_prefix(s: &str) -> String {
    let mut p = s.to_lowercase().replace('/', "\\");
    if !p.ends_with('\\') {
        p.push('\\');
    }
    p
}

fn main() -> anyhow::Result<()> {
    let Cli { base_pdb, target_pdb, base_engine_path, target_engine_path, list } = Cli::parse();

    if list {
        return list_modules(&base_pdb);
    }

    let base   = collect_checksums(&base_pdb,   &normalize_prefix(&base_engine_path))?;
    let target = collect_checksums(&target_pdb, &normalize_prefix(&target_engine_path))?;

    print_diff(&base, &target);
    Ok(())
}

fn list_modules(path: &std::path::Path) -> anyhow::Result<()> {
    let file = std::fs::File::open(path)?;
    let mut pdb = pdb::PDB::open(file)?;
    let dbi = pdb.debug_information()?;
    let string_table = pdb.string_table()?;
    let mut modules = dbi.modules()?;
    let mut total = 0usize;
    while let Some(module) = modules.next()? {
        let name = module.module_name();
        if total < 20 {
            // Try to get first source file from this module
            let src = pdb.module_info(&module).ok().flatten().and_then(|mi| {
                let prog = mi.line_program().ok()?;
                let mut syms = mi.symbols().ok()?;
                loop {
                    let sym = syms.next().ok()??;
                    if let Ok(pdb::SymbolData::Procedure(p)) = sym.parse() {
                        let mut lines = prog.lines_for_symbol(p.offset);
                        if let Ok(Some(line)) = lines.next() {
                            let fi = prog.get_file_info(line.file_index).ok()?;
                            return fi.name.to_string_lossy(&string_table).ok().map(|s| s.to_string());
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

/// Returns a map of lowercased engine-relative .cpp path → raw checksum bytes.
fn collect_checksums(
    path: &std::path::Path,
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
