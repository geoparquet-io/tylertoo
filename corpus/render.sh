#!/usr/bin/env bash
#
# render.sh - V2 quality-evaluation renders (see render.py docstring).
#
# Renders one PNG per overview level (duplicating mode) + one PNG per
# matching tippecanoe zoom from the goldens, then writes the contact
# sheet (data/renders/index.html) and corpus/V2_METRICS.md.
#
# Outputs land under corpus/data/renders/ (gitignored). Only the script,
# V2_METRICS.md and V2_REVIEW.md are committed.
#
# Deps are pulled per-run by uv (never bare python). Pass --only <id> to
# render a single dataset while iterating.
#
set -euo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

exec uv run \
  --with pyarrow \
  --with shapely \
  --with matplotlib \
  --with numpy \
  --with pmtiles \
  --with mapbox-vector-tile \
  python3 "$HERE/render.py" "$@"
