//! Static descriptions of every generator. Single source of truth for:
//!
//! * `dataseed functions [--json]` output
//! * runtime argument validation in `bind`
//! * "did you mean" suggestions for unknown function names
//!
//! Keeping these in one table avoids drift between docs and behaviour.
//!
//! JSON emission is handled manually in `cli::functions_json` — these types
//! are intentionally not `Serialize`, so changes to the wire shape don't
//! require touching derive attributes here.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArgType {
    Number,
    Integer,
    String,
    Boolean,
    /// Any literal value — used by `randomChoice` for its variadic positionals.
    Any,
    /// Array literal. Element type is held by reference so the enum stays
    /// `Copy` and the catalog can be defined in `const`/`static` context.
    /// The optional length constraint lives on `ArgSpec.length` rather than
    /// here so it doesn't leak into nested types.
    Array(&'static ArgType),
    /// `table.column` reference — Phase 3, used only by `ref()`. The actual
    /// existence check (table declared in this file, column declared in
    /// that table) happens at semantic-check time, not in the catalog.
    ColumnRef,
    /// `N..M` integer range literal — Phase 4, used only by `ref()`'s
    /// `per_parent` kwarg. Bounds validity is checked at bind time.
    Range,
}

impl ArgType {
    /// String form used in `dataseed functions --json`'s `type` field.
    pub fn as_str(&self) -> &'static str {
        match self {
            ArgType::Number => "number",
            ArgType::Integer => "integer",
            ArgType::String => "string",
            ArgType::Boolean => "boolean",
            ArgType::Any => "any",
            ArgType::ColumnRef => "column_reference",
            ArgType::Range => "range",
            ArgType::Array(inner) => match inner {
                ArgType::Number => "array<number>",
                ArgType::Integer => "array<integer>",
                ArgType::String => "array<string>",
                ArgType::Boolean => "array<boolean>",
                ArgType::Any => "array<any>",
                ArgType::ColumnRef => "array<column_reference>",
                // Arrays of ranges aren't a real use case; map it so the
                // outer match stays exhaustive.
                ArgType::Range => "array<range>",
                // Phase 2 never produces array-of-array; if Phase 3 needs it,
                // extend this match with the specific nested literal you want.
                ArgType::Array(_) => "array<array>",
            },
        }
    }

    pub fn element_type(&self) -> Option<&'static ArgType> {
        match self {
            ArgType::Array(inner) => Some(*inner),
            _ => None,
        }
    }
}

// Static element types for use in CATALOG entries. Pre-declared here so the
// catalog can take `&'static ArgType` references without runtime allocation.
pub(crate) const T_NUMBER: ArgType = ArgType::Number;

#[derive(Debug, Clone)]
pub struct ArgSpec {
    pub name: &'static str,
    pub ty: ArgType,
    pub required: bool,
    /// Position-only arg if true; keyword-only if false. Mirrors the DSL,
    /// where positional args always come before kwargs.
    pub positional: bool,
    pub default: Option<&'static str>,
    /// For `ArgType::Array` args: required element count, if fixed. `None`
    /// means any length is acceptable. Ignored for non-array types.
    pub length: Option<usize>,
}

#[derive(Debug, Clone)]
pub struct FunctionSpec {
    pub name: &'static str,
    pub args: &'static [ArgSpec],
    /// If `Some`, additional positional arguments of this type are accepted
    /// beyond those in `args` (e.g. `randomChoice("a", "b", "c", ...)`).
    pub variadic: Option<ArgType>,
    pub returns: &'static str,
    pub example: &'static str,
}

/// Every generator the CLI knows about. Order is reflected in
/// `functions --json` output, so keep it stable and roughly alphabetical
/// after the "sequence" odd-one-out.
pub static CATALOG: &[FunctionSpec] = &[
    FunctionSpec {
        name: "sequence",
        args: &[ArgSpec {
            name: "start",
            ty: ArgType::Integer,
            required: false,
            positional: false,
            default: Some("1"),
            length: None,
        }],
        variadic: None,
        returns: "integer",
        example: "sequence  # or sequence(start: 1000)",
    },
    FunctionSpec {
        name: "randomBool",
        args: &[ArgSpec {
            name: "weight",
            ty: ArgType::Number,
            required: false,
            positional: false,
            default: Some("0.5"),
            length: None,
        }],
        variadic: None,
        returns: "boolean",
        example: "randomBool(weight: 0.85)",
    },
    FunctionSpec {
        name: "randomChoice",
        // At least one positional argument is required — enforced via the
        // variadic check in `bind`. We don't list a fixed arg here because
        // any number of literals are accepted.
        args: &[],
        variadic: Some(ArgType::Any),
        returns: "any",
        example: "randomChoice(\"oak\", \"birch\", \"pine\")",
    },
    FunctionSpec {
        name: "randomDate",
        args: &[
            ArgSpec { name: "start", ty: ArgType::String, required: true, positional: true, default: None, length: None },
            ArgSpec { name: "end",   ty: ArgType::String, required: true, positional: true, default: None, length: None },
        ],
        variadic: None,
        returns: "string",
        example: "randomDate(\"1990-01-01\", \"2024-12-31\")",
    },
    FunctionSpec {
        name: "randomEmail",
        args: &[],
        variadic: None,
        returns: "string",
        example: "randomEmail()",
    },
    FunctionSpec {
        name: "randomInt",
        args: &[
            ArgSpec { name: "min", ty: ArgType::Integer, required: true, positional: true, default: None, length: None },
            ArgSpec { name: "max", ty: ArgType::Integer, required: true, positional: true, default: None, length: None },
        ],
        variadic: None,
        returns: "integer",
        example: "randomInt(1, 100)",
    },
    FunctionSpec {
        name: "randomName",
        args: &[],
        variadic: None,
        returns: "string",
        example: "randomName()",
    },
    FunctionSpec {
        name: "randomRealNumber",
        args: &[
            ArgSpec { name: "min", ty: ArgType::Number, required: true, positional: true, default: None, length: None },
            ArgSpec { name: "max", ty: ArgType::Number, required: true, positional: true, default: None, length: None },
            ArgSpec { name: "decimals", ty: ArgType::Integer, required: false, positional: false, default: Some("2"), length: None },
        ],
        variadic: None,
        returns: "number",
        example: "randomRealNumber(1.0, 45.0, decimals: 2)",
    },
    FunctionSpec {
        name: "randomUuid",
        args: &[],
        variadic: None,
        returns: "string",
        example: "randomUuid()",
    },
    FunctionSpec {
        name: "randomWord",
        args: &[],
        variadic: None,
        returns: "string",
        example: "randomWord()",
    },
    // ---------- Phase 2: geospatial generators ----------------------------
    FunctionSpec {
        name: "randomPoint",
        args: &[ArgSpec {
            name: "bbox",
            ty: ArgType::Array(&T_NUMBER),
            required: true,
            positional: false,
            default: None,
            length: Some(4),
        }],
        variadic: None,
        returns: "geometry:point",
        example: "randomPoint(bbox: [3.3, 50.7, 7.2, 53.5])",
    },
    FunctionSpec {
        name: "randomPointNear",
        args: &[
            ArgSpec {
                name: "center",
                ty: ArgType::Array(&T_NUMBER),
                required: true,
                positional: false,
                default: None,
                length: Some(2),
            },
            ArgSpec {
                name: "radius_m",
                ty: ArgType::Number,
                required: true,
                positional: false,
                default: None,
                length: None,
            },
        ],
        variadic: None,
        returns: "geometry:point",
        example: "randomPointNear(center: [5.12, 52.37], radius_m: 1000)",
    },
    FunctionSpec {
        name: "randomLineString",
        args: &[
            ArgSpec {
                name: "bbox",
                ty: ArgType::Array(&T_NUMBER),
                required: true,
                positional: false,
                default: None,
                length: Some(4),
            },
            ArgSpec {
                name: "segments",
                ty: ArgType::Integer,
                required: false,
                positional: false,
                default: Some("5"),
                length: None,
            },
            ArgSpec {
                name: "jitter",
                ty: ArgType::Number,
                required: false,
                positional: false,
                default: Some("0.3"),
                length: None,
            },
        ],
        variadic: None,
        returns: "geometry:linestring",
        example: "randomLineString(bbox: [3.3, 50.7, 7.2, 53.5], segments: 8, jitter: 0.4)",
    },
    FunctionSpec {
        name: "randomPolygon",
        args: &[
            ArgSpec {
                name: "bbox",
                ty: ArgType::Array(&T_NUMBER),
                required: true,
                positional: false,
                default: None,
                length: Some(4),
            },
            ArgSpec {
                name: "vertices",
                ty: ArgType::Integer,
                required: false,
                positional: false,
                default: Some("6"),
                length: None,
            },
            ArgSpec {
                name: "irregularity",
                ty: ArgType::Number,
                required: false,
                positional: false,
                default: Some("0.3"),
                length: None,
            },
        ],
        variadic: None,
        returns: "geometry:polygon",
        example: "randomPolygon(bbox: [3.3, 50.7, 7.2, 53.5], vertices: 8, irregularity: 0.4)",
    },
    FunctionSpec {
        name: "randomBbox",
        args: &[
            ArgSpec {
                name: "within",
                ty: ArgType::Array(&T_NUMBER),
                required: true,
                positional: false,
                default: None,
                length: Some(4),
            },
            ArgSpec {
                name: "min_size_deg",
                ty: ArgType::Number,
                required: false,
                positional: false,
                default: Some("0.01"),
                length: None,
            },
            ArgSpec {
                name: "max_size_deg",
                ty: ArgType::Number,
                required: false,
                positional: false,
                default: Some("0.5"),
                length: None,
            },
        ],
        variadic: None,
        returns: "geometry:polygon",
        example: "randomBbox(within: [3.3, 50.7, 7.2, 53.5], min_size_deg: 0.01, max_size_deg: 0.2)",
    },
    // ---------- Phase 3: relations --------------------------------------
    FunctionSpec {
        name: "ref",
        args: &[
            ArgSpec {
                name: "target",
                ty: ArgType::ColumnRef,
                required: true,
                positional: true,
                default: None,
                length: None,
            },
            ArgSpec {
                name: "per_parent",
                ty: ArgType::Range,
                required: false,
                positional: false,
                default: None,
                length: None,
            },
        ],
        variadic: None,
        // Catalog sentinel: the actual return type depends on which column
        // is being referenced. Documented in the README.
        returns: "depends_on_target",
        example: "ref(users.id)",
    },
];

pub fn lookup(name: &str) -> Option<&'static FunctionSpec> {
    CATALOG.iter().find(|s| s.name == name)
}

pub fn function_names() -> impl Iterator<Item = &'static str> {
    CATALOG.iter().map(|s| s.name)
}
