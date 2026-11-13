"""Unit tests of metrics.py on synthetic sources and hand-built decoded tiles.

    uv run --no-project --python 3.12 --with pytest==8.4.2 \
        --with hypothesis==6.170.14 --with numpy==2.5.4 \
        --with rasterio==1.5.2 --with shapely==2.1.1 --with pyarrow==21.0.0 \
        --with pmtiles==3.8.1 --with mapbox-vector-tile==2.2.0 --with pillow==13.0.0 \
        pytest -q benchmarks/visual
"""

import math
import sys
from pathlib import Path

import numpy as np
import pytest
from shapely.affinity import translate
from shapely.geometry import LineString, MultiPoint, Point, box, mapping

sys.path.insert(0, str(Path(__file__).resolve().parent))
import metrics as m

Z, X, Y = 3, 4, 2
E = m.EXTENT_DEFAULT


def world(geom):
    """A geometry in tile units (y down) of tile Z/X/Y, in EPSG:3857."""
    from shapely.affinity import affine_transform

    x0, _, _, ytop = m.tile_bounds_3857(Z, X, Y)
    s = m.WORLD / (1 << Z) / E
    return affine_transform(geom, [s, 0, 0, -s, x0, ytop])


def source(tile_geoms, values=None):
    return m.Source([world(g) for g in tile_geoms], values)


def tile(geoms, props=None):
    feats = [{"geometry": mapping(g), "properties": (props[i] if props else {}), "id": i + 1}
             for i, g in enumerate(geoms)]
    return {"layer": {"extent": E, "features": feats}}


LEFT, RIGHT = box(0, 0, 2048, 4096), box(2048, 0, 4096, 4096)


def test_polygon_identical_is_zero():
    src = source([LEFT, RIGHT], [np.int64(7), np.int64(8)])
    e = m.polygon_errors(tile([LEFT, RIGHT], [{"a": 7}, {"a": 8}]), src, "a", Z, X, Y)
    assert (e["missing"], e["added"], e["attr_mismatch"]) == (0.0, 0.0, 0.0)


def test_polygon_missing_and_added():
    src = source([LEFT])
    assert m.polygon_errors(tile([]), src, None, Z, X, Y)["missing"] == 1.0
    e = m.polygon_errors(tile([LEFT, RIGHT]), src, None, Z, X, Y)
    assert (e["missing"], e["added"]) == (0.0, 1.0)


def test_polygon_two_pixel_shift_is_seen():
    src = source([box(1024, 1024, 3072, 3072)])
    shifted = translate(box(1024, 1024, 3072, 3072), xoff=2 * E / m.PX)
    e = m.polygon_errors(tile([shifted]), src, None, Z, X, Y)
    assert e["missing"] == pytest.approx(2 / 128) and e["added"] == pytest.approx(2 / 128)


def test_attribute_values_compare_across_types():
    src = source([LEFT, RIGHT], [np.int64(7), "x"])
    same = m.polygon_errors(tile([LEFT, RIGHT], [{"a": 7}, {"a": "x"}]), src, "a", Z, X, Y)
    swapped = m.polygon_errors(tile([LEFT, RIGHT], [{"a": "x"}, {"a": 7}]), src, "a", Z, X, Y)
    assert same["attr_mismatch"] == 0.0 and swapped["attr_mismatch"] == 1.0


def test_null_attribute_matches_an_absent_property():
    src = source([LEFT, RIGHT], [None, 8])
    e = m.polygon_errors(tile([LEFT, RIGHT], [{}, {"a": 8}]), src, "a", Z, X, Y)
    assert e["attr_mismatch"] == 0.0


def test_a_mixed_source_is_refused():
    with pytest.raises(ValueError, match="one geometry kind"):
        source([LEFT, Point(5, 5)])


def test_sliver_tile_is_not_gated():
    assert m.polygon_errors(tile([]), source([box(0, 0, 40, 40)]), None, Z, X, Y) is None


def test_line_errors():
    line = LineString([(100, 2000), (4000, 2000)])
    src = source([line])
    assert m.line_errors(tile([line]), src, None, Z, X, Y) == {"src_uncovered": 0.0, "drawn_stray": 0.0}
    assert m.line_errors(tile([]), src, None, Z, X, Y) == {"src_uncovered": 1.0, "drawn_stray": 0.0}
    far = m.line_errors(tile([translate(line, yoff=3 * E / m.PX)]), src, None, Z, X, Y)
    assert far["src_uncovered"] == pytest.approx(1.0) and far["drawn_stray"] == pytest.approx(1.0)
    near = m.line_errors(tile([translate(line, yoff=0.5 * E / m.PX)]), src, None, Z, X, Y)
    assert near == {"src_uncovered": 0.0, "drawn_stray": 0.0}


def test_point_errors_ignore_buffer_copies():
    pts = [Point(100 + 37 * i, 200 + 91 * (i % 40)) for i in range(100)]
    src = source(pts)
    assert m.point_errors(tile(pts), src, None, Z, X, Y) == {"count_error": 0.0}
    assert m.point_errors(tile(pts[:50]), src, None, Z, X, Y) == {"count_error": 0.5}
    outside = [Point(-20, 100), Point(E, 100), Point(100, E + 5)]
    assert m.point_errors(tile(pts + outside), src, None, Z, X, Y) == {"count_error": 0.0}
    assert m.point_errors(tile([MultiPoint(pts)]), src, None, Z, X, Y) == {"count_error": 0.0}


def test_point_rounding_is_not_an_error():
    # Just left of a cell edge, and one just outside the tile's left edge.
    pts = [Point(255.6 + 256 * i, 100.4) for i in range(10)] + [Point(-0.3, 100.4)]
    src = source(pts)
    for snap in (round, math.floor):
        drawn = [Point(snap(p.x), snap(p.y)) for p in pts]
        assert m.point_errors(tile(drawn), src, None, Z, X, Y) == {"count_error": 0.0}


def test_cluster_sum_counts_each_point_once():
    feats = [{"geometry": mapping(Point(10, 10)), "properties": {"point_count": 5}},
             {"geometry": mapping(Point(20, 20)), "properties": {}},
             {"geometry": mapping(Point(-3, 20)), "properties": {"point_count": 9}}]
    assert m.cluster_sum([{"l": {"extent": E, "features": feats}}]) == 6


def test_shares_and_seam_gap():
    ref = np.zeros((4, 4), bool)
    ref[:, :2] = True
    fill = np.zeros((4, 4), bool)
    fill[:, 1:3] = True
    assert m.shares(fill, ref) == (0.5, 0.5)
    assert m.shares(fill, np.zeros((4, 4), bool)) == (None, None)

    a, b = (3, 0, 0), (3, 1, 0)
    full = np.ones((8, 8), bool)
    colour = {a: np.ones((8, 8), np.int32), b: np.ones((8, 8), np.int32)}
    assert m.seams([a], colour, {a: full, b: full}, 8)["edge_gap"] == 0.0
    colour[a][:, -2:] = 0  # nothing drawn in a's last two columns
    s = m.seams([a], colour, {a: full, b: full}, 8)
    assert s["edge_gap"] == 0.5


def test_sample_depends_on_the_source_only():
    src = source([box(10 * i, 10 * i, 10 * i + 5, 10 * i + 5) for i in range(1, 50)] + [box(5, 5, 2048, 4000)])
    one, two = m.sample_tiles(src, 9, 5), m.sample_tiles(src, 9, 5)
    assert one == two and len(one) == 5 and all(k[0] == 9 for k in one)
    assert m.sample_tiles(src, Z, 5) == [(Z, X, Y)]


def test_read_source_without_geo_metadata(tmp_path):
    import pyarrow as pa
    import pyarrow.parquet as pq
    import shapely

    geoms = [box(0, 0, 1, 1), Point(2, 2)]
    pq.write_table(pa.table({"geometry": [shapely.to_wkb(g) for g in geoms], "fid": [1, 2]}),
                   tmp_path / "s.parquet")
    src = m.read_source(tmp_path / "s.parquet", "fid", ("Polygon", "MultiPolygon"))
    assert len(src.geoms) == 1 and src.values == [1] and src.kind == "polygon"
    assert src.geoms[0].bounds[2] == pytest.approx(111319.49, abs=0.01)


@pytest.mark.parametrize("crs,loads", [
    ({"id": {"authority": "OGC", "code": "CRS84"}}, True),
    ({"id": {"authority": "EPSG", "code": 4326}}, True),
    ({"id": {"authority": "EPSG", "code": "4326"}}, True),
    ({"id": {"authority": "EPSG", "code": 3857}}, False),
    ({"type": "ProjectedCRS", "name": "no id"}, False),
    ("EPSG:3857", False)])
def test_read_source_refuses_a_crs_it_does_not_know(tmp_path, crs, loads):
    import json

    import pyarrow as pa
    import pyarrow.parquet as pq
    import shapely

    geo = {"primary_column": "geometry", "columns": {"geometry": {"encoding": "WKB", "crs": crs}}}
    table = pa.table({"geometry": [shapely.to_wkb(Point(1, 1))]}).replace_schema_metadata(
        {"geo": json.dumps(geo)})
    pq.write_table(table, tmp_path / "s.parquet")
    if loads:
        assert m.read_source(tmp_path / "s.parquet").kind == "point"
    else:
        with pytest.raises(ValueError, match="lon/lat"):
            m.read_source(tmp_path / "s.parquet")


# ---- one tool's tile against the other's, no source ----


def test_pair_of_identical_tiles_is_zero():
    props = [{"a": 7}, {"a": 8}]
    assert m.pair_errors("polygon", tile([LEFT, RIGHT], props), tile([LEFT, RIGHT], props), "a") == {
        "pair_iou_loss": 0.0, "pair_attr": 0.0}
    line = LineString([(100, 2000), (4000, 2000)])
    assert m.pair_errors("line", tile([line]), tile([line]), None) == {"pair_apart": 0.0}
    pts = [Point(10 * i, 20 * i) for i in range(1, 9)]
    assert m.pair_errors("point", tile(pts), tile(pts), None) == {"pair_count": 0.0}


def test_pair_polygon_cover_and_attribute():
    # One draws both halves, the other the left half only: half the drawn area differs.
    e = m.pair_errors("polygon", tile([LEFT, RIGHT]), tile([LEFT]), None)
    assert e == {"pair_iou_loss": 0.5}
    # Same shapes, the right half carries another feature's value.
    e = m.pair_errors("polygon", tile([LEFT, RIGHT], [{"a": 7}, {"a": 8}]),
                      tile([LEFT, RIGHT], [{"a": 7}, {"a": 9}]), "a")
    assert e == {"pair_iou_loss": 0.0, "pair_attr": 0.5}


def test_pair_line_within_a_pixel_agrees_and_beyond_does_not():
    line = LineString([(100, 2000), (4000, 2000)])
    px = E / m.PX
    assert m.pair_errors("line", tile([line]), tile([translate(line, yoff=0.5 * px)]), None)["pair_apart"] == 0.0
    assert m.pair_errors("line", tile([line]), tile([translate(line, yoff=4 * px)]), None)["pair_apart"] == 1.0
    assert m.pair_errors("line", tile([line]), tile([]), None)["pair_apart"] == 1.0


def test_pair_point_counts_per_cell():
    pts = [Point(10 * i, 20 * i) for i in range(1, 9)]
    assert m.pair_errors("point", tile(pts), tile(pts[:6]), None) == {"pair_count": 2 / 14}
    far = [Point(4000 - 10 * i, 20 * i) for i in range(1, 9)]
    assert m.pair_errors("point", tile(pts), tile(far), None) == {"pair_count": 1.0}
    assert m.pair_errors("point", tile([]), tile([]), None) is None
