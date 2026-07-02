#!/usr/bin/env bash
#
# goldens.sh - build reference/baseline outputs into data/goldens/
#
# For each gpio-optimized dataset (data/gpio/<id>.parquet):
#   1. tippecanoe -> data/goldens/tippecanoe/<id>.pmtiles
#      (per-zoom MVT baseline; exact flags recorded next to it)
#   2. cogp-rs   -> data/goldens/cogp/<id>.parquet
#      (thinning-parity baseline; SKIPPED gracefully if cogp
#       cannot be installed)
#
# tippecanoe flags per dataset come from manifest.json.
#
# Idempotent: skips outputs newer than their input (--force rebuilds).
#
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
MANIFEST="$HERE/manifest.json"
GPIODIR="$HERE/data/gpio"
GOLD="$HERE/data/goldens"
TIPPE="$GOLD/tippecanoe"
COGPDIR="$GOLD/cogp"

FORCE=0
NO_COGP=0
for arg in "$@"; do
  case "$arg" in
    --force) FORCE=1 ;;
    --no-cogp) NO_COGP=1 ;;
    -h|--help)
      grep '^#' "$0" | sed 's/^# \{0,1\}//'
      exit 0 ;;
    *) echo "Unknown arg: $arg" >&2; exit 2 ;;
  esac
done

# ---- required tools --------------------------------------------
command -v tippecanoe >/dev/null 2>&1 || {
  echo "ERROR: 'tippecanoe' not found." >&2
  echo "  Install: https://github.com/felt/tippecanoe" >&2
  exit 1
}
command -v gpio >/dev/null 2>&1 || {
  echo "ERROR: 'gpio' not found." >&2
  echo "  Install: 'uv tool install geoparquet-io'" >&2
  exit 1
}
command -v jq >/dev/null 2>&1 || {
  echo "ERROR: 'jq' not found." >&2
  exit 1
}

[ -d "$GPIODIR" ] || {
  echo "No data/gpio/. Run ./optimize.sh first." >&2
  exit 1
}
mkdir -p "$TIPPE"

# ---- optionally provision cogp-rs ------------------------------
# cogp is not on crates.io; install from git. If that fails we
# skip cogp goldens rather than aborting the whole run.
COGP_BIN=""
provision_cogp() {
  [ "$NO_COGP" = 1 ] && return 1
  if command -v cogp >/dev/null 2>&1; then
    COGP_BIN="$(command -v cogp)"; return 0
  fi
  command -v cargo >/dev/null 2>&1 || {
    echo "!! cargo not found; skipping cogp goldens."
    return 1
  }
  echo ">> installing cogp-rs (cargo install --git) ..."
  if cargo install --git \
       https://github.com/Kanahiro/cogp-rs cogp \
       2>/tmp/cogp_install.log; then
    COGP_BIN="$(command -v cogp)"; return 0
  fi
  echo "!! cogp install failed; skipping cogp goldens."
  echo "   (see /tmp/cogp_install.log). Build manually:"
  echo "   git clone https://github.com/Kanahiro/cogp-rs"
  echo "   cd cogp-rs && cargo build --release -p cogp"
  return 1
}

HAVE_COGP=0
if provision_cogp; then
  HAVE_COGP=1
  mkdir -p "$COGPDIR"
  echo "   cogp: $COGP_BIN"
fi
echo

# ---- lookup helper: manifest entry by id -----------------------
mget() { # id jq-filter
  jq -r --arg id "$1" \
    '.datasets[] | select(.id==$id) | '"$2" "$MANIFEST"
}

shopt -s nullglob
INPUTS=("$GPIODIR"/*.parquet)
[ ${#INPUTS[@]} -gt 0 ] || {
  echo "No optimized files. Run ./optimize.sh first." >&2
  exit 1
}

for src in "${INPUTS[@]}"; do
  id="$(basename "$src" .parquet)"

  minz="$(mget "$id" '.tippecanoe.minzoom // 0')"
  maxz="$(mget "$id" '.tippecanoe.maxzoom // 14')"
  flags="$(mget "$id" '.tippecanoe.flags // ""')"
  if [ -z "$minz" ]; then
    echo "-- [$id] not in manifest; skip"
    continue
  fi

  # ---------- tippecanoe golden ----------
  pmt="$TIPPE/$id.pmtiles"
  if [ -f "$pmt" ] && [ "$FORCE" = 0 ] \
     && [ "$pmt" -nt "$src" ]; then
    echo "== tippecanoe [$id]; skip"
  else
    full="-Z$minz -z$maxz -l data $flags"
    echo ">> tippecanoe [$id] ($full)"
    # gpio emits newline-delimited GeoJSON (-P parallel).
    gpio convert geojson "$src" \
      | tippecanoe -P $full -o "$pmt" -f
    # record exact flags for reproducibility / V2/V3.
    printf 'input: %s\nflags: %s\nzoom: %s..%s\n' \
      "$src" "$full" "$minz" "$maxz" \
      > "$TIPPE/$id.flags.txt"
    echo "   wrote $pmt"
  fi

  # ---------- cogp golden ----------
  if [ "$HAVE_COGP" = 1 ]; then
    cout="$COGPDIR/$id.parquet"
    if [ -f "$cout" ] && [ "$FORCE" = 0 ] \
       && [ "$cout" -nt "$src" ]; then
      echo "== cogp [$id]; skip"
    else
      echo ">> cogp convert [$id]"
      if "$COGP_BIN" convert "$src" "$cout" \
           --webmerc-minzoom "$minz" \
           --webmerc-maxzoom "$maxz"; then
        "$COGP_BIN" validate "$cout" || \
          echo "   !! cogp validate failed for $id"
        echo "   wrote $cout"
      else
        echo "   !! cogp convert failed for $id (skipped)"
      fi
    fi
  fi
done

echo
echo "Goldens written under $GOLD"
[ "$HAVE_COGP" = 0 ] && \
  echo "(cogp goldens skipped; see notes above.)"
echo "Done."
