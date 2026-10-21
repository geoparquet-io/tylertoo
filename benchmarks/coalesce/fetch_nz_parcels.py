# /// script
# requires-python = ">=3.10"
# dependencies = ["pyarrow>=17", "shapely>=2.0"]
# ///
"""
fetch_nz_parcels.py — download LINZ NZ Parcels (layer 51571) as GeoParquet,
for the #745 benchmark.

Pages through the LINZ Data Service WFS in a stable order (`sortBy=id`) and
writes every page into one GeoParquet file (WKB geometry, OGC:CRS84).
Parcels with no geometry are skipped and counted. The API
key is read from `~/.linz_key` (create one at data.linz.govt.nz; a key with
query access is enough) and is never written anywhere else.

    uv run benchmarks/coalesce/fetch_nz_parcels.py ~/bench-data/745/raw/nz-parcels.parquet

Then order it with prepare_data.py like any other input.
"""

from __future__ import annotations

import json
import sys
import time
import urllib.request
from pathlib import Path

import pyarrow as pa
import pyarrow.parquet as pq
import shapely

LAYER = 51571
PAGE = 50_000
COLUMNS = ["id", "appellation", "affected_surveys", "parcel_intent", "topology_type",
           "statutory_actions", "land_district", "titles", "survey_area", "calc_area", "status"]


def page_url(key: str, start: int) -> str:
    return (f"https://data.linz.govt.nz/services;key={key}/wfs?service=WFS&version=2.0.0"
            f"&request=GetFeature&typeNames=layer-{LAYER}&count={PAGE}&startIndex={start}"
            f"&sortBy=id&srsName=EPSG:4326&outputFormat=application/json")


def fetch(key: str, start: int) -> list[dict]:
    for attempt in range(5):
        try:
            with urllib.request.urlopen(page_url(key, start), timeout=600) as r:
                return json.load(r)["features"]
        except Exception as e:  # noqa: BLE001 — retry any transient failure
            wait = 2 ** attempt
            print(f"[fetch] page at {start} failed ({e}); retrying in {wait}s", flush=True)
            time.sleep(wait)
    raise SystemExit(f"page at {start} failed 5 times")


def to_table(features: list[dict]) -> pa.Table:
    geoms = shapely.from_geojson([json.dumps(f["geometry"]) for f in features])
    cols = {c: [f["properties"].get(c) for f in features] for c in COLUMNS}
    data = {c: pa.array([None if v is None else str(v) for v in vals], pa.string())
            if c not in ("id", "survey_area", "calc_area") else
            pa.array(vals, pa.float64() if c != "id" else pa.int64())
            for c, vals in cols.items()}
    data["geometry"] = pa.array(shapely.to_wkb(geoms), pa.binary())
    return pa.table(data)


def main() -> int:
    out = Path(sys.argv[1]).expanduser()
    key = Path("~/.linz_key").expanduser().read_text().strip()
    geo = {"version": "1.1.0", "primary_column": "geometry",
           "columns": {"geometry": {"encoding": "WKB", "geometry_types": []}}}
    writer = None
    start, total, skipped, t0 = 0, 0, 0, time.monotonic()
    while True:
        features = fetch(key, start)
        if not features:
            break
        skipped += sum(f.get("geometry") is None for f in features)
        table = to_table([f for f in features if f.get("geometry") is not None])
        if writer is None:
            schema = table.schema.with_metadata({b"geo": json.dumps(geo).encode()})
            writer = pq.ParquetWriter(out, schema, compression="zstd")
        writer.write_table(table.replace_schema_metadata(writer.schema.metadata))
        total += len(features)
        start += PAGE
        print(f"[fetch] {total:,} parcels, {time.monotonic() - t0:.0f}s", flush=True)
        if len(features) < PAGE:
            break
    if writer:
        writer.close()
    print(f"[fetch] wrote {total - skipped:,} parcels to {out} "
          f"({skipped:,} with no geometry skipped)", flush=True)
    return 0


if __name__ == "__main__":
    sys.exit(main())
