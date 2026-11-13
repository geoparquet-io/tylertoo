"""Unit tests of gate.py and mutate.py on a synthetic archive: no tylertoo, no
tippecanoe. Run as test_metrics.py says."""

import gzip
import sys
from pathlib import Path

import mapbox_vector_tile
import pytest
from pmtiles.tile import Compression, TileType, zxy_to_tileid
from pmtiles.writer import write as pm_write
from shapely.geometry import Point

sys.path.insert(0, str(Path(__file__).resolve().parent))
import gate
import metrics as m
import mutate
from test_metrics import E, X, Y, Z, world

# 8 x 8 discs filling most of one tile, each with its own fid.
DISCS = [Point(256 + 512 * i, 256 + 512 * j).buffer(240) for j in range(8) for i in range(8)]
TOL = {"gate": {"default_band": [-1.0, 0.02]},
       "datasets": {"d": {"zoom_bands": [{"zooms": [Z, Z], "missing_max": [-0.01, 0.01]}]}}}


@pytest.fixture(scope="module")
def src():
    return m.Source([world(g) for g in DISCS], list(range(len(DISCS))))


@pytest.fixture(scope="module")
def archive(tmp_path_factory):
    path = tmp_path_factory.mktemp("a") / "a.pmtiles"
    feats = [{"geometry": g, "properties": {"fid": i}, "id": i + 1} for i, g in enumerate(DISCS)]
    data = mapbox_vector_tile.encode([{"name": "l", "features": feats}],
                                     default_options={"extents": E, "y_coord_down": True})
    with pm_write(str(path)) as w:
        w.write_tile(zxy_to_tileid(Z, X, Y), gzip.compress(data))
        w.finalize({"tile_type": TileType.MVT, "tile_compression": Compression.GZIP,
                    "min_zoom": Z, "max_zoom": Z, "min_lon_e7": 0, "min_lat_e7": 0,
                    "max_lon_e7": 0, "max_lat_e7": 0, "center_zoom": Z,
                    "center_lon_e7": 0, "center_lat_e7": 0}, {})
    return path


def errors(path, src):
    return gate.score(path, src, "fid", [Z], 10)[str(Z)]["tiles"][f"{X}/{Y}"]


def test_a_faithful_archive_scores_about_zero(archive, src):
    e = errors(archive, src)
    # The encoder rounds the discs' vertices to tile units, hence not exactly 0.
    assert e["missing"] < 0.02 and e["added"] < 0.02 and e["attr_mismatch"] == 0.0


@pytest.mark.parametrize("kind,metric,at_least", [
    ("drop", "missing", 0.01), ("shift", "missing", 0.01), ("simplify", "missing", 0.01),
    ("swap", "attr_mismatch", 0.9)])
def test_each_defect_raises_its_error(archive, src, tmp_path, kind, metric, at_least):
    base = errors(archive, src)
    control = tmp_path / "control.pmtiles"
    mutate.mutate(archive, control, "control", [Z], "fid")
    assert errors(control, src) == base
    copy = tmp_path / f"{kind}.pmtiles"
    mutate.mutate(archive, copy, kind, [Z], "fid")
    assert errors(copy, src)[metric] >= base[metric] + at_least


def test_an_absent_tile_is_all_missing(archive, src):
    # One zoom down the archive has no tiles: what `empty` leaves behind.
    scored = gate.score(archive, src, "fid", [Z + 1], 10)[str(Z + 1)]["tiles"]
    assert scored and all(e["missing"] == 1.0 for e in scored.values())


def test_a_mutant_that_changes_nothing_is_refused(archive, tmp_path):
    with pytest.raises(SystemExit, match="changed nothing"):
        mutate.mutate(archive, tmp_path / "x.pmtiles", "swap", [Z], "no_such_attribute")


def zoom(missing):
    return {str(Z): {"tiles": {"1/1": {"missing": missing}, "2/2": {"missing": 0.0}}}}


def test_compare_gates_the_difference_not_the_value():
    ok, = [r for r in gate.compare("d", zoom(0.30), zoom(0.295), TOL) if r["metric"] == "missing_max"]
    assert ok["ok"] is True and ok["tile"] == f"{Z}/1/1" and ok["d"] == pytest.approx(0.005)
    worse, = [r for r in gate.compare("d", zoom(0.35), zoom(0.30), TOL) if r["metric"] == "missing_max"]
    better, = [r for r in gate.compare("d", zoom(0.20), zoom(0.30), TOL) if r["metric"] == "missing_max"]
    assert worse["ok"] is False and better["ok"] is False  # the band is a ratchet: both directions
    assert gate.worse(worse) and not gate.worse(better) and not gate.worse(ok)


def test_worse_is_about_the_band_not_the_sign_of_d():
    row = {"ok": False, "d": -0.001, "band": [-0.03, -0.02]}
    assert gate.worse(row)  # still better than tippecanoe, but worse than it was
    assert not gate.worse({"ok": False, "d": 0.0, "band": [0.02, 0.03]})


def test_a_cell_without_a_band_takes_the_default():
    row, = [r for r in gate.compare("other", zoom(0.5), zoom(0.1), TOL) if r["metric"] == "missing_max"]
    assert row["band"] == [-1.0, 0.02] and row["ok"] is False
    row, = [r for r in gate.compare("other", zoom(0.1), zoom(0.5), TOL) if r["metric"] == "missing_max"]
    assert row["ok"] is True
