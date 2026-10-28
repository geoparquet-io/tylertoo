# /// script
# requires-python = ">=3.11"
# dependencies = [
#   "pyarrow==26.0.0",
#   "geoarrow-pyarrow==0.2.0",
#   "shapely==2.2.0",
# ]
# ///
"""Generate the small fixtures behind the whole-output invariant tests (#779, G3).

Each one reaches a bug path no other fixture does:

  shape-column.parquet         open-buildings with its geometry column named
                               `SHAPE` (every Esri FileGDB layer's name) and
                               the `geo` key pointing at it (#675)
  geoparquet2-native.parquet   open-buildings as GeoParquet 2.0: the Parquet
                               GEOMETRY logical type, no `geo` key at all, so
                               the CRS is the logical type's default
                               (OGC:CRS84), declared, not assumed (#708)
  lon-0-360.parquet            small squares stored in the 0..360 longitude
                               convention across the antimeridian; with
                               lon-0-360.expected.json, the features a --bbox
                               must select under the export's wrap rule (#643)
  banded-parcels.parquet       synthetic parcels whose rows run west to east
                               in longitude bands, unsorted within a band:
                               the order behind #768's merge-valve strips
  banded-parcels-sorted.parquet  the same rows in Hilbert order
  nonfinite-vertex.parquet     200 open-buildings rows with one NaN and one
                               +inf vertex in the middle of a ring: features
                               export drops without a counter (#707)

Deterministic (fixed seeds, no timestamps), so a re-run reproduces the
committed bytes' content. Run from the repository root:

  uv run crates/core/tests/fixtures/invariants/make_fixtures.py
"""

from __future__ import annotations

import json
import math
import random
from pathlib import Path

import geoarrow.pyarrow as ga
import pyarrow as pa
import pyarrow.parquet as pq
import shapely

HERE = Path(__file__).resolve().parent
ROOT = HERE.parents[4]
OPEN_BUILDINGS = ROOT / "tests" / "fixtures" / "realdata" / "open-buildings.parquet"


def geo_metadata(column: str, types: list[str], bbox: list[float]) -> bytes:
    return json.dumps(
        {
            "version": "1.1.0",
            "primary_column": column,
            "columns": {
                column: {"encoding": "WKB", "geometry_types": types, "bbox": bbox}
            },
        }
    ).encode()


def write(table: pa.Table, name: str, **kw) -> None:
    path = HERE / name
    pq.write_table(table, path, compression="zstd", **kw)
    print(f"wrote {path.relative_to(ROOT)} ({path.stat().st_size} bytes, {table.num_rows} rows)")


def shape_column() -> None:
    t = pq.read_table(OPEN_BUILDINGS)
    md = dict(t.schema.metadata)
    geo = json.loads(md[b"geo"])
    col = geo["primary_column"]
    geo["columns"] = {"SHAPE": geo["columns"][col]}
    geo["primary_column"] = "SHAPE"
    md[b"geo"] = json.dumps(geo).encode()
    t = t.rename_columns(["SHAPE" if n == col else n for n in t.column_names])
    write(t.replace_schema_metadata(md), "shape-column.parquet")


def geoparquet2_native() -> None:
    t = pq.read_table(OPEN_BUILDINGS)
    geom = t.column("geometry").combine_chunks().cast(pa.binary())
    cols = {n: t.column(n) for n in t.column_names if n not in ("geometry", "bbox")}
    cols["geometry"] = ga.wkb().wrap_array(geom)
    # No `geo` key and no ARROW:schema: the logical type is the only
    # declaration (an empty crs is the spec's default, OGC:CRS84).
    write(pa.table(cols).replace_schema_metadata(None), "geoparquet2-native.parquet",
          store_schema=False)


# 0..360 squares: a 0.5 deg grid of 0.2 deg squares over lon 170..200,
# lat -10..10, stored with longitudes > 180 where they fall east of the
# antimeridian. The bbox below is in the -180..180 convention, so only the
# wrap rule (a feature matches when it or its copy shifted by +-360
# intersects the box) selects the stored-at-190..200 squares.
LON_BBOX = [-170.0, -5.0, -160.0, 5.0]


def lon_0_360() -> None:
    geoms, ids = [], []
    k = 0
    for i in range(60):  # lon 170.0 .. 199.5
        for j in range(40):  # lat -10.0 .. 9.5
            x, y = 170.0 + 0.5 * i + 0.15, -10.0 + 0.5 * j + 0.15
            geoms.append(shapely.box(x, y, x + 0.2, y + 0.2))
            ids.append(k)
            k += 1
    xmin, ymin, xmax, ymax = LON_BBOX

    def hits(g, shift):
        bx0, by0, bx1, by1 = g.bounds
        return bx0 + shift <= xmax and bx1 + shift >= xmin and by0 <= ymax and by1 >= ymin

    expected = sum(1 for g in geoms if any(hits(g, s) for s in (-360.0, 0.0, 360.0)))
    t = pa.table({"id": pa.array(ids, pa.int64()),
                  "geometry": pa.array(list(shapely.to_wkb(geoms)), pa.binary())})
    bounds = shapely.total_bounds(geoms).tolist()
    t = t.replace_schema_metadata({b"geo": geo_metadata("geometry", ["Polygon"], bounds)})
    write(t, "lon-0-360.parquet")
    (HERE / "lon-0-360.expected.json").write_text(
        json.dumps({"bbox": LON_BBOX, "expected_selected": expected,
                    "rule": "feature bbox, or it shifted by +-360, intersects bbox"},
                   indent=1) + "\n")
    print(f"  lon-0-360: {expected} features under the wrap rule in {LON_BBOX}")


def hilbert_d(n: int, x: int, y: int) -> int:
    """Index of (x, y) on an n x n Hilbert curve (n a power of two)."""
    d, s = 0, n // 2
    while s > 0:
        rx = 1 if x & s else 0
        ry = 1 if y & s else 0
        d += s * s * ((3 * rx) ^ ry)
        if ry == 0:
            if rx == 1:
                x, y = s - 1 - x, s - 1 - y
            x, y = y, x
        s //= 2
    return d


def banded_parcels() -> None:
    rng = random.Random(768)
    # A dense block of ~300 m parcels: 120 x 50 cells, jittered quads, over
    # lon 174.0..175.2, lat -37.5..-37.2 (Auckland-ish, where #768's dense
    # tiles were). Coordinates rounded to 1e-6 deg keep the file small.
    nx, ny, dx, dy = 120, 50, 0.01, 0.006
    rows = []
    for i in range(nx):
        for j in range(ny):
            x0, y0 = 174.0 + i * dx, -37.5 + j * dy
            pts = []
            for cx, cy in ((0, 0), (1, 0), (1, 1), (0, 1)):
                jx = rng.uniform(-0.15, 0.15) * dx
                jy = rng.uniform(-0.15, 0.15) * dy
                pts.append((round(x0 + cx * dx + jx, 6), round(y0 + cy * dy + jy, 6)))
            poly = shapely.Polygon(pts)
            if not poly.is_valid:
                poly = shapely.make_valid(poly)
            rows.append((i, j, poly))
    # Banded: west to east in 0.1 deg bands (10 columns), shuffled within a
    # band, which is "sorted enough" that no unsorted-input warning fires.
    band = {}
    for r in rows:
        band.setdefault(r[0] // 10, []).append(r)
    banded = []
    for b in sorted(band):
        chunk = band[b]
        rng.shuffle(chunk)
        banded += chunk
    hil = sorted(rows, key=lambda r: hilbert_d(128, r[0], r[1]))
    bounds = shapely.total_bounds([r[2] for r in rows]).tolist()
    for name, order in (("banded-parcels.parquet", banded),
                        ("banded-parcels-sorted.parquet", hil)):
        t = pa.table({
            "parcel_id": pa.array([r[0] * ny + r[1] for r in order], pa.int64()),
            "geometry": pa.array(list(shapely.to_wkb([r[2] for r in order])), pa.binary()),
        }).replace_schema_metadata(
            {b"geo": geo_metadata("geometry", ["Polygon"], bounds)})
        write(t, name, row_group_size=1000)


def nonfinite_vertex() -> None:
    t = pq.read_table(OPEN_BUILDINGS).slice(0, 200)
    geoms = list(shapely.from_wkb(t.column("geometry").to_pylist()))
    for row, value in ((10, math.nan), (20, math.inf)):
        coords = shapely.get_coordinates(geoms[row])
        coords[2] = [value, value]  # a middle vertex: the ring stays closed
        geoms[row] = shapely.polygons(coords)
    i = t.column_names.index("geometry")
    t = t.set_column(i, "geometry", pa.array(list(shapely.to_wkb(geoms)), pa.binary()))
    write(t, "nonfinite-vertex.parquet")


def main() -> None:
    shape_column()
    geoparquet2_native()
    lon_0_360()
    banded_parcels()
    nonfinite_vertex()


if __name__ == "__main__":
    main()
