#!/usr/bin/env bash
#
# overture.sh — the tippecanoe comparison on Overture data at scale (#664).
#
# Runs on one fresh Ubuntu 24.04 x86 VM in AWS us-west-2, next to the
# Overture bucket. Every step is a subcommand, so a run can resume where it
# stopped:
#
#   ./overture.sh setup              # NVMe, toolchain, tylertoo, tippecanoe
#   ./overture.sh prep places        # gpio: Overture release -> sorted input
#   ./overture.sh run places         # run_e2e.py, defaults + quality-matched
#   ./overture.sh render places      # render_parity.py on both archive pairs
#   ./overture.sh publish places S3  # archives + results + renders to S3
#   ./overture.sh all places         # prep, run, render
#
# Everything lands under $DATA (default /data), which `setup` mounts on the
# instance-store NVMe. tippecanoe 2.79.0 writes its temp files to a
# hard-coded /tmp (main.cpp:2994) unless given -t, so `setup` bind-mounts
# $DATA/tmp over /tmp. Both tools then spool to NVMe with the same argv the
# committed record used.
#
# Pins. Change them here, and the provenance follows into results.json via
# the harness and into $DATA/provenance-<id>.json via `run`.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/../.." && pwd)"
DATA="${DATA:-/data}"

OVERTURE_RELEASE="2026-09-23.1"
OVERTURE="s3://overturemaps-us-west-2/release/$OVERTURE_RELEASE"
GPIO_VERSION="1.6.0"
REPEAT="${REPEAT:-3}"
RENDER_ZOOMS="${RENDER_ZOOMS:-2,4,6}"

# --- datasets ----------------------------------------------------------------
# One function per dataset sets SRC (a glob over the release), COLS
# (the columns kept) and WHERE (a DuckDB filter, empty for none).
#
# COLS keeps flat scalar columns only. Overture's nested structs (names,
# categories, addresses, sources) have no FlatGeobuf representation, so
# keeping them would hand the two tools different attribute sets. Overture's
# own `bbox` is dropped too: its struct order is xmin, xmax, ymin, ymax, which
# GeoParquet 1.1 does not allow as a covering, so gpio 1.6.0 refuses to
# declare it. `gpio sort hilbert --add-bbox` writes a spec-order one instead.
dataset() {
  case "$1" in
    places)
      SRC="$OVERTURE/theme=places/type=place/*.parquet"
      COLS="id,confidence,basic_category,operating_status"
      WHERE=""
      ;;
    *)
      echo "unknown dataset: $1 (known: places)" >&2
      exit 1
      ;;
  esac
}

log() { printf '\n[%s] %s\n' "$(date -u +%H:%M:%SZ)" "$*"; }

# --- setup -------------------------------------------------------------------
mount_nvme() {
  if mountpoint -q "$DATA"; then
    log "$DATA already mounted"
    return
  fi
  # Instance-store disks only; the EBS root also shows up as NVMe.
  mapfile -t disks < <(lsblk -dpno NAME,MODEL \
    | awk '/Instance Storage/ {print $1}')
  if [ "${#disks[@]}" -eq 0 ]; then
    echo "no instance-store NVMe found; pick an instance type with local NVMe" >&2
    exit 1
  fi
  sudo mkdir -p "$DATA"
  if [ "${#disks[@]}" -eq 1 ]; then
    dev="${disks[0]}"
  else
    dev=/dev/md0
    sudo mdadm --create "$dev" --run --level=0 \
      --raid-devices="${#disks[@]}" "${disks[@]}"
  fi
  sudo mkfs.ext4 -F -E nodiscard -m 0 "$dev"
  sudo mount -o noatime "$dev" "$DATA"
  sudo chown "$(id -u):$(id -g)" "$DATA"
  mkdir -p "$DATA/tmp"
  chmod 1777 "$DATA/tmp"
  sudo mount --bind "$DATA/tmp" /tmp
  log "mounted ${disks[*]} on $DATA; /tmp is $DATA/tmp"
}

cmd_setup() {
  sudo apt-get update -q
  sudo DEBIAN_FRONTEND=noninteractive apt-get install -yq \
    build-essential git curl time pkg-config mdadm \
    libsqlite3-dev zlib1g-dev python3 unzip protobuf-compiler
  mount_nvme

  if ! command -v cargo >/dev/null; then
    curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
  fi
  # shellcheck disable=SC1091
  . "$HOME/.cargo/env"
  if ! command -v uv >/dev/null; then
    curl -LsSf https://astral.sh/uv/install.sh | sh
  fi
  export PATH="$HOME/.local/bin:$PATH"
  uv tool install --force "geoparquet-io==$GPIO_VERSION"
  if ! command -v aws >/dev/null; then
    curl -sSf -o /tmp/awscli.zip \
      https://awscli.amazonaws.com/awscli-exe-linux-x86_64.zip
    (cd /tmp && unzip -qo awscli.zip && sudo ./aws/install --update)
  fi

  log "building tylertoo $(git -C "$ROOT" rev-parse --short HEAD)"
  CARGO_TARGET_DIR="$DATA/target" \
    cargo build --release -p tylertoo --manifest-path "$ROOT/Cargo.toml"
  "$HERE/setup_tippecanoe.sh"
  log "setup done"
}

env_for_run() {
  # shellcheck disable=SC1091
  . "$HOME/.cargo/env"
  export PATH="$HOME/.local/bin:$PATH"
  export TYLERTOO="$DATA/target/release/tylertoo"
  [ -x "$TYLERTOO" ] || { echo "run setup first" >&2; exit 1; }
}

# --- prep --------------------------------------------------------------------
cmd_prep() {
  env_for_run
  dataset "$1"
  local dir="$DATA/input"
  mkdir -p "$dir"
  local raw="$dir/$1.raw.parquet" out="$dir/$1.parquet"
  local args=(extract geoparquet "$SRC" "$raw" --overwrite --skip-count
              --include-cols "$COLS" --exclude-cols bbox)
  [ -n "$WHERE" ] && args+=(--where "$WHERE")

  log "gpio ${args[*]}"
  /usr/bin/time -v gpio "${args[@]}" 2> >(tee "$dir/$1.extract.time" >&2)
  # 128 MB row groups: the size docs/guides/remote-reads.md recommends, and
  # big enough that tylertoo's row-group parallelism is not footer-bound.
  log "gpio sort hilbert $raw -> $out"
  /usr/bin/time -v gpio sort hilbert "$raw" "$out" --overwrite \
    --add-bbox --row-group-size-mb 128 \
    2> >(tee "$dir/$1.sort.time" >&2)
  rm -f "$raw"
  gpio check all "$out" || true
  log "prepared $out ($(du -h "$out" | cut -f1))"
}

# --- run ---------------------------------------------------------------------
cmd_run() {
  env_for_run
  dataset "$1"
  local src="$DATA/input/$1.parquet" work="$DATA/work/$1"
  [ -f "$src" ] || { echo "run prep $1 first" >&2; exit 1; }
  mkdir -p "$work" "$DATA/results"
  provenance "$1" > "$DATA/results/provenance-$1.json"
  log "run_e2e.py on $src"
  python3 "$HERE/run_e2e.py" \
    --dataset "$1=$src" --only "$1" \
    --quality-matched --repeat "$REPEAT" \
    --work "$work" --keep \
    --out "$DATA/results/results-$1.json" \
    --markdown "$DATA/results/results-$1.md" \
    2>&1 | tee "$DATA/results/run-$1.log"
}

provenance() {
  dataset "$1"
  local imds=http://169.254.169.254/latest
  local token
  token=$(curl -s -X PUT "$imds/api/token" \
    -H "X-aws-ec2-metadata-token-ttl-seconds: 60" || true)
  md() { curl -s -H "X-aws-ec2-metadata-token: $token" "$imds/meta-data/$1" || true; }
  P_dataset="$1" \
  P_overture_release="$OVERTURE_RELEASE" \
  P_source="$SRC" \
  P_columns="$COLS" \
  P_where="$WHERE" \
  P_instance_type="$(md instance-type)" \
  P_availability_zone="$(md placement/availability-zone)" \
  P_ami="$(md ami-id)" \
  P_kernel="$(uname -r)" \
  P_os="$(. /etc/os-release && echo "$PRETTY_NAME")" \
  P_cpu="$(grep -m1 'model name' /proc/cpuinfo | cut -d: -f2- | xargs)" \
  P_tylertoo="$("$TYLERTOO" --version)" \
  P_tylertoo_git="$(git -C "$ROOT" rev-parse HEAD)" \
  P_gpio="$(gpio --version)" \
  P_tippecanoe="$(tippecanoe_bin --version 2>&1 | head -1)" \
  P_nvme="$(df -h "$DATA" | tail -1 | awk '{print $1" "$2}')" \
  python3 -c '
import json, os
print(json.dumps({k[2:]: v for k, v in sorted(os.environ.items())
                  if k.startswith("P_")}, indent=2))'
}

tippecanoe_bin() {
  "$ROOT/.tools/tippecanoe-2.79.0/bin/tippecanoe" "$@"
}

# --- render ------------------------------------------------------------------
cmd_render() {
  env_for_run
  local work="$DATA/work/$1" out="$DATA/results/renders-$1"
  local tip="$work/$1.tippecanoe-fgb.pmtiles"
  for side in tylertoo tylertoo-qm; do
    log "render $side vs tippecanoe at z$RENDER_ZOOMS"
    uv run --with pmtiles --with mapbox-vector-tile --with matplotlib \
      python3 "$HERE/render_parity.py" \
      "$work/$1.$side.pmtiles" "$tip" \
      --zooms "$RENDER_ZOOMS" --labels "$side,tippecanoe" \
      --out "$out/$side"
  done
}

# --- publish -----------------------------------------------------------------
cmd_publish() {
  local dest="${2:?usage: publish <dataset> s3://bucket/prefix}"
  local work="$DATA/work/$1"
  for f in "$work/$1".*.pmtiles; do
    aws s3 cp "$f" "$dest/$1/$(basename "$f")"
  done
  aws s3 cp --recursive "$DATA/results" "$dest/$1/results" \
    --exclude "*" --include "*$1*"
}

case "${1:-}" in
  setup) cmd_setup ;;
  prep) cmd_prep "${2:?dataset}" ;;
  run) cmd_run "${2:?dataset}" ;;
  render) cmd_render "${2:?dataset}" ;;
  publish) cmd_publish "${2:?dataset}" "${3:-}" ;;
  all)
    cmd_prep "${2:?dataset}"
    cmd_run "$2"
    cmd_render "$2"
    ;;
  *)
    sed -n '3,15p' "$0"
    exit 1
    ;;
esac
