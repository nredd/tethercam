# UniFi Protect

Third-party camera adoption, as observed. Protect's third-party camera support is
ONVIF-based; the vendor's article is
[here](https://help.ui.com/hc/en-us/articles/26301104828439-Third-Party-Cameras-in-UniFi-Protect).
Addresses are synthetic. Start from the [macOS quickstart](macos-quickstart.md): the service must
be `ready` and listening on the LAN first.

## Verified

Owner, one run, `tethercam` main e48ae46 on a Mac mini, a7 III. UI and version names below are as
seen then; Protect's UI changes between versions, so labels may differ on yours.

- In Protect's Devices view, the camera appeared as a third-party camera with a Click to Adopt
  action. Adopting it asks for camera credentials: enter the generated `tethercam` username and
  password
- It came up online and recording. The owner confirmed moving playback in Protect
- Adoption by IP ("Advanced Adoption") and a per-call ONVIF trace were NOT recorded here. Which
  calls Protect makes, how it picks HQ and LQ, and which codecs it accepts beyond H.264 Baseline at
  1024x680 are not documented yet
- Protect and firmware versions were not recorded; no compatibility claim beyond the one run

## Steps

1. The service is `ready` (`GET /tethercam/status`) and bound to a LAN address
2. In Protect, open Devices and find the camera under adoptable third-party cameras. Click to
   Adopt
3. Enter the camera credentials from `setup`. Never put them in a URL
4. Wait for online, then check live view and recording

If it is not listed, discovery may not reach it (other VLAN, multicast filtered). Add it by IP
and ONVIF port (`onvif_port`, default 8080) where your Protect version offers that.

## Known issue: duplicate rows

Protect may show the same camera more than once. This is an open 1.0 blocker
([#112](https://github.com/nredd/tethercam/issues/112)). A fix to discovery scopes
([#115](https://github.com/nredd/tethercam/pull/115)) merged but was not running during the
verified run, so it is not proven to fix it. Do not delete the config to "fix" a duplicate: a new
identity adds another camera.

## Troubleshooting

- Not discovered: subnet, VLAN, multicast, `onvif_address`; the camera is silent until ready
- Credential rejected: re-read `camera.toml` privately; the installed service uses its own fixed
  config
- Offline after a stretch: see the [network link loss](macos-quickstart.md#network-link-loss) note
- Clock skew breaks ONVIF authentication: keep the Mac's clock synced
- Camera not in PC Remote: the service is `waiting` and advertises nothing
