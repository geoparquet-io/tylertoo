#!/usr/bin/env bash
# Manual release tier. Estimated box wall: build 3 min; tippecanoe 0-10 min;
# corpus ~40 min; cells ~20 min; Africa ~54 min; tutorial-map 20-35 min (untimed);
# upload ~5 min; docs ~1 min. About 2.5 h in all; --skip africa saves about 1 h.
# Corpus: one warm-up + three alternating rounds per tool (median of three).
# Cells: manifest defaults. Africa: one default-profile run, no warm-up.
# All scratch I/O lives under --data (default: ${TYLERTOO_BENCH_DATA:-$HOME/bench-data}).
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
STAGES=(build tippecanoe corpus cells africa tutorial-map upload docs)
# The committed Africa default peaked at 146 GB (results/overture-buildings-africa.json peak_disk_bytes); retain headroom.
AFRICA_DISK_GB=200
DATA="${TYLERTOO_BENCH_DATA:-$HOME/bench-data}"
FORCE=0 DRY=0 PRERELEASE=()
skip="${SKIP:-}" only="${ONLY:-}"
usage() { echo 'benchmarks/release.sh [--data DIR (all scratch; default ${TYLERTOO_BENCH_DATA:-$HOME/bench-data})] [--force] [--dry-run] [--prerelease] [--skip "S ..."] [--only "S ..."] [STAGE ...]'; }
invalid() { echo "Unknown stage or option: $1. Stages: ${STAGES[*]}" >&2; exit 2; }
while (($#)); do
  case "$1" in
    --data) [[ $# -ge 2 ]] || { usage >&2; exit 2; }; DATA="$2"; shift 2 ;;
    --force) FORCE=1; shift ;;
    --dry-run) DRY=1; shift ;;
    # Publish from a clean tree past the version's tag, labelled by git describe.
    --prerelease) PRERELEASE=(--prerelease); shift ;;
    --skip|--only)
      option="$1"; shift
      [[ $# -gt 0 && "$1" != --* ]] || { usage >&2; exit 2; }
      while (($#)) && [[ "$1" != --* ]]; do
        if [[ "$option" == --skip ]]; then skip+=" $1"; else only+=" $1"; fi
        shift
      done ;;
    --help|-h) usage; echo "Stages: ${STAGES[*]}"; exit 0 ;;
    --*) invalid "$1" ;;
    *) only+=" $1"; shift ;;
  esac
done
for stage in $skip $only; do
  [[ " ${STAGES[*]} " == *" $stage "* ]] || invalid "$stage"
done
selected=()
for stage in "${STAGES[@]}"; do
  [[ -z "$only" || " $only " == *" $stage "* ]] || continue
  [[ " $skip " != *" $stage "* ]] || continue
  selected+=("$stage")
done
has() { [[ " ${selected[*]} " == *" $1 "* ]]; }
PY=(uv run --no-project --python 3.12 python -I)
# Resolve local metadata offline even in a preview; no stage/check runs here.
META=(uv run --offline --no-project --python 3.12 python -I)
VERSION="$("${META[@]}" benchmarks/run_manifest.py release-version "${PRERELEASE[@]}")"
RC=1
if [[ -z "$VERSION" ]]; then
  RC=0
  VERSION="$("${META[@]}" -c 'import sys; sys.path.insert(0,"benchmarks"); import run_manifest as r; print(r.git_describe() or "unknown")')"
fi
OUT="$DATA/release/$VERSION"
TIP_PIN="$("${META[@]}" benchmarks/e2e/run_e2e.py --print-tippecanoe-pin)"
TIP="${TIPPECANOE_TOOLS_DIR:-$ROOT/.tools}/tippecanoe-$TIP_PIN/bin/tippecanoe"
GPIO_PACKAGE="$("${META[@]}" -c 'import tomllib; print(tomllib.load(open("benchmarks/coalesce/bench.toml","rb"))["tippecanoe"]["gpio"])')"
GPIO_PIN="${GPIO_PACKAGE#geoparquet-io==}"
echo "Stages: ${selected[*]}"
echo "Output: $OUT"
echo 'Preflight: benchmarks/run_manifest.py release-version'
if (( ! RC )); then
  echo 'Not a release candidate: source changes, an existing release tag at another commit, or unavailable git metadata; outputs stay local.'
  for stage in tutorial-map upload docs; do has "$stage" && echo "$stage: refuse (not a release candidate)"; done
fi
# Preview every command through this function; quoting also documents paths with spaces.
run() { printf '  '; printf '%q ' "$@"; echo; if (( ! DRY )); then "$@"; fi; }
tip_check_pending=1
check_tip() {
  [[ "$("$TIP" --version 2>&1)" == "tippecanoe v$TIP_PIN" ]] || { echo "Expected tippecanoe v$TIP_PIN at $TIP" >&2; exit 1; }
  tip_check_pending=0
}
echo "Preflight: $TIP --version must equal tippecanoe v$TIP_PIN"
if has tippecanoe; then echo 'Preflight: check tippecanoe after setup, before timed stages'; fi
echo "Preflight: uvx --from $GPIO_PACKAGE gpio --version must equal $GPIO_PIN"
if has africa; then
  echo "Preflight: at least $AFRICA_DISK_GB GB free under $DATA"
  echo 'Preflight: benchmarks/showcase/showcase.py check-pins'
fi
if has upload; then
  echo 'Preflight: aws s3 ls s3://nlebovits/tylertoo-benchmarks/ --profile source-coop; on failure run `source-coop login`'
fi
if (( ! DRY )); then
  mkdir -p "$DATA"
  exec 9>"$DATA/release.lock"
  flock -n 9 || { echo "Release lock held: $DATA/release.lock" >&2; exit 1; }
  if (( ! RC )) && { has tutorial-map || has upload || has docs; }; then
    echo 'tutorial-map, upload, and docs refuse: not a release candidate (source changes, an existing release tag at another commit, or unavailable git metadata).' >&2
    exit 1
  fi
  if has corpus && ! has tippecanoe; then check_tip; fi
  gpio_version="$(uvx --from "$GPIO_PACKAGE" gpio --version)"
  [[ "${gpio_version##* }" == "$GPIO_PIN" ]] || { echo "Expected gpio $GPIO_PIN; got $gpio_version" >&2; exit 1; }
  if has africa; then
    "${PY[@]}" -c 'import shutil,sys; free=shutil.disk_usage(sys.argv[1]).free; need=int(sys.argv[2])*10**9; sys.exit(0 if free>=need else f"Need at least {sys.argv[2]} GB free under {sys.argv[1]}")' "$DATA" "$AFRICA_DISK_GB"
    "${PY[@]}" benchmarks/showcase/showcase.py check-pins
  fi
  if has upload; then
    aws s3 ls s3://nlebovits/tylertoo-benchmarks/ --profile source-coop || { echo 'run `source-coop login`' >&2; exit 1; }
  fi
  mkdir -p "$OUT/bin" "$OUT/tmp"
  export TMPDIR="$OUT/tmp"
  printf '#!/usr/bin/env bash\nexec uvx --from %q gpio "$@"\n' "$GPIO_PACKAGE" > "$OUT/bin/gpio"
  chmod +x "$OUT/bin/gpio"
  export PATH="$OUT/bin:$PATH"
fi
extra=()
(( ! FORCE )) || extra+=(--force)
for stage in "${selected[@]}"; do
  if [[ -f "$OUT/$stage.done" && "$FORCE" == 0 ]]; then echo "$stage: skip (done)"; continue; fi
  if (( ! DRY && tip_check_pending )) && [[ "$stage" == corpus ]]; then check_tip; fi
  echo "$stage:"
  case "$stage" in
    build) run cargo build --locked --release -p tylertoo ;;
    tippecanoe)
      run benchmarks/e2e/setup_tippecanoe.sh
      if (( ! DRY )); then check_tip; fi
      echo "  tippecanoe path: $TIP" ;;
    corpus)
      run corpus/fetch.sh --large
      run corpus/optimize.sh
      datasets=()
      while IFS= read -r did; do datasets+=(--only "$did"); done < <("${META[@]}" -c 'import json; print("\n".join(json.load(open("benchmarks/results/e2e-corpus.json"))["datasets"]))')
      run "${PY[@]}" benchmarks/e2e/run_e2e.py --corpus --quality-matched --repeat 3 --keep \
        --tylertoo target/release/tylertoo --tippecanoe "$TIP" --work "$OUT/corpus" \
        "${datasets[@]}" --out "$OUT/e2e-corpus.json"
      run "${PY[@]}" -c 'import json,sys; expected=json.load(open("benchmarks/results/e2e-corpus.json"))["datasets"]; actual=json.load(open(sys.argv[1]))["datasets"]; sys.exit(0 if actual.keys()==expected.keys() else "Corpus incomplete: expected " + ", ".join(expected))' "$OUT/e2e-corpus.json" ;;
    cells)
      run "${PY[@]}" benchmarks/run_manifest.py run --set release --name release --data "$DATA" \
        --results "$OUT/cells" --bench-out "$OUT/scratch" --bin head=target/release/tylertoo "${extra[@]}"
      run "${PY[@]}" benchmarks/run_manifest.py report --name release --data "$DATA" \
        --results "$OUT/cells" --json-out "$OUT/release-cells.json" ;;
    africa)
      run "${PY[@]}" benchmarks/run_manifest.py --manifest benchmarks/showcase/manifest-africa.toml \
        run --cells ovt-s3 --name africa-s3 --data "$DATA" --results "$OUT/results/africa-s3" \
        --bench-out "$OUT/scratch" --bin head=target/release/tylertoo "${extra[@]}"
      # Pin numpy: pyarrow imports any numpy it can see, including a system 1.x it cannot use.
      run uv run --no-project --python 3.12 --with pyarrow==26.0.0 --with numpy==2.2.6 python -I \
        benchmarks/showcase/showcase.py collect-stream africa --data "$OUT" \
        --history-from benchmarks/results/overture-buildings-africa.json --out "$OUT/overture-buildings-africa.json" ;;
    tutorial-map)
      # MAP_FROM records an archive already built and published instead of rebuilding it.
      run "${PY[@]}" benchmarks/comparison.py tutorial-map --work "$OUT/tutorial-map" \
        --out "$OUT/tutorial-map.json" --tylertoo target/release/tylertoo ${MAP_FROM:+--from "$MAP_FROM"} \
        --target "${MAP_TARGET:-s3://nlebovits/tylertoo-brazil-demo/brazil.pmtiles}" ;;
    upload)
      run "${PY[@]}" benchmarks/comparison.py build --corpus "$OUT/e2e-corpus.json" \
        --work "$OUT/corpus" --out "$OUT/comparison-pmtiles.json"
      run "${PY[@]}" benchmarks/comparison.py upload --manifest "$OUT/comparison-pmtiles.json" \
        --work "$OUT/corpus" --target "${UPLOAD_TARGET:-s3://nlebovits/tylertoo-benchmarks}" "${extra[@]}" ;;
    docs)
      if (( DRY )); then echo "  cp $OUT/*.json benchmarks/results/"; else
        for result in "$OUT"/*.json; do [[ ! -f "$result" ]] || run cp "$result" benchmarks/results/; done
      fi
      run "${PY[@]}" scripts/gen_bench_docs.py
      run "${PY[@]}" scripts/gen_bench_docs.py --diff origin/main ;;
  esac
  if (( ! DRY )); then touch "$OUT/$stage.done"; fi
done
