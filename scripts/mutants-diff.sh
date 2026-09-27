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
#   1. Diffs the working tree (committed + uncommitted; untracked *.rs are
#      marked intent-to-add so they count) against the merge-base with
#      BASE_REF, restricted to *.rs under crates/core/, and writes that
#      diff to a temp file.
#   2. Runs `cargo mutants --package tylertoo-core --in-diff <file>` with
#      the repo's .cargo/mutants.toml (nextest, `quick` tier, timeouts),
#      so only mutants inside changed lines are generated and tested.
#   3. Prints the score and the missed mutants (file:line: description).
#
# Exit codes: 0 no mutant missed (or nothing to mutate), 1 at least one
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
  sed -n '2,27p' "$0"
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

# Only tylertoo-core is mutated, as in the weekly job: the pyo3 cdylib's
# wrappers have no Rust tests and would all show as missed. Only Rust
# under crates/core/ can produce mutants there.
pathspec=':(glob)crates/core/**/*.rs'

# `git diff` does not see untracked files. Mark new Rust files
# intent-to-add so a brand-new module counts; the index entry is empty
# and `git reset -- <file>` undoes it.
untracked=$(git ls-files --others --exclude-standard -- "$pathspec")
if [ -n "$untracked" ]; then
  echo "mutants-diff: marking untracked Rust file(s) intent-to-add so they are diffed:"
  printf '%s\n' "$untracked" | sed 's/^/  /'
  printf '%s\n' "$untracked" | xargs git add -N --
fi

diff_file=$(mktemp -t mutants-diff.XXXXXX)
trap 'rm -f "$diff_file"' EXIT

# Working tree vs merge-base: committed and uncommitted changes alike, so
# the inner loop sees the code you are about to commit.
git diff "$merge_base" -- "$pathspec" > "$diff_file"

if [ ! -s "$diff_file" ]; then
  echo "mutants-diff: no Rust changes under crates/core/ vs $base_ref" \
    "(merge-base ${merge_base:0:12}); nothing to mutate."
  exit 0
fi

# A deletion-only diff has no `+++ b/` lines; that is still a diff worth
# listing mutants for (a removed guard shifts the lines around it).
changed=$(grep '^+++ b/' "$diff_file" | sed 's|^+++ b/||' || true)
if [ -n "$changed" ]; then
  echo "mutants-diff: $(printf '%s\n' "$changed" | wc -l) Rust file(s) changed vs $base_ref" \
    "(merge-base ${merge_base:0:12})"
  printf '%s\n' "$changed" | sed 's/^/  /'
else
  echo "mutants-diff: Rust deletions only vs $base_ref (merge-base ${merge_base:0:12})"
fi
echo

out_dir=$root
rm -rf "$out_dir/mutants.out"

# A diff that touches Rust but no mutation site (tests only, a cfg(test)
# module, comments) makes cargo-mutants exit 0 without writing a report;
# say so instead of reading that as a failure.
planned=$(cargo mutants --package tylertoo-core --in-diff "$diff_file" --list 2>/dev/null | wc -l)
if [ "$planned" -eq 0 ]; then
  echo "mutants-diff: no mutants in this diff (the changed lines hold no mutation site)."
  exit 0
fi
echo "mutants-diff: $planned mutant(s) in the diff"

# Same invocation as the CI job. Exit codes: https://mutants.rs/exit-codes.html
start=$(date +%s)
set +e
cargo mutants --package tylertoo-core --in-diff "$diff_file" --output "$out_dir" "${extra_args[@]}"
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
printf '  planned %s | tested %s | caught %s | missed %s | timeout %s | unviable %s\n' \
  "$planned" "$total" "$caught" "$missed" "$timeout" "$unviable"

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

# The gate is the parsed count, not the exit code: cargo-mutants reports
# exit 3 (timeouts) ahead of 2 (missed), so a run with both would look
# clean if we only read the code.
case "$code" in
  0|2|3) ;;
  4) echo "mutants-diff: baseline failed" >&2; exit 4 ;;
  *) echo "mutants-diff: cargo mutants failed with exit $code" >&2; exit "$code" ;;
esac
if [ "$missed" -gt 0 ]; then
  exit 1
fi
