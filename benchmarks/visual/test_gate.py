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


def write_archive(path, geoms):
    feats = [{"geometry": g, "properties": {"fid": i}, "id": i + 1} for i, g in enumerate(geoms)]
    data = mapbox_vector_tile.encode([{"name": "l", "features": feats}],
                                     default_options={"extents": E, "y_coord_down": True})
    with pm_write(str(path)) as w:
        w.write_tile(zxy_to_tileid(Z, X, Y), gzip.compress(data))
        w.finalize({"tile_type": TileType.MVT, "tile_compression": Compression.GZIP,
                    "min_zoom": Z, "max_zoom": Z, "min_lon_e7": 0, "min_lat_e7": 0,
                    "max_lon_e7": 0, "max_lat_e7": 0, "center_zoom": Z,
                    "center_lon_e7": 0, "center_lat_e7": 0}, {})
    return path


@pytest.fixture(scope="module")
def archive(tmp_path_factory):
    return write_archive(tmp_path_factory.mktemp("a") / "a.pmtiles", DISCS)


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


def test_a_band_without_a_value_is_a_failure():
    """A metric one tool stops yielding (here: tippecanoe) must not drop out of the gate."""
    rows = gate.compare("d", zoom(0.3), {str(Z): {"tiles": {}}}, TOL)
    row, = rows
    assert row["metric"] == "missing_max" and row["ok"] is False and gate.worse(row)
    assert "no value" in gate.line(row)


def test_a_source_with_nothing_to_score_is_not_a_pass(archive, src, tmp_path, monkeypatch):
    """Every tile under its floor (a short line, a sliver): no cell, so no verdict."""
    import argparse

    monkeypatch.setattr(gate.M, "read_source", lambda path, attr: src)
    (tmp_path / "archive.pmtiles").write_bytes(archive.read_bytes())
    monkeypatch.setattr(gate, "tippecanoe_dir", lambda *a: tmp_path)
    monkeypatch.setattr(gate, "tylertoo_archive", lambda *a: archive)
    # A polygon source still yields a seam row: it is not a scored tile.
    nothing = {str(Z): {"tiles": {}, "seams": {"edge_gap": 0.0}}}
    monkeypatch.setattr(gate, "score", lambda *a: nothing)
    monkeypatch.setattr(gate, "cached_score", lambda *a: nothing)
    tol = {"gate": {"default_band": [-1.0, 0.02], "zoom_range": [Z, Z], "zooms": [Z]}, "datasets": {}}
    args = argparse.Namespace(work=tmp_path, zooms=None, cap=10)
    with pytest.raises(ValueError, match="nothing to score"):
        gate.gate_dataset(args, tol, {}, "d", {"path": "unused", "layer": "l"})
    # A banded direct cell is a verdict without the source (open-buildings z12, fieldmaps z2 and z4).
    banded = {**tol, "datasets": {"d": {"zoom_bands": [{"zooms": [Z, Z], "pair_iou_loss_mean": [0.0, 0.01]}]}}}
    rows = gate.gate_dataset(args, banded, {}, "d", {"path": "unused", "layer": "l"})
    assert any(r["metric"] == "pair_iou_loss_mean" and r["ok"] for r in rows)
    # Measuring other settings bands nothing: the direct cells are the measurement.
    args.preset = False
    assert any(r["metric"] == "pair_iou_loss_mean" for r in
               gate.gate_dataset(args, tol, {}, "d", {"path": "unused", "layer": "l"}))


def test_a_zoom_with_nothing_to_score_is_named(archive, src, tmp_path, monkeypatch, capsys):
    import argparse

    monkeypatch.setattr(gate.M, "read_source", lambda path, attr: src)
    (tmp_path / "archive.pmtiles").write_bytes(archive.read_bytes())
    monkeypatch.setattr(gate, "tippecanoe_dir", lambda *a: tmp_path)
    monkeypatch.setattr(gate, "tylertoo_archive", lambda *a: archive)
    scores = {str(Z): {"tiles": {"1/1": {"missing": 0.0}}}, "9": {"tiles": {}, "seams": {"edge_gap": 0.0}}}
    monkeypatch.setattr(gate, "score", lambda *a: scores)
    monkeypatch.setattr(gate, "cached_score", lambda *a: scores)
    tol = {"gate": {"zoom_range": [Z, 9], "zooms": [Z, 9]}, "datasets": {}}
    gate.gate_dataset(argparse.Namespace(work=tmp_path, zooms=None, cap=10), tol, {}, "d",
                      {"path": "unused", "layer": "l"})
    assert "d: not scored against the source (no sampled tile holds enough of it): z9" in capsys.readouterr().out


@pytest.mark.parametrize("preset", [True, False])
def test_the_exact_cluster_sum_is_checked_only_for_the_preset(archive, tmp_path, monkeypatch, preset):
    """--cluster's exact sum is the preset's promise; other flags are measured, not held to it."""
    import argparse

    points = m.Source([world(Point(256 + 512 * i, 256)) for i in range(8)], list(range(8)))
    monkeypatch.setattr(gate.M, "read_source", lambda path, attr: points)
    (tmp_path / "archive.pmtiles").write_bytes(archive.read_bytes())
    monkeypatch.setattr(gate, "tippecanoe_dir", lambda *a: tmp_path)
    monkeypatch.setattr(gate, "tylertoo_archive", lambda *a, **k: archive)
    scores = {str(Z): {"tiles": {f"{X}/{Y}": {"point_missing": 0.0}}}}
    monkeypatch.setattr(gate, "score", lambda *a: scores)
    monkeypatch.setattr(gate, "cached_score", lambda *a: scores)
    monkeypatch.setattr(gate, "pair_score", lambda *a: {})
    called = []
    monkeypatch.setattr(gate, "cluster_rows", lambda *a: called.append(1) or [])
    tol = {"gate": {"zoom_range": [Z, Z], "zooms": [Z]}, "datasets": {}}
    gate.gate_dataset(argparse.Namespace(work=tmp_path, zooms=None, cap=10, preset=preset), tol, {}, "d",
                      {"path": "unused", "layer": "l"})
    assert called == ([1] if preset else [])


def test_a_breach_png_is_written_once_per_run_over_an_old_one(archive, src, tmp_path, monkeypatch):
    """max, p99 and p95 share a tile: one PNG, and never the previous run's."""
    import argparse

    old = tmp_path / "png" / f"d_z{Z}_missing_{Z}-1-1.png"
    old.parent.mkdir()
    old.write_bytes(b"the previous build")
    calls = []
    real = gate.write_png
    monkeypatch.setattr(gate, "write_png", lambda *a: (calls.append(a[0]), real(*a)))
    monkeypatch.setattr(gate.M, "read_source", lambda path, attr: src)
    (tmp_path / "archive.pmtiles").write_bytes(archive.read_bytes())
    monkeypatch.setattr(gate, "tippecanoe_dir", lambda *a: tmp_path)
    monkeypatch.setattr(gate, "tylertoo_archive", lambda *a: archive)
    monkeypatch.setattr(gate, "score", lambda *a: zoom(0.9))
    monkeypatch.setattr(gate, "cached_score", lambda *a: zoom(0.0))
    tol = {"gate": {"default_band": [-1.0, 0.02], "zoom_range": [Z, Z], "zooms": [Z]}, "datasets": {}}
    args = argparse.Namespace(work=tmp_path, zooms=None, cap=10)
    rows = gate.gate_dataset(args, tol, {}, "d", {"path": "unused", "layer": "l"})
    assert len([r for r in rows if r.get("png")]) == 3 and calls == [old]
    assert old.read_bytes().startswith(b"\x89PNG")


def test_the_png_marks_what_differs(archive, src, tmp_path):
    import numpy as np
    from PIL import Image
    from shapely.geometry import LineString

    def colours(png):
        px = np.asarray(Image.open(png).convert("RGB")).reshape(-1, 3)
        return {name: int((px == rgb).all(axis=1).sum())
                for name, rgb in (("red", (220, 30, 30)), ("blue", (30, 80, 220)))}

    shifted = tmp_path / "shift.pmtiles"
    mutate.mutate(archive, shifted, "shift", [Z], "fid")
    gate.write_png(tmp_path / "p.png", src, "fid", f"{Z}/{X}/{Y}", [shifted, archive])
    seen = colours(tmp_path / "p.png")
    assert seen["red"] > 1000 and seen["blue"] > 1000  # the shift, in the first panel

    line = LineString([(100, 2000), (4000, 2000)])
    nothing = write_archive(tmp_path / "none.pmtiles", [LineString([(100, 300), (4000, 300)])])
    gate.write_png(tmp_path / "l.png", m.Source([world(line)]), None, f"{Z}/{X}/{Y}", [nothing])
    seen = colours(tmp_path / "l.png")
    assert seen["red"] > 400 and seen["blue"] > 400  # the source line missing, another drawn


def test_a_cell_without_a_band_takes_the_default():
    row, = [r for r in gate.compare("other", zoom(0.5), zoom(0.1), TOL) if r["metric"] == "missing_max"]
    assert row["band"] == [-1.0, 0.02] and row["ok"] is False
    row, = [r for r in gate.compare("other", zoom(0.1), zoom(0.5), TOL) if r["metric"] == "missing_max"]
    assert row["ok"] is True


def test_pair_rows_gate_the_direct_difference(archive, tmp_path):
    """tylertoo's tiles against tippecanoe's: identical archives differ by 0, a shifted one does not."""
    shifted = tmp_path / "shift.pmtiles"
    mutate.mutate(archive, shifted, "shift", [Z], "fid")
    tiles = {str(Z): {"tiles": {f"{X}/{Y}": {}}}}
    tol = {"gate": {}, "datasets": {"d": {"zoom_bands": [{"zooms": [Z, Z], "pair_iou_loss_max": [0.0, 0.01]}]}}}

    def rows(a, b):
        return {r["metric"]: r for r in gate.pair_rows("d", gate.pair_score(a, b, "polygon", "fid", tiles), tol)}

    same = rows(archive, archive)
    assert same["pair_iou_loss_max"]["d"] == 0.0 and same["pair_iou_loss_max"]["ok"] is True
    assert same["pair_attr_max"]["ok"] is None  # measured, no band
    row = rows(shifted, archive)["pair_iou_loss_max"]
    assert row["d"] > 0.01 and row["ok"] is False and gate.worse(row) and row["tile"] == f"{Z}/{X}/{Y}"
    assert "tylertoo vs tippecanoe" in gate.line(row)
    # A pair band is not a "band without a value" of the comparison against the source.
    assert gate.compare("d", tiles, tiles, tol) == []


def test_distance_is_the_mean_tile_difference_per_dataset(archive, tmp_path):
    shifted = tmp_path / "shift.pmtiles"
    mutate.mutate(archive, shifted, "shift", [Z], "fid")
    tiles = {str(Z): {"tiles": {f"{X}/{Y}": {}}}}
    tol = {"gate": {}, "datasets": {}}
    same = gate.pair_rows("d", gate.pair_score(archive, archive, "polygon", "fid", tiles), tol)
    moved = gate.pair_rows("d", gate.pair_score(shifted, archive, "polygon", "fid", tiles), tol)
    assert gate.distance(same) == {"d": {"pair_attr": 0.0, "pair_iou_loss": 0.0,
                                         "bytes_ratio": 1.0, "features_ratio": 1.0}}
    far = gate.distance(moved)["d"]
    assert far["pair_iou_loss"] > 0.01 and far["pair_attr"] == 0.0  # moved, still the same features


def test_other_settings_are_measured_not_gated(monkeypatch, tmp_path):
    """With --tylertoo-flags the bands, measured for the preset, do not apply."""
    seen = {}
    monkeypatch.setattr(gate.run_e2e, "resolve_tippecanoe", lambda p: {"binary": "t", "version": "v"})
    monkeypatch.setattr(gate, "gate", lambda args, tol, tip, datasets: seen.update(args=args, tol=tol) or 0)
    monkeypatch.setattr(sys, "argv", ["gate.py", "--work", str(tmp_path), "--tylertoo", sys.executable,
                                      "--tylertoo-flags=--simplify-factor 2"])
    assert gate.main() == 0
    assert seen["args"].tylertoo_flags == ["--simplify-factor", "2"]
    assert "default_band" not in seen["tol"]["gate"]
    assert all("zoom_bands" not in d for d in seen["tol"]["datasets"].values())


def test_feature_and_byte_ratios_per_zoom(archive, tmp_path):
    """How much each tool keeps: tylertoo's features and tile bytes over tippecanoe's, 1 when equal."""
    fewer = write_archive(tmp_path / "fewer.pmtiles", DISCS[:1])
    tiles = {str(Z): {"tiles": {f"{X}/{Y}": {}}}}
    tol = {"gate": {}, "datasets": {}}

    def ratios(a, b):
        return {r["metric"]: r for r in gate.pair_rows("d", gate.pair_score(a, b, "polygon", None, tiles), tol)
                if r["metric"].endswith("_ratio")}

    same = ratios(archive, archive)
    assert same["features_ratio"]["d"] == 1.0 and same["bytes_ratio"]["d"] == 1.0
    kept = ratios(fewer, archive)["features_ratio"]
    assert (kept["tylertoo"], kept["tippecanoe"], kept["d"]) == (1, len(DISCS), 1 / len(DISCS))
    assert ratios(fewer, archive)["bytes_ratio"]["d"] < 1
    assert "ratio" in gate.line(kept)
    banded = {"gate": {}, "datasets": {"d": {"zoom_bands": [{"zooms": [Z, Z], "features_ratio": [0.9, 1.1]}]}}}
    assert gate.compare("d", tiles, tiles, banded) == []  # not a band of the comparison against the source
    # Reported, never gated: neither keeping more nor fewer is better, and gzip bytes vary by machine.
    pair = gate.pair_score(fewer, archive, "polygon", None, tiles)
    assert all(r["band"] is None and r["ok"] is None
               for r in gate.pair_rows("d", pair, banded) if r["metric"].endswith("_ratio"))
    rows = gate.pair_rows("d", gate.pair_score(fewer, archive, "polygon", None, tiles), tol)
    assert gate.distance(rows)["d"]["features_ratio"] == 1 / len(DISCS)


@pytest.mark.parametrize("cap,sampled", [("0", sys.maxsize), ("250", 250)])
def test_another_sample_size_is_measured_not_gated(monkeypatch, tmp_path, cap, sampled):
    """--cap sets the tiles sampled per zoom (0: all of them); the bands were measured at the default."""
    seen = {}
    monkeypatch.setattr(gate.run_e2e, "resolve_tippecanoe", lambda p: {"binary": "t", "version": "v"})
    monkeypatch.setattr(gate, "gate", lambda args, tol, tip, datasets: seen.update(args=args, tol=tol) or 0)
    monkeypatch.setattr(sys, "argv", ["gate.py", "--work", str(tmp_path), "--tylertoo", sys.executable, "--cap", cap])
    assert gate.main() == 0
    assert seen["args"].cap == sampled
    assert all("zoom_bands" not in d for d in seen["tol"]["datasets"].values())
    # The self-test's bands are the preset's at the default sample: no other run of it means anything.
    monkeypatch.setattr(sys, "argv", sys.argv + ["--self-test"])
    assert gate.main() == 2


def test_the_mean_counts_a_tile_by_what_is_drawn_in_it():
    """A sliver tile of ten pixels that differs wholly must not outweigh a full tile that agrees."""
    pair = {str(Z): {"tiles": {"1/1": {"pair_iou_loss": 0.0}, "2/2": {"pair_iou_loss": 1.0}},
                     "weights": {"1/1": {"pair_iou_loss": 990}, "2/2": {"pair_iou_loss": 10}}}}
    rows = {r["metric"]: r for r in gate.pair_rows("d", pair, {"gate": {}, "datasets": {}})}
    assert rows["pair_iou_loss_mean"]["pair"] == pytest.approx(0.01)
    # ... and is not a worst tile: one pixel of difference would make it one.
    assert rows["pair_iou_loss_max"]["pair"] == 0.0 and rows["pair_iou_loss_max"]["tile"] == f"{Z}/1/1"
