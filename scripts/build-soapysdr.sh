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
  # Force lib (not lib64): GNUInstallDirs picks lib64 on Fedora/RHEL by
  # default, lib on Debian/Ubuntu. Pinning lib keeps PKG_CONFIG_PATH +
  # SOAPY_SDR_PLUGIN_PATH (and the package install rules) identical
  # across distros.
  cmake -S "$src" -B "$build" \
    -DCMAKE_BUILD_TYPE=Release \
    -DCMAKE_INSTALL_PREFIX="$PREFIX" \
    -DCMAKE_INSTALL_LIBDIR=lib \
    -DCMAKE_PREFIX_PATH="$PREFIX" \
    -DCMAKE_SKIP_INSTALL_RPATH=ON
  # SKIP_INSTALL_RPATH strips RPATH on installed binaries / libs.
  # Without it CMake bakes the build-host's $PREFIX into RPATH and
  # Fedora's check-rpaths step (rpmbuild %install) fails with
  # "invalid runpath". Setting `INSTALL_RPATH=$ORIGIN/../lib` on the
  # global doesn't help because SoapySDR's CMakeLists sets per-target
  # RPATH that overrides ours. We rely on the package wrapper
  # (packaging/ferrited) to set LD_LIBRARY_PATH at runtime.
  cmake --build "$build" -j"$JOBS"
  cmake --install "$build"
}

clone_or_update https://github.com/pothosware/SoapySDR.git        "$SRC_DIR/SoapySDR"
clone_or_update https://github.com/pothosware/SoapySDRPlay3.git   "$SRC_DIR/SoapySDRPlay3"
clone_or_update https://github.com/pothosware/SoapyHackRF.git     "$SRC_DIR/SoapyHackRF"
clone_or_update https://github.com/pothosware/SoapyRTLSDR.git     "$SRC_DIR/SoapyRTLSDR"
clone_or_update https://github.com/pothosware/SoapyAirspy.git     "$SRC_DIR/SoapyAirspy"
clone_or_update https://github.com/pothosware/SoapyAirspyHF.git   "$SRC_DIR/SoapyAirspyHF"
clone_or_update https://github.com/pothosware/SoapyBladeRF.git    "$SRC_DIR/SoapyBladeRF"
clone_or_update https://github.com/pothosware/SoapyPlutoSDR.git   "$SRC_DIR/SoapyPlutoSDR"
clone_or_update https://github.com/pothosware/SoapyUHD.git        "$SRC_DIR/SoapyUHD"
# LimeSDR (SoapyLMS7) lives inside the LimeSuite repo, not as a
# standalone Soapy module. Building just that subcomponent would mean
# a partial-checkout dance against the full LimeSuite tree — skip for
# now; users can install the distro `soapysdr-module-lms7` package and
# point ferrite at the system Soapy plugin path if they need LimeSDR.

echo ">>> Building SoapySDR"
build_cmake "$SRC_DIR/SoapySDR"

# Each driver requires its userland lib system-wide. If a lib is missing we
# skip that driver with a warning rather than aborting the whole build, so
# you can install just the radios you have. SKIPPED is shown at the end.
SKIPPED=()

build_if() {
  local name="$1" check="$2" hint="$3" src="$4"
  if eval "$check" >/dev/null 2>&1; then
    echo ">>> Building $name"
    build_cmake "$src"
  else
    echo ">>> Skipping $name — $hint"
    SKIPPED+=("$name ($hint)")
  fi
}

# SDRplay API (closed-source, .run installer from sdrplay.com) drops
# libsdrplay_api in /usr/local/lib by default; not in pkg-config.
build_if SoapySDRPlay3 \
  '[ -f /usr/local/lib/libsdrplay_api.so ] || ldconfig -p | grep -q libsdrplay_api' \
  'SDRplay API not installed — grab .run installer from sdrplay.com' \
  "$SRC_DIR/SoapySDRPlay3"

build_if SoapyHackRF    'pkg-config --exists libhackrf'    'install libhackrf-dev (apt) / hackrf-devel (dnf)'   "$SRC_DIR/SoapyHackRF"
build_if SoapyRTLSDR    'pkg-config --exists librtlsdr'    'install librtlsdr-dev (apt) / rtl-sdr-devel (dnf)'  "$SRC_DIR/SoapyRTLSDR"
build_if SoapyAirspy    'pkg-config --exists libairspy'    'install libairspy-dev (apt) / airspyone_host-devel (dnf)'  "$SRC_DIR/SoapyAirspy"
build_if SoapyAirspyHF  'pkg-config --exists libairspyhf'  'install libairspyhf-dev (apt) / airspyhf-devel (dnf)'      "$SRC_DIR/SoapyAirspyHF"
build_if SoapyBladeRF   'pkg-config --exists libbladeRF'   'install libbladerf-dev (apt) / bladeRF-devel (dnf)'        "$SRC_DIR/SoapyBladeRF"

# PlutoSDR via libiio (Analog Devices' I/O abstraction). Pluto firmware
# ≥0.30 ships an internal IIO server reachable over USB/Ethernet.
build_if SoapyPlutoSDR  'pkg-config --exists libiio'       'install libiio-dev libad9361-dev (apt) / libiio-devel libad9361-devel (dnf)' "$SRC_DIR/SoapyPlutoSDR"

# UHD (USRP) is large; we don't pull it ourselves — users with USRPs
# install UHD via apt/dnf, then re-run this script.
build_if SoapyUHD       'pkg-config --exists uhd'          'install libuhd-dev (apt) / uhd-devel (dnf)' "$SRC_DIR/SoapyUHD"

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
