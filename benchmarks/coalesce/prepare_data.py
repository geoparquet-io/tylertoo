# /// script
# requires-python = ">=3.10"
# dependencies = ["pyarrow>=17", "numpy>=1.26"]
# ///
"""
prepare_data.py — build the #745 benchmark inputs in every row order, timing
each preparation step on its own.

The question #745's benchmarks ask about input order is not "is Hilbert
faster?" but "what does each order cost end to end, sort included?". So every
ordering step runs under /usr/bin/time and its wall time and peak RSS land in
`<out>/prep.json`, keyed by `<dataset>/<order>`. run_matrix.py reports totals
with and without them.

Orders:

  hilbert       gpio sort hilbert --add-bbox
  str           gpio sort str --add-bbox
  unsorted      a seeded row shuffle (pyarrow), bbox column kept, written with
                gpio's default 49,152-row groups
  as-delivered  the downloaded file, untouched (no prep cost)

gpio is pinned (GPIO below) and run through uvx, so the sort is reproducible.

    uv run benchmarks/coalesce/prepare_data.py --out ~/bench-data/745 \\
        --dataset moldova=~/bench-data/745/raw/moldova.parquet \\
        --orders hilbert,str,unsorted,as-delivered

For the scaling ladder, prefixes of a prepared file:

    uv run benchmarks/coalesce/prepare_data.py --prefixes \\
        ~/bench-data/745/mato-grosso/hilbert.parquet 250000,500000,1000000,2000000
"""

from __future__ import annotations

import argparse
import json
import re
import shutil
import subprocess
import sys
import time
from pathlib import Path

GPIO = "geoparquet-io==1.7.0"
ROW_GROUP_ROWS = 49_152
SHUFFLE_SEED = 745


def timed(argv: list[str]) -> dict:
    """Run argv under GNU time; return wall seconds, peak RSS (KiB), rc."""
    t0 = time.monotonic()
    proc = subprocess.run(
        ["/usr/bin/time", "-v", *argv], capture_output=True, text=True
    )
    wall = time.monotonic() - t0
    m = re.search(r"Maximum resident set size \(kbytes\):\s*(\d+)", proc.stderr)
    if proc.returncode != 0:
        sys.stderr.write(proc.stdout[-2000:] + proc.stderr[-4000:])
        raise SystemExit(f"failed ({proc.returncode}): {' '.join(argv)}")
    return {
        "argv": argv,
        "wall_s": round(wall, 3),
        "peak_rss_kib": int(m.group(1)) if m else None,
    }


def gpio(*args: str) -> list[str]:
    return ["uvx", "--from", GPIO, "gpio", *args]


def shuffle(src: Path, dst: Path) -> list[str]:
    """argv that writes a seeded row shuffle of src to dst (this script,
    re-entered in --shuffle mode, so it is timed like the gpio steps)."""
    return [
        "uv", "run", "-q", "--no-project", "--with", "pyarrow>=17", "--with", "numpy>=1.26",
        "python", "-I", str(Path(__file__).resolve()), "--shuffle", str(src), str(dst),
    ]


def do_shuffle(src: str, dst: str) -> None:
    import numpy as np
    import pyarrow.parquet as pq

    table = pq.read_table(src)
    perm = np.random.default_rng(SHUFFLE_SEED).permutation(table.num_rows)
    # Schema metadata (the `geo` key) survives `take` and `write_table`.
    pq.write_table(table.take(perm), dst, row_group_size=ROW_GROUP_ROWS, compression="zstd")


def ensure_geoparquet(name: str, src: Path, out: Path) -> Path:
    """src, or a copy with minimal GeoParquet 1.1 metadata when it has none
    (a WKB `geometry` column, OGC:CRS84, and a `geometry_bbox` covering if
    present), so gpio can read it. Not timed: it normalizes the source, it
    does not order it."""
    import pyarrow.parquet as pq

    schema = pq.read_schema(src)
    if schema.metadata and b"geo" in schema.metadata:
        return src
    col = {"encoding": "WKB", "geometry_types": []}
    if "geometry_bbox" in schema.names:
        col["covering"] = {"bbox": {k: ["geometry_bbox", k] for k in ("xmin", "ymin", "xmax", "ymax")}}
    geo = {"version": "1.1.0", "primary_column": "geometry", "columns": {"geometry": col}}
    table = pq.read_table(src)
    meta = dict(table.schema.metadata or {})
    meta[b"geo"] = json.dumps(geo).encode()
    dst = out / name / "source.parquet"
    dst.parent.mkdir(parents=True, exist_ok=True)
    pq.write_table(table.replace_schema_metadata(meta), dst, compression="zstd")
    print(f"[prep] {name}: added GeoParquet metadata -> {dst}", flush=True)
    return dst


def prepare(name: str, src: Path, order: str, out: Path) -> dict:
    dst = out / name / f"{order}.parquet"
    dst.parent.mkdir(parents=True, exist_ok=True)
    if dst.exists():
        dst.unlink()
    if order == "as-delivered":
        shutil.copyfile(src, dst)
        return {"argv": None, "wall_s": 0.0, "peak_rss_kib": None}
    if order == "hilbert":
        return timed(gpio("sort", "hilbert", "--add-bbox", "--row-group-size",
                          str(ROW_GROUP_ROWS), str(src), str(dst)))
    if order == "str":
        return timed(gpio("sort", "str", "--add-bbox", "--row-group-size",
                          str(ROW_GROUP_ROWS), str(src), str(dst)))
    if order == "unsorted":
        return timed(shuffle(src, dst))
    raise SystemExit(f"unknown order {order!r}")


def write_prefixes(src: Path, rows: list[int]) -> None:
    """The first N rows of src, for each N, as `<stem>-<N>.parquet` beside
    it: the scaling ladder. On a Hilbert-sorted file a prefix is a compact
    region, so density stays about the same while N grows."""
    import pyarrow.parquet as pq

    table = pq.read_table(src)
    for n in rows:
        dst = src.with_name(f"{src.stem}-{n}.parquet")
        pq.write_table(table.slice(0, min(n, table.num_rows)), dst,
                       row_group_size=ROW_GROUP_ROWS, compression="zstd")
        print(f"[prep] {dst.name}: {min(n, table.num_rows)} rows", flush=True)


def main() -> int:
    if len(sys.argv) == 4 and sys.argv[1] == "--shuffle":
        do_shuffle(sys.argv[2], sys.argv[3])
        return 0
    if len(sys.argv) == 4 and sys.argv[1] == "--prefixes":
        write_prefixes(Path(sys.argv[2]).expanduser(), [int(n) for n in sys.argv[3].split(",")])
        return 0
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("--out", type=Path, required=True)
    ap.add_argument("--dataset", action="append", required=True,
                    help="name=path to the source GeoParquet (repeatable)")
    ap.add_argument("--orders", default="hilbert,str,unsorted,as-delivered")
    args = ap.parse_args()
    out = args.out.expanduser()
    prep_path = out / "prep.json"
    prep = json.loads(prep_path.read_text()) if prep_path.exists() else {}
    for spec in args.dataset:
        name, _, path = spec.partition("=")
        src = ensure_geoparquet(name, Path(path).expanduser(), out)
        for order in args.orders.split(","):
            print(f"[prep] {name}/{order} ...", flush=True)
            rec = prepare(name, src, order, out)
            rec["bytes"] = (out / name / f"{order}.parquet").stat().st_size
            prep[f"{name}/{order}"] = rec
            print(f"[prep] {name}/{order}: {rec['wall_s']}s, "
                  f"{(rec['peak_rss_kib'] or 0) / 1024:.0f} MiB", flush=True)
            prep_path.write_text(json.dumps(prep, indent=2))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
