#!/usr/bin/env bash
set -euo pipefail

# Build a quick overview of Antananarivo only. --bbox reads just the row
# groups whose bounds touch the box (xmin,ymin,xmax,ymax in lon/lat).
tylertoo overview prepared.parquet preview-ov.parquet \
  --max-zoom 12 \
  --bbox=47.3,-19.1,47.7,-18.7
