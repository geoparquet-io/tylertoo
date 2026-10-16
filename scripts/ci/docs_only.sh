#!/usr/bin/env bash
# Exit 0 when every changed path read from stdin is documentation, 1
# otherwise. CI uses it to skip the Rust and Python jobs on docs-only PRs:
#
#   git diff --name-only HEAD^1 HEAD | scripts/ci/docs_only.sh
#
# On a pull_request checkout HEAD is the merge commit and HEAD^1 the base
# tip, so that diff is exactly what the PR changes (checkout needs
# fetch-depth: 2).
#
# Conservative on purpose: a path counts as docs only if no Rust or Python
# build, test, or doctest can read it. An empty list is NOT docs-only, so a
# diff that failed to list anything runs everything. The Docs and Docs
# Prose checks (ci.yml) still run on docs-only PRs: they are what tests
# these files.

set -euo pipefail

docs_re='^(docs/|context/|\.vale/|corpus/.*\.md$|[^/]+\.md$|mkdocs\.yml$|proselint\.json$)'

seen=0
while IFS= read -r path; do
    [ -n "$path" ] || continue
    seen=1
    if ! [[ $path =~ $docs_re ]]; then
        echo "not docs-only: $path" >&2
        exit 1
    fi
done

[ "$seen" -eq 1 ]
