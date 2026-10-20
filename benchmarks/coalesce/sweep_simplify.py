# /// script
# requires-python = ">=3.10"
# dependencies = ["pyarrow>=17"]
# ///
"""
sweep_simplify.py — drawn-area share against `--simplify-factor` on a dense
polygon coverage (#745).

Builds a bbox subset of the input once, tiles it at each factor with the
default settings (plus the caps given), and scores the named tiles with
tile_metrics.py. Wall time and peak RSS come from GNU time. Writes one JSON
object per factor to `--out`.

    uv run benchmarks/coalesce/sweep_simplify.py \\
        --source ~/bench-data/745/nz-parcels/hilbert.parquet \\
        --bbox 174.5,-37.1,175.0,-36.6 \\
        --named 12/4036/2499,12/4035/2500,12/4036/2500 \\
        --factors 2,1,0.75,0.5,0.35,0.25,0.15,0.1,0.05,0 \\
        --bin target/release/tylertoo --work /tmp/sweep --out sweep.jsonl
"""

from __future__ import annotations

import argparse
import json
import re
import subprocess
import sys
import time
from pathlib import Path

HERE = Path(__file__).resolve().parent


def subset(source: Path, bbox: list[float], dst: Path) -> None:
    """Rows whose bbox covering intersects `bbox`, keeping the geo metadata."""
    import pyarrow.compute as pc
    import pyarrow.parquet as pq

    t = pq.read_table(source)
    b = t.column("bbox")
    keep = pc.and_(
        pc.and_(pc.less_equal(pc.struct_field(b, "xmin"), bbox[2]),
                pc.greater_equal(pc.struct_field(b, "xmax"), bbox[0])),
        pc.and_(pc.less_equal(pc.struct_field(b, "ymin"), bbox[3]),
                pc.greater_equal(pc.struct_field(b, "ymax"), bbox[1])),
    )
    pq.write_table(t.filter(keep), dst, row_group_size=49_152, compression="zstd")


def timed(argv: list[str]) -> dict:
    t0 = time.monotonic()
    p = subprocess.run(["/usr/bin/time", "-v", *argv], capture_output=True, text=True)
    if p.returncode != 0:
        raise SystemExit(f"failed: {' '.join(argv)}\n{p.stderr[-2000:]}")
    m = re.search(r"Maximum resident set size \(kbytes\):\s*(\d+)", p.stderr)
    return {"wall_s": round(time.monotonic() - t0, 2), "peak_rss_kib": int(m.group(1)) if m else None}


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("--source", type=Path, required=True)
    ap.add_argument("--bbox", required=True)
    ap.add_argument("--named", required=True)
    ap.add_argument("--factors", required=True)
    ap.add_argument("--bin", type=Path, required=True)
    ap.add_argument("--work", type=Path, required=True)
    ap.add_argument("--out", type=Path, required=True)
    ap.add_argument("--max-zoom", type=int, default=14)
    ap.add_argument("--zoom", type=int, default=12, help="zoom of the named tiles")
    ap.add_argument("--caps", default="--max-tile-size 0 --max-compressed-tile-size 2000000")
    args = ap.parse_args()
    bbox = [float(v) for v in args.bbox.split(",")]
    args.work.mkdir(parents=True, exist_ok=True)
    sub = args.work / "subset.parquet"
    if not sub.exists():
        subset(args.source.expanduser(), bbox, sub)
    with args.out.open("w") as out:
        for f in args.factors.split(","):
            pm = args.work / f"sf{f}.pmtiles"
            run = timed([str(args.bin), "tiles", str(sub), str(pm), "--min-zoom", "0",
                         "--max-zoom", str(args.max_zoom), "--simplify-factor", f, "--force",
                         *args.caps.split()])
            metrics = args.work / f"sf{f}.metrics.json"
            subprocess.run(["uv", "run", "-q", str(HERE / "tile_metrics.py"), str(pm),
                            "--source", str(sub), "--zooms", str(args.zoom), "--max-tiles", "20",
                            "--named", args.named, "--raw-limit", "0", "--gz-limit", "0",
                            "--json", str(metrics)], capture_output=True, check=True)
            m = json.loads(metrics.read_text())
            named = m["named"]
            rec = {"simplify_factor": float(f), **run, "pmtiles_bytes": pm.stat().st_size,
                   "named": named,
                   "named_mean": sum(named.values()) / len(named) if named else None,
                   "zoom_median": m["zooms"][str(args.zoom)]["drawn_share"]["median"]}
            out.write(json.dumps(rec) + "\n")
            out.flush()
            print(f"sf {f}: named mean {rec['named_mean']:.4f} "
                  f"({', '.join(f'{v:.4f}' for v in named.values())}), {run['wall_s']}s, "
                  f"{(run['peak_rss_kib'] or 0) / 1048576:.2f} GiB, "
                  f"{rec['pmtiles_bytes'] / 1e6:.0f} MB", flush=True)
    return 0


if __name__ == "__main__":
    sys.exit(main())
