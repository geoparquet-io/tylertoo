"""Build a multi-resolution overview file from a GeoParquet input."""

from pathlib import Path
from urllib.request import urlretrieve

import tylertoo

SRC = "fieldmaps-madagascar-adm4.parquet"
URL = f"https://github.com/geoparquet-io/tylertoo/releases/download/fixtures-v1/{SRC}"

if not Path(SRC).exists():
    urlretrieve(URL, SRC)

report = tylertoo.overview(SRC, "madagascar-ov.parquet", min_zoom=1, max_zoom=10)

print(f"{report['input_features']:,} features -> {report['total_rows']:,} rows")
print("zoom  level  features   vertices")
for lvl in report["levels"]:
    print(
        f"{lvl['zoom']:>4}  {lvl['level']:>5}"
        f"  {lvl['feature_count']:>8,}  {lvl['vertex_count']:>9,}"
    )
