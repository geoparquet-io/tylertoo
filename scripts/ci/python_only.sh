#!/usr/bin/env bash
# Exit 0 when every changed path read from stdin is Python-side or
# documentation, 1 otherwise. CI uses it to skip build/test jobs (Test, Slow Tests,
# Coverage, Archive e2e, tippecanoe parity) on PRs that
# can't change a Rust build or its output (#756):
#
#   git diff --name-only HEAD^1 HEAD | scripts/ci/python_only.sh
#
# Same contract as docs_only.sh, which it extends: conservative on purpose,
# and an empty list is NOT Python-only. "Python-side" is everything under
# crates/python/ except the extension's Rust (src/ and its Cargo.toml): the
# package, its tests and scripts, pyproject.toml and uv.lock. Also
# examples/, the tutorials that crates/python/tests/docs runs in the Docs
# job. No Rust test reads any of these, and the e2e and parity scripts pin
# their own dependencies (PEP 723), not crates/python/uv.lock. The Python
# jobs, Check (clippy and duplicate detection), Lint (including cargo-deny),
# and Docs still run on these PRs.

set -euo pipefail

here=$(dirname "$0")
python_re='^(crates/python/|examples/)'
rust_re='^crates/python/(src/|Cargo\.toml$)'

seen=0
while IFS= read -r path; do
    [ -n "$path" ] || continue
    seen=1
    if [[ $path =~ $python_re ]] && ! [[ $path =~ $rust_re ]]; then
        continue
    fi
    if ! "$here/docs_only.sh" <<<"$path" 2>/dev/null; then
        echo "not Python-only: $path" >&2
        exit 1
    fi
done

[ "$seen" -eq 1 ]
