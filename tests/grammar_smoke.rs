//! Smoke test: the canonical spec example must parse cleanly.
//!
//! Semantic checks (unknown functions, type mismatches) come later — this just
//! verifies the grammar shape.

use dataseed::parser::{DataseedParser, Rule};
use pest::Parser;

const SPEC_EXAMPLE: &str = include_str!("../examples/trees.dataseed");

#[test]
fn spec_example_parses() {
    DataseedParser::parse(Rule::file, SPEC_EXAMPLE)
        .unwrap_or_else(|e| panic!("spec example failed to parse:\n{e}"));
}

#[test]
fn bare_generator_call_parses() {
    let src = "output: json\ntable: t\nschema { id: sequence }\ngenerate 1\n";
    DataseedParser::parse(Rule::file, src).expect("bare call should parse");
}

#[test]
fn comments_and_blank_lines_ignored() {
    let src = r#"
# top-of-file comment
output: sql
table: t  # trailing comment

schema {
  # field comment
  x: randomInt(1, 10)
}

generate 5
"#;
    DataseedParser::parse(Rule::file, src).expect("comments should be ignored");
}

#[test]
fn mixed_positional_and_kwargs() {
    let src = r#"output: json
table: t
schema { h: randomRealNumber(1.0, 45.0, decimals: 2) }
generate 1
"#;
    DataseedParser::parse(Rule::file, src).expect("mixed args should parse");
}

#[test]
fn negative_integer_args_parse() {
    let src = r#"output: json
table: t
schema { x: randomInt(-10, 10) }
generate 1
"#;
    let parsed = DataseedParser::parse(Rule::file, src)
        .expect("negative int args should parse");
    let text = format!("{parsed:#?}");
    assert!(
        text.contains("-10"),
        "expected `-10` to be lexed as one atomic number token, got:\n{text}"
    );
}

#[test]
fn negative_real_args_parse() {
    let src = r#"output: json
table: t
schema { y: randomRealNumber(-3.5, 3.5, decimals: 2) }
generate 1
"#;
    let parsed = DataseedParser::parse(Rule::file, src)
        .expect("negative real args should parse");
    let text = format!("{parsed:#?}");
    assert!(
        text.contains("-3.5"),
        "expected `-3.5` to be lexed as one atomic number token, got:\n{text}"
    );
}
