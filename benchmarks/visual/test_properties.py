"""Property tests (Hypothesis) of the metrics and the band rule: each error
against an answer computed another way. Run as test_metrics.py says."""

import gzip
import math
import sys
from pathlib import Path

import mapbox_vector_tile
from hypothesis import assume, given, settings
from hypothesis import strategies as st
from pmtiles.tile import Compression, TileType, zxy_to_tileid
from pmtiles.writer import write as pm_write
from shapely import unary_union
from shapely.affinity import translate
from shapely.geometry import LineString, Point, box

sys.path.insert(0, str(Path(__file__).resolve().parent))
import gate
import metrics as m
import mutate
from test_metrics import E, X, Y, Z, source, tile

PIXEL = E // m.PX  # tile units per raster pixel
settings.register_profile("visual", deadline=None, max_examples=60)
settings.load_profile("visual")


@st.composite
def rects(draw, min_size=1, max_size=6):
    """Rectangles on the pixel grid, where rasterizing is exact."""
    out = []
    for _ in range(draw(st.integers(min_size, max_size))):
        x0, y0 = draw(st.integers(0, m.PX - 2)), draw(st.integers(0, m.PX - 2))
        x1, y1 = draw(st.integers(x0 + 1, m.PX)), draw(st.integers(y0 + 1, m.PX))
        out.append(box(x0 * PIXEL, y0 * PIXEL, x1 * PIXEL, y1 * PIXEL))
    return out


@given(rects(), rects(min_size=0))
def test_polygon_shares_equal_the_exact_areas(src_rects, drawn_rects):
    s, d = unary_union(src_rects), unary_union(drawn_rects)
    assume(s.area >= m.COVER_FLOOR * E * E)
    e = m.polygon_errors(tile(drawn_rects), source(src_rects), None, Z, X, Y)
    assert math.isclose(e["missing"], s.difference(d).area / s.area, abs_tol=1e-9)
    assert math.isclose(e["added"], d.difference(s).area / s.area, abs_tol=1e-9)


@given(rects())
def test_a_tile_that_draws_the_source_has_no_polygon_error(src_rects):
    assume(unary_union(src_rects).area >= m.COVER_FLOOR * E * E)
    values = list(range(len(src_rects)))
    e = m.polygon_errors(tile(src_rects, [{"a": v} for v in values]), source(src_rects, values), "a", Z, X, Y)
    assert e == {"missing": 0.0, "added": 0.0, "attr_mismatch": 0.0}


coord = st.floats(0.5, E - 1.5, allow_nan=False)


@given(st.lists(st.tuples(coord, coord), min_size=m.MIN_POINTS, max_size=80, unique=True), st.data())
def test_point_error_is_the_dropped_share(xy, data):
    pts = [Point(x, y) for x, y in xy]
    src = source(pts)
    for snap in (round, math.floor):  # a tile holds integers, either way
        drawn = [Point(snap(x), snap(y)) for x, y in xy]
        assert m.point_errors(tile(drawn), src, None, Z, X, Y) == {"count_error": 0.0}
    keep = data.draw(st.integers(0, len(pts)))
    drawn = [Point(math.floor(x), math.floor(y)) for x, y in xy[:keep]]
    e = m.point_errors(tile(drawn), src, None, Z, X, Y)
    assert math.isclose(e["count_error"], (len(pts) - keep) / len(pts), abs_tol=1e-9)


@given(st.integers(200, E - 200), st.integers(100, 1500), st.integers(2600, E - 100),
       st.floats(0.0, 6.0).filter(lambda t: abs(t - m.LINE_TOL_PX) > 0.05))
def test_a_line_within_a_pixel_is_covered_and_beyond_it_is_not(y, x0, x1, offset_px):
    line = LineString([(x0, y), (x1, y)])
    e = m.line_errors(tile([translate(line, yoff=offset_px * PIXEL)]), source([line]), None, Z, X, Y)
    expected = 0.0 if offset_px < m.LINE_TOL_PX else 1.0
    assert math.isclose(e["src_uncovered"], expected, abs_tol=1e-6)
    assert math.isclose(e["drawn_stray"], expected, abs_tol=1e-6)


share = st.floats(0, 1, allow_nan=False)


@given(share, share, share, share)
def test_a_cell_passes_exactly_when_d_is_in_its_band(ty, tip, a, b):
    lo, hi = sorted((a - 0.5, b - 0.5))
    tol = {"gate": {}, "datasets": {"d": {"zoom_bands": [{"zooms": [Z, Z], "missing_max": [lo, hi]}]}}}

    def zoom(v):
        return {str(Z): {"tiles": {"1/1": {"missing": v}}}}

    row, = [r for r in gate.compare("d", zoom(ty), zoom(tip), tol) if r["metric"] == "missing_max"]
    assert row["d"] == ty - tip
    assert row["ok"] == (lo <= ty - tip <= hi)
    assert gate.worse(row) == (ty - tip > hi)


@given(rects(min_size=2))
def test_the_unchanged_copy_scores_as_the_original(tmp_path_factory, src_rects):
    assume(unary_union(src_rects).area >= m.COVER_FLOOR * E * E)
    d = tmp_path_factory.mktemp("p")
    feats = [{"geometry": g, "properties": {"a": i}, "id": i + 1} for i, g in enumerate(src_rects)]
    data = mapbox_vector_tile.encode([{"name": "l", "features": feats}],
                                     default_options={"extents": E, "y_coord_down": True})
    with pm_write(str(d / "a.pmtiles")) as w:
        w.write_tile(zxy_to_tileid(Z, X, Y), gzip.compress(data))
        w.finalize({"tile_type": TileType.MVT, "tile_compression": Compression.GZIP,
                    "min_zoom": Z, "max_zoom": Z, "min_lon_e7": 0, "min_lat_e7": 0,
                    "max_lon_e7": 0, "max_lat_e7": 0, "center_zoom": Z,
                    "center_lon_e7": 0, "center_lat_e7": 0}, {})
    mutate.mutate(d / "a.pmtiles", d / "control.pmtiles", "control", [Z], "a")
    src = source(src_rects, list(range(len(src_rects))))
    scores = [gate.score(d / name, src, "a", [Z], 10) for name in ("a.pmtiles", "control.pmtiles")]
    assert scores[0] == scores[1] and scores[0][str(Z)]["tiles"]
