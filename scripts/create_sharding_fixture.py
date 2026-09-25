#!/usr/bin/env python3
"""Create the sharded-build test fixture for tylertoo (#498).

Run with: uv run python scripts/create_sharding_fixture.py

Writes ``tests/fixtures/streaming/sharding-grid.parquet``: a small, globally
distributed polygon grid whose row groups carry GeoParquet 1.1 **covering**
statistics.

Why a purpose-built fixture rather than one of the real-data ones:

* It has to span the world. A shard plan cuts the pivot zoom's Hilbert id
  space; a fixture confined to one country (every realdata fixture is) puts
  every tile in one or two shards and leaves the seams untested.
* It has to have **many row groups with usable bboxes**. That is what lets a
  shard prune its input, which in turn is what exercises the re-addressing of
  the shared convert plan onto a narrower row stream — the single most
  delicate part of a sharded build. ``fieldmaps-madagascar-adm4.parquet`` has
  one row group and no ``geo`` metadata at all, and
  ``multi-rowgroup-small.parquet`` declares no ``covering``, so neither one
  prunes.
* It has to be small enough to commit (~100 KB) and to tile in seconds.

Rows are sorted by longitude, so each row group is a narrow vertical band —
the shape a ``gpio``-optimized input has, and the shape that makes row-group
pruning bite.
"""

import json
import math
from pathlib import Path

import pyarrow as pa
import pyarrow.parquet as pq

project_root = Path(__file__).parent.parent
out_path = project_root / "tests" / "fixtures" / "streaming" / "sharding-grid.parquet"

# A 60 x 24 grid of small squares over the tiling domain, which is 1,440
# features: enough for several tiles per shard at the pivot without making the
# file big or the test slow.
COLS, ROWS = 60, 24
LON_MIN, LON_MAX = -175.0, 175.0
LAT_MIN, LAT_MAX = -70.0, 70.0
# Each square is a fraction of a cell, so features never touch and every tile's
# contents are unambiguous.
FILL = 0.45

ROW_GROUP_SIZE = 72  # -> 20 row groups


def wkb_polygon(ring):
    """Little-endian WKB for a Polygon with one ring (closed by the caller)."""
    out = bytearray()
    out += b"\x01"  # little endian
    out += (3).to_bytes(4, "little")  # Polygon
    out += (1).to_bytes(4, "little")  # one ring
    out += len(ring).to_bytes(4, "little")
    for x, y in ring:
        out += bytearray(memoryview(pa.array([x, y], type=pa.float64()).buffers()[1])[:16])
    return bytes(out)


def square(cx, cy, half_w, half_h):
    ring = [
        (cx - half_w, cy - half_h),
        (cx + half_w, cy - half_h),
        (cx + half_w, cy + half_h),
        (cx - half_w, cy + half_h),
        (cx - half_w, cy - half_h),
    ]
    return ring


records = []
dx = (LON_MAX - LON_MIN) / COLS
dy = (LAT_MAX - LAT_MIN) / ROWS
for j in range(ROWS):
    for i in range(COLS):
        cx = LON_MIN + dx * (i + 0.5)
        cy = LAT_MIN + dy * (j + 0.5)
        ring = square(cx, cy, dx * FILL / 2, dy * FILL / 2)
        xs = [p[0] for p in ring]
        ys = [p[1] for p in ring]
        records.append(
            {
                "cell_id": j * COLS + i,
                # A ranked attribute so thinning has something to order by,
                # and one that is NOT correlated with position.
                "weight": float((i * 7 + j * 13) % 97),
                "geometry": wkb_polygon(ring),
                "bbox": {
                    "xmin": min(xs),
                    "ymin": min(ys),
                    "xmax": max(xs),
                    "ymax": max(ys),
                },
            }
        )

# Sort by longitude so each row group is a narrow vertical band: that is what
# gives the row groups tight, prunable bboxes.
records.sort(key=lambda r: (r["bbox"]["xmin"], r["bbox"]["ymin"]))

schema = pa.schema(
    [
        pa.field("cell_id", pa.int64(), nullable=False),
        pa.field("weight", pa.float64(), nullable=False),
        pa.field("geometry", pa.binary(), nullable=False),
        pa.field(
            "bbox",
            pa.struct(
                [
                    pa.field("xmin", pa.float32(), nullable=False),
                    pa.field("ymin", pa.float32(), nullable=False),
                    pa.field("xmax", pa.float32(), nullable=False),
                    pa.field("ymax", pa.float32(), nullable=False),
                ]
            ),
            nullable=False,
        ),
    ]
)

geo = {
    "version": "1.1.0",
    "primary_column": "geometry",
    "columns": {
        "geometry": {
            "encoding": "WKB",
            "geometry_types": ["Polygon"],
            "crs": None,  # null => OGC:CRS84 (lon/lat), per the GeoParquet spec
            "bbox": [
                min(r["bbox"]["xmin"] for r in records),
                min(r["bbox"]["ymin"] for r in records),
                max(r["bbox"]["xmax"] for r in records),
                max(r["bbox"]["ymax"] for r in records),
            ],
            # THE point of this fixture: declare the bbox struct as the
            # covering, so every row group's footer carries a usable envelope.
            "covering": {
                "bbox": {
                    "xmin": ["bbox", "xmin"],
                    "ymin": ["bbox", "ymin"],
                    "xmax": ["bbox", "xmax"],
                    "ymax": ["bbox", "ymax"],
                }
            },
        }
    },
}

table = pa.Table.from_pylist(records, schema=schema).replace_schema_metadata(
    {"geo": json.dumps(geo)}
)

out_path.parent.mkdir(parents=True, exist_ok=True)
pq.write_table(
    table,
    out_path,
    row_group_size=ROW_GROUP_SIZE,
    compression="zstd",
    # The covering columns are only useful if their per-row-group statistics
    # are written; they are by default, but say so.
    write_statistics=True,
)

pf = pq.ParquetFile(out_path)
print(f"wrote {out_path}")
print(f"  {pf.metadata.num_rows} rows in {pf.metadata.num_row_groups} row groups")
print(f"  {out_path.stat().st_size / 1024:.1f} KiB")
rg0 = pf.metadata.row_group(0)
for i in range(rg0.num_columns):
    c = rg0.column(i)
    if c.path_in_schema.startswith("bbox."):
        print(f"  rg0 {c.path_in_schema}: {c.statistics.min} .. {c.statistics.max}")
assert math.isfinite(rg0.column(3).statistics.min)
