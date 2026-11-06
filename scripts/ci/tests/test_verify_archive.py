"""Source-feature conservation in the independent archive verifier."""

import sys
from pathlib import Path

import pytest

sys.path.insert(0, str(Path(__file__).resolve().parents[2]))

from verify_archive import ZoomScan, evaluate


@pytest.mark.parametrize(
    ("written", "dropped", "ok"),
    [(1, 100, False), (1, 99, True), (1, 98, False), (101, 0, False)],
)
def test_feature_accounting_requires_exact_partition(written, dropped, ok):
    """Reject unexplained losses and input overcounts."""
    outcomes = evaluate(
        {10: ZoomScan(ids=set(range(written)))},
        {"vector_layers": [{"id": "test", "fields": {}}]},
        "test",
        {"export": {"source_losses": [{"id": i, "zoom": 10, "cause": "valve_dropped"} for i in range(written, written + dropped)]}},
        (500_000, 500_000),
        True,
        False,
        (100, []),
    )
    accounting = next(o for o in outcomes if o.invariant == "feature_accounting")
    assert accounting.ok is ok, accounting.detail

@pytest.mark.parametrize("losses,ok", [
    ([{"id": 1, "zoom": 10, "cause": "encode_dropped"}], True),
    ([{"id": 0, "zoom": 10, "cause": "encode_dropped"}], False),
    ([{"id": 1, "zoom": 9, "cause": "encode_dropped"}], False),
    ([{"id": 2, "zoom": 10, "cause": "encode_dropped"}], False),
    ([{"id": 1, "zoom": 10, "cause": "encode_dropped"}] * 2, False),
])
def test_feature_accounting_uses_disjoint_source_losses(losses, ok):
    outcomes = evaluate(
        {10: ZoomScan(ids={0})}, {"vector_layers": [{"id": "test", "fields": {}}]}, "test",
        {"convert": {"source_losses": []}, "export": {"source_losses": losses}},
        (500_000, 500_000), True, False, (2, []),
    )
    accounting = next(o for o in outcomes if o.invariant == "feature_accounting")
    assert accounting.ok is ok, accounting.detail

@pytest.mark.parametrize("scanned", [None, [[1, 3]]])
def test_accounting_maps_original_rows_to_nonsequential_feature_ids(scanned):
    convert = {"input_rows": 3 if scanned is None else 2,
               "source_losses": [{"id": 1, "zoom": None, "cause": "missing_geometry"}]}
    if scanned is not None:
        convert["source_row_ranges"] = scanned
    outcomes = evaluate(
        {10: ZoomScan(ids={100, 300} if scanned is None else {300})},
        {"vector_layers": [{"id": "test", "fields": {}}]}, "test",
        {"convert": convert, "export": {"source_losses": []}},
        (500_000, 500_000), True, False, (3, [], [100, 200, 300]),
    )
    accounting = next(o for o in outcomes if o.invariant == "feature_accounting")
    assert accounting.ok, accounting.detail


def test_accounting_uses_export_feature_id_instead_of_band_row():
    outcomes = evaluate(
        {10: ZoomScan(ids={100})},
        {"vector_layers": [{"id": "test", "fields": {}}]}, "test",
        {"export": {"source_losses": [{"id": 0, "feature_id": 200, "zoom": 10,
                                          "cause": "quantized"}]}},
        (500_000, 500_000), True, False, (2, [], [100, 200]),
    )
    accounting = next(o for o in outcomes if o.invariant == "feature_accounting")
    assert accounting.ok, accounting.detail


# ---- variable-depth pyramids (#803) ----

import gzip
import json

import mapbox_vector_tile
from pmtiles.tile import Compression, TileType, zxy_to_tileid
from pmtiles.writer import write as pm_write

import verify_archive

M = 3  # canonical (finest) zoom of the synthetic pyramids
NOTE = {"tylertoo:variable_depth": {"enabled": True, "canonical_maxzoom": M}}


def _tile(fid_xs, extent):
    """One MVT tile; each feature is a point whose id is its source id."""
    feats = [
        {"geometry": f"POINT({10 + i} {10 + i})", "properties": {}, "id": fid}
        for i, fid in enumerate(fid_xs)
    ]
    return mapbox_vector_tile.encode(
        [{"name": "t", "features": feats}],
        default_options={"extents": extent, "y_coord_down": True},
    )


def _archive(path, tiles, note=True):
    """tiles: {(z, x, y): (feature ids, extent)}. Header max = deepest tile."""
    meta = {"vector_layers": [{"id": "t", "fields": {}}], **(NOTE if note else {})}
    with pm_write(str(path)) as w:
        for (z, x, y), (ids, extent) in sorted(tiles.items(), key=lambda kv: zxy_to_tileid(*kv[0])):
            w.write_tile(zxy_to_tileid(z, x, y), gzip.compress(_tile(ids, extent)))
        zs = [z for z, _, _ in tiles]
        w.finalize(
            {
                "tile_type": TileType.MVT,
                "tile_compression": Compression.GZIP,
                "min_zoom": min(zs),
                "max_zoom": max(zs),
                "min_lon_e7": -1_800_000_000, "min_lat_e7": -850_000_000,
                "max_lon_e7": 1_800_000_000, "max_lat_e7": 850_000_000,
                "center_zoom": min(zs), "center_lon_e7": 0, "center_lat_e7": 0,
            },
            meta,
        )


def _verify(path, tmp_path, n_in=6):
    report = tmp_path / "r.json"
    report.write_text(json.dumps({"convert": {"input_rows": n_in, "source_losses": []}, "export": {}}))
    args = verify_archive.argparse.Namespace(
        layer="t", max_tile_size=0, max_compressed_tile_size=0, report=report, input=None,
        input_features=None, ids=True, feature_id=None, expect_decay=False, exemptions=None,
        fixture=None, settings=None, workers=1,
    )
    outcomes, _ = verify_archive.verify_strict(path, args)
    return {o.invariant: o for o in outcomes if o.zoom is None}, outcomes


# z1 (0,0) is stopped: holds ids 0-3 at extent 4096<<2. z1 (1,0) continues to
# two z3 tiles via z2 (2,0); ids 4,5 live there.
def _mixed():
    return {
        (1, 0, 0): ([0, 1, 2, 3], 4096 << (M - 1)),
        (1, 1, 0): ([4, 5], 4096),
        (2, 2, 0): ([4, 5], 4096),
        (3, 4, 0): ([4], 4096),
        (3, 5, 0): ([5], 4096),
    }


def test_variable_depth_mixed_archive_passes(tmp_path):
    p = tmp_path / "a.pmtiles"
    _archive(p, _mixed())
    by, outcomes = _verify(p, tmp_path)
    assert all(o.ok for o in outcomes), [o for o in outcomes if not o.ok]
    assert by["variable_depth"].ok and by["feature_accounting"].ok


def test_variable_depth_stopped_tile_with_child_fails(tmp_path):
    t = _mixed()
    t[(2, 0, 0)] = ([0, 1, 2, 3], 4096)
    p = tmp_path / "a.pmtiles"
    _archive(p, t)
    by, _ = _verify(p, tmp_path)
    assert not by["variable_depth"].ok and "2/0/0" in by["variable_depth"].detail


def test_variable_depth_stopped_tile_dropping_canonical_feature_fails(tmp_path):
    t = _mixed()
    t[(1, 0, 0)] = ([0, 1, 2], 4096 << (M - 1))
    p = tmp_path / "a.pmtiles"
    _archive(p, t)
    by, _ = _verify(p, tmp_path)
    assert not by["feature_accounting"].ok


def test_variable_depth_wrong_extent_fails(tmp_path):
    t = _mixed()
    t[(1, 0, 0)] = ([0, 1, 2, 3], 4096 << M)
    p = tmp_path / "a.pmtiles"
    _archive(p, t)
    by, _ = _verify(p, tmp_path)
    assert not by["variable_depth"].ok


def test_without_note_stopped_tiles_are_not_special(tmp_path):
    """No note: the mixed archive still fails the full-pyramid invariants."""
    p = tmp_path / "a.pmtiles"
    _archive(p, _mixed(), note=False)
    by, outcomes = _verify(p, tmp_path)
    assert "variable_depth" not in by
    assert not by["feature_accounting"].ok
