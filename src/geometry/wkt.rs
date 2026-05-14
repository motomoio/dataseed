//! WKT (Well-Known Text) serialisation. The textual format that's
//! universally understood by GIS tooling — PostGIS, GDAL, QGIS, etc.

use std::io::{self, Write};

use super::{fmt_coord, Geometry};

pub fn write_wkt<W: Write + ?Sized>(out: &mut W, g: &Geometry) -> io::Result<()> {
    match g {
        Geometry::Point { lon, lat } => {
            write!(out, "POINT({} {})", fmt_coord(*lon), fmt_coord(*lat))
        }
        Geometry::LineString(verts) => {
            write!(out, "LINESTRING(")?;
            write_vertex_list(out, verts)?;
            write!(out, ")")
        }
        Geometry::Polygon { rings } => {
            write!(out, "POLYGON(")?;
            for (i, ring) in rings.iter().enumerate() {
                if i > 0 {
                    write!(out, ", ")?;
                }
                write!(out, "(")?;
                write_vertex_list(out, ring)?;
                write!(out, ")")?;
            }
            write!(out, ")")
        }
    }
}

fn write_vertex_list<W: Write + ?Sized>(out: &mut W, verts: &[(f64, f64)]) -> io::Result<()> {
    for (i, (x, y)) in verts.iter().enumerate() {
        if i > 0 {
            write!(out, ", ")?;
        }
        write!(out, "{} {}", fmt_coord(*x), fmt_coord(*y))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wkt(g: &Geometry) -> String {
        let mut buf = Vec::new();
        write_wkt(&mut buf, g).unwrap();
        String::from_utf8(buf).unwrap()
    }

    #[test]
    fn point() {
        let p = Geometry::Point { lon: 5.12, lat: 52.37 };
        assert_eq!(wkt(&p), "POINT(5.12 52.37)");
    }

    #[test]
    fn point_rounds_to_7_decimals() {
        let p = Geometry::Point { lon: 5.123456789, lat: 52.3712345 };
        assert_eq!(wkt(&p), "POINT(5.1234568 52.3712345)");
    }

    #[test]
    fn linestring() {
        let l = Geometry::LineString(vec![(5.1, 52.3), (5.2, 52.4), (5.21, 52.45)]);
        assert_eq!(wkt(&l), "LINESTRING(5.1 52.3, 5.2 52.4, 5.21 52.45)");
    }

    #[test]
    fn polygon_single_ring() {
        let p = Geometry::Polygon {
            rings: vec![vec![
                (5.1, 52.3),
                (5.2, 52.3),
                (5.2, 52.4),
                (5.1, 52.4),
                (5.1, 52.3),
            ]],
        };
        assert_eq!(
            wkt(&p),
            "POLYGON((5.1 52.3, 5.2 52.3, 5.2 52.4, 5.1 52.4, 5.1 52.3))"
        );
    }

    #[test]
    fn polygon_with_hole_format() {
        // Phase 2 generators only produce single rings, but the format
        // must already support multiple rings for Phase 3+.
        let p = Geometry::Polygon {
            rings: vec![
                vec![(0.0, 0.0), (10.0, 0.0), (10.0, 10.0), (0.0, 10.0), (0.0, 0.0)],
                vec![(2.0, 2.0), (4.0, 2.0), (4.0, 4.0), (2.0, 4.0), (2.0, 2.0)],
            ],
        };
        let s = wkt(&p);
        assert!(s.starts_with("POLYGON(("), "got: {s}");
        assert!(s.contains("), ("), "rings separated by comma-space-paren: {s}");
        assert!(s.ends_with("))"), "got: {s}");
    }
}
