#!/usr/bin/env bash
# Clone SoapySDR + SoapySDRPlay3 sources into ./soapysdr-src/ and install
# to ./soapysdr/ (prefix, no sudo). Source env.sh after to use.

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

echo ">>> Building SoapySDR"
build_cmake "$SRC_DIR/SoapySDR"

echo ">>> Building SoapySDRPlay3 (requires SDRplay API installed system-wide)"
build_cmake "$SRC_DIR/SoapySDRPlay3"

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
echo "    source $PREFIX/env.sh && SoapySDRUtil --info && SoapySDRUtil --probe=driver=sdrplay"
