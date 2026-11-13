"""
mutate.py — copies of an archive with one known defect each, to test the
gate itself (#869): a gate that passes a broken archive is no gate.

    control    re-encoded, nothing changed: must still pass
    drop       5% of the features removed (with an attribute, the same
               features in every tile)
    shift      every geometry moved 2 pixels (of a 256-pixel tile) to the right
    simplify   every geometry simplified with a 4-pixel tolerance
    empty      one tile left out
    swap       the attribute values of neighbouring features exchanged

Only the tiles of the given zooms are written.
"""

from __future__ import annotations

import gzip
import hashlib
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import metrics as M  # noqa: E402

MUTANTS = ("control", "drop", "shift", "simplify", "empty", "swap")
DROP_ONE_IN = 20
SHIFT_PX = 2
SIMPLIFY_PX = 4


def _dropped(ident) -> bool:
    h = hashlib.blake2b(str(ident).encode(), digest_size=4).digest()
    return int.from_bytes(h, "little") % DROP_ONE_IN == 0


def _swap(feats: list, attr: str) -> int:
    """Exchange `attr` between features next to each other from left to
    right, where the values differ. Returns the number of pairs swapped."""
    order = sorted(range(len(feats)), key=lambda i: feats[i]["geometry"].centroid.x)
    n = 0
    for i, j in zip(order[::2], order[1::2]):
        a, b = feats[i]["properties"], feats[j]["properties"]
        if a.get(attr) != b.get(attr):
            a[attr], b[attr] = b.get(attr), a.get(attr)
            n += 1
    return n


def mutate(src: Path, dst: Path, kind: str, zooms, attr: str | None = None, empty=None) -> int:
    """Write `src`'s tiles of `zooms` to `dst` with the defect `kind`.
    Returns how many features (or tiles, or pairs) the defect touched."""
    import mapbox_vector_tile
    from mapbox_vector_tile.encoder import on_invalid_geometry_make_valid
    from pmtiles.tile import Compression, TileType, zxy_to_tileid
    from pmtiles.writer import write as pm_write
    from shapely.affinity import translate
    from shapely.geometry import shape

    tiles, gz = M.read_archive(src)
    keys = sorted((k for k in tiles if k[0] in zooms), key=lambda k: zxy_to_tileid(*k))
    touched = 0
    with pm_write(str(dst)) as w:
        for key in keys:
            if kind == "empty" and key == tuple(empty):
                touched += 1
                continue
            layers = []
            options = {"y_coord_down": True, "on_invalid_geometry": on_invalid_geometry_make_valid}
            for name, layer in M.decode(tiles[key], gz)[1].items():
                extent = layer.get("extent", M.EXTENT_DEFAULT)
                px = extent / M.PX
                feats = [{"geometry": shape(f["geometry"]), "properties": dict(f["properties"]),
                          **({"id": f["id"]} if f.get("id") is not None else {})}
                         for f in layer["features"]]
                if kind == "drop":
                    kept = [f for i, f in enumerate(feats)
                            if not _dropped(f["properties"].get(attr, f.get("id", i)))]
                    touched += len(feats) - len(kept)
                    feats = kept
                elif kind == "shift":
                    for f in feats:
                        f["geometry"] = translate(f["geometry"], xoff=SHIFT_PX * px)
                    touched += len(feats)
                elif kind == "simplify":
                    for f in feats:
                        f["geometry"] = f["geometry"].simplify(SIMPLIFY_PX * px)
                    touched += len(feats)
                elif kind == "swap":
                    touched += _swap(feats, attr)
                layers.append({"name": name, "features": [f for f in feats if not f["geometry"].is_empty]})
                options["extents"] = extent
            w.write_tile(zxy_to_tileid(*key), gzip.compress(
                mapbox_vector_tile.encode(layers, default_options=options)))
        zs = [k[0] for k in keys]
        w.finalize({
            "tile_type": TileType.MVT, "tile_compression": Compression.GZIP,
            "min_zoom": min(zs), "max_zoom": max(zs),
            "min_lon_e7": -1_800_000_000, "min_lat_e7": -850_000_000,
            "max_lon_e7": 1_800_000_000, "max_lat_e7": 850_000_000,
            "center_zoom": min(zs), "center_lon_e7": 0, "center_lat_e7": 0,
        }, {})
    if kind != "control" and not touched:
        raise SystemExit(f"mutant {kind!r} changed nothing in {src}")
    return touched
