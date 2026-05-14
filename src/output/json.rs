//! JSON output emitter. Streams a top-level array of records so partial
//! output is still well-formed up to the closing `]`.

use std::io::{self, Write};

use serde_json::{Map, Number, Value};

use crate::ast::Table;
use crate::generators::{Cell, Generator};
use crate::geometry::to_geojson;
use crate::rng::SeedRng;

#[allow(clippy::too_many_arguments)]
pub(super) fn write_json(
    table: &Table,
    gens: &mut [Box<dyn Generator>],
    count: u64,
    rng: &mut SeedRng,
    out: &mut dyn Write,
    pool: &mut crate::pool::GeneratedPool,
    forced_parent_assignment: Option<(&str, &str, &[usize])>,
    cached_target_cells: Option<&[Vec<Option<Cell>>]>,
) -> io::Result<()> {
    if count == 0 {
        writeln!(out, "[]")?;
        return Ok(());
    }

    writeln!(out, "[")?;
    for row in 0..count {
        let forced_parent = forced_parent_assignment.map(|(t, c, assn)| (t, c, assn[row as usize]));
        let row_cache = cached_target_cells.map(|c| c[row as usize].as_slice());
        let cells = super::produce_row(
            &table.name, &table.fields, gens, rng, row, pool, forced_parent, row_cache,
        );
        let mut obj = Map::with_capacity(table.fields.len());
        for (field, cell) in table.fields.iter().zip(cells.iter()) {
            obj.insert(field.name.clone(), cell_to_json(cell));
        }
        let comma = if row + 1 == count { "" } else { "," };
        // Indent by 2 spaces for human readability — same shape SQL output uses.
        let serialised = serde_json::to_string(&Value::Object(obj))
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
        writeln!(out, "  {serialised}{comma}")?;
    }
    writeln!(out, "]")?;
    Ok(())
}

/// Like `write_json` but renders the array inline (no trailing newline
/// before `]`). Used inside the multi-table top-level object so the comma
/// between table entries lands cleanly.
#[allow(clippy::too_many_arguments)]
pub(super) fn write_json_inline(
    table: &Table,
    gens: &mut [Box<dyn Generator>],
    count: u64,
    rng: &mut SeedRng,
    out: &mut dyn Write,
    pool: &mut crate::pool::GeneratedPool,
    forced_parent_assignment: Option<(&str, &str, &[usize])>,
    cached_target_cells: Option<&[Vec<Option<Cell>>]>,
) -> io::Result<()> {
    if count == 0 {
        write!(out, "[]")?;
        return Ok(());
    }
    writeln!(out, "[")?;
    for row in 0..count {
        let forced_parent = forced_parent_assignment.map(|(t, c, assn)| (t, c, assn[row as usize]));
        let row_cache = cached_target_cells.map(|c| c[row as usize].as_slice());
        let cells = super::produce_row(
            &table.name, &table.fields, gens, rng, row, pool, forced_parent, row_cache,
        );
        let mut obj = Map::with_capacity(table.fields.len());
        for (field, cell) in table.fields.iter().zip(cells.iter()) {
            obj.insert(field.name.clone(), cell_to_json(cell));
        }
        let comma = if row + 1 == count { "" } else { "," };
        let serialised = serde_json::to_string(&Value::Object(obj))
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
        writeln!(out, "    {serialised}{comma}")?;
    }
    // No trailing newline after `]` — caller appends `,` or newline itself.
    write!(out, "  ]")?;
    Ok(())
}

fn cell_to_json(cell: &Cell) -> Value {
    match cell {
        Cell::Integer(n) => Value::Number(Number::from(*n)),
        // JSON spec disallows NaN/Infinity, and our generators never produce
        // them, but `Number::from_f64` returns Option to be safe — fall back
        // to null in the (unreachable) bad case rather than panicking.
        Cell::Real(n) => Number::from_f64(*n)
            .map(Value::Number)
            .unwrap_or(Value::Null),
        Cell::Text(s) => Value::String(s.clone()),
        Cell::Bool(b) => Value::Bool(*b),
        // Geometries embed as an inline GeoJSON object so consumers can use
        // the data without re-parsing a string.
        Cell::Geometry(g) => to_geojson(g),
    }
}
