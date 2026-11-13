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

`tests/usb_emulated.rs` uses the public, off-by-default `test-support` library to export
synthetic Sony/SICDD and independent vendor-class echo devices through the kernel's
[USB/IP protocol](https://docs.kernel.org/usb/usbip_protocol.html). The generic engine lives in
`src/test_support/`; scenarios remain in `tests/usbip/`. Both personalities are imported through
hosted Linux VHCI and exercised with actual `nusb` handles; no physical USB device is opened.

Hosted jobs explicitly run `cargo run --features repo-tools --bin tethercam-tools -- usbip prepare`
and set `TETHERCAM_USBIP=1`. Cargo/tests never run as root. Only a canonical VHCI node with the
exact synthetic serial/vendor/product tuple authorizes permission changes or detach. Native
cleanup runs on failure too. Linux CI cannot disable/skip the adapter scenario.

Local Linux verification requires an already configured controller, writable delegated attach
**and** detach controls and access to the exact synthetic nodes. The controls normally belong to
root with mode `0200`; a loaded module is not sufficient delegation. Local commands never run
`sudo`, install packages, load modules, alter permissions or touch services. Portable loopback
protocol tests require none of that setup. Full API, authority and command contracts:
[`verification.md`](verification.md).

The server bounds transfers, pending memory/work, connections and deadlines, retains framing
across notification cancellation, and joins shutdown. Tests cover fragmenting, duplicate import/
sequence rejection, independent echo transfers, malformed/oversized URBs, unlink, stalls, clock/
I/O faults, re-import and resource exhaustion. The native Sony suite also covers hotplug,
wrong-mode enumeration, class/port reset recovery and mid-transfer disconnect. SICDD cancellation
discards the matching bulk transaction, flushes pending bulk URBs, preserves its session and
reports busy until the next status poll. Native tests continue with a fresh transaction and
close the same session; unknown transaction IDs do not cancel it.

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
selection and open errors with one concrete test source. The engine, fixtures and scenarios ship with the crate behind their documented feature gates. Only repository-policy tests are excluded because their workflow/ruleset inputs are unpublished. Physical-camera tests are unrelated and remain ignored.
