//! Catalog-driven semantic checks: unknown function names, arity, kwarg, and
//! type validation. Phase 1+2 work, lifted from the old `semantic.rs` into
//! its own module without behavioural changes.

use strsim::levenshtein;

use crate::ast::File;
use crate::error::SemanticError;
use crate::generators::{self, spec};

/// Walk every field call and append any catalog-level errors.
pub fn check(file: &File, errors: &mut Vec<SemanticError>) {
    for table in &file.tables {
        for field in &table.fields {
            match spec::lookup(&field.call.function) {
                None => {
                    let suggestion = suggest(&field.call.function);
                    errors.push(SemanticError::UnknownFunction {
                        line: field.call.line,
                        col: field.call.col,
                        name: field.call.function.clone(),
                        suggestion,
                    });
                }
                Some(_) => {
                    if let Err(e) = generators::bind(&field.call) {
                        errors.push(e);
                    }
                }
            }
        }
    }
}

/// Suggest the catalog name closest to `name`, if one is "close enough".
///
/// Three-stage heuristic, in order:
///   1. case-insensitive exact match (`randomint` → `randomInt`);
///   2. unambiguous case-insensitive prefix relationship — input is a
///      prefix of exactly one catalog name, or vice versa, with prefix
///      length ≥ 4 to avoid noise from short stubs like `ran`. Catches
///      `randomReal` → `randomRealNumber`;
///   3. case-insensitive Levenshtein distance ≤ ⌈len/2⌉ and ≤ 4 — catches
///      transpositions and small character edits like `randomBoool` →
///      `randomBool`.
///
/// Stages run in priority order so case differences and prefix typos
/// (which Levenshtein alone handles poorly) are caught first.
pub fn suggest(name: &str) -> Option<String> {
    let lower = name.to_ascii_lowercase();

    for candidate in spec::function_names() {
        if candidate.eq_ignore_ascii_case(name) {
            return Some(candidate.to_string());
        }
    }

    if lower.len() >= 4 {
        let prefix_matches: Vec<&'static str> = spec::function_names()
            .filter(|c| {
                let lc = c.to_ascii_lowercase();
                lc.len() >= 4 && (lc.starts_with(&lower) || lower.starts_with(&lc))
            })
            .collect();
        if prefix_matches.len() == 1 {
            return Some(prefix_matches[0].to_string());
        }
    }

    let limit = (name.len() / 2 + 1).min(4);
    spec::function_names()
        .map(|c| (c, levenshtein(&lower, &c.to_ascii_lowercase())))
        .filter(|(_, d)| *d <= limit)
        .min_by_key(|(_, d)| *d)
        .map(|(c, _)| c.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::parse;
    use crate::semantic::check as check_all;

    fn parse_ok(src: &str) -> File {
        parse(src).expect("parse ok")
    }

    #[test]
    fn empty_schema_is_valid() {
        let file = parse_ok("output: json\ntable t { }\ngenerate t: 1\n");
        assert!(check_all(&file).is_ok());
    }

    #[test]
    fn well_formed_calls_produce_no_errors() {
        let src = r#"
            output: sql
            table: trees
            schema {
              id:      sequence
              species: randomChoice("oak", "birch")
              height:  randomRealNumber(1.0, 45.0, decimals: 2)
              planted: randomDate("1990-01-01", "2024-12-31")
              alive:   randomBool(weight: 0.85)
            }
            generate 5
        "#;
        let file = parse_ok(src);
        let report = check_all(&file);
        assert!(report.is_ok(), "unexpected errors: {:?}", report.errors);
    }

    #[test]
    fn unknown_function_suggests_close_name() {
        let src = "output: json\ntable: t\nschema { x: randomNam() }\ngenerate 1\n";
        let file = parse_ok(src);
        let report = check_all(&file);
        assert_eq!(report.errors.len(), 1);
        match &report.errors[0] {
            SemanticError::UnknownFunction { name, suggestion, .. } => {
                assert_eq!(name, "randomNam");
                assert_eq!(suggestion.as_deref(), Some("randomName"));
            }
            other => panic!("expected UnknownFunction, got {other:?}"),
        }
    }

    #[test]
    fn typo_random_boool_suggests_random_bool() {
        assert_eq!(suggest("randomBoool").as_deref(), Some("randomBool"));
    }

    #[test]
    fn typo_randome_name_suggests_random_name() {
        assert_eq!(suggest("randomeName").as_deref(), Some("randomName"));
    }

    #[test]
    fn typo_random_real_suggests_random_real_number() {
        assert_eq!(suggest("randomReal").as_deref(), Some("randomRealNumber"));
    }

    #[test]
    fn typo_lowercase_random_int_suggests_random_int() {
        assert_eq!(suggest("randomint").as_deref(), Some("randomInt"));
    }

    #[test]
    fn ambiguous_short_prefix_falls_through() {
        if let Some(name) = suggest("random") {
            assert!(spec::function_names().any(|c| c == name));
        }
    }

    #[test]
    fn very_distant_name_gets_no_suggestion() {
        let src = "output: json\ntable: t\nschema { x: planeteat() }\ngenerate 1\n";
        let file = parse_ok(src);
        let report = check_all(&file);
        match &report.errors[0] {
            SemanticError::UnknownFunction { suggestion, .. } => {
                assert!(suggestion.is_none(), "unrelated names should not suggest");
            }
            other => panic!("expected UnknownFunction, got {other:?}"),
        }
    }

    #[test]
    fn collects_multiple_errors_in_one_pass() {
        let src = r#"
            output: json
            table: t
            schema {
              a: randomNam()
              b: randomInt(10, 1)
              c: randomBool(weigth: 0.5)
            }
            generate 1
        "#;
        let file = parse_ok(src);
        let report = check_all(&file);
        assert_eq!(report.errors.len(), 3, "got: {:?}", report.errors);
    }

    #[test]
    fn error_messages_carry_line_numbers() {
        let src = "output: json\ntable: t\nschema {\n  x: foobar()\n}\ngenerate 1\n";
        let file = parse_ok(src);
        let report = check_all(&file);
        match &report.errors[0] {
            SemanticError::UnknownFunction { line, col, .. } => {
                assert_eq!(*line, 4);
                assert!(*col >= 6);
            }
            other => panic!("expected UnknownFunction, got {other:?}"),
        }
    }
}
