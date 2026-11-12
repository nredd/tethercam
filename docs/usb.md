# USB setup

`tethercam` talks to the camera with [`nusb`](https://docs.rs/nusb), which is pure Rust: no
libusb, no driver to install.

## Camera

Set the camera's USB Connection to PC Remote. The transport reads the mode from the USB
product id and reports it when it is wrong:

- `0x0c34`: PC Remote. The only mode that works
- `0x0c03`: MTP. Opening fails with "the camera is in MTP mode; set USB Connection to PC Remote"
- `0x0c02`: Mass Storage. Same error

Only one program can hold the camera. Stop anything else that opens it (e.g. a photo import
tool) first, or opening fails with "the camera is in use by another program".

## Linux

Opening a USB device needs write access to its `/dev/bus/usb` node. Install the udev rule
[`70-tethercam.rules`](70-tethercam.rules), which grants it to the logged-in user and to the
`plugdev` group for Sony devices (vendor `054c`):

```
sudo cp docs/70-tethercam.rules /etc/udev/rules.d/
sudo udevadm control --reload
sudo udevadm trigger
```

Replug the camera afterwards. Without the rule opening fails with "permission denied opening
the camera; see docs/usb.md". If a kernel driver holds the interface it is detached when the
interface is claimed.

## macOS

No driver and no entitlement. If a system daemon holds the camera, opening reports it as in
use; unplug and replug the camera.

`UsbTransport::reset` rejects a timeout whose deadline exceeds the clock range with
`TransportError::Other` before issuing any USB operation. The transport remains usable.

## Hardware tests

With a camera in PC Remote mode attached:

```
cargo test --test usb_hardware -- --ignored
```

They skip cleanly when no camera is attached. The session test opens, runs `GetDeviceInfo`
and closes 20 times. The transaction result is checked only after `CloseSession` finishes.
An abandoned PTP session wedges the camera until it is power-cycled.

## Emulated USB/IP tests

`tests/usb_emulated.rs` exports a synthetic Sony device on loopback using the kernel's
[USB/IP protocol](https://docs.kernel.org/usb/usbip_protocol.html). The device is backed by
`SimCamera` and the Sony responder; it never opens a physical USB device. The hosted Linux
bootstrap runs `scripts/usbip-ci.sh prepare`, then enables the test with `TETHERCAM_USBIP=1`.
The test attaches through `vhci_hcd`, grants access only to the synthetic device node, and
drives `nusb` and `UsbTransport` through the actual Linux kernel. Cargo never runs as root.
The server is test-only, binds loopback only, bounds URB sizes and pending requests, and
closes when its owning test ends. Only one import may own the device at a time. Pure protocol
tests also cover fragmented requests, duplicate imports, malformed/oversized URBs, pending
interrupt delivery and unlink, URB count/memory ceilings, and pending deadlines. The native
suite has a whole-suite deadline and explicitly tests a pending interrupt receiving an event,
class status changes, class cancellation, endpoint stalls, clear-halt, port reset fallback,
wrong-mode enumeration, mid-transfer disconnect and hotplug removal/reattachment. Stateful
cancellation decodes the transaction id, discards incomplete DATA OUT and queued DATA IN,
flushes pending bulk URBs, preserves the session, and reports busy until the next status poll
completes cancellation, as SICDD 1.0 sections 5.2.1 and 7.2.1.1 specify. Native tests continue
with a fresh transaction and close the same session; unknown transaction ids do not cancel it.

The export also has a permanent 128 MiB admission-credit budget for simulator-retained work,
not just pending URBs. Each bulk OUT reserves four input copies, two maximum-size generated
replies and 512 bytes of bookkeeping before touching `SimCamera`. Reads, unlink, class reset
and re-import do not refund credits. This conservatively bounds queued replies, command/data
histories and zero-length-transfer history even when a peer never reads bulk IN. Control
history is capped before insertion, including after re-import, with a capacity-boundary
regression. The Sony responder's exact host-value decoder preserves typed malformed-value
and trailing-byte diagnostics, with arbitrary-byte tests for every supported datatype and
deterministic truncation/trailing-byte rejection tests. The native mid-transfer disconnect
scenario waits for a confirmed pending bulk-IN URB before detaching the serial-verified VHCI
port, then checks both transfer failure and the removal event. `usb::factory` separately covers enumeration failure, exact-address
selection and open errors with one concrete test source. These repository-only tests and their
support modules are excluded from the published crate because they require the excluded CI
setup script. Physical-camera tests are unrelated and remain ignored.
