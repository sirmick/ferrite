#!/usr/bin/env bash
# Unattended install of the SDRplay API (libsdrplay_api + sdrplay_apiService
# + udev rules + systemd unit) from the vendor's makeself .run.
#
# The vendor's install_lib.sh is interactive (license pager + y/n loops
# that spin forever on piped/EOF stdin — it once filled a radio host's
# eMMC with 14 GB of trace log). This performs the same file drops and
# unit as install_lib.sh 3.15, minus the prompts. Keep in sync if the
# .run is bumped.
#
#   scripts/sdrplay-api-install.sh [path/to/SDRplay_RSP_API-Linux-3.15.2.run]
#
# Needs passwordless sudo. Idempotent — re-running re-copies and
# restarts the service.

set -euo pipefail

RUN="${1:-$(dirname "$0")/../SDRplay_RSP_API-Linux-3.15.2.run}"
[ -f "$RUN" ] || { echo "installer not found: $RUN" >&2; exit 1; }
sudo -n true 2>/dev/null || { echo "needs passwordless sudo" >&2; exit 1; }

ARCH="$(dpkg --print-architecture 2>/dev/null || uname -m)"
case "$ARCH" in
  amd64|x86_64) ARCH=amd64 ;;
  arm64|aarch64) ARCH=arm64 ;;
  armhf) ;;
  *) echo "unsupported arch $ARCH" >&2; exit 1 ;;
esac

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT
"$RUN" --noexec --quiet --target "$TMP" >/dev/null
VERS="$(sed -nE 's/^VERS="([0-9.]+)".*/\1/p' "$TMP/install_lib.sh" | head -1)"
MAJVERS="${VERS%%.*}"
[ -n "$VERS" ] || { echo "could not read VERS from install_lib.sh" >&2; exit 1; }
echo ">>> SDRplay API $VERS ($ARCH)"

SVC=/opt/sdrplay_api
LIB=/usr/local/lib
INC=/usr/local/include

sudo -n mkdir -p -m 755 "$SVC" "$LIB" "$INC"
sudo -n rm -f "$LIB"/libsdrplay_api.so*
sudo -n install -m 644 "$TMP/$ARCH/libsdrplay_api.so.$VERS" "$LIB/"
sudo -n ln -s "$LIB/libsdrplay_api.so.$VERS" "$LIB/libsdrplay_api.so.$MAJVERS"
sudo -n ln -s "$LIB/libsdrplay_api.so.$MAJVERS" "$LIB/libsdrplay_api.so"
sudo -n install -m 644 "$TMP"/inc/sdrplay_api*.h "$INC/"
sudo -n install -m 755 "$TMP/$ARCH/sdrplay_apiService" "$SVC/sdrplay_apiService"
cp -f "$TMP/sdrplay_license.txt" "$HOME/sdrplay_license.txt"

# udev: world-rw for every RSP model (same list as the vendor script).
sudo -n tee /etc/udev/rules.d/66-sdrplay.rules >/dev/null <<'EOF'
SUBSYSTEM=="usb",ENV{DEVTYPE}=="usb_device",ATTRS{idVendor}=="1df7",ATTRS{idProduct}=="2500",MODE:="0666"
SUBSYSTEM=="usb",ENV{DEVTYPE}=="usb_device",ATTRS{idVendor}=="1df7",ATTRS{idProduct}=="3000",MODE:="0666"
SUBSYSTEM=="usb",ENV{DEVTYPE}=="usb_device",ATTRS{idVendor}=="1df7",ATTRS{idProduct}=="3010",MODE:="0666"
SUBSYSTEM=="usb",ENV{DEVTYPE}=="usb_device",ATTRS{idVendor}=="1df7",ATTRS{idProduct}=="3020",MODE:="0666"
SUBSYSTEM=="usb",ENV{DEVTYPE}=="usb_device",ATTRS{idVendor}=="1df7",ATTRS{idProduct}=="3030",MODE:="0666"
SUBSYSTEM=="usb",ENV{DEVTYPE}=="usb_device",ATTRS{idVendor}=="1df7",ATTRS{idProduct}=="3050",MODE:="0666"
SUBSYSTEM=="usb",ENV{DEVTYPE}=="usb_device",ATTRS{idVendor}=="1df7",ATTRS{idProduct}=="3060",MODE:="0666"
EOF
sudo -n rm -f /etc/udev/rules.d/66-mirics.rules
if [ -d /etc/udev/hwdb.d ]; then
  sudo -n tee /etc/udev/hwdb.d/20-sdrplay.hwdb >/dev/null <<'EOF'
usb:v1DF7*
 ID_VENDOR_FROM_DATABASE=SDRplay

usb:v1DF7p2500*
 ID_MODEL_FROM_DATABASE=RSP1

usb:v1DF7p3000*
 ID_MODEL_FROM_DATABASE=RSP1A

usb:v1DF7p3010*
 ID_MODEL_FROM_DATABASE=RSP2/RSP2pro

usb:v1DF7p3020*
 ID_MODEL_FROM_DATABASE=RSPduo

usb:v1DF7p3030*
 ID_MODEL_FROM_DATABASE=RSPdx

usb:v1DF7p3050*
 ID_MODEL_FROM_DATABASE=RSP1B

usb:v1DF7p3060*
 ID_MODEL_FROM_DATABASE=RSPdxR2
EOF
  sudo -n systemd-hwdb update || true
fi
sudo -n udevadm control --reload-rules || true
sudo -n udevadm trigger || true

# systemd unit — verbatim from the vendor script.
if systemctl --version >/dev/null 2>&1; then
  sudo -n tee /etc/systemd/system/sdrplay.service >/dev/null <<EOF
[Unit]
Description=SDRplay API Service
After=network.target
StartLimitIntervalSec=0

[Service]
Type=simple
Restart=on-failure
RestartSec=1
User=root
ExecStart=$SVC/sdrplay_apiService

[Install]
WantedBy=multi-user.target
EOF
  sudo -n systemctl daemon-reload
  sudo -n systemctl enable sdrplay >/dev/null
  sudo -n systemctl restart sdrplay
fi
sudo -n ldconfig

echo ">>> installed:"
ls -la "$LIB"/libsdrplay_api.so* "$SVC/sdrplay_apiService"
systemctl is-enabled sdrplay 2>/dev/null || true
systemctl is-active sdrplay 2>/dev/null || true
