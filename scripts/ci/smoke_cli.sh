#!/usr/bin/env bash
# smoke_cli.sh: run every stage of a sharded build through the CLI an
# installed tylertoo wheel or sdist provides (#819), as `python -m tylertoo`
# and as the `tylertoo` script. Run it with that environment's `python` and
# scripts directory first on PATH, from any directory:
#
#   pip install --no-index --find-links dist tylertoo
#   scripts/ci/smoke_cli.sh
set -euo pipefail

input="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)/tests/fixtures/streaming/sharding-grid.parquet"
cd "$(mktemp -d)"

tylertoo --version
python -m tylertoo --help >/dev/null
python -m tylertoo -q tiles "$input" whole.pmtiles --max-zoom 4
python -m tylertoo shard-plan "$input" --shards 2 --pivot 3 -o shards.json
python -m tylertoo -q tiles "$input" coarse.pmtiles --max-zoom 4 \
  --shard coarse --shard-plan shards.json --save-plan convert.plan
for i in 0 1; do
  python -m tylertoo -q tiles "$input" "shard-$i.pmtiles" --max-zoom 4 \
    --shard "$i/2" --shard-plan shards.json --plan convert.plan
done
python -m tylertoo merge merged.pmtiles coarse.pmtiles shard-0.pmtiles shard-1.pmtiles
python -m tylertoo stats merged.pmtiles
# A usage error exits 2, as from the binary.
status=0
python -m tylertoo tiles 2>/dev/null || status=$?
[ "$status" = 2 ] || { echo "smoke_cli: a usage error exited $status, not 2" >&2; exit 1; }
echo "smoke_cli: OK"
