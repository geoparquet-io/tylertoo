#!/usr/bin/env bash
set -euo pipefail

# Build the full pyramid, z1 to z10, then check it against the spec.
tylertoo overview fieldmaps-madagascar-adm4-hilbert.parquet madagascar-ov.parquet \
  --min-zoom 1 \
  --max-zoom 10
tylertoo validate madagascar-ov.parquet
