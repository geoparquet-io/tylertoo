#!/usr/bin/env bash
set -euo pipefail

# One archive, two layers: density cells at z0-z7, fields at z8-z14.
tylertoo pyramid brazil.pmtiles \
  --band 0-7:density.parquet:density \
  --band 8-14:fields.pmtiles:fields
tylertoo stats brazil.pmtiles
