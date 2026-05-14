//! CLI glue. The `main` binary calls `run(Cli::parse())`.

use std::fs;
use std::io::{self, BufWriter, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};

use crate::generators::spec::{ArgType, CATALOG};
use crate::parser::parse;
use crate::rng::SeedRng;
use crate::semantic;

mod functions_json;

#[derive(Parser)]
#[command(
    name = "dataseed",
    version,
    about = "Plant a seed, grow a dataset.",
    propagate_version = true,
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand)]
pub enum Command {
    /// Parse a `.dataseed` file and produce a dataset.
    Plant {
        /// Path to the `.dataseed` source.
        file: PathBuf,

        /// Seed the PRNG for byte-identical output across runs. Defaults to
        /// a fresh entropy-based seed; the chosen seed is printed to stderr.
        #[arg(long)]
        seed: Option<u64>,

        /// Write output to this file instead of stdout.
        #[arg(short = 'o', long = "output")]
        output: Option<PathBuf>,

        /// Override a `generate` directive. Two forms:
        ///   --count N           (single-table files only)
        ///   --count NAME=N      (per-table; repeatable for multi-table files)
        #[arg(long, value_name = "[NAME=]N", action = clap::ArgAction::Append)]
        count: Vec<String>,

        /// Only emit rows for the named table(s). Dependency tables are still
        /// generated (so `ref()` resolves) but their rows are not written.
        /// Repeatable.
        #[arg(long, value_name = "NAME", action = clap::ArgAction::Append)]
        table: Vec<String>,

        /// Prepend CREATE TABLE statements to SQL/PostGIS output. No-op for
        /// JSON (with a stderr warning).
        #[arg(long = "emit-ddl")]
        emit_ddl: bool,
    },

    /// List every available generator. Use `--json` for machine consumption.
    Functions {
        #[arg(long)]
        json: bool,
    },

    /// Parse and validate without generating any data.
    Lint {
        file: PathBuf,
    },

    /// Connect to a Postgres database, sample rows, and emit a `.dataseed`
    /// file inferred from the actual data. Requires the `harvest` feature
    /// (on by default).
    #[cfg(feature = "harvest")]
    Harvest {
        /// libpq connection string (`postgres://user:pass@host:port/db`).
        connection_string: String,

        /// Postgres schema to inspect.
        #[arg(long, default_value = "public")]
        schema: String,

        /// Comma-separated allowlist of tables to inspect.
        #[arg(long, value_delimiter = ',')]
        tables: Option<Vec<String>>,

        /// Comma-separated denylist of tables to skip.
        #[arg(long, value_delimiter = ',', default_value = "")]
        exclude: Vec<String>,

        /// Rows to sample per table for inference. Capped at 100_000.
        #[arg(long, default_value_t = 1000)]
        sample: usize,

        /// Multiplier for inferred `generate N` counts (1.0 = match source row count).
        #[arg(long, default_value_t = 1.0)]
        scale: f64,

        /// Output mode to write into the `output:` directive (`sql`, `json`, `postgis`).
        /// Defaults to `postgis` when geometry columns are present, else `sql`.
        #[arg(long = "output-mode", value_parser = parse_output_mode)]
        output_mode: Option<crate::ast::OutputKind>,

        /// Write to file instead of stdout.
        #[arg(short = 'o', long = "output")]
        output: Option<PathBuf>,

        /// Print per-column inference reasoning to stderr.
        #[arg(long)]
        verbose: bool,
    },
}

#[cfg(feature = "harvest")]
fn parse_output_mode(s: &str) -> Result<crate::ast::OutputKind, String> {
    match s {
        "sql" => Ok(crate::ast::OutputKind::Sql),
        "json" => Ok(crate::ast::OutputKind::Json),
        "postgis" => Ok(crate::ast::OutputKind::Postgis),
        other => Err(format!("invalid output mode `{other}`; expected sql|json|postgis")),
    }
}

/// Entry point. Returns a process exit code.
pub fn run(cli: Cli) -> ExitCode {
    match cli.command {
        Command::Plant { file, seed, output, count, table, emit_ddl } => {
            plant(&file, seed, output, count, table, emit_ddl)
        }
        Command::Functions { json } => functions(json),
        Command::Lint { file } => lint(&file),
        #[cfg(feature = "harvest")]
        Command::Harvest {
            connection_string,
            schema,
            tables,
            exclude,
            sample,
            scale,
            output_mode,
            output,
            verbose,
        } => harvest_cmd(
            connection_string,
            schema,
            tables,
            exclude,
            sample,
            scale,
            output_mode,
            output,
            verbose,
        ),
    }
}

#[cfg(feature = "harvest")]
#[allow(clippy::too_many_arguments)]
fn harvest_cmd(
    connection_string: String,
    schema: String,
    tables: Option<Vec<String>>,
    exclude_raw: Vec<String>,
    sample: usize,
    scale: f64,
    output_mode: Option<crate::ast::OutputKind>,
    output_file: Option<PathBuf>,
    verbose: bool,
) -> ExitCode {
    let exclude: std::collections::BTreeSet<String> = exclude_raw
        .into_iter()
        .filter(|s| !s.is_empty())
        .collect();
    let invocation_line = format!(
        "dataseed harvest {}{}",
        crate::harvest::run::redact_connection_string(&connection_string),
        format_options_for_header(&schema, &tables, &exclude, sample, scale, output_mode)
    );
    let opts = crate::harvest::HarvestOptions {
        connection_string,
        schema,
        tables,
        exclude,
        sample,
        scale,
        output_mode,
        output_file,
        verbose,
        invocation_line,
    };
    match crate::harvest::run_harvest(opts) {
        Ok(()) => ExitCode::SUCCESS,
        Err(msg) => {
            eprintln!("Error: {msg}");
            ExitCode::from(1)
        }
    }
}

#[cfg(feature = "harvest")]
fn format_options_for_header(
    schema: &str,
    tables: &Option<Vec<String>>,
    exclude: &std::collections::BTreeSet<String>,
    sample: usize,
    scale: f64,
    output_mode: Option<crate::ast::OutputKind>,
) -> String {
    let mut parts = Vec::new();
    if schema != "public" {
        parts.push(format!("--schema {schema}"));
    }
    if let Some(t) = tables {
        parts.push(format!("--tables {}", t.join(",")));
    }
    if !exclude.is_empty() {
        let joined: Vec<String> = exclude.iter().cloned().collect();
        parts.push(format!("--exclude {}", joined.join(",")));
    }
    if sample != 1000 {
        parts.push(format!("--sample {sample}"));
    }
    if (scale - 1.0).abs() > f64::EPSILON {
        parts.push(format!("--scale {scale}"));
    }
    if let Some(m) = output_mode {
        parts.push(format!("--output-mode {}", output_kind_label(m)));
    }
    if parts.is_empty() {
        String::new()
    } else {
        format!(" {}", parts.join(" "))
    }
}

fn plant(
    src_path: &PathBuf,
    seed: Option<u64>,
    out_path: Option<PathBuf>,
    count_args: Vec<String>,
    table_filters: Vec<String>,
    emit_ddl: bool,
) -> ExitCode {
    let src = match fs::read_to_string(src_path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("Error: failed to read `{}`: {e}", src_path.display());
            return ExitCode::from(1);
        }
    };
    let file = match parse(&src) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::from(1);
        }
    };
    let report = semantic::check(&file);
    if !report.is_ok() {
        for e in &report.errors {
            eprintln!("{e}");
        }
        return ExitCode::from(1);
    }

    // Resolve --count [NAME=]N args against the topo plan.
    let counts = match resolve_counts(&file, &count_args) {
        Ok(c) => c,
        Err(msg) => {
            eprintln!("Error: {msg}");
            return ExitCode::from(2);
        }
    };
    // Resolve --table NAME filter — every name must be a declared table.
    let emit_only = if table_filters.is_empty() {
        None
    } else {
        let declared: std::collections::BTreeSet<&str> =
            file.tables.iter().map(|t| t.name.as_str()).collect();
        for t in &table_filters {
            if !declared.contains(t.as_str()) {
                eprintln!("Error: --table `{t}` is not a declared table in `{}`", src_path.display());
                return ExitCode::from(2);
            }
        }
        Some(table_filters.iter().cloned().collect())
    };

    if emit_ddl && matches!(file.output, crate::ast::OutputKind::Json) {
        eprintln!("warning: --emit-ddl is a no-op for JSON output");
    }

    let plan = crate::output::RenderPlan {
        topo_order: report.topo_order.clone(),
        referenced: report.referenced.clone(),
        counts,
        emit_only,
        per_parent_owners: report.per_parent_owners.clone(),
        self_ref_tables: report.self_ref_tables.clone(),
        emit_ddl,
    };

    let mut rng = match seed {
        Some(s) => SeedRng::from_seed(s),
        None => {
            let r = SeedRng::from_entropy();
            eprintln!("dataseed: seed = {} (use --seed {0} to reproduce)", r.seed());
            r
        }
    };

    let render_result = match out_path {
        Some(path) => match fs::File::create(&path) {
            Ok(f) => {
                let mut w = BufWriter::new(f);
                crate::output::render_plan(&file, &plan, &mut rng, &mut w).and_then(|_| {
                    w.flush().map_err(crate::output::RenderError::Io)
                })
            }
            Err(e) => {
                eprintln!("Error: failed to open `{}` for writing: {e}", path.display());
                return ExitCode::from(1);
            }
        },
        None => {
            let stdout = io::stdout();
            let mut w = BufWriter::new(stdout.lock());
            crate::output::render_plan(&file, &plan, &mut rng, &mut w).and_then(|_| {
                w.flush().map_err(crate::output::RenderError::Io)
            })
        }
    };
    if let Err(e) = render_result {
        eprintln!("{e}");
        return ExitCode::from(1);
    }
    ExitCode::SUCCESS
}

/// Build the per-table count map from in-file `generate` directives plus
/// any `--count` overrides. Two override syntaxes:
///   * `N`         — bare; only legal in single-table files
///   * `NAME=N`    — per-table; repeatable in multi-table files
fn resolve_counts(
    file: &crate::ast::File,
    args: &[String],
) -> Result<std::collections::BTreeMap<String, u64>, String> {
    let mut counts: std::collections::BTreeMap<String, u64> =
        file.generate.iter().map(|g| (g.table.clone(), g.count)).collect();
    for raw in args {
        match raw.split_once('=') {
            Some((name, n)) => {
                let n: u64 = n.parse().map_err(|_| {
                    format!("--count: invalid number `{n}` (expected NAME=N)")
                })?;
                if !file.tables.iter().any(|t| t.name == name) {
                    return Err(format!(
                        "--count {name}=…: no table named `{name}` declared in this file"
                    ));
                }
                counts.insert(name.to_string(), n);
            }
            None => {
                let n: u64 = raw.parse().map_err(|_| {
                    format!("--count: invalid value `{raw}` (expected N or NAME=N)")
                })?;
                if file.tables.len() != 1 {
                    return Err(format!(
                        "bare `--count {raw}` requires a single-table file; use --count NAME={raw}"
                    ));
                }
                counts.insert(file.tables[0].name.clone(), n);
            }
        }
    }
    Ok(counts)
}

fn functions(as_json: bool) -> ExitCode {
    if as_json {
        let value = functions_json::catalog_as_json();
        // Pretty for human reading; consumers should not depend on exact
        // whitespace — only on the documented field shape.
        let s = serde_json::to_string_pretty(&value).expect("catalog -> json");
        println!("{s}");
        return ExitCode::SUCCESS;
    }
    // Human-friendly table-ish listing.
    for spec in CATALOG {
        println!("{}", spec.name);
        if spec.args.is_empty() && spec.variadic.is_none() {
            println!("  (no arguments)");
        }
        for a in spec.args {
            let kind = if a.positional { "positional" } else { "keyword" };
            let req = if a.required { "required" } else { "optional" };
            let default = a.default.map(|d| format!(" (default: {d})")).unwrap_or_default();
            let length = a.length.map(|n| format!(" length={n}")).unwrap_or_default();
            println!("  {} : {}{} [{}, {}]{}", a.name, a.ty.as_str(), length, kind, req, default);
        }
        if let Some(v) = spec.variadic {
            println!("  ... : {} [variadic positional]", v.as_str());
        }
        println!("  returns {}", spec.returns);
        println!("  example: {}", spec.example);
        println!();
    }
    ExitCode::SUCCESS
}

fn lint(src_path: &PathBuf) -> ExitCode {
    let src = match fs::read_to_string(src_path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("Error: failed to read `{}`: {e}", src_path.display());
            return ExitCode::from(1);
        }
    };
    let file = match parse(&src) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::from(1);
        }
    };
    let report = semantic::check(&file);
    if report.is_ok() {
        // Single-table files keep their Phase 1/2 output verbatim so the
        // stability guarantee in the README holds. Multi-table files get a
        // richer report including the topological generation order.
        if file.tables.len() == 1 {
            println!(
                "ok: {} generates {} row(s) into table `{}`",
                src_path.display(),
                file.generate[0].count,
                file.tables[0].name,
            );
        } else {
            println!("ok: {}", src_path.display());
            println!("  output: {}", output_kind_label(file.output));
            let parts: Vec<String> = report
                .topo_order
                .iter()
                .map(|t| {
                    if let Some((parent, _col, (lo, hi))) = report.per_parent_owners.get(t) {
                        format!("{t} (per_parent {lo}..{hi} of {parent})")
                    } else {
                        let n = file.count_for(t).unwrap_or(0);
                        format!("{t} ({n})")
                    }
                })
                .collect();
            println!("  tables: {}", parts.join(", "));
            println!("  generation order: {}", report.topo_order.join(" → "));
        }
        ExitCode::SUCCESS
    } else {
        for e in &report.errors {
            eprintln!("{e}");
        }
        ExitCode::from(1)
    }
}

fn output_kind_label(k: crate::ast::OutputKind) -> &'static str {
    match k {
        crate::ast::OutputKind::Sql => "sql",
        crate::ast::OutputKind::Json => "json",
        crate::ast::OutputKind::Postgis => "postgis",
    }
}

// `ArgType` is referenced inside this module via the spec; suppress the
// unused-import warning if it ever becomes redundant after future edits.
#[allow(dead_code)]
fn _arg_type_compile_check(_: ArgType) {}

#[allow(dead_code)]
fn _stdio_compile_check<W: Write>(_: &W) {}