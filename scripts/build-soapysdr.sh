#!/usr/bin/env bash
# Clone SoapySDR + driver modules (SDRplay, HackRF, RTL-SDR) into
# ./soapysdr-src/ and install to ./soapysdr/ (prefix, no sudo). Source
# env.sh after to use.
#
# HackRF and RTL-SDR require their low-level userland libs installed
# system-wide (libhackrf-dev, librtlsdr-dev on Debian/Ubuntu). The
# cmake steps below will fail loudly if they're missing.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SRC_DIR="$ROOT/soapysdr-src"
PREFIX="$ROOT/soapysdr"
JOBS="$(nproc 2>/dev/null || echo 4)"

mkdir -p "$SRC_DIR" "$PREFIX"

clone_or_update() {
  local url="$1" dir="$2"
  if [ -d "$dir/.git" ]; then
    echo ">>> Updating $dir"
    git -C "$dir" fetch --depth=1 origin
    git -C "$dir" reset --hard origin/HEAD
  else
    echo ">>> Cloning $url"
    git clone --depth=1 "$url" "$dir"
  fi
}

build_cmake() {
  local src="$1"
  local build="$src/build"
  rm -rf "$build"
  cmake -S "$src" -B "$build" \
    -DCMAKE_BUILD_TYPE=Release \
    -DCMAKE_INSTALL_PREFIX="$PREFIX" \
    -DCMAKE_PREFIX_PATH="$PREFIX"
  cmake --build "$build" -j"$JOBS"
  cmake --install "$build"
}

clone_or_update https://github.com/pothosware/SoapySDR.git        "$SRC_DIR/SoapySDR"
clone_or_update https://github.com/pothosware/SoapySDRPlay3.git   "$SRC_DIR/SoapySDRPlay3"
clone_or_update https://github.com/pothosware/SoapyHackRF.git     "$SRC_DIR/SoapyHackRF"
clone_or_update https://github.com/pothosware/SoapyRTLSDR.git     "$SRC_DIR/SoapyRTLSDR"

echo ">>> Building SoapySDR"
build_cmake "$SRC_DIR/SoapySDR"

# Each driver requires its userland lib system-wide. If a lib is missing we
# skip that driver with a warning rather than aborting the whole build, so
# you can install just the radios you have. SKIPPED is shown at the end.
SKIPPED=()

# SDRplay API (closed-source, .run installer from sdrplay.com) drops libsdrplay_api
# in /usr/local/lib by default.
if [ -f /usr/local/lib/libsdrplay_api.so ] || ldconfig -p | grep -q libsdrplay_api; then
  echo ">>> Building SoapySDRPlay3"
  build_cmake "$SRC_DIR/SoapySDRPlay3"
else
  echo ">>> Skipping SoapySDRPlay3 — SDRplay API not installed (libsdrplay_api missing)"
  SKIPPED+=("SoapySDRPlay3 (install SDRplay .run installer to enable)")
fi

if pkg-config --exists libhackrf 2>/dev/null; then
  echo ">>> Building SoapyHackRF"
  build_cmake "$SRC_DIR/SoapyHackRF"
else
  echo ">>> Skipping SoapyHackRF — libhackrf-dev not installed"
  SKIPPED+=("SoapyHackRF (apt install libhackrf-dev to enable)")
fi

if pkg-config --exists librtlsdr 2>/dev/null; then
  echo ">>> Building SoapyRTLSDR"
  build_cmake "$SRC_DIR/SoapyRTLSDR"
else
  echo ">>> Skipping SoapyRTLSDR — librtlsdr-dev not installed"
  SKIPPED+=("SoapyRTLSDR (apt install librtlsdr-dev to enable)")
fi

cat > "$PREFIX/env.sh" <<EOF
# source this from repo root to use the local SoapySDR build:
#   source soapysdr/env.sh
export SOAPY_SDR_ROOT="$PREFIX"
export PATH="$PREFIX/bin:\$PATH"
export LD_LIBRARY_PATH="$PREFIX/lib:\${LD_LIBRARY_PATH:-}"
export PKG_CONFIG_PATH="$PREFIX/lib/pkgconfig:\${PKG_CONFIG_PATH:-}"
export SOAPY_SDR_PLUGIN_PATH="$PREFIX/lib/SoapySDR/modules0.8-3"
EOF

echo
echo ">>> Done. Sanity check:"
echo "    source $PREFIX/env.sh && SoapySDRUtil --info && SoapySDRUtil --find"
if [ ${#SKIPPED[@]} -gt 0 ]; then
  echo
  echo ">>> Skipped drivers (re-run this script after installing the missing dep):"
  for s in "${SKIPPED[@]}"; do echo "      - $s"; done
fi
