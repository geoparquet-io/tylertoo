#!/usr/bin/env bash
# verify_fixtures.sh <dir> [pins]: check every file pinned in
# benchmarks/fixtures.sha256 (the fixtures-v1 release assets) is in <dir> and
# matches its sha256. Exits nonzero on any mismatch or missing file, so a
# drifted or truncated download fails the job instead of moving the numbers.
#
#   gh release download fixtures-v1 --dir tests/fixtures/realdata/ --clobber
#   scripts/ci/verify_fixtures.sh tests/fixtures/realdata
set -euo pipefail

dir="${1:?usage: verify_fixtures.sh <dir> [pins]}"
pins="${2:-$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)/benchmarks/fixtures.sha256}"
pins="$(cd "$(dirname "$pins")" && pwd)/$(basename "$pins")"
[ -d "$dir" ] || { echo "verify_fixtures: no directory $dir" >&2; exit 1; }

if command -v sha256sum >/dev/null 2>&1; then
  check=(sha256sum --check --strict)
else
  check=(shasum -a 256 --check --strict)  # macOS
fi
cd "$dir"
"${check[@]}" "$pins"
