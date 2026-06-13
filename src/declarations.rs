//! Declarations dump: every function the original source *declared*, as JSONL.
//!
//! Methods come from the type stream's class field lists (LF_CLASS/LF_STRUCTURE
//! -> LF_ONEMETHOD/LF_METHOD with LF_MFUNCTION types) — the same records
//! `gen_headers` reconstructs headers from. Crucially this includes methods that
//! were inlined at every call site and therefore emit NO symbol in the binary.
//!
//! Free functions can't be recovered that way (VS2008 PDBs have no LF_FUNC_ID),
//! so they come from the module streams' S_GPROC32/S_LPROC32 records whose type
//! is LF_PROCEDURE — i.e. only the free functions that emitted a body somewhere.
//!
//! Types are rendered by the same formatter the header generator uses, so the
//! signatures use the engine-style primitive names (`u8`, `s32`, ...).

use std::collections::BTreeSet;
use std::io::Write;

use pdb::{FallibleIterator, ItemIndex, PDB};
use pdb_addr2line::type_parser::{AttributeFlags, Function, ReturnType};
use serde::Serialize;

use crate::pdb_parser::PdbParser;

/// One declared function. Field order is the JSON field order; the derived
/// `Ord` (class, name, signature, ...) is the output order.
#[derive(Serialize, PartialEq, Eq, PartialOrd, Ord)]
pub struct Declaration {
    /// Full class name; `null` for free functions.
    pub class: Option<String>,
    pub name: String,
    /// `<return type> (<parameter types>)`; constructors/destructors have no
    /// return type and render as `(<parameter types>)`.
    pub signature: String,
    /// "public" | "protected" | "private"; `null` for free functions.
    pub access: Option<&'static str>,
    pub is_virtual: bool,
    pub is_static: bool,
    pub is_const: bool,
    /// "method" | "free".
    pub kind: &'static str,
}

pub fn dump_declarations(pdb_path: &std::path::Path, out: &mut impl Write) -> crate::Result<usize> {
    let mut rows = BTreeSet::new();

    PdbParser::with(pdb_path, |formatter| {
        let file = std::fs::File::open(pdb_path)?;
        let mut pdb = PDB::open(file)?;

        collect_methods(&mut pdb, &formatter, &mut rows)?;
        collect_free_functions(&mut pdb, &formatter, &mut rows)?;

        Ok(())
    })?;

    for row in &rows {
        serde_json::to_writer(&mut *out, row)?;
        writeln!(out)?;
    }

    Ok(rows.len())
}

//
// Methods: walk the class field lists, exactly the records gen_headers uses.
//

fn collect_methods(
    pdb: &mut PDB<std::fs::File>,
    formatter: &PdbParser,
    rows: &mut BTreeSet<Declaration>,
) -> crate::Result<()> {
    let type_information = pdb.type_information()?;
    let type_finder = {
        let mut type_finder = type_information.finder();
        let mut type_iter = type_information.iter();
        while type_iter.next()?.is_some() {
            type_finder.update(&type_iter);
        }
        type_finder
    };

    let mut type_iter = type_information.iter();
    while let Some(item) = type_iter.next()? {
        let Ok(pdb::TypeData::Class(class)) = item.parse() else {
            continue;
        };
        if class.properties.forward_reference() {
            continue;
        }
        let Some(fields) = class.fields else {
            continue;
        };

        let class_name = class.name.to_string().to_string();
        add_field_list_methods(formatter, &type_finder, &class_name, fields, rows)?;
    }

    Ok(())
}

fn add_field_list_methods(
    formatter: &PdbParser,
    type_finder: &pdb::TypeFinder<'_>,
    class_name: &str,
    field_list: pdb::TypeIndex,
    rows: &mut BTreeSet<Declaration>,
) -> crate::Result<()> {
    let pdb::TypeData::FieldList(data) = type_finder.find(field_list)?.parse()? else {
        return Ok(());
    };

    for field in &data.fields {
        match *field {
            pdb::TypeData::Method(ref data) => add_method(
                formatter,
                class_name,
                data.name,
                data.attributes,
                data.method_type,
                rows,
            ),
            pdb::TypeData::OverloadedMethod(ref data) => {
                let pdb::TypeData::MethodList(method_list) =
                    type_finder.find(data.method_list)?.parse()?
                else {
                    continue;
                };
                for entry in method_list.methods {
                    add_method(
                        formatter,
                        class_name,
                        data.name,
                        entry.attributes,
                        entry.method_type,
                        rows,
                    );
                }
            }
            _ => (),
        }
    }

    if let Some(continuation) = data.continuation {
        add_field_list_methods(formatter, type_finder, class_name, continuation, rows)?;
    }

    Ok(())
}

fn add_method(
    formatter: &PdbParser,
    class_name: &str,
    name: pdb::RawString,
    attributes: pdb::FieldAttributes,
    method_type: pdb::TypeIndex,
    rows: &mut BTreeSet<Declaration>,
) {
    // Compiler-generated helpers, never declared in source.
    match name.as_bytes() {
        b"__vecDelDtor" | b"__local_vftable_ctor_closure" | b"__dflt_ctor_closure" => return,
        _ => (),
    }

    // The type stream is shared across modules, so module 0 resolves it.
    let Ok(fn_t) = formatter.parse_function(&name, 0, method_type) else {
        return;
    };

    rows.insert(Declaration {
        class: Some(class_name.to_string()),
        name: fn_t.name.clone(),
        signature: render_signature(&fn_t),
        access: access_name(attributes.access()),
        is_virtual: attributes.is_virtual()
            || attributes.is_pure_virtual()
            || attributes.is_intro_virtual(),
        is_static: attributes.is_static() || fn_t.attrs.contains(AttributeFlags::IS_STATIC),
        is_const: fn_t.attrs.contains(AttributeFlags::IS_CONST),
        kind: "method",
    });
}

//
// Free functions: module S_GPROC32/S_LPROC32 whose type is LF_PROCEDURE.
//

fn collect_free_functions(
    pdb: &mut PDB<std::fs::File>,
    formatter: &PdbParser,
    rows: &mut BTreeSet<Declaration>,
) -> crate::Result<()> {
    let type_information = pdb.type_information()?;
    let type_finder = {
        let mut type_finder = type_information.finder();
        let mut type_iter = type_information.iter();
        while type_iter.next()?.is_some() {
            type_finder.update(&type_iter);
        }
        type_finder
    };

    let dbi = pdb.debug_information()?;
    let mut modules = dbi.modules()?;

    while let Some(module) = modules.next()? {
        let Some(module_info) = pdb.module_info(&module)? else {
            continue;
        };

        let mut symbols = module_info.symbols()?;
        while let Some(symbol) = symbols.next()? {
            let Ok(pdb::SymbolData::Procedure(proc)) = symbol.parse() else {
                continue;
            };

            if proc.type_index == pdb::TypeIndex(0) || proc.type_index.is_cross_module() {
                continue;
            }
            // LF_MFUNCTION procs are methods, already covered by the type stream.
            let Ok(Ok(pdb::TypeData::Procedure(_))) =
                type_finder.find(proc.type_index).map(|item| item.parse())
            else {
                continue;
            };

            let name = proc.name.to_string();
            // Compiler-generated bodies (`dynamic initializer for ...`, etc.).
            if name.contains('`') {
                continue;
            }

            let Ok(fn_t) = formatter.parse_function(&proc.name, 0, proc.type_index) else {
                continue;
            };

            rows.insert(Declaration {
                class: None,
                name: name.into_owned(),
                signature: render_signature(&fn_t),
                access: None,
                is_virtual: false,
                // S_LPROC32 = internal linkage, i.e. a `static` free function.
                is_static: !proc.global,
                is_const: false,
                kind: "free",
            });
        }
    }

    Ok(())
}

//
//
//

fn render_signature(fn_t: &Function) -> String {
    let args = fn_t.arg_types.join(", ");
    match &fn_t.return_type {
        ReturnType::Type(ty) => format!("{ty} ({args})"),
        ReturnType::Constructor | ReturnType::Destructor => format!("({args})"),
    }
}

fn access_name(access: u8) -> Option<&'static str> {
    match access {
        1 => Some("private"),
        2 => Some("protected"),
        3 => Some("public"),
        _ => None,
    }
}
