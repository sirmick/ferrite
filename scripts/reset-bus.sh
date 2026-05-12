#!/usr/bin/env bash
# Hard-reset the USB bus the SDRplay sits on by unbinding/rebinding the
# xHCI host controller from PCI. This drops VBUS on the root hub for a
# moment — functionally equivalent to physically unplugging every device
# on that controller's buses.
#
# DANGER: every USB device on the same controller drops out for ~1 s.
# Verify the bus list (printed below) only contains things you can
# safely lose.
#
# Usage: ./scripts/reset-bus.sh
#   needs sudo for sysfs writes + systemctl restart.

set -uo pipefail
cd "$(dirname "$0")/.."

# Find PCI ID of the controller serving the SDRplay's bus.
node=
for d in /sys/bus/usb/devices/*; do
  [ -f "$d/idVendor" ] || continue
  if [ "$(cat "$d/idVendor")" = "1df7" ]; then
    node=$d
    break
  fi
done
if [ -z "$node" ]; then
  echo "[reset-bus] no SDRplay device on USB (vendor 1df7)" >&2
  exit 1
fi

# Walk up to find the PCI parent (xhci_hcd).
pci_path=$(readlink -f "$node")
while [ "$pci_path" != "/" ] && [ ! -e "$pci_path/driver/module/drivers/pci:xhci_hcd" ]; do
  parent=$(dirname "$pci_path")
  if [ -L "$parent/driver" ] && readlink "$parent/driver" | grep -q xhci_hcd; then
    pci_path=$parent
    break
  fi
  pci_path=$parent
done
pci_id=$(basename "$pci_path")

if ! [[ "$pci_id" =~ ^[0-9a-f]{4}:[0-9a-f]{2}:[0-9a-f]{2}\.[0-9a-f]$ ]]; then
  echo "[reset-bus] couldn't locate PCI parent for $node" >&2
  exit 1
fi

echo "[reset-bus] SDRplay sits behind PCI $pci_id (xhci_hcd)"
echo "[reset-bus] devices that will momentarily drop:"
for bus in /sys/bus/pci/drivers/xhci_hcd/"$pci_id"/usb*; do
  [ -d "$bus" ] || continue
  busnum=$(cat "$bus/busnum")
  echo "  bus $busnum:"
  lsusb -s "$busnum:" | sed 's/^/    /'
done

echo "[reset-bus] stopping sdrplay_apiService"
sudo systemctl stop sdrplay 2>/dev/null || true

echo "[reset-bus] unbinding $pci_id from xhci_hcd"
sudo sh -c "echo $pci_id > /sys/bus/pci/drivers/xhci_hcd/unbind"
sleep 2
echo "[reset-bus] rebinding $pci_id to xhci_hcd"
sudo sh -c "echo $pci_id > /sys/bus/pci/drivers/xhci_hcd/bind"
sleep 3

echo "[reset-bus] starting sdrplay_apiService"
sudo systemctl start sdrplay
sleep 1

if [ -f soapysdr/env.sh ]; then
  # shellcheck disable=SC1091
  . soapysdr/env.sh
fi

echo "[reset-bus] lsusb after reset:"
lsusb | grep -i "1df7\|sdrplay" || echo "  (SDRplay not enumerated — likely needs physical replug)"
echo
echo "[reset-bus] SoapySDRUtil --find:"
SoapySDRUtil --find 2>&1 | sed -n '/####/,$p'
