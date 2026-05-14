//! DSL parser. Grammar lives in `dataseed.pest`; this module turns a `Pairs`
//! tree into the strongly-typed `ast::File`.

use pest::Parser;
use pest_derive::Parser;

use crate::ast::File;
pub use crate::error::ParseError;

mod build;

#[derive(Parser)]
#[grammar = "parser/dataseed.pest"]
pub struct DataseedParser;

/// Parse a `.dataseed` source string into a validated AST.
///
/// Failures fall into two buckets:
/// * grammar errors from pest, reported with line/column;
/// * structural errors (missing or duplicate top-level directive), with the
///   line/column of the offending occurrence (both occurrences for duplicates).
pub fn parse(src: &str) -> Result<File, ParseError> {
    let mut pairs = DataseedParser::parse(Rule::file, src).map_err(syntax_from_pest)?;
    let root = pairs.next().expect("pest::parse(file) yields at least one pair");
    build::build_file(root)
}

fn syntax_from_pest(err: pest::error::Error<Rule>) -> ParseError {
    let (line, col) = match err.line_col {
        pest::error::LineColLocation::Pos((l, c)) => (l, c),
        pest::error::LineColLocation::Span((l, c), _) => (l, c),
    };
    // pest's default Display includes a multi-line caret diagram. Strip the
    // first line off the variant so we just get the human-readable summary.
    let message = match &err.variant {
        pest::error::ErrorVariant::ParsingError { positives, negatives } => {
            format_expected(positives, negatives)
        }
        pest::error::ErrorVariant::CustomError { message } => message.clone(),
    };
    ParseError::Syntax { line, col, message }
}

fn format_expected(positives: &[Rule], negatives: &[Rule]) -> String {
    let render = |rs: &[Rule]| {
        rs.iter()
            .map(|r| format!("{r:?}"))
            .collect::<Vec<_>>()
            .join(", ")
    };
    match (positives.is_empty(), negatives.is_empty()) {
        (false, true) => format!("expected one of: {}", render(positives)),
        (true, false) => format!("unexpected: {}", render(negatives)),
        (false, false) => format!(
            "expected one of: {}; unexpected: {}",
            render(positives),
            render(negatives),
        ),
        (true, true) => "unrecognized input".to_string(),
    }
}
