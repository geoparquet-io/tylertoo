#!/usr/bin/env bash
# Run the workspace diff gate shared by local runs and CI against a base ref.
set -euo pipefail
root=$(git rev-parse --show-toplevel)
cd "$root"
base=${1:-origin/main}
if [ "$#" -gt 0 ]; then shift; fi
mkdir -p target
mutation_diff=$(mktemp "$root/target/mutation-diff.XXXXXX")
trap 'rm -f "$mutation_diff"' EXIT
git diff --no-ext-diff --no-renames "$base" > "$mutation_diff"
# The CLI config has no package-specific build targets; the weekly core
# config does. Native workspace selection includes the I/O owner, Python
# bindings and future crates, while --in-diff limits the mutations. Run
# consumer tests as well: most I/O boundary regressions live in core.
# Pass --workspace through to Cargo so even the unmutated baseline builds
# all consumers (cargo-mutants otherwise baselines only changed packages).
cargo mutants --cargo-arg=--locked --workspace --all-features --cargo-arg=--workspace \
  --config .cargo/mutants-cli.toml --in-diff "$mutation_diff" "$@"
