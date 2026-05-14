//! CREATE TABLE emission for SQL/PostGIS output.
//!
//! Column types are inferred from each generator's catalog `returns` field.
//! For `ref()` columns, the type is taken from the target column (which
//! itself may be a ref — resolution recurses).
//!
//! This module is purely a SQL/PostGIS concern; JSON output ignores DDL
//! entirely (the CLI prints a stderr warning if --emit-ddl is asked for
//! with `output: json`).

use crate::ast::{File, OutputKind, Table, Value};
use crate::generators::spec;

/// Infer the SQL column type for `table.fields[field_idx]` under the given
/// output kind. For `ref()` columns, recurses into the target column.
pub fn sql_type_for(table: &Table, field_idx: usize, file: &File) -> String {
    let field = &table.fields[field_idx];
    let function = field.call.function.as_str();

    // `ref` resolves to its target column's type.
    if function == "ref" {
        if let Some((target_table, target_column)) = ref_target(&field.call) {
            if let Some(t) = file.table(&target_table) {
                if let Some(idx) = t.fields.iter().position(|f| f.name == target_column) {
                    return sql_type_for(t, idx, file);
                }
            }
        }
        // Target not found — semantic check should have caught this.
        // Fall through to TEXT as a safe default.
        return "TEXT".into();
    }

    let returns = spec::lookup(function)
        .map(|s| s.returns)
        .unwrap_or("string");
    match (returns, file.output) {
        ("integer", _) => "BIGINT".into(),
        ("number", _) => "DOUBLE PRECISION".into(),
        ("string", _) => "TEXT".into(),
        ("boolean", _) => "BOOLEAN".into(),
        ("geometry:point", OutputKind::Postgis) => "geometry(Point, 4326)".into(),
        ("geometry:point", _) => "TEXT".into(),
        ("geometry:linestring", OutputKind::Postgis) => "geometry(LineString, 4326)".into(),
        ("geometry:linestring", _) => "TEXT".into(),
        ("geometry:polygon", OutputKind::Postgis) => "geometry(Polygon, 4326)".into(),
        ("geometry:polygon", _) => "TEXT".into(),
        ("any", _) => "TEXT".into(),
        _ => "TEXT".into(),
    }
}

fn ref_target(call: &crate::ast::Call) -> Option<(String, String)> {
    call.positional.iter().find_map(|v| match v {
        Value::ColumnRef { table, column } => Some((table.clone(), column.clone())),
        _ => None,
    })
}

/// Emit `CREATE TABLE name (col1 type1, col2 type2, ...);\n`.
pub fn write_create_table(
    out: &mut dyn std::io::Write,
    table: &Table,
    file: &File,
) -> std::io::Result<()> {
    writeln!(out, "CREATE TABLE {} (", table.name)?;
    for (i, field) in table.fields.iter().enumerate() {
        let t = sql_type_for(table, i, file);
        let suffix = if i + 1 == table.fields.len() { "" } else { "," };
        writeln!(out, "  {} {}{}", field.name, t, suffix)?;
    }
    writeln!(out, ");")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::parse;

    #[test]
    fn ddl_for_simple_table() {
        let src = r#"
            output: sql
            table t {
              id:    sequence
              name:  randomName()
              score: randomRealNumber(0.0, 100.0)
              ok:    randomBool()
            }
            generate t: 1
        "#;
        let file = parse(src).unwrap();
        let mut buf: Vec<u8> = Vec::new();
        write_create_table(&mut buf, &file.tables[0], &file).unwrap();
        let s = String::from_utf8(buf).unwrap();
        assert!(s.contains("id BIGINT"));
        assert!(s.contains("name TEXT"));
        assert!(s.contains("score DOUBLE PRECISION"));
        assert!(s.contains("ok BOOLEAN"));
        assert!(s.contains("CREATE TABLE t ("));
        assert!(s.contains(");"));
    }

    #[test]
    fn ddl_for_postgis_geometry() {
        let src = r#"
            output: postgis
            table sensors {
              id:       sequence
              location: randomPoint(bbox: [3.0, 51.0, 7.0, 53.0])
            }
            generate sensors: 1
        "#;
        let file = parse(src).unwrap();
        let mut buf: Vec<u8> = Vec::new();
        write_create_table(&mut buf, &file.tables[0], &file).unwrap();
        let s = String::from_utf8(buf).unwrap();
        assert!(s.contains("location geometry(Point, 4326)"), "{s}");
    }

    #[test]
    fn ddl_for_sql_geometry_uses_text() {
        // Plain `output: sql` should not emit PostGIS column types — geometry
        // values render as WKT strings, so the column is TEXT.
        let src = r#"
            output: sql
            table sensors {
              id:       sequence
              location: randomPoint(bbox: [3.0, 51.0, 7.0, 53.0])
            }
            generate sensors: 1
        "#;
        let file = parse(src).unwrap();
        let mut buf: Vec<u8> = Vec::new();
        write_create_table(&mut buf, &file.tables[0], &file).unwrap();
        let s = String::from_utf8(buf).unwrap();
        assert!(s.contains("location TEXT"), "{s}");
    }

    #[test]
    fn ddl_for_ref_inherits_target_type() {
        let src = r#"
            output: sql
            table users { id: sequence }
            table orders { id: sequence  user_id: ref(users.id) }
            generate users: 1
            generate orders: 1
        "#;
        let file = parse(src).unwrap();
        let orders = file.table("orders").unwrap();
        let mut buf: Vec<u8> = Vec::new();
        write_create_table(&mut buf, orders, &file).unwrap();
        let s = String::from_utf8(buf).unwrap();
        assert!(s.contains("user_id BIGINT"), "{s}");
    }

    #[test]
    fn ddl_for_postgis_linestring_polygon() {
        let src = r#"
            output: postgis
            table roads {
              id:    sequence
              shape: randomLineString(bbox: [3.0, 51.0, 7.0, 53.0])
            }
            table fields {
              id:    sequence
              shape: randomPolygon(bbox: [3.0, 51.0, 7.0, 53.0])
            }
            generate roads: 1
            generate fields: 1
        "#;
        let file = parse(src).unwrap();
        let roads = file.table("roads").unwrap();
        let fields = file.table("fields").unwrap();

        let mut buf1: Vec<u8> = Vec::new();
        write_create_table(&mut buf1, roads, &file).unwrap();
        assert!(String::from_utf8(buf1).unwrap().contains("shape geometry(LineString, 4326)"));

        let mut buf2: Vec<u8> = Vec::new();
        write_create_table(&mut buf2, fields, &file).unwrap();
        assert!(String::from_utf8(buf2).unwrap().contains("shape geometry(Polygon, 4326)"));
    }

    #[test]
    fn ddl_for_random_choice_is_text() {
        let src = r#"
            output: sql
            table t {
              id:     sequence
              status: randomChoice("a", "b", "c")
            }
            generate t: 1
        "#;
        let file = parse(src).unwrap();
        let mut buf: Vec<u8> = Vec::new();
        write_create_table(&mut buf, &file.tables[0], &file).unwrap();
        let s = String::from_utf8(buf).unwrap();
        assert!(s.contains("status TEXT"), "{s}");
    }

    #[test]
    fn ddl_handles_chained_refs() {
        // user_id -> users.id (BIGINT) -> stays BIGINT through two levels.
        let src = r#"
            output: sql
            table users { id: sequence }
            table orders { id: sequence  user_id: ref(users.id) }
            table line_items { id: sequence  order_user: ref(orders.user_id) }
            generate users: 1
            generate orders: 1
            generate line_items: 1
        "#;
        let file = parse(src).unwrap();
        let li = file.table("line_items").unwrap();
        let mut buf: Vec<u8> = Vec::new();
        write_create_table(&mut buf, li, &file).unwrap();
        let s = String::from_utf8(buf).unwrap();
        assert!(s.contains("order_user BIGINT"), "{s}");
    }
}
