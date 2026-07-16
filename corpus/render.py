#!/usr/bin/env python3
"""V2 quality-evaluation render harness for tylertoo overviews.

For every DUPLICATING-mode overview file in corpus/data/overviews/ this
script renders one PNG per level (the full dataset extent) and, where a
tippecanoe golden PMTiles exists, one PNG per matching zoom decoded from
the golden. It then emits:

  - corpus/data/renders/<dataset>/level_<k>.png        (our overview)
  - corpus/data/renders/<dataset>/tippecanoe_z<z>.png  (golden baseline)
  - corpus/data/renders/index.html                     (contact sheet)
  - corpus/V2_METRICS.md                               (automated metrics)

Rendering is intentionally simple (matplotlib via geopandas, thin lines,
rasterized). Large levels are sampled down to MAX_PLOT features for the
plot only; counts/metrics always use the full report numbers.

Run through corpus/render.sh (wires the uv deps).
"""
import os
import sys
import json
import glob
import math
import argparse
from collections import Counter

import numpy as np
import pyarrow.parquet as pq
import shapely
from shapely.geometry import shape
import matplotlib

matplotlib.use("Agg")
import matplotlib.pyplot as plt  # noqa: E402

from pmtiles.reader import Reader, MmapSource  # noqa: E402
from pmtiles.tile import (  # noqa: E402
    deserialize_directory,
    tileid_to_zxy,
    Compression,
)
import gzip  # noqa: E402
import mapbox_vector_tile  # noqa: E402

HERE = os.path.dirname(os.path.abspath(__file__))
OVR = os.path.join(HERE, "data", "overviews")
GOLD = os.path.join(HERE, "data", "goldens", "tippecanoe")
RENDER = os.path.join(HERE, "data", "renders")
MANIFEST = json.load(open(os.path.join(HERE, "manifest.json")))
MANI_BY_ID = {d["id"]: d for d in MANIFEST["datasets"]}

MAX_PLOT = 200_000  # cap on features actually drawn per PNG
TILE_CAP = 5_000  # skip tippecanoe render at a zoom with more tiles than this
                  # (global monster sets have tens of thousands at high zoom;
                  #  decoding them all blows the wall-time budget)
FIG_W_IN = 16.0  # 16in * 100dpi = 1600px wide
DPI = 100
SEED = 1234

STYLE = {
    "point": dict(color="#1f5fa8", markersize=1.4),
    "line": dict(color="#111111", linewidth=0.3),
    "polygon": dict(color="#12203a", linewidth=0.2),
}


def dataset_kind(mid):
    d = MANI_BY_ID.get(mid, {})
    cls = d.get("class")
    if cls == "monster":
        gk = d.get("geometry_kind", "line")
        return "polygon" if gk == "polygon" else "line"
    return cls or "line"


def geom_col_of(path):
    sc = pq.read_schema(path)
    names = sc.names
    if "geometry" in names:
        return "geometry"
    if "geom" in names:
        return "geom"
    raise RuntimeError(f"no geometry column in {path}")


def covering_col_of(path, geomcol):
    names = pq.read_schema(path).names
    cand = f"{geomcol}_bbox"
    if cand in names:
        return cand
    if "bbox" in names:
        return "bbox"
    return None


def extent_of(path, geomcol, cov):
    """Full dataset bbox from the covering struct column (cheap, numeric)."""
    if cov is None:
        # fall back: read geometry, expensive; only tiny datasets reach here
        t = pq.read_table(path, columns=[geomcol])
        g = shapely.from_wkb(t[geomcol].to_numpy(zero_copy_only=False))
        xmin, ymin, xmax, ymax = shapely.total_bounds(g)
        return xmin, ymin, xmax, ymax
    t = pq.read_table(path, columns=[cov])
    st = t[cov].combine_chunks()
    xmin = np.asarray(st.field("xmin")).min()
    ymin = np.asarray(st.field("ymin")).min()
    xmax = np.asarray(st.field("xmax")).max()
    ymax = np.asarray(st.field("ymax")).max()
    return float(xmin), float(ymin), float(xmax), float(ymax)


def read_level_geoms(path, geomcol, level):
    t = pq.read_table(
        path, columns=[geomcol], filters=[("level", "==", level)]
    )
    if t.num_rows == 0:
        return np.array([], dtype=object)
    wkb = t[geomcol].to_numpy(zero_copy_only=False)
    return shapely.from_wkb(wkb)


def make_axes(extent):
    xmin, ymin, xmax, ymax = extent
    dx = (xmax - xmin) or 1e-6
    dy = (ymax - ymin) or 1e-6
    padx, pady = dx * 0.02, dy * 0.02
    xmin, xmax = xmin - padx, xmax + padx
    ymin, ymax = ymin - pady, ymax + pady
    latmid = (ymin + ymax) / 2.0
    coslat = max(math.cos(math.radians(latmid)), 1e-3)
    data_aspect = (dy / (dx * coslat))
    figh = min(max(FIG_W_IN * data_aspect, 4.0), 26.0)
    fig, ax = plt.subplots(figsize=(FIG_W_IN, figh), dpi=DPI)
    ax.set_xlim(xmin, xmax)
    ax.set_ylim(ymin, ymax)
    ax.set_aspect(1.0 / coslat)
    ax.set_axis_off()
    fig.subplots_adjust(left=0, right=1, top=1, bottom=0)
    return fig, ax


def _nan_separated(line_geoms):
    """Stack coords of many single-ring geoms with NaN gaps between them
    so they can be drawn as one fast rasterized Line2D."""
    coords, idx = shapely.get_coordinates(line_geoms, return_index=True)
    if len(coords) == 0:
        return np.array([]), np.array([])
    change = np.where(np.diff(idx) != 0)[0] + 1
    xs = np.insert(coords[:, 0], change, np.nan)
    ys = np.insert(coords[:, 1], change, np.nan)
    return xs, ys


def plot_geoms(ax, geoms, kind):
    """Fast rasterized render. Polygons drawn as boundaries (exteriors +
    holes); lines as polylines; points as markers. Large levels sampled
    to MAX_PLOT features for the plot only."""
    n = len(geoms)
    drawn = n
    if n == 0:
        ax.text(
            0.5, 0.5, "(empty level)", ha="center", va="center",
            transform=ax.transAxes, fontsize=20, color="#999",
        )
        return drawn
    if n > MAX_PLOT:
        rng = np.random.default_rng(SEED)
        geoms = geoms[rng.choice(n, MAX_PLOT, replace=False)]
        drawn = MAX_PLOT
    st = STYLE[kind]
    if kind == "point":
        coords = shapely.get_coordinates(geoms)
        ax.plot(
            coords[:, 0], coords[:, 1], linestyle="none", marker=".",
            markersize=st["markersize"], color=st["color"],
            markeredgewidth=0, rasterized=True,
        )
    elif kind == "line":
        parts = shapely.get_parts(geoms)  # explode multi -> single lines
        xs, ys = _nan_separated(parts)
        ax.plot(xs, ys, color=st["color"], linewidth=st["linewidth"],
                rasterized=True, solid_capstyle="round")
    else:  # polygon -> boundaries
        parts = shapely.get_parts(geoms)
        tids = shapely.get_type_id(parts)
        polys = parts[tids == 3]
        rings = [shapely.get_exterior_ring(polys)]
        nint = shapely.get_num_interior_rings(polys)
        maxint = int(nint.max()) if len(nint) else 0
        for i in range(maxint):
            sel = polys[nint > i]
            rings.append(shapely.get_interior_ring(sel, i))
        line_like = parts[(tids == 1) | (tids == 2)]  # stray (multi)lines
        allrings = list(rings)
        if len(line_like):
            allrings.append(line_like)
        ring_arr = np.concatenate([np.asarray(r, dtype=object) for r in allrings])
        xs, ys = _nan_separated(ring_arr)
        ax.plot(xs, ys, color=st["color"], linewidth=st["linewidth"],
                rasterized=True)
    return drawn


def save(fig, path):
    os.makedirs(os.path.dirname(path), exist_ok=True)
    fig.savefig(path, dpi=DPI, facecolor="white")
    plt.close(fig)


# ---------------- tippecanoe / PMTiles side ----------------

def enumerate_tiles(path):
    data = open(path, "rb").read()
    r = Reader(MmapSource(open(path, "rb")))
    h = r.header()
    out = []

    def walk(off, length):
        for e in deserialize_directory(data[off:off + length]):
            if e.run_length == 0:
                walk(h["leaf_directory_offset"] + e.offset, e.length)
            else:
                for i in range(e.run_length):
                    out.append(tileid_to_zxy(e.tile_id + i))

    walk(h["root_offset"], h["root_length"])
    return out, h, r


def mvt_transform_factory(z, x, y, extent):
    n = 2.0 ** z

    def to_lonlat(px, py):
        wx = (x + px / extent) / n
        wy = (y + py / extent) / n
        lon = wx * 360.0 - 180.0
        lat = math.degrees(math.atan(math.sinh(math.pi * (1.0 - 2.0 * wy))))
        return (lon, lat)

    return to_lonlat


def transform_coords(coords, fn):
    if not coords:
        return coords
    if isinstance(coords[0], (int, float)):
        return list(fn(coords[0], coords[1]))
    return [transform_coords(c, fn) for c in coords]


def feature_id(props, mvt_id):
    for k in ("id", "OGC_FID", "NE_ID"):
        if k in props and props[k] is not None:
            return ("p", props[k])
    return ("m", mvt_id)  # falls back to per-tile mvt id (may overcount)


def decode_zoom(tiles_by_z, h, r, z):
    """Decode every tile at zoom z. Counting distinct feature ids is cheap
    (no geometry work); shapely geometry (lon/lat) is built only for a
    sampled subset (<= MAX_PLOT) to keep global high-zoom monster sets
    tractable. Returns (geoms_for_render, distinct_id_count)."""
    tc = h["tile_compression"]
    ids = set()
    raw_feats = []  # (tx, ty, extent, geometry_dict)
    for (tx, ty) in tiles_by_z.get(z, []):
        raw = r.get(z, tx, ty)
        if raw is None:
            continue
        if tc == Compression.GZIP:
            raw = gzip.decompress(raw)
        try:
            dec = mapbox_vector_tile.decode(raw)
        except Exception:
            continue
        for lname, lay in dec.items():
            ext = lay.get("extent", 4096)
            for ft in lay["features"]:
                ids.add(feature_id(ft.get("properties", {}), ft.get("id")))
                raw_feats.append((tx, ty, ext, ft["geometry"]))
    n = len(raw_feats)
    if n > MAX_PLOT:
        rng = np.random.default_rng(SEED)
        chosen = (raw_feats[i] for i in rng.choice(n, MAX_PLOT, replace=False))
    else:
        chosen = raw_feats
    geoms = []
    for (tx, ty, ext, g) in chosen:
        fn = mvt_transform_factory(z, tx, ty, ext)
        gt = {"type": g["type"],
              "coordinates": transform_coords(g["coordinates"], fn)}
        try:
            geoms.append(shape(gt))
        except Exception:
            continue
    return np.array(geoms, dtype=object), len(ids)


# ---------------- driver ----------------

def area_km2(extent):
    xmin, ymin, xmax, ymax = extent
    latmid = (ymin + ymax) / 2.0
    w = (xmax - xmin) * 111.320 * math.cos(math.radians(latmid))
    hgt = (ymax - ymin) * 110.574
    return max(abs(w * hgt), 1e-6)


# ---------------- true-scale (actual on-screen size) ----------------
#
# A level with declared Web Mercator zoom z is meant to be *displayed* at that
# zoom. Our normal renders blow the dataset extent up to a fixed 1600px-wide
# figure, magnifying coarse levels ~50x beyond their intended display scale —
# which makes real sparsity indistinguishable from microscope artifacts. The
# true-scale mode renders the extent at the pixel size it would actually occupy
# on screen at zoom z: extent_fraction_of_world * 256px * 2^z (capped 1600px).

TRUESCALE_CAP_PX = 1600  # never render larger than this on a side
TRUESCALE_TINY_PX = 24   # below this, also emit a 4x supersample for legibility
TRUESCALE_SS = 4         # supersample factor for tiny levels


def _merc_world_px(z):
    return 256.0 * (2.0 ** z)


def _merc_project_fn(z):
    """Return a coord-array transform: (lon,lat) -> Web Mercator pixel at zoom z
    (world origin top-left, y increasing south). For shapely.transform."""
    world = _merc_world_px(z)

    def fn(coords):
        lon = coords[:, 0]
        lat = np.clip(coords[:, 1], -85.05112878, 85.05112878)
        x = (lon + 180.0) / 360.0 * world
        siny = np.sin(np.radians(lat))
        siny = np.clip(siny, -0.9999, 0.9999)
        y = (0.5 - np.log((1.0 + siny) / (1.0 - siny)) / (4.0 * math.pi)) * world
        return np.column_stack([x, y])

    return fn


def truescale_px(extent, z):
    """On-screen (width_px, height_px) of the extent at zoom z, uncapped."""
    xmin, ymin, xmax, ymax = extent
    fn = _merc_project_fn(z)
    corners = fn(np.array([[xmin, ymin], [xmax, ymax]]))
    w = abs(corners[1, 0] - corners[0, 0])
    # y grows south, so ymin(lat) -> larger y; height is the span.
    h = abs(corners[0, 1] - corners[1, 1])
    return max(w, 1.0), max(h, 1.0)


def render_truescale(geoms, kind, extent, z, out_png, supersample=1):
    """Render `geoms` (lon/lat shapely) at true on-screen scale for zoom z.

    Returns (out_w_px, out_h_px, tiny) where `tiny` is True when the natural
    size is below TRUESCALE_TINY_PX on both sides (caller emits a supersample).
    Coordinates are projected to Web Mercator pixels so shapes are correct and
    the figure's pixel dimensions equal the true display size (× supersample).
    """
    w_px, h_px = truescale_px(extent, z)
    tiny = max(w_px, h_px) < TRUESCALE_TINY_PX
    # Cap the long side at TRUESCALE_CAP_PX, preserving aspect.
    scale = min(1.0, TRUESCALE_CAP_PX / max(w_px, h_px))
    out_w = max(w_px * scale, 1.0)
    out_h = max(h_px * scale, 1.0)

    fn = _merc_project_fn(z)
    proj = shapely.transform(geoms, fn) if len(geoms) else geoms
    xmin, ymin, xmax, ymax = extent
    c = fn(np.array([[xmin, ymin], [xmax, ymax]]))
    x0, x1 = min(c[0, 0], c[1, 0]), max(c[0, 0], c[1, 0])
    y_bottom, y_top = max(c[0, 1], c[1, 1]), min(c[0, 1], c[1, 1])

    dpi = DPI * supersample
    fig, ax = plt.subplots(figsize=(out_w / DPI, out_h / DPI), dpi=dpi)
    ax.set_xlim(x0, x1)
    ax.set_ylim(y_bottom, y_top)  # y_bottom > y_top => north up
    ax.set_aspect("equal")
    ax.set_axis_off()
    fig.subplots_adjust(left=0, right=1, top=1, bottom=0)
    drawn = plot_geoms(ax, proj, kind)
    os.makedirs(os.path.dirname(out_png), exist_ok=True)
    fig.savefig(out_png, dpi=dpi, facecolor="white")
    plt.close(fig)
    return int(round(out_w * supersample)), int(round(out_h * supersample)), tiny, drawn


def _truescale_pair(geoms, kind, extent, z, out_base):
    """Render one true-scale PNG (and, when the natural size is tiny, a 4x
    supersampled companion). Returns (png_relpath, ss_relpath_or_None, w, h)."""
    png = f"{out_base}.png"
    w, h, tiny, _ = render_truescale(geoms, kind, extent, z, png, supersample=1)
    ss_rel = None
    if tiny:
        ss = f"{out_base}.4x.png"
        render_truescale(geoms, kind, extent, z, ss, supersample=TRUESCALE_SS)
        ss_rel = os.path.relpath(ss, RENDER)
    return os.path.relpath(png, RENDER), ss_rel, w, h


def run(only=None, true_scale=False):
    os.makedirs(RENDER, exist_ok=True)
    metrics = {}  # dataset -> list of level rows
    contact = {}  # dataset -> list of (level, zoom, our_png, tippe_png, fcount, tippe_fcount, drawn)
    tcontact = {}  # dataset -> list of true-scale rows

    dupfiles = sorted(glob.glob(os.path.join(OVR, "*.dup.parquet")))
    for path in dupfiles:
        mid = os.path.basename(path)[: -len(".dup.parquet")]
        if only and mid != only:
            continue
        report = json.load(
            open(os.path.join(OVR, f"{mid}.dup.report.json"))
        )
        kind = dataset_kind(mid)
        geomcol = geom_col_of(path)
        cov = covering_col_of(path, geomcol)
        extent = extent_of(path, geomcol, cov)
        akm2 = area_km2(extent)
        print(f"== {mid}  kind={kind} geom={geomcol} extent={extent}")

        # golden?
        gpath = os.path.join(GOLD, f"{mid}.pmtiles")
        have_golden = os.path.exists(gpath)
        tiles_by_z = h = r = None
        tippe_range = None
        if have_golden:
            tiles, h, r = enumerate_tiles(gpath)
            tiles_by_z = {}
            for (tz, tx, ty) in tiles:
                tiles_by_z.setdefault(tz, []).append((tx, ty))
            tippe_range = (h["min_zoom"], h["max_zoom"])
        # zoom -> dict(mag_png, fcount, skip, ts_png, ts_ss, ts_w, ts_h).
        # One decode per zoom feeds BOTH the magnified and true-scale panels, so
        # true-scale never decodes a tippecanoe zoom that magnified mode didn't.
        tippe_cache = {}

        rows = []
        crows = []
        tcrows = []
        canon_feat = report["levels"][-1]["feature_count"]
        for lv in report["levels"]:
            k = lv["level"]
            z = lv["zoom"]
            fcount = lv["feature_count"]
            vcount = lv["vertex_count"]
            cbytes = lv.get("compressed_bytes", 0)

            # our render (magnified)
            our_png = os.path.join(RENDER, mid, f"level_{k}.png")
            geoms = read_level_geoms(path, geomcol, k)
            fig, ax = make_axes(extent)
            drawn = plot_geoms(ax, geoms, kind)
            save(fig, our_png)
            print(f"   level {k} z{z}: {fcount} feats (drew {drawn})")

            # our render (true scale)
            ts_our_png = ts_our_ss = None
            ts_w = ts_h = None
            if true_scale:
                ts_our_png, ts_our_ss, ts_w, ts_h = _truescale_pair(
                    geoms, kind, extent,
                    z, os.path.join(RENDER, mid, f"ts_level_{k}"))
                print(f"      true-scale level {k}: {ts_w}x{ts_h}px"
                      f"{' (+4x)' if ts_our_ss else ''}")

            # tippecanoe render at matching zoom
            tippe_png = None
            tippe_fcount = None
            tippe_skip = None
            ts_tippe_png = ts_tippe_ss = None
            if have_golden and tippe_range[0] <= z <= tippe_range[1]:
                if z in tippe_cache:
                    c = tippe_cache[z]
                    tippe_png, tippe_fcount, tippe_skip = (
                        c["mag_png"], c["fcount"], c["skip"])
                    ts_tippe_png, ts_tippe_ss = c["ts_png"], c["ts_ss"]
                else:
                    ntiles = len(tiles_by_z.get(z, []))
                    if ntiles > TILE_CAP:
                        tippe_skip = ntiles
                        print(f"      tippe z{z}: SKIP ({ntiles} tiles > cap)")
                        tippe_cache[z] = dict(mag_png=None, fcount=None,
                                              skip=ntiles, ts_png=None,
                                              ts_ss=None)
                    else:
                        tgeoms, tcnt = decode_zoom(tiles_by_z, h, r, z)
                        tpng = os.path.join(
                            RENDER, mid, f"tippecanoe_z{z}.png")
                        fig, ax = make_axes(extent)
                        plot_geoms(ax, tgeoms, kind)
                        save(fig, tpng)
                        tippe_png, tippe_fcount = tpng, tcnt
                        if true_scale:
                            ts_tippe_png, ts_tippe_ss, _, _ = _truescale_pair(
                                tgeoms, kind, extent, z,
                                os.path.join(RENDER, mid, f"ts_tippecanoe_z{z}"))
                        tippe_cache[z] = dict(
                            mag_png=tpng, fcount=tcnt, skip=None,
                            ts_png=ts_tippe_png, ts_ss=ts_tippe_ss)
                        print(f"      tippe z{z}: {tcnt} distinct feats "
                              f"({ntiles} tiles)")

            if true_scale:
                tcrows.append(dict(
                    level=k, zoom=z, features=fcount, tippe_feat=tippe_fcount,
                    our_png=ts_our_png, our_ss=ts_our_ss,
                    tippe_png=ts_tippe_png, tippe_ss=ts_tippe_ss,
                    w=ts_w, h=ts_h,
                ))

            ratio = None
            if tippe_fcount:
                ratio = fcount / tippe_fcount
            attn = ratio is not None and (ratio > 3.0 or ratio < 1.0 / 3.0)
            rows.append(
                dict(
                    level=k, zoom=z, features=fcount, vertices=vcount,
                    mean_vpf=(vcount / fcount if fcount else 0),
                    band_bytes=cbytes,
                    feat_per_km2=fcount / akm2,
                    tippe_feat=tippe_fcount,
                    ratio=ratio, attention=attn,
                    dropped=1 - fcount / canon_feat if canon_feat else 0,
                )
            )
            crows.append(
                dict(
                    level=k, zoom=z,
                    our_png=os.path.relpath(our_png, RENDER),
                    tippe_png=(os.path.relpath(tippe_png, RENDER)
                               if tippe_png else None),
                    features=fcount, tippe_feat=tippe_fcount,
                    drawn=drawn, ratio=ratio, attention=attn,
                )
            )
        metrics[mid] = dict(rows=rows, area_km2=akm2, extent=extent,
                            have_golden=have_golden, kind=kind,
                            canon_feat=canon_feat)
        contact[mid] = crows
        if true_scale:
            tcontact[mid] = tcrows

    if not only:
        write_metrics(metrics)
        write_index(contact, metrics)
        if true_scale:
            write_truescale(tcontact, metrics)
    return metrics, contact


def write_metrics(metrics):
    out = os.path.join(HERE, "V2_METRICS.md")
    lines = []
    lines.append("# V2 Quality Metrics — automated\n")
    lines.append(
        "Generated by `corpus/render.py`. Per dataset x level: feature "
        "count, vertex count, mean vertices/feature, level-band compressed "
        "bytes (from the conversion reports), a features-per-km² density "
        "proxy (features / dataset-extent area), and the ratio of our level "
        "feature count vs tippecanoe's distinct-feature count at the "
        "matching zoom.\n"
    )
    lines.append(
        "Tippecanoe counts dedup on a feature id (`id`/`OGC_FID`/`NE_ID` "
        "property) so features split across tiles are counted once; where no "
        "such id exists the per-tile MVT id is used and the count may "
        "overcount. **Attention** rows are where our count is >3x or <1/3x "
        "tippecanoe's at the same zoom.\n"
    )
    # attention summary first
    attn_rows = []
    for mid, m in metrics.items():
        for row in m["rows"]:
            if row["attention"]:
                attn_rows.append((mid, row))
    lines.append("## Attention rows (ratio >3x or <1/3x tippecanoe)\n")
    if attn_rows:
        lines.append("| dataset | level | zoom | our feats | tippe feats | ratio |")
        lines.append("|---|--:|--:|--:|--:|--:|")
        for mid, row in attn_rows:
            lines.append(
                f"| {mid} | {row['level']} | {row['zoom']} | "
                f"{row['features']} | {row['tippe_feat']} | "
                f"{row['ratio']:.2f} |"
            )
    else:
        lines.append("_None._")
    lines.append("")

    for mid, m in metrics.items():
        lines.append(f"## {mid}\n")
        gnote = "" if m["have_golden"] else " (no tippecanoe golden)"
        lines.append(
            f"kind: {m['kind']} · extent area: {m['area_km2']:.1f} km² · "
            f"canonical features: {m['canon_feat']}{gnote}\n"
        )
        lines.append(
            "| level | zoom | features | vertices | mean v/f | band bytes | "
            "feat/km² | tippe feats | ratio | flag |"
        )
        lines.append("|--:|--:|--:|--:|--:|--:|--:|--:|--:|:-:|")
        for row in m["rows"]:
            ratio = f"{row['ratio']:.2f}" if row["ratio"] is not None else "-"
            tf = row["tippe_feat"] if row["tippe_feat"] is not None else "-"
            flag = "⚠️" if row["attention"] else ""
            lines.append(
                f"| {row['level']} | {row['zoom']} | {row['features']} | "
                f"{row['vertices']} | {row['mean_vpf']:.1f} | "
                f"{row['band_bytes']} | {row['feat_per_km2']:.2f} | {tf} | "
                f"{ratio} | {flag} |"
            )
        lines.append("")
    open(out, "w").write("\n".join(lines))
    print(f"wrote {out}")


def write_index(contact, metrics):
    out = os.path.join(RENDER, "index.html")
    h = []
    h.append("<!doctype html><html><head><meta charset='utf-8'>")
    h.append("<title>tylertoo V2 render contact sheet</title>")
    h.append(
        "<style>body{font-family:system-ui,sans-serif;margin:24px;"
        "background:#f7f7f8;color:#111}h1{font-size:22px}"
        "h2{margin-top:40px;border-bottom:2px solid #ccc;padding-bottom:4px}"
        ".row{display:flex;gap:12px;align-items:flex-start;margin:14px 0;"
        "padding:10px;background:#fff;border:1px solid #e0e0e0;border-radius:8px}"
        ".cell{flex:1}.cell img{width:100%;height:auto;border:1px solid #ddd;"
        "background:#fff}.cap{font-size:12px;color:#444;margin:4px 0}"
        ".lab{font-weight:600;font-size:13px}.attn{color:#b00;font-weight:700}"
        ".meta{font-size:13px;color:#555}.miss{color:#999;font-style:italic;"
        "padding:40px;text-align:center;border:1px dashed #ccc}"
        "</style></head><body>"
    )
    h.append("<h1>tylertoo overviews — V2 quality contact sheet</h1>")
    h.append(
        "<p class='meta'>Left = our duplicating-mode overview level. "
        "Right = tippecanoe golden decoded at the matching zoom, same extent. "
        "Rows go coarse &rarr; fine. Feature counts from the conversion "
        "reports; tippecanoe counts are distinct feature ids at that zoom. "
        "See <code>corpus/V2_REVIEW.md</code> for what to judge.</p>"
    )
    # toc
    h.append("<p class='meta'>Datasets: " + " · ".join(
        f"<a href='#{mid}'>{mid}</a>" for mid in contact) + "</p>")

    for mid, crows in contact.items():
        m = metrics[mid]
        h.append(f"<h2 id='{mid}'>{mid}</h2>")
        h.append(
            f"<p class='meta'>kind: {m['kind']} · extent "
            f"{m['area_km2']:.0f} km&sup2; · canonical {m['canon_feat']} "
            f"features · golden: {'yes' if m['have_golden'] else 'MISSING'}</p>"
        )
        for cr in crows:
            attn = " attn" if cr["attention"] else ""
            ratio = (f"{cr['ratio']:.2f}" if cr["ratio"] is not None else "n/a")
            drawnote = ""
            if cr["drawn"] < cr["features"]:
                drawnote = f" (drew {cr['drawn']:,} sampled)"
            h.append("<div class='row'>")
            h.append(
                f"<div class='cell'><div class='lab'>ours — level "
                f"{cr['level']} (z{cr['zoom']})</div>"
                f"<img src='{cr['our_png']}' loading='lazy'>"
                f"<div class='cap'>{cr['features']:,} features{drawnote}</div>"
                "</div>"
            )
            if cr["tippe_png"]:
                rcls = "attn" if cr["attention"] else ""
                h.append(
                    f"<div class='cell'><div class='lab'>tippecanoe — z"
                    f"{cr['zoom']}</div>"
                    f"<img src='{cr['tippe_png']}' loading='lazy'>"
                    f"<div class='cap'>{cr['tippe_feat']:,} distinct feats · "
                    f"<span class='{rcls}'>ratio {ratio}</span></div></div>"
                )
            else:
                h.append(
                    "<div class='cell'><div class='miss'>no tippecanoe tile "
                    "at this zoom</div></div>"
                )
            h.append("</div>")
    h.append("</body></html>")
    open(out, "w").write("\n".join(h))
    print(f"wrote {out}")


def write_truescale(tcontact, metrics):
    """Contact sheet at TRUE display scale: each panel is the size the level
    would occupy on screen at its zoom (capped 1600px). Tiny panels link to a
    4x supersample."""
    out = os.path.join(RENDER, "truescale.html")
    h = []
    h.append("<!doctype html><html><head><meta charset='utf-8'>")
    h.append("<title>tylertoo true-scale contact sheet</title>")
    h.append(
        "<style>body{font-family:system-ui,sans-serif;margin:24px;"
        "background:#f7f7f8;color:#111}h1{font-size:22px}"
        "h2{margin-top:40px;border-bottom:2px solid #ccc;padding-bottom:4px}"
        ".row{display:flex;gap:24px;align-items:flex-start;margin:14px 0;"
        "padding:10px;background:#fff;border:1px solid #e0e0e0;border-radius:8px}"
        ".cell{min-width:120px}.cell img{image-rendering:crisp-edges;"
        "border:1px solid #ddd;background:#fff}.cap{font-size:12px;color:#444;"
        "margin:4px 0}.lab{font-weight:600;font-size:13px}"
        ".meta{font-size:13px;color:#555}.miss{color:#999;font-style:italic;"
        "padding:20px;text-align:center;border:1px dashed #ccc}"
        "</style></head><body>"
    )
    h.append("<h1>tylertoo overviews — true-scale contact sheet</h1>")
    h.append(
        "<p class='meta'>Each image is rendered at the pixel size the dataset "
        "extent actually occupies on screen at that level's Web Mercator zoom "
        "(<code>extent_fraction_of_world × 256px × 2^z</code>, capped "
        f"{TRUESCALE_CAP_PX}px). This is the intended display scale — no "
        "magnification. Left = our level; right = tippecanoe at the matching "
        "zoom (reusing cached decodes; skipped where not decoded). Panels below "
        f"{TRUESCALE_TINY_PX}px link to a 4x supersample.</p>"
    )
    h.append("<p class='meta'>Datasets: " + " · ".join(
        f"<a href='#{mid}'>{mid}</a>" for mid in tcontact) + "</p>")

    def panel(lab, png, ss):
        if not png:
            return (f"<div class='cell'><div class='lab'>{lab}</div>"
                    "<div class='miss'>not decoded</div></div>")
        link_open = f"<a href='{ss}'>" if ss else ""
        link_close = "</a>" if ss else ""
        note = " · <a href='%s'>4x</a>" % ss if ss else ""
        return (f"<div class='cell'><div class='lab'>{lab}</div>"
                f"{link_open}<img src='{png}' loading='lazy'>{link_close}"
                f"<div class='cap'>true scale{note}</div></div>")

    for mid, rows in tcontact.items():
        m = metrics[mid]
        h.append(f"<h2 id='{mid}'>{mid}</h2>")
        h.append(
            f"<p class='meta'>kind: {m['kind']} · extent "
            f"{m['area_km2']:.0f} km&sup2; · canonical {m['canon_feat']} "
            f"features · golden: {'yes' if m['have_golden'] else 'MISSING'}</p>"
        )
        for cr in rows:
            dim = (f"{cr['w']}x{cr['h']}px" if cr["w"] else "")
            h.append("<div class='row'>")
            h.append(
                f"<div><div class='lab'>level {cr['level']} (z{cr['zoom']}) "
                f"· {dim}</div><div class='cap'>{cr['features']:,} features</div>"
                "</div>")
            h.append(panel(f"ours z{cr['zoom']}", cr["our_png"], cr["our_ss"]))
            tlab = (f"tippecanoe z{cr['zoom']}"
                    + (f" · {cr['tippe_feat']:,} feats"
                       if cr["tippe_feat"] else ""))
            h.append(panel(tlab, cr["tippe_png"], cr["tippe_ss"]))
            h.append("</div>")
    h.append("</body></html>")
    open(out, "w").write("\n".join(h))
    print(f"wrote {out}")


if __name__ == "__main__":
    ap = argparse.ArgumentParser()
    ap.add_argument("--only", help="render a single dataset id")
    ap.add_argument("--true-scale", action="store_true",
                    help="also render each level at true on-screen display "
                         "scale and write truescale.html")
    args = ap.parse_args()
    run(only=args.only, true_scale=args.true_scale)
