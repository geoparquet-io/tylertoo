#!/usr/bin/env bash
set -euo pipefail

# One archive, two layers: density cells at z0-z7, fields at z8-z14.
# The a5 band counts the fields whose centroids fall in each A5 cell and
# sums their area. grid, h3 and s2 are the other cell systems.
tylertoo pyramid brazil.pmtiles \
  --band 0-7:a5:fields.parquet:density \
  --accumulate-attribute metrics:area:sum \
  --band 8-14:fields.pmtiles:fields
tylertoo stats brazil.pmtiles
