#!/usr/bin/env bash
#
# fetch.sh - download/build raw corpus datasets into data/raw/
#
# Default profile = small + medium tiers (< ~2-3 GB).
# Pass --large to also fetch large-tier (state/country) datasets.
#
# Idempotent: existing outputs are skipped (use --force to refetch), but
# every output, new or kept, is checked against its pin in manifest.json and
# the script fails on a mismatch (#810): `sha256` of the download (the zip for
# Natural Earth), or for an Overture extract its exact row count and
# `ids_sha256` (sha256 of its ids, sorted, joined with ','). An overridden
# GPQ_OVERTURE_RELEASE has no pins, so its extracts are not checked.
# Source of truth for dataset definitions is manifest.json.
#
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
MANIFEST="$HERE/manifest.json"
RAW="$HERE/data/raw"

WANT_LARGE=0
FORCE=0
for arg in "$@"; do
  case "$arg" in
    --large) WANT_LARGE=1 ;;
    --force) FORCE=1 ;;
    -h|--help)
      grep '^#' "$0" | sed 's/^# \{0,1\}//'
      exit 0 ;;
    *)
      echo "Unknown arg: $arg" >&2
      exit 2 ;;
  esac
done

# ---- tool checks -----------------------------------------------
need() {
  command -v "$1" >/dev/null 2>&1 && return 0
  echo "ERROR: required tool '$1' not found." >&2
  echo "  $2" >&2
  exit 1
}
need duckdb \
  "Install: https://duckdb.org/docs/installation/"
need jq \
  "Install: 'apt-get install jq' or 'brew install jq'"
need gpio \
  "Install: 'uv tool install geoparquet-io' (CLI: gpio)"
need curl \
  "Install via your OS package manager."
need unzip \
  "Install via your OS package manager."

mkdir -p "$RAW"

REL="${GPQ_OVERTURE_RELEASE:-$(jq -r '.overture.release' \
  "$MANIFEST")}"
BUCKET="$(jq -r '.overture.bucket' "$MANIFEST")"
S3REGION="$(jq -r '.overture.region' "$MANIFEST")"

echo "Overture release: $REL"
echo "Raw output dir:   $RAW"
echo "Large tier:       $([ $WANT_LARGE = 1 ] \
  && echo yes || echo no)"
echo

PINNED_REL="$(jq -r '.overture.release' "$MANIFEST")"

sha256_of() {
  if command -v sha256sum >/dev/null 2>&1; then sha256sum "$1" | cut -d' ' -f1
  else shasum -a 256 "$1" | cut -d' ' -f1; fi
}

# ---- pins: fail loudly on any input drift (#696, #810) ---------
check_sha256() {
  local id="$1" file="$2" want="$3" got
  got="$(sha256_of "$file")"
  if [ "$got" != "$want" ]; then
    echo "ERROR: [$id] $file has sha256 $got, manifest.json pins $want" >&2
    exit 1
  fi
}

check_overture_pin() {
  local id="$1" file="$2" rows="$3" ids="$4" got
  if [ "$REL" != "$PINNED_REL" ]; then
    echo "   (release $REL is not the pinned $PINNED_REL: pins not checked)"
    return 0
  fi
  got="$(duckdb -noheader -list -c "SELECT count(*) || ' ' || \
sha256(string_agg(id, ',' ORDER BY id)) FROM read_parquet('$file')" | tail -1)"
  if [ "$got" != "$rows $ids" ]; then
    echo "ERROR: [$id] $file has rows/ids_sha256 '$got', manifest.json pins '$rows $ids'" >&2
    exit 1
  fi
}

check_pin() {
  local ds="$1" out="$2" id sha
  id="$(jq -r '.id' <<<"$ds")"
  sha="$(jq -r '.sha256 // empty' <<<"$ds")"
  case "$(jq -r '.source' <<<"$ds")" in
    overture)
      check_overture_pin "$id" "$out" \
        "$(jq -r '.expected_features' <<<"$ds")" "$(jq -r '.ids_sha256' <<<"$ds")" ;;
    http-parquet) check_sha256 "$id" "$out" "$sha" ;;
    *) echo "   [$id] pinned by its download sha256 (checked at fetch)"; return 0 ;;
  esac
  echo "   pin ok [$id]"
}

# ---- fetch one Overture dataset via DuckDB ---------------------
fetch_overture() {
  local id="$1" theme="$2" type="$3" select="$4"
  local xmin="$5" ymin="$6" xmax="$7" ymax="$8"
  local out="$RAW/$id.parquet"
  local src
  src="$BUCKET/$REL/theme=$theme/type=$type/*"

  echo ">> [$id] Overture $theme/$type"
  local sql
  sql=$(cat <<SQL
INSTALL spatial; LOAD spatial;
INSTALL httpfs; LOAD httpfs;
SET s3_region='$S3REGION';
COPY (
  SELECT $select
  FROM read_parquet('$src', hive_partitioning=1)
  WHERE bbox.xmin BETWEEN $xmin AND $xmax
    AND bbox.ymin BETWEEN $ymin AND $ymax
) TO '$out' (FORMAT PARQUET, COMPRESSION ZSTD);
SQL
)
  duckdb <<<"$sql"
  echo "   wrote $out ($(du -h "$out" | cut -f1))"
}

# ---- fetch one Natural Earth dataset (shp -> parquet) ----------
fetch_natural_earth() {
  local id="$1" url="$2" shp="$3" sha="$4"
  local out="$RAW/$id.parquet"
  local tmp
  tmp="$(mktemp -d)"

  echo ">> [$id] Natural Earth $url"
  curl -sL --fail --max-time 300 \
    -o "$tmp/ne.zip" "$url"
  check_sha256 "$id" "$tmp/ne.zip" "$sha"
  unzip -q -o "$tmp/ne.zip" -d "$tmp"
  gpio convert geoparquet \
    "$tmp/$shp" "$out"
  rm -rf "$tmp"
  echo "   wrote $out ($(du -h "$out" | cut -f1))"
}

# ---- fetch one direct-URL parquet ------------------------------
fetch_http_parquet() {
  local id="$1" url="$2"
  local out="$RAW/$id.parquet"

  echo ">> [$id] direct parquet $url"
  curl -sL --fail --max-time 900 \
    -o "$out" "$url"
  echo "   wrote $out ($(du -h "$out" | cut -f1))"
}

# ---- iterate manifest ------------------------------------------
COUNT=0
while read -r ds; do
  id="$(jq -r '.id' <<<"$ds")"
  profile="$(jq -r '.profile' <<<"$ds")"
  source="$(jq -r '.source' <<<"$ds")"

  # profile gating
  case "$profile" in
    default) : ;;
    large)
      [ "$WANT_LARGE" = 1 ] || {
        echo "-- skip [$id] (large tier; pass --large)"
        continue
      } ;;
    *)
      echo "-- skip [$id] (profile=$profile, manual)"
      continue ;;
  esac

  out="$RAW/$id.parquet"
  if [ -f "$out" ] && [ "$FORCE" = 0 ]; then
    echo "== have [$id] ($(du -h "$out" | cut -f1)); \
skip (use --force)"
    check_pin "$ds" "$out"
    continue
  fi

  case "$source" in
    overture)
      theme="$(jq -r '.theme' <<<"$ds")"
      type="$(jq -r '.type' <<<"$ds")"
      select="$(jq -r '.select' <<<"$ds")"
      xmin="$(jq -r '.bbox[0]' <<<"$ds")"
      ymin="$(jq -r '.bbox[1]' <<<"$ds")"
      xmax="$(jq -r '.bbox[2]' <<<"$ds")"
      ymax="$(jq -r '.bbox[3]' <<<"$ds")"
      fetch_overture "$id" "$theme" "$type" \
        "$select" "$xmin" "$ymin" "$xmax" "$ymax"
      ;;
    natural-earth)
      url="$(jq -r '.url' <<<"$ds")"
      shp="$(jq -r '.shapefile' <<<"$ds")"
      fetch_natural_earth "$id" "$url" "$shp" \
        "$(jq -r '.sha256' <<<"$ds")"
      ;;
    http-parquet)
      url="$(jq -r '.url' <<<"$ds")"
      fetch_http_parquet "$id" "$url"
      ;;
    *)
      echo "-- skip [$id] (source=$source \
not auto-fetchable)"
      continue ;;
  esac
  check_pin "$ds" "$out"
  COUNT=$((COUNT + 1))
done < <(jq -c '.datasets[]' "$MANIFEST")

echo
echo "Done. Fetched/updated $COUNT dataset(s)."
echo "Next: ./optimize.sh"
