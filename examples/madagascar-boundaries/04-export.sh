#!/usr/bin/env bash
set -euo pipefail

# Cut the overview into vector tiles, then report tile weight per zoom.
tylertoo export-pmtiles madagascar-ov.parquet madagascar.pmtiles \
  --layer-name boundaries \
  --min-zoom 1
tylertoo stats madagascar.pmtiles
