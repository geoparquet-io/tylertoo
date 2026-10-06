#!/usr/bin/env bash
# Lint the prose in tylertoo's handwritten docs with Vale and proselint.
#
#   scripts/lint-prose.sh            # every in-scope file
#   scripts/lint-prose.sh --staged   # staged in-scope files (pre-commit)
#   scripts/lint-prose.sh FILE...    # the in-scope subset of FILE...
#
# Vale fails on error-level alerts (.vale.ini); proselint fails on any
# check enabled in proselint.json. Both skip code blocks and inline code.
# For Vale's advisory warnings and suggestions, run:
#   vale --minAlertLevel=suggestion $(scripts/lint-prose.sh --list)
#
# The pre-commit hook and the CI `prose` job both call this script, so the
# scope below is the single list of files under prose lint.

set -euo pipefail

cd "$(git rev-parse --show-toplevel)"

# Prose people read. The generated CLI and Python reference pages are in
# scope on purpose: they are the clap help and the Python docstrings as
# rendered, so linting them lints the docstrings. Excluded: README.md, which
# the maintainer writes by hand in their own voice; pages that only
# `--8<--`-include a file linted here; and the symlinks under docs/ (their
# targets are listed directly or out of scope: CHANGELOG.md is generated,
# context/ is design notes).
in_scope() {
    git ls-files -- \
        CONTRIBUTING.md DEVELOPMENT.md \
        ':(glob)docs/**/*.md' ':(glob)examples/*/README.md' \
        ':(exclude)docs/index.md' \
        ':(exclude,glob)docs/tutorials/*.md' |
        while IFS= read -r f; do
            [ -L "$f" ] || printf '%s\n' "$f"
        done
}

mode=all
case "${1:-}" in
    --staged) mode=staged ;;
    --list) mode=list ;;
    --help | -h)
        sed -n '2,14p' "$0"
        exit 0
        ;;
esac

# Plain read loops, not mapfile: macOS still ships bash 3.2.
scope=()
while IFS= read -r f; do scope+=("$f"); done < <(in_scope)

if [ "$mode" = list ]; then
    printf '%s\n' ${scope[@]+"${scope[@]}"}
    exit 0
fi

if [ "$mode" = staged ]; then
    wanted=()
    while IFS= read -r f; do wanted+=("$f"); done < <(
        git diff --cached --name-only --diff-filter=ACMR
    )
elif [ "$#" -gt 0 ]; then
    wanted=("$@")
else
    wanted=("${scope[@]}")
fi

files=()
for f in ${wanted[@]+"${wanted[@]}"}; do
    for s in ${scope[@]+"${scope[@]}"}; do
        if [ "$f" = "$s" ] && [ -f "$f" ]; then
            files+=("$f")
            break
        fi
    done
done

if [ "${#files[@]}" -eq 0 ]; then
    echo "No in-scope Markdown to lint."
    exit 0
fi

# Outside CI a missing tool is a warning: the CI `prose` job still gates.
missing() {
    if [ -n "${CI:-}" ]; then
        echo "ERROR: $1 is not installed." >&2
        exit 1
    fi
    echo "WARNING: $1 not found; skipping it ($2). CI still runs it." >&2
}

status=0

if command -v vale >/dev/null 2>&1; then
    if [ ! -d .vale/styles/Google ]; then
        vale sync
    fi
    echo "Vale: ${#files[@]} file(s)"
    vale --minAlertLevel=error --output=line "${files[@]}" || status=1
else
    missing vale "install it from https://vale.sh/docs/install"
fi

if command -v uv >/dev/null 2>&1; then
    echo "proselint: ${#files[@]} file(s)"
    uv run --quiet --script scripts/proselint_md.py "${files[@]}" ||
        status=1
else
    missing uv "install it from https://docs.astral.sh/uv/"
fi

exit "$status"
