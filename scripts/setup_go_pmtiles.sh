#!/usr/bin/env bash
#
# setup_go_pmtiles.sh — install the PINNED go-pmtiles release binary into
# ./.tools, for the independent-reader e2e check (#421).
#
# `pmtiles verify` is the reference structural validator for PMTiles v3
# archives (header, directories, clustering, zoom bounds). Our writer is
# self-implemented, so CI runs every archive the export guard produces
# through it. The binary is downloaded from the protomaps/go-pmtiles GitHub
# release at a pinned version and checked against the sha256 recorded here
# before anything is extracted: go-pmtiles publishes no checksum file, so the
# hashes below were computed from the release assets on 2026-09-27 and a
# changed asset fails loudly instead of being run.
#
# Usage:
#   scripts/setup_go_pmtiles.sh          # installs .tools/go-pmtiles-<ver>/pmtiles
#   eval "$(scripts/setup_go_pmtiles.sh --print-path)"   # exports PMTILES_BIN
#
# Re-pinning: bump GO_PMTILES_VERSION and replace every hash (compute them
# with `sha256sum` over the freshly downloaded assets); never accept a hash
# mismatch by editing the expected value to whatever was downloaded.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/.." && pwd)"
TOOLS="${GO_PMTILES_TOOLS_DIR:-$ROOT/.tools}"

# --- the pin -----------------------------------------------------------------
# protomaps/go-pmtiles v1.31.2 (commit a3e4951, built 2026-07-22).
GO_PMTILES_VERSION="1.31.2"
# Per-asset sha256 of the release tarball/zip, by `uname -s`_`uname -m`.
sha_for_asset() {
  case "$1" in
    go-pmtiles_${GO_PMTILES_VERSION}_Linux_x86_64.tar.gz)
      echo "3ed7dbf4ec2e6dfe5e25b6f70d1ffc932729f93c86db353bf514dd71010a312f" ;;
    go-pmtiles_${GO_PMTILES_VERSION}_Linux_arm64.tar.gz)
      echo "f8bd47e7ea866863489cad588fbaf2f31f42e5821f7a03f009b3769f05801cb1" ;;
    go-pmtiles-${GO_PMTILES_VERSION}_Darwin_arm64.zip)
      echo "40528f7f616fcbf91207cd48c8fc023d213f6d86c0cbf1f748732803d1880f3d" ;;
    go-pmtiles-${GO_PMTILES_VERSION}_Darwin_x86_64.zip)
      echo "1f0dc02eee6c58312dd6c509faee1b5c32f0596568af1bf51f1b034e7a88a65b" ;;
    *) echo "" ;;
  esac
}

PREFIX="$TOOLS/go-pmtiles-$GO_PMTILES_VERSION"
BIN="$PREFIX/pmtiles"

if [ "${1:-}" = "--print-path" ]; then
  echo "export PMTILES_BIN=\"$BIN\""
  exit 0
fi

os="$(uname -s)"
arch="$(uname -m)"
case "$arch" in
  aarch64) arch="arm64" ;;
  amd64) arch="x86_64" ;;
esac
case "$os" in
  Linux) asset="go-pmtiles_${GO_PMTILES_VERSION}_${os}_${arch}.tar.gz" ;;
  Darwin) asset="go-pmtiles-${GO_PMTILES_VERSION}_${os}_${arch}.zip" ;;
  *) echo "ERROR: unsupported OS $os" >&2; exit 1 ;;
esac
expected="$(sha_for_asset "$asset")"
if [ -z "$expected" ]; then
  echo "ERROR: no pinned sha256 for $asset ($os/$arch)" >&2
  exit 1
fi

verify_version() {
  local got
  got="$("$BIN" version 2>&1 | head -1)"
  case "$got" in
    "pmtiles $GO_PMTILES_VERSION,"*) ;;
    *)
      echo "ERROR: $BIN reports '$got', expected pmtiles $GO_PMTILES_VERSION" >&2
      echo "       (stale .tools/ from another pin — remove $PREFIX and re-run)" >&2
      exit 1 ;;
  esac
}

if [ -x "$BIN" ]; then
  verify_version
  echo "already installed: $BIN ($("$BIN" version 2>&1 | head -1))"
else
  mkdir -p "$PREFIX"
  url="https://github.com/protomaps/go-pmtiles/releases/download/v${GO_PMTILES_VERSION}/${asset}"
  archive="$PREFIX/$asset"
  echo "downloading $url ..."
  curl -sSfL --retry 3 -o "$archive" "$url"
  got="$(sha256sum "$archive" | cut -d' ' -f1)"
  if [ "$got" != "$expected" ]; then
    echo "ERROR: sha256 mismatch for $asset" >&2
    echo "       expected $expected" >&2
    echo "       got      $got" >&2
    rm -f "$archive"
    exit 1
  fi
  case "$asset" in
    *.tar.gz) tar -xzf "$archive" -C "$PREFIX" pmtiles ;;
    *.zip) unzip -q -o "$archive" pmtiles -d "$PREFIX" ;;
  esac
  chmod +x "$BIN"
  rm -f "$archive"
  verify_version
  echo "installed: $BIN"
fi

echo "version: $("$BIN" version 2>&1 | head -1)"
echo "sha256($asset): $expected"
