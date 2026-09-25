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

**Regenerating changes the bytes.** Parquet encoding, compression and
statistics all depend on the pyarrow (and arrow-cpp) version, so re-running
this will produce a file that differs from the committed one even with no
change here. That is fine — the tests assert properties, not bytes — but do
not expect ``git diff`` to come back empty.
"""

import json
import math
import struct
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
    """Little-endian WKB for a Polygon with one ring (closed by the caller).

    ``struct.pack`` rather than poking at a pyarrow buffer: the buffer trick
    produced NATIVE-endian doubles inside a little-endian-declared WKB, so the
    fixture would have been silently corrupt on a big-endian host — and it
    depended on pyarrow's internal buffer layout for no gain.
    """
    out = bytearray()
    out += b"\x01"  # little endian
    out += (3).to_bytes(4, "little")  # Polygon
    out += (1).to_bytes(4, "little")  # one ring
    out += len(ring).to_bytes(4, "little")
    for x, y in ring:
        out += struct.pack("<dd", x, y)
    return bytes(out)


def _f32(v):
    """`v` as pyarrow will store it in a float32 column."""
    return struct.unpack("<f", struct.pack("<f", v))[0]


def f32_down(v):
    """A float32 <= v — the right rounding for a covering's MIN.

    Nudged by 1e-6 RELATIVE, which is ~16 float32 ulps (float32 carries about
    1.2e-7 relative precision), so the result is certainly below `v` while
    still being a change of ~1e-4 metres at these magnitudes. Widening a
    covering is free; shrinking one drops row groups a query really does
    reach.
    """
    f = _f32(v)
    return f if f <= v else _f32(f - abs(f) * 1e-6 - 1e-30)


def f32_up(v):
    """A float32 >= v — the right rounding for a covering's MAX."""
    f = _f32(v)
    return f if f >= v else _f32(f + abs(f) * 1e-6 + 1e-30)


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
                # The covering columns are float32, and the coordinates are
                # float64: round the envelope OUTWARD on the way down. A
                # covering is a conservative bound, and round-to-nearest can
                # shrink it — a reader that prunes against a too-small bbox
                # drops a row group whose features really do reach the query,
                # which in a sharded build is a tile missing geometry.
                "bbox": {
                    "xmin": f32_down(min(xs)),
                    "ymin": f32_down(min(ys)),
                    "xmax": f32_up(max(xs)),
                    "ymax": f32_up(max(ys)),
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
