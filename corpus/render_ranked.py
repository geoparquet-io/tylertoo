#!/usr/bin/env python3
"""Render ONLY our overview levels 0-8 for the class-ranked road datasets.

Q1 A/B helper: reuses corpus/render.py's plotting functions but skips the
tippecanoe decode entirely (the goldens are unchanged). Writes
corpus/data/renders/<dataset>/level_<k>_ranked.png for k in 0..=8, alongside
the existing level_<k>.png so a human can flip between them.

Run through the same uv wrapper as render.sh, e.g.:

  uv run --with pyarrow --with shapely --with matplotlib --with numpy \
         --with pmtiles --with mapbox-vector-tile \
         python3 corpus/render_ranked.py
"""
import os

import render as R  # corpus/render.py (same dir)

DATASETS = ["lines-boise-small", "lines-portland-medium"]
MAX_LEVEL = 8


def main():
    for mid in DATASETS:
        path = os.path.join(R.OVR, f"{mid}.dup.parquet")
        if not os.path.exists(path):
            print(f"!! missing {path}, skipping")
            continue
        kind = R.dataset_kind(mid)
        geomcol = R.geom_col_of(path)
        cov = R.covering_col_of(path, geomcol)
        extent = R.extent_of(path, geomcol, cov)
        print(f"== {mid} kind={kind} geom={geomcol}")
        for k in range(0, MAX_LEVEL + 1):
            geoms = R.read_level_geoms(path, geomcol, k)
            fig, ax = R.make_axes(extent)
            drawn = R.plot_geoms(ax, geoms, kind)
            out = os.path.join(R.RENDER, mid, f"level_{k}_ranked.png")
            R.save(fig, out)
            print(f"   level {k}: {len(geoms)} feats (drew {drawn}) -> {out}")


if __name__ == "__main__":
    main()
