"""Validate the overview, then export it twice with different settings."""

from pathlib import Path

import tylertoo

OV = "madagascar-ov.parquet"

result = tylertoo.validate(OV)
failed = [c["name"] for c in result["checks"] if not c["passed"]]
print(f"valid={result['valid']}  checks={len(result['checks'])}  failed={failed}")

# The overview is the durable artifact: each export reuses it as built.
for name, extent in [("full", 4096), ("light", 1024)]:
    out = f"madagascar-{name}.pmtiles"
    report = tylertoo.export_pmtiles(
        OV, out, layer_name="boundaries", min_zoom=1, extent=extent
    )
    size = Path(out).stat().st_size
    print(
        f"{out}: z{report['min_zoom']}-z{report['max_zoom']},"
        f" {report['total_tiles']} tiles, {size:,} bytes"
    )
