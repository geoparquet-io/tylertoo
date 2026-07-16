#!/usr/bin/env python3
"""Create streaming test fixtures for tylertoo.

Run with: uv run python scripts/create_streaming_fixtures.py

This script creates three test fixtures:
1. multi-rowgroup-small.parquet - Small file with many row groups
2. unsorted.parquet - Shuffled row order
3. no-geo-metadata.parquet - Stripped geo extension metadata
"""

import random
from pathlib import Path

import pyarrow.parquet as pq

# Get project root (parent of scripts directory)
script_dir = Path(__file__).parent
project_root = script_dir.parent
fixtures_dir = project_root / "tests" / "fixtures"
streaming_dir = fixtures_dir / "streaming"
realdata_dir = fixtures_dir / "realdata"

# Ensure output directory exists
streaming_dir.mkdir(parents=True, exist_ok=True)

# Source file
source_file = realdata_dir / "open-buildings.parquet"

if not source_file.exists():
    print(f"Error: Source file not found: {source_file}")
    exit(1)

print(f"Reading source file: {source_file}")
table = pq.read_table(source_file)
print(f"  - {table.num_rows} rows, {table.num_columns} columns")

# 1. Multi-row-group: force tiny row groups (50 rows each)
print("\n1. Creating multi-rowgroup-small.parquet...")
multi_rg_path = streaming_dir / "multi-rowgroup-small.parquet"
pq.write_table(table, multi_rg_path, row_group_size=50)
# Verify row group count
pf = pq.ParquetFile(multi_rg_path)
print(f"   - Created with {pf.metadata.num_row_groups} row groups")

# 2. Unsorted: shuffle row order
print("\n2. Creating unsorted.parquet...")
indices = list(range(table.num_rows))
random.seed(42)  # deterministic
random.shuffle(indices)
shuffled = table.take(indices)
unsorted_path = streaming_dir / "unsorted.parquet"
pq.write_table(shuffled, unsorted_path)
print("   - Created with shuffled row order")

# 3. No geo metadata: strip geo extension
print("\n3. Creating no-geo-metadata.parquet...")
metadata = table.schema.metadata or {}
# Filter out any key containing 'geo' (case-insensitive)
stripped_metadata = {k: v for k, v in metadata.items() if b"geo" not in k.lower()}
new_schema = table.schema.with_metadata(stripped_metadata)
stripped = table.cast(new_schema)
no_geo_path = streaming_dir / "no-geo-metadata.parquet"
pq.write_table(stripped, no_geo_path)
print("   - Created without geo metadata")

# Verify the metadata was actually stripped
pf_no_geo = pq.ParquetFile(no_geo_path)
remaining_meta = pf_no_geo.schema_arrow.metadata or {}
geo_keys = [k for k in remaining_meta.keys() if b"geo" in k.lower()]
if geo_keys:
    print(f"   WARNING: Still has geo keys: {geo_keys}")
else:
    print("   - Verified: no geo metadata present")

print("\nDone! Created fixtures in:", streaming_dir)
