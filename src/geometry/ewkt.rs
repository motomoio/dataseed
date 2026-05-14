//! EWKT (Extended WKT) for PostGIS. Wraps a WKT literal in
//! `ST_GeomFromText('...', SRID)`. SRID is taken as a parameter rather
//! than hardcoded so Phase 3+ can introduce non-WGS84 outputs without
//! changing the formatter signature.

use std::io::{self, Write};

use super::{write_wkt, Geometry};

pub fn write_ewkt<W: Write + ?Sized>(out: &mut W, g: &Geometry, srid: u32) -> io::Result<()> {
    write!(out, "ST_GeomFromText('")?;
    write_wkt(out, g)?;
    write!(out, "', {srid})")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ewkt(g: &Geometry, srid: u32) -> String {
        let mut buf = Vec::new();
        write_ewkt(&mut buf, g, srid).unwrap();
        String::from_utf8(buf).unwrap()
    }

    #[test]
    fn point_wraps_in_st_geom_from_text() {
        let p = Geometry::Point { lon: 5.12, lat: 52.37 };
        assert_eq!(ewkt(&p, 4326), "ST_GeomFromText('POINT(5.12 52.37)', 4326)");
    }

    #[test]
    fn linestring() {
        let l = Geometry::LineString(vec![(0.0, 0.0), (1.0, 1.0)]);
        assert_eq!(
            ewkt(&l, 4326),
            "ST_GeomFromText('LINESTRING(0 0, 1 1)', 4326)"
        );
    }

    #[test]
    fn polygon() {
        let p = Geometry::Polygon {
            rings: vec![vec![(0.0, 0.0), (1.0, 0.0), (1.0, 1.0), (0.0, 0.0)]],
        };
        assert_eq!(
            ewkt(&p, 4326),
            "ST_GeomFromText('POLYGON((0 0, 1 0, 1 1, 0 0))', 4326)"
        );
    }

    #[test]
    fn srid_parameter_is_honoured() {
        let p = Geometry::Point { lon: 0.0, lat: 0.0 };
        assert_eq!(
            ewkt(&p, 3857),
            "ST_GeomFromText('POINT(0 0)', 3857)"
        );
    }

    #[test]
    fn wkt_contents_match_standalone_wkt() {
        // EWKT must embed exactly the same WKT bytes as the standalone
        // formatter — that's the parity guarantee.
        let g = Geometry::Point { lon: 5.1234568, lat: 52.3712345 };
        let mut plain = Vec::new();
        write_wkt(&mut plain, &g).unwrap();
        let plain_s = String::from_utf8(plain).unwrap();
        let wrapped = ewkt(&g, 4326);
        assert!(
            wrapped.contains(&plain_s),
            "EWKT must embed standalone WKT verbatim:\n  ewkt: {wrapped}\n  wkt:  {plain_s}"
        );
    }
}
