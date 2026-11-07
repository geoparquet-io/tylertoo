#!/usr/bin/env bash
# Re-measures for #772 (factor 0 vs defaults) and #732 (wave auto under cgroup caps).
# Recorded as run on the #810 bench host; T, IN and W are that host's paths.
# 3 interleaved rounds per cell, warm cache, outputs deleted between runs.
set -uo pipefail
T=/home/ubuntu/bench-data/810/bin/tylertoo-head-ae0363f
IN=/home/ubuntu/bench-data/810/inputs
W=/home/ubuntu/bench-data/810/remeasure; rm -rf $W; mkdir -p $W/out
export TMPDIR=$W/tmp; mkdir -p $TMPDIR
OUT=$W/runs.jsonl

run() { # cell cap cmd...
  local cell=$1 cap=$2; shift 2
  local prof=$W/prof-$cell.jsonl pre=()
  [ "$cap" != none ] && pre=(systemd-run --user --scope -q -p MemoryMax=$cap -p MemorySwapMax=0)
  rm -f $W/out/out.pmtiles; sync
  local t0=$(date +%s.%N)
  TYLERTOO_PROFILE_JSON=$prof "${pre[@]}" /usr/bin/time -f '%e %M' -o $W/time "$@" $W/out/out.pmtiles >$W/last.log 2>&1
  local rc=$?
  read wall rss < <(tail -1 $W/time)
  local sha=$( [ $rc = 0 ] && sha256sum $W/out/out.pmtiles | cut -c1-16 || echo -)
  python3 - "$cell" "$cap" "$rc" "${wall:-0}" "${rss:-0}" "$sha" "$prof" >>$OUT <<'EOF'
import json,sys
cell,cap,rc,wall,rss,sha,prof=sys.argv[1:]
p={}
try: p=json.loads(open(prof).read().splitlines()[-1])
except Exception: pass
ex=p.get('export') or {}
pz=ex.get('per_zoom') or []
print(json.dumps({"cell":cell,"cap":cap,"rc":int(rc),"wall_s":float(wall),"peak_rss_mib":int(rss)//1024,
  "sha":sha,"partition_wave_width":ex.get('partition_wave_width'),"waves_total":ex.get('waves_total'),
  "wave_widths":sorted({z.get('wave_width') for z in pz if z.get('wave_width') is not None}),
  "convert_s":(p.get('convert') or {}).get('wall_secs'),"export_wall":ex.get('wall_secs')}))
EOF
  echo "$cell $cap rc=$rc wall=$wall rss=$((${rss:-0}/1024))MiB sha=$sha"
}

for r in 1 2 3; do
  run mold-default none $T tiles $IN/moldova-hilbert.parquet --min-zoom 0 --max-zoom 12
  run mold-factor0 none $T tiles $IN/moldova-hilbert.parquet --min-zoom 0 --max-zoom 12 --simplify-factor 0
done
NZ="$T tiles $IN/nz-parcels-hilbert.parquet --min-zoom 0 --max-zoom 14"
for r in 1 2 3; do
  run nz-auto-nocap none $NZ
  run nz-auto-16G 16G $NZ
  run nz-auto-8G 8G $NZ
  run nz-w16-16G 16G $NZ --partition-wave 16
  run nz-w64-16G 16G $NZ --partition-wave 64
done
echo done
