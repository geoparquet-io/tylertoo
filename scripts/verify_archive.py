# /// script
# requires-python = ">=3.11"
# dependencies = [
#   "pmtiles==3.8.1",
#   "mapbox-vector-tile==2.2.0",
#   "shapely==2.2.0",
#   "pyarrow==26.0.0",
# ]
# ///
"""Validate a tylertoo-produced PMTiles archive with independent readers (#421).

Nothing here uses tylertoo's own code. The archive is opened with the Python
`pmtiles` reader (header, metadata, directory walk) and tiles are decoded with
`mapbox-vector-tile`, so a self-consistent-but-wrong header, directory or MVT
framing is caught by readers that never saw our writer. `pmtiles verify`
(go-pmtiles) is the structural complement; this script is the one that opens
tile contents.

Checks, in order:
  * header: magic + spec version 3, MVT tile type, a supported tile
    compression, well-ordered bounds, min_zoom <= max_zoom
  * metadata: parseable JSON; `vector_layers` names the expected layer
  * directories: every addressed tile is reachable through the directory
    walk, non-empty, and the count matches the header's
    addressed_tiles_count; header min/max zoom equal the shallowest/deepest
    zoom that actually holds a tile (what go-pmtiles requires, #554)
  * per zoom: `--per-zoom` sampled tiles (deterministic: first, last, evenly
    spaced) decompress, decode, contain the expected layer, hold only valid
    geometry types with non-empty coordinates, and at least one sampled tile
    at every zoom carries a feature

`--strict` (#779, G3) decodes EVERY tile instead of a sample and checks the
properties that must hold across the whole archive, not just its framing:

  tile_size            (a) every stored (compressed) tile is <= the
                       compressed cap and every decompressed tile is <= the
                       raw cap: the caps the archive was built with, passed
                       as --max-compressed-tile-size / --max-tile-size
                       (tylertoo's defaults when omitted; 0 = no cap) (#752)
  polygon_valid        (b) shapely.is_valid for every Polygon/MultiPolygon in
                       tile coordinates (decoded y-down, as a client draws
                       it); the first invalid tile and feature are printed
                       (#748)
  feature_accounting   (c) input features = distinct features at the finest
                       zoom + disjoint source losses `--report` exposes. Needs
                       source-unique MVT ids (--ids, i.e. the archive was
                       built with `--feature-id`). A loss no counter explains
                       is a silent drop (#707: the clip guard has no counter)
  monotone             (d) distinct features at zoom z-1 <= at zoom z (needs
                       --ids)
  decay                (d) with --expect-decay: under thinning, placeholder
                       squares (a sub-pixel polygon drawn as a square) decay
                       toward coarse zooms instead of surviving every level
                       (#749)
  property_accounting  (e) every input property column (--input) is either a
                       `vector_layers` field or listed in the report's
                       `skipped_property_columns`, and no tile carries a key
                       the metadata does not declare (#434)

A failing invariant can be exempted, one row per {fixture, settings, zoom,
invariant, issue}, in benchmarks/e2e/invariant_exemptions.toml (pass
--exemptions with --fixture/--settings). Exemptions are strict: an exempted
invariant that PASSES is a failure too ("XPASS: remove the row"), so a fix
cannot land without its exemption coming off. `--check-exemptions` validates
the file itself for valid issue references, invariant names and duplicate rows.

Usage:
  uv run scripts/verify_archive.py out.pmtiles --layer overview
  uv run scripts/verify_archive.py a.pmtiles b.pmtiles --layer overview --per-zoom 5
  uv run scripts/verify_archive.py out.pmtiles --layer roads --strict \\
      --report tiles-report.json --input roads.parquet --ids \\
      --exemptions benchmarks/e2e/invariant_exemptions.toml \\
      --fixture road-detections --settings defaults
  uv run scripts/verify_archive.py --check-exemptions \\
      benchmarks/e2e/invariant_exemptions.toml

Exit status is 1 on any failure, with every failure listed.
"""

from __future__ import annotations

import argparse
import gzip
import json
import os
import re
import struct
import sys
import tomllib
from collections import defaultdict
from concurrent.futures import ProcessPoolExecutor
from dataclasses import dataclass, field
from pathlib import Path

import mapbox_vector_tile
from pmtiles.reader import MmapSource, all_tiles
from pmtiles.tile import (
    Compression,
    MagicNumberNotFound,
    SpecVersionUnsupported,
    TileType,
    deserialize_header,
)

# What the pmtiles reader raises on malformed input: bad magic / spec byte,
# a truncated header or directory (IndexError, struct.error), a corrupt
# gzip block (BadGzipFile, EOFError), an undecodable varint (ValueError).
READER_ERRORS = (
    MagicNumberNotFound,
    SpecVersionUnsupported,
    gzip.BadGzipFile,
    EOFError,
    struct.error,
    IndexError,
    ValueError,
)

VALID_GEOMETRY_TYPES = frozenset(
    {
        "Point",
        "MultiPoint",
        "LineString",
        "MultiLineString",
        "Polygon",
        "MultiPolygon",
    }
)
POLYGON_TYPES = frozenset({"Polygon", "MultiPolygon"})
HEADER_LEN = 127
SPEC_VERSION = 3
E7 = 1e7
LON_MAX = 180.0
LAT_MAX = 90.0

# tylertoo's `tiles` defaults (#761): DEFAULT_TILE_SIZE_LIMIT (2 MiB raw) and
# DEFAULT_COMPRESSED_TILE_SIZE_LIMIT (500 KiB gzip) in
# crates/core/src/overview/export.rs. `--verbatim` turns both off.
DEFAULT_RAW_CAP = 2048 * 1024
DEFAULT_COMPRESSED_CAP = 500 * 1024

INVARIANTS = (
    "tile_size",
    "polygon_valid",
    "feature_accounting",
    "monotone",
    "decay",
    "property_accounting",
)
# Invariants with a per-zoom verdict; the rest are archive-wide (zoom "*").
PER_ZOOM_INVARIANTS = frozenset({"tile_size", "polygon_valid", "monotone"})

# Placeholder-square decay (#749). Under the dither, a square standing in for
# a polygon of area A at a level of side s survives with probability A / s^2,
# and the side doubles per coarser zoom, so the squares should fall ~4x per
# zoom. Over DECAY_WINDOW zooms they must at least halve, once there are
# enough of them for the ratio to mean something.
DECAY_WINDOW = 3
DECAY_MAX_RATIO = 0.5
DECAY_MIN_SQUARES = 32

INVALID_EXEMPLARS = 3


class VerifyError(Exception):
    """One failed check, with a message that names the archive and the check."""


def sample_indices(n: int, k: int) -> list[int]:
    """Deterministic sample of `k` indices out of `n`: first, last, and
    evenly spaced ones between. Returns all indices when n <= k."""
    if n <= k:
        return list(range(n))
    if k == 1:
        return [0]
    step = (n - 1) / (k - 1)
    return sorted({round(i * step) for i in range(k)})


def decompress(data: bytes, compression: Compression) -> bytes:
    if compression == Compression.GZIP:
        return gzip.decompress(data)
    if compression == Compression.NONE:
        return data
    raise VerifyError(f"unsupported tile compression {compression.name}")


def check_header(header: dict) -> None:
    if header["version"] != SPEC_VERSION:
        raise VerifyError(f"spec version {header['version']}, expected {SPEC_VERSION}")
    if header["tile_type"] != TileType.MVT:
        raise VerifyError(f"tile type {header['tile_type'].name}, expected MVT")
    if header["tile_compression"] not in (Compression.GZIP, Compression.NONE):
        raise VerifyError(
            f"tile compression {header['tile_compression'].name} "
            "is not one this script can decode (gzip/none)"
        )
    if header["min_zoom"] > header["max_zoom"]:
        raise VerifyError(
            f"header min_zoom {header['min_zoom']} > max_zoom {header['max_zoom']}"
        )
    lon_min, lat_min = header["min_lon_e7"] / E7, header["min_lat_e7"] / E7
    lon_max, lat_max = header["max_lon_e7"] / E7, header["max_lat_e7"] / E7
    lon_ok = -LON_MAX <= lon_min <= lon_max <= LON_MAX
    lat_ok = -LAT_MAX <= lat_min <= lat_max <= LAT_MAX
    if not (lon_ok and lat_ok):
        raise VerifyError(
            f"header bounds not well-ordered / in range: "
            f"[{lon_min}, {lat_min}, {lon_max}, {lat_max}]"
        )
    if not header["min_zoom"] <= header["center_zoom"] <= header["max_zoom"]:
        raise VerifyError(
            f"center_zoom {header['center_zoom']} outside "
            f"[{header['min_zoom']}, {header['max_zoom']}]"
        )


def check_metadata(get_bytes, header: dict, layer: str) -> dict:
    raw = get_bytes(header["metadata_offset"], header["metadata_length"])
    # pmtiles 3.8.1's deserialize_directory always gzip-decompresses, so an
    # archive whose directories are stored uncompressed (spec-legal) cannot
    # be walked by this reader; refuse it here rather than crash later.
    if header["internal_compression"] != Compression.GZIP:
        raise VerifyError(
            f"internal compression {header['internal_compression'].name}: the "
            "python pmtiles reader (3.8.1) only walks gzip directories"
        )
    raw = gzip.decompress(raw)
    try:
        meta = json.loads(raw)
    except ValueError as e:
        raise VerifyError(f"metadata is not JSON: {e}") from e
    layers = meta.get("vector_layers")
    if not isinstance(layers, list) or not layers:
        raise VerifyError("metadata has no vector_layers")
    names = [lv.get("id") for lv in layers]
    if layer not in names:
        raise VerifyError(f"metadata vector_layers {names} lacks layer {layer!r}")
    return meta


def walk_tiles(get_bytes, header: dict) -> dict[int, list[tuple[int, int, bytes]]]:
    """Every addressed tile via the directory walk, grouped by zoom."""
    by_zoom: dict[int, list[tuple[int, int, bytes]]] = defaultdict(list)
    total = 0
    for (z, x, y), data in all_tiles(get_bytes):
        total += 1
        if len(data) == 0:
            raise VerifyError(f"tile z{z}/{x}/{y} is addressed but has 0 bytes")
        by_zoom[z].append((x, y, data))
    if total == 0:
        raise VerifyError("archive addresses no tiles")
    if total != header["addressed_tiles_count"]:
        raise VerifyError(
            f"directory walk found {total} tiles, header says "
            f"addressed_tiles_count={header['addressed_tiles_count']}"
        )
    z_lo, z_hi = min(by_zoom), max(by_zoom)
    if (z_lo, z_hi) != (header["min_zoom"], header["max_zoom"]):
        raise VerifyError(
            f"tiles span z{z_lo}..z{z_hi} but header declares "
            f"min_zoom={header['min_zoom']} max_zoom={header['max_zoom']}"
        )
    return by_zoom


def check_feature(z: int, x: int, y: int, i: int, feature: dict) -> None:
    geom = feature.get("geometry")
    if not isinstance(geom, dict):
        raise VerifyError(f"z{z}/{x}/{y} feature {i}: no geometry object")
    gtype = geom.get("type")
    if gtype not in VALID_GEOMETRY_TYPES:
        raise VerifyError(f"z{z}/{x}/{y} feature {i}: invalid geometry type {gtype!r}")
    coords = geom.get("coordinates")
    if not coords:
        raise VerifyError(f"z{z}/{x}/{y} feature {i}: {gtype} with empty coordinates")


def decode_features(
    z: int, x: int, y: int, raw: bytes, layer: str, empty_polygons_ok: bool = False
) -> list[dict]:
    """Decode one tile with mapbox-vector-tile (y-down tile coordinates, as a
    client draws it, and as #748 measured); return `layer`'s features. Raises
    VerifyError for a missing layer or an invalid geometry. With
    `empty_polygons_ok` (strict mode) a polygon with no coordinates is
    returned for the polygon_valid check to count, not raised."""
    try:
        decoded = mapbox_vector_tile.decode(
            raw, default_options={"geojson": True, "y_coord_down": True}
        )
    except Exception as e:  # any decoder error is a failure
        raise VerifyError(f"z{z}/{x}/{y}: mapbox-vector-tile failed: {e}") from e
    if layer not in decoded:
        raise VerifyError(
            f"z{z}/{x}/{y}: layer {layer!r} missing (found {sorted(decoded)})"
        )
    features = decoded[layer].get("features", [])
    for i, feature in enumerate(features):
        geom = feature.get("geometry")
        if (
            empty_polygons_ok
            and isinstance(geom, dict)
            and geom.get("type") in POLYGON_TYPES
            and not geom.get("coordinates")
        ):
            continue
        check_feature(z, x, y, i, feature)
    return features


def decode_tile(z: int, x: int, y: int, raw: bytes, layer: str) -> int:
    """Decode one tile; return its feature count in `layer`."""
    return len(decode_features(z, x, y, raw, layer))


def verify_archive(path: Path, layer: str, per_zoom: int) -> list[str]:
    """Run every sampled check; return the per-zoom summary lines. Raises
    VerifyError on the first failed check."""
    with path.open("rb") as f:
        get_bytes = MmapSource(f)
        header = deserialize_header(get_bytes(0, HEADER_LEN))
        check_header(header)
        check_metadata(get_bytes, header, layer)
        by_zoom = walk_tiles(get_bytes, header)

        lines = []
        for z in sorted(by_zoom):
            tiles = by_zoom[z]
            picked = sample_indices(len(tiles), per_zoom)
            counts = []
            for idx in picked:
                x, y, data = tiles[idx]
                raw = decompress(data, header["tile_compression"])
                counts.append(decode_tile(z, x, y, raw, layer))
            if not any(counts):
                raise VerifyError(
                    f"z{z}: none of {len(picked)} sampled tiles carries a feature "
                    f"in layer {layer!r}"
                )
            lines.append(
                f"  z{z:<2} tiles={len(tiles):<6} sampled={len(picked)} "
                f"features/sample={counts}"
            )
        return lines


# ---------------------------------------------------------------------------
# --strict: every tile, whole-archive invariants (#779)
# ---------------------------------------------------------------------------


@dataclass
class ZoomScan:
    """Everything the strict invariants need from one zoom's tiles."""

    tiles: int = 0
    features: int = 0  # (feature, tile) pairs, like the report's counts
    ids: set = field(default_factory=set)
    no_id: int = 0
    polygons: int = 0
    invalid: int = 0
    invalid_exemplars: list = field(default_factory=list)
    squares: int = 0  # (square, tile) pairs
    square_ids: set = field(default_factory=set)
    over_compressed: list = field(default_factory=list)  # (z/x/y, bytes)
    over_raw: list = field(default_factory=list)
    max_compressed: int = 0
    max_raw: int = 0
    keys: set = field(default_factory=set)

    def merge(self, other: ZoomScan) -> None:
        self.tiles += other.tiles
        self.features += other.features
        self.ids |= other.ids
        self.no_id += other.no_id
        self.polygons += other.polygons
        self.invalid += other.invalid
        room = INVALID_EXEMPLARS - len(self.invalid_exemplars)
        self.invalid_exemplars.extend(other.invalid_exemplars[: max(room, 0)])
        self.squares += other.squares
        self.square_ids |= other.square_ids
        self.over_compressed.extend(other.over_compressed)
        self.over_raw.extend(other.over_raw)
        self.max_compressed = max(self.max_compressed, other.max_compressed)
        self.max_raw = max(self.max_raw, other.max_raw)
        self.keys |= other.keys


def is_placeholder_square(gtype: str, coords) -> bool:
    """A one-ring axis-aligned square: the shape tylertoo draws in place of a
    polygon too small for its level (`--collapse-square`, the default since
    #745). Tile-coordinate quantization may shift a side by one unit."""
    if gtype != "Polygon" or len(coords) != 1:
        return False
    ring = coords[0]
    pts = ring[:-1] if len(ring) > 1 and ring[0] == ring[-1] else ring
    if len(pts) != 4:
        return False
    xs = sorted({p[0] for p in pts})
    ys = sorted({p[1] for p in pts})
    if len(xs) != 2 or len(ys) != 2:
        return False
    w, h = xs[1] - xs[0], ys[1] - ys[0]
    return w > 0 and h > 0 and abs(w - h) <= 1


def scan_chunk(args) -> dict[int, ZoomScan]:
    """Decode a chunk of tiles; return per-zoom partial scans. Runs in a
    worker process, so it imports shapely itself."""
    import shapely.geometry
    import shapely.validation

    chunk, layer, compression, caps, ids = args
    raw_cap, compressed_cap = caps
    out: dict[int, ZoomScan] = defaultdict(ZoomScan)
    for z, x, y, data in chunk:
        s = out[z]
        s.tiles += 1
        stored = len(data)
        raw = decompress(data, Compression(compression))
        s.max_compressed = max(s.max_compressed, stored)
        s.max_raw = max(s.max_raw, len(raw))
        if compressed_cap and stored > compressed_cap:
            s.over_compressed.append((f"{z}/{x}/{y}", stored))
        if raw_cap and len(raw) > raw_cap:
            s.over_raw.append((f"{z}/{x}/{y}", len(raw)))
        features = decode_features(z, x, y, raw, layer, empty_polygons_ok=True)
        s.features += len(features)
        for i, feat in enumerate(features):
            s.keys.update((feat.get("properties") or {}).keys())
            if ids:
                fid = feat.get("id")
                if fid is None:
                    s.no_id += 1
                else:
                    s.ids.add(fid)
            geom = feat["geometry"]
            if geom["type"] not in POLYGON_TYPES:
                continue
            s.polygons += 1
            if not geom["coordinates"]:
                s.invalid += 1
                if len(s.invalid_exemplars) < INVALID_EXEMPLARS:
                    s.invalid_exemplars.append(
                        f"tile {z}/{x}/{y} feature {i} (id {feat.get('id')}): "
                        f"{geom['type']} with no coordinates"
                    )
                continue
            if is_placeholder_square(geom["type"], geom["coordinates"]):
                s.squares += 1
                if ids and feat.get("id") is not None:
                    s.square_ids.add(feat["id"])
            try:
                shp = shapely.geometry.shape(geom)
                valid = shp.is_valid
            except Exception as e:  # unbuildable rings are invalid too
                valid, shp = False, None
                reason = f"shapely could not build it: {e}"
            if not valid:
                s.invalid += 1
                if len(s.invalid_exemplars) < INVALID_EXEMPLARS:
                    if shp is not None:
                        reason = shapely.validation.explain_validity(shp)
                    s.invalid_exemplars.append(
                        f"tile {z}/{x}/{y} feature {i} (id {feat.get('id')}): "
                        f"{geom['type']}: {reason}"
                    )
    return dict(out)


def strict_scan(
    by_zoom: dict[int, list[tuple[int, int, bytes]]],
    layer: str,
    compression: Compression,
    caps: tuple[int, int],
    ids: bool,
    workers: int,
) -> dict[int, ZoomScan]:
    """Decode every tile, in parallel chunks; merge into one scan per zoom."""
    flat = [(z, x, y, d) for z in sorted(by_zoom) for (x, y, d) in by_zoom[z]]
    target = 64
    chunks = [flat[i : i + target] for i in range(0, len(flat), target)]
    jobs = [(c, layer, compression.value, caps, ids) for c in chunks]
    scans: dict[int, ZoomScan] = {z: ZoomScan() for z in by_zoom}
    if workers <= 1 or len(chunks) <= 1:
        results = map(scan_chunk, jobs)
    else:
        pool = ProcessPoolExecutor(max_workers=workers)
        results = pool.map(scan_chunk, jobs)
    for part in results:
        for z, s in part.items():
            scans[z].merge(s)
    if workers > 1 and len(chunks) > 1:
        pool.shutdown()
    return scans


@dataclass
class Outcome:
    """One invariant's verdict at one zoom (or archive-wide, zoom None)."""

    invariant: str
    zoom: int | None
    ok: bool
    detail: str
    status: str = ""  # PASS / FAIL / XFAIL / XPASS / SKIP, set by apply_exemptions
    issue: int | None = None

    @property
    def where(self) -> str:
        return "*" if self.zoom is None else f"z{self.zoom}"


def input_columns(input_path: Path, feature_id: str | None) -> tuple[int, list[str]]:
    """Rows and property columns of the source parquet: every top-level
    column except the geometry (the `geo` primary column, or the native
    GEOMETRY/GEOGRAPHY one), its bbox covering column and the feature-id
    column (which becomes the MVT id, not a tag)."""
    import pyarrow.parquet as pq

    pf = pq.ParquetFile(input_path)
    schema = pf.schema_arrow
    md = schema.metadata or {}
    skip = set()
    geo = json.loads(md[b"geo"]) if b"geo" in md else None
    if geo:
        skip.add(geo.get("primary_column", "geometry"))
        for col in (geo.get("columns") or {}).values():
            cov = (col.get("covering") or {}).get("bbox") or {}
            for path in cov.values():
                if isinstance(path, list) and path:
                    skip.add(path[0])
    import pyarrow.types as pat

    for f in schema:
        ext = getattr(f.type, "extension_name", "") or ""
        if ext.startswith("geoarrow"):
            skip.add(f.name)
        # A {xmin, ymin, xmax, ymax} struct of floats is a bbox covering by
        # design, declared or not (writer.rs `is_covering_column`).
        if pat.is_struct(f.type):
            kids = [f.type.field(i) for i in range(f.type.num_fields)]
            names = sorted(k.name.lower() for k in kids)
            if names == ["xmax", "xmin", "ymax", "ymin"] and all(
                pat.is_floating(k.type) for k in kids
            ):
                skip.add(f.name)
    if not geo and not any(getattr(f.type, "extension_name", "") for f in schema):
        # No `geo` key and no native geometry type: tylertoo picks the column
        # by name (`geometry`, else the first `geom*`; convert.rs
        # `find_geometry_column`).
        names = schema.names
        pick = "geometry" if "geometry" in names else next(
            (n for n in names if n.lower().startswith("geom")), None
        )
        if pick:
            skip.add(pick)
    if feature_id:
        skip.add(feature_id)
    cols = [f.name for f in schema if f.name not in skip]
    return pf.metadata.num_rows, cols


def add_feature_ids(src: Path, dst: Path, column: str) -> int:
    """Copy `src` to `dst` with one more column, `column`, holding each row's
    index (uint64): a source-unique id for `tiles --feature-id`, so the strict
    checks can count distinct features per zoom. Row groups, the schema
    metadata (`geo`) and the column order are kept; the id goes last.
    Returns the row count."""
    import pyarrow as pa
    import pyarrow.parquet as pq

    pf = pq.ParquetFile(src)
    schema = pf.schema_arrow
    if column in schema.names:
        raise VerifyError(f"{src} already has a column named {column!r}")
    out_schema = schema.append(pa.field(column, pa.uint64(), nullable=False))
    start = 0
    with pq.ParquetWriter(dst, out_schema, compression="zstd") as w:
        for i in range(pf.metadata.num_row_groups):
            t = pf.read_row_group(i)
            n = t.num_rows
            t = t.append_column(
                out_schema.field(column), pa.array(range(start, start + n), pa.uint64())
            )
            w.write_table(t.replace_schema_metadata(schema.metadata), row_group_size=max(n, 1))
            start += n
    return start


def evaluate(
    scans: dict[int, ZoomScan],
    meta: dict,
    layer: str,
    report: dict | None,
    caps: tuple[int, int],
    ids: bool,
    expect_decay: bool,
    input_info: tuple[int, list[str]] | None,
) -> list[Outcome]:
    """Turn the scan into per-invariant outcomes."""
    raw_cap, compressed_cap = caps
    out: list[Outcome] = []
    zooms = sorted(scans)

    # (a) tile size
    for z in zooms:
        s = scans[z]
        bad = [(t, b, "stored") for t, b in s.over_compressed] + [
            (t, b, "raw") for t, b in s.over_raw
        ]
        if bad:
            worst = ", ".join(
                f"{t} {b} B {kind}" for t, b, kind in sorted(bad, key=lambda t: -t[1])[:3]
            )
            detail = (
                f"{len(s.over_compressed)} tile(s) over the {compressed_cap} B compressed "
                f"cap, {len(s.over_raw)} over the {raw_cap} B raw cap; worst: {worst}"
            )
        else:
            detail = f"max {s.max_compressed} B stored / {s.max_raw} B raw"
        out.append(Outcome("tile_size", z, not bad, detail))

    # (b) polygon validity
    for z in zooms:
        s = scans[z]
        if s.polygons == 0:
            continue
        detail = f"{s.invalid} / {s.polygons} polygon features invalid"
        if s.invalid:
            detail += "; e.g. " + "; ".join(s.invalid_exemplars)
        out.append(Outcome("polygon_valid", z, s.invalid == 0, detail))

    exp = (report or {}).get("export") or {}
    conv = (report or {}).get("convert") or {}

    if ids:
        # (c) feature accounting at the finest zoom
        finest = max(zooms)
        written = len(scans[finest].ids)
        if input_info is not None:
            n_in, src = input_info[0], "input rows"
        else:
            n_in, src = conv.get("input_rows"), "report input_rows"
        # A source lost in one tile may survive in another. Only the core's
        # final per-source losses can partition the input; pair counters do
        # not participate in conservation.
        losses = [
            loss for section in (conv, exp)
            for loss in section.get("source_losses", [])
            if loss.get("zoom") in (None, finest)
        ]
        lost_ids = [loss["id"] for loss in losses]
        lost = set(lost_ids)
        seen = scans[finest].ids
        if n_in is None:
            out.append(Outcome("feature_accounting", None, False, "no input count"))
        else:
            expected = set(range(n_in))
            missing = expected - seen - lost
            overlap = seen & lost
            foreign = (seen | lost) - expected
            duplicates = len(lost_ids) - len(lost)
            causes: dict[str, int] = defaultdict(int)
            for loss in losses:
                causes[loss["cause"]] += 1
            named = ", ".join(f"{k}={v}" for k, v in sorted(causes.items()))
            detail = (
                f"{n_in} {src} = {written} distinct at z{finest} + {len(lost)} source losses "
                f"({named or 'no source loss'})"
            )
            if missing:
                detail += f"; {len(missing)} unexplained source ids: {sorted(missing)[:5]}"
            if overlap:
                detail += f"; {len(overlap)} source ids both written and lost: {sorted(overlap)[:5]}"
            if foreign:
                detail += f"; {len(foreign)} source ids outside the input: {sorted(foreign)[:5]}"
            if duplicates:
                detail += f"; {duplicates} duplicate source losses"
            ok = not (missing or overlap or foreign or duplicates)
            out.append(Outcome("feature_accounting", None, ok, detail))

        # (d) monotone: distinct features never rise toward coarser zooms.
        # A zoom inside the archive's range with no tile counts as 0.
        counts = {z: len(scans[z].ids) for z in zooms}
        for z in range(min(zooms) + 1, max(zooms) + 1):
            coarse, fine = counts.get(z - 1, 0), counts.get(z, 0)
            ok = coarse <= fine
            detail = f"z{z - 1} {coarse} <= z{z} {fine}" if ok else (
                f"z{z - 1} holds {coarse} distinct features, more than z{z}'s {fine}"
            )
            out.append(Outcome("monotone", z - 1, ok, detail))

    # (d) decay of placeholder squares under thinning (#749)
    if expect_decay:
        # Squares first RISE toward coarse zooms (more polygons fall below a
        # pixel), so decay is only owed from the zoom holding the most of
        # them down. A zoom with no tiles holds none.
        # Distinct squares when ids are source-unique; (square, tile) pairs
        # otherwise, which a zoom split over more tiles inflates.
        sq = {z: len(scans[z].square_ids) if ids else scans[z].squares for z in zooms}
        peak = max(zooms, key=lambda z: (sq[z], z))
        stuck = []
        for z in zooms:
            lo = z - DECAY_WINDOW
            if z > peak or sq[z] < DECAY_MIN_SQUARES or lo < 0:
                continue
            if sq.get(lo, 0) > DECAY_MAX_RATIO * sq[z]:
                stuck.append(f"z{z} {sq[z]} -> z{lo} {sq.get(lo, 0)}")
        series = " ".join(f"z{z}:{sq[z]}" for z in zooms if sq[z])
        detail = f"placeholder squares {series or 'none'}"
        if stuck:
            more = f" (+{len(stuck) - 3} more)" if len(stuck) > 3 else ""
            detail += (
                f"; not halving over {DECAY_WINDOW} zooms: " + ", ".join(stuck[:3]) + more
            )
        out.append(Outcome("decay", None, not stuck, detail))

    # (e) property-column accounting (#434)
    if input_info is not None:
        layer_meta = next(lv for lv in meta["vector_layers"] if lv.get("id") == layer)
        declared = set((layer_meta.get("fields") or {}).keys())
        skipped = {c["name"] for c in exp.get("skipped_property_columns", [])}
        cols = set(input_info[1])
        missing = sorted(cols - declared - skipped)
        seen = set().union(*(scans[z].keys for z in zooms))
        undeclared = sorted(seen - declared)
        problems = []
        if missing:
            problems.append(
                f"input column(s) neither in vector_layers nor skipped_property_columns: "
                f"{missing}"
            )
        if undeclared:
            problems.append(f"tile key(s) vector_layers does not declare: {undeclared}")
        detail = "; ".join(problems) or (
            f"{len(cols)} input column(s): {len(cols & declared)} declared, "
            f"{len(cols & skipped)} reported skipped"
        )
        out.append(Outcome("property_accounting", None, not problems, detail))
    return out


# ---------------------------------------------------------------------------
# exemptions
# ---------------------------------------------------------------------------

ISSUE_RE = re.compile(r"^#?(\d+)$")


def load_exemptions(path: Path) -> list[dict]:
    data = tomllib.loads(path.read_text())
    rows = data.get("exemption", [])
    if not isinstance(rows, list):
        raise VerifyError(f"{path}: `exemption` must be an array of tables")
    return rows


def check_exemption_rows(rows: list[dict], path: Path) -> list[str]:
    """Structural problems with the exemption file: every row names a
    fixture, settings, zoom ("*" or an int), a known invariant and an issue;
    no duplicate rows."""
    problems = []
    seen = set()
    for i, r in enumerate(rows):
        where = f"{path}: row {i + 1}"
        for k in ("fixture", "settings", "zoom", "invariant", "issue", "reason"):
            if k not in r:
                problems.append(f"{where}: missing `{k}`")
        if r.get("invariant") not in INVARIANTS:
            problems.append(f"{where}: unknown invariant {r.get('invariant')!r}")
        z = r.get("zoom")
        if not (z == "*" or (isinstance(z, int) and 0 <= z <= 30)):
            problems.append(f"{where}: zoom must be \"*\" or 0..30, got {z!r}")
        if r.get("invariant") not in PER_ZOOM_INVARIANTS and z != "*":
            problems.append(f"{where}: {r.get('invariant')} is archive-wide; zoom must be \"*\"")
        if not ISSUE_RE.match(str(r.get("issue", ""))):
            problems.append(f"{where}: issue must be an issue number, got {r.get('issue')!r}")
        key = tuple(r.get(k) for k in ("fixture", "settings", "zoom", "invariant"))
        if key in seen:
            problems.append(f"{where}: duplicate of an earlier row {key}")
        seen.add(key)
    return problems


def check_issues_open(rows: list[dict]) -> list[str]:
    """Every exemption must link an OPEN issue: an exemption for a closed
    issue is stale (the fix landed, or the issue was dropped). Uses `gh`."""
    import subprocess

    problems = []
    for num in sorted({int(ISSUE_RE.match(str(r["issue"])).group(1)) for r in rows}):
        proc = subprocess.run(
            ["gh", "issue", "view", str(num), "--json", "state", "--jq", ".state"],
            capture_output=True,
            text=True,
        )
        state = proc.stdout.strip()
        if proc.returncode != 0:
            problems.append(f"#{num}: could not read its state: {proc.stderr.strip()}")
        elif state != "OPEN":
            problems.append(f"#{num} is {state}: remove its exemption row(s)")
    return problems


def apply_exemptions(
    outcomes: list[Outcome], rows: list[dict], fixture: str, settings: str
) -> list[Outcome]:
    """Mark each outcome PASS / FAIL / XFAIL / XPASS. A row matches on
    fixture and settings; `zoom = "*"` covers every zoom of its invariant
    (XFAIL when at least one fails), an integer zoom only that one."""
    mine = [r for r in rows if r["fixture"] == fixture and r["settings"] == settings]
    for o in outcomes:
        o.status = "PASS" if o.ok else "FAIL"
    extra: list[Outcome] = []
    for r in mine:
        inv, z = r["invariant"], r["zoom"]
        covered = [o for o in outcomes if o.invariant == inv and (z == "*" or o.zoom == z)]
        issue = int(ISSUE_RE.match(str(r["issue"])).group(1))
        failing = [o for o in covered if not o.ok]
        if failing:
            for o in failing:
                o.status, o.issue = "XFAIL", issue
        else:
            where = "*" if z == "*" else f"z{z}"
            extra.append(
                Outcome(
                    inv,
                    None if z == "*" else z,
                    False,
                    f"exempted for #{issue} but it passed"
                    + ("" if covered else " (the check did not run)")
                    + f": remove the {fixture}/{settings}/{where}/{inv} row",
                    status="XPASS",
                    issue=issue,
                )
            )
    return outcomes + extra


def verify_strict(path: Path, args) -> tuple[list[Outcome], list[str]]:
    """Every tile, every invariant. Returns the outcomes (with exemptions
    applied) and the per-zoom summary lines."""
    with path.open("rb") as f:
        get_bytes = MmapSource(f)
        header = deserialize_header(get_bytes(0, HEADER_LEN))
        check_header(header)
        meta = check_metadata(get_bytes, header, args.layer)
        by_zoom = walk_tiles(get_bytes, header)
        caps = (args.max_tile_size, args.max_compressed_tile_size)
        scans = strict_scan(
            by_zoom, args.layer, header["tile_compression"], caps, args.ids, args.workers
        )
    report = json.loads(args.report.read_text()) if args.report else None
    if report is not None and "convert" not in report:
        raise VerifyError(f"{args.report}: expected a `tiles --report` JSON")
    input_info = input_columns(args.input, args.feature_id) if args.input else None
    if input_info is not None and args.input_features is not None:
        input_info = (args.input_features, input_info[1])
    outcomes = evaluate(
        scans,
        meta,
        args.layer,
        report,
        caps,
        args.ids,
        args.expect_decay,
        input_info,
    )
    rows = load_exemptions(args.exemptions) if args.exemptions else []
    outcomes = apply_exemptions(outcomes, rows, args.fixture or "", args.settings or "")
    lines = []
    for z in sorted(scans):
        s = scans[z]
        ids = f" distinct={len(s.ids)}" if args.ids else ""
        lines.append(
            f"  z{z:<2} tiles={s.tiles:<6} features={s.features:<8}{ids} "
            f"polygons={s.polygons} invalid={s.invalid} squares={s.squares} "
            f"max_stored={s.max_compressed} max_raw={s.max_raw}"
        )
    return outcomes, lines


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("archives", nargs="*", type=Path, help="PMTiles archive(s)")
    ap.add_argument(
        "--layer",
        help="MVT layer name every sampled tile must contain",
    )
    ap.add_argument(
        "--per-zoom",
        type=int,
        default=4,
        help="tiles to decode per zoom (default 4; first, last, evenly spaced)",
    )
    st = ap.add_argument_group("strict mode (#779)")
    st.add_argument("--strict", action="store_true", help="decode every tile; check invariants")
    st.add_argument(
        "--max-tile-size",
        type=int,
        default=DEFAULT_RAW_CAP,
        help=f"raw cap the archive was built with, bytes (default {DEFAULT_RAW_CAP}; 0 = none)",
    )
    st.add_argument(
        "--max-compressed-tile-size",
        type=int,
        default=DEFAULT_COMPRESSED_CAP,
        help=(
            f"compressed cap the archive was built with, bytes "
            f"(default {DEFAULT_COMPRESSED_CAP}; 0 = none)"
        ),
    )
    st.add_argument("--report", type=Path, help="the `tiles --report` JSON of the run")
    st.add_argument("--input", type=Path, help="the source parquet (rows, property columns)")
    st.add_argument(
        "--input-features",
        type=int,
        help="override the input count for accounting (e.g. after --bbox)",
    )
    st.add_argument(
        "--ids",
        action="store_true",
        help="MVT ids are source-unique (built with --feature-id): enables "
        "feature_accounting and monotone",
    )
    st.add_argument("--feature-id", help="the --feature-id column (not a property)")
    st.add_argument("--expect-decay", action="store_true", help="check placeholder decay")
    st.add_argument("--exemptions", type=Path, help="invariant_exemptions.toml")
    st.add_argument("--fixture", help="fixture label, for exemption matching")
    st.add_argument("--settings", help="settings label, for exemption matching")
    st.add_argument("--json-out", type=Path, help="write the outcomes as JSON")
    st.add_argument(
        "--workers",
        type=int,
        default=min(8, os.cpu_count() or 1),
        help="decode processes (default min(8, cpus))",
    )
    st.add_argument(
        "--check-exemptions",
        type=Path,
        metavar="TOML",
        help="validate an exemption file, then exit",
    )
    st.add_argument(
        "--check-issues-open",
        action="store_true",
        help="with --check-exemptions: every linked issue must be open (needs gh)",
    )
    st.add_argument(
        "--add-feature-ids",
        nargs=2,
        type=Path,
        metavar=("SRC", "DST"),
        help="write SRC with a row-index column named by --feature-id to DST, then exit",
    )
    args = ap.parse_args()

    if args.add_feature_ids:
        if not args.feature_id:
            ap.error("--add-feature-ids needs --feature-id")
        n = add_feature_ids(*args.add_feature_ids, args.feature_id)
        print(f"wrote {args.add_feature_ids[1]} ({n} rows, ids in {args.feature_id!r})")
        return 0

    if args.check_exemptions:
        rows = load_exemptions(args.check_exemptions)
        problems = check_exemption_rows(rows, args.check_exemptions)
        if args.check_issues_open and not problems:
            problems += check_issues_open(rows)
        for p in problems:
            print(f"FAIL {p}")
        if not problems:
            print(f"OK   {args.check_exemptions}: {len(rows)} exemption row(s)")
        return 1 if problems else 0

    if not args.archives:
        ap.error("no archives given")
    if not args.layer:
        ap.error("--layer is required")
    if args.per_zoom < 1:
        ap.error("--per-zoom must be >= 1")

    failures = []
    all_outcomes = []
    for path in args.archives:
        if not path.is_file():
            failures.append(f"{path}: not a file")
            continue
        try:
            if args.strict:
                outcomes, lines = verify_strict(path, args)
            else:
                lines = verify_archive(path, args.layer, args.per_zoom)
                outcomes = []
        except VerifyError as e:
            failures.append(f"{path}: {e}")
            print(f"FAIL {path}: {e}")
            continue
        except READER_ERRORS as e:
            # The pmtiles reader raises these on a malformed header,
            # directory or compressed block; report them per archive so
            # every archive on the command line gets a verdict.
            msg = f"pmtiles reader error ({type(e).__name__}): {e}"
            failures.append(f"{path}: {msg}")
            print(f"FAIL {path}: {msg}")
            continue
        bad = [o for o in outcomes if o.status in ("FAIL", "XPASS")]
        verdict = "FAIL" if bad else "OK  "
        print(f"{verdict} {path} (layer {args.layer!r}, {path.stat().st_size} bytes)")
        print("\n".join(lines))
        for o in outcomes:
            if o.status == "PASS" and o.zoom is not None:
                continue  # per-zoom passes are in the summary lines
            tag = f" #{o.issue}" if o.issue else ""
            print(f"  {o.status:<5} {o.invariant:<20} {o.where:<4}{tag} {o.detail}")
        for o in bad:
            failures.append(f"{path}: {o.status} {o.invariant} {o.where}: {o.detail}")
        all_outcomes += [
            {
                "archive": str(path),
                "fixture": args.fixture,
                "settings": args.settings,
                "invariant": o.invariant,
                "zoom": o.where,
                "status": o.status,
                "issue": o.issue,
                "detail": o.detail,
            }
            for o in outcomes
        ]

    if args.json_out:
        args.json_out.write_text(json.dumps(all_outcomes, indent=1) + "\n")
    if failures:
        print(f"\n{len(failures)} failure(s) in independent-reader verification:")
        for msg in failures:
            print(f"  - {msg}")
        return 1
    n = len(args.archives)
    mode = "every tile, strict invariants" if args.strict else "sampled tiles"
    print(f"\nverified {n} archive(s) with pmtiles + mapbox-vector-tile ({mode})")
    return 0


if __name__ == "__main__":
    sys.exit(main())
