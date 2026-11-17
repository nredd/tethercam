# macOS quickstart and camera checklist

From a boxed Sony body to a stream UniFi Protect and Home Assistant can use, on a Mac. Every
address here is synthetic (`192.0.2.0/24`). Nothing in it needs a Sony or Apple helper killed,
unloaded or bypassed: the `icc` backend runs beside them.

## What was measured, and what was not

One owner commissioning, Sony a7 III (ILCE-7M3) on a Mac mini over USB-C, published
`image-capture` 0.1.0, `tethercam` at `main` e48ae46. Counts and outcomes only.

Verified on that hardware:

- `tethercam cameras` listed the camera; selecting it by exact `camera_id` worked
- Two `GetDeviceInfo` cycles, each with a confirmed close
- The live-view probe: 60 frames over 2 cycles at 1024x680, 2 confirmed closes
- Authenticated RTSP H.264 played, viewed by the owner
- UniFi Protect adopted it as a third-party camera with the generated credentials. It showed
  online and recording, and the owner confirmed moving playback
- Home Assistant snapshots: the ONVIF config flow, and the camera Home Assistant imported
  automatically from Protect, each returned a real 1024x680 JPEG (HTTP 200)
- `tethercam service install` (launchd), then a graceful `SIGTERM` stop; `KeepAlive` brought it
  back with a new `run_id` and status `ready`
- A 6 h 27 min partial soak. The host's network interface dropped out during it; the stream
  recovered with no observed process restart

NOT verified, and not claimed:

- 24 hours, or any endurance pass. The soak was stopped by the owner at 6 h 27 min
  ([`soak.md`](soak.md) is the procedure for the real run, [#14](https://github.com/nredd/tethercam/issues/14))
- One USB power connection sustaining the camera for long runs. Battery telemetry read null/unknown
- Screen lock, reboot, camera power-cycle and USB replug, and whether `camera_id` survives them
- Moving playback in the Home Assistant frontend. Only snapshots were checked there
- Protect showing exactly one entry per camera: duplicate Protect rows are an open 1.0 blocker
  ([#112](https://github.com/nredd/tethercam/issues/112)). A scope fix
  ([#115](https://github.com/nredd/tethercam/pull/115)) merged but was not deployed during the run,
  so it is not proven to fix the duplicates
- Other bodies, hosts, and Protect or Home Assistant versions

## Camera checklist

On the camera, with the lens on and the body powered:

1. Menu -> Setup -> USB Connection -> **PC Remote**. Required: the body is refused in MTP and
   Mass Storage mode, and picked up within a minute of switching
2. Menu -> Setup -> USB Power Supply -> **On**
3. Data-capable USB-C cable, directly to the Mac mini, not through an unpowered hub

Settings that were NOT checked on the a7 III, so whether they matter is unknown: the mode dial
position, save destination, focus settings, and auto power-off. A body was seen to leave USB
within hours with the cable untouched and nobody connected
([#86](https://github.com/nredd/tethercam/issues/86) history); the cause is unestablished. If
your camera disappears, look at its power-off and sleep settings first and report what you find.

## Steps

1. Install: `cargo install tethercam --features macos-icc` (Rust 1.99 or newer for the `icc`
   feature; see the [README](../README.md#install))
2. Create the config privately: `tethercam setup --config ./camera.toml`. It prints the generated
   credentials once to your terminal; keep that output private
3. Select the backend in `camera.toml`: `camera_backend = "icc"`
4. List cameras: `tethercam cameras --config ./camera.toml`. Copy the exact identifier it prints
   into `camera_id`. The identifier is printed nowhere else, so treat it as private. With one
   camera attached and `camera_id` unset the sole candidate is used; several are refused
   ([details](macos-camera.md#listing-and-selecting-cameras))
5. First view, loopback only. Set `rtsp_address` and `onvif_address` to `"127.0.0.1"`, then
   `tethercam doctor --config ./camera.toml` and `tethercam run --config ./camera.toml`. Open
   `rtsp://127.0.0.1:8554/live/main` in a player, with the generated credentials entered in the
   player's authentication prompt, not in the URL. Status is at
   `http://127.0.0.1:8080/tethercam/status` (digest auth). `waiting` means no first live-view frame
   yet; the log's `camera_open_failed` reason says why. No frame, no stream: nothing is advertised
   before the camera reports its size
6. Open it to the network. Only now set both addresses to the Mac's LAN address, or `0.0.0.0`
   (the generated default), and restart. Everything is authenticated, but treat this as exposing a
   camera. WS-Discovery announces only once the camera is ready, and multicast discovery does
   not cross VLANs: from another subnet add by address
7. Install the service: see [Service](#service)
8. Adopt it: [UniFi Protect](unifi-protect.md), then [Home Assistant](home-assistant.md)

## Service

```sh
tethercam service install    # from a logged-in GUI session, not over a bare SSH login
tethercam service status
tethercam service uninstall
```

- The agent runs `run --config` with the FIXED path `~/Library/Application Support/tethercam/config.toml`.
  It does not follow your `./camera.toml`. To keep an identity and credentials you already gave
  to Protect, put that file in place first: `tethercam setup --config "$HOME/Library/Application Support/tethercam/config.toml"`,
  or move your protected file there while the agent is stopped. Install then loads it instead
  of generating a new one
- Stable identity: install and reinstall preserve the config, credentials and endpoint UUID.
  `uninstall` removes the agent but keeps the config, so a reinstall presents the same camera
  ([`service.md`](service.md): "matching files, credentials and UUID are preserved")
  Replacing the config with a fresh `setup` generates a new identity, which Protect sees as a new
  camera
- Stopping is `SIGTERM` and a wait. launchd runs with `ExitTimeOut` 0, so nothing escalates to
  `SIGKILL`. A stop waits for the camera session to close and the native runtime to join, and it can
  wait a long time. A timeout is never cancellation: do not kill a stuck agent unless you accept
  an unclean camera session ([`service.md`](service.md#shutdown-and-validation))
- `KeepAlive` restarts it after a ten-second throttle. Under `icc` it also exits with 75 and
  restarts if the native runtime is quarantined (camera removed mid-command, for example). Verified: a
  graceful `SIGTERM` was followed by an automatic restart with a new `run_id`, status `ready`
- Logs: `~/Library/Logs/tethercam/stdout.log` and `stderr.log`, not rotated
  ([`service.md`](service.md#run-counters-and-the-log-bound))
- Screen lock and unlock, and reboot with auto-login, are not validated. Keep the Mac mini on
  with sleep disabled (sleep behavior was not tested)

## Network link loss

The camera is only reachable while the host's network link is up, and the listeners and
WS-Discovery are bound at start to the configured addresses. During the partial soak the host's
network interface disappeared once and the stream recovered with no observed process restart. That is
one observation, not a guarantee: how listeners and discovery behave across interface changes is
untested here and tracked in [#116](https://github.com/nredd/tethercam/issues/116). Until that
lands, treat a pinned address (`rtsp_address`, `onvif_address`, `interface`) as bound at start and
check the stream after any link loss.

## Firewall

macOS may prompt to allow incoming connections for the `tethercam` binary the first time it listens
beyond loopback. Allow it only if you are on the network you intend. Open TCP 8554 (RTSP) and 8080
(ONVIF/snapshot) and UDP 3702 (WS-Discovery multicast) to your NVR and Home Assistant only. The ports
are `rtsp_port` and `onvif_port`.

## Bitrate

The default `streams.bitrate` is 2 Mbps for the H.264 stream. Older notes of "about 3 Mbps" in
[`encoder-evaluation.md`](encoder-evaluation.md) are that evaluation's target, not the default.

## Troubleshooting

- `doctor` does not yet name the likely cause when the first frame never arrives (remaining part of [#86](https://github.com/nredd/tethercam/issues/86)); read `camera_open_failed` in the log
- `waiting` forever: not in PC Remote, camera asleep or off, wrong `camera_id` (it matches nothing
  and the service waits), or another program has the session. Read `camera_open_failed` in the log
- Protect or Home Assistant cannot see it: check clock skew (WS-Security timestamps), subnet and
  VLAN, `onvif_address`, and that the Mac's firewall allows it
- Home Assistant playback does not work: frontend moving playback is not verified; see
  [Home Assistant](home-assistant.md)
