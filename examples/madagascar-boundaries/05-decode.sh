#!/usr/bin/env bash
set -euo pipefail

# Read the z8 tiles back out as GeoParquet.
tylertoo decode madagascar.pmtiles z8.parquet --zoom 8
