#!/usr/bin/env python3
"""Storage table: on-disk size of each artifact per dataset.

Artifacts compared:
  gpio input           corpus/data/gpio/<id>.parquet
  overview duplicating corpus/data/bench/overviews/<id>.dup.parquet
  overview partitioning corpus/data/bench/overviews/<id>.par.parquet
  tippecanoe PMTiles   corpus/data/goldens/tippecanoe/<id>.pmtiles
  cogp (optional)      corpus/data/bench/cogp/<id>.parquet
  gpio + PMTiles total = status-quo deployment (source kept + derived tiles)

Overheads:
  dup_overhead  = dup / gpio - 1        (cost of embedding all levels)
  par_overhead  = par / gpio - 1        (level column + fresh covering)
  vs_statusquo  = dup / (gpio + pmtiles) - 1

Emits storage_results.json + a markdown table on stdout.
"""
import json
import os

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.abspath(os.path.join(HERE, "..", ".."))
D = os.path.join(ROOT, "corpus", "data")

DATASETS = [
    "points-nyc-medium",
    "lines-portland-medium",
    "polygons-portland-medium",
    "polygons-ftw-moldova-large",
]
# canonical feature counts (from V1_RESULTS)
FEATURES = {
    "points-nyc-medium": 458135,
    "lines-portland-medium": 295881,
    "polygons-portland-medium": 812435,
    "polygons-ftw-moldova-large": 631910,
}


def sz(path):
    return os.path.getsize(path) if os.path.exists(path) else None


def mb(b):
    return None if b is None else round(b / 1e6, 2)


def main():
    rows = {}
    for ds in DATASETS:
        gpio = sz(os.path.join(D, "gpio", ds + ".parquet"))
        dup = sz(os.path.join(D, "bench", "overviews", ds + ".dup.parquet"))
        par = sz(os.path.join(D, "bench", "overviews", ds + ".par.parquet"))
        pmt = sz(os.path.join(D, "goldens", "tippecanoe", ds + ".pmtiles"))
        cogp = sz(os.path.join(D, "bench", "cogp", ds + ".parquet"))
        statusquo = (gpio + pmt) if (gpio and pmt) else None
        n = FEATURES[ds]
        rows[ds] = {
            "features": n,
            "gpio_bytes": gpio,
            "dup_bytes": dup,
            "par_bytes": par,
            "pmtiles_bytes": pmt,
            "cogp_bytes": cogp,
            "statusquo_bytes": statusquo,
            "dup_overhead_pct": round((dup / gpio - 1) * 100, 1)
            if (dup and gpio) else None,
            "par_overhead_pct": round((par / gpio - 1) * 100, 1)
            if (par and gpio) else None,
            "dup_vs_statusquo_pct": round((dup / statusquo - 1) * 100, 1)
            if (dup and statusquo) else None,
            "dup_bytes_per_feature": round(dup / n, 1) if dup else None,
        }
    with open(os.path.join(HERE, "storage_results.json"), "w") as f:
        json.dump(rows, f, indent=2)

    # markdown
    hdr = ("| dataset | feats | gpio | ov-dup | ov-par | pmtiles | "
           "cogp | gpio+pmt | dup/gpio | par/gpio | dup vs status-quo |")
    sep = "|" + "---|" * 11
    print(hdr)
    print(sep)
    for ds in DATASETS:
        r = rows[ds]
        def m(k):
            v = mb(r[k])
            return f"{v} MB" if v is not None else "n/a"
        print(
            f"| {ds} | {r['features']:,} | {m('gpio_bytes')} | "
            f"{m('dup_bytes')} | {m('par_bytes')} | {m('pmtiles_bytes')} | "
            f"{m('cogp_bytes')} | {m('statusquo_bytes')} | "
            f"+{r['dup_overhead_pct']}% | +{r['par_overhead_pct']}% | "
            f"{'+' if (r['dup_vs_statusquo_pct'] or 0) >= 0 else ''}"
            f"{r['dup_vs_statusquo_pct']}% |"
        )


if __name__ == "__main__":
    main()
