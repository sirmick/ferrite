#!/usr/bin/env bash
# Provision a small Linux box as a SoapyRemote radio host: the SDR's
# USB + vendor driver stay next to the antenna, and ferrited on another
# machine opens it as `driver=remote,remote=<this-host>`.
#
# Source-builds SoapySDR + SoapySDRPlay3 + SoapyRemote into /usr/local
# (same upstream repos as scripts/build-soapysdr.sh, so both sides can
# be patched in lockstep) and installs a systemd unit that starts
# SoapySDRServer after the SDRplay API service on boot.
#
# Run ON the radio host (Debian/Ubuntu, passwordless sudo):
#   scripts/soapy-remote-host.sh            # build + install + enable
#   scripts/soapy-remote-host.sh --update   # refetch upstream, rebuild
#
# Prereq: the SDRplay API (.run from sdrplay.com) is installed —
# /usr/local/lib/libsdrplay_api.so present, `sdrplay` unit enabled.
# Other radios (RTL-SDR, HackRF…) just need their userland lib +
# the matching Soapy module added below.

set -euo pipefail

SRC_DIR="${SOAPY_SRC_DIR:-$HOME/soapysdr-src}"
PREFIX=/usr/local
JOBS="$(nproc 2>/dev/null || echo 2)"
BIND="${SOAPY_REMOTE_BIND:-0.0.0.0:55132}"   # SoapyRemote default port

sudo -n true 2>/dev/null || { echo "needs passwordless sudo" >&2; exit 1; }

if ! ldconfig -p | grep -q libsdrplay_api && [ ! -f /usr/local/lib/libsdrplay_api.so ]; then
  echo "SDRplay API not installed — run the sdrplay.com .run installer first" >&2
  exit 1
fi

export DEBIAN_FRONTEND=noninteractive
sudo -n apt-get install -y -qq build-essential cmake git pkg-config \
  libusb-1.0-0-dev libavahi-client-dev >/dev/null

UPDATE=0; [ "${1:-}" = "--update" ] && UPDATE=1

mkdir -p "$SRC_DIR"
clone_or_update() {
  local url="$1" dir="$2"
  if [ ! -d "$dir/.git" ]; then
    echo ">>> Cloning $url"
    git clone --depth=1 "$url" "$dir"
  elif [ "$UPDATE" = 1 ]; then
    echo ">>> Updating $dir"
    git -C "$dir" fetch --depth=1 origin
    git -C "$dir" reset --hard origin/HEAD
  fi
}

build_cmake() {
  local src="$1" build="$1/build"
  shift
  rm -rf "$build"
  cmake -S "$src" -B "$build" -DCMAKE_BUILD_TYPE=Release \
    -DCMAKE_INSTALL_PREFIX="$PREFIX" -DCMAKE_INSTALL_LIBDIR=lib "$@"
  cmake --build "$build" -j"$JOBS"
  sudo -n cmake --install "$build"
}

clone_or_update https://github.com/pothosware/SoapySDR.git      "$SRC_DIR/SoapySDR"
clone_or_update https://github.com/pothosware/SoapySDRPlay3.git "$SRC_DIR/SoapySDRPlay3"
clone_or_update https://github.com/pothosware/SoapyRemote.git   "$SRC_DIR/SoapyRemote"

echo ">>> Building SoapySDR"
build_cmake "$SRC_DIR/SoapySDR" -DENABLE_PYTHON=OFF -DENABLE_PYTHON3=OFF
echo ">>> Building SoapySDRPlay3"
build_cmake "$SRC_DIR/SoapySDRPlay3"
echo ">>> Building SoapyRemote"
build_cmake "$SRC_DIR/SoapyRemote"
sudo -n ldconfig

echo ">>> Installing soapy-remote.service (bind $BIND)"
sudo -n tee /etc/systemd/system/soapy-remote.service >/dev/null <<EOF
[Unit]
Description=SoapyRemote SDR server (SoapySDRServer)
# The SDRplay vendor service must be up before the Soapy module can
# enumerate the RSP; Requires= also restarts us if it is recycled.
After=network-online.target sdrplay.service
Wants=network-online.target
Requires=sdrplay.service

[Service]
ExecStart=$PREFIX/bin/SoapySDRServer --bind=$BIND
Restart=always
RestartSec=3
# Give the SDRplay service a moment after boot/restart before enumerating.
ExecStartPre=/bin/sleep 2
Nice=-5

[Install]
WantedBy=multi-user.target
EOF
sudo -n systemctl daemon-reload
sudo -n systemctl enable --now soapy-remote.service
sleep 3
sudo -n systemctl --no-pager --lines=5 status soapy-remote.service || true

echo
echo ">>> Local sanity check:"
SoapySDRUtil --find 2>&1 | grep -E 'driver|label|serial|No devices' || true
echo
echo ">>> From the ferrite box:"
echo "    source soapysdr/env.sh && SoapySDRUtil --find=\"driver=remote,remote=$(hostname)\""
