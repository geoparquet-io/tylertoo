# /// script
# dependencies = ["pmtiles>=3.4", "mapbox-vector-tile>=2.1", "shapely>=2.0"]
# ///
"""
check_validity.py — count OGC-invalid polygon features in a PMTiles archive,
per zoom and by reason (#748).

    uv run benchmarks/coalesce/check_validity.py out.pmtiles
"""
import gzip, sys, collections
import mapbox_vector_tile
from pmtiles.reader import MmapSource, Reader, all_tiles
from shapely.geometry import shape
from shapely.validation import explain_validity
path = sys.argv[1]
src = MmapSource(open(path, "rb"))
reasons = collections.Counter(); per_zoom = collections.Counter(); total = collections.Counter(); example = {}
for (z, x, y), data in all_tiles(src):
    raw = gzip.decompress(data)
    for layer in mapbox_vector_tile.decode(raw, default_options={"y_coord_down": True}).values():
        for f in layer["features"]:
            g = f["geometry"]
            if g["type"] not in ("Polygon", "MultiPolygon"):
                continue
            total[z] += 1
            s = shape(g)
            if not s.is_valid:
                r = explain_validity(s).split("[")[0].strip()
                reasons[r] += 1; per_zoom[z] += 1
                example.setdefault(r, (z, x, y, g["type"], len(s.geoms) if g["type"] == "MultiPolygon" else 1))
print("invalid by reason:", dict(reasons))
print("invalid by zoom:", {z: f"{per_zoom[z]}/{total[z]}" for z in sorted(total)})
print("examples:", example)
