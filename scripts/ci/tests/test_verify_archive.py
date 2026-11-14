"""Source-feature conservation in the independent archive verifier."""

import sys
from pathlib import Path

import pytest

sys.path.insert(0, str(Path(__file__).resolve().parents[2]))

from verify_archive import ZoomScan, evaluate, is_placeholder_square


def _decay(scans, ids=True):
    outcomes = evaluate(
        scans, {"vector_layers": [{"id": "test", "fields": {}}]}, "test",
        None, (0, 0), ids, True, None,
    )
    return next(o for o in outcomes if o.invariant == "decay")


def test_decay_tracks_a_cohort_instead_of_newly_collapsed_polygons():
    # Moldova's aggregate counts fail (76 -> 45), but these are NEW squares.
    fine = set(range(76))
    coarse = set(range(100, 145))
    result = _decay({8: ZoomScan(ids=fine, square_ids=fine),
                     5: ZoomScan(ids=coarse, square_ids=coarse)})
    assert result.ok, result.detail
    assert "z8 76 -> z5 0" in result.detail


def test_decay_fails_when_no_cohort_is_eligible():
    cohort = set(range(31))
    result = _decay({8: ZoomScan(ids=cohort, square_ids=cohort),
                     5: ZoomScan(ids=set())})
    assert not result.ok
    assert result.detail == (
        "square cohort survivors: no cohort was eligible; requires at least 32 tiny-square ids "
        "at a zoom with a level three zooms coarser; EPSG:3857 source at GSD base 1024 "
        "and extent 4096"
    )


def test_expect_decay_help_names_source_crs_and_no_cohort_failure(monkeypatch, capsys):
    monkeypatch.setattr(sys, "argv", ["verify_archive.py", "--help"])
    with pytest.raises(SystemExit) as exc:
        verify_archive.main()
    assert exc.value.code == 0
    help_text = " ".join(capsys.readouterr().out.split())
    assert "requires --ids, EPSG:3857 source, GSD base 1024, extent 4096" in help_text
    assert "fails if no cohort is eligible" in help_text


@pytest.mark.parametrize("coarse_is_square", [True, False])
def test_decay_rejects_stuck_cohort_regardless_of_later_shape(coarse_is_square):
    # The #749 defect: a square keeps the same hash against p=1/4 forever.
    cohort = set(range(64))
    result = _decay({8: ZoomScan(ids=cohort, square_ids=cohort),
                     5: ZoomScan(ids=cohort, square_ids=cohort if coarse_is_square else set())})
    assert not result.ok, result.detail
    assert "z8 64 -> z5 64" in result.detail


def test_decay_checks_cohorts_above_the_aggregate_peak_too():
    fine = set(range(64))
    coarse = set(range(100))
    result = _decay({8: ZoomScan(ids=fine, square_ids=fine),
                     5: ZoomScan(ids=coarse, square_ids=coarse)})
    assert not result.ok, result.detail


@pytest.mark.parametrize("ids,missing", [(False, 0), (True, 1)])
def test_decay_requires_source_unique_ids(ids, missing):
    result = _decay({8: ZoomScan(squares=76, no_id=missing),
                     5: ZoomScan(squares=45)}, ids=ids)
    assert not result.ok
    assert "--ids" in result.detail


@pytest.mark.parametrize("survivors,ok", [(31, True), (32, True), (33, False)])
def test_decay_keeps_the_existing_halving_threshold(survivors, ok):
    cohort = set(range(64))
    result = _decay({8: ZoomScan(ids=cohort, square_ids=cohort),
                     5: ZoomScan(ids=set(range(survivors)))})
    assert result.ok is ok, result.detail


def test_decay_does_not_classify_large_source_squares_as_placeholders():
    # Moldova's canonical level has ~50-unit field squares. These have not
    # collapsed and need not decay over three zooms. Default placeholders
    # are 4 units across (one GSD at extent 4096 / GSD base 1024).
    def square(side):
        return [[[0, 0], [side, 0], [side, side], [0, side], [0, 0]]]

    assert is_placeholder_square("Polygon", square(4))
    assert is_placeholder_square("Polygon", [[[0, 0], [4, 0], [4, 5], [0, 5], [0, 0]]])
    assert not is_placeholder_square("Polygon", square(50))


def test_decay_excludes_features_with_large_instances_at_the_fine_zoom():
    cohort = set(range(64))
    # A tile can clip a small square from a much larger feature. Its other
    # tile instances reveal that the feature has not fully collapsed.
    fine = ZoomScan(ids=cohort, square_ids=cohort, large_ids=cohort)
    result = _decay({8: fine, 5: ZoomScan(ids=cohort)})
    assert "no cohort was eligible" in result.detail


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


def _polygon_tile(wkt, with_ids=True):
    features = [{"geometry": wkt, "properties": {}, **({"id": i} if with_ids else {})}
                for i in range(64)]
    return mapbox_vector_tile.encode(
        [{"name": "test", "features": features}],
        default_options={"extents": 4096, "y_coord_down": True},
    )


def test_decay_rejects_real_mvt_with_omitted_ids():
    from mapbox_vector_tile.Mapbox import vector_tile_pb2

    raw = _polygon_tile("POLYGON((0 0,4 0,4 4,0 4,0 0))", with_ids=False)
    tile = vector_tile_pb2.tile()
    tile.ParseFromString(raw)
    assert all(not f.HasField("id") for f in tile.layers[0].features)
    scans = verify_archive.scan_chunk(
        ([(z, 0, 0, raw) for z in [5, 8]], "test", Compression.NONE, (0, 0), True)
    )
    result = _decay(scans)
    assert not result.ok, result.detail
    assert "MVT ids on every feature" in result.detail
    assert scans[8].no_id == 64


def test_decay_keeps_a_placeholder_clipped_at_the_buffered_tile_edge():
    # Buffer 5 at extent 4096 ends at x=4176. The same source square
    # spans 4175..4179, clipped to 1x4 here and complete in the neighbour.
    clipped = _polygon_tile("POLYGON((4175 20,4176 20,4176 24,4175 24,4175 20))")
    whole = _polygon_tile("POLYGON((79 20,83 20,83 24,79 24,79 20))")
    fine = verify_archive.scan_chunk(
        ([(8, 0, 128, clipped)], "test", Compression.NONE, (0, 0), True)
    )[8]
    neighbour = verify_archive.scan_chunk(
        ([(8, 1, 128, whole)], "test", Compression.NONE, (0, 0), True)
    )[8]
    fine.merge(neighbour)
    result = _decay({8: fine, 5: ZoomScan(ids=set(range(64)))})
    assert not result.ok, result.detail
    assert "z8 64 -> z5 64" in result.detail
    assert fine.square_ids == set(range(64))
    assert not fine.large_ids


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


def test_decay_scan_and_merge_exclude_a_clipped_square_from_a_larger_feature():
    def scan(wkt):
        raw = mapbox_vector_tile.encode(
            [{"name": "t", "features": [{"id": 7, "properties": {}, "geometry": wkt}]}],
            default_options={"extents": 4096, "y_coord_down": True},
        )
        return verify_archive.scan_chunk(
            ([(8, 0, 0, gzip.compress(raw))], "t", Compression.GZIP, (0, 0), True)
        )[8]

    tiny = scan("POLYGON((0 0,4 0,4 4,0 4,0 0))")
    large = scan("POLYGON((0 0,50 0,50 50,0 50,0 0))")
    assert tiny.square_ids == {7} and not tiny.large_ids
    assert not large.square_ids and large.large_ids == {7}
    tiny.merge(large)
    assert not (tiny.square_ids - tiny.large_ids)


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
