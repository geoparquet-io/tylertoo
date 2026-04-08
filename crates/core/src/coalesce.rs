//! Geometry coalescing for dense tiles.
//!
//! This module implements GeoParquet-native predictive coalescing that merges
//! geometries into Multi* types to reduce tile complexity without losing data.
//!
//! Unlike tippecanoe's reactive approach (encode → measure → retry), we predict
//! dense tiles upfront using row group metadata.

use geo::Geometry;

/// Result of attempting to coalesce two geometries.
#[derive(Debug)]
pub enum CoalesceResult {
    /// Geometries were merged into target
    Merged,
    /// Type mismatch - source should be kept as separate feature
    TypeMismatch(Geometry),
}

/// Coalesce source geometry into target, converting to Multi* as needed.
///
/// Geometries are only coalesced within the same "family":
/// - Point/MultiPoint
/// - LineString/MultiLineString/Line
/// - Polygon/MultiPolygon/Rect/Triangle
///
/// Type mismatches return `CoalesceResult::TypeMismatch` with the source geometry.
///
/// # Arguments
///
/// * `target` - Mutable reference to the target geometry (will be modified)
/// * `source` - Source geometry to coalesce into target
///
/// # Returns
///
/// `CoalesceResult::Merged` if successful, `CoalesceResult::TypeMismatch(source)` otherwise.
pub fn coalesce_geometries(target: &mut Geometry, source: Geometry) -> CoalesceResult {
    use Geometry::*;

    // Handle convertible types first (before the main match)
    let source = match source {
        Line(l) => LineString(l.into()),
        Rect(r) => Polygon(r.to_polygon()),
        Triangle(t) => Polygon(t.to_polygon()),
        other => other,
    };

    // Handle GeometryCollection separately
    if let GeometryCollection(gc) = source {
        let mut unmerged = Vec::new();
        for geom in gc.0 {
            if let CoalesceResult::TypeMismatch(g) = coalesce_geometries(target, geom) {
                unmerged.push(g);
            }
        }
        return if unmerged.is_empty() {
            CoalesceResult::Merged
        } else if unmerged.len() == 1 {
            CoalesceResult::TypeMismatch(unmerged.remove(0))
        } else {
            CoalesceResult::TypeMismatch(GeometryCollection(geo::GeometryCollection::new_from(
                unmerged,
            )))
        };
    }

    match (&*target, source) {
        // === Point family ===
        (Point(p1), Point(p2)) => {
            *target = MultiPoint(geo::MultiPoint::new(vec![*p1, p2]));
            CoalesceResult::Merged
        }
        (MultiPoint(mp), Point(p)) => {
            if let MultiPoint(mp) = target {
                mp.0.push(p);
            }
            CoalesceResult::Merged
        }
        (Point(p1), MultiPoint(mp2)) => {
            let mut points = vec![*p1];
            points.extend(mp2.0);
            *target = MultiPoint(geo::MultiPoint::new(points));
            CoalesceResult::Merged
        }
        (MultiPoint(_), MultiPoint(mp2)) => {
            if let MultiPoint(mp1) = target {
                mp1.0.extend(mp2.0);
            }
            CoalesceResult::Merged
        }

        // === LineString family ===
        (LineString(l1), LineString(l2)) => {
            let l1_clone = l1.clone();
            *target = MultiLineString(geo::MultiLineString::new(vec![l1_clone, l2]));
            CoalesceResult::Merged
        }
        (MultiLineString(_), LineString(l)) => {
            if let MultiLineString(ml) = target {
                ml.0.push(l);
            }
            CoalesceResult::Merged
        }
        (LineString(l1), MultiLineString(ml2)) => {
            let mut lines = vec![l1.clone()];
            lines.extend(ml2.0);
            *target = MultiLineString(geo::MultiLineString::new(lines));
            CoalesceResult::Merged
        }
        (MultiLineString(_), MultiLineString(ml2)) => {
            if let MultiLineString(ml1) = target {
                ml1.0.extend(ml2.0);
            }
            CoalesceResult::Merged
        }

        // === Polygon family ===
        (Polygon(p1), Polygon(p2)) => {
            let p1_clone = p1.clone();
            *target = MultiPolygon(geo::MultiPolygon::new(vec![p1_clone, p2]));
            CoalesceResult::Merged
        }
        (MultiPolygon(_), Polygon(p)) => {
            if let MultiPolygon(mp) = target {
                mp.0.push(p);
            }
            CoalesceResult::Merged
        }
        (Polygon(p1), MultiPolygon(mp2)) => {
            let mut polys = vec![p1.clone()];
            polys.extend(mp2.0);
            *target = MultiPolygon(geo::MultiPolygon::new(polys));
            CoalesceResult::Merged
        }
        (MultiPolygon(_), MultiPolygon(mp2)) => {
            if let MultiPolygon(mp1) = target {
                mp1.0.extend(mp2.0);
            }
            CoalesceResult::Merged
        }

        // === Type mismatch: return source unchanged ===
        (_, source) => CoalesceResult::TypeMismatch(source),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use geo::{
        coord, point, polygon, GeometryCollection, Line, LineString, MultiPoint, Rect, Triangle,
    };

    // =========================================================================
    // Point family coalescing
    // =========================================================================

    #[test]
    fn test_point_plus_point_becomes_multipoint() {
        let mut target = Geometry::Point(point!(x: 0.0, y: 0.0));
        let source = Geometry::Point(point!(x: 1.0, y: 1.0));

        let result = coalesce_geometries(&mut target, source);

        assert!(matches!(result, CoalesceResult::Merged));
        match &target {
            Geometry::MultiPoint(mp) => {
                assert_eq!(mp.0.len(), 2);
                assert_eq!(mp.0[0], point!(x: 0.0, y: 0.0));
                assert_eq!(mp.0[1], point!(x: 1.0, y: 1.0));
            }
            _ => panic!("Expected MultiPoint, got {:?}", target),
        }
    }

    #[test]
    fn test_multipoint_plus_point_extends() {
        let mut target = Geometry::MultiPoint(MultiPoint::new(vec![
            point!(x: 0.0, y: 0.0),
            point!(x: 1.0, y: 1.0),
        ]));
        let source = Geometry::Point(point!(x: 2.0, y: 2.0));

        let result = coalesce_geometries(&mut target, source);

        assert!(matches!(result, CoalesceResult::Merged));
        match &target {
            Geometry::MultiPoint(mp) => {
                assert_eq!(mp.0.len(), 3);
            }
            _ => panic!("Expected MultiPoint"),
        }
    }

    #[test]
    fn test_multipoint_plus_multipoint_merges() {
        let mut target = Geometry::MultiPoint(MultiPoint::new(vec![point!(x: 0.0, y: 0.0)]));
        let source = Geometry::MultiPoint(MultiPoint::new(vec![
            point!(x: 1.0, y: 1.0),
            point!(x: 2.0, y: 2.0),
        ]));

        let result = coalesce_geometries(&mut target, source);

        assert!(matches!(result, CoalesceResult::Merged));
        match &target {
            Geometry::MultiPoint(mp) => {
                assert_eq!(mp.0.len(), 3);
            }
            _ => panic!("Expected MultiPoint"),
        }
    }

    // =========================================================================
    // LineString family coalescing
    // =========================================================================

    #[test]
    fn test_linestring_plus_linestring_becomes_multilinestring() {
        let mut target = Geometry::LineString(LineString::new(vec![
            coord!(x: 0.0, y: 0.0),
            coord!(x: 1.0, y: 1.0),
        ]));
        let source = Geometry::LineString(LineString::new(vec![
            coord!(x: 2.0, y: 2.0),
            coord!(x: 3.0, y: 3.0),
        ]));

        let result = coalesce_geometries(&mut target, source);

        assert!(matches!(result, CoalesceResult::Merged));
        match &target {
            Geometry::MultiLineString(mls) => {
                assert_eq!(mls.0.len(), 2);
            }
            _ => panic!("Expected MultiLineString"),
        }
    }

    #[test]
    fn test_line_coalesces_as_linestring() {
        let mut target = Geometry::LineString(LineString::new(vec![
            coord!(x: 0.0, y: 0.0),
            coord!(x: 1.0, y: 1.0),
        ]));
        let source = Geometry::Line(Line::new(coord!(x: 2.0, y: 2.0), coord!(x: 3.0, y: 3.0)));

        let result = coalesce_geometries(&mut target, source);

        assert!(matches!(result, CoalesceResult::Merged));
        match &target {
            Geometry::MultiLineString(mls) => {
                assert_eq!(mls.0.len(), 2);
            }
            _ => panic!("Expected MultiLineString"),
        }
    }

    // =========================================================================
    // Polygon family coalescing
    // =========================================================================

    #[test]
    fn test_polygon_plus_polygon_becomes_multipolygon() {
        let mut target = Geometry::Polygon(polygon![
            (x: 0.0, y: 0.0),
            (x: 1.0, y: 0.0),
            (x: 1.0, y: 1.0),
            (x: 0.0, y: 1.0),
            (x: 0.0, y: 0.0),
        ]);
        let source = Geometry::Polygon(polygon![
            (x: 2.0, y: 2.0),
            (x: 3.0, y: 2.0),
            (x: 3.0, y: 3.0),
            (x: 2.0, y: 3.0),
            (x: 2.0, y: 2.0),
        ]);

        let result = coalesce_geometries(&mut target, source);

        assert!(matches!(result, CoalesceResult::Merged));
        match &target {
            Geometry::MultiPolygon(mp) => {
                assert_eq!(mp.0.len(), 2);
            }
            _ => panic!("Expected MultiPolygon"),
        }
    }

    #[test]
    fn test_rect_coalesces_as_polygon() {
        let mut target = Geometry::Polygon(polygon![
            (x: 0.0, y: 0.0),
            (x: 1.0, y: 0.0),
            (x: 1.0, y: 1.0),
            (x: 0.0, y: 1.0),
            (x: 0.0, y: 0.0),
        ]);
        let source = Geometry::Rect(Rect::new(coord!(x: 2.0, y: 2.0), coord!(x: 3.0, y: 3.0)));

        let result = coalesce_geometries(&mut target, source);

        assert!(matches!(result, CoalesceResult::Merged));
        match &target {
            Geometry::MultiPolygon(mp) => {
                assert_eq!(mp.0.len(), 2);
            }
            _ => panic!("Expected MultiPolygon"),
        }
    }

    #[test]
    fn test_triangle_coalesces_as_polygon() {
        let mut target = Geometry::Polygon(polygon![
            (x: 0.0, y: 0.0),
            (x: 1.0, y: 0.0),
            (x: 1.0, y: 1.0),
            (x: 0.0, y: 1.0),
            (x: 0.0, y: 0.0),
        ]);
        let source = Geometry::Triangle(Triangle::new(
            coord!(x: 2.0, y: 2.0),
            coord!(x: 3.0, y: 2.0),
            coord!(x: 2.5, y: 3.0),
        ));

        let result = coalesce_geometries(&mut target, source);

        assert!(matches!(result, CoalesceResult::Merged));
        match &target {
            Geometry::MultiPolygon(mp) => {
                assert_eq!(mp.0.len(), 2);
            }
            _ => panic!("Expected MultiPolygon"),
        }
    }

    // =========================================================================
    // Type mismatch handling
    // =========================================================================

    #[test]
    fn test_point_plus_linestring_mismatch() {
        let mut target = Geometry::Point(point!(x: 0.0, y: 0.0));
        let source = Geometry::LineString(LineString::new(vec![
            coord!(x: 1.0, y: 1.0),
            coord!(x: 2.0, y: 2.0),
        ]));

        let result = coalesce_geometries(&mut target, source);

        match result {
            CoalesceResult::TypeMismatch(g) => {
                assert!(matches!(g, Geometry::LineString(_)));
            }
            _ => panic!("Expected TypeMismatch"),
        }
        // Target should be unchanged
        assert!(matches!(target, Geometry::Point(_)));
    }

    #[test]
    fn test_polygon_plus_point_mismatch() {
        let mut target = Geometry::Polygon(polygon![
            (x: 0.0, y: 0.0),
            (x: 1.0, y: 0.0),
            (x: 1.0, y: 1.0),
            (x: 0.0, y: 0.0),
        ]);
        let source = Geometry::Point(point!(x: 5.0, y: 5.0));

        let result = coalesce_geometries(&mut target, source);

        assert!(matches!(result, CoalesceResult::TypeMismatch(_)));
    }

    // =========================================================================
    // GeometryCollection handling
    // =========================================================================

    #[test]
    fn test_geometry_collection_flattens_and_coalesces() {
        let mut target = Geometry::Point(point!(x: 0.0, y: 0.0));
        let source = Geometry::GeometryCollection(GeometryCollection::new_from(vec![
            Geometry::Point(point!(x: 1.0, y: 1.0)),
            Geometry::Point(point!(x: 2.0, y: 2.0)),
        ]));

        let result = coalesce_geometries(&mut target, source);

        assert!(matches!(result, CoalesceResult::Merged));
        match &target {
            Geometry::MultiPoint(mp) => {
                assert_eq!(mp.0.len(), 3);
            }
            _ => panic!("Expected MultiPoint with 3 points"),
        }
    }

    #[test]
    fn test_geometry_collection_with_mixed_types_returns_unmerged() {
        let mut target = Geometry::Point(point!(x: 0.0, y: 0.0));
        let source = Geometry::GeometryCollection(GeometryCollection::new_from(vec![
            Geometry::Point(point!(x: 1.0, y: 1.0)),
            Geometry::LineString(LineString::new(vec![
                coord!(x: 2.0, y: 2.0),
                coord!(x: 3.0, y: 3.0),
            ])),
        ]));

        let result = coalesce_geometries(&mut target, source);

        // The point should be merged, but the linestring should be returned as mismatch
        match result {
            CoalesceResult::TypeMismatch(g) => {
                // Should contain the linestring that couldn't be merged
                match g {
                    Geometry::GeometryCollection(gc) => {
                        assert_eq!(gc.0.len(), 1);
                        assert!(matches!(gc.0[0], Geometry::LineString(_)));
                    }
                    Geometry::LineString(_) => {
                        // Also acceptable if only one unmerged
                    }
                    _ => panic!("Expected unmerged geometries"),
                }
            }
            CoalesceResult::Merged => {
                // If all merged, target should be MultiPoint with the point only
                // (but this shouldn't happen with mixed types)
                panic!("Expected TypeMismatch for mixed GeometryCollection");
            }
        }
    }
}
