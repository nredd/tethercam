# Home Assistant

Two paths were tried. Addresses are synthetic. Home Assistant's UI and integrations change
between versions, so labels may differ. Start from the [macOS quickstart](macos-quickstart.md) or
[Linux quickstart](linux-quickstart.md).

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

Via Protect: set up the UniFi Protect integration against the console. The adopted camera appeared in the
integration; the setup steps were not recorded.

## Read-only REST status sensor

Design guidance only: this recipe has not yet been verified on Home Assistant OS. Add it to
`configuration.yaml`, substitute the tethercam host address, and store the generated credentials
as `tethercam_username` and `tethercam_password` in Home Assistant's `secrets.yaml`. Protect both
files. The example address is synthetic. HTTP Digest authenticates but does not encrypt traffic;
use a trusted network.

```yaml
rest:
  - resource: http://192.0.2.10:8080/tethercam/status
    authentication: digest
    username: !secret tethercam_username
    password: !secret tethercam_password
    scan_interval: 30
    sensor:
      - name: Tethercam status
        value_template: "{{ value_json.camera | default('unknown', true) }}"
        json_attributes:
          - battery_percent
          - power
          - uptime_secs
          - frames_fetched
          - frames_served
          - frames_dropped
          - reconnects
          - session_restarts
          - connect_misses
          - snapshots_served
          - snapshots_refused
          - run_id
```

The sensor state is `waiting`, `ready` or `stopped`. Battery fields can be absent, including
while waiting; `power` can be `battery`, `usb`, `no_battery` or `unknown`. When using an
attribute in a template, guard missing or null values, for example
`{{ state_attr('sensor.tethercam_status', 'battery_percent') | default(none, true) }}`;
also check `has_value('sensor.tethercam_status')` before using a reading. A failed request can
leave the entity unavailable; do not treat a missing reading as zero. Counters reset on process
restart; `run_id` identifies the run. Definitions: [`service.md`](service.md#run-counters).

## Troubleshooting

Expectations from the design, not observed in the measured run:

- Cannot connect: subnet, firewall on the Mac, `onvif_address`, service not `ready` (it answers
  503 until the first live-view frame)
- Authentication fails: credentials, and clock skew between hosts
- Camera not in PC Remote: the service reports `waiting` and ONVIF answers 503 ([`architecture.md`](architecture.md#waiting-for-the-camera)); fix the camera mode first
- Duplicate cameras: see the known Protect issue in [`unifi-protect.md`](unifi-protect.md)
