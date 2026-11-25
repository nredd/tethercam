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
- A 24-hour unattended `systemd` endurance soak or sustained bus-power endurance
- Real-time H.264 throughput on an ARM SBC / Raspberry Pi; other bodies and hosts

The steps below are guidance, not a claim that every configuration or service operation was
measured in that run.

## Camera checklist

1. Set USB Connection to **PC Remote** and USB Power Supply to **On**
2. Connect a data-capable USB cable directly to the host. Only one program can hold the camera
3. Other settings, including sleep and auto power-off, have not been established; investigate them
   if the camera disappears

## Power

On the Linux USB4 test host, the A7 III battery fell from 97% to 80% over about two hours,
even while status reported `power: usb`. This is one observation, not an endurance test.
`USB Power Supply = On` lets USB supplement the battery and reduce drain; it does not guarantee
battery-neutral operation.
Sony's [A7 III guide](https://helpguide.sony.net/ilc/1720/v1/en/contents/TP0001653088.html)
says the battery does not charge while the camera is on and can supplement external USB power.
The A7 III does not support USB-PD; it is not listed among Sony's
[USB-PD-compatible cameras](https://www.sony.com/electronics/support/articles/00301779).

For unattended 24/7 A7 III use, consider a compatible NP-FZ100 DC coupler (dummy battery)
with a suitable continuous power supply, and test the complete setup; this has not been
verified here. Newer PD-capable bodies include the A7 IV, A7C II, ZV-E10 II, FX30 and A6700.
Check the camera-facing connection, cable and power source against Sony's requirements: host
USB-PD support alone does not establish that the camera receives sufficient power while tethered.

## Steps

1. Install from source as described in the [README](../README.md#install). The published
   0.0.1 release does not include `cameras` or `/tethercam/status`
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
   the first frame is not ready; check the log for `camera_open_failed` or `camera_disconnected`.
   An absent camera need not log an open error. Stop with SIGTERM or Ctrl-C and wait for cleanup
   before running `probe` or `doctor` again
6. To serve other hosts, set the listener addresses to the intended network address or the
   generated wildcard defaults and restart. Restrict network access with a firewall. Discovery
   does not cross VLANs; add by address where needed
7. Install a system or user service as below, then consult [Home Assistant](home-assistant.md)
   and [UniFi Protect](unifi-protect.md). Their Linux adoption/playback is not yet verified

## Service

See [`service.md`](service.md) before installing: installation starts the service and can open
the camera. Stop the foreground process first. The system service uses
`/var/lib/tethercam/config.toml`; the user service uses `~/.config/tethercam/config.toml`.
Neither uses `./camera.toml`. Prepare the fixed path or move an existing protected config there
while stopped to preserve credentials and identity. For system migration, follow the ownership
requirements in [`service.md`](service.md). Carry over or reapply the intended listener addresses
before installing: a fresh service config uses wildcard defaults.

- System: install a durable binary under `/usr` or `/opt`; substitute its actual absolute path
  for `/usr/local/bin/tethercam` in the commands below. If you installed the manual
  `plugdev` rule in step 2, first remove that manual rule from
  `/etc/udev/rules.d/70-tethercam.rules` and reload udev. Never remove or overwrite a rule
  already managed by the system service. Then run `sudo /usr/local/bin/tethercam service install`.
  The installer manages its account and rule. Replug after installation. Status:
  `sudo /usr/local/bin/tethercam service status`
- User: after provisioning USB access, run `tethercam service install --user` and
  `tethercam service status --user`. A user bus is required; logout normally stops it unless an
  administrator enables lingering

See [`service.md`](service.md) for credential retrieval, uninstall and shutdown details.
Stopping waits for camera cleanup and may take indefinitely if a process wedges.
