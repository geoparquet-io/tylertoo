#!/usr/bin/env bash
# #290 remote-staging sweep: concurrency x coalescing window x HTTP client,
# over one object reachable on two hosts (the source.coop proxy and the S3
# bucket behind it), plus a localhost server that emulates a host.
#
#   GRID=main      concurrency {1,2,4,8,16} x window {0,16,64,128 MiB}
#   GRID=http      HTTP/1.1 (default) vs HTTP/2, at CELLS (see below)
#   GRID=baseline  single-stream curl / aws s3 cp of the whole object
#
#   HOSTS="coop s3"   which hosts to run (coop, s3, emu)
#   TTFB_MS / CONN_BW_MBPS  the emu host's profile (logging_server.py); emu
#                     rows are labelled emu:<ttfb>ms:<bw>MBps
#   CONCS="1 2 4 8 16"     main-grid concurrency ceilings
#   WINDOWS="0 16 64 128"  main-grid windows in MiB (0 = off). On a host
#                     slower than ~4 MB/s per connection, drop 128: a span
#                     that takes over 30 s hits object_store's default
#                     request timeout and staging falls back (RESULTS.md)
#   N=3               repetitions per cell (median is reported by summarize.py)
#   MAX_ZOOM=6        keeps the passes short, so staging dominates the wall
#
# Every run appends one CSV row to $OUT/sweep.csv:
#   host,grid,concurrency,window,http,rep,live_conc,stage_s,wall_s,requests,bytes,connections
# `connections` is filled only on the emu host (the server counts accepts).
# Build without --all-features: the dhat-heap feature skews every timing.
set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/../.." && pwd)"
BIN="$ROOT/target/release/tylertoo"
OUT="${OUT:-$HERE/sweep_out}"
SCRATCH="${SCRATCH:-$OUT/scratch}"
KEY="${KEY:-ftw/global-data-2e/vector/2017/zone=25/utm25.parquet}"
BUCKET="us-west-2.opendata.source.coop"
GRID="${GRID:-main}"
HOSTS="${HOSTS:-coop s3}"
N="${N:-3}"
MAX_ZOOM="${MAX_ZOOM:-6}"
PORT="${PORT:-8902}"
MIB=$((1024 * 1024))
mkdir -p "$OUT" "$SCRATCH"
OUT="$(cd "$OUT" && pwd)"
SCRATCH="$(cd "$SCRATCH" && pwd)"
cd "$SCRATCH" || exit 1
EMU_LABEL="emu:${TTFB_MS:-0}ms:${CONN_BW_MBPS:-0}MBps"

[ -x "$BIN" ] || { echo "build the release binary first (cargo build --release -p tylertoo --all-features)" >&2; exit 1; }
CSV="$OUT/sweep.csv"
[ -s "$CSV" ] || echo "host,grid,concurrency,window,http,rep,live_conc,stage_s,wall_s,requests,bytes,connections" >"$CSV"
echo "# commit $(git -C "$ROOT" rev-parse --short HEAD) $(date -u +%FT%TZ) GRID=$GRID HOSTS=$HOSTS N=$N" >>"$OUT/runs.log"

url_for() {
  case "$1" in
    coop) echo "https://data.source.coop/$KEY" ;;
    s3) echo "s3://$BUCKET/$KEY" ;;
    emu) echo "http://127.0.0.1:$PORT/$(basename "$KEY")" ;;
  esac
}

start_emu() {
  # Emulated host: TTFB_MS / CONN_BW_MBPS are read by logging_server.py.
  local local_copy="$SCRATCH/$(basename "$KEY")"
  [ -s "$local_copy" ] || curl -sf -o "$local_copy" "https://data.source.coop/$KEY"
  python3 -I "$ROOT/benchmarks/overview/logging_server.py" "$SCRATCH" "$PORT" >"$OUT/emu.log" 2>&1 &
  EMU=$!
  trap 'kill $EMU 2>/dev/null' EXIT
  for _ in $(seq 1 30); do curl -sf "http://127.0.0.1:$PORT/__stats" >/dev/null && return; sleep 0.3; done
  echo "emu server failed to start" >&2; exit 1
}

# run_cell HOST GRID CONCURRENCY WINDOW_BYTES HTTP(1|2) REP
run_cell() {
  local host="$1" grid="$2" conc="$3" window="$4" http="$5" rep="$6"
  local label="$host"
  [ "$host" = emu ] && label="$EMU_LABEL"
  local tag="${label//:/_}-$grid-c$conc-w$window-h$http-r$rep"
  local log="$OUT/logs/$tag.log" rf="$OUT/logs/$tag.report.json" tf="$OUT/logs/$tag.time"
  local dst="$SCRATCH/$tag.parquet"
  mkdir -p "$OUT/logs"
  [ "$host" = emu ] && curl -sf "http://127.0.0.1:$PORT/__reset" >/dev/null
  local http_env=()
  [ "$http" = 2 ] && http_env=(TYLERTOO_HTTP_HTTP2_ONLY=true)
  rm -f "$dst"
  env TYLERTOO_STAGING_CONCURRENCY="$conc" TYLERTOO_STAGING_COALESCE_BYTES="$window" \
    "${http_env[@]}" \
    /usr/bin/time -f "%e" -o "$tf" \
    "$BIN" overview "$(url_for "$host")" "$dst" --min-zoom 0 --max-zoom "$MAX_ZOOM" \
    --report "$rf" >"$log" 2>&1
  local rc=$?
  rm -f "$dst"
  local stage live wall requests bytes conns=""
  stage=$(grep -oP 'staged \d+ selected row group\(s\) to local disk in \K[0-9.]+' "$log" | head -1)
  live=$(grep -oP '\[stage\] live concurrency \K\d+' "$log" | head -1)
  wall=$(tail -1 "$tf" 2>/dev/null)
  requests=$(jq -r '.remote_fetch.requests // empty' "$rf" 2>/dev/null)
  bytes=$(jq -r '.remote_fetch.bytes_fetched // empty' "$rf" 2>/dev/null)
  [ "$host" = emu ] && conns=$(curl -sf "http://127.0.0.1:$PORT/__stats" | jq -r .connections)
  if [ "$rc" != 0 ]; then
    echo "FAIL $tag (exit $rc), see $log" | tee -a "$OUT/runs.log"
    return
  fi
  echo "$label,$grid,$conc,$window,$http,$rep,$live,$stage,$wall,$requests,$bytes,$conns" | tee -a "$CSV"
}

case " $HOSTS " in *" emu "*) start_emu ;; esac

case "$GRID" in
  main)
    for rep in $(seq 1 "$N"); do
      for host in $HOSTS; do
        for conc in ${CONCS:-1 2 4 8 16}; do
          for w in ${WINDOWS:-0 16 64 128}; do
            run_cell "$host" main "$conc" $((w * MIB)) 1 "$rep"
          done
        done
      done
    done
    ;;
  http)
    # CELLS: space-separated concurrency:window_mib pairs, e.g. "8:0 2:128".
    for rep in $(seq 1 "$N"); do
      for host in $HOSTS; do
        for cell in ${CELLS:-8:0 4:16}; do
          for http in 1 2; do
            # Only source.coop (Cloudflare) negotiates HTTP/2: S3 and the
            # emu server speak HTTP/1.1, where http2_only cannot connect.
            [ "$host" != coop ] && [ "$http" = 2 ] && continue
            run_cell "$host" http "${cell%%:*}" $(( ${cell##*:} * MIB )) "$http" "$rep"
          done
        done
      done
    done
    ;;
  baseline)
    for rep in $(seq 1 "$N"); do
      t=$( { /usr/bin/time -f "%e" curl -sf -o /dev/null "https://data.source.coop/$KEY"; } 2>&1 | tail -1)
      echo "coop,baseline-curl,1,0,1,$rep,,,$t,1,," | tee -a "$CSV"
      t=$( { /usr/bin/time -f "%e" env -u AWS_PROFILE aws s3 cp --no-sign-request --region us-west-2 --quiet \
        "s3://$BUCKET/$KEY" "$SCRATCH/awscp.parquet"; } 2>&1 | tail -1)
      rm -f "$SCRATCH/awscp.parquet"
      echo "s3,baseline-awscli,10,8388608,1,$rep,,,$t,,," | tee -a "$CSV"
    done
    ;;
  *) echo "unknown GRID=$GRID" >&2; exit 2 ;;
esac
