# Linux quickstart and camera checklist

From a Sony body to a foreground stream on Linux. Addresses in examples are synthetic
(`192.0.2.0/24`); replace them with your host's address.

## What was measured, and what was not

One Sony A7 III (ILCE-7M3), native Linux xHCI and the pure-Rust `nusb` backend:

- `probe` and `doctor` succeeded
- `usb_hardware`: 20 open / `GetDeviceInfo` / close cycles
- `sony_hardware`: 30-second live-view soak at about 20.6 fps
- Foreground `tethercam run` served authenticated `/tethercam/status`, a 1024x680 JPEG at
  `/onvif/snapshot/main`, and 1024x680 H.264 Constrained Baseline at 15 fps on `/live/main`
- `SIGTERM` shut down gracefully and a subsequent `probe` succeeded

NOT yet verified on Linux:

- UniFi Protect adoption and recorded timeline playback; Home Assistant OS live stream card
- A 24-hour unattended `systemd` endurance soak or bus-power endurance
- Real-time H.264 throughput on an ARM SBC / Raspberry Pi; other bodies and hosts

The steps below are guidance, not a claim that every configuration or service operation was
measured in that run.

## Camera checklist

1. Set USB Connection to **PC Remote** and USB Power Supply to **On**
2. Connect a data-capable USB cable directly to the host. Only one program can hold the camera
3. Other settings, including sleep and auto power-off, have not been established; investigate them
   if the camera disappears

## Steps

1. Install `tethercam` as described in the [README](../README.md#install)
2. Provision USB access for the foreground user: follow [`usb.md`](usb.md#linux) for the
   `70-tethercam.rules` rule, `plugdev` on headless hosts, login renewal and replug. Do not
   overwrite the system-service-managed rule; see [`service.md`](service.md)
3. Run `tethercam setup --config ./camera.toml`. It creates or loads the protected configuration
   and prints credentials; keep the output private
4. Run `tethercam cameras --config ./camera.toml`, then `tethercam probe --config ./camera.toml`
   and `tethercam doctor --config ./camera.toml`. Stop any other camera application first
5. For a foreground check, set `rtsp_address` and `onvif_address` to loopback in the config,
   then run `tethercam run --config ./camera.toml`. Use HTTP Digest credentials to read
   `/tethercam/status` on port 8080 and `/onvif/snapshot/main`; use a player's authentication
   prompt for `/live/main` on RTSP port 8554. Do not put credentials in URLs. `waiting` means
   the first frame is not ready; read `camera_open_failed` in the log. Stop with SIGTERM or
   Ctrl-C and wait for cleanup before running `probe` again
6. To serve other hosts, set the listener addresses to the intended network address or the
   generated wildcard defaults and restart. Restrict network access with a firewall. Discovery
   does not cross VLANs; add by address where needed
7. Install a system or user service as below, then consult [Home Assistant](home-assistant.md)
   and [UniFi Protect](unifi-protect.md). Their Linux adoption/playback is not yet verified

## Service

See [`service.md`](service.md) before installing: installation starts the service and can open
the camera. Stop the foreground process first.

- System: install a durable binary under `/usr` or `/opt`, then run
  `sudo /usr/local/bin/tethercam service install`. The installer manages its account and udev
  rule. Replug after the rule is installed. Status: `sudo /usr/local/bin/tethercam service status`
- User: after provisioning USB access, run `tethercam service install --user` and
  `tethercam service status --user`. A user bus is required; logout normally stops it unless an
  administrator enables lingering

The system service uses `/var/lib/tethercam/config.toml`; the user service uses
`~/.config/tethercam/config.toml`. Neither uses `./camera.toml`. Prepare the fixed path or move
an existing protected config there while stopped to preserve credentials and identity; see
[`service.md`](service.md) for ownership, credential retrieval, uninstall and shutdown details.
Stopping waits for camera cleanup and may take indefinitely if a process wedges.
