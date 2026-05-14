//! SQL INSERT emitter.
//!
//! Output shape — multi-row INSERTs in fixed-size batches:
//!
//! ```sql
//! INSERT INTO trees (id, species, height) VALUES
//!   (1, 'oak', 3.45),
//!   (2, 'birch', 12.30);
//! ```
//!
//! Strings are single-quoted with the SQL-standard `''` escape for embedded
//! quotes. Booleans render as `TRUE`/`FALSE` (Postgres-compatible; MySQL
//! accepts these too).

use std::io::{self, Write};

use crate::ast::Table;
use crate::generators::{Cell, Generator};
use crate::geometry::{write_ewkt, write_wkt};
use crate::rng::SeedRng;

const POSTGIS_SRID: u32 = 4326;

const BATCH_SIZE: u64 = 1000;

/// SQL flavour for geometry value emission.
///
/// * `Plain` — geometry values render as WKT strings (`'POINT(...)'`),
///   which load into any SQL database, no PostGIS required.
/// * `Postgis` — geometry values render as `ST_GeomFromText('...', 4326)`,
///   ready for a PostGIS-enabled Postgres.
///
/// Both dialects share the same row/batch structure; only the geometry
/// cell rendering differs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Dialect {
    Plain,
    Postgis,
}

pub(super) fn write_sql(
    table: &Table,
    gens: &mut [Box<dyn Generator>],
    count: u64,
    rng: &mut SeedRng,
    out: &mut dyn Write,
    dialect: Dialect,
    pool: &mut crate::pool::GeneratedPool,
) -> io::Result<()> {
    if count == 0 {
        // Still emit a comment so the file isn't empty — helps diff tooling.
        writeln!(out, "-- dataseed: 0 rows generated for `{}`.", table.name)?;
        return Ok(());
    }

    let columns: Vec<&str> = table.fields.iter().map(|f| f.name.as_str()).collect();
    let col_list = columns.join(", ");

    let mut row = 0u64;
    while row < count {
        let batch_end = (row + BATCH_SIZE).min(count);
        writeln!(out, "INSERT INTO {} ({}) VALUES", table.name, col_list)?;

        for r in row..batch_end {
            let cells = super::produce_row(&table.name, &table.fields, gens, rng, r, pool);
            let suffix = if r + 1 == batch_end { ";" } else { "," };
            write!(out, "  (")?;
            for (i, cell) in cells.iter().enumerate() {
                if i > 0 { write!(out, ", ")?; }
                write_cell(out, cell, dialect)?;
            }
            writeln!(out, "){suffix}")?;
        }
        row = batch_end;
    }
    Ok(())
}

fn write_cell(out: &mut dyn Write, cell: &Cell, dialect: Dialect) -> io::Result<()> {
    match cell {
        Cell::Integer(n) => write!(out, "{n}"),
        Cell::Real(n) => {
            // `{}` for f64 omits trailing zeros, which is fine for SQL.
            // For deterministic output we rely on Rust's f64 Display being
            // stable (it is — Grisu/Ryu-based).
            write!(out, "{n}")
        }
        Cell::Text(s) => write_quoted(out, s),
        Cell::Bool(b) => write!(out, "{}", if *b { "TRUE" } else { "FALSE" }),
        Cell::Geometry(g) => match dialect {
            // Plain SQL: WKT string in single quotes — loadable into any
            // database, no PostGIS extension required.
            Dialect::Plain => {
                out.write_all(b"'")?;
                write_wkt(out, g)?;
                out.write_all(b"'")
            }
            // PostGIS: full EWKT call. The WKT body can't contain a `'` so
            // we don't need SQL escaping.
            Dialect::Postgis => write_ewkt(out, g, POSTGIS_SRID),
        },
    }
}

fn write_quoted(out: &mut dyn Write, s: &str) -> io::Result<()> {
    out.write_all(b"'")?;
    // SQL-standard escaping: `'` → `''`. We're not handling backslash
    // because we don't enable the (non-standard) E'...' mode.
    let mut last = 0;
    let bytes = s.as_bytes();
    for (i, &b) in bytes.iter().enumerate() {
        if b == b'\'' {
            out.write_all(&bytes[last..i])?;
            out.write_all(b"''")?;
            last = i + 1;
        }
    }
    out.write_all(&bytes[last..])?;
    out.write_all(b"'")?;
    Ok(())
}
