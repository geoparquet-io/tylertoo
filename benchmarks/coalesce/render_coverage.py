# /// script
# requires-python = ">=3.10"
# dependencies = [
#   "pmtiles>=3.4", "mapbox-vector-tile>=2.1", "shapely>=2.0", "rasterio>=1.4",
#   "geopandas>=1.0", "pyarrow>=17", "numpy>=1.26", "pillow>=10",
# ]
# ///
"""
render_coverage.py — the source and several PMTiles archives, drawn over the
same block of tiles in the same style, with coverage errors marked (#745).

Every image shows polygons as a grey fill with dark outlines. On the archive
images, red is where the source has a polygon and the tiles draw nothing
(missing coverage), and blue is where the tiles draw a polygon and the source
has none (falsely added coverage). Coverage is compared pixel by pixel at the
image resolution, so a border moved by simplification shows as a thin
red/blue fringe and a dropped polygon as a red patch.

    uv run benchmarks/coalesce/render_coverage.py --source src.parquet \\
        --zoom 8 --label "today=a.pmtiles" --label "C2=b.pmtiles" --out viz/

The block is the `--span`×`--span` tiles centred on the tile with the most
source polygons, unless `--center x,y` is given. Writes one PNG per input and
`coverage.json` with each image's missing and added shares.
"""

from __future__ import annotations

import argparse
import gzip
import json
import sys
from pathlib import Path

import numpy as np

WORLD = 20037508.342789244 * 2
PX = 256  # pixels per tile


def tile_xy(xm: float, ym: float, z: int) -> tuple[int, int]:
    n = 1 << z
    return int((xm + WORLD / 2) / WORLD * n), int((WORLD / 2 - ym) / WORLD * n)


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("--source", type=Path, required=True)
    ap.add_argument("--zoom", type=int, required=True)
    ap.add_argument("--label", action="append", required=True, help="name=archive.pmtiles")
    ap.add_argument("--span", type=int, default=3)
    ap.add_argument("--center", help="x,y of the centre tile")
    ap.add_argument("--out", type=Path, required=True)
    ap.add_argument("--no-outline", action="store_true",
                    help="fill only, for features smaller than a pixel")
    args = ap.parse_args()

    import geopandas as gpd
    import mapbox_vector_tile
    from affine import Affine
    from PIL import Image
    from pmtiles.reader import MmapSource, Reader
    from rasterio.features import rasterize
    from shapely import STRtree
    from shapely.affinity import affine_transform
    from shapely.geometry import box, shape

    z = args.zoom
    src = gpd.read_parquet(args.source)
    if src.crs is None:
        src = src.set_crs(4326)
    src = src.to_crs(3857)
    src = src[src.geometry.geom_type.isin(["Polygon", "MultiPolygon"])]
    geoms = np.array(src.geometry.values)

    if args.center:
        cx, cy = (int(v) for v in args.center.split(","))
    else:
        cent = src.geometry.representative_point()
        keys, counts = np.unique([tile_xy(p.x, p.y, z) for p in cent], axis=0, return_counts=True)
        cx, cy = (int(v) for v in keys[counts.argmax()])
    half = args.span // 2
    x0, y0 = cx - half, cy - half
    w = WORLD / (1 << z)
    left, top = -WORLD / 2 + x0 * w, WORLD / 2 - y0 * w
    size = args.span * PX
    world = Affine(w / PX, 0, left, 0, -w / PX, top)
    extent_box = box(left, top - args.span * w, left + args.span * w, top)

    tree = STRtree(geoms)
    in_view = [geoms[i] for i in tree.query(extent_box)]
    src_fill = rasterize(((g, 1) for g in in_view), out_shape=(size, size), transform=world,
                         fill=0, dtype="uint8").astype(bool)
    src_line = rasterize(((g.boundary, 1) for g in in_view), out_shape=(size, size),
                         transform=world, fill=0, dtype="uint8", all_touched=True).astype(bool)

    def image(fill, line, ref=None):
        img = np.full((size, size, 3), 255, dtype=np.uint8)
        img[fill] = (200, 200, 200)
        if ref is not None:
            img[ref & ~fill] = (220, 30, 30)    # missing: source only
            img[fill & ~ref] = (30, 80, 220)    # added: tiles only
        if not args.no_outline:
            img[line] = (60, 60, 60)
        return img

    args.out.mkdir(parents=True, exist_ok=True)
    Image.fromarray(image(src_fill, src_line)).save(args.out / "source.png")
    stats = {"zoom": z, "tiles": {"x": [x0, x0 + args.span - 1], "y": [y0, y0 + args.span - 1]},
             "px_per_tile": PX, "source_cover": float(src_fill.mean()), "images": {}}

    for spec in args.label:
        name, _, path = spec.partition("=")
        f = open(path, "rb")
        reader = Reader(MmapSource(f))
        gz = reader.header().get("tile_compression") in (2, "gzip") or str(
            reader.header().get("tile_compression")).endswith("GZIP")
        shapes, lines, features, invalid = [], [], 0, 0
        for dx in range(args.span):
            for dy in range(args.span):
                data = reader.get(z, x0 + dx, y0 + dy)
                if not data:
                    continue
                raw = gzip.decompress(data) if gz else data
                for layer in mapbox_vector_tile.decode(raw, default_options={"y_coord_down": True}).values():
                    ext = layer.get("extent", 4096)
                    s = PX / ext
                    # Tile coordinates to image pixels, then to the world
                    # transform's coordinates so one raster pass draws all.
                    for feat in layer["features"]:
                        g = feat["geometry"]
                        if g["type"] not in ("Polygon", "MultiPolygon"):
                            continue
                        features += 1
                        geom = affine_transform(shape(g), [s, 0, 0, s, dx * PX, dy * PX])
                        geom = affine_transform(geom, [w / PX, 0, 0, -w / PX, left, top])
                        cell = box(left + dx * w, top - (dy + 1) * w, left + (dx + 1) * w, top - dy * w)
                        if not geom.is_valid:
                            invalid += 1
                            geom = geom.buffer(0)
                        shapes.append(geom.intersection(cell))
                        # Outline the polygon's own boundary, not the tile
                        # edge the clip adds, which would read as a seam.
                        lines.append(geom.boundary.intersection(cell))
        fill = rasterize(((g, 1) for g in shapes if not g.is_empty), out_shape=(size, size),
                         transform=world, fill=0, dtype="uint8").astype(bool) if shapes else \
            np.zeros((size, size), bool)
        line = rasterize(((g, 1) for g in lines if not g.is_empty),
                         out_shape=(size, size), transform=world, fill=0, dtype="uint8",
                         all_touched=True).astype(bool) if shapes else np.zeros((size, size), bool)
        Image.fromarray(image(fill, line, src_fill)).save(args.out / f"{name}.png")
        ref = src_fill.sum()
        stats["images"][name] = {
            "archive": path,
            "features_in_view": features,
            "invalid_features": invalid,
            "missing_share": float((src_fill & ~fill).sum() / ref) if ref else None,
            "added_share": float((fill & ~src_fill).sum() / ref) if ref else None,
        }
        print(name, stats["images"][name], flush=True)

    stats["source_polygons_in_view"] = len(in_view)
    (args.out / "coverage.json").write_text(json.dumps(stats, indent=2))
    print(json.dumps({k: v for k, v in stats.items() if k != "images"}), flush=True)
    return 0


if __name__ == "__main__":
    sys.exit(main())
