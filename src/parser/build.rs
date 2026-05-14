//! Turn the pest `Pairs` tree into an `ast::File`, performing structural
//! validation (required + non-duplicate directives) along the way.
//!
//! Semantic checks against the generator catalog (unknown function names,
//! arity, types) live in `semantic` — kept separate so `dataseed lint` can
//! report shape problems even when no catalog is loaded.
//!
//! Phase 3: a file may declare multiple tables. The legacy single-table
//! form (`table: NAME` + `schema { ... }` + `generate N`) is translated
//! into the same `Vec<Table>` shape so downstream code only sees one model.

use crate::ast::{Call, FieldDef, File, Generate, OutputKind, Table, Value};
use crate::error::ParseError;
use crate::parser::Rule;
use pest::iterators::{Pair, Pairs};

/// Build the typed AST from the root `file` pair.
pub(super) fn build_file(root: Pair<Rule>) -> Result<File, ParseError> {
    debug_assert_eq!(root.as_rule(), Rule::file);

    let mut output: Option<(OutputKind, (usize, usize))> = None;
    // Legacy form: at most one `table: NAME` directive followed by one
    // `schema { ... }` block. We carry these around until end-of-file and
    // pair them into a single Table.
    let mut legacy_table: Option<(String, (usize, usize))> = None;
    let mut legacy_schema: Option<(Vec<FieldDef>, (usize, usize))> = None;

    let mut tables: Vec<Table> = Vec::new();
    let mut generates: Vec<Generate> = Vec::new();

    for pair in root.into_inner() {
        match pair.as_rule() {
            Rule::output_dir => {
                let pos = pair.line_col();
                let kind = build_output_kind(pair)?;
                if let Some((_, first)) = output {
                    return Err(ParseError::DuplicateDirective {
                        name: "output",
                        first,
                        second: pos,
                    });
                }
                output = Some((kind, pos));
            }
            Rule::table_dir => {
                let pos = pair.line_col();
                let name = build_table_name(pair)?;
                if let Some((_, first)) = legacy_table {
                    return Err(ParseError::DuplicateDirective {
                        name: "table",
                        first,
                        second: pos,
                    });
                }
                legacy_table = Some((name, pos));
            }
            Rule::schema_block => {
                let pos = pair.line_col();
                let fields = build_field_list(pair)?;
                if let Some((_, first)) = legacy_schema {
                    return Err(ParseError::DuplicateDirective {
                        name: "schema",
                        first,
                        second: pos,
                    });
                }
                legacy_schema = Some((fields, pos));
            }
            Rule::table_block => {
                let pos = pair.line_col();
                let table = build_table_block(pair, pos)?;
                tables.push(table);
            }
            Rule::generate_dir => {
                generates.push(build_generate(pair)?);
            }
            Rule::EOI => {}
            other => unreachable!("unexpected top-level rule: {other:?}"),
        }
    }

    // Fold the legacy form into the unified shape.
    let legacy = match (legacy_table, legacy_schema) {
        (Some((name, pos)), Some((fields, _))) => Some(Table {
            name,
            fields,
            line: pos.0,
            col: pos.1,
        }),
        (Some((_, _)), None) => {
            return Err(ParseError::MissingDirective { name: "schema" });
        }
        (None, Some((_, pos))) => {
            return Err(ParseError::SchemaWithoutTable {
                line: pos.0,
                col: pos.1,
            });
        }
        (None, None) => None,
    };
    if let Some(t) = legacy {
        tables.insert(0, t);
    }

    let output = output.map(|(k, _)| k).ok_or(ParseError::MissingDirective { name: "output" })?;

    if tables.is_empty() {
        return Err(ParseError::MissingDirective { name: "table" });
    }
    // Duplicate table names — catch here so the engine can index by name.
    for i in 0..tables.len() {
        for j in (i + 1)..tables.len() {
            if tables[i].name == tables[j].name {
                return Err(ParseError::DuplicateTable {
                    name: tables[i].name.clone(),
                    first: (tables[i].line, tables[i].col),
                    second: (tables[j].line, tables[j].col),
                });
            }
        }
    }

    if generates.is_empty() {
        return Err(ParseError::MissingDirective { name: "generate" });
    }

    // Bare-form `generate N` (no table name) is only legal when there is
    // exactly one table — adopt it. With multiple tables every generate
    // must name its target.
    if tables.len() == 1 {
        let only = &tables[0].name;
        for g in &mut generates {
            if g.table.is_empty() {
                g.table = only.clone();
            }
        }
    } else {
        for g in &generates {
            if g.table.is_empty() {
                return Err(ParseError::BareGenerateInMultiTableFile {
                    line: g.line,
                    col: g.col,
                });
            }
        }
    }

    // Duplicate `generate` for the same table — choose to error so the
    // user sees the conflict instead of getting a silent override.
    for i in 0..generates.len() {
        for j in (i + 1)..generates.len() {
            if generates[i].table == generates[j].table {
                return Err(ParseError::DuplicateGenerate {
                    table: generates[i].table.clone(),
                    first: (generates[i].line, generates[i].col),
                    second: (generates[j].line, generates[j].col),
                });
            }
        }
    }

    Ok(File {
        output,
        tables,
        generate: generates,
    })
}

fn build_output_kind(pair: Pair<Rule>) -> Result<OutputKind, ParseError> {
    let kind_pair = first_child(pair, Rule::output_kind);
    Ok(match kind_pair.as_str() {
        "sql" => OutputKind::Sql,
        "json" => OutputKind::Json,
        "postgis" => OutputKind::Postgis,
        other => unreachable!("output_kind matched unexpected text: {other}"),
    })
}

fn build_table_name(pair: Pair<Rule>) -> Result<String, ParseError> {
    let ident = first_child(pair, Rule::ident);
    Ok(ident.as_str().to_string())
}

fn build_table_block(pair: Pair<Rule>, pos: (usize, usize)) -> Result<Table, ParseError> {
    let mut inner = pair.into_inner();
    let name_pair = inner.next().expect("table_block: ident");
    let name = name_pair.as_str().to_string();
    let mut fields = Vec::new();
    for child in inner {
        if child.as_rule() == Rule::field_def {
            fields.push(build_field(child)?);
        }
    }
    Ok(Table {
        name,
        fields,
        line: pos.0,
        col: pos.1,
    })
}

fn build_field_list(pair: Pair<Rule>) -> Result<Vec<FieldDef>, ParseError> {
    let mut fields = Vec::new();
    for child in pair.into_inner() {
        if child.as_rule() == Rule::field_def {
            fields.push(build_field(child)?);
        }
    }
    Ok(fields)
}

fn build_field(pair: Pair<Rule>) -> Result<FieldDef, ParseError> {
    let (line, col) = pair.line_col();
    let mut inner = pair.into_inner();
    let name_pair = inner.next().expect("field_def: ident");
    let call_pair = inner.next().expect("field_def: call");
    Ok(FieldDef {
        name: name_pair.as_str().to_string(),
        call: build_call(call_pair)?,
        line,
        col,
    })
}

fn build_call(pair: Pair<Rule>) -> Result<Call, ParseError> {
    let (line, col) = pair.line_col();
    let mut inner = pair.into_inner();
    let name_pair = inner.next().expect("call: ident");
    let function = name_pair.as_str().to_string();

    let mut positional = Vec::new();
    let mut kwargs: Vec<(String, Value)> = Vec::new();

    if let Some(args_pair) = inner.next() {
        debug_assert_eq!(args_pair.as_rule(), Rule::call_args);
        if let Some(list) = args_pair.into_inner().next() {
            debug_assert_eq!(list.as_rule(), Rule::arg_list);
            for arg in list.into_inner() {
                match arg.as_rule() {
                    Rule::positional => {
                        let v = build_value(arg.into_inner().next().expect("positional value"))?;
                        positional.push(v);
                    }
                    Rule::kwarg => {
                        let mut it = arg.into_inner();
                        let name = it.next().expect("kwarg name").as_str().to_string();
                        let v = build_value(it.next().expect("kwarg value"))?;
                        kwargs.push((name, v));
                    }
                    other => unreachable!("unexpected arg child: {other:?}"),
                }
            }
        }
    }

    Ok(Call { function, positional, kwargs, line, col })
}

fn build_value(pair: Pair<Rule>) -> Result<Value, ParseError> {
    match pair.as_rule() {
        Rule::number => {
            let n: f64 = pair.as_str().parse().expect("number rule matched non-f64");
            Ok(Value::Number(n))
        }
        Rule::string => {
            let inner = first_child(pair, Rule::string_inner);
            Ok(Value::String(decode_string(inner.as_str())))
        }
        Rule::boolean => Ok(Value::Bool(pair.as_str() == "true")),
        Rule::array => {
            let mut items = Vec::new();
            for child in pair.into_inner() {
                items.push(build_value(child)?);
            }
            Ok(Value::Array(items))
        }
        Rule::column_ref => {
            let mut it = pair.into_inner();
            let table = it.next().expect("column_ref: table ident").as_str().to_string();
            let column = it.next().expect("column_ref: column ident").as_str().to_string();
            Ok(Value::ColumnRef { table, column })
        }
        Rule::range => {
            let (line, col) = pair.line_col();
            let mut it = pair.into_inner();
            let lo_pair = it.next().expect("range: lo");
            let hi_pair = it.next().expect("range: hi");
            let lo: i64 = lo_pair.as_str().parse().expect("integer rule");
            let hi: i64 = hi_pair.as_str().parse().expect("integer rule");
            if hi < lo {
                return Err(ParseError::InvalidRange { line, col, lo, hi });
            }
            Ok(Value::Range { lo, hi })
        }
        other => unreachable!("value matched unexpected rule: {other:?}"),
    }
}

fn build_generate(pair: Pair<Rule>) -> Result<Generate, ParseError> {
    let (line, col) = pair.line_col();
    // Two forms:
    //   generate_bare:   generate N             -- one child of Rule::integer
    //   generate_named:  generate NAME: N       -- ident + integer
    let mut table = String::new();
    let mut int_pair: Option<Pair<Rule>> = None;
    for child in pair.into_inner() {
        match child.as_rule() {
            Rule::ident => table = child.as_str().to_string(),
            Rule::integer => int_pair = Some(child),
            _ => {}
        }
    }
    let int_pair = int_pair.expect("generate_dir must contain an integer");
    let (int_line, int_col) = int_pair.line_col();
    let raw = int_pair.as_str();
    let count = match raw.parse::<i64>() {
        Ok(n) if n < 0 => {
            return Err(ParseError::InvalidCount {
                line: int_line,
                col: int_col,
                raw: raw.to_string(),
                reason: "row count must be non-negative",
            });
        }
        Ok(n) => n as u64,
        Err(_) => raw.parse::<u64>().map_err(|_| ParseError::InvalidCount {
            line: int_line,
            col: int_col,
            raw: raw.to_string(),
            reason: "value is out of range for a 64-bit row count",
        })?,
    };
    Ok(Generate { table, count, line, col })
}

// ---------- helpers -------------------------------------------------------

fn first_child(pair: Pair<Rule>, expected: Rule) -> Pair<Rule> {
    let p = find_child(pair.clone().into_inner(), expected);
    p.unwrap_or_else(|| {
        panic!(
            "grammar/build mismatch: expected child {:?} inside {:?}",
            expected,
            pair.as_rule()
        )
    })
}

fn find_child(pairs: Pairs<Rule>, rule: Rule) -> Option<Pair<Rule>> {
    pairs.into_iter().find(|p| p.as_rule() == rule)
}

fn decode_string(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
            Some('"') => out.push('"'),
            Some('\\') => out.push('\\'),
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}
