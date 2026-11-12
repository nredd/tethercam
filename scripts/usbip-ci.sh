#!/bin/sh
# Hosted Linux USB/IP prerequisites and targeted permissions for our synthetic device only.
# Never invoke cargo as root, touch a physical USB device, or change global USB permissions.
set -eu
serial=TETHERCAM-USBIP-0001
usbip_tool() {
    for tool in /usr/lib/linux-tools/*/usbip; do
        if test -x "$tool"; then printf '%s\n' "$tool"; return; fi
    done
    printf 'No packaged USB/IP binary found\n' >&2
    return 1
}
case "${1:-}" in
    prepare)
        test "$(uname -s)" = Linux || { printf 'USB/IP requires hosted Linux\n' >&2; exit 1; }
        sudo apt-get update -qq
        sudo apt-get install -y linux-tools-generic
        sudo modprobe vhci_hcd
        test -r /sys/devices/platform/vhci_hcd.0/status
        "$(usbip_tool)" version
        ;;
    attach)
        timeout 15 sudo "$(usbip_tool)" attach -r 127.0.0.1 -b 1-1
        count=0
        while test "$count" -lt 100; do
            for device in /sys/bus/usb/devices/*; do
                test -r "$device/serial" || continue
                case "$(readlink -f "$device")" in
                    /sys/devices/platform/vhci_hcd.*/usb*/*) ;;
                    *) continue ;;
                esac
                test "$(tr -d '\n' < "$device/serial")" = "$serial" || continue
                test "$(tr -d '\n' < "$device/idVendor")" = 054c
                bus=$(tr -d '\n' < "$device/busnum")
                dev=$(tr -d '\n' < "$device/devnum")
                node=$(printf '/dev/bus/usb/%03d/%03d' "$bus" "$dev")
                test -c "$node" || continue
                sudo chown "$(id -u):$(id -g)" "$node"
                printf 'USB/IP attached synthetic device %s\n' "$node"
                exit 0
            done
            count=$((count + 1))
            sleep 0.1
        done
        printf 'Synthetic USB/IP device did not enumerate\n' >&2
        sudo dmesg | tail -60 >&2
        exit 1
        ;;
    detach)
        # Match the imported local bus id back to our exact synthetic serial before detach.
        for status in /sys/devices/platform/vhci_hcd.*/status*; do
            test -r "$status" || continue
            while read -r hub port sta speed dev socket busid rest; do
                test -n "${busid:-}" || continue
                device="/sys/bus/usb/devices/$busid"
                test -r "$device/serial" || continue
                test "$(tr -d '\n' < "$device/serial")" = "$serial" || continue
                timeout 15 sudo "$(usbip_tool)" detach -p "$port"
            done < "$status"
        done
        ;;
    *) printf 'usage: usbip-ci.sh prepare|attach|detach\n' >&2; exit 2 ;;
esac
