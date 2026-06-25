//! Cross-PDB structural divergence report.
//!
//! Given two PDBs — conventionally `base` (our compiled build) and `target`
//! (the original game) — this builds an owned, comparable model of each side
//! and reports where the two diverge:
//!
//! * **headers** (classes / structs / unions / enums, joined by qualified name):
//!   instance size, member layout (offset / type / name / order), and
//!   member-function declaration order.
//! * **sources** (`.cpp` compilands, joined by engine-relative path): the order
//!   functions are defined in, per-function statement count, and per-function
//!   constants (matched by `(type, value)`, so a renamed-but-equal constant is
//!   surfaced as a *misname* rather than an add/remove).
//!
//! Nothing is written to disk; the whole thing is a read + compare + print.
//! Headers are extracted straight off the type stream (no source join needed),
//! while sources reuse [`gen_sources::for_each_function`] so the parsed model
//! stays identical to what the generator emits.

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::collections::HashSet;
use std::path::Path;

use pdb::FallibleIterator;
use pdb::TypeData;

use crate::GenFlags;
use crate::Namespace;
use crate::gen_sources;
use crate::helpers::FunctionLocation;
use crate::pdb_parser::PdbParser;

/// Library namespace roots whose types are skipped by default (overridable with
/// `include_external`). The engine's own source path filter already keeps the
/// `sources` side clean; this is purely to stop the `headers` side from drowning
/// in `std::`/`boost:: ` template instantiations that always match anyway.
const EXTERNAL_ROOTS: &[&str] = &[
    "std::",
    "stlp_std::",
    "boost::",
    "eastl::",
    "stdext::",
    "__",
];

pub struct Config {
    /// Lowercased substrings; a header (by qualified name) or source (by
    /// engine-relative path) matching any of these is skipped.
    pub skip: Vec<String>,
    /// Compare every type, including the [`EXTERNAL_ROOTS`] library namespaces.
    pub include_external: bool,
    pub do_headers: bool,
    pub do_sources: bool,
    /// List the names of one-sided (base-only / target-only) headers and files
    /// instead of only counting them.
    pub list_presence: bool,
    /// List the per-function out-of-line PRESENCE divergences (a function with a
    /// standalone out-of-line body in exactly one side's compilands) instead of
    /// only counting them. See [`report_presence_functions`].
    pub list_presence_fns: bool,
}

// ── Owned comparison model ──────────────────────────────────────────────────

#[derive(Default)]
struct SideModel {
    classes: BTreeMap<String, ClassModel>,
    enums: BTreeMap<String, EnumModel>,
    files: BTreeMap<String, FileModel>,
}

struct ClassModel {
    kind: &'static str,
    size: u64,
    fields: Vec<FieldModel>,
    methods: Vec<MethodModel>,
}

struct FieldModel {
    offset: u64,
    type_name: String,
    name: String,
    // CV_access_t: 0=unspecified, 1=private, 2=protected, 3=public.
    access: u8,
}

struct MethodModel {
    /// The formatted signature (also the join key for the fn-order diff).
    sig: String,
    // CV_access_t (see FieldModel::access).
    access: u8,
}

struct EnumModel {
    underlying: String,
    values: Vec<(String, i64)>,
}

struct FileModel {
    functions: Vec<FnModel>,
}

struct FnModel {
    name_orig: String,
    statement_count: usize,
    constants: Vec<ConstModel>,
}

struct ConstModel {
    name: String,
    type_name: String,
    value: i64,
}

// ── Entry point ─────────────────────────────────────────────────────────────

pub fn run(
    base_pdb: &Path,
    base_engine: &str,
    target_pdb: &Path,
    target_engine: &str,
    cfg: &Config,
) -> crate::Result<()> {
    let base = extract_side(base_pdb, base_engine, cfg)?;
    let target = extract_side(target_pdb, target_engine, cfg)?;

    if cfg.do_headers {
        report_headers(&base, &target, cfg);
    }
    if cfg.do_sources {
        report_sources(&base, &target, cfg);
    }

    Ok(())
}

fn extract_side(pdb_path: &Path, engine: &str, cfg: &Config) -> crate::Result<SideModel> {
    let mut out = SideModel::default();

    PdbParser::with(pdb_path, |fmt| {
        let file = std::fs::File::open(pdb_path)?;
        let mut pdb = pdb::PDB::open(file)?;

        if cfg.do_headers {
            extract_headers(&mut pdb, &fmt, cfg, &mut out)?;
        }
        if cfg.do_sources {
            extract_sources(&mut pdb, &fmt, engine, cfg, &mut out)?;
        }

        Ok(())
    })?;

    Ok(out)
}

// ── Header extraction (off the type stream) ─────────────────────────────────

fn extract_headers(
    pdb: &mut pdb::PDB<std::fs::File>,
    fmt: &PdbParser,
    cfg: &Config,
    out: &mut SideModel,
) -> crate::Result<()> {
    let type_information = pdb.type_information()?;
    let type_finder = {
        let mut finder = type_information.finder();
        let mut iter = type_information.iter();
        while iter.next()?.is_some() {
            finder.update(&iter);
        }
        finder
    };

    let type_information2 = pdb.type_information()?;
    let mut type_iter = type_information2.iter();

    while let Some(item) = type_iter.next()? {
        match item.parse() {
            Ok(TypeData::Class(data)) if !data.properties.forward_reference() => {
                let qualified_name = data.name.to_string().to_string();
                if !include_type(&qualified_name, cfg) {
                    continue;
                }

                let namespace = Namespace::get_from_class_name_impl(&qualified_name);
                let mut class = ClassModel {
                    kind: kind_label(data.kind),
                    size: data.size,
                    fields: Vec::new(),
                    methods: Vec::new(),
                };

                if let Some(fields) = data.fields {
                    if walk_fields(&type_finder, fmt, &namespace, fields, &mut class).is_err() {
                        continue;
                    }
                }

                insert_class(out, qualified_name, class);
            }

            Ok(TypeData::Union(data))
                if !data.properties.forward_reference() && !data.properties.is_nested_type() =>
            {
                let qualified_name = data.name.to_string().to_string();
                if !include_type(&qualified_name, cfg) {
                    continue;
                }

                let namespace = Namespace::get_from_class_name_impl(&qualified_name);
                let mut class = ClassModel {
                    kind: "union",
                    size: data.size,
                    fields: Vec::new(),
                    methods: Vec::new(),
                };

                if data.count > 0
                    && walk_fields(&type_finder, fmt, &namespace, data.fields, &mut class).is_err()
                {
                    continue;
                }

                insert_class(out, qualified_name, class);
            }

            Ok(TypeData::Enumeration(data))
                if !data.properties.forward_reference() && !data.properties.is_nested_type() =>
            {
                let qualified_name = data.name.to_string().to_string();
                if !include_type(&qualified_name, cfg) {
                    continue;
                }

                let namespace = Namespace::get_from_class_name_impl(&qualified_name);
                let Ok(underlying) = fmt.emit_type(0, data.underlying_type, &namespace) else {
                    continue;
                };

                let mut e = EnumModel {
                    underlying: clean_type(underlying.0),
                    values: Vec::new(),
                };
                if walk_enum(&type_finder, data.fields, &mut e).is_err() {
                    continue;
                }

                insert_enum(out, qualified_name, e);
            }

            _ => continue,
        }
    }

    Ok(())
}

fn walk_fields(
    finder: &pdb::TypeFinder,
    fmt: &PdbParser,
    namespace: &Namespace,
    fields: pdb::TypeIndex,
    class: &mut ClassModel,
) -> crate::Result<()> {
    match finder.find(fields)?.parse()? {
        TypeData::FieldList(data) => {
            for field in &data.fields {
                handle_field(finder, fmt, namespace, field, class)?;
            }
            if let Some(continuation) = data.continuation {
                walk_fields(finder, fmt, namespace, continuation, class)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn handle_field(
    finder: &pdb::TypeFinder,
    fmt: &PdbParser,
    namespace: &Namespace,
    field: &TypeData,
    class: &mut ClassModel,
) -> crate::Result<()> {
    match *field {
        TypeData::Member(ref data) => {
            let type_name = clean_type(fmt.emit_type(0, data.field_type, namespace)?.0);
            class.fields.push(FieldModel {
                offset: data.offset,
                type_name,
                name: data.name.to_string().to_string(),
                access: data.attributes.access(),
            });
        }

        TypeData::BaseClass(ref data) => {
            let type_name = clean_type(fmt.emit_type(0, data.base_class, namespace)?.0);
            class.fields.push(FieldModel {
                offset: data.offset as u64,
                name: format!("<base> {type_name}"),
                type_name,
                access: data.attributes.access(),
            });
        }

        TypeData::VirtualBaseClass(ref data) => {
            let type_name = clean_type(fmt.emit_type(0, data.base_class, namespace)?.0);
            class.fields.push(FieldModel {
                offset: data.base_pointer_offset as u64,
                name: format!("<vbase> {type_name}"),
                type_name,
                access: data.attributes.access(),
            });
        }

        TypeData::Method(ref data) => {
            add_method(
                fmt,
                data.name,
                data.method_type,
                data.attributes.access(),
                class,
            )?;
        }

        TypeData::OverloadedMethod(ref data) => {
            if let TypeData::MethodList(method_list) = finder.find(data.method_list)?.parse()? {
                // Mirror the generator's traversal order so a clean class shows
                // no spurious method-order divergence against itself.
                for entry in method_list.methods.into_iter().rev() {
                    add_method(
                        fmt,
                        data.name,
                        entry.method_type,
                        entry.attributes.access(),
                        class,
                    )?;
                }
            }
        }

        _ => {}
    }

    Ok(())
}

fn add_method(
    fmt: &PdbParser,
    name: pdb::RawString,
    method_type: pdb::TypeIndex,
    access: u8,
    class: &mut ClassModel,
) -> crate::Result<()> {
    // Compiler-synthesised thunks (`__vecDelDtor`, vftable ctor closures, …) are
    // noise for a declaration-order comparison.
    if name.as_bytes().starts_with(b"__") {
        return Ok(());
    }
    class.methods.push(MethodModel {
        sig: clean_type(fmt.emit_function_orig(&name, 0, method_type)?),
        access,
    });
    Ok(())
}

fn walk_enum(
    finder: &pdb::TypeFinder,
    fields: pdb::TypeIndex,
    e: &mut EnumModel,
) -> crate::Result<()> {
    if let TypeData::FieldList(data) = finder.find(fields)?.parse()? {
        for field in &data.fields {
            if let TypeData::Enumerate(ref d) = *field {
                e.values
                    .push((d.name.to_string().to_string(), variant_to_i64(d.value)));
            }
        }
        if let Some(continuation) = data.continuation {
            walk_enum(finder, continuation, e)?;
        }
    }
    Ok(())
}

fn insert_class(out: &mut SideModel, name: String, class: ClassModel) {
    // Same-named definitions can appear more than once (a fieldless forward
    // shape alongside the real one); keep whichever carries more detail.
    match out.classes.get(&name) {
        Some(existing) if richness(existing) >= class.fields.len() + class.methods.len() => {}
        _ => {
            out.classes.insert(name, class);
        }
    }
}

fn richness(class: &ClassModel) -> usize {
    class.fields.len() + class.methods.len()
}

fn insert_enum(out: &mut SideModel, name: String, e: EnumModel) {
    match out.enums.get(&name) {
        Some(existing) if existing.values.len() >= e.values.len() => {}
        _ => {
            out.enums.insert(name, e);
        }
    }
}

// ── Source extraction (reuse the generator's parse) ─────────────────────────

fn extract_sources(
    pdb: &mut pdb::PDB<std::fs::File>,
    fmt: &PdbParser,
    engine: &str,
    cfg: &Config,
    out: &mut SideModel,
) -> crate::Result<()> {
    gen_sources::for_each_function(pdb, fmt, GenFlags::empty(), |filename, fun| {
        // The header side already covers inline functions defined in `.h`; here
        // we only compare definition order within real compilands.
        if !matches!(FunctionLocation::get(filename), FunctionLocation::Source) {
            return;
        }

        let lowered = filename.to_lowercase().replace('/', "\\");
        let Some(relative) = lowered.strip_prefix(engine) else {
            return;
        };
        let relative = relative.trim_start_matches('\\').replace('\\', "/");
        if skipped(&relative, cfg) {
            return;
        }

        let constants = fun
            .constants
            .iter()
            .map(|(name, type_name, value)| ConstModel {
                name: name.to_string().to_string(),
                type_name: clean_type(type_name.0.clone()),
                value: variant_to_i64(*value),
            })
            .collect();

        out.files
            .entry(relative)
            .or_insert_with(|| FileModel {
                functions: Vec::new(),
            })
            .functions
            .push(FnModel {
                name_orig: fun.name_orig.clone(),
                statement_count: fun.statements.len(),
                constants,
            });
    })
}

// ── Header reporting ────────────────────────────────────────────────────────

fn report_headers(base: &SideModel, target: &SideModel, cfg: &Config) {
    println!("================ HEADERS ================\n");

    let mut counts = HeaderCounts::default();

    let keys = union_keys(base.classes.keys(), target.classes.keys());
    for name in keys {
        match (base.classes.get(name), target.classes.get(name)) {
            (Some(b), Some(t)) => {
                counts.both += 1;
                if diff_class(name, b, t, &mut counts) {
                    counts.diverged += 1;
                }
            }
            (Some(_), None) => counts.only_base.push(name.clone()),
            (None, Some(_)) => counts.only_target.push(name.clone()),
            (None, None) => unreachable!(),
        }
    }

    let enum_keys = union_keys(base.enums.keys(), target.enums.keys());
    for name in enum_keys {
        match (base.enums.get(name), target.enums.get(name)) {
            (Some(b), Some(t)) => {
                counts.enums_both += 1;
                if diff_enum(name, b, t) {
                    counts.enums_diverged += 1;
                }
            }
            (Some(_), None) => counts.enum_only_base.push(name.clone()),
            (None, Some(_)) => counts.enum_only_target.push(name.clone()),
            (None, None) => unreachable!(),
        }
    }

    report_presence(
        "classes/structs/unions",
        &counts.only_base,
        &counts.only_target,
        cfg,
    );
    report_presence(
        "enums",
        &counts.enum_only_base,
        &counts.enum_only_target,
        cfg,
    );

    println!("---- header summary ----");
    println!(
        "types:  {both} compared, {div} diverged ({size} size, {member} member, {order} fn-order, \
         {vis} visibility); base-only {ob}, target-only {ot}",
        both = counts.both,
        div = counts.diverged,
        size = counts.size_diff,
        member = counts.member_diff,
        order = counts.order_diff,
        vis = counts.visibility_diff,
        ob = counts.only_base.len(),
        ot = counts.only_target.len(),
    );
    println!(
        "enums:  {both} compared, {div} diverged; base-only {ob}, target-only {ot}\n",
        both = counts.enums_both,
        div = counts.enums_diverged,
        ob = counts.enum_only_base.len(),
        ot = counts.enum_only_target.len(),
    );
}

/// Returns `true` if `b` and `t` diverge in any way.
fn diff_class(name: &str, b: &ClassModel, t: &ClassModel, counts: &mut HeaderCounts) -> bool {
    let mut lines: Vec<String> = Vec::new();

    if b.size != t.size {
        counts.size_diff += 1;
        lines.push(format!(
            "  [size]   base=0x{:X}  target=0x{:X}",
            b.size, t.size
        ));
    }

    let fields = diff_fields(&b.fields, &t.fields);
    if fields.diverged() {
        counts.member_diff += 1;
        lines.push("  [member]".to_string());
        for (n, bf, tf) in &fields.changed {
            lines.push(format!(
                "    changed  {n}: base({} @0x{:X})  target({} @0x{:X})",
                bf.0, bf.1, tf.0, tf.1
            ));
        }
        push_list(&mut lines, "    only-base  ", &fields.only_base);
        push_list(&mut lines, "    only-tgt   ", &fields.only_target);
        push_list(&mut lines, "    reordered  ", &fields.moved);
    }

    let base_sigs: Vec<String> = b.methods.iter().map(|m| m.sig.clone()).collect();
    let target_sigs: Vec<String> = t.methods.iter().map(|m| m.sig.clone()).collect();
    let methods = seq_diff(&base_sigs, &target_sigs);
    if methods.diverged() {
        counts.order_diff += 1;
        lines.push("  [fn-order]".to_string());
        push_list(&mut lines, "    only-base  ", &methods.only_base);
        push_list(&mut lines, "    only-tgt   ", &methods.only_target);
        push_list(&mut lines, "    moved      ", &methods.moved);
    }

    // Access (visibility) only applies to members present on BOTH sides, joined
    // by name (data) / signature (methods). A one-sided member is already
    // surfaced by [member]/[fn-order] above, so we never double-report it here.
    let vis = diff_visibility(b, t, &fields, &methods);
    if !vis.is_empty() {
        counts.visibility_diff += vis.len();
        lines.push("  [visibility]".to_string());
        for (member, base_access, tgt_access) in &vis {
            lines.push(format!(
                "    {member}: base={} tgt={}",
                access_name(*base_access),
                access_name(*tgt_access),
            ));
        }
    }

    if lines.is_empty() {
        return false;
    }

    println!("{kind} {name}", kind = b.kind);
    for line in lines {
        println!("{line}");
    }
    println!();
    true
}

fn diff_enum(name: &str, b: &EnumModel, t: &EnumModel) -> bool {
    let mut lines: Vec<String> = Vec::new();

    if b.underlying != t.underlying {
        lines.push(format!(
            "  [underlying]  base={}  target={}",
            b.underlying, t.underlying
        ));
    }

    let base_vals: Vec<((String, i64), String)> = b
        .values
        .iter()
        .map(|(n, v)| (("".to_string(), *v), n.clone()))
        .collect();
    let target_vals: Vec<((String, i64), String)> = t
        .values
        .iter()
        .map(|(n, v)| (("".to_string(), *v), n.clone()))
        .collect();
    let vals = value_match(&base_vals, &target_vals);

    if vals.diverged() {
        lines.push("  [values]".to_string());
        for (key, bn, tn) in &vals.misnamed {
            lines.push(format!(
                "    misname  0x{:X}  base={bn}  target={tn}",
                key.1
            ));
        }
        for (key, n) in &vals.only_base {
            lines.push(format!("    only-base  {n} = 0x{:X}", key.1));
        }
        for (key, n) in &vals.only_target {
            lines.push(format!("    only-tgt   {n} = 0x{:X}", key.1));
        }
    }

    if lines.is_empty() {
        return false;
    }

    println!("enum {name}");
    for line in lines {
        println!("{line}");
    }
    println!();
    true
}

// ── Source reporting ────────────────────────────────────────────────────────

fn report_sources(base: &SideModel, target: &SideModel, cfg: &Config) {
    println!("================ SOURCES ================\n");

    let mut counts = SourceCounts::default();

    let keys = union_keys(base.files.keys(), target.files.keys());
    for path in keys {
        match (base.files.get(path), target.files.get(path)) {
            (Some(b), Some(t)) => {
                counts.both += 1;
                if diff_file(path, b, t, &mut counts) {
                    counts.diverged += 1;
                }
            }
            (Some(_), None) => counts.only_base.push(path.clone()),
            (None, Some(_)) => counts.only_target.push(path.clone()),
            (None, None) => unreachable!(),
        }
    }

    report_presence("files", &counts.only_base, &counts.only_target, cfg);

    let presence = report_presence_functions(base, target, cfg);

    println!("---- source summary ----");
    println!(
        "files:  {both} compared, {div} diverged; base-only {ob}, target-only {ot}",
        both = counts.both,
        div = counts.diverged,
        ob = counts.only_base.len(),
        ot = counts.only_target.len(),
    );
    println!(
        "        {order} files w/ fn-order diff, {stmt} functions w/ stmt-count diff, \
         {cst} functions w/ const diff",
        order = counts.order_diff,
        stmt = counts.stmt_diff,
        cst = counts.const_diff,
    );
    println!(
        "        out-of-line presence: {pb} base-only (we emit standalone; target inlines), \
         {pt} target-only (target emits standalone; we inline / no source)\n",
        pb = presence.0,
        pt = presence.1,
    );
}

/// Per-function out-of-line PRESENCE divergence, across the whole source corpus.
///
/// `extract_sources` collected, per side, the set of functions that have a
/// standalone out-of-line body (a real code symbol / compiland definition with
/// statements). Joining those two sets by `(engine-relative path, qualified
/// signature)` — the same key the `[stmt]`/`[const]` per-function diffs use —
/// a function present in exactly one side's set is an out-of-line presence
/// divergence:
///
/// * **base-only**: we emit the function out-of-line but the target inlines it
///   (no standalone body there) — a noinline/forceinline decision.
/// * **tgt-only**: the target emits the function out-of-line but our base
///   inlines it / it is a `/* no source */` (inlined-only) function — a real
///   reconstruction target: the target's standalone symbol gives us a body.
///
/// This is deliberately distinct from `[fn-order]`: that reports the relative
/// definition ORDER of functions present on *both* sides within a matched file
/// (mirroring the header `[fn-order]` decl-order check), so the one-sided lists
/// are owned here and stripped from the source `[fn-order]` to avoid
/// double-reporting. Returns `(base_only_count, target_only_count)`.
///
/// Note: the join key is whatever `emit_function_orig` formats, so where that
/// rendering differs between the two PDBs for the *same* logical function — e.g.
/// the static-init/atexit thunks the base still renders mangled
/// (`??__E…`) while the target renders demangled (`` `dynamic initializer
/// for '…'' ``) — the function shows as paired one-sided entries (one base-only,
/// one tgt-only). That is a pre-existing formatter-fidelity gap shared with the
/// `[stmt]`/`[fn-order]` joins, not a real presence divergence; the genuine
/// reconstruction targets are the non-thunk `tgt-only` entries.
fn report_presence_functions(base: &SideModel, target: &SideModel, cfg: &Config) -> (usize, usize) {
    let presence = presence_functions(base, target);

    if cfg.list_presence_fns && (!presence.base_only.is_empty() || !presence.target_only.is_empty())
    {
        if !presence.base_only.is_empty() {
            println!(
                "---- base-only out-of-line functions ({}) ----",
                presence.base_only.len()
            );
            for (path, sig) in &presence.base_only {
                println!("  [presence] base-only  {path}  {sig}");
            }
        }
        if !presence.target_only.is_empty() {
            println!(
                "---- target-only out-of-line functions ({}) ----",
                presence.target_only.len()
            );
            for (path, sig) in &presence.target_only {
                println!("  [presence] tgt-only   {path}  {sig}");
            }
        }
        println!();
    }

    (presence.base_only.len(), presence.target_only.len())
}

struct PresenceDiff {
    base_only: Vec<(String, String)>,
    target_only: Vec<(String, String)>,
}

/// Build the `(path, signature)` sets of functions present out-of-line on
/// exactly one side. Multiple out-of-line bodies sharing a `(path, signature)`
/// key (overloads collapsing under the same formatted signature) are paired
/// positionally, so only a genuine count surplus on one side is reported.
fn presence_functions(base: &SideModel, target: &SideModel) -> PresenceDiff {
    let mut base_only = Vec::new();
    let mut target_only = Vec::new();

    let paths = union_keys(base.files.keys(), target.files.keys());
    for path in paths {
        let base_sigs = file_signatures(base.files.get(path));
        let target_sigs = file_signatures(target.files.get(path));

        let mut target_counts: HashMap<&str, usize> = HashMap::new();
        for sig in &target_sigs {
            *target_counts.entry(sig.as_str()).or_default() += 1;
        }
        let mut base_counts: HashMap<&str, usize> = HashMap::new();
        for sig in &base_sigs {
            *base_counts.entry(sig.as_str()).or_default() += 1;
        }

        // base-only: a body on base with no remaining target counterpart.
        let mut consumed: HashMap<&str, usize> = HashMap::new();
        for sig in &base_sigs {
            let used = consumed.entry(sig.as_str()).or_default();
            if *used < target_counts.get(sig.as_str()).copied().unwrap_or(0) {
                *used += 1;
            } else {
                base_only.push((path.clone(), sig.clone()));
            }
        }
        // target-only: the mirror.
        let mut consumed: HashMap<&str, usize> = HashMap::new();
        for sig in &target_sigs {
            let used = consumed.entry(sig.as_str()).or_default();
            if *used < base_counts.get(sig.as_str()).copied().unwrap_or(0) {
                *used += 1;
            } else {
                target_only.push((path.clone(), sig.clone()));
            }
        }
    }

    PresenceDiff {
        base_only,
        target_only,
    }
}

fn file_signatures(file: Option<&FileModel>) -> Vec<String> {
    file.map(|f| {
        f.functions
            .iter()
            .map(|fun| fun.name_orig.clone())
            .collect()
    })
    .unwrap_or_default()
}

fn diff_file(path: &str, b: &FileModel, t: &FileModel, counts: &mut SourceCounts) -> bool {
    let mut lines: Vec<String> = Vec::new();

    // [fn-order] reports only the relative DEFINITION ORDER of functions present
    // out-of-line on BOTH sides (the `moved` set). Functions present out-of-line
    // on exactly one side are an out-of-line PRESENCE divergence, owned by the
    // global [presence] report (report_presence_functions) so we never
    // double-report a one-sided body here.
    let base_order: Vec<String> = b.functions.iter().map(|f| f.name_orig.clone()).collect();
    let target_order: Vec<String> = t.functions.iter().map(|f| f.name_orig.clone()).collect();
    let order = seq_diff(&base_order, &target_order);
    if !order.moved.is_empty() {
        counts.order_diff += 1;
        lines.push("  [fn-order]".to_string());
        push_list(&mut lines, "    moved      ", &order.moved);
    }

    // Per-function stmt/const comparison over functions present on both sides.
    let target_by_name: HashMap<&str, &FnModel> = t
        .functions
        .iter()
        .map(|f| (f.name_orig.as_str(), f))
        .collect();

    for bf in &b.functions {
        let Some(tf) = target_by_name.get(bf.name_orig.as_str()) else {
            continue;
        };

        if bf.statement_count != tf.statement_count {
            counts.stmt_diff += 1;
            lines.push(format!(
                "  [stmt]   {}: base={} target={}",
                bf.name_orig, bf.statement_count, tf.statement_count
            ));
        }

        let base_consts: Vec<((String, i64), String)> = bf
            .constants
            .iter()
            .map(|c| ((c.type_name.clone(), c.value), c.name.clone()))
            .collect();
        let target_consts: Vec<((String, i64), String)> = tf
            .constants
            .iter()
            .map(|c| ((c.type_name.clone(), c.value), c.name.clone()))
            .collect();
        let consts = value_match(&base_consts, &target_consts);

        if consts.diverged() {
            counts.const_diff += 1;
            lines.push(format!("  [const]  {}", bf.name_orig));
            for (key, bn, tn) in &consts.misnamed {
                lines.push(format!(
                    "    misname  {} = {}  base={bn}  target={tn}",
                    key.0, key.1
                ));
            }
            for (key, n) in &consts.only_base {
                lines.push(format!("    only-base  {} {} = {}", key.0, n, key.1));
            }
            for (key, n) in &consts.only_target {
                lines.push(format!("    only-tgt   {} {} = {}", key.0, n, key.1));
            }
        }
    }

    if lines.is_empty() {
        return false;
    }

    println!("{path}");
    for line in lines {
        println!("{line}");
    }
    println!();
    true
}

// ── Diff primitives ─────────────────────────────────────────────────────────

struct SeqDiff {
    only_base: Vec<String>,
    only_target: Vec<String>,
    moved: Vec<String>,
}

impl SeqDiff {
    fn diverged(&self) -> bool {
        !self.only_base.is_empty() || !self.only_target.is_empty() || !self.moved.is_empty()
    }
}

/// Compare two ordered key sequences. One-sided keys are reported as presence
/// diffs; among the keys common to both, those not on a longest common
/// subsequence are reported as `moved` (i.e. their relative order changed).
fn seq_diff(base: &[String], target: &[String]) -> SeqDiff {
    let bset: HashSet<&String> = base.iter().collect();
    let tset: HashSet<&String> = target.iter().collect();

    let only_base = dedup_filter(base, |k| !tset.contains(k));
    let only_target = dedup_filter(target, |k| !bset.contains(k));

    let common_base: Vec<&String> = base.iter().filter(|k| tset.contains(*k)).collect();
    let common_target: Vec<&String> = target.iter().filter(|k| bset.contains(*k)).collect();

    let lcs = lcs_set(&common_base, &common_target);
    let mut seen = HashSet::new();
    let mut moved = Vec::new();
    for k in &common_base {
        if !lcs.contains(*k) && seen.insert((*k).clone()) {
            moved.push((*k).clone());
        }
    }

    SeqDiff {
        only_base,
        only_target,
        moved,
    }
}

fn lcs_set(a: &[&String], b: &[&String]) -> HashSet<String> {
    let n = a.len();
    let m = b.len();
    let mut dp = vec![vec![0u32; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            dp[i][j] = if a[i] == b[j] {
                dp[i + 1][j + 1] + 1
            } else {
                dp[i + 1][j].max(dp[i][j + 1])
            };
        }
    }

    let mut set = HashSet::new();
    let (mut i, mut j) = (0, 0);
    while i < n && j < m {
        if a[i] == b[j] {
            set.insert(a[i].clone());
            i += 1;
            j += 1;
        } else if dp[i + 1][j] >= dp[i][j + 1] {
            i += 1;
        } else {
            j += 1;
        }
    }
    set
}

struct FieldDiff {
    changed: Vec<(String, (String, u64), (String, u64))>,
    only_base: Vec<String>,
    only_target: Vec<String>,
    moved: Vec<String>,
}

impl FieldDiff {
    fn diverged(&self) -> bool {
        !self.changed.is_empty()
            || !self.only_base.is_empty()
            || !self.only_target.is_empty()
            || !self.moved.is_empty()
    }
}

fn diff_fields(base: &[FieldModel], target: &[FieldModel]) -> FieldDiff {
    let base_names: Vec<String> = base.iter().map(|f| f.name.clone()).collect();
    let target_names: Vec<String> = target.iter().map(|f| f.name.clone()).collect();
    let order = seq_diff(&base_names, &target_names);

    let target_by_name: HashMap<&str, &FieldModel> =
        target.iter().map(|f| (f.name.as_str(), f)).collect();

    let mut changed = Vec::new();
    let mut seen = HashSet::new();
    for bf in base {
        if !seen.insert(bf.name.clone()) {
            continue;
        }
        if let Some(tf) = target_by_name.get(bf.name.as_str()) {
            if bf.type_name != tf.type_name || bf.offset != tf.offset {
                changed.push((
                    bf.name.clone(),
                    (bf.type_name.clone(), bf.offset),
                    (tf.type_name.clone(), tf.offset),
                ));
            }
        }
    }

    FieldDiff {
        changed,
        only_base: order.only_base,
        only_target: order.only_target,
        moved: order.moved,
    }
}

/// Access mismatches for members present on BOTH sides. Data members join by
/// name, methods by formatted signature; the join keys flagged one-sided by the
/// preceding `[member]`/`[fn-order]` diffs are excluded so visibility never
/// double-reports a member that is really just added/removed. `unspecified` (0,
/// e.g. compiler thunks) inherits the class default and is not compared.
///
/// Returns `(member, base_access, target_access)` triples.
fn diff_visibility(
    b: &ClassModel,
    t: &ClassModel,
    fields: &FieldDiff,
    methods: &SeqDiff,
) -> Vec<(String, u8, u8)> {
    let mut out = Vec::new();

    // ── data members (join by name) ──
    let field_one_sided: HashSet<&str> = fields
        .only_base
        .iter()
        .chain(fields.only_target.iter())
        .map(String::as_str)
        .collect();
    let mut tgt_fields: HashMap<&str, u8> = HashMap::new();
    for f in &t.fields {
        tgt_fields.entry(f.name.as_str()).or_insert(f.access);
    }
    let mut seen = HashSet::new();
    for bf in &b.fields {
        if field_one_sided.contains(bf.name.as_str()) || !seen.insert(bf.name.as_str()) {
            continue;
        }
        if let Some(&ta) = tgt_fields.get(bf.name.as_str()) {
            if access_differs(bf.access, ta) {
                out.push((bf.name.clone(), bf.access, ta));
            }
        }
    }

    // ── member functions (join by signature) ──
    let method_one_sided: HashSet<&str> = methods
        .only_base
        .iter()
        .chain(methods.only_target.iter())
        .map(String::as_str)
        .collect();
    let mut tgt_methods: HashMap<&str, u8> = HashMap::new();
    for m in &t.methods {
        tgt_methods.entry(m.sig.as_str()).or_insert(m.access);
    }
    let mut seen = HashSet::new();
    for bm in &b.methods {
        if method_one_sided.contains(bm.sig.as_str()) || !seen.insert(bm.sig.as_str()) {
            continue;
        }
        if let Some(&ta) = tgt_methods.get(bm.sig.as_str()) {
            if access_differs(bm.access, ta) {
                out.push((bm.sig.clone(), bm.access, ta));
            }
        }
    }

    out
}

/// True only when both sides carry a *specified* access (1/2/3) that disagree.
/// `unspecified` (0) inherits the class default and is treated as a non-diff.
fn access_differs(base: u8, target: u8) -> bool {
    base != 0 && target != 0 && base != target
}

fn access_name(access: u8) -> &'static str {
    match access {
        1 => "private",
        2 => "protected",
        3 => "public",
        _ => "unspecified",
    }
}

struct ValueDiff {
    misnamed: Vec<((String, i64), String, String)>,
    only_base: Vec<((String, i64), String)>,
    only_target: Vec<((String, i64), String)>,
}

impl ValueDiff {
    fn diverged(&self) -> bool {
        !self.misnamed.is_empty() || !self.only_base.is_empty() || !self.only_target.is_empty()
    }
}

/// Match two name lists keyed by `(type, value)`. Same key, same name → fine;
/// same key, different name → misname; unmatched → one-sided. Multiple entries
/// sharing a key are paired positionally.
fn value_match(base: &[((String, i64), String)], target: &[((String, i64), String)]) -> ValueDiff {
    let mut base_by_key: HashMap<(String, i64), Vec<String>> = HashMap::new();
    for (key, name) in base {
        base_by_key
            .entry(key.clone())
            .or_default()
            .push(name.clone());
    }
    let mut target_by_key: HashMap<(String, i64), Vec<String>> = HashMap::new();
    for (key, name) in target {
        target_by_key
            .entry(key.clone())
            .or_default()
            .push(name.clone());
    }

    let mut keys: Vec<(String, i64)> = base_by_key.keys().cloned().collect();
    for key in target_by_key.keys() {
        if !base_by_key.contains_key(key) {
            keys.push(key.clone());
        }
    }
    keys.sort();

    let mut misnamed = Vec::new();
    let mut only_base = Vec::new();
    let mut only_target = Vec::new();

    for key in keys {
        let bn = base_by_key.get(&key).cloned().unwrap_or_default();
        let tn = target_by_key.get(&key).cloned().unwrap_or_default();
        let paired = bn.len().min(tn.len());

        for i in 0..paired {
            if bn[i] != tn[i] {
                misnamed.push((key.clone(), bn[i].clone(), tn[i].clone()));
            }
        }
        for name in bn.into_iter().skip(paired) {
            only_base.push((key.clone(), name));
        }
        for name in tn.into_iter().skip(paired) {
            only_target.push((key.clone(), name));
        }
    }

    ValueDiff {
        misnamed,
        only_base,
        only_target,
    }
}

// ── Shared helpers ──────────────────────────────────────────────────────────

#[derive(Default)]
struct HeaderCounts {
    both: usize,
    diverged: usize,
    size_diff: usize,
    member_diff: usize,
    order_diff: usize,
    visibility_diff: usize,
    only_base: Vec<String>,
    only_target: Vec<String>,
    enums_both: usize,
    enums_diverged: usize,
    enum_only_base: Vec<String>,
    enum_only_target: Vec<String>,
}

#[derive(Default)]
struct SourceCounts {
    both: usize,
    diverged: usize,
    order_diff: usize,
    stmt_diff: usize,
    const_diff: usize,
    only_base: Vec<String>,
    only_target: Vec<String>,
}

fn union_keys<'a>(
    a: impl Iterator<Item = &'a String>,
    b: impl Iterator<Item = &'a String>,
) -> Vec<&'a String> {
    let mut keys: Vec<&String> = a.chain(b).collect();
    keys.sort();
    keys.dedup();
    keys
}

fn report_presence(label: &str, only_base: &[String], only_target: &[String], cfg: &Config) {
    if only_base.is_empty() && only_target.is_empty() {
        return;
    }
    if cfg.list_presence {
        if !only_base.is_empty() {
            println!("---- base-only {label} ({}) ----", only_base.len());
            for name in only_base {
                println!("  {name}");
            }
        }
        if !only_target.is_empty() {
            println!("---- target-only {label} ({}) ----", only_target.len());
            for name in only_target {
                println!("  {name}");
            }
        }
        println!();
    }
}

fn push_list(lines: &mut Vec<String>, prefix: &str, items: &[String]) {
    if items.is_empty() {
        return;
    }
    lines.push(format!("{prefix}{}", items.join(", ")));
}

fn dedup_filter(seq: &[String], keep: impl Fn(&String) -> bool) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for k in seq {
        if keep(k) && seen.insert(k.clone()) {
            out.push(k.clone());
        }
    }
    out
}

fn include_type(qualified_name: &str, cfg: &Config) -> bool {
    if !cfg.include_external {
        for root in EXTERNAL_ROOTS {
            if qualified_name.starts_with(root) {
                return false;
            }
        }
    }
    !skipped(qualified_name, cfg)
}

fn skipped(name: &str, cfg: &Config) -> bool {
    if cfg.skip.is_empty() {
        return false;
    }
    let lowered = name.to_lowercase();
    cfg.skip.iter().any(|pat| lowered.contains(pat))
}

/// Drop PDB-internal `TypeIndex( 0x… )` payloads that leak into the formatter's
/// "unhandled type" debug fallback (most visibly for bitfields). The numeric
/// index names a slot in *this* PDB's type stream and so differs between two
/// PDBs for the very same logical type; leaving it in would flag every
/// bitfielded struct as a spurious member divergence. The rest of the rendered
/// shape (bitfield length/position, surrounding type) is preserved and still
/// compared.
fn clean_type(s: String) -> String {
    if !s.contains("TypeIndex(") {
        return s;
    }

    let mut out = String::with_capacity(s.len());
    let mut rest = s.as_str();
    while let Some(pos) = rest.find("TypeIndex(") {
        out.push_str(&rest[..pos]);
        out.push_str("TypeIndex(..)");
        rest = &rest[pos + "TypeIndex(".len()..];
        match rest.find(')') {
            Some(close) => rest = &rest[close + 1..],
            None => {
                rest = "";
                break;
            }
        }
    }
    out.push_str(rest);
    out
}

fn kind_label(kind: pdb::ClassKind) -> &'static str {
    match kind {
        pdb::ClassKind::Class => "class",
        pdb::ClassKind::Struct => "struct",
        pdb::ClassKind::Interface => "interface",
    }
}

fn variant_to_i64(value: pdb::Variant) -> i64 {
    match value {
        pdb::Variant::U8(v) => v as i64,
        pdb::Variant::U16(v) => v as i64,
        pdb::Variant::U32(v) => v as i64,
        pdb::Variant::U64(v) => v as i64,
        pdb::Variant::I8(v) => v as i64,
        pdb::Variant::I16(v) => v as i64,
        pdb::Variant::I32(v) => v as i64,
        pdb::Variant::I64(v) => v,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn seq_diff_identical_is_clean() {
        let diff = seq_diff(&s(&["a", "b", "c"]), &s(&["a", "b", "c"]));
        assert!(!diff.diverged());
    }

    #[test]
    fn seq_diff_reports_one_sided() {
        let diff = seq_diff(&s(&["a", "b", "c"]), &s(&["a", "c", "d"]));
        assert_eq!(diff.only_base, s(&["b"]));
        assert_eq!(diff.only_target, s(&["d"]));
        // a and c keep their relative order, so nothing is "moved".
        assert!(diff.moved.is_empty());
    }

    #[test]
    fn seq_diff_reports_reorder() {
        // Common set {a,b,c}; target swaps a after c. The minimal move is one
        // element falling off the longest common subsequence.
        let diff = seq_diff(&s(&["a", "b", "c"]), &s(&["b", "c", "a"]));
        assert!(diff.only_base.is_empty());
        assert!(diff.only_target.is_empty());
        assert_eq!(diff.moved, s(&["a"]));
    }

    #[test]
    fn value_match_pairs_by_value_not_name() {
        // Same (type, value) but different names → misname, not add/remove.
        let base = vec![(("s32".to_string(), 5), "THRESHOLD".to_string())];
        let target = vec![(("s32".to_string(), 5), "LIMIT".to_string())];
        let diff = value_match(&base, &target);
        assert_eq!(
            diff.misnamed,
            vec![(
                ("s32".to_string(), 5),
                "THRESHOLD".to_string(),
                "LIMIT".to_string()
            )]
        );
        assert!(diff.only_base.is_empty());
        assert!(diff.only_target.is_empty());
    }

    #[test]
    fn value_match_same_name_is_clean() {
        let base = vec![(("s32".to_string(), 5), "MAX".to_string())];
        let target = vec![(("s32".to_string(), 5), "MAX".to_string())];
        assert!(!value_match(&base, &target).diverged());
    }

    #[test]
    fn value_match_unmatched_value_is_one_sided() {
        // Different value → not a misname; it is a genuine add/remove.
        let base = vec![(("s32".to_string(), 5), "A".to_string())];
        let target = vec![(("s32".to_string(), 7), "A".to_string())];
        let diff = value_match(&base, &target);
        assert!(diff.misnamed.is_empty());
        assert_eq!(
            diff.only_base,
            vec![(("s32".to_string(), 5), "A".to_string())]
        );
        assert_eq!(
            diff.only_target,
            vec![(("s32".to_string(), 7), "A".to_string())]
        );
    }

    #[test]
    fn access_differs_ignores_unspecified() {
        // Both specified and disagreeing → diff.
        assert!(access_differs(3, 1));
        // Equal → no diff.
        assert!(!access_differs(3, 3));
        // Either side unspecified inherits the class default → no diff.
        assert!(!access_differs(0, 3));
        assert!(!access_differs(3, 0));
        assert!(!access_differs(0, 0));
    }

    fn class_with(fields: &[(&str, u8)], methods: &[(&str, u8)]) -> ClassModel {
        ClassModel {
            kind: "class",
            size: 0,
            fields: fields
                .iter()
                .map(|(n, a)| FieldModel {
                    offset: 0,
                    type_name: "int".to_string(),
                    name: n.to_string(),
                    access: *a,
                })
                .collect(),
            methods: methods
                .iter()
                .map(|(s, a)| MethodModel {
                    sig: s.to_string(),
                    access: *a,
                })
                .collect(),
        }
    }

    fn empty_seq() -> SeqDiff {
        SeqDiff {
            only_base: vec![],
            only_target: vec![],
            moved: vec![],
        }
    }

    #[test]
    fn diff_visibility_flags_matched_mismatch() {
        let b = class_with(&[("m_foo", 3)], &[("void f()", 1)]);
        let t = class_with(&[("m_foo", 1)], &[("void f()", 3)]);
        let fields = diff_fields(&b.fields, &t.fields);
        let vis = diff_visibility(&b, &t, &fields, &empty_seq());
        assert_eq!(
            vis,
            vec![("m_foo".to_string(), 3, 1), ("void f()".to_string(), 1, 3),]
        );
    }

    #[test]
    fn diff_visibility_skips_one_sided_members() {
        // m_only is base-only (a [member] diff), so it must NOT be reported as a
        // visibility diff even though the matched member m_foo agrees.
        let b = class_with(&[("m_foo", 3), ("m_only", 1)], &[]);
        let t = class_with(&[("m_foo", 3)], &[]);
        let fields = diff_fields(&b.fields, &t.fields);
        assert!(fields.diverged());
        let vis = diff_visibility(&b, &t, &fields, &empty_seq());
        assert!(vis.is_empty());
    }

    fn side_with(files: &[(&str, &[&str])]) -> SideModel {
        let mut out = SideModel::default();
        for (path, sigs) in files {
            out.files.insert(
                path.to_string(),
                FileModel {
                    functions: sigs
                        .iter()
                        .map(|sig| FnModel {
                            name_orig: sig.to_string(),
                            statement_count: 0,
                            constants: Vec::new(),
                        })
                        .collect(),
                },
            );
        }
        out
    }

    #[test]
    fn presence_self_compare_is_clean() {
        // Identical source-function sets on both sides → zero presence diffs.
        // This is the binary's `self-compare MUST report 0 [presence]` invariant.
        let side = side_with(&[
            ("a/x.cpp", &["void a::f()", "int a::g()"]),
            ("b/y.cpp", &["void b::h()"]),
        ]);
        let other = side_with(&[
            ("a/x.cpp", &["void a::f()", "int a::g()"]),
            ("b/y.cpp", &["void b::h()"]),
        ]);
        let diff = presence_functions(&side, &other);
        assert!(diff.base_only.is_empty());
        assert!(diff.target_only.is_empty());
    }

    #[test]
    fn presence_flags_one_sided_out_of_line_bodies() {
        // base emits f() and g() out-of-line; target emits f() and h(). So:
        //   g() is base-only  (we emit standalone, target inlines it)
        //   h() is tgt-only   (target emits standalone, we inline it / no source)
        // f() is on both sides → NOT a presence diff (it is the [fn-order] domain).
        let base = side_with(&[("m/u.cpp", &["void f()", "int g()"])]);
        let target = side_with(&[("m/u.cpp", &["void f()", "void h()"])]);
        let diff = presence_functions(&base, &target);
        assert_eq!(
            diff.base_only,
            vec![("m/u.cpp".to_string(), "int g()".to_string())]
        );
        assert_eq!(
            diff.target_only,
            vec![("m/u.cpp".to_string(), "void h()".to_string())]
        );
    }

    #[test]
    fn presence_covers_one_sided_files() {
        // A whole file present out-of-line on only one side: every function in it
        // is a presence diff for that side (the file-level presence list reports
        // the file; this reports its bodies as reconstruction targets).
        let base = side_with(&[("only/base.cpp", &["void only_base()"])]);
        let target = side_with(&[("only/tgt.cpp", &["void only_tgt()"])]);
        let diff = presence_functions(&base, &target);
        assert_eq!(
            diff.base_only,
            vec![("only/base.cpp".to_string(), "void only_base()".to_string())]
        );
        assert_eq!(
            diff.target_only,
            vec![("only/tgt.cpp".to_string(), "void only_tgt()".to_string())]
        );
    }

    #[test]
    fn presence_pairs_duplicate_signatures_positionally() {
        // Two out-of-line bodies share a formatted signature on base, one on
        // target → exactly one surplus base-only body, none target-only.
        let base = side_with(&[("m/u.cpp", &["void f()", "void f()"])]);
        let target = side_with(&[("m/u.cpp", &["void f()"])]);
        let diff = presence_functions(&base, &target);
        assert_eq!(
            diff.base_only,
            vec![("m/u.cpp".to_string(), "void f()".to_string())]
        );
        assert!(diff.target_only.is_empty());
    }

    #[test]
    fn clean_type_strips_pdb_type_indices() {
        let a = clean_type(
            "Bitfield( BitfieldType { underlying_type: TypeIndex( 0x65038 ), length: 4 } )"
                .to_string(),
        );
        let b = clean_type(
            "Bitfield( BitfieldType { underlying_type: TypeIndex( 0x68899 ), length: 4 } )"
                .to_string(),
        );
        // Same logical bitfield, different per-PDB index → equal after cleaning.
        assert_eq!(a, b);
        assert!(a.contains("length: 4"));
        // A plain type is returned untouched.
        assert_eq!(clean_type("s32".to_string()), "s32");
    }
}
