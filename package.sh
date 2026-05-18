#!/usr/bin/env bash
# Build distributable OS packages (.deb / .rpm) across the (distro, arch)
# matrix. Thin wrapper around packaging/run_matrix.sh — that script
# builds the web bundle once on the host, stages a source tarball, and
# runs the per-target docker builds.
#
# Prerequisites:
#   - docker + buildx
#   - for arm64 / riscv64 rows: qemu-user-static + binfmt_misc, one-time:
#       sudo apt install qemu-user-static binfmt-support docker-buildx
#       docker run --privileged --rm tonistiigi/binfmt --install all
#
# Build a subset (fast local sanity check) by exporting a newline-
# separated FERRITE_PKG_TARGETS, e.g. amd64-only:
#   FERRITE_PKG_TARGETS='ubuntu-24.04-amd64 linux/amd64 ubuntu:24.04 deb' ./package.sh
#
# Output: dist/packages/<tag>/*.{deb,rpm}, logs in dist/<tag>.log

set -euo pipefail
cd "$(dirname "$0")"

if ! command -v docker >/dev/null 2>&1; then
  echo "docker not found — package.sh builds inside containers." >&2
  exit 1
fi

exec packaging/run_matrix.sh "$@"
