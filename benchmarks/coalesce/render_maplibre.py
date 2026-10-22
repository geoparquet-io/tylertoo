# /// script
# requires-python = ">=3.10"
# dependencies = [
#   "playwright==1.64.0", "pmtiles>=3.4", "shapely>=2.0", "geopandas>=1.0",
#   "pyarrow>=17", "numpy>=1.26", "pillow>=10", "rasterio>=1.4",
# ]
# ///
"""
render_maplibre.py — a window of several PMTiles archives drawn by MapLibre GL
JS in headless Chromium, the way a web map shows them, next to the source,
with what the tiles miss marked (#745).

Each archive is rendered twice at the same camera: once as a solid fill with
antialiasing off, which is the drawn mask, and once styled (pale fill, thin
edges) for the picture. The source is rasterized directly on the same pixel
grid. Magenta is source area the map leaves empty; orange is area the map
fills where the source has nothing. `maplibre.json` records each panel's
missing and added shares of the source area in the window.

    uvx --from playwright==1.64.0 playwright install chromium-headless-shell  # once
    uv run benchmarks/coalesce/render_maplibre.py --source src.parquet \\
        --zoom 12 --tile 4036,2500 --window 0.17,0,0.25 --draw-zoom 10,12 \\
        --label "today=a.pmtiles" --label "new=b.pmtiles" --out viz/

`--window fx,fy,size` is the window's top-left corner and side as fractions
of `--tile` at `--zoom`. For each `--draw-zoom` the camera is set so MapLibre
draws that zoom's tiles: at that zoom when the window is at least `--px`
pixels wide there, otherwise overzoomed from it (the source's `maxzoom` is
pinned to it).

MapLibre and pmtiles.js load from unpkg unless `--js-dir` names a directory
holding `maplibre-gl.js` and `pmtiles.js` (run.py fetches pinned copies
there), which are served to the page locally.
"""

from __future__ import annotations

import argparse
import json
import math
import sys
from pathlib import Path

WORLD = 20037508.342789244 * 2
WHITE = (255, 255, 255)
FILL = (200, 212, 226)
EDGE = (128, 142, 160)
MISSING = (226, 0, 122)
ADDED = (255, 150, 0)
MAX_CANVAS = 4096
MAPLIBRE = "https://unpkg.com/maplibre-gl@4.7.1/dist/maplibre-gl.js"
PMTILES = "https://unpkg.com/pmtiles@3.2.1/dist/pmtiles.js"

PAGE = """<!doctype html><html><head><meta charset="utf-8">
<script src="%(maplibre)s"></script><script src="%(pmtiles)s"></script>
<style>html,body{margin:0;background:#fff}#map{width:%(px)dpx;height:%(px)dpx}</style>
</head><body><div id="map"></div><script>
const protocol = new pmtiles.Protocol();
maplibregl.addProtocol("pmtiles", protocol.tile);
const cfg = %(cfg)s;
const layers = [];
for (const id of cfg.layers) {
  layers.push({id: "f-" + id, type: "fill", source: "a", "source-layer": id,
    paint: cfg.mask ? {"fill-color": "#000", "fill-antialias": false}
                    : {"fill-color": "rgb(200,212,226)", "fill-antialias": true}});
  if (!cfg.mask) layers.push({id: "l-" + id, type: "line", source: "a", "source-layer": id,
    paint: {"line-color": "rgb(128,142,160)", "line-width": 0.5}});
}
const map = new maplibregl.Map({
  container: "map", interactive: false, attributionControl: false, fadeDuration: 0,
  preserveDrawingBuffer: true, center: cfg.center, zoom: cfg.zoom,
  style: {version: 8, sources: {a: {type: "vector", url: "pmtiles://http://local/archive.pmtiles",
          minzoom: 0, maxzoom: cfg.maxzoom}},
          layers: [{id: "bg", type: "background", paint: {"background-color": "#fff"}}, ...layers]},
});
map.once("idle", () => { window.done = true; });
map.on("error", (e) => { window.err = String(e.error && e.error.message || e); });
</script></body></html>"""


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("--source", type=Path, required=True)
    ap.add_argument("--zoom", type=int, required=True, help="zoom of --tile")
    ap.add_argument("--tile", required=True, help="x,y of the tile the window is in")
    ap.add_argument("--window", default="0,0,1", help="fx,fy,size as tile fractions")
    ap.add_argument("--draw-zoom", help="zoom(s) whose tiles to draw, comma-separated")
    ap.add_argument("--label", action="append", required=True, help="name=archive.pmtiles")
    ap.add_argument("--px", type=int, default=768)
    ap.add_argument("--prefix", default="")
    ap.add_argument("--js-dir", type=Path, help="local maplibre-gl.js and pmtiles.js")
    ap.add_argument("--out", type=Path, required=True)
    args = ap.parse_args()

    import geopandas as gpd
    import numpy as np
    from PIL import Image
    from playwright.sync_api import sync_playwright
    from pmtiles.reader import MmapSource, Reader
    from shapely import STRtree
    from shapely.geometry import box

    tx, ty = (int(v) for v in args.tile.split(","))
    fx, fy, fs = (float(v) for v in args.window.split(","))
    w = WORLD / (1 << args.zoom)
    left = -WORLD / 2 + (tx + fx) * w
    top = WORLD / 2 - (ty + fy) * w
    side = fs * w
    view = box(left, top - side, left + side, top)
    cx, cy = left + side / 2, top - side / 2
    lng = cx / (WORLD / 2) * 180
    lat = math.degrees(math.atan(math.sinh(cy / (WORLD / 2) * math.pi)))

    src = gpd.read_parquet(args.source)
    if src.crs is None:
        src = src.set_crs(4326)
    src = src.to_crs(3857)
    sub = src.iloc[STRtree(src.geometry.values).query(view, predicate="intersects")]
    src_geoms = [g.intersection(view) for g in sub.geometry.values]
    src_geoms = [g for g in src_geoms if not g.is_empty]

    def source_mask(n: int):
        """The source on the canvas grid, by pixel centre (as the GPU fills),
        so a polygon is not a pixel wider here than on the map."""
        from affine import Affine
        from rasterio.features import rasterize

        if not src_geoms:
            return np.zeros((n, n), dtype=bool)
        transform = Affine(side / n, 0, left, 0, -side / n, top)
        return rasterize(((g, 1) for g in src_geoms), out_shape=(n, n), transform=transform,
                         fill=0, dtype="uint8", all_touched=False).astype(bool)

    def shrink(mask, size):
        """Any-pooling resize: a feature at least one output pixel's share wide shows."""
        if mask.shape[0] == size:
            return mask
        img = Image.fromarray(mask.astype(np.uint8) * 255).resize((size, size), Image.BOX)
        return np.asarray(img) > 0

    def canvas_for(dz: int) -> tuple[int, float]:
        at_dz = side / WORLD * 512 * 2**dz
        n = min(max(math.ceil(at_dz), args.px), MAX_CANVAS)
        return n, math.log2(n * WORLD / (side * 512))

    args.out.mkdir(parents=True, exist_ok=True)
    stats = {"window": {"zoom": args.zoom, "tile": [tx, ty], "fx": fx, "fy": fy, "size": fs,
                        "ground_m": side}, "source_polygons": len(sub), "images": {}}
    zooms = [int(v) for v in args.draw_zoom.split(",")] if args.draw_zoom else [args.zoom]

    scripts = {"maplibre": MAPLIBRE, "pmtiles": PMTILES}
    local_js = {}
    if args.js_dir:
        for key, fname in (("maplibre", "maplibre-gl.js"), ("pmtiles", "pmtiles.js")):
            scripts[key] = f"http://local/{fname}"
            local_js[scripts[key]] = (args.js_dir / fname).read_bytes()

    def serve_js(body: bytes):
        # Playwright passes (route, request) to a handler that takes two
        # arguments, so bind the body here rather than as a default.
        def handler(route):
            route.fulfill(status=200, body=body,
                          headers={"Content-Type": "application/javascript"})
        return handler

    with sync_playwright() as pw:
        browser = pw.chromium.launch(args=["--use-angle=swiftshader", "--enable-unsafe-swiftshader"])
        for dz in zooms:
            n, cam_zoom = canvas_for(dz)
            src_fill = source_mask(n)
            if not (args.out / f"{args.prefix}z{dz}-source.png").exists():
                img = np.full((n, n, 3), WHITE, np.uint8)
                img[src_fill] = FILL
                Image.fromarray(img).resize((args.px, args.px), Image.LANCZOS).save(
                    args.out / f"{args.prefix}z{dz}-source.png", optimize=True)
            for spec in args.label:
                name, _, path = spec.partition("=")
                data = Path(path)
                meta = Reader(MmapSource(open(data, "rb"))).metadata()
                layer_ids = [vl["id"] for vl in meta.get("vector_layers", [])]
                shots = {}
                for mask_mode in (True, False):
                    page = browser.new_page(viewport={"width": n, "height": n}, device_scale_factor=1)
                    fh = open(data, "rb")

                    def serve(route, request, fh=fh, size=data.stat().st_size):
                        rng = request.headers.get("range")
                        if rng:
                            a, b = rng.split("=")[1].split("-")
                            a = int(a)
                            b = min(int(b) if b else size - 1, size - 1)
                            fh.seek(a)
                            route.fulfill(status=206, body=fh.read(b - a + 1), headers={
                                "Content-Range": f"bytes {a}-{b}/{size}",
                                "Accept-Ranges": "bytes", "Access-Control-Allow-Origin": "*",
                                "Content-Type": "application/octet-stream"})
                        else:
                            fh.seek(0)
                            route.fulfill(status=200, body=fh.read(), headers={
                                "Access-Control-Allow-Origin": "*"})

                    cfg = {"layers": layer_ids, "mask": mask_mode, "center": [lng, lat],
                           "zoom": cam_zoom, "maxzoom": dz}
                    html = PAGE % {**scripts, "px": n, "cfg": json.dumps(cfg)}
                    for url, body in local_js.items():
                        page.route(url, serve_js(body))
                    page.route("http://local/archive.pmtiles", serve)
                    page.route("http://local/index.html",
                               lambda route: route.fulfill(status=200, body=html,
                                                           headers={"Content-Type": "text/html"}))
                    page.goto("http://local/index.html")
                    page.wait_for_function("window.done === true || window.err", timeout=600_000)
                    err = page.evaluate("window.err")
                    if err:
                        raise SystemExit(f"{name} z{dz}: MapLibre error: {err}")
                    png = page.locator("#map").screenshot()
                    page.close()
                    fh.close()
                    from io import BytesIO
                    shots[mask_mode] = np.asarray(Image.open(BytesIO(png)).convert("RGB"))
                drawn = shots[True].min(axis=2) < 128
                missing = src_fill & ~drawn
                added = drawn & ~src_fill
                pic = Image.fromarray(shots[False]).resize((args.px, args.px), Image.LANCZOS)
                pic = np.asarray(pic).copy()
                pic[shrink(added, args.px)] = ADDED
                pic[shrink(missing, args.px)] = MISSING
                Image.fromarray(pic).save(args.out / f"{args.prefix}z{dz}-{name}.png", optimize=True)
                ref = src_fill.sum()
                key = f"z{dz}-{name}"
                stats["images"][key] = {
                    "archive": path, "zoom": dz, "canvas_px": n, "camera_zoom": cam_zoom,
                    "missing_share": float(missing.sum() / ref) if ref else None,
                    "added_share": float(added.sum() / ref) if ref else None,
                }
                print(key, {k: round(v, 5) if isinstance(v, float) else v
                            for k, v in stats["images"][key].items() if k != "archive"}, flush=True)
        browser.close()

    (args.out / f"{args.prefix}maplibre.json").write_text(json.dumps(stats, indent=2))
    return 0


if __name__ == "__main__":
    sys.exit(main())
