# Home Assistant

Two paths were tried. Addresses are synthetic. Home Assistant's UI and integrations change
between versions, so labels may differ. Start from the [macOS quickstart](macos-quickstart.md).

## Verified

Owner, one run:

- ONVIF integration, added manually with host (the Mac's LAN address, e.g. `192.0.2.10`), port (`onvif_port`, default 8080), and the
  generated username and password. The config flow succeeded and the entity's snapshot was a real
  1024x680 JPEG (HTTP 200)
- UniFi Protect integration: the camera Protect had adopted was imported automatically, and its
  snapshot was likewise a real 1024x680 JPEG (HTTP 200)

NOT verified: moving playback in the Home Assistant frontend (stream card, for either path). Only
snapshots were checked. Home Assistant and integration versions were not recorded.

## Steps

ONVIF (menu names are from memory of the HA UI and not recorded; the notes below are design
expectations, not measured):

1. Add integration -> ONVIF. Choose manual configuration, not discovery, if Home Assistant is on
   another VLAN: multicast discovery does not cross it
2. Enter host, port and credentials
3. Check the camera entity's snapshot

Via Protect: set up the UniFi Protect integration against the console. Adopted cameras are
imported without further steps.

## Troubleshooting

Expectations from the design, not observed in the measured run:

- Cannot connect: subnet, firewall on the Mac, `onvif_address`, service not `ready` (it answers
  503 until the first live-view frame)
- Authentication fails: credentials, and clock skew between hosts
- Camera not in PC Remote: the service reports `waiting` and ONVIF answers 503 ([`architecture.md`](architecture.md#waiting-for-the-camera)); fix the camera mode first
- Duplicate cameras: see the known Protect issue in [`unifi-protect.md`](unifi-protect.md)
