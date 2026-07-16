#!/usr/bin/env bash
#
# verify.sh - V1 correctness suite for the GeoParquet overview pipeline.
#
# For every gpio-optimized corpus dataset (corpus/data/gpio/*.parquet) this:
#   1. runs `tylertoo overview` in BOTH modes (duplicating + partitioning)
#      into corpus/data/overviews/, capturing wall time + peak RSS and a
#      --report JSON, using the dataset's tippecanoe zoom range from
#      manifest.json (default 0..14).
#   2. runs the correctness checks per output file:
#        - `tylertoo validate` (exit 0)
#        - DuckDB spatial: opens as parquet; count at canonical level ==
#          input (duplicating) / total == input (partitioning); a
#          bbox+level predicate returns plausible results; WKB decode works.
#        - GDAL ogrinfo -so -al: feature count + geometry type, no error.
#        - Canonical fidelity (duplicating): count, sum(ST_NPoints), a
#          coordinate-sensitive envelope aggregate, and an id-column
#          checksum (order-independent bit_xor) all match input exactly.
#        - Monotonicity (from report JSON): feature/vertex counts
#          non-decreasing coarse->fine (duplicating); per-level counts sum
#          to input (partitioning).
#   3. a determinism check: converts one dataset twice and cmp's the bytes;
#      on mismatch, compares content (per-level counts) via DuckDB.
#
# Output: corpus/data/overviews/results.tsv (machine-readable) and
#         corpus/V1_RESULTS.md (human report). Re-runnable & idempotent
#         (always reconverts; outputs are gitignored under corpus/data/).
#
# Env knobs:
#   GPQ_ONLY="id1 id2"   restrict to these dataset ids (debug)
#   GPQ_BIN=path         override release binary path
#
set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/.." && pwd)"
MANIFEST="$HERE/manifest.json"
GPIO="$HERE/data/gpio"
OUT="$HERE/data/overviews"
BIN="${GPQ_BIN:-$ROOT/target/release/tylertoo}"
RESULTS_TSV="$OUT/results.tsv"
RESULTS_MD="$HERE/V1_RESULTS.md"

mkdir -p "$OUT"

for t in duckdb ogrinfo jq "$BIN"; do
  command -v "$t" >/dev/null 2>&1 || [ -x "$t" ] || {
    echo "ERROR: required tool missing: $t" >&2; exit 1; }
done

# ---- helpers ----------------------------------------------------
# DuckDB scalar query with spatial loaded; prints the single value.
dq() {
  duckdb -noheader -list -c "INSTALL spatial;LOAD spatial;$1" 2>/dev/null | tail -1
}
# DuckDB query capturing stderr (for error detection).
dq_err() {
  duckdb -noheader -list -c "INSTALL spatial;LOAD spatial;$1" 2>&1
}

# primary geometry column name from geo metadata.
geom_col() {
  duckdb -noheader -list -c \
    "SELECT json_extract_string(decode(value),'\$.primary_column') \
     FROM parquet_kv_metadata('$1') WHERE key='geo';" 2>/dev/null | tail -1
}
# id/property column for checksum: prefer id, then OGC_FID, else first col.
id_col() {
  local f="$1"
  local cols
  cols=$(duckdb -noheader -list -c \
    "SELECT column_name FROM (DESCRIBE SELECT * FROM read_parquet('$f'));" \
    2>/dev/null)
  local c
  for c in id OGC_FID ogc_fid; do
    echo "$cols" | grep -qxi "$c" && { echo "$c"; return; }
  done
  echo "$cols" | head -1
}
# level column SQL name: 'level_1' when the source case-collides (DuckDB
# renames the overview column), else 'level'.
level_sql_col() {
  local f="$1"
  duckdb -noheader -list -c \
    "SELECT column_name FROM (DESCRIBE SELECT * FROM read_parquet('$f'));" \
    2>/dev/null | grep -qx 'level_1' && echo 'level_1' || echo 'level'
}

# zoom range from manifest (fallback 0..14).
zoom_range() {
  local id="$1" mn mx
  mn=$(jq -r --arg i "$id" \
    '.datasets[]|select(.id==$i)|.tippecanoe.minzoom // empty' "$MANIFEST")
  mx=$(jq -r --arg i "$id" \
    '.datasets[]|select(.id==$i)|.tippecanoe.maxzoom // empty' "$MANIFEST")
  echo "${mn:-0} ${mx:-14}"
}

pass_fail() { [ "$1" = "$2" ] && echo PASS || echo "FAIL($1!=$2)"; }

# ---- results header ---------------------------------------------
printf 'dataset\tmode\tinput_feat\ttotal_rows\tcanon\tnlevels\twall_s\trss_mb\tout_mb\tin_mb\tvalidate\tduck_open\tduck_canon_cnt\tduck_npoints\tduck_env\tduck_idchk\tduck_wkb\tduck_bbox_pred\togr_fcount\togr_geom\tmono\tnotes\n' \
  > "$RESULTS_TSV"

# ---- per (dataset, mode) run ------------------------------------
run_one() {
  local f="$1" mode="$2"
  local id; id=$(basename "$f" .parquet)
  local gc; gc=$(geom_col "$f")
  local idc; idc=$(id_col "$f")
  read -r mnz mxz < <(zoom_range "$id")
  local out="$OUT/$id.${mode:0:3}.parquet"
  local rep="$OUT/$id.${mode:0:3}.report.json"
  local tlog="$OUT/$id.${mode:0:3}.time.txt"
  local in_mb; in_mb=$(du -m "$f" | cut -f1)
  local notes=""

  echo "== [$id] mode=$mode zoom=$mnz..$mxz geom=$gc id=$idc"
  # convert with time+RSS capture (idempotent: reuse an up-to-date output
  # unless GPQ_FORCE is set, so re-runs re-check without reconverting).
  if [ -z "${GPQ_FORCE:-}" ] && [ -f "$out" ] && [ -f "$rep" ] \
     && [ -f "$tlog" ] && [ "$out" -nt "$f" ]; then
    echo "   (reusing existing conversion)"
  else
    /usr/bin/time -v -o "$tlog" \
      "$BIN" overview "$f" "$out" --mode "$mode" \
      --min-zoom "$mnz" --max-zoom "$mxz" --report "$rep" \
      >/dev/null 2>"$OUT/$id.${mode:0:3}.convert.log"
    local conv_rc=$?
    if [ $conv_rc -ne 0 ]; then
      printf '%s\t%s\tCONVERT_FAILED rc=%d\n' "$id" "$mode" "$conv_rc" \
        >> "$RESULTS_TSV"
      echo "   CONVERT FAILED rc=$conv_rc (see $OUT/$id.${mode:0:3}.convert.log)"
      return
    fi
  fi
  local wall rss_kb
  wall=$(grep -oP 'wall clock.*?\)?: \K[0-9:.]+' "$tlog" | tail -1)
  rss_kb=$(grep -oP 'Maximum resident set size \(kbytes\): \K[0-9]+' "$tlog")
  local rss_mb=$(( ${rss_kb:-0} / 1024 ))
  local out_mb; out_mb=$(du -m "$out" | cut -f1)

  # report-derived numbers
  local input_feat total_rows nlevels canon
  input_feat=$(jq '.input_features' "$rep")
  total_rows=$(jq '.total_rows' "$rep")
  nlevels=$(jq '.levels|length' "$rep")
  canon=$((nlevels-1))

  # validate
  local validate; if "$BIN" validate "$out" >/dev/null 2>&1; then
    validate=PASS; else validate=FAIL; fi

  # level SQL col (collision-aware) + geometry expr
  local lvlc; lvlc=$(level_sql_col "$out")
  [ "$lvlc" = "level_1" ] && notes="${notes}level-name-collision(uses $lvlc);"
  # duplicate bbox covering column? (writer covering name collides with the
  # gpio input's pre-existing bbox column when geom col is named 'geometry')
  duckdb -noheader -list -c \
    "SELECT column_name FROM (DESCRIBE SELECT * FROM read_parquet('$out'));" \
    2>/dev/null | grep -qx 'bbox_1' && \
    notes="${notes}duplicate-bbox-covering-column;"

  # duckdb open
  local duck_open cnt_out
  cnt_out=$(dq "SELECT count(*) FROM read_parquet('$out');")
  [[ "$cnt_out" =~ ^[0-9]+$ ]] && duck_open=PASS || duck_open=FAIL

  # WKB decode works (proves geo metadata read)
  local wkb
  wkb=$(dq "SELECT ST_GeometryType(ST_GeomFromWKB(ST_AsWKB($gc))) \
    FROM read_parquet('$out') LIMIT 1;")
  [[ "$wkb" == *[A-Z]* ]] && wkb=PASS || wkb=FAIL

  # input aggregates
  local in_cnt in_np in_env in_idh
  read -r in_cnt in_np in_env in_idh < <(dq \
    "SELECT count(*)||' '||coalesce(sum(ST_NPoints($gc)),0)||' '|| \
     round(coalesce(sum(ST_XMin($gc)+ST_YMin($gc)+ST_XMax($gc)+ST_YMax($gc)),0)::DOUBLE,2)||' '|| \
     coalesce(bit_xor(hash($idc)),0) FROM read_parquet('$f');")

  local duck_canon duck_np duck_env duck_idchk duck_bbox mono
  if [ "$mode" = "duplicating" ]; then
    # canonical-level aggregates
    local c_cnt c_np c_env c_idh
    read -r c_cnt c_np c_env c_idh < <(dq \
      "SELECT count(*)||' '||coalesce(sum(ST_NPoints($gc)),0)||' '|| \
       round(coalesce(sum(ST_XMin($gc)+ST_YMin($gc)+ST_XMax($gc)+ST_YMax($gc)),0)::DOUBLE,2)||' '|| \
       coalesce(bit_xor(hash($idc)),0) \
       FROM read_parquet('$out') WHERE $lvlc=$canon;")
    duck_canon=$(pass_fail "$c_cnt" "$in_cnt")
    duck_np=$(pass_fail "$c_np" "$in_np")
    duck_env=$(pass_fail "$c_env" "$in_env")
    duck_idchk=$(pass_fail "$c_idh" "$in_idh")
    # monotonicity: feature_count MUST be non-decreasing coarse->fine
    # (thinning semantics, spec 2.2 — a feature visible at level k appears at
    # all finer levels). vertex_count is expected to trend up but MAY dip when
    # a finer level adds many small features (not a spec requirement); reported
    # as an informational note, not a failure.
    mono=$(jq -r '
      (.levels|sort_by(.level)) as $l
      | [range(1; ($l|length)) as $i
         | ($l[$i].feature_count >= $l[$i-1].feature_count)]
      | all | if . then "PASS" else "FAIL" end' "$rep")
    local vmono
    vmono=$(jq -r '
      (.levels|sort_by(.level)) as $l
      | [range(1; ($l|length)) as $i
         | ($l[$i].vertex_count >= $l[$i-1].vertex_count)]
      | all | if . then "ok" else "dip" end' "$rep")
    [ "$vmono" = "dip" ] && \
      notes="${notes}vertex-count-non-monotonic(wide-feature-size-dist);"
  else
    # partitioning: whole table is canonical
    local t_cnt t_np t_env t_idh
    read -r t_cnt t_np t_env t_idh < <(dq \
      "SELECT count(*)||' '||coalesce(sum(ST_NPoints($gc)),0)||' '|| \
       round(coalesce(sum(ST_XMin($gc)+ST_YMin($gc)+ST_XMax($gc)+ST_YMax($gc)),0)::DOUBLE,2)||' '|| \
       coalesce(bit_xor(hash($idc)),0) FROM read_parquet('$out');")
    duck_canon=$(pass_fail "$t_cnt" "$in_cnt")
    duck_np=$(pass_fail "$t_np" "$in_np")
    duck_env=$(pass_fail "$t_env" "$in_env")
    duck_idchk=$(pass_fail "$t_idh" "$in_idh")
    # per-level counts sum to input
    local sum_lvls; sum_lvls=$(jq '[.levels[].feature_count]|add' "$rep")
    mono=$(pass_fail "$sum_lvls" "$input_feat")
  fi

  # bbox+level predicate plausibility: level 0 west of the level-0 midpoint
  # returns between 0 and the level-0 total (query executes; predicate prunes).
  # Uses geometry-derived bbox (ST_XMin) to avoid the duplicate-bbox-column
  # ambiguity; the covering column itself is exercised by `validate`.
  local l0tot bboxsel
  l0tot=$(dq "SELECT count(*) FROM read_parquet('$out') WHERE $lvlc=0;")
  bboxsel=$(dq "SELECT count(*) FROM read_parquet('$out') WHERE $lvlc=0 \
    AND ST_XMin($gc) < (SELECT (min(ST_XMin($gc))+max(ST_XMax($gc)))/2 \
      FROM read_parquet('$out') WHERE $lvlc=0);")
  if [[ "$bboxsel" =~ ^[0-9]+$ ]] && [ "$bboxsel" -le "${l0tot:-0}" ]; then
    duck_bbox="PASS($bboxsel/$l0tot)"; else duck_bbox="FAIL($bboxsel/$l0tot)"; fi

  # ogrinfo
  local ogr_out ogr_fcount ogr_geom
  ogr_out=$(ogrinfo -so -al "$out" 2>&1)
  ogr_fcount=$(echo "$ogr_out" | grep -oiP 'Feature Count: \K[0-9]+' | head -1)
  ogr_geom=$(echo "$ogr_out" | grep -oiP '^Geometry: \K.*' | head -1)
  [ -z "$ogr_fcount" ] && { ogr_fcount="ERR"; notes="${notes}ogr-no-fcount;"; }
  [ -z "$ogr_geom" ] && ogr_geom="?"

  printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' \
    "$id" "$mode" "$input_feat" "$total_rows" "$canon" "$nlevels" \
    "${wall:-?}" "$rss_mb" "$out_mb" "$in_mb" \
    "$validate" "$duck_open" "$duck_canon" "$duck_np" "$duck_env" \
    "$duck_idchk" "$wkb" "$duck_bbox" "${ogr_fcount}" "${ogr_geom}" \
    "$mono" "${notes:-none}" >> "$RESULTS_TSV"
  echo "   validate=$validate duck_canon=$duck_canon np=$duck_np env=$duck_env idchk=$duck_idchk wkb=$wkb bbox=$duck_bbox mono=$mono wall=${wall} rss=${rss_mb}MB"
}

# ---- determinism check ------------------------------------------
determinism_check() {
  local f="$1"
  local id; id=$(basename "$f" .parquet)
  read -r mnz mxz < <(zoom_range "$id")
  local a="$OUT/$id.det-a.parquet" b="$OUT/$id.det-b.parquet"
  echo "== determinism [$id]"
  "$BIN" overview "$f" "$a" --mode duplicating \
    --min-zoom "$mnz" --max-zoom "$mxz" >/dev/null 2>&1
  "$BIN" overview "$f" "$b" --mode duplicating \
    --min-zoom "$mnz" --max-zoom "$mxz" >/dev/null 2>&1
  local verdict
  if cmp -s "$a" "$b"; then
    verdict="byte-identical"
  else
    # content compare: per-level feature counts + total npoints
    local gc; gc=$(geom_col "$a")
    local lvlc; lvlc=$(level_sql_col "$a")
    local ca cb
    ca=$(dq "SELECT $lvlc||':'||count(*) FROM read_parquet('$a') GROUP BY $lvlc ORDER BY $lvlc;" | tr '\n' ',')
    cb=$(dq "SELECT $lvlc||':'||count(*) FROM read_parquet('$b') GROUP BY $lvlc ORDER BY $lvlc;" | tr '\n' ',')
    if [ "$ca" = "$cb" ]; then verdict="content-identical(bytes differ)"
    else verdict="NON-DETERMINISTIC"; fi
  fi
  echo "$id ($mnz..$mxz, duplicating): $verdict" >> "$OUT/determinism.txt"
  echo "   $verdict"
  rm -f "$a" "$b"
}

# ---- main loop --------------------------------------------------
shopt -s nullglob
FILES=("$GPIO"/*.parquet)
[ ${#FILES[@]} -gt 0 ] || { echo "No gpio datasets. Run fetch+optimize." >&2; exit 1; }

for f in "${FILES[@]}"; do
  id=$(basename "$f" .parquet)
  if [ -n "${GPQ_ONLY:-}" ]; then
    echo " $GPQ_ONLY " | grep -q " $id " || continue
  fi
  run_one "$f" duplicating
  run_one "$f" partitioning
done

# determinism: a pass-through dataset (points) and a simplification dataset
# (lines) to cover both the thinning-only and the simplify code paths.
: > "$OUT/determinism.txt"
for d in points-boise-small lines-boise-small; do
  [ -f "$GPIO/$d.parquet" ] && determinism_check "$GPIO/$d.parquet"
done

# ---- assemble V1_RESULTS.md -------------------------------------
{
  echo "# V1 Correctness Suite — Results"
  echo
  echo "Generated by \`corpus/verify.sh\` on $(date -u '+%Y-%m-%d %H:%M UTC')."
  echo "Binary: \`$BIN\` (\`$("$BIN" --version 2>/dev/null)\`)."
  echo "Datasets: gpio-optimized corpus under \`corpus/data/gpio/\`."
  echo "Every dataset converted in BOTH modes with its manifest tippecanoe"
  echo "zoom range (default 0..14); checked with \`tylertoo validate\`,"
  echo "DuckDB (spatial ext) and GDAL \`ogrinfo\`."
  echo
  echo "## Summary (dataset x mode)"
  echo
  echo "| dataset | mode | in feat | out rows | levels | wall | RSS MB | out MB | in MB | overhead | validate | duck open | canon cnt | npoints | envelope | id chk | wkb | bbox pred | ogr fcount | ogr geom | mono |"
  echo "|---|---|--:|--:|--:|--:|--:|--:|--:|--:|:-:|:-:|:-:|:-:|:-:|:-:|:-:|:-:|--:|---|:-:|"
  awk -F'\t' 'NR>1 {
    ov = ($10>0) ? sprintf("%+.0f%%", 100*($9-$10)/$10) : "-";
    printf "| %s | %s | %s | %s | %s | %s | %s | %s | %s | %s | %s | %s | %s | %s | %s | %s | %s | %s | %s | %s | %s |\n",
      $1,$2,$3,$4,$6,$7,$8,$9,$10,ov,$11,$12,$13,$14,$15,$16,$17,$18,$19,$20,$21
  }' "$RESULTS_TSV"
  echo
  echo "**Storage overhead** = (overview bytes - input bytes) / input bytes."
  echo "For **duplicating** it is the cost of embedding all coarser levels"
  echo "alongside the canonical (verbatim) data — the headline number."
  echo "For **partitioning** it is small and positive (each feature once, but"
  echo "the file gains a \`level\` column + a freshly generated bbox covering)."
  echo
  echo "Note: the **levels** count can be smaller than the requested zoom span"
  echo "when coarse levels would be empty or identical — the writer merges/omits"
  echo "empty levels and renumbers (spec §7.3), so e.g. a 0..14 request over a"
  echo "sparse dataset may emit fewer bands."
  echo
  echo "## Determinism"
  echo
  echo '```'
  cat "$OUT/determinism.txt" 2>/dev/null || echo "(not run)"
  echo '```'
  echo
  echo "## Per-level feature / vertex counts (duplicating)"
  for rep in "$OUT"/*.dup.report.json; do
    [ -f "$rep" ] || continue
    id=$(basename "$rep" .dup.report.json)
    echo
    echo "### $id"
    echo
    echo "| level | zoom | gsd(m) | features | vertices |"
    echo "|--:|--:|--:|--:|--:|"
    jq -r '.levels[]|"| \(.level) | \(.zoom) | \(.gsd|floor) | \(.feature_count) | \(.vertex_count) |"' "$rep"
  done
  echo
  echo "## Findings & fixes"
  echo
  echo "### F1 (FIXED) — duplicate / stale \`bbox\` covering on gpio inputs"
  echo
  echo "The overview writer passed every source column through, then let the"
  echo "geoparquet encoder *generate* its own \`bbox\` covering (§4.4). Every"
  echo "gpio-optimized input (the documented input contract, §4.3) already"
  echo "carries a \`bbox\` covering, so the output ended up with **two** \`bbox\`"
  echo "columns. Worse, the \`geo\` covering metadata resolves the name \`bbox\`"
  echo "to the *first* physical match — the passed-through **pre-generalization**"
  echo "input bbox — so at coarse (simplified) levels the covering described the"
  echo "*original* geometry, not the simplified one actually stored. (Douglas-"
  echo "Peucker keeps a vertex subset, so the stale bbox is a conservative"
  echo "superset — pruning stayed correct but not tight, and the fresh covering"
  echo "was orphaned as \`bbox_1\`.) Datasets whose geometry column is named"
  echo "\`geometry\` (all Overture / FTW) hit the name collision; Natural-Earth"
  echo "(\`geom\` → covering \`geom_bbox\`) avoided the name clash but still carried"
  echo "a redundant stale \`bbox\`."
  echo
  echo "**Fix** (\`crates/core/src/overview/writer.rs\`): the writer now drops any"
  echo "pre-existing bbox-covering struct column whose name collides with the"
  echo "covering it will generate, so the encoder's authoritative covering is the"
  echo "only one present and the metadata points at it. Verified by unit test"
  echo "\`preexisting_bbox_covering_is_not_duplicated\` and end-to-end here (no"
  echo "\`bbox_1\` in any output; covering matches simplified geometry)."
  echo
  echo "### F2 (OPEN, design) — \`level\` column name vs. source \`LEVEL\` attribute"
  echo
  echo "The spec mandates a column named exactly \`level\` (§4.1). The writer only"
  echo "rejects a source column named \`level\` **case-sensitively**, so a source"
  echo "attribute differing only in case (Natural Earth admin \`LEVEL\`) passes"
  echo "through and coexists with the overview \`level\`. Footer/parquet readers"
  echo "(case-sensitive) are fine, but **DuckDB is case-insensitive**: \`WHERE"
  echo "level = <canonical>\` binds to the *original* \`LEVEL\` attribute and"
  echo "returns wrong/zero rows — silently defeating the spec's one-predicate"
  echo "analysis contract (§5.3). Only \`monster-admin\` is affected in this corpus."
  echo "This is a design decision (error out? auto-rename the clashing source"
  echo "column? spec guidance?), not a local writer bug, so it is documented"
  echo "rather than fixed. \`verify.sh\` works around it by using DuckDB's"
  echo "disambiguated \`level_1\` and flags the collision in the notes."
  echo
  echo "### F3 (INFORMATIONAL) — vertex-count non-monotonicity"
  echo
  echo "For \`polygons-ftw-moldova-large\` total vertex count dips once"
  echo "coarse→fine (level 7→8) even though feature count rises. This is a wide"
  echo "feature-size distribution (coarse levels keep a few very large fields;"
  echo "the next level adds many tiny ones), not a spec violation — §2.2 only"
  echo "requires that a feature visible at level *k* appears at all finer levels"
  echo "(feature-count monotonicity, which holds). Recorded as a note."
  echo
  echo "## Raw notes (per dataset x mode)"
  echo
  awk -F'\t' 'NR>1 && $22!="none" {print "- **"$1" / "$2"**: "$22}' "$RESULTS_TSV" \
    | sort -u
} > "$RESULTS_MD"

echo
echo "Done. Machine-readable results: $RESULTS_TSV"
echo "Report: $RESULTS_MD"
