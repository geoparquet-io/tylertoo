#!/usr/bin/env bash
# Print the test binaries cargo-mutants should build for tylertoo-core, in
# the form .cargo/mutants.toml's `additional_cargo_args` expects, and (with
# --check) fail if that file's list has drifted from the tree.
#
# Why a list exists at all: cargo-mutants relinks every test binary of the
# mutated package for every mutant, but nextest's `default-filter` only
# decides which tests *run*. The slow-tier binaries (shard_merge_parity,
# convert_guard_golden, ...) and the all-#[ignore] ones (debug_antarctica,
# huge_polygon_clip) were linked ~8200 times a sweep for nothing. Passing
# `--lib --test <name>...` to cargo restricts the build to binaries that
# have at least one test the mutation run would execute.
#
# The list is derived from nextest itself (`cargo nextest list --profile
# mutants --message-format json`): a binary is kept when it has a test that
# is not #[ignore] and matches the profile's default-filter. So marking a
# binary slow in .config/nextest.toml drops it here too; adding a new
# integration test binary adds it. Either way, rerun this script and paste
# its output into .cargo/mutants.toml (or run with --check to see whether
# you need to).
#
# Usage:
#   scripts/mutants-test-binaries.sh          # print the TOML fragment
#   scripts/mutants-test-binaries.sh --check  # exit 1 if mutants.toml differs
set -euo pipefail

cd "$(dirname "$0")/.."

for tool in cargo jq; do
  command -v "$tool" > /dev/null \
    || { echo "error: $tool is required" >&2; exit 2; }
done
cargo nextest --version > /dev/null 2>&1 \
  || { echo "error: cargo-nextest is required (cargo install cargo-nextest --locked)" >&2; exit 2; }

# One line per integration-test binary with a runnable test, sorted.
binaries=$(
  cargo nextest list --package tylertoo-core --profile mutants \
      --message-format json 2> /dev/null \
    | jq -r '
        .["rust-suites"][]
        | select(.kind == "test")
        | select([.testcases[]
                  | select(.ignored | not)
                  | select(.["filter-match"].status == "matches")]
                 | length > 0)
        | .["binary-name"]' \
    | sort
)

fragment=$(
  echo 'additional_cargo_args = ['
  echo '  "--lib",'
  while IFS= read -r b; do
    [ -n "$b" ] && printf '  "--test", "%s",\n' "$b"
  done <<< "$binaries"
  echo ']'
)

if [ "${1:-}" = "--check" ]; then
  # Compare against the array in .cargo/mutants.toml, ignoring comments and
  # blank lines inside it.
  current=$(
    sed -n '/^additional_cargo_args = \[/,/^\]/p' .cargo/mutants.toml \
      | sed -e 's/[[:space:]]*#.*$//' -e '/^[[:space:]]*$/d'
  )
  if [ "$current" = "$fragment" ]; then
    echo "ok: .cargo/mutants.toml additional_cargo_args matches the tree"
  else
    echo "error: .cargo/mutants.toml additional_cargo_args is out of date." >&2
    echo "Replace the array with:" >&2
    echo "$fragment" >&2
    exit 1
  fi
else
  echo "$fragment"
fi
