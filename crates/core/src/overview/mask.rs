//! `--mask` (#826): keep only the features that intersect a polygon.
//!
//! The mask's bbox joins `--bbox` in the footer-statistics pruning
//! ([`region_query`]); [`MaskIndex`] then tests each surviving feature
//! exactly. The index answers in about O(feature vertices × log mask
//! vertices): an R-tree over the mask's edges finds boundary contacts, and
//! an interval tree answers point-in-mask.

use std::path::{Path, PathBuf};

use geo::algorithm::indexed::IntervalTreeMultiPolygon;
use geo::{
    BoundingRect, Contains, Coord, Geometry, HasDimensions, Intersects, Line, LineString,
    MultiPolygon, Point, Polygon, Translate,
};
use rstar::primitives::{GeomWithData, Rectangle};
use rstar::{RTree, AABB};

use super::config::ConfigError;
use super::convert::{BboxQuery, ConvertError};
use super::level::Crs;

/// Where a [`ConvertOptions::mask`](super::convert::ConvertOptions::mask)
/// polygon comes from. TODO(docs)
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(untagged)]
#[non_exhaustive]
pub enum MaskSource {
    /// TODO(docs)
    Path(PathBuf),
    /// TODO(docs)
    Wkb(Vec<u8>),
}

impl From<PathBuf> for MaskSource {
    fn from(path: PathBuf) -> Self {
        Self::Path(path)
    }
}

/// Clap's parser for `--mask PATH`.
#[cfg(feature = "cli")]
#[expect(clippy::unnecessary_wraps, reason = "clap's value_parser signature")]
pub(crate) fn parse_mask(s: &str) -> Result<MaskSource, std::convert::Infallible> {
    Ok(MaskSource::Path(s.into()))
}

fn invalid(message: String) -> ConfigError {
    ConfigError::InvalidSpec {
        option: "--mask",
        message: format!("--mask: {message}"),
    }
}

/// A loaded mask: its polygons and the CRS its source declares (`None` for
/// WKB, which carries no CRS and is taken to be in the input's).
#[derive(Debug, Clone)]
pub(crate) struct LoadedMask {
    polygons: MultiPolygon<f64>,
    crs: Option<Crs>,
}

/// Read and check a mask: readable, polygonal, not empty.
///
/// # Errors
/// A `--mask` [`ConfigError`] naming what is wrong.
pub(crate) fn load(source: &MaskSource) -> Result<LoadedMask, ConfigError> {
    // Validation runs once per option check, several times a run: parse a
    // mask once and hand out copies while its file is unchanged.
    type Key = (MaskSource, Option<(std::time::SystemTime, u64)>);
    static LAST: std::sync::Mutex<Option<(Key, LoadedMask)>> = std::sync::Mutex::new(None);
    let stamp = match source {
        MaskSource::Path(p) => std::fs::metadata(p)
            .and_then(|m| Ok((m.modified()?, m.len())))
            .ok(),
        MaskSource::Wkb(_) => None,
    };
    let key = (source.clone(), stamp);
    let cacheable = stamp.is_some() || matches!(source, MaskSource::Wkb(_));
    if cacheable {
        if let Some((k, mask)) = &*LAST
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
        {
            if *k == key {
                return Ok(mask.clone());
            }
        }
    }
    let mask = read(source)?;
    if cacheable {
        *LAST
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some((key, mask.clone()));
    }
    Ok(mask)
}

fn read(source: &MaskSource) -> Result<LoadedMask, ConfigError> {
    let (geoms, crs) = match source {
        MaskSource::Wkb(bytes) => (
            vec![crate::wkb::wkb_to_geometry(bytes)
                .map_err(|e| invalid(format!("invalid WKB: {e}")))?],
            None,
        ),
        MaskSource::Path(path) if is_geojson(path) => (read_geojson(path)?, Some(Crs::Epsg4326)),
        MaskSource::Path(path) => read_geoparquet(path)?,
    };
    let mut polygons = Vec::new();
    for g in geoms {
        collect_polygons(g, &mut polygons)?;
    }
    let polygons = MultiPolygon::new(polygons);
    if polygons.is_empty() {
        return Err(invalid("the mask holds no polygon area".into()));
    }
    Ok(LoadedMask { polygons, crs })
}

fn is_geojson(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("geojson") || e.eq_ignore_ascii_case("json"))
}

fn read_geojson(path: &Path) -> Result<Vec<Geometry<f64>>, ConfigError> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| invalid(format!("cannot read {}: {e}", path.display())))?;
    use geozero::ToGeo;
    match geozero::geojson::GeoJson(&text).to_geo() {
        Ok(Geometry::GeometryCollection(gc)) => Ok(gc.0),
        Ok(g) => Ok(vec![g]),
        Err(e) => Err(invalid(format!(
            "{}: no GeoJSON polygon: {e}",
            path.display()
        ))),
    }
}

fn read_geoparquet(path: &Path) -> Result<(Vec<Geometry<f64>>, Option<Crs>), ConfigError> {
    use crate::input_set::{ConvertSource, ReadPlan};
    let fail = |e: &dyn std::fmt::Display| invalid(format!("cannot read {}: {e}", path.display()));
    let source = ConvertSource::resolve_path(path).map_err(|e| fail(&e))?;
    let descriptor = source.resolved_schema().map_err(|e| fail(&e))?;
    let schema = source.schema().map_err(|e| fail(&e))?;
    let geom_idx = descriptor
        .geometry_name()
        .and_then(|n| schema.index_of(n).ok())
        .ok_or_else(|| invalid(format!("{} has no geometry column", path.display())))?;
    let crs = super::convert::crs_from_resolved(
        descriptor,
        &source.display_name(),
        &crate::diagnostics::Diagnostics::silent(),
    )
    .map_err(|e| fail(&e))?
    .0;
    let field = schema.field(geom_idx).clone();
    let diagnostics = crate::diagnostics::Diagnostics::silent();
    let mut geoms = Vec::new();
    let stream = source
        .open_stream(&ReadPlan {
            batch_size: 8192,
            projection: Some(&[geom_idx]),
            row_groups: None,
        })
        .map_err(|e| fail(&e))?;
    for batch in stream {
        let batch = batch.map_err(|e| fail(&e))?;
        let arr = geoarrow_array::array::from_arrow_array(batch.column(0).as_ref(), &field)
            .map_err(|e| fail(&e))?;
        let mut out = Vec::new();
        crate::batch_processor::extract_geometries_opt_from_array(
            arr.as_ref(),
            &mut out,
            &diagnostics,
        )
        .map_err(|e| fail(&e))?;
        geoms.extend(out.into_iter().flatten());
    }
    Ok((geoms, Some(crs)))
}

fn not_polygonal(kind: &str) -> ConfigError {
    invalid(format!(
        "the mask must be polygons or multipolygons, found a {kind}"
    ))
}

fn collect_polygons(g: Geometry<f64>, out: &mut Vec<Polygon<f64>>) -> Result<(), ConfigError> {
    match g {
        Geometry::Polygon(p) => out.push(p),
        Geometry::MultiPolygon(mp) => out.extend(mp.0),
        Geometry::Rect(r) => out.push(r.to_polygon()),
        Geometry::Triangle(t) => out.push(t.to_polygon()),
        Geometry::GeometryCollection(gc) => {
            for g in gc.0 {
                collect_polygons(g, out)?;
            }
        }
        Geometry::Point(_) | Geometry::MultiPoint(_) => return Err(not_polygonal("point")),
        Geometry::Line(_) | Geometry::LineString(_) | Geometry::MultiLineString(_) => {
            return Err(not_polygonal("line"))
        }
    }
    Ok(())
}

impl LoadedMask {
    /// The mask in the input's CRS.
    ///
    /// # Errors
    /// Refuses a mask whose CRS differs from the input's.
    // shortcut: refuses a CRS mismatch; reprojection (#670) replaces this check.
    pub(crate) fn into_input_crs(self, input: Crs) -> Result<MultiPolygon<f64>, ConvertError> {
        match self.crs {
            Some(crs) if crs != input => Err(invalid(format!(
                "the mask is in {crs:?} but the input is in {input:?}; reproject the mask \
                 to the input's CRS"
            ))
            .into()),
            _ => Ok(self.polygons),
        }
    }
}

/// Load `source` for an input in `crs` (load, then the CRS check).
///
/// # Errors
/// As [`load`] and [`LoadedMask::into_input_crs`].
pub(crate) fn load_for_input(
    source: &MaskSource,
    crs: Crs,
) -> Result<MultiPolygon<f64>, ConvertError> {
    load(source)?.into_input_crs(crs)
}

/// The footer-pruning and per-feature bbox query for `--bbox` AND `--mask`:
/// the bbox (lon/lat degrees) and the mask's bbox (CRS units), intersected.
pub(crate) fn region_query(
    bbox: Option<&[f64; 4]>,
    mask: Option<&MultiPolygon<f64>>,
    crs: Crs,
) -> Option<BboxQuery> {
    let bbox = bbox.map(|b| super::convert::bbox_to_crs_units(b, crs));
    let mask = mask.and_then(BoundingRect::bounding_rect).map(rect_box);
    let base = match (bbox, mask) {
        (Some(a), Some(b)) => [
            a[0].max(b[0]),
            a[1].max(b[1]),
            a[2].min(b[2]),
            a[3].min(b[3]),
        ],
        (Some(a), None) | (None, Some(a)) => a,
        (None, None) => return None,
    };
    Some(BboxQuery::in_crs_units(base, crs))
}

fn rect_box(r: geo::Rect<f64>) -> [f64; 4] {
    [r.min().x, r.min().y, r.max().x, r.max().y]
}

/// A mask indexed for exact per-feature intersects tests. `Send + Sync`,
/// so pass 1's chunk tasks share one.
pub(crate) struct MaskIndex {
    area: IntervalTreeMultiPolygon<f64>,
    edges: RTree<Line<f64>>,
    /// Each mask polygon's bbox, carrying one exterior vertex: a feature
    /// whose boundary no mask edge meets intersects that polygon only by
    /// holding it, so holding this vertex.
    parts: RTree<GeomWithData<Rectangle<[f64; 2]>, Coord<f64>>>,
    /// The mask bbox, and its copies one world width west and east (#643).
    query: BboxQuery,
    world: f64,
}

impl MaskIndex {
    pub(crate) fn new(mask: &MultiPolygon<f64>, crs: Crs) -> Self {
        use geo::{orient::Direction, LinesIter, Orient};
        // The interval tree counts winding numbers: holes must wind against
        // their exterior.
        let mask = &mask.orient(Direction::Default);
        let parts = mask
            .0
            .iter()
            .filter_map(|p| {
                let r = p.bounding_rect()?;
                Some(GeomWithData::new(
                    Rectangle::from_corners(r.min().into(), r.max().into()),
                    p.exterior().0[0],
                ))
            })
            .collect();
        Self {
            area: IntervalTreeMultiPolygon::new(mask),
            edges: RTree::bulk_load(mask.lines_iter().collect()),
            parts: RTree::bulk_load(parts),
            query: region_query(None, Some(mask), crs).expect("a mask has a bbox"),
            world: super::convert::world_width(crs),
        }
    }

    /// Whether a feature with bbox `fbbox` can reach the mask at all.
    pub(crate) fn may_hit(&self, fbbox: &[f64; 4]) -> bool {
        self.query.intersects(fbbox)
    }

    /// Whether `g` (bbox `fbbox`) intersects the mask, or would one world
    /// width away, as `--bbox` matches (#643).
    pub(crate) fn intersects(&self, g: &Geometry<f64>, fbbox: &[f64; 4]) -> bool {
        let shifts = [0.0, self.world, -self.world];
        self.query.boxes().iter().zip(shifts).any(|(b, dx)| {
            if !super::convert::bboxes_intersect(fbbox, b) {
                return false;
            }
            // No mask edge reaches the feature's bbox: the box lies
            // wholly inside or outside the mask, so one vertex decides.
            let env = AABB::from_corners(
                Point::new(fbbox[0] + dx, fbbox[1]),
                Point::new(fbbox[2] + dx, fbbox[3]),
            );
            let near: Vec<&Line<f64>> = self
                .edges
                .locate_in_envelope_intersecting(&env)
                .take(NEAR_EDGES_MAX + 1)
                .collect();
            if near.is_empty() {
                use geo::CoordsIter;
                return g.coords_iter().next().is_some_and(|c| {
                    self.area.contains(&Coord {
                        x: c.x + dx,
                        y: c.y,
                    })
                });
            }
            let hit = Hit {
                mask: self,
                near: (near.len() <= NEAR_EDGES_MAX).then_some(near.as_slice()),
            };
            if dx == 0.0 {
                hit.hits(g)
            } else {
                hit.hits(&g.translate(dx, 0.0))
            }
        })
    }
}

/// Above this many mask edges near a feature, its segments query the R-tree
/// one at a time instead of scanning the near edges.
const NEAR_EDGES_MAX: usize = 64;

/// One feature's exact test: the mask, plus the mask edges near the feature
/// when there are few.
struct Hit<'a> {
    mask: &'a MaskIndex,
    near: Option<&'a [&'a Line<f64>]>,
}

impl Hit<'_> {
    fn hits(&self, g: &Geometry<f64>) -> bool {
        match g {
            Geometry::Point(p) => self.hits_point(p.0),
            Geometry::MultiPoint(mp) => mp.iter().any(|p| self.hits_point(p.0)),
            Geometry::Line(l) => self.hits_line(&LineString::from(vec![l.start, l.end])),
            Geometry::LineString(ls) => self.hits_line(ls),
            Geometry::MultiLineString(mls) => mls.iter().any(|ls| self.hits_line(ls)),
            Geometry::Polygon(p) => self.hits_polygon(p),
            Geometry::MultiPolygon(mp) => mp.iter().any(|p| self.hits_polygon(p)),
            Geometry::Rect(r) => self.hits_polygon(&r.to_polygon()),
            Geometry::Triangle(t) => self.hits_polygon(&t.to_polygon()),
            Geometry::GeometryCollection(gc) => gc.iter().any(|g| self.hits(g)),
        }
    }

    /// Whether segment `a`–`b` meets a mask edge.
    fn meets_edge(&self, a: Coord<f64>, b: Coord<f64>) -> bool {
        let seg = Line::new(a, b);
        let (lo, hi) = (
            Point::new(a.x.min(b.x), a.y.min(b.y)),
            Point::new(a.x.max(b.x), a.y.max(b.y)),
        );
        match self.near {
            Some(near) => near.iter().any(|e| {
                e.start.x.min(e.end.x) <= hi.x()
                    && e.start.x.max(e.end.x) >= lo.x()
                    && e.start.y.min(e.end.y) <= hi.y()
                    && e.start.y.max(e.end.y) >= lo.y()
                    && e.intersects(&seg)
            }),
            None => self
                .mask
                .edges
                .locate_in_envelope_intersecting(&AABB::from_corners(lo, hi))
                .any(|e| e.intersects(&seg)),
        }
    }

    fn hits_point(&self, c: Coord<f64>) -> bool {
        self.mask.area.contains(&c) || self.meets_edge(c, c)
    }

    /// A vertex inside the mask decides at once; otherwise the line hits
    /// only by meeting a mask edge.
    fn hits_line(&self, ls: &LineString<f64>) -> bool {
        ls.0.first().is_some_and(|c| self.mask.area.contains(c))
            || ls.lines().any(|l| self.meets_edge(l.start, l.end))
    }

    /// As [`Self::hits_line`] for the rings, plus a mask polygon wholly
    /// inside the feature.
    fn hits_polygon(&self, p: &Polygon<f64>) -> bool {
        if p.is_empty() {
            return false;
        }
        if self.mask.area.contains(&p.exterior().0[0])
            || std::iter::once(p.exterior())
                .chain(p.interiors())
                .any(|r| r.lines().any(|l| self.meets_edge(l.start, l.end)))
        {
            return true;
        }
        let Some(r) = p.bounding_rect() else {
            return false;
        };
        let env = AABB::from_corners([r.min().x, r.min().y], [r.max().x, r.max().y]);
        self.mask
            .parts
            .locate_in_envelope_intersecting(&env)
            .any(|part| p.intersects(&part.data))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use geo::{line_string, point, polygon, wkt};

    /// An L-shaped (concave) mask with a hole: the notch at x,y in 2..4 is
    /// outside, as is the hole (0.5..1.5)².
    fn l_mask() -> MaskIndex {
        let mask = MultiPolygon::new(vec![polygon!(
            exterior: [
                (x: 0.0, y: 0.0), (x: 4.0, y: 0.0), (x: 4.0, y: 2.0),
                (x: 2.0, y: 2.0), (x: 2.0, y: 4.0), (x: 0.0, y: 4.0),
            ],
            interiors: [[
                (x: 0.5, y: 0.5), (x: 1.5, y: 0.5), (x: 1.5, y: 1.5), (x: 0.5, y: 1.5),
            ]],
        )]);
        MaskIndex::new(&mask, Crs::Epsg4326)
    }

    fn hit(m: &MaskIndex, g: Geometry<f64>) -> bool {
        let r = g.bounding_rect().unwrap();
        m.intersects(&g, &rect_box(r))
    }

    #[test]
    fn exact_intersects_on_a_concave_mask_with_a_hole() {
        let m = l_mask();
        // Points: inside, in the notch, in the hole, on the boundary.
        assert!(hit(&m, point!(x: 3.0, y: 1.0).into()));
        assert!(!hit(&m, point!(x: 3.0, y: 3.0).into()));
        assert!(!hit(&m, point!(x: 1.0, y: 1.0).into()));
        assert!(hit(&m, point!(x: 2.0, y: 3.0).into()));
        assert!(hit(&m, point!(x: 0.5, y: 1.0).into()));
        // Lines: in the notch, crossing into the mask, inside.
        assert!(!hit(
            &m,
            line_string![(x: 2.5, y: 2.5), (x: 3.5, y: 3.5)].into()
        ));
        assert!(hit(
            &m,
            line_string![(x: 3.0, y: 3.0), (x: 3.0, y: 1.0)].into()
        ));
        assert!(hit(
            &m,
            line_string![(x: 3.0, y: 0.5), (x: 3.5, y: 1.5)].into()
        ));
        // Polygons: in the notch, in the hole, holding the whole mask,
        // holding the mask in its own hole.
        assert!(!hit(
            &m,
            wkt!(POLYGON((2.5 2.5,3.5 2.5,3.5 3.5,2.5 2.5))).into()
        ));
        assert!(!hit(
            &m,
            wkt!(POLYGON((0.7 0.7,1.3 0.7,1.3 1.3,0.7 0.7))).into()
        ));
        assert!(hit(
            &m,
            wkt!(POLYGON((-1.0 -1.0,5.0 -1.0,5.0 5.0,-1.0 5.0,-1.0 -1.0))).into()
        ));
        assert!(!hit(
            &m,
            wkt!(POLYGON((-2.0 -2.0,6.0 -2.0,6.0 6.0,-2.0 6.0,-2.0 -2.0),(-1.0 -1.0,5.0 -1.0,5.0 5.0,-1.0 5.0,-1.0 -1.0))).into()
        ));
    }

    /// More near edges than `NEAR_EDGES_MAX`: segments query the R-tree.
    #[test]
    fn a_feature_near_many_mask_edges_uses_the_tree() {
        let ring: Vec<Coord<f64>> = (0..=400)
            .map(|i| {
                let t = f64::from(i) * std::f64::consts::TAU / 400.0;
                Coord {
                    x: t.cos(),
                    y: t.sin(),
                }
            })
            .collect();
        let m = MaskIndex::new(
            &MultiPolygon::new(vec![Polygon::new(LineString::new(ring), vec![])]),
            Crs::Epsg4326,
        );
        assert!(hit(
            &m,
            line_string![(x: -2.0, y: 0.0), (x: 2.0, y: 0.5)].into()
        ));
        assert!(!hit(
            &m,
            wkt!(POLYGON((
                -2.0 - 2.0,
                2.0 - 2.0,
                2.0 - 1.5,
                -2.0 - 1.5,
                -2.0 - 2.0
            )))
            .into()
        ));
        // A ring around the circle: no edge contact, the mask inside it.
        assert!(hit(
            &m,
            wkt!(POLYGON((-2.0 -2.0,2.0 -2.0,2.0 2.0,-2.0 2.0,-2.0 -2.0))).into()
        ));
        // The circle in the feature's hole: no hit.
        assert!(!hit(
            &m,
            wkt!(POLYGON((-3.0 -3.0,3.0 -3.0,3.0 3.0,-3.0 3.0,-3.0 -3.0),(-2.0 -2.0,2.0 -2.0,2.0 2.0,-2.0 2.0,-2.0 -2.0))).into()
        ));
    }

    #[test]
    fn a_feature_one_world_away_matches_as_bbox_does() {
        let m = l_mask();
        assert!(hit(&m, point!(x: 363.0, y: 1.0).into()));
        assert!(!hit(&m, point!(x: 363.0, y: 3.0).into()));
    }

    fn write(name: &str, text: &str) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(name);
        std::fs::write(&path, text).unwrap();
        (dir, path)
    }

    #[test]
    fn loads_geojson_and_refuses_bad_masks() {
        let (_d, ok) = write(
            "m.geojson",
            r#"{"type":"FeatureCollection","features":[{"type":"Feature","properties":{},
            "geometry":{"type":"Polygon","coordinates":[[[0,0],[1,0],[1,1],[0,0]]]}}]}"#,
        );
        let loaded = load(&MaskSource::Path(ok)).unwrap();
        assert_eq!(loaded.polygons.0.len(), 1);
        assert_eq!(loaded.crs, Some(Crs::Epsg4326));
        assert!(
            loaded.into_input_crs(Crs::Epsg3857).is_err(),
            "CRS mismatch"
        );

        let (_d, line) = write(
            "l.geojson",
            r#"{"type":"LineString","coordinates":[[0,0],[1,1]]}"#,
        );
        let err = load(&MaskSource::Path(line)).unwrap_err().to_string();
        assert!(
            err.contains("--mask") && err.contains("found a line"),
            "{err}"
        );

        let (_d, empty) = write("e.geojson", r#"{"type":"FeatureCollection","features":[]}"#);
        let err = load(&MaskSource::Path(empty)).unwrap_err().to_string();
        assert!(err.contains("--mask") && err.contains("no "), "{err}");

        let err = load(&MaskSource::Path("/nonexistent/m.geojson".into()))
            .unwrap_err()
            .to_string();
        assert!(err.contains("cannot read"), "{err}");
    }

    #[test]
    fn loads_a_geoparquet_mask_with_its_crs() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("m.parquet");
        crate::overview::testutil::write_input(
            &path,
            &[
                Some(wkt!(POLYGON((0.0 0.0,1.0 0.0,1.0 1.0,0.0 0.0))).into()),
                None,
            ],
            false,
            None,
        );
        let loaded = load(&MaskSource::Path(path)).unwrap();
        assert_eq!(loaded.polygons.0.len(), 1);
        assert_eq!(loaded.crs, Some(Crs::Epsg4326));
    }

    #[test]
    fn loads_wkb_without_a_crs() {
        let wkb =
            crate::wkb::geometry_to_wkb(&wkt!(POLYGON((0.0 0.0,1.0 0.0,1.0 1.0,0.0 0.0))).into())
                .unwrap();
        let loaded = load(&MaskSource::Wkb(wkb)).unwrap();
        assert!(loaded.into_input_crs(Crs::Epsg3857).is_ok());
    }

    #[test]
    fn mask_and_bbox_intersect_into_one_query() {
        let mask = MultiPolygon::new(vec![wkt!(POLYGON((0.0 0.0,4.0 0.0,4.0 4.0,0.0 0.0)))]);
        let q = region_query(Some(&[2.0, -1.0, 10.0, 1.0]), Some(&mask), Crs::Epsg4326).unwrap();
        assert_eq!(q.boxes()[0], [2.0, 0.0, 4.0, 1.0]);
    }
}
