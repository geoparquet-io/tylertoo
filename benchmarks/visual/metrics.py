"""
metrics.py — what a PMTiles archive draws, measured against its source data.

The one owner of the measurements (#869). benchmarks/coalesce/tile_metrics.py
and render_coverage.py take their reading, decoding and rasterizing from here,
and gate.py takes the per-tile errors below. Every error is 0 when the tile
draws exactly what the source has:

  polygons   missing        source pixels the tile does not draw / source pixels
             added          drawn pixels the source does not have / source pixels
             attr_mismatch  pixels drawn on both sides whose attribute differs
  lines      src_uncovered  source length further than LINE_TOL_PX from a drawn line
             drawn_stray    drawn length further than LINE_TOL_PX from a source line
                            (both over the source length)
  points     count_error    sum over a GRID x GRID grid of |drawn - source| / source

Decoded tile geometry is rasterized or measured directly: no GL render, so
no anti-aliasing and no style.
"""

from __future__ import annotations

import gzip
import hashlib
import json
import random
from pathlib import Path

import numpy as np

WORLD = 20037508.342789244 * 2
EXTENT_DEFAULT = 4096

PX = 256            # the gate's pixels per tile side; 2 px is 32 units at extent 4096
# A tile is gated only above these: a sliver of source in a tile scores near
# 1.0 whatever the tool does, and would hide a real defect behind it.
COVER_FLOOR = 0.01  # polygons: share of the tile's pixels the source covers
MIN_LENGTH_PX = 16  # lines: source length in the tile
MIN_POINTS = 8      # points: source points in the tile
LINE_TOL_PX = 1
GRID = 16

POLYGON, LINE, POINT = ("Polygon", "MultiPolygon"), ("LineString", "MultiLineString"), ("Point", "MultiPoint")
KINDS = {"polygon": POLYGON, "line": LINE, "point": POINT}


# --------------------------------------------------------------------------
# tiles and archives
# --------------------------------------------------------------------------


def tile_bounds_3857(z: int, x: int, y: int) -> tuple[float, float, float, float]:
    w = WORLD / (1 << z)
    x0 = -WORLD / 2 + x * w
    ytop = WORLD / 2 - y * w
    return x0, ytop - w, x0 + w, ytop


def tile_xy(xm: float, ym: float, z: int) -> tuple[int, int]:
    n = 1 << z
    return int((xm + WORLD / 2) / WORLD * n), int((WORLD / 2 - ym) / WORLD * n)


def colour_of(value) -> int:
    h = hashlib.blake2b(repr(value).encode(), digest_size=4).digest()
    return (int.from_bytes(h, "little") & 0x7FFFFFFF) or 1


def is_gzip(header: dict) -> bool:
    c = header.get("tile_compression")
    return c in (2, "gzip") or str(c).endswith("GZIP")


def read_archive(path: Path):
    """Every tile of the archive, {(z, x, y): stored bytes}, and whether they are gzipped."""
    from pmtiles.reader import MmapSource, Reader, all_tiles

    f = open(path, "rb")
    src = MmapSource(f)
    gz = is_gzip(Reader(src).header())
    tiles = {}
    for (z, x, y), data in all_tiles(src):
        tiles[(z, x, y)] = data
    return tiles, gz


def decode(data: bytes, gz: bool):
    import mapbox_vector_tile

    raw = gzip.decompress(data) if gz else data
    return raw, mapbox_vector_tile.decode(raw, default_options={"y_coord_down": True})


# --------------------------------------------------------------------------
# source
# --------------------------------------------------------------------------


class Source:
    """Source geometries in EPSG:3857, with an attribute value each (or None)."""

    def __init__(self, geoms, values=None):
        from shapely import STRtree

        self.geoms = np.array(geoms, dtype=object)
        self.values = values
        self.tree = STRtree(self.geoms)
        types = {g.geom_type for g in self.geoms}
        self.kind = next((k for k, t in KINDS.items() if types & set(t)), None)

    def colours(self):
        return None if self.values is None else [colour_of(str(v)) for v in self.values]


def read_source(path: Path, attr: str | None = None, types: tuple | None = None) -> Source:
    """The parquet's geometries in EPSG:3857, optionally only `types`.

    Read with pyarrow and shapely, not geopandas, which refuses a file without
    `geo` metadata (the Madagascar fixture).
    """
    import pyarrow.parquet as pq
    import shapely

    meta = pq.read_schema(path).metadata or {}
    geo = json.loads(meta.get(b"geo", b"{}") or b"{}")
    col = geo.get("primary_column", "geometry")
    crs = geo.get("columns", {}).get(col, {}).get("crs")
    code = (crs or {}).get("id", {}).get("code") if isinstance(crs, dict) else None
    # shortcut: lon/lat sources only, add a reprojection when a fixture needs one
    if code not in (None, 4326, "CRS84"):
        raise SystemExit(f"{path}: source CRS {code} is not lon/lat")
    table = pq.read_table(path, columns=[col] + ([attr] if attr else []))
    geoms = shapely.from_wkb(table.column(col).combine_chunks().to_numpy(zero_copy_only=False))
    values = table.column(attr).to_pylist() if attr else None
    keep = ~shapely.is_missing(geoms)
    if types:
        keep &= np.isin(shapely.get_type_id(geoms), [shapely.GeometryType[t.upper()].value for t in types])

    def mercator(c):
        x = c[:, 0] * (WORLD / 360)
        y = np.log(np.tan(np.pi / 4 + np.radians(c[:, 1]) / 2)) * (WORLD / (2 * np.pi))
        return np.column_stack([x, y])

    geoms = shapely.transform(geoms[keep], mercator)
    return Source(geoms, [v for v, k in zip(values, keep) if k] if attr else None)


def sample_tiles(src: Source, z: int, cap: int, seed: int = 869) -> list:
    """Up to `cap` tiles of zoom z that hold source vertices, chosen from the
    source alone so that both tools are scored on the same tiles."""
    import shapely

    c = np.concatenate([shapely.get_coordinates(src.geoms),
                        shapely.get_coordinates(shapely.point_on_surface(src.geoms))])
    n = 1 << z
    xs = np.clip(((c[:, 0] + WORLD / 2) / WORLD * n).astype(np.int64), 0, n - 1)
    ys = np.clip(((WORLD / 2 - c[:, 1]) / WORLD * n).astype(np.int64), 0, n - 1)
    tiles = sorted({(z, int(x), int(y)) for x, y in np.unique(np.column_stack([xs, ys]), axis=0)})
    # shortcut: a defect outside the sample is not seen, raise the cap for release checks
    return sorted(random.Random(seed).sample(tiles, cap)) if len(tiles) > cap else tiles


# --------------------------------------------------------------------------
# rasters
# --------------------------------------------------------------------------


def features(decoded: dict, types: tuple):
    """(shapely geometry, feature, layer extent) of every feature of `types`."""
    from shapely.geometry import shape

    for layer in decoded.values():
        extent = layer.get("extent", EXTENT_DEFAULT)
        for f in layer["features"]:
            if f["geometry"]["type"] in types:
                yield shape(f["geometry"]), f, extent


def tile_shapes(decoded: dict, color_attr: str | None, norm=None):
    """The tile's polygons, each with the colour of its attribute (or its id).
    `norm` maps the value first, to compare it with a source value."""
    out = []
    extent = EXTENT_DEFAULT
    for geom, f, extent in features(decoded, POLYGON):
        key = f["properties"].get(color_attr) if color_attr else None
        if key is None:
            key = f.get("id")
        out.append((geom, colour_of(norm(key) if norm else key)))
    return out, extent


def rasterize_tile(shapes, extent: int, px: int) -> np.ndarray:
    from affine import Affine
    from rasterio.features import rasterize

    if not shapes:
        return np.zeros((px, px), dtype=np.int32)
    # Tile coordinates, y down: pixel (col, row) -> (col * s, row * s).
    s = extent / px
    return rasterize(shapes, out_shape=(px, px), transform=Affine(s, 0, 0, 0, s, 0),
                     fill=0, dtype="int32")


def rasterize_source(tree, geoms, z: int, x: int, y: int, px: int, colours=None) -> np.ndarray:
    """The source in the tile: a coverage mask, or with `colours` (one per
    geometry) the colour raster a tile's is compared with."""
    from affine import Affine
    from rasterio.features import rasterize
    from shapely.geometry import box

    b = tile_bounds_3857(z, x, y)
    idx = tree.query(box(*b))
    dtype = "uint8" if colours is None else "int32"
    if len(idx) == 0:
        return np.zeros((px, px), dtype=bool if colours is None else np.int32)
    w = (b[2] - b[0]) / px
    arr = rasterize(((geoms[i], 1 if colours is None else colours[i]) for i in idx),
                    out_shape=(px, px), transform=Affine(w, 0, b[0], 0, -w, b[3]), fill=0, dtype=dtype)
    return arr.astype(bool) if colours is None else arr


def shares(fill: np.ndarray, ref: np.ndarray):
    """(missing, added): source pixels not drawn, and drawn pixels the source
    lacks, both as a share of the source pixels."""
    n = ref.sum()
    if not n:
        return None, None
    return float((ref & ~fill).sum() / n), float((fill & ~ref).sum() / n)


def pct(vals, q):
    return float(np.percentile(vals, q)) if len(vals) else None


def seams(seeds, colour: dict, ref: dict, px: int) -> dict:
    """Each seed tile against its right and lower neighbour, pooled.

    edge_gap: the share of source-covered pixels in a 2-pixel band on each
    side of the shared edge that neither tile draws. colour_change: the share
    of pixel pairs straddling the edge whose colours differ, both drawn;
    control_change: the same across a line inside the tile; excess_change is
    their difference. `worst` lists (colour_change, a, b) per seam.
    """
    edge_gap_num = edge_gap_den = 0
    diff_edge = both_edge = diff_ctl = both_ctl = 0
    worst = []
    mid = px // 2
    for (zz, x, y) in seeds:
        a = (zz, x, y)
        for b, axis in (((zz, x + 1, y), 1), ((zz, x, y + 1), 0)):
            if b not in colour or a not in colour:
                continue
            ca, cb = colour[a], colour[b]
            ra, rb = ref[a], ref[b]
            if axis == 1:  # a | b, vertical seam
                ea, eb = ca[:, -1], cb[:, 0]
                band_c = np.concatenate([ca[:, -2:], cb[:, :2]], axis=1)
                band_r = np.concatenate([ra[:, -2:], rb[:, :2]], axis=1)
                ctl_a, ctl_b = ca[:, mid - 1], ca[:, mid]
            else:  # a over b, horizontal seam
                ea, eb = ca[-1, :], cb[0, :]
                band_c = np.concatenate([ca[-2:, :], cb[:2, :]], axis=0)
                band_r = np.concatenate([ra[-2:, :], rb[:2, :]], axis=0)
                ctl_a, ctl_b = ca[mid - 1, :], ca[mid, :]
            edge_gap_num += int((band_r & (band_c == 0)).sum())
            edge_gap_den += int(band_r.sum())
            both = (ea > 0) & (eb > 0)
            d = int((both & (ea != eb)).sum())
            diff_edge += d
            both_edge += int(both.sum())
            bc = (ctl_a > 0) & (ctl_b > 0)
            diff_ctl += int((bc & (ctl_a != ctl_b)).sum())
            both_ctl += int(bc.sum())
            if both.sum():
                worst.append((d / both.sum(), a, b))
    cc = diff_edge / both_edge if both_edge else None
    ctl = diff_ctl / both_ctl if both_ctl else None
    return {
        "edge_gap": edge_gap_num / edge_gap_den if edge_gap_den else None,
        "colour_change": cc,
        "control_change": ctl,
        "excess_change": cc - ctl if cc is not None and ctl is not None else None,
        "worst": worst,
    }


# --------------------------------------------------------------------------
# per-tile errors against the source (the gate's metrics)
# --------------------------------------------------------------------------


def polygon_rasters(decoded: dict, src: Source, attr: str | None, z: int, x: int, y: int):
    """(drawn colour raster, source raster) of one tile at PX; the source
    raster holds colours when the source has attribute values."""
    shapes, extent = tile_shapes(decoded, attr, norm=str)
    return rasterize_tile(shapes, extent, PX), rasterize_source(src.tree, src.geoms, z, x, y, PX, src.colours())


def polygon_errors(decoded: dict, src: Source, attr: str | None, z: int, x: int, y: int,
                   rasters=None) -> dict | None:
    drawn, ref = rasters or polygon_rasters(decoded, src, attr, z, x, y)
    if (ref > 0).sum() < COVER_FLOOR * PX * PX:
        return None
    missing, added = shares(drawn > 0, ref > 0)
    out = {"missing": missing, "added": added}
    if src.values is not None and attr:
        both = (drawn > 0) & (ref > 0)
        out["attr_mismatch"] = float((both & (drawn != ref)).sum() / both.sum()) if both.sum() else None
    return out


def _in_tile_units(geom, z: int, x: int, y: int, extent: int):
    from shapely.affinity import affine_transform

    x0, _, x1, ytop = tile_bounds_3857(z, x, y)
    s = extent / (x1 - x0)
    return affine_transform(geom, [s, 0, 0, -s, -x0 * s, ytop * s])


def line_errors(decoded: dict, src: Source, attr: str | None, z: int, x: int, y: int) -> dict | None:
    from shapely import unary_union
    from shapely.geometry import box

    cell = box(*tile_bounds_3857(z, x, y))
    drawn_parts, extent = [], EXTENT_DEFAULT
    for geom, _, extent in features(decoded, LINE):
        drawn_parts.append(geom)
    ref = _in_tile_units(unary_union([src.geoms[i].intersection(cell) for i in src.tree.query(cell)]),
                         z, x, y, extent)
    px = extent / PX
    if ref.length < MIN_LENGTH_PX * px:
        return None
    drawn = unary_union(drawn_parts).intersection(box(0, 0, extent, extent))
    tol = LINE_TOL_PX * px
    return {"src_uncovered": float(ref.difference(drawn.buffer(tol)).length / ref.length),
            "drawn_stray": float(drawn.difference(ref.buffer(tol)).length / ref.length)}


def _cells(xy: np.ndarray, extent: float) -> np.ndarray:
    """Per-cell counts of the points inside [0, extent): a tile's buffer
    holds copies of its neighbours' points, which are not this tile's."""
    xy = xy[(xy >= 0).all(axis=1) & (xy < extent).all(axis=1)]
    ij = (xy * (GRID / extent)).astype(np.int64)
    return np.bincount(ij[:, 1] * GRID + ij[:, 0], minlength=GRID * GRID)


def point_errors(decoded: dict, src: Source, attr: str | None, z: int, x: int, y: int) -> dict | None:
    import shapely
    from shapely.geometry import box

    drawn_parts, extent = [], EXTENT_DEFAULT
    for geom, _, extent in features(decoded, POINT):
        drawn_parts.append(geom)
    cell = box(*tile_bounds_3857(z, x, y))
    near = [_in_tile_units(src.geoms[i], z, x, y, extent) for i in src.tree.query(cell)]
    ref = _cells(shapely.get_coordinates(near).reshape(-1, 2), extent)
    if ref.sum() < MIN_POINTS:
        return None
    drawn = _cells(shapely.get_coordinates(drawn_parts).reshape(-1, 2), extent)
    return {"count_error": float(np.abs(drawn - ref).sum() / ref.sum())}


def cluster_sum(decoded_tiles) -> int:
    """Points the tiles of one zoom stand for: `point_count` (1 when absent)
    of every point feature inside its tile's extent."""
    total = 0
    for decoded in decoded_tiles:
        for geom, f, extent in features(decoded, POINT):
            x, y = geom.coords[0] if geom.geom_type == "Point" else geom.geoms[0].coords[0]
            if 0 <= x < extent and 0 <= y < extent:
                total += int(f["properties"].get("point_count", 1))
    return total


ERRORS = {"polygon": polygon_errors, "line": line_errors, "point": point_errors}
