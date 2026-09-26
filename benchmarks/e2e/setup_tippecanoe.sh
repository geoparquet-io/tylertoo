#!/usr/bin/env bash
#
# setup_tippecanoe.sh — build the PINNED tippecanoe the e2e harness compares against.
#
# Why build from source instead of using whatever `tippecanoe` is on PATH:
# a benchmark that does not name its baseline's version is not reproducible.
# On the machine this harness was written on, `which tippecanoe` resolved to a
# conda-forge build of v2.31.0 (the old mapbox lineage) while Homebrew had
# 2.79.0 installed but shadowed. Two tippecanoes, eight years of tiling changes
# between them, and the harness would silently have picked the wrong one.
#
# So: clone felt/tippecanoe at a pinned TAG, verify the commit SHA matches the
# one recorded here, build it into ./.tools, and record version + SHA in
# tippecanoe.lock.json, which run_e2e.py copies into every result file.
#
# Homebrew note: `brew install tippecanoe` currently also ships 2.79.0, so a
# brew binary is a valid stand-in — but brew's formula follows stable and will
# move. Pass TIPPECANOE=/opt/homebrew/bin/tippecanoe to run_e2e.py to use it;
# the harness records whatever `--version` reports either way and refuses to
# run if it is not the pinned version (unless TIPPECANOE_ALLOW_MISMATCH=1).
#
# Usage:
#   ./setup_tippecanoe.sh            # build the pinned tag into ./.tools
#   TIPPECANOE_TAG=2.78.0 ./setup_tippecanoe.sh   # pin something else (records its SHA)
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
TOOLS="$HERE/.tools"

# --- the pin -----------------------------------------------------------------
# felt/tippecanoe 2.79.0, published 2025-07-24, the latest release as of
# 2026-09-26. Verified: `gh api repos/felt/tippecanoe/git/ref/tags/2.79.0`.
TIPPECANOE_TAG="${TIPPECANOE_TAG:-2.79.0}"
TIPPECANOE_SHA="${TIPPECANOE_SHA:-68ab8dcc229f95b8b25877697d5e8d66783af503}"
TIPPECANOE_REPO="${TIPPECANOE_REPO:-https://github.com/felt/tippecanoe.git}"

SRC="$TOOLS/src-$TIPPECANOE_TAG"
PREFIX="$TOOLS/tippecanoe-$TIPPECANOE_TAG"
BIN="$PREFIX/bin/tippecanoe"

mkdir -p "$TOOLS"

if [ -x "$BIN" ]; then
  echo "already built: $BIN ($("$BIN" --version 2>&1 | head -1))"
else
  if [ ! -d "$SRC/.git" ]; then
    echo "cloning $TIPPECANOE_REPO @ $TIPPECANOE_TAG ..."
    rm -rf "$SRC"
    git clone --quiet --depth 1 --branch "$TIPPECANOE_TAG" "$TIPPECANOE_REPO" "$SRC"
  fi

  got_sha="$(git -C "$SRC" rev-parse HEAD)"
  if [ "$TIPPECANOE_SHA" != "" ] && [ "$got_sha" != "$TIPPECANOE_SHA" ]; then
    echo "ERROR: tag $TIPPECANOE_TAG resolved to $got_sha, expected $TIPPECANOE_SHA" >&2
    echo "       (a moved tag — re-pin deliberately, do not silently accept)" >&2
    exit 1
  fi

  echo "building tippecanoe $TIPPECANOE_TAG ($got_sha) ..."
  make -C "$SRC" -j"$(getconf _NPROCESSORS_ONLN 2>/dev/null || echo 4)" >"$TOOLS/build.log" 2>&1 || {
    echo "ERROR: build failed, see $TOOLS/build.log" >&2
    tail -30 "$TOOLS/build.log" >&2
    exit 1
  }
  make -C "$SRC" install PREFIX="$PREFIX" >>"$TOOLS/build.log" 2>&1
fi

VERSION="$("$BIN" --version 2>&1 | head -1)"
SHA="$(git -C "$SRC" rev-parse HEAD 2>/dev/null || echo "$TIPPECANOE_SHA")"

cat >"$HERE/tippecanoe.lock.json" <<EOF
{
  "tag": "$TIPPECANOE_TAG",
  "sha": "$SHA",
  "repo": "$TIPPECANOE_REPO",
  "version_string": "$VERSION",
  "binary": "$BIN",
  "built_at": "$(date -u +%Y-%m-%dT%H:%M:%SZ)"
}
EOF

echo
echo "tippecanoe: $BIN"
echo "version:    $VERSION"
echo "sha:        $SHA"
echo "lockfile:   $HERE/tippecanoe.lock.json"
