#!/usr/bin/env bash
# Run the same direct cargo-mutants checks as CI against a base ref.
set -euo pipefail
root=$(git rev-parse --show-toplevel)
cd "$root"
base=${1:-origin/main}
if [ "$#" -gt 0 ]; then shift; fi
mkdir -p target
mutation_diff=$(mktemp "$root/target/mutation-diff.XXXXXX")
trap 'rm -f "$mutation_diff"' EXIT
git diff --no-ext-diff --no-renames "$base" -- crates/ > "$mutation_diff"
cargo mutants --cargo-arg=--locked --package tylertoo-core --config .cargo/mutants.toml --in-diff "$mutation_diff" "$@"
cargo mutants --cargo-arg=--locked --package tylertoo --config .cargo/mutants-cli.toml --in-diff "$mutation_diff" "$@"
