# /// script
# requires-python = ">=3.10"
# dependencies = [
#   "pmtiles==3.8.1",
#   "mapbox-vector-tile==2.2.0",
# ]
# ///
"""Validate a tylertoo-produced PMTiles archive with independent readers (#421).

Nothing here uses tylertoo's own code. The archive is opened with the Python
`pmtiles` reader (header, metadata, directory walk) and a sample of tiles per
zoom is decoded with `mapbox-vector-tile`, so a self-consistent-but-wrong
header, directory or MVT framing is caught by readers that never saw our
writer. `pmtiles verify` (go-pmtiles) is the structural complement; this script
is the one that opens tile contents.

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

Usage:
  uv run scripts/verify_archive.py out.pmtiles --layer overview
  uv run scripts/verify_archive.py a.pmtiles b.pmtiles --layer overview --per-zoom 5

Exit status is 1 on any failure, with every failure listed.
"""

from __future__ import annotations

import argparse
import gzip
import json
import sys
from collections import defaultdict
from pathlib import Path

import mapbox_vector_tile
from pmtiles.reader import MmapSource, all_tiles
from pmtiles.tile import Compression, TileType, deserialize_header

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
HEADER_LEN = 127
SPEC_VERSION = 3
E7 = 1e7
LON_MAX = 180.0
LAT_MAX = 90.0


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
    if header["internal_compression"] == Compression.GZIP:
        raw = gzip.decompress(raw)
    elif header["internal_compression"] != Compression.NONE:
        raise VerifyError(
            f"internal compression {header['internal_compression'].name} unsupported"
        )
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


def decode_tile(z: int, x: int, y: int, raw: bytes, layer: str) -> int:
    """Decode one tile with mapbox-vector-tile; return its feature count in
    `layer`. Raises VerifyError for a missing layer or an invalid geometry."""
    try:
        decoded = mapbox_vector_tile.decode(raw, default_options={"geojson": True})
    except Exception as e:  # any decoder error is a failure
        raise VerifyError(f"z{z}/{x}/{y}: mapbox-vector-tile failed: {e}") from e
    if layer not in decoded:
        raise VerifyError(
            f"z{z}/{x}/{y}: layer {layer!r} missing (found {sorted(decoded)})"
        )
    features = decoded[layer].get("features", [])
    for i, feature in enumerate(features):
        check_feature(z, x, y, i, feature)
    return len(features)


def verify_archive(path: Path, layer: str, per_zoom: int) -> list[str]:
    """Run every check; return the per-zoom summary lines. Raises
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


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("archives", nargs="+", type=Path, help="PMTiles archive(s)")
    ap.add_argument(
        "--layer",
        required=True,
        help="MVT layer name every sampled tile must contain",
    )
    ap.add_argument(
        "--per-zoom",
        type=int,
        default=4,
        help="tiles to decode per zoom (default 4; first, last, evenly spaced)",
    )
    args = ap.parse_args()
    if args.per_zoom < 1:
        ap.error("--per-zoom must be >= 1")

    failures = []
    for path in args.archives:
        if not path.is_file():
            failures.append(f"{path}: not a file")
            continue
        try:
            lines = verify_archive(path, args.layer, args.per_zoom)
        except VerifyError as e:
            failures.append(f"{path}: {e}")
            print(f"FAIL {path}: {e}")
            continue
        print(f"OK   {path} (layer {args.layer!r}, {path.stat().st_size} bytes)")
        print("\n".join(lines))

    if failures:
        print(f"\n{len(failures)} archive(s) FAILED independent-reader verification:")
        for msg in failures:
            print(f"  - {msg}")
        return 1
    n = len(args.archives)
    print(f"\nverified {n} archive(s) with pmtiles + mapbox-vector-tile")
    return 0


if __name__ == "__main__":
    sys.exit(main())
