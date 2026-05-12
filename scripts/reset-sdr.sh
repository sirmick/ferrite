#!/usr/bin/env bash
# Soft USB-reset the SDRplay (RSPdx/RSP1/RSPduo) without unplugging the cable.
# Writes 0 then 1 to the device's /sys/bus/usb/devices/<n-m>/authorized,
# which forces the kernel to detach and re-enumerate the device. Then
# bounces sdrplay_apiService and runs SoapySDRUtil --find to verify.
#
# Usage: ./scripts/reset-sdr.sh
#   needs sudo for the sysfs writes + systemctl restart.

set -uo pipefail
cd "$(dirname "$0")/.."

node=
for d in /sys/bus/usb/devices/*; do
  [ -f "$d/idVendor" ] || continue
  if [ "$(cat "$d/idVendor")" = "1df7" ]; then
    node=$d
    break
  fi
done

if [ -z "$node" ]; then
  echo "[reset-sdr] no SDRplay device on USB (vendor 1df7)" >&2
  exit 1
fi

echo "[reset-sdr] device: $(basename "$node") ($(cat "$node/idVendor"):$(cat "$node/idProduct"))"
echo "[reset-sdr] re-enumerating via authorized toggle"
sudo sh -c "echo 0 > '$node/authorized'"
sleep 1
sudo sh -c "echo 1 > '$node/authorized'"
sleep 2

if systemctl list-unit-files sdrplay.service >/dev/null 2>&1; then
  echo "[reset-sdr] restarting sdrplay_apiService"
  sudo systemctl restart sdrplay
  sleep 1
fi

if [ -f soapysdr/env.sh ]; then
  # shellcheck disable=SC1091
  . soapysdr/env.sh
fi

echo "[reset-sdr] SoapySDRUtil --find:"
SoapySDRUtil --find 2>&1 | sed -n '/####/,$p'
