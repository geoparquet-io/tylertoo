"""
Tests for shared_borders.py's scorer (#802), on two squares that share a border.

    uv run --no-project --python 3.12 --with pytest==8.4.2 --with shapely==2.1.1 \\
        --with numpy==2.5.4 pytest -q benchmarks/coalesce/test_shared_borders.py
"""

from __future__ import annotations

import sys
from pathlib import Path

import pytest

shapely = pytest.importorskip("shapely")
HERE = Path(__file__).resolve().parent
sys.path[:0] = [str(HERE), str(HERE.parent / "visual")]

from metrics import Source, tile_bounds_3857
from shared_borders import score_tile

Z, X, Y = 10, 512, 511


def mercator_square(u0, v0, u1, v1):
    """A square given in tile units (y down), in EPSG:3857."""
    x0, _, x1, ytop = tile_bounds_3857(Z, X, Y)
    s = (x1 - x0) / 4096
    return shapely.box(x0 + u0 * s, ytop - v1 * s, x0 + u1 * s, ytop - v0 * s)


def decoded(*rects):
    feats = [{"geometry": {"type": "Polygon", "coordinates": [list(shapely.box(*r).exterior.coords)]},
              "properties": {}, "id": i} for i, r in enumerate(rects)]
    return {"l": {"extent": 4096, "features": feats}}


SRC = Source([mercator_square(1000, 1000, 2000, 2000), mercator_square(2000, 1000, 3000, 2000)])


def test_exact_coverage_scores_clean():
    r = score_tile(SRC, decoded((1000, 1000, 2000, 2000), (2000, 1000, 3000, 2000)), Z, X, Y, 16)
    assert r["drawn"] == pytest.approx(1)
    assert r["missing"] == pytest.approx(0, abs=1e-9)
    assert r["overlap"] == pytest.approx(0, abs=1e-9)


def test_gap_on_the_shared_border_is_inner():
    # The right square starts 2 units late: a 2 x 1000 sliver on the shared border.
    r = score_tile(SRC, decoded((1000, 1000, 2000, 2000), (2002, 1000, 3000, 2000)), Z, X, Y, 16)
    assert r["missing"] == pytest.approx(2000 / 2e6, rel=1e-6)
    # Only the sliver's ends lie within the band of the outline.
    assert r["missing_inner"] == pytest.approx((1000 - 32) * 2 / 2e6, rel=1e-6)


def test_overlap_counts_once():
    r = score_tile(SRC, decoded((1000, 1000, 2003, 2000), (2000, 1000, 3000, 2000)), Z, X, Y, 16)
    assert r["overlap"] == pytest.approx(3000 / 2e6, rel=1e-6)
    assert r["overlap_new"] == pytest.approx(3000 / 2e6, rel=1e-6)
    assert r["missing"] == pytest.approx(0, abs=1e-9)


def test_overlap_the_source_has_is_not_new():
    square = mercator_square(1000, 1000, 2000, 2000)
    r = score_tile(Source([square, square]), decoded((1000, 1000, 2000, 2000), (1000, 1000, 2000, 2000)),
                   Z, X, Y, 16)
    assert r["overlap"] == pytest.approx(1)
    assert r["overlap_new"] == pytest.approx(0, abs=1e-9)
