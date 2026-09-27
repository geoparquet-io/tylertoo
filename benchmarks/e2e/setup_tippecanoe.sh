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
# one recorded here, build it into the repo-root .tools/ (next to go-pmtiles;
# TIPPECANOE_TOOLS_DIR overrides), and record version + SHA in
# tippecanoe.lock.json, which run_e2e.py copies into every result file.
#
# Homebrew note: `brew install tippecanoe` currently also ships 2.79.0, so a
# brew binary is a valid stand-in — but brew's formula follows stable and will
# move. Pass TIPPECANOE=/opt/homebrew/bin/tippecanoe to run_e2e.py to use it;
# the harness records whatever `--version` reports either way and refuses to
# run if it is not the pinned version (unless TIPPECANOE_ALLOW_MISMATCH=1).
# Only the binary this script built gets a tag + SHA in the results; any other
# binary is recorded with sha null.
#
# Usage:
#   ./setup_tippecanoe.sh            # build the pinned tag into <repo>/.tools
#   eval "$(./setup_tippecanoe.sh --print-path)"   # put that build on PATH
#   TIPPECANOE_TAG=2.78.0 TIPPECANOE_SHA=<full commit sha> ./setup_tippecanoe.sh
#                                    # pin something else: the SHA is required,
#                                    # because verifying a tag against the SHA
#                                    # it resolves to proves nothing. (run_e2e.py
#                                    # also pins the --version string; a
#                                    # different tag needs
#                                    # TIPPECANOE_ALLOW_MISMATCH=1 there.)
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/../.." && pwd)"
# Built into the repo-root .tools/ (gitignored), next to go-pmtiles
# (scripts/setup_go_pmtiles.sh), so one cache path covers every pinned
# external binary. TIPPECANOE_TOOLS_DIR overrides it.
TOOLS="${TIPPECANOE_TOOLS_DIR:-$ROOT/.tools}"

# --- the pin -----------------------------------------------------------------
# felt/tippecanoe 2.79.0, published 2025-07-24, the latest release as of
# 2026-09-26. Verified: `gh api repos/felt/tippecanoe/git/ref/tags/2.79.0`.
DEFAULT_TAG="2.79.0"
DEFAULT_SHA="68ab8dcc229f95b8b25877697d5e8d66783af503"
if [ -n "${TIPPECANOE_TAG:-}" ] && [ -z "${TIPPECANOE_SHA:-}" ]; then
  echo "ERROR: TIPPECANOE_TAG=$TIPPECANOE_TAG was set without TIPPECANOE_SHA." >&2
  echo "       A re-pin must name the commit it expects the tag to resolve to:" >&2
  echo "         git ls-remote https://github.com/felt/tippecanoe.git refs/tags/$TIPPECANOE_TAG" >&2
  echo "       then re-run with TIPPECANOE_SHA=<that full sha>." >&2
  exit 1
fi
TIPPECANOE_TAG="${TIPPECANOE_TAG:-$DEFAULT_TAG}"
TIPPECANOE_SHA="${TIPPECANOE_SHA:-$DEFAULT_SHA}"
TIPPECANOE_REPO="${TIPPECANOE_REPO:-https://github.com/felt/tippecanoe.git}"

SRC="$TOOLS/src-$TIPPECANOE_TAG"
PREFIX="$TOOLS/tippecanoe-$TIPPECANOE_TAG"
BIN="$PREFIX/bin/tippecanoe"

# `--print-path`: emit shell that puts the pinned build on PATH (tippecanoe,
# tippecanoe-decode, tile-join, ...) and names the binary, for `eval` in a
# workflow step or a shell. Does not build.
if [ "${1:-}" = "--print-path" ]; then
  echo "export TIPPECANOE_BIN=\"$BIN\""
  echo "export PATH=\"$PREFIX/bin:\$PATH\""
  exit 0
fi

mkdir -p "$TOOLS"

# The checked-out source must be the pinned commit. Run before building, and
# again when a previous build is reused, so a stale .tools/ from another pin
# cannot be recorded under this one.
verify_sha() {
  local got_sha
  got_sha="$(git -C "$SRC" rev-parse HEAD)"
  if [ "$got_sha" != "$TIPPECANOE_SHA" ]; then
    echo "ERROR: tag $TIPPECANOE_TAG resolved to $got_sha, expected $TIPPECANOE_SHA" >&2
    echo "       (a moved tag — re-pin deliberately, do not silently accept)" >&2
    exit 1
  fi
}

# Minimal JSON string escaping (backslash and double quote), so a version
# string or path containing either cannot corrupt the lockfile.
json_escape() {
  local s="${1//\\/\\\\}"
  s="${s//\"/\\\"}"
  printf '%s' "$s"
}

if [ -x "$BIN" ]; then
  echo "already built: $BIN ($("$BIN" --version 2>&1 | head -1))"
  if [ -d "$SRC/.git" ]; then
    verify_sha
  else
    echo "ERROR: $BIN exists but its source checkout $SRC is gone; cannot" >&2
    echo "       verify the commit. Remove $PREFIX and re-run." >&2
    exit 1
  fi
else
  if [ ! -d "$SRC/.git" ]; then
    echo "cloning $TIPPECANOE_REPO @ $TIPPECANOE_TAG ..."
    rm -rf "$SRC"
    git clone --quiet --depth 1 --branch "$TIPPECANOE_TAG" "$TIPPECANOE_REPO" "$SRC"
  fi

  # Verify BEFORE building: nothing from an unverified checkout gets compiled.
  verify_sha

  echo "building tippecanoe $TIPPECANOE_TAG ($TIPPECANOE_SHA) ..."
  make -C "$SRC" -j"$(getconf _NPROCESSORS_ONLN 2>/dev/null || echo 4)" >"$TOOLS/build.log" 2>&1 || {
    echo "ERROR: build failed, see $TOOLS/build.log" >&2
    tail -30 "$TOOLS/build.log" >&2
    exit 1
  }
  make -C "$SRC" install PREFIX="$PREFIX" >>"$TOOLS/build.log" 2>&1
fi

VERSION="$("$BIN" --version 2>&1 | head -1)"
SHA="$(git -C "$SRC" rev-parse HEAD)"

cat >"$HERE/tippecanoe.lock.json" <<EOF
{
  "tag": "$(json_escape "$TIPPECANOE_TAG")",
  "sha": "$(json_escape "$SHA")",
  "repo": "$(json_escape "$TIPPECANOE_REPO")",
  "version_string": "$(json_escape "$VERSION")",
  "binary": "$(json_escape "$BIN")",
  "built_at": "$(date -u +%Y-%m-%dT%H:%M:%SZ)"
}
EOF

echo
echo "tippecanoe: $BIN"
echo "version:    $VERSION"
echo "sha:        $SHA"
echo "lockfile:   $HERE/tippecanoe.lock.json"
