//! Errors surfaced to the CLI. Display impls are the user-facing format —
//! LLMs editing `.dataseed` files will read these, so keep them specific,
//! cite line/column, and suggest fixes where possible.

use std::fmt;

#[derive(Debug, Clone, PartialEq)]
pub enum ParseError {
    /// Grammar-level error from pest (unknown token, missing brace, etc.).
    Syntax {
        line: usize,
        col: usize,
        message: String,
    },

    /// A required top-level directive is absent.
    MissingDirective { name: &'static str },

    /// A top-level directive appears more than once. Both occurrences are
    /// reported so the user can pick which to delete.
    DuplicateDirective {
        name: &'static str,
        first: (usize, usize),
        second: (usize, usize),
    },

    /// `generate N` with N that cannot be a u64 row count (e.g. negative,
    /// or out of range).
    InvalidCount {
        line: usize,
        col: usize,
        raw: String,
        reason: &'static str,
    },

    /// Two `table` declarations share the same name.
    DuplicateTable {
        name: String,
        first: (usize, usize),
        second: (usize, usize),
    },

    /// Two `generate` directives target the same table.
    DuplicateGenerate {
        table: String,
        first: (usize, usize),
        second: (usize, usize),
    },

    /// A `schema { ... }` block appears without a preceding `table:` directive.
    SchemaWithoutTable { line: usize, col: usize },

    /// A bare `generate N` (no table name) appears in a file that declares
    /// more than one table.
    BareGenerateInMultiTableFile { line: usize, col: usize },

    /// A range literal `N..M` that is invalid — either a bound overflows
    /// `i64`, or `hi < lo` (e.g. `5..1`). The `raw` field carries the
    /// offending text and `reason` explains the specific problem so
    /// downstream consumers can assume `lo <= hi`.
    InvalidRange {
        line: usize,
        col: usize,
        raw: String,
        reason: &'static str,
    },
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ParseError::Syntax { line, col, message } => {
                write!(f, "Error: syntax error at line {line}, column {col}: {message}")
            }
            ParseError::MissingDirective { name } => {
                write!(
                    f,
                    "Error: missing required directive `{name}`\nHint: every .dataseed file must declare `output:`, `table:`, a `schema {{ ... }}` block, and `generate N`."
                )
            }
            ParseError::DuplicateDirective { name, first, second } => {
                write!(
                    f,
                    "Error: directive `{name}` declared twice — first at line {fl}, column {fc}; again at line {sl}, column {sc}\nHint: remove one of the two occurrences.",
                    fl = first.0, fc = first.1, sl = second.0, sc = second.1,
                )
            }
            ParseError::InvalidCount { line, col, raw, reason } => {
                write!(
                    f,
                    "Error: invalid generate count `{raw}` at line {line}, column {col}: {reason}"
                )
            }
            ParseError::DuplicateTable { name, first, second } => {
                write!(
                    f,
                    "Error: table `{name}` declared twice — first at line {fl}, column {fc}; again at line {sl}, column {sc}\nHint: table names must be unique within a file.",
                    fl = first.0, fc = first.1, sl = second.0, sc = second.1,
                )
            }
            ParseError::DuplicateGenerate { table, first, second } => {
                write!(
                    f,
                    "Error: two `generate` directives target table `{table}` — first at line {fl}, column {fc}; again at line {sl}, column {sc}\nHint: remove one of the two.",
                    fl = first.0, fc = first.1, sl = second.0, sc = second.1,
                )
            }
            ParseError::SchemaWithoutTable { line, col } => {
                write!(
                    f,
                    "Error: `schema {{ ... }}` at line {line}, column {col} has no preceding `table:` directive\nHint: write `table NAME {{ ... }}` (new form) or pair the `schema` block with `table: NAME` above it (legacy form)."
                )
            }
            ParseError::BareGenerateInMultiTableFile { line, col } => {
                write!(
                    f,
                    "Error: bare `generate N` at line {line}, column {col} is only allowed in single-table files\nHint: this file declares multiple tables — use `generate NAME: N` to say which table this count applies to."
                )
            }
            ParseError::InvalidRange { line, col, raw, reason } => write!(
                f,
                "Error: invalid range `{raw}` at line {line}, column {col}: {reason}"
            ),
        }
    }
}

impl std::error::Error for ParseError {}

/// Semantic errors caught after parsing — typically against the generator
/// catalog. Carries line/column so the user can locate the offending call.
#[derive(Debug, Clone, PartialEq)]
pub enum SemanticError {
    UnknownFunction {
        line: usize,
        col: usize,
        name: String,
        suggestion: Option<String>,
    },
    WrongArity {
        line: usize,
        col: usize,
        function: String,
        expected: String,
        got: usize,
    },
    UnknownKwarg {
        line: usize,
        col: usize,
        function: String,
        name: String,
        allowed: Vec<&'static str>,
    },
    MissingArg {
        line: usize,
        col: usize,
        function: String,
        arg: &'static str,
    },
    TypeMismatch {
        line: usize,
        col: usize,
        function: String,
        arg: String,
        expected: &'static str,
        got: &'static str,
    },
    InvalidArgValue {
        line: usize,
        col: usize,
        function: String,
        arg: String,
        reason: String,
    },

    // ---------- Phase 3: relations ---------------------------------------
    /// `ref(T.X)` where `T` is not a declared table in this file.
    UndeclaredRefTable {
        line: usize,
        col: usize,
        table: String,
    },

    /// `ref(T.X)` where `X` is not a declared field in `T`.
    UndeclaredRefColumn {
        line: usize,
        col: usize,
        table: String,
        column: String,
    },

    /// `ref(self.X)` — a field inside table `T` refs `T.X`. Phase 3 forbids
    /// this; Phase 4 may allow correlated within-table refs.
    SelfReference {
        line: usize,
        col: usize,
        table: String,
        column: String,
    },

    /// A declared table has no `generate` directive.
    MissingGenerate {
        table: String,
        table_line: usize,
        table_col: usize,
    },

    /// A `generate NAME: N` targets a table that isn't declared.
    GenerateForUnknownTable {
        line: usize,
        col: usize,
        name: String,
    },

    /// Two or more tables form a `ref()` cycle. All edges in the cycle are
    /// listed so the user knows which to break.
    CyclicReference {
        edges: Vec<CycleEdge>,
    },

    /// A child table is driven by `per_parent` on some field, but the file also
    /// contains an explicit `generate <child>: N` directive. The count must come
    /// from one source — keep the per_parent, drop the explicit generate.
    ExplicitGenerateConflictsWithPerParent {
        child: String,
        parent: String,
        field: String,
        generate_line: usize,
        generate_col: usize,
    },

    /// A child table has more than one field using `per_parent`. Each child
    /// table can only have one owning parent.
    MultiplePerParentOwners {
        child: String,
        first: PerParentSite,
        second: PerParentSite,
    },
}

/// Locator for a single `per_parent` ref-site, used in multi-owner diagnostics
/// so the user can see both the field name, its parent target, and where the
/// offending call lives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PerParentSite {
    pub parent: String,
    pub field: String,
    pub line: usize,
    pub col: usize,
}

/// One edge in a cyclic-reference diagnostic. Carries enough information
/// to print a self-contained line like:
///   `users.favorite_order references orders.id (at line 5, column 22)`
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CycleEdge {
    pub from_table: String,
    pub from_field: String,
    pub to_table: String,
    pub to_column: String,
    pub line: usize,
    pub col: usize,
}

impl fmt::Display for SemanticError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SemanticError::UnknownFunction { line, col, name, suggestion } => {
                write!(f, "Error: unknown function `{name}` at line {line}, column {col}")?;
                if let Some(s) = suggestion {
                    write!(f, "\nHint: did you mean `{s}`?")?;
                }
                Ok(())
            }
            SemanticError::WrongArity { line, col, function, expected, got } => {
                write!(
                    f,
                    "Error: `{function}` at line {line}, column {col} expects {expected} but got {got}"
                )
            }
            SemanticError::UnknownKwarg { line, col, function, name, allowed } => {
                let list = if allowed.is_empty() {
                    "(none accepted)".to_string()
                } else {
                    allowed.iter().map(|s| format!("`{s}`")).collect::<Vec<_>>().join(", ")
                };
                write!(
                    f,
                    "Error: `{function}` at line {line}, column {col} got unknown keyword argument `{name}`\nHint: accepted keyword arguments: {list}"
                )
            }
            SemanticError::MissingArg { line, col, function, arg } => {
                write!(
                    f,
                    "Error: `{function}` at line {line}, column {col} is missing required argument `{arg}`"
                )
            }
            SemanticError::TypeMismatch { line, col, function, arg, expected, got } => {
                write!(
                    f,
                    "Error: `{function}` at line {line}, column {col}: argument `{arg}` expects {expected}, got {got}"
                )
            }
            SemanticError::InvalidArgValue { line, col, function, arg, reason } => {
                write!(
                    f,
                    "Error: `{function}` at line {line}, column {col}: argument `{arg}` is invalid — {reason}"
                )
            }
            SemanticError::UndeclaredRefTable { line, col, table } => {
                write!(
                    f,
                    "Error: `ref({table}.…)` at line {line}, column {col}: table `{table}` is not declared in this file\nHint: add a `table {table} {{ ... }}` block, or fix the spelling."
                )
            }
            SemanticError::UndeclaredRefColumn { line, col, table, column } => {
                write!(
                    f,
                    "Error: `ref({table}.{column})` at line {line}, column {col}: table `{table}` has no field named `{column}`"
                )
            }
            SemanticError::SelfReference { line, col, table, column } => {
                write!(
                    f,
                    "Error: self-reference at line {line}, column {col}: `{table}` cannot `ref({table}.{column})` itself\nHint: Phase 3 forbids self-references — split into two tables, or wait for Phase 4 (correlated refs)."
                )
            }
            SemanticError::MissingGenerate { table, table_line, table_col } => {
                write!(
                    f,
                    "Error: table `{table}` (declared at line {table_line}, column {table_col}) has no `generate` directive\nHint: add `generate {table}: N` for some row count N."
                )
            }
            SemanticError::GenerateForUnknownTable { line, col, name } => {
                write!(
                    f,
                    "Error: `generate {name}: …` at line {line}, column {col}: no table named `{name}` is declared in this file"
                )
            }
            SemanticError::CyclicReference { edges } => {
                writeln!(f, "Error: cyclic reference between tables")?;
                let max_lhs = edges
                    .iter()
                    .map(|e| e.from_table.len() + 1 + e.from_field.len())
                    .max()
                    .unwrap_or(0);
                for e in edges {
                    let lhs = format!("{}.{}", e.from_table, e.from_field);
                    writeln!(
                        f,
                        "  {:<width$} references {}.{}  (at line {}, column {})",
                        lhs, e.to_table, e.to_column, e.line, e.col,
                        width = max_lhs
                    )?;
                }
                write!(f, "Tables in a single file cannot have mutually recursive refs.")
            }
            SemanticError::ExplicitGenerateConflictsWithPerParent {
                child,
                parent,
                field,
                generate_line,
                generate_col,
            } => write!(
                f,
                "Error: table `{child}` is driven by `per_parent` on `{field}` (references `{parent}`), but an explicit `generate {child}: …` directive is present at line {generate_line}, column {generate_col}\nHint: remove the explicit count — per_parent derives it from the parent."
            ),
            SemanticError::MultiplePerParentOwners { child, first, second } => write!(
                f,
                "Error: table `{child}` has two fields using `per_parent`: `{}` (refs `{}`, at line {}, col {}) and `{}` (refs `{}`, at line {}, col {})\nHint: only one field per child table may use per_parent.",
                first.field, first.parent, first.line, first.col,
                second.field, second.parent, second.line, second.col,
            ),
        }
    }
}

impl std::error::Error for SemanticError {}
