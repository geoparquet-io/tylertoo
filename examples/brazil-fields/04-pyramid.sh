#!/usr/bin/env bash
set -euo pipefail

# One archive, two layers: density cells at z0-z7, fields at z8-z14.
# The grid band counts the fields in each cell and sums their area.
tylertoo pyramid brazil.pmtiles \
  --band 0-7:grid:fields.parquet:density \
  --accumulate-attribute metrics:area:sum \
  --band 8-14:fields.pmtiles:fields
tylertoo stats brazil.pmtiles
