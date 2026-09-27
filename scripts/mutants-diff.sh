#!/usr/bin/env bash
# Diff-scoped mutation testing: run cargo-mutants only on the code your
# branch touches. The local twin of .github/workflows/mutation-diff.yml.
#
# Usage:
#   scripts/mutants-diff.sh                    # vs origin/main
#   scripts/mutants-diff.sh main               # vs another base ref
#   scripts/mutants-diff.sh origin/main -- -j 2  # extra cargo-mutants args
#
# What it does:
#   1. Diffs the working tree (committed + uncommitted) against the
#      merge-base with BASE_REF, restricted to *.rs under crates/, and
#      writes that diff to a temp file.
#   2. Runs `cargo mutants --in-diff <file> --output <repo root>` with
#      the repo's .cargo/mutants.toml (nextest, `quick` tier, timeouts),
#      so only mutants inside changed lines are generated and tested.
#   3. Prints the score and the missed mutants (file:line: description).
#
# Exit codes: 0 all mutants caught (or nothing to mutate), 1 at least one
# mutant missed, 2 usage / missing tool, 4 unmutated baseline failed
# (cargo-mutants' own code; nothing was measured), other = cargo-mutants
# internal error. Timeouts are reported but do not fail the run, matching
# the weekly job. Use `&&` in your inner loop if you want it as a gate.
#
# Needs: cargo-mutants, cargo-nextest, jq, the geometry-test-data
# submodule and the realdata fixtures (the baseline is the quick tier).
# Report lands in <repo root>/mutants.out/ (gitignored).

set -euo pipefail

usage() {
  sed -n '2,25p' "$0"
}

base_ref=origin/main
if [ $# -gt 0 ] && [ "$1" != "--" ]; then
  case "$1" in
    -h|--help) usage; exit 0 ;;
  esac
  base_ref=$1
  shift
fi
if [ $# -gt 0 ]; then
  if [ "$1" != "--" ]; then
    echo "error: unexpected argument '$1' (extra cargo-mutants args go after --)" >&2
    usage >&2
    exit 2
  fi
  shift
fi
extra_args=("$@")

for tool in cargo-mutants cargo-nextest jq; do
  if ! command -v "$tool" > /dev/null 2>&1; then
    case "$tool" in
      jq) echo "error: jq not found (apt install jq / brew install jq)" >&2 ;;
      *) echo "error: $tool not found; install with:" >&2
         echo "  cargo install cargo-mutants cargo-nextest --locked" >&2 ;;
    esac
    exit 2
  fi
done

root=$(git rev-parse --show-toplevel)
cd "$root"

if ! merge_base=$(git merge-base "$base_ref" HEAD 2>/dev/null); then
  echo "error: cannot find merge-base of '$base_ref' and HEAD" >&2
  echo "  (does the ref exist? try: git fetch origin main)" >&2
  exit 2
fi

diff_file=$(mktemp -t mutants-diff.XXXXXX)
trap 'rm -f "$diff_file"' EXIT

# Working tree vs merge-base: committed and uncommitted changes alike, so
# the inner loop sees the code you are about to commit. Only Rust under
# crates/ can produce mutants (fuzz/ and benchmarks/ are separate
# workspaces; Cargo.toml edits have no mutation sites).
git diff "$merge_base" -- ':(glob)crates/**/*.rs' > "$diff_file"

if [ ! -s "$diff_file" ]; then
  echo "mutants-diff: no Rust changes under crates/ vs $base_ref" \
    "(merge-base ${merge_base:0:12}); nothing to mutate."
  exit 0
fi

n_files=$(grep -c '^+++ b/' "$diff_file" || true)
echo "mutants-diff: $n_files Rust file(s) changed vs $base_ref" \
  "(merge-base ${merge_base:0:12})"
grep '^+++ b/' "$diff_file" | sed 's|^+++ b/|  |'
echo

out_dir=$root
rm -rf "$out_dir/mutants.out"

# Same invocation as the CI job. Exit codes: https://mutants.rs/exit-codes.html
start=$(date +%s)
set +e
cargo mutants --in-diff "$diff_file" --output "$out_dir" "${extra_args[@]}"
code=$?
set -e
elapsed=$(( $(date +%s) - start ))

outcomes=$out_dir/mutants.out/outcomes.json
if [ ! -f "$outcomes" ]; then
  echo
  echo "mutants-diff: cargo mutants exited $code and wrote no outcomes.json" >&2
  case "$code" in
    4) echo "  the unmutated baseline failed: fix the tests before mutating" >&2 ;;
  esac
  exit "$code"
fi

read -r total caught missed timeout unviable < <(jq -r \
  '[.total_mutants, .caught, .missed, .timeout, .unviable] | @tsv' "$outcomes")
tested=$((caught + missed + timeout))
if [ "$tested" -gt 0 ]; then
  score=$(awk -v c="$caught" -v t="$tested" 'BEGIN { printf "%.1f", 100 * c / t }')
else
  score="n/a"
fi

echo
echo "mutants-diff: score ${score}% (${caught} caught / ${tested} tested)" \
  "in $((elapsed / 60))m$((elapsed % 60))s"
printf '  total %s | caught %s | missed %s | timeout %s | unviable %s\n' \
  "$total" "$caught" "$missed" "$timeout" "$unviable"

for kind in missed timeout; do
  list=$out_dir/mutants.out/$kind.txt
  if [ -s "$list" ]; then
    echo
    echo "$kind mutants ($(wc -l < "$list")):"
    sed 's/^/  /' "$list"
  fi
done
echo
echo "report: $out_dir/mutants.out/ (missed.txt, timeout.txt, log/)"

case "$code" in
  0|3) ;;                                   # all caught (3: some timed out)
  2) exit 1 ;;                              # missed mutants: the gate
  4) echo "mutants-diff: baseline failed" >&2; exit 4 ;;
  *) echo "mutants-diff: cargo mutants failed with exit $code" >&2; exit "$code" ;;
esac
