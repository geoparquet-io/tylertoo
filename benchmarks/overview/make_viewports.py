#!/usr/bin/env python3
"""Derive the three canonical viewport rectangles per dataset.

Reproducible from the gpio inputs only (no hand-picked numbers):
  world    = full dataset extent, at the coarsest useful zoom
  regional = centered 1/4 of the linear extent (= 1/16 of area), mid zoom
  street   = fixed 0.02 deg box centered on the densest 0.02 deg cell,
             at the finest (canonical) zoom

Zoom choices are per dataset so the full extent fits one screenful at the
world zoom and an overview *level* exists at each chosen zoom. Emits
viewports.json consumed by bench_access.py. Same rectangles are used for
BOTH the overview and PMTiles paths (fairness).
"""
import json
import os
import subprocess
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.abspath(os.path.join(HERE, "..", ".."))
GPIO = os.path.join(ROOT, "corpus", "data", "gpio")

# zoom triples per dataset: (world, regional, street). Street is always the
# canonical/finest zoom (14). World = coarsest zoom whose tile spans ~ the
# whole extent; regional = a mid zoom. All chosen zooms exist as overview
# levels for the dataset (verified against V1_RESULTS level tables).
ZOOMS = {
    "points-nyc-medium": (8, 11, 14),
    "lines-portland-medium": (8, 11, 14),
    "polygons-portland-medium": (8, 11, 14),
    "polygons-ftw-moldova-large": (6, 9, 14),
}

STREET_DEG = 0.02  # fixed street box size (~1.5-2 km at these latitudes)


def dq(sql):
    out = subprocess.run(
        ["duckdb", "-noheader", "-list", "-c", sql],
        capture_output=True, text=True,
    )
    return out.stdout.strip().splitlines()[-1]


def extent(path):
    row = dq(
        "SELECT min(bbox.xmin)||','||min(bbox.ymin)||','||"
        "max(bbox.xmax)||','||max(bbox.ymax) "
        f"FROM read_parquet('{path}');"
    )
    return [float(x) for x in row.split(",")]


def densest_center(path):
    row = dq(
        "WITH c AS (SELECT "
        f"round((bbox.xmin+bbox.xmax)/2/{STREET_DEG})*{STREET_DEG} gx,"
        f"round((bbox.ymin+bbox.ymax)/2/{STREET_DEG})*{STREET_DEG} gy,"
        "count(*) n FROM read_parquet"
        f"('{path}') GROUP BY 1,2 ORDER BY n DESC LIMIT 1) "
        "SELECT gx||','||gy FROM c;"
    )
    gx, gy = [float(x) for x in row.split(",")]
    return gx, gy


def main():
    cfg = {}
    for ds, (zw, zr, zs) in ZOOMS.items():
        path = os.path.join(GPIO, ds + ".parquet")
        xmin, ymin, xmax, ymax = extent(path)
        cx, cy = (xmin + xmax) / 2, (ymin + ymax) / 2
        wx, wy = (xmax - xmin), (ymax - ymin)
        # regional: quarter linear extent centered -> half-width = extent/8
        rhx, rhy = wx / 8, wy / 8
        dgx, dgy = densest_center(path)
        h = STREET_DEG / 2
        cfg[ds] = {
            "extent": [xmin, ymin, xmax, ymax],
            "viewports": {
                "world": {"zoom": zw, "bbox": [xmin, ymin, xmax, ymax]},
                "regional": {
                    "zoom": zr,
                    "bbox": [cx - rhx, cy - rhy, cx + rhx, cy + rhy],
                },
                "street": {
                    "zoom": zs,
                    "bbox": [dgx - h, dgy - h, dgx + h, dgy + h],
                },
            },
        }
    out = os.path.join(HERE, "viewports.json")
    with open(out, "w") as f:
        json.dump(cfg, f, indent=2)
    print(f"wrote {out}")
    print(json.dumps(cfg, indent=2))


if __name__ == "__main__":
    main()
