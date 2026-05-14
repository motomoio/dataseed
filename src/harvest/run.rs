//! End-to-end harvest pipeline: introspect → sample → infer → emit.
//!
//! All four phases live in sibling modules; this file is the orchestrator
//! and is the only thing `cli` knows about.

use std::collections::BTreeSet;
use std::fs;
use std::io::{self, BufWriter, Write};
use std::path::PathBuf;

use chrono::Utc;

use crate::ast::OutputKind;
use crate::harvest::{connect, emit, infer, sample};

/// Parsed CLI options for `dataseed harvest`. Mirrors the clap-derived
/// struct in `cli/mod.rs`; kept as a plain struct here so the harvest
/// pipeline doesn't depend on clap.
#[derive(Debug, Clone)]
pub struct HarvestOptions {
    pub connection_string: String,
    pub schema: String,
    pub tables: Option<Vec<String>>,
    pub exclude: BTreeSet<String>,
    pub sample: usize,
    pub scale: f64,
    pub output_mode: Option<OutputKind>,
    pub output_file: Option<PathBuf>,
    pub verbose: bool,
    /// The raw CLI line, redacted of any password component, used in the
    /// emitted file header for reproducibility.
    pub invocation_line: String,
}

const MAX_SAMPLE: usize = 100_000;

pub fn run_harvest(opts: HarvestOptions) -> Result<(), String> {
    let sample_size = opts.sample.min(MAX_SAMPLE).max(1);
    if opts.scale <= 0.0 || !opts.scale.is_finite() {
        return Err(format!("--scale must be > 0 (got {})", opts.scale));
    }

    let mut client = connect::open(&opts.connection_string)
        .map_err(|e| format!("connect: {e}"))?;

    let geometry_supported = connect::detect_postgis(&mut client)
        .map_err(|e| format!("postgis check: {e}"))?;

    let mut schema = connect::introspect(
        &mut client,
        &opts.schema,
        opts.tables.as_deref(),
        &opts.exclude,
        geometry_supported,
    )
    .map_err(|e| format!("introspect: {e}"))?;

    schema.source.harvested_at = Utc::now();
    schema.source.invocation = opts.invocation_line.clone();

    if schema.tables.is_empty() {
        return Err(format!(
            "no tables found in schema `{}` (after --tables/--exclude filters)",
            opts.schema
        ));
    }

    sample::populate(&mut client, &mut schema, sample_size)
        .map_err(|e| format!("sample: {e}"))?;

    let inference = infer::infer_schema(&schema);

    let output_mode = opts
        .output_mode
        .unwrap_or_else(|| infer::default_output_mode(&schema));

    let rendered = emit::render(&schema, &inference, output_mode, opts.scale)
        .map_err(|e| format!("emit: {e}"))?;

    // Self-check: feed the output back through our own parser+semantic
    // pass so a bug in inference can't ship a file `plant` will reject.
    emit::self_check(&rendered).map_err(|e| format!("self-check: {e}"))?;

    if opts.verbose {
        let mut stderr = io::stderr().lock();
        for line in inference.verbose_lines() {
            writeln!(stderr, "{line}").ok();
        }
    }

    match opts.output_file {
        Some(path) => {
            let f = fs::File::create(&path)
                .map_err(|e| format!("open `{}`: {e}", path.display()))?;
            let mut w = BufWriter::new(f);
            w.write_all(rendered.as_bytes())
                .map_err(|e| format!("write: {e}"))?;
            w.flush().map_err(|e| format!("flush: {e}"))?;
        }
        None => {
            let stdout = io::stdout();
            let mut w = BufWriter::new(stdout.lock());
            w.write_all(rendered.as_bytes())
                .map_err(|e| format!("write: {e}"))?;
            w.flush().map_err(|e| format!("flush: {e}"))?;
        }
    }

    Ok(())
}

/// Strip the password component (if any) from a libpq connection URL for
/// logging back into the emitted file header. Falls back to passing the
/// input through unchanged if it doesn't parse as a URL.
pub fn redact_connection_string(raw: &str) -> String {
    // postgres://user:pass@host/db → postgres://user:***@host/db
    let Some(scheme_end) = raw.find("://") else {
        return raw.to_string();
    };
    let (scheme, rest) = raw.split_at(scheme_end + 3);
    // Find an authority section ending at '/' or '?' or end of string.
    let auth_end = rest
        .find(|c: char| c == '/' || c == '?')
        .unwrap_or(rest.len());
    let (auth, tail) = rest.split_at(auth_end);
    let Some(at_pos) = auth.rfind('@') else {
        return raw.to_string();
    };
    let (userinfo, host) = auth.split_at(at_pos);
    match userinfo.split_once(':') {
        Some((user, _pass)) => format!("{scheme}{user}:***{host}{tail}"),
        None => raw.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::redact_connection_string;

    #[test]
    fn redacts_password() {
        assert_eq!(
            redact_connection_string("postgres://alice:secret@db:5432/shop"),
            "postgres://alice:***@db:5432/shop"
        );
    }

    #[test]
    fn passes_through_without_password() {
        assert_eq!(
            redact_connection_string("postgres://alice@db/shop"),
            "postgres://alice@db/shop"
        );
    }

    #[test]
    fn passes_through_non_url() {
        assert_eq!(
            redact_connection_string("host=localhost user=alice"),
            "host=localhost user=alice"
        );
    }
}
