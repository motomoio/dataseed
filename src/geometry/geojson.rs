//! GeoJSON value builder. Returns a `serde_json::Value` (not a string) so
//! the JSON output emitter can embed the geometry inline as an object,
//! producing `{"point": {"type": "Point", "coordinates": [...]}}` rather
//! than a stringified blob.

use serde_json::{json, Number, Value};

use super::{round_coord, Geometry};

pub fn to_geojson(g: &Geometry) -> Value {
    match g {
        Geometry::Point { lon, lat } => json!({
            "type": "Point",
            "coordinates": coord_pair(*lon, *lat),
        }),
        Geometry::LineString(verts) => json!({
            "type": "LineString",
            "coordinates": coords_array(verts),
        }),
        Geometry::Polygon { rings } => json!({
            "type": "Polygon",
            "coordinates": rings
                .iter()
                .map(|r| coords_array(r))
                .collect::<Vec<_>>(),
        }),
    }
}

fn coord_pair(lon: f64, lat: f64) -> Value {
    Value::Array(vec![number(lon), number(lat)])
}

fn coords_array(verts: &[(f64, f64)]) -> Value {
    Value::Array(verts.iter().map(|(x, y)| coord_pair(*x, *y)).collect())
}

/// Round-then-build the JSON Number. `from_f64` is the only public way to
/// build a Number from an f64 in stable serde_json; serialisation uses Ryu,
/// so the on-the-wire representation matches `fmt_coord` exactly.
fn number(x: f64) -> Value {
    Number::from_f64(round_coord(x))
        .map(Value::Number)
        .unwrap_or(Value::Null)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn point() {
        let v = to_geojson(&Geometry::Point { lon: 5.12, lat: 52.37 });
        assert_eq!(v["type"], "Point");
        assert_eq!(v["coordinates"][0], 5.12);
        assert_eq!(v["coordinates"][1], 52.37);
    }

    #[test]
    fn point_rounds_to_7_decimals() {
        let v = to_geojson(&Geometry::Point {
            lon: 5.12345678999,
            lat: 52.3712345,
        });
        // Re-serialise and parse to confirm the wire representation is rounded.
        let s = serde_json::to_string(&v).unwrap();
        assert!(s.contains("5.1234568"), "wire form should be rounded: {s}");
        assert!(!s.contains("5.123456789"), "raw precision must not leak: {s}");
    }

    #[test]
    fn linestring() {
        let v = to_geojson(&Geometry::LineString(vec![(5.1, 52.3), (5.2, 52.4)]));
        assert_eq!(v["type"], "LineString");
        let coords = v["coordinates"].as_array().unwrap();
        assert_eq!(coords.len(), 2);
        assert_eq!(coords[0][0], 5.1);
        assert_eq!(coords[1][1], 52.4);
    }

    #[test]
    fn polygon_coordinates_is_array_of_rings() {
        let v = to_geojson(&Geometry::Polygon {
            rings: vec![vec![
                (0.0, 0.0),
                (1.0, 0.0),
                (1.0, 1.0),
                (0.0, 1.0),
                (0.0, 0.0),
            ]],
        });
        assert_eq!(v["type"], "Polygon");
        let rings = v["coordinates"].as_array().unwrap();
        assert_eq!(rings.len(), 1, "single ring");
        let ring = rings[0].as_array().unwrap();
        assert_eq!(ring.len(), 5, "closed ring has first==last");
    }

    #[test]
    fn round_trip_through_wire_preserves_coords() {
        // round_coord → from_f64 → to_string → parse should give back the
        // same rounded f64. This is the property the SQL/JSON parity test
        // relies on.
        let g = Geometry::Point { lon: 5.1234568, lat: 52.3712345 };
        let s = serde_json::to_string(&to_geojson(&g)).unwrap();
        let parsed: Value = serde_json::from_str(&s).unwrap();
        assert_eq!(parsed["coordinates"][0].as_f64().unwrap(), 5.1234568);
        assert_eq!(parsed["coordinates"][1].as_f64().unwrap(), 52.3712345);
    }
}
